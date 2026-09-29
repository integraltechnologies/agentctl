//! The boundary to procd, the installed lifecycle authority over a
//! provider's processes.
//!
//! procd owns lifecycle domains: it places a task's first process in one
//! before the task runs, keeps its descendants there, terminates the domain
//! itself and reports authoritative evidence of what became of it. agentctl
//! owns everything else, and never reimplements containment: no process
//! ids, ancestry, process groups or environment markers ever stand in for a
//! domain here.
//!
//! This module mirrors the installed v0.1.0 `procd.h` (the build script
//! checks the header's digest) behind a safe API. It validates everything
//! procd returns before the rest of agentctl can rely on it: an unrecognized
//! code or level is an error, never a guess, and uncertainty stays
//! uncertainty ([`Recovery::Unresolved`], [`Settlement::Uncertain`]).

use std::ffi::{CStr, CString, c_char, c_int};
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use crate::platform::{Level, Need};

/// How long terminating or recovering a domain may take, at most.
pub const TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);

/// The C ABI of `procd.h`, exactly as installed.
mod ffi {
    use std::ffi::{c_char, c_int};

    pub const OK: c_int = 0;
    pub const E_UNSUPPORTED_ENFORCEMENT: c_int = 1;
    pub const E_PREREQUISITE: c_int = 2;

    pub const REQUIRE_ENFORCED: c_int = 0;
    pub const ALLOW_BEST_EFFORT: c_int = 1;

    pub const IDENTITY_MAX: usize = 512;

    /// `procd_domain`: opaque.
    #[repr(C)]
    pub struct Domain {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    pub struct Policy {
        pub enforcement: c_int,
        pub label: *const c_char,
        pub drop_uid: i64,
        pub drop_gid: i64,
    }

    #[repr(C)]
    pub struct Capabilities {
        pub process_tree_termination: c_int,
        pub pre_execution_containment: c_int,
        pub descendant_containment: c_int,
        pub topology_escape_resistance: c_int,
        pub domain_emptiness_proof: c_int,
        pub safe_recovery: c_int,
        pub crash_behavior: c_int,
        pub backend: *const c_char,
        pub detail: *const c_char,
    }

    #[repr(C)]
    pub struct DomainStatus {
        pub state: c_int,
        pub population: c_int,
        pub process_tree_termination: c_int,
        pub population_is_authoritative: c_int,
    }

    #[repr(C)]
    pub struct TerminationEvidence {
        pub admission_closed: c_int,
        pub authority_directed: c_int,
        pub emptiness_proven: c_int,
        pub enforced: c_int,
        pub final_state: c_int,
        pub detail: *const c_char,
    }

    unsafe extern "C" {
        pub fn procd_capabilities_probe(out: *mut Capabilities) -> c_int;
        pub fn procd_create_domain(policy: *const Policy, out: *mut *mut Domain) -> c_int;
        pub fn procd_domain_spawn(
            domain: *mut Domain,
            argv: *const *const c_char,
            out_pid: *mut i64,
        ) -> c_int;
        pub fn procd_domain_status_get(domain: *mut Domain, out: *mut DomainStatus) -> c_int;
        pub fn procd_domain_terminate(
            domain: *mut Domain,
            timeout_ms: c_int,
            out: *mut TerminationEvidence,
        ) -> c_int;
        pub fn procd_domain_identity(domain: *mut Domain, buf: *mut c_char, len: usize) -> c_int;
        pub fn procd_domain_release(domain: *mut Domain) -> c_int;
        pub fn procd_recover(
            identity: *const c_char,
            outcome: *mut c_int,
            out: *mut *mut Domain,
        ) -> c_int;
        pub fn procd_status_name(status: c_int) -> *const c_char;
    }
}

/// What a procd call reported, in procd's words.
fn status_name(code: c_int) -> String {
    // SAFETY: takes a plain integer; the result is a static string or null.
    let name = unsafe { ffi::procd_status_name(code) };
    text(name).unwrap_or_else(|| format!("status {code}"))
}

/// A library-owned C string, copied; `None` for null.
fn text(raw: *const c_char) -> Option<String> {
    // SAFETY: procd's strings are NUL-terminated and valid for the moment
    // they are read, which is all this does with them.
    (!raw.is_null()).then(|| {
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    })
}

/// A capability level, once procd's code for it is recognized.
fn level(code: c_int) -> Result<Level> {
    match code {
        0 => Ok(Level::Unsupported),
        1 => Ok(Level::BestEffort),
        2 => Ok(Level::Enforced),
        other => bail!("procd reported an unrecognized capability level ({other})"),
    }
}

/// What happens to a domain when its controlling authority disappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crash {
    /// The OS destroys the domain and everything in it.
    AutomaticDestruction,
    /// The exact domain survives and can be reacquired.
    DurableReacquisition,
    /// Its fate cannot be established afterwards.
    Unresolved,
}

/// What procd can establish on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub process_tree_termination: Level,
    pub pre_execution_containment: Level,
    pub descendant_containment: Level,
    pub topology_escape_resistance: Level,
    pub domain_emptiness_proof: Level,
    pub safe_recovery: Level,
    pub crash: Crash,
    /// procd's backend identifier, for diagnostics.
    pub backend: String,
    /// Why the levels are what they are: metadata, never authority.
    pub detail: String,
}

/// procd's capabilities on this host, or why they cannot be established.
/// Discovery, not establishment: [`Domain::create`] revalidates.
pub fn capabilities() -> &'static Result<Capabilities, String> {
    static CAPABILITIES: OnceLock<Result<Capabilities, String>> = OnceLock::new();
    CAPABILITIES.get_or_init(|| probe().map_err(|e| format!("{e:#}")))
}

fn probe() -> Result<Capabilities> {
    let mut raw = ffi::Capabilities {
        process_tree_termination: -1,
        pre_execution_containment: -1,
        descendant_containment: -1,
        topology_escape_resistance: -1,
        domain_emptiness_proof: -1,
        safe_recovery: -1,
        crash_behavior: -1,
        backend: ptr::null(),
        detail: ptr::null(),
    };
    // SAFETY: `raw` is a valid, writable `procd_capabilities`.
    let status = unsafe { ffi::procd_capabilities_probe(&mut raw) };
    if status != ffi::OK {
        bail!(
            "procd cannot report its capabilities: {}",
            status_name(status)
        );
    }
    let crash = match raw.crash_behavior {
        0 => Crash::AutomaticDestruction,
        1 => Crash::DurableReacquisition,
        2 => Crash::Unresolved,
        other => bail!("procd reported an unrecognized crash behavior ({other})"),
    };
    Ok(Capabilities {
        process_tree_termination: level(raw.process_tree_termination)?,
        pre_execution_containment: level(raw.pre_execution_containment)?,
        descendant_containment: level(raw.descendant_containment)?,
        topology_escape_resistance: level(raw.topology_escape_resistance)?,
        domain_emptiness_proof: level(raw.domain_emptiness_proof)?,
        safe_recovery: level(raw.safe_recovery)?,
        crash,
        backend: text(raw.backend).ok_or_else(|| anyhow!("procd named no backend"))?,
        detail: text(raw.detail).unwrap_or_default(),
    })
}

/// Why a domain could not be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// procd refused, fail closed: the required lifecycle guarantee cannot
    /// be established here. Nothing was created or executed.
    Refused(String),
    /// Domain creation failed for another reason.
    Failed(String),
}

impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CreateError {}

/// A lifecycle domain, and the one handle that owns it: procd terminates
/// the domain by authority, never by process.
///
/// Dropping the handle releases procd's authority without terminating
/// anything (whether the domain survives depends on the backend), so
/// whoever owns work in a domain terminates it first.
pub struct Domain {
    raw: NonNull<ffi::Domain>,
    identity: String,
}

// SAFETY: procd serializes operations on a single handle internally, and
// distinct handles are independent (see `procd.h`, THREAD-SAFETY).
unsafe impl Send for Domain {}
unsafe impl Sync for Domain {}

impl fmt::Debug for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Domain")
            .field("identity", &self.identity)
            .finish()
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // SAFETY: the handle is live and never used again: it is owned here.
        unsafe { ffi::procd_domain_release(self.raw.as_ptr()) };
    }
}

impl Domain {
    /// Creates a domain as `need` requires. Under `RequireEnforced` this
    /// refuses, creating nothing, unless procd establishes enforced
    /// lifecycle termination; a weaker level is never taken for it.
    pub fn create(need: Need, label: &str) -> Result<Self, CreateError> {
        let label = CString::new(label.replace('\0', "")).unwrap_or_default();
        let policy = ffi::Policy {
            enforcement: match need {
                Need::RequireEnforced => ffi::REQUIRE_ENFORCED,
                Need::AllowBestEffort => ffi::ALLOW_BEST_EFFORT,
            },
            label: label.as_ptr(),
            drop_uid: -1,
            drop_gid: -1,
        };
        let mut raw = ptr::null_mut();
        // SAFETY: `policy` and `label` outlive the call; `raw` is writable.
        let status = unsafe { ffi::procd_create_domain(&policy, &mut raw) };
        let refused = matches!(status, ffi::E_UNSUPPORTED_ENFORCEMENT | ffi::E_PREREQUISITE);
        if status != ffi::OK {
            let why = format!(
                "procd refused to create a lifecycle domain: {}",
                status_name(status)
            );
            return Err(match refused {
                true => CreateError::Refused(why),
                false => CreateError::Failed(why),
            });
        }
        let raw = NonNull::new(raw)
            .ok_or_else(|| CreateError::Failed("procd created no domain".into()))?;
        let mut domain = Self {
            raw,
            identity: String::new(),
        };
        // From here, dropping `domain` releases the handle.
        domain.identity = domain
            .read_identity()
            .map_err(|e| CreateError::Failed(format!("{e:#}")))?;
        if need == Need::RequireEnforced {
            let established = domain
                .level()
                .map_err(|e| CreateError::Failed(format!("{e:#}")))?;
            if established != Level::Enforced {
                return Err(CreateError::Refused(format!(
                    "procd established only {established:?} lifecycle termination where \
                     enforced was required"
                )));
            }
        }
        Ok(domain)
    }

    /// The domain's durable, generation-safe identity: what [`recover`]
    /// takes after a restart. Never a process id.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    fn read_identity(&self) -> Result<String> {
        let mut buf = vec![0u8; ffi::IDENTITY_MAX];
        // SAFETY: `buf` is writable for the length given.
        let status = unsafe {
            ffi::procd_domain_identity(self.raw.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if status != ffi::OK {
            bail!(
                "procd gave no identity for the domain: {}",
                status_name(status)
            );
        }
        let identity = CStr::from_bytes_until_nul(&buf)
            .map_err(|_| anyhow!("procd's domain identity is not terminated"))?
            .to_str()
            .map_err(|_| anyhow!("procd's domain identity is not text"))?;
        valid_identity(identity)?;
        Ok(identity.to_owned())
    }

    /// The lifecycle termination level procd established for this domain.
    pub fn level(&self) -> Result<Level> {
        let mut raw = ffi::DomainStatus {
            state: -1,
            population: -1,
            process_tree_termination: -1,
            population_is_authoritative: 0,
        };
        // SAFETY: `raw` is a valid, writable `procd_domain_status`.
        let status = unsafe { ffi::procd_domain_status_get(self.raw.as_ptr(), &mut raw) };
        if status != ffi::OK {
            bail!(
                "procd cannot report the domain's status: {}",
                status_name(status)
            );
        }
        level(raw.process_tree_termination)
    }

    /// Starts `argv` inside the domain, its first process placed there
    /// before it runs. Returns that process's id, which is diagnostic
    /// metadata only. If procd cannot place it, nothing runs.
    pub fn spawn(&self, argv: &[&str]) -> Result<Option<u32>> {
        let owned: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(*a).map_err(|_| anyhow!("an argument holds a NUL byte")))
            .collect::<Result<_>>()?;
        let mut pointers: Vec<*const c_char> = owned.iter().map(|a| a.as_ptr()).collect();
        pointers.push(ptr::null());
        let mut pid = -1i64;
        // SAFETY: `pointers` is a NULL-terminated array of NUL-terminated
        // strings that `owned` keeps alive; `pid` is writable.
        let status =
            unsafe { ffi::procd_domain_spawn(self.raw.as_ptr(), pointers.as_ptr(), &mut pid) };
        if status != ffi::OK {
            bail!("procd refused the launch: {}", status_name(status));
        }
        Ok(u32::try_from(pid).ok().filter(|&p| p > 0))
    }

    /// Terminates the domain by authority, closing admission first, and
    /// reports what procd established. An error means it could not be
    /// terminated at all; success does not mean the domain is proven empty
    /// (see [`Evidence::proves_empty`]).
    pub fn terminate(&self, timeout: Duration) -> Result<Evidence> {
        let mut raw = ffi::TerminationEvidence {
            admission_closed: 0,
            authority_directed: 0,
            emptiness_proven: 0,
            enforced: 0,
            final_state: -1,
            detail: ptr::null(),
        };
        let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
        // SAFETY: `raw` is a valid, writable `procd_termination_evidence`.
        let status = unsafe { ffi::procd_domain_terminate(self.raw.as_ptr(), ms, &mut raw) };
        if status != ffi::OK {
            bail!(
                "procd could not terminate the domain: {}",
                status_name(status)
            );
        }
        Ok(Evidence {
            admission_closed: raw.admission_closed != 0,
            authority_directed: raw.authority_directed != 0,
            emptiness_proven: raw.emptiness_proven != 0,
            enforced: raw.enforced != 0,
            final_state: State::from_code(raw.final_state),
            // Owned by the domain: copied while it is live.
            detail: text(raw.detail).unwrap_or_default(),
        })
    }
}

/// A domain's authority state, as procd names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created,
    Active,
    Terminating,
    Empty,
    Released,
    Unresolved,
}

impl State {
    fn from_code(code: c_int) -> Option<Self> {
        Some(match code {
            0 => Self::Created,
            1 => Self::Active,
            2 => Self::Terminating,
            3 => Self::Empty,
            4 => Self::Released,
            5 => Self::Unresolved,
            _ => return None,
        })
    }
}

/// What terminating a domain established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub admission_closed: bool,
    pub authority_directed: bool,
    pub emptiness_proven: bool,
    pub enforced: bool,
    /// `None` when procd's code for it is not recognized.
    pub final_state: Option<State>,
    /// Diagnostic metadata only.
    pub detail: String,
}

impl Evidence {
    /// Whether procd proved, by authority, that no process of the domain
    /// remains: every part of the hard invariant, at an enforced level.
    /// Nothing less is proof.
    pub fn proves_empty(&self) -> bool {
        self.admission_closed
            && self.authority_directed
            && self.emptiness_proven
            && self.enforced
            && self.final_state == Some(State::Empty)
    }
}

/// What reacquiring a domain from its identity established.
pub enum Recovery<H = Domain> {
    /// The domain still exists, and authority over it was reacquired.
    Recovered(H),
    /// procd proved the exact domain was destroyed.
    Destroyed,
    /// Its fate cannot be established, for the reason given.
    Unresolved(String),
}

/// Whether `identity` can be a procd identity token at all.
fn valid_identity(identity: &str) -> Result<()> {
    if identity.is_empty() {
        bail!("the lifecycle identity is empty");
    }
    if identity.len() >= ffi::IDENTITY_MAX {
        bail!("the lifecycle identity is longer than procd issues");
    }
    if !identity.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        bail!("the lifecycle identity is not printable text");
    }
    Ok(())
}

/// Reacquires the domain `identity` names, without guessing: anything procd
/// cannot verify is [`Recovery::Unresolved`].
pub fn recover(identity: &str) -> Recovery {
    if let Err(e) = valid_identity(identity) {
        return Recovery::Unresolved(format!("{e:#}"));
    }
    let Ok(token) = CString::new(identity) else {
        return Recovery::Unresolved("the lifecycle identity holds a NUL byte".into());
    };
    let mut outcome: c_int = -1;
    let mut raw = ptr::null_mut();
    // SAFETY: `token` outlives the call; `outcome` and `raw` are writable.
    let status = unsafe { ffi::procd_recover(token.as_ptr(), &mut outcome, &mut raw) };
    // Whatever procd handed back is owned here, and released if unused.
    let handle = NonNull::new(raw).map(|raw| Domain {
        raw,
        identity: identity.to_owned(),
    });
    if status != ffi::OK {
        return Recovery::Unresolved(format!(
            "procd could not recover it: {}",
            status_name(status)
        ));
    }
    match (outcome, handle) {
        (0, Some(domain)) => Recovery::Recovered(domain),
        (0, None) => Recovery::Unresolved("procd recovered no domain".into()),
        (1, _) => Recovery::Destroyed,
        (2, _) => Recovery::Unresolved("procd cannot establish the domain's fate".into()),
        (other, _) => {
            Recovery::Unresolved(format!("procd reported an unrecognized outcome ({other})"))
        }
    }
}

/// What settling a recorded lifecycle established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// procd proved the owned execution is gone.
    Gone,
    /// It may remain, or nothing can prove otherwise, for the reason given.
    Uncertain(String),
}

/// Settles the lifecycle recorded as `identity` with procd. Only proof that
/// the domain is gone is [`Settlement::Gone`].
pub fn settle(identity: Option<&str>) -> Settlement {
    settle_with(
        identity,
        capabilities().as_ref().map_err(String::as_str),
        recover,
        |domain| domain.terminate(TERMINATE_TIMEOUT),
    )
}

/// [`settle`], with procd's answers given. This is the whole decision, kept
/// apart from procd so that each answer procd may give can be tested.
pub fn settle_with<H>(
    identity: Option<&str>,
    host: Result<&Capabilities, &str>,
    recover: impl FnOnce(&str) -> Recovery<H>,
    stop: impl FnOnce(H) -> Result<Evidence>,
) -> Settlement {
    let uncertain = |why: String| Settlement::Uncertain(why);
    let Some(identity) = identity else {
        return uncertain("no lifecycle identity was durably recorded for it".into());
    };
    if let Err(e) = valid_identity(identity) {
        return uncertain(format!("its recorded {e:#}"));
    }
    match host {
        Err(why) => return uncertain(format!("procd's capabilities are unavailable: {why}")),
        Ok(caps) if caps.process_tree_termination != Level::Enforced => {
            return uncertain(format!(
                "procd's {} backend offers only {:?} lifecycle termination here, so nothing \
                 it reports about a domain is proof",
                caps.backend, caps.process_tree_termination
            ));
        }
        Ok(_) => {}
    }
    match recover(identity) {
        Recovery::Destroyed => Settlement::Gone,
        Recovery::Unresolved(why) => uncertain(why),
        Recovery::Recovered(domain) => match stop(domain) {
            Ok(evidence) if evidence.proves_empty() => Settlement::Gone,
            Ok(evidence) => uncertain(format!(
                "the domain still existed and its termination proved nothing ({})",
                evidence.detail
            )),
            Err(e) => uncertain(format!("the domain still existed: {e:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(level: Level) -> Capabilities {
        Capabilities {
            process_tree_termination: level,
            pre_execution_containment: level,
            descendant_containment: level,
            topology_escape_resistance: level,
            domain_emptiness_proof: level,
            safe_recovery: level,
            crash: Crash::DurableReacquisition,
            backend: "fake".into(),
            detail: String::new(),
        }
    }

    fn evidence(mutate: impl FnOnce(&mut Evidence)) -> Evidence {
        let mut e = Evidence {
            admission_closed: true,
            authority_directed: true,
            emptiness_proven: true,
            enforced: true,
            final_state: Some(State::Empty),
            detail: "fake".into(),
        };
        mutate(&mut e);
        e
    }

    fn settle_as(
        identity: Option<&str>,
        host: Level,
        recovery: Recovery<()>,
        stopped: Result<Evidence>,
    ) -> Settlement {
        let host = caps(host);
        settle_with(identity, Ok(&host), |_| recovery, |()| stopped)
    }

    fn uncertain(s: Settlement) -> String {
        match s {
            Settlement::Uncertain(why) => why,
            Settlement::Gone => panic!("uncertainty was taken for proof"),
        }
    }

    const ID: Option<&str> = Some("linux-cgroup2:1:abc");

    #[test]
    fn only_proof_of_destruction_is_gone() {
        use Level::Enforced;
        assert_eq!(
            settle_as(ID, Enforced, Recovery::Destroyed, Err(anyhow!("unused"))),
            Settlement::Gone
        );
        // A domain that still existed is gone only once procd proves it
        // empty after terminating it, at every part of the proof.
        assert_eq!(
            settle_as(ID, Enforced, Recovery::Recovered(()), Ok(evidence(|_| {}))),
            Settlement::Gone
        );
        let weaker: [fn(&mut Evidence); 6] = [
            |e| e.admission_closed = false,
            |e| e.authority_directed = false,
            |e| e.emptiness_proven = false,
            |e| e.enforced = false,
            |e| e.final_state = Some(State::Unresolved),
            |e| e.final_state = None,
        ];
        for weaken in weaker {
            let s = settle_as(ID, Enforced, Recovery::Recovered(()), Ok(evidence(weaken)));
            assert!(uncertain(s).contains("proved nothing"));
        }
        let s = settle_as(ID, Enforced, Recovery::Recovered(()), Err(anyhow!("boom")));
        assert!(uncertain(s).contains("boom"));
    }

    #[test]
    fn everything_else_stays_uncertain() {
        use Level::*;
        let unresolved = || Recovery::Unresolved("fate unknown".into());
        assert!(
            uncertain(settle_as(ID, Enforced, unresolved(), Err(anyhow!("")))).contains("fate")
        );
        // A host whose termination is not enforced proves nothing, whatever
        // procd claims about the domain.
        for level in [BestEffort, Unsupported] {
            let s = settle_as(ID, level, Recovery::Destroyed, Err(anyhow!("")));
            assert!(uncertain(s).contains("nothing it reports"));
        }
        // No identity, or one that cannot be procd's: fail closed, without
        // asking procd anything.
        for identity in [
            None,
            Some(""),
            Some("a\nb"),
            Some("é"),
            Some(&*"x".repeat(600)),
        ] {
            let host = caps(Enforced);
            let s = settle_with(
                identity,
                Ok(&host),
                |_| -> Recovery<()> { panic!("procd was asked about {identity:?}") },
                |()| Err(anyhow!("")),
            );
            uncertain(s);
        }
        let s = settle_with(
            ID,
            Err("no probe"),
            |_| -> Recovery<()> { panic!("asked") },
            |()| Err(anyhow!("")),
        );
        assert!(uncertain(s).contains("no probe"));
    }

    #[test]
    fn unknown_codes_are_errors() {
        assert!(level(3).is_err() && level(-1).is_err());
        assert_eq!(State::from_code(6), None);
        assert_eq!(State::from_code(-1), None);
    }

    #[test]
    fn this_host_is_described_truthfully() {
        let caps = capabilities().as_ref().expect("procd probes");
        assert!(!caps.backend.is_empty());
        // What the host cannot enforce is never created as if it could.
        let created = Domain::create(Need::RequireEnforced, "agentctl-test");
        match caps.process_tree_termination {
            Level::Enforced => {
                let domain = created.unwrap();
                assert_eq!(domain.level().unwrap(), Level::Enforced);
            }
            _ => assert!(
                matches!(created, Err(CreateError::Refused(_))),
                "{created:?}"
            ),
        }
    }

    #[test]
    fn a_domain_has_a_durable_identity_and_terminates_empty() {
        let domain = Domain::create(Need::AllowBestEffort, "agentctl-test").unwrap();
        valid_identity(domain.identity()).unwrap();
        // Never above what the host offers.
        let rank = |l| match l {
            Level::Unsupported => 0,
            Level::BestEffort => 1,
            Level::Enforced => 2,
        };
        let host = capabilities().as_ref().unwrap().process_tree_termination;
        assert!(rank(domain.level().unwrap()) <= rank(host));
        domain.terminate(TERMINATE_TIMEOUT).unwrap();
    }

    #[test]
    fn a_released_domain_is_never_taken_for_gone_on_a_weak_host() {
        let identity = {
            let domain = Domain::create(Need::AllowBestEffort, "agentctl-test").unwrap();
            domain.identity().to_owned()
        };
        let settled = settle(Some(&identity));
        let host = capabilities().as_ref().unwrap();
        if host.process_tree_termination != Level::Enforced {
            uncertain(settled);
        }
        // Whatever the host, procd is never guessed past.
        uncertain(settle(Some("no-such-backend:0:nothing")));
        uncertain(settle(Some("garbage")));
    }
}
