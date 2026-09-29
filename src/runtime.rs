//! The agent runtime: runs one provider invocation on behalf of a logical
//! agent, through a provider-neutral contract.
//!
//! Providers supply intelligence; agentctl owns the rest. An invocation's
//! identity, lifecycle, process, input, structured result, usage, liveness,
//! cancellation and failure classification are agentctl's, and how it ended
//! is recorded through [`Store`]. Every invocation starts fresh: a provider
//! session is observed as metadata, never resumed and never identity.
//! Provider command lines and output formats live only in the adapters.
//!
//! Lifecycle is procd's. Every invocation's processes run in a lifecycle
//! domain that procd creates, before anything is recorded or run, at the
//! strength the launch requires, and whose durable identity is recorded with
//! the invocation before its first process exists. Cancellation, and the end
//! of every invocation however it ended, terminate the domain by procd's
//! authority: nothing a provider started is left to outlive it, or to depend
//! on agentctl's death. The provider itself is started by a small shim, the
//! domain's first process (see [`shim`]).
//!
//! A launch delivers its whole input on the provider's standard input, which
//! is then closed: neither adapter's non-interactive mode accepts input
//! mid-run. The provider must answer with one structured result satisfying
//! the launch's output schema, which agentctl itself checks; prose is never
//! taken as a result.
//!
//! What is recorded durably about how an invocation ended is agentctl's own
//! classification. Provider-controlled text (its output, standard error and
//! error messages) may carry secrets, so it is never recorded; standard error
//! is returned in the [`Outcome`] only.

mod claude;
mod codex;
pub mod shim;

#[cfg(any(test, feature = "lifecycle-double"))]
pub mod testing {
    //! A test double for what terminating a domain proves, never built
    //! into agentctl itself. By default a termination that succeeded is
    //! taken for proof, so that suites can complete runs where procd's
    //! backend cannot prove a domain empty. A test says otherwise with
    //! [`evidence`] in its thread, or [`EVIDENCE`] in a child process's
    //! environment (`real` or `unproven`).

    use std::cell::Cell;

    use crate::procd::{Evidence, State};

    /// Set in a test process's environment to a [`Mode`]'s name.
    pub const EVIDENCE: &str = "AGENTCTL_TEST_EVIDENCE";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// A termination that succeeded proves the domain empty.
        Proven,
        /// procd's own evidence, whatever it proves.
        Real,
        /// Nothing is ever proven: what a backend that cannot prove
        /// emptiness reports.
        Unproven,
    }

    thread_local!(static MODE: Cell<Option<Mode>> = const { Cell::new(None) });

    /// Puts this thread in `mode` while it lives.
    pub struct Guard(Option<Mode>);

    pub fn evidence(mode: Mode) -> Guard {
        Guard(MODE.replace(Some(mode)))
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            MODE.set(self.0);
        }
    }

    fn mode() -> Mode {
        MODE.get()
            .unwrap_or_else(|| match std::env::var(EVIDENCE).as_deref() {
                Ok("real") => Mode::Real,
                Ok("unproven") => Mode::Unproven,
                _ => Mode::Proven,
            })
    }

    pub(super) fn adjust(reported: Evidence) -> Evidence {
        match mode() {
            Mode::Real => reported,
            Mode::Proven => Evidence {
                admission_closed: true,
                authority_directed: true,
                emptiness_proven: true,
                enforced: true,
                final_state: Some(State::Empty),
                detail: format!("test double over: {}", reported.detail),
            },
            Mode::Unproven => Evidence {
                emptiness_proven: false,
                enforced: false,
                final_state: Some(State::Unresolved),
                detail: format!("test double over: {}", reported.detail),
                ..reported
            },
        }
    }
}

/// Terminates `domain` by procd's authority, and says what that established.
fn terminate_domain(domain: &Domain) -> Result<Evidence> {
    let evidence = domain.terminate(procd::TERMINATE_TIMEOUT)?;
    #[cfg(any(test, feature = "lifecycle-double"))]
    let evidence = testing::adjust(evidence);
    Ok(evidence)
}

use std::ffi::OsString;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use jsonschema::Validator;
use serde_json::{Map, Value};
use tempfile::{NamedTempFile, TempDir};

use crate::config::ReasoningEffort;
use crate::platform::{self, Need};
use crate::procd::{self, Domain, Evidence};
use crate::state::{AgentId, InvocationId, Store};

pub use crate::state::{FailureKind, InvocationEnd, InvocationState, TokenUsage, Usage};

const POLL: Duration = Duration::from_millis(20);
/// How long a terminated domain's shim may take to be seen ending.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// How long output may keep flowing once the provider has exited. A
/// descendant that inherited its pipes can hold them open indefinitely.
const DRAIN: Duration = Duration::from_secs(2);
const STDERR_TAIL: usize = 16 * 1024;
const DIAGNOSTIC_LIMIT: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    /// The provider's configuration name, which is also its CLI's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => bail!("unknown provider `{s}`; agentctl runs claude and codex"),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What an invocation may do to files in its working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    ReadOnly,
    /// It may edit files within its working directory, in the provider's
    /// own editing mode. The provider decides what that confines; agentctl
    /// neither relies on it nor adds a sandbox of its own, so an editable
    /// working directory must hold nothing agentctl needs protected.
    Editable,
    /// It may also run commands there, such as builds and tests, which may
    /// write where the provider lets them: within its working directory and
    /// the temporary directory. For a working directory that is a
    /// disposable copy, whose outcome agentctl observes itself; like an
    /// editable one, it must hold nothing agentctl needs protected.
    Disposable,
}

/// The lifecycle guarantee agentctl's own roles launch with. An invocation
/// whose processes procd cannot enforce the termination of is not refused:
/// procd's evidence, not agentctl's hope, decides what it may release, so
/// whatever cannot be proven stays unresolved and blocks recovery (see
/// `crate::recovery`). A launch may require `Need::RequireEnforced` instead.
pub const ROLE_LIFECYCLE: Need = Need::AllowBestEffort;

/// Everything needed to launch one invocation.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The logical agent the invocation embodies.
    pub agent: AgentId,
    pub provider: Provider,
    /// The provider CLI; `None` finds the provider's executable on `PATH`.
    pub executable: Option<PathBuf>,
    pub model: String,
    pub effort: Option<ReasoningEffort>,
    /// Instructions added to the provider's own system instructions.
    pub bootstrap: String,
    /// The task and its context.
    pub input: String,
    /// The JSON Schema the structured result must satisfy.
    pub output_schema: Value,
    /// The absolute working directory.
    pub cwd: PathBuf,
    pub workspace: Workspace,
    /// The lifecycle guarantee procd must establish for the invocation's
    /// processes. `RequireEnforced` refuses the launch, before anything is
    /// recorded or run, unless procd can enforce termination of the whole
    /// process tree here; `AllowBestEffort` runs at whatever level procd
    /// offers, and what it cannot prove afterwards stays unresolved.
    pub lifecycle: Need,
}

/// How an invocation ended, with what agentctl received from the provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub invocation: InvocationId,
    /// Exactly what was recorded.
    pub end: InvocationEnd,
    /// The structured result, present exactly when the invocation succeeded.
    /// Its meaning is defined by whoever chose the output schema.
    pub payload: Option<Value>,
    /// Provider facts with no neutral equivalent, such as Claude's cost.
    /// Provider-controlled, so never recorded.
    pub provider_metadata: Map<String, Value>,
    /// The end of the provider's standard error, for diagnosis only.
    /// Provider-controlled, so never recorded.
    pub stderr: String,
}

/// A snapshot of a live invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub state: InvocationState,
    /// The first process of the invocation's lifecycle domain (its shim),
    /// once launched: diagnostic, never identity or authority.
    pub pid: Option<u32>,
    /// Whether agentctl has seen the provider exit.
    pub exited: bool,
    pub cancel_requested: bool,
    /// How long since the provider last wrote any output.
    pub quiet_for: Option<Duration>,
    /// Structured events received, and the latest one's provider type.
    pub events: u64,
    pub last_event: Option<String>,
    pub provider_session: Option<String>,
    /// Usage the provider has reported so far.
    pub usage: Usage,
}

impl Observation {
    /// Whether the provider process is running as far as agentctl knows.
    pub fn alive(&self) -> bool {
        self.state == InvocationState::Running && !self.exited
    }
}

/// Observes and cancels an invocation from any thread.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

impl Control {
    /// Requests cancellation. The invocation is cancelled only once its
    /// process has been terminated and observed to exit.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
    }

    pub fn observe(&self) -> Observation {
        let p = self.shared.progress();
        Observation {
            state: p.state,
            pid: p.pid,
            exited: p.exited,
            cancel_requested: self.shared.cancel.load(Ordering::SeqCst),
            quiet_for: p.last_output.map(|at| at.elapsed()),
            events: p.events,
            last_event: p.last_event.clone(),
            provider_session: p.stream.session.clone(),
            usage: p.stream.usage(),
        }
    }
}

/// One invocation, owned by agentctl from launch until it has ended.
/// Dropping a launched invocation before [`Invocation::wait`] kills its
/// process without recording an end, as if agentctl had disappeared.
pub struct Invocation {
    id: InvocationId,
    shared: Arc<Shared>,
    stage: Stage,
}

enum Stage {
    Launched(Process),
    /// It failed before a process existed, and that is already recorded.
    Ended(Box<Outcome>),
}

/// Records and launches an invocation. An invalid launch, including an
/// output schema that is not a valid JSON Schema, is refused before anything
/// is recorded; a provider that cannot be started is recorded as failed, and
/// [`Invocation::wait`] then returns that outcome.
pub fn spawn(store: &mut Store, launch: &Launch) -> Result<Invocation> {
    spawn_after(store, launch, |_, _| Ok(()))
}

/// [`spawn`], running `prepare` once the invocation is recorded and before
/// any provider process exists, so that whatever it records durably
/// precedes anything the provider does. Should `prepare` fail, nothing is
/// launched and the invocation is recorded as failed to spawn, if it can be.
pub fn spawn_after(
    store: &mut Store,
    launch: &Launch,
    prepare: impl FnOnce(&mut Store, InvocationId) -> Result<()>,
) -> Result<Invocation> {
    ensure!(
        launch.cwd.is_absolute(),
        "working directory {} is not absolute",
        launch.cwd.display()
    );
    ensure!(
        launch.output_schema.is_object(),
        "the output schema must be a JSON object"
    );
    // Only in-document references resolve: this build of the validator
    // cannot fetch files or URLs.
    let schema = jsonschema::validator_for(&launch.output_schema)
        .map_err(|e| anyhow!("the output schema is not a valid JSON Schema: {e}"))?;
    let prepared = match launch.provider {
        Provider::Claude => claude::prepare(launch),
        Provider::Codex => codex::prepare(launch),
    }?;
    let effort = launch.effort.map(|e| e.to_string());
    // Admission: the domain that will own every process of the invocation
    // is established, or refused, before anything is recorded or run.
    let domain = Domain::create(launch.lifecycle, &format!("agentctl {}", launch.provider))
        .map_err(|e| anyhow!("refusing to launch {}: {e}", launch.provider))?;
    // Its identity is recorded with the invocation, so no process can exist
    // that a restart cannot name the domain of.
    let id = store.start_contained_invocation(
        launch.agent,
        launch.provider.name(),
        &launch.model,
        effort.as_deref(),
        domain.identity(),
    )?;
    if let Err(e) = prepare(store, id) {
        let end = InvocationEnd {
            state: InvocationState::Failed,
            failure: Some(FailureKind::SpawnFailed),
            diagnostic: Some("agentctl abandoned the launch before starting the provider".into()),
            exit_code: None,
            provider_session: None,
            usage: Usage::Unavailable,
        };
        // The original error matters more than any failure to record this.
        let _ = store.finish_invocation(id, &end);
        return Err(e);
    }
    let shared = Arc::new(Shared {
        cancel: AtomicBool::new(false),
        progress: Mutex::new(Progress::new(prepared.decoder)),
        schema,
    });
    let process = match start(launch, &prepared.args, domain, &shared, &launch.input) {
        Launched::Process(process) => *process,
        Launched::Failed(kind, why) => {
            let end = InvocationEnd {
                state: InvocationState::Failed,
                failure: Some(kind),
                diagnostic: Some(clip(&why, DIAGNOSTIC_LIMIT)),
                exit_code: None,
                provider_session: None,
                usage: Usage::Unavailable,
            };
            store.finish_invocation(id, &end)?;
            shared.progress().state = end.state;
            let outcome = Outcome {
                invocation: id,
                end,
                payload: None,
                provider_metadata: Map::new(),
                stderr: String::new(),
            };
            return Ok(Invocation {
                id,
                shared,
                stage: Stage::Ended(Box::new(outcome)),
            });
        }
        // A process may have existed, and its termination was not
        // confirmed: the record stays `starting`, unresolved.
        Launched::Unconfirmed(why) => bail!("{why}"),
    };
    let process = process.with_files(prepared.files);
    let invocation = Invocation {
        id,
        shared,
        stage: Stage::Launched(process),
    };
    // Should this fail, dropping `invocation` terminates its domain, and the
    // record stays `starting`: a process may have existed.
    store.invocation_running(id)?;
    invocation.shared.progress().state = InvocationState::Running;
    Ok(invocation)
}

impl Invocation {
    pub fn id(&self) -> InvocationId {
        self.id
    }

    pub fn control(&self) -> Control {
        Control {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Waits for the invocation to end, cancelling it if requested, then
    /// records and returns how it ended.
    pub fn wait(self, store: &mut Store) -> Result<Outcome> {
        let process = match self.stage {
            Stage::Ended(outcome) => return Ok(*outcome),
            Stage::Launched(process) => process,
        };
        // Unless procd proved the domain empty, nothing is recorded: the
        // invocation keeps no end, which is what unresolved means.
        let outcome = process
            .supervise(self.id, &self.shared)
            .map_err(anyhow::Error::new)?;
        store
            .finish_invocation(self.id, &outcome.end)
            .with_context(|| format!("recording how invocation {} ended", self.id))?;
        self.shared.progress().state = outcome.end.state;
        Ok(outcome)
    }
}

/// A provider command line, as an adapter prepares it.
struct Prepared {
    args: Vec<OsString>,
    decoder: Decoder,
    /// Files the arguments name, removed once the invocation ends.
    files: Vec<NamedTempFile>,
}

/// Environment variables a provider needs beyond [`COMMON_ENV`], typically
/// for its authentication and configuration.
struct Passthrough {
    names: &'static [&'static str],
    prefixes: &'static [&'static str],
}

/// What any provider may need from the environment: to find programs and
/// the user's home and configuration, temporary space, locale, proxies and
/// certificates, on Unix and on Windows. Nothing else is passed on.
const COMMON_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "COMMONPROGRAMFILES",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

impl Passthrough {
    /// Whether the variable `name` is passed on. Names compare ignoring
    /// ASCII case, as Windows compares them.
    fn passes(&self, name: &str) -> bool {
        COMMON_ENV
            .iter()
            .chain(self.names)
            .any(|n| n.eq_ignore_ascii_case(name))
            || self.prefixes.iter().any(|p| {
                name.get(..p.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(p))
            })
    }
}

/// What starting an invocation's processes came to.
enum Launched {
    Process(Box<Process>),
    /// No provider process ran, for the reason given.
    Failed(FailureKind, String),
    /// A process may have run, and its domain could not be terminated.
    Unconfirmed(String),
}

/// Starts the provider in `domain`: the provider executable itself, never
/// through a shell, by way of the shim that gives it exactly the standard
/// streams, working directory and environment agentctl constructs.
fn start(
    launch: &Launch,
    args: &[OsString],
    domain: Domain,
    shared: &Arc<Shared>,
    input: &str,
) -> Launched {
    let failed = |kind, why: String| Launched::Failed(kind, why);
    let name = launch.provider.name();
    let exe = match &launch.executable {
        Some(path) => path.clone(),
        None => match which::which(name) {
            Ok(path) => path,
            Err(e) => {
                return failed(
                    FailureKind::ExecutableMissing,
                    format!("no `{name}` executable on PATH ({e})"),
                );
            }
        },
    };
    if !exe.is_file() {
        return failed(
            FailureKind::ExecutableMissing,
            format!("no {name} executable at {}", exe.display()),
        );
    }
    if !platform::runs_directly(&exe) {
        return failed(
            FailureKind::SpawnFailed,
            format!(
                "{} would run through a command interpreter; agentctl runs providers directly",
                exe.display()
            ),
        );
    }
    let spec = shim::Spec {
        provider: launch.provider,
        exe,
        cwd: launch.cwd.clone(),
        args: args.to_vec(),
    };
    let unusable = |what: &str, e: anyhow::Error| {
        failed(
            FailureKind::SpawnFailed,
            format!("cannot start the provider: {what}: {e:#}"),
        )
    };
    let shim_exe = match shim::locate() {
        Ok(path) => path,
        Err(e) => return unusable("no provider shim", e),
    };
    let rendezvous = match shim::Rendezvous::create(&spec) {
        Ok(rendezvous) => rendezvous,
        Err(e) => return unusable("no channel to the provider", e),
    };
    let (Some(shim_arg), Some(dir_arg)) = (shim_exe.to_str(), rendezvous.dir().to_str()) else {
        return failed(
            FailureKind::SpawnFailed,
            "cannot start the provider: a path is not text procd can be given".into(),
        );
    };
    // procd places the shim in the domain before it runs; when it cannot,
    // nothing runs at all.
    let pid = match domain.spawn(&[shim_arg, dir_arg]) {
        Ok(pid) => pid,
        Err(e) => return failed(FailureKind::SpawnFailed, format!("{e:#}")),
    };
    // From here a process exists: it is never left without its domain being
    // terminated, or the invocation stays unresolved.
    let abandon = |why: String| abandoned(terminate_domain(&domain), why);
    let (streams, started, dir) = match rendezvous.accept() {
        Ok(accepted) => accepted,
        Err(e) => return abandon(format!("{e:#}")),
    };
    match started {
        shim::Started::Yes => {}
        shim::Started::Failed(why) => return abandon(why),
    }
    Launched::Process(Box::new(Process::own(
        domain,
        streams,
        dir,
        pid,
        shared,
        input.to_owned(),
    )))
}

/// What abandoning a launch whose process may have run came to. That
/// terminating succeeded says nothing of the domain: only evidence that
/// proves it empty settles the launch as one that failed to spawn.
fn abandoned(terminated: Result<Evidence>, why: String) -> Launched {
    match terminated {
        Ok(evidence) if evidence.proves_empty() => Launched::Failed(FailureKind::SpawnFailed, why),
        Ok(evidence) => Launched::Unconfirmed(format!(
            "{why}; and its domain was terminated but not proven empty ({})",
            evidence.detail
        )),
        Err(e) => Launched::Unconfirmed(format!("{why}; and its domain is not terminated: {e:#}")),
    }
}

/// An invocation whose lifecycle domain procd did not prove empty. Whatever
/// the provider did or reported, nothing is recorded of how it ended: it
/// stays unresolved, and only recovery may settle it, by procd's proof.
#[derive(Debug)]
pub struct Unresolved {
    pub invocation: InvocationId,
    pub why: String,
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invocation {} is left unresolved: {}",
            self.invocation, self.why
        )
    }
}

impl std::error::Error for Unresolved {}

struct Shared {
    cancel: AtomicBool,
    progress: Mutex<Progress>,
    /// The launch's output schema, which a result must satisfy.
    schema: Validator,
}

impl Shared {
    fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What agentctl has observed of a live invocation.
struct Progress {
    state: InvocationState,
    pid: Option<u32>,
    exited: bool,
    input_delivered: bool,
    input_error: Option<String>,
    /// How the shim said the provider ended: its raw exit status, and
    /// whether an output stream stayed open after it.
    ended: Option<(i64, bool)>,
    /// That the shim's control channel closed: it will report nothing more.
    control_closed: bool,
    last_output: Option<Instant>,
    events: u64,
    last_event: Option<String>,
    decoder: Decoder,
    stream: Stream,
    stderr: Vec<u8>,
}

impl Progress {
    fn new(decoder: Decoder) -> Self {
        Self {
            state: InvocationState::Starting,
            pid: None,
            exited: false,
            input_delivered: false,
            input_error: None,
            ended: None,
            control_closed: false,
            last_output: None,
            events: 0,
            last_event: None,
            decoder,
            stream: Stream::default(),
            stderr: Vec::new(),
        }
    }

    /// Takes one line of the provider's JSON Lines output.
    fn observe(&mut self, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        self.events += 1;
        match parse_event(line) {
            Ok((kind, event)) => {
                match &mut self.decoder {
                    Decoder::Claude(d) => d.decode(&kind, event, &mut self.stream),
                    Decoder::Codex(d) => d.decode(&kind, event, &mut self.stream),
                }
                self.last_event = Some(kind);
            }
            Err(why) => self.stream.malformed(why),
        }
    }
}

enum Decoder {
    Claude(claude::Decoder),
    Codex(codex::Decoder),
}

/// What a provider's structured output has established, in neutral terms.
#[derive(Debug, Default)]
struct Stream {
    session: Option<String>,
    /// Token usage the provider reported.
    tokens: Option<TokenUsage>,
    result: Option<Value>,
    /// That the provider reported an error, in agentctl's words: what it
    /// said is provider-controlled, so it goes to `metadata` at most.
    error: Option<&'static str>,
    /// Why the output cannot be trusted, once it cannot. agentctl's words
    /// only: the offending output is never quoted.
    malformed: Option<String>,
    metadata: Map<String, Value>,
}

impl Stream {
    fn malformed(&mut self, why: impl Into<String>) {
        self.malformed.get_or_insert_with(|| why.into());
    }

    fn session(&mut self, id: Option<&str>) {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            self.session = Some(id.to_owned());
        }
    }

    fn usage(&self) -> Usage {
        self.tokens
            .map_or(Usage::Unavailable, Usage::ProviderReported)
    }
}

/// Parses one output line as a JSON object with a string `type`.
fn parse_event(line: &[u8]) -> Result<(String, Value), &'static str> {
    let event: Value = serde_json::from_slice(line).map_err(|_| "provider output is not JSON")?;
    match event.get("type").and_then(Value::as_str) {
        Some(kind) => Ok((kind.to_owned(), event)),
        None => Err("a provider output event has no type"),
    }
}

/// How the provider ended, as agentctl observed it.
enum End {
    Exited {
        status: ExitStatus,
        held: bool,
    },
    /// agentctl terminated its domain first; the provider's own status is
    /// known only if it was reported before it was cut off.
    Cancelled {
        status: Option<ExitStatus>,
    },
}

/// An invocation's lifecycle domain and the threads serving its streams.
struct Process {
    domain: Domain,
    /// What terminating the domain established, once it was terminated.
    stopped: Option<Evidence>,
    threads: Vec<JoinHandle<()>>,
    _dir: TempDir,
    _files: Vec<NamedTempFile>,
}

impl Process {
    fn own(
        domain: Domain,
        streams: shim::Streams,
        dir: TempDir,
        pid: Option<u32>,
        shared: &Arc<Shared>,
        input: String,
    ) -> Self {
        shared.progress().pid = pid;
        let shim::Streams {
            input: mut stdin,
            output,
            error,
            control,
        } = streams;
        let writer = {
            let shared = Arc::clone(shared);
            // Closing the stream afterwards says the input is complete. That
            // it reached the provider is the shim's to report.
            thread::spawn(move || match stdin.write_all(input.as_bytes()) {
                Ok(()) => {
                    let _ = stdin.shutdown(Shutdown::Write);
                }
                Err(e) => {
                    shared
                        .progress()
                        .input_error
                        .get_or_insert_with(|| e.to_string());
                }
            })
        };
        let reader = {
            let shared = Arc::clone(shared);
            thread::spawn(move || read_events(output, &shared))
        };
        let diagnostics = {
            let shared = Arc::clone(shared);
            thread::spawn(move || read_stderr(error, &shared))
        };
        let supervisor = {
            let shared = Arc::clone(shared);
            thread::spawn(move || read_control(control, &shared))
        };
        Self {
            domain,
            stopped: None,
            threads: vec![writer, reader, diagnostics, supervisor],
            _dir: dir,
            _files: Vec::new(),
        }
    }

    /// Keeps `files`, which the provider's arguments name, until the
    /// invocation ends.
    fn with_files(mut self, files: Vec<NamedTempFile>) -> Self {
        self._files = files;
        self
    }

    /// Terminates the domain by procd's authority, if not yet terminated:
    /// the one way an invocation's processes are ended.
    fn terminate(&mut self) -> Result<&Evidence, String> {
        if self.stopped.is_none() {
            let evidence = terminate_domain(&self.domain).map_err(|e| format!("{e:#}"))?;
            self.stopped = Some(evidence);
        }
        Ok(self.stopped.as_ref().expect("terminated"))
    }

    /// Waits for the provider to end, terminating its domain once
    /// cancellation is requested, and classifies how the invocation ended.
    /// Unresolved unless procd proved the domain empty.
    fn supervise(
        mut self,
        invocation: InvocationId,
        shared: &Shared,
    ) -> Result<Outcome, Unresolved> {
        let ended = self.wait_or_terminate(shared);
        // However it ended, nothing it started is left behind, and nothing
        // relies on a parent's death: the domain is terminated.
        let cleaned = self.terminate().cloned();
        shared.progress().exited = ended.is_ok();
        conclude(
            invocation,
            ended,
            cleaned,
            || self.drain(),
            &shared.progress(),
            &shared.schema,
        )
    }

    /// How the provider ended; an error when that cannot be established.
    fn wait_or_terminate(&mut self, shared: &Shared) -> Result<End, String> {
        let mut terminated: Option<Instant> = None;
        loop {
            {
                let p = shared.progress();
                if let Some((raw, held)) = p.ended {
                    let status = shim::status_from_raw(raw).ok_or_else(|| {
                        format!("the provider's exit status ({raw}) is not one this host has")
                    })?;
                    return Ok(match terminated {
                        Some(_) => End::Cancelled {
                            status: Some(status),
                        },
                        None => End::Exited { status, held },
                    });
                }
                if p.control_closed {
                    return match terminated {
                        Some(_) => Ok(End::Cancelled { status: None }),
                        None => Err("the provider's shim ended without reporting how the \
                                     provider ended"
                            .into()),
                    };
                }
            }
            let now = Instant::now();
            match terminated {
                None if shared.cancel.load(Ordering::SeqCst) => {
                    self.terminate()
                        .map_err(|why| format!("termination not confirmed: {why}"))?;
                    terminated = Some(now);
                    continue;
                }
                Some(at) if now >= at + KILL_WAIT => {
                    return Err(
                        "termination not confirmed: the domain was terminated but its \
                                shim was not seen to end"
                            .into(),
                    );
                }
                _ => {}
            }
            thread::sleep(POLL);
        }
    }

    /// Waits a bounded time for the stream threads to finish; whether they
    /// did.
    fn drain(&self) -> bool {
        let deadline = Instant::now() + DRAIN;
        while !self.threads.iter().all(JoinHandle::is_finished) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL);
        }
        true
    }
}

/// Why `terminated` does not settle an invocation's lifecycle, if it does
/// not. Only evidence that proves the domain empty does: a termination
/// that succeeded is no proof.
fn unproven(terminated: &Result<Evidence, String>) -> Option<String> {
    match terminated {
        Ok(evidence) if evidence.proves_empty() => None,
        Ok(evidence) => Some(format!(
            "its lifecycle domain was terminated but not proven empty ({})",
            evidence.detail
        )),
        Err(why) => Some(format!("termination not confirmed: {why}")),
    }
}

/// What an invocation came to, given how the provider ended and what
/// terminating its domain established. The provider's result is not
/// lifecycle settlement: with the domain unproven it is discarded, success,
/// failure and cancellation alike, and the invocation stays unresolved.
fn conclude(
    invocation: InvocationId,
    ended: Result<End, String>,
    terminated: Result<Evidence, String>,
    drain: impl FnOnce() -> bool,
    p: &Progress,
    schema: &Validator,
) -> Result<Outcome, Unresolved> {
    if let Some(why) = unproven(&terminated) {
        return Err(Unresolved { invocation, why });
    }
    let held = matches!(ended, Ok(End::Exited { held: true, .. }));
    let drained = ended.is_ok() && drain() && !held;
    let (state, failure, mut diagnostic, status) = match ended {
        Ok(end) => {
            let (state, failure, diagnostic) = classify(&end, p, schema);
            let status = match end {
                End::Exited { status, .. } => Some(status),
                End::Cancelled { status } => status,
            };
            (state, failure, diagnostic, status)
        }
        // The provider's end is unknown, its domain proven gone.
        Err(why) => (InvocationState::Interrupted, None, Some(why), None),
    };
    if ended_ok(state) && !drained {
        let note = "its output stayed open after it exited, held by a process it started";
        diagnostic = Some(match diagnostic {
            Some(d) => format!("{d}; {note}"),
            None => note.to_owned(),
        });
    }
    Ok(Outcome {
        invocation,
        end: InvocationEnd {
            state,
            failure,
            diagnostic: diagnostic.map(|d| clip(&d, DIAGNOSTIC_LIMIT)),
            exit_code: status.and_then(|s| s.code()),
            provider_session: p.stream.session.clone(),
            usage: p.stream.usage(),
        },
        payload: (state == InvocationState::Succeeded)
            .then(|| p.stream.result.clone())
            .flatten(),
        provider_metadata: p.stream.metadata.clone(),
        stderr: String::from_utf8_lossy(&p.stderr).into_owned(),
    })
}

/// Whether `state` records that agentctl saw the provider end.
fn ended_ok(state: InvocationState) -> bool {
    state != InvocationState::Interrupted
}

impl Drop for Process {
    /// A domain agentctl stops owning is terminated, so that nothing in it
    /// lingers.
    fn drop(&mut self) {
        if self.stopped.is_none() {
            let _ = self.domain.terminate(KILL_WAIT);
        }
    }
}

/// How an invocation whose provider ended as `end` ended. The diagnostic is
/// agentctl's own: it quotes nothing the provider wrote. The domain is
/// already proven empty: see [`conclude`].
fn classify(
    end: &End,
    p: &Progress,
    schema: &Validator,
) -> (InvocationState, Option<FailureKind>, Option<String>) {
    use FailureKind::*;
    let failed = |kind, why: String| (InvocationState::Failed, Some(kind), Some(why));
    let stream = &p.stream;
    let status = match end {
        End::Cancelled { status } => {
            let how = status
                .map(|s| format!("; the provider {s}"))
                .unwrap_or_default();
            let why = format!(
                "cancelled by agentctl; its lifecycle domain was terminated and proven empty{how}"
            );
            return (InvocationState::Cancelled, None, Some(why));
        }
        End::Exited { status, .. } => *status,
    };
    if !p.input_delivered {
        let why = p
            .input_error
            .as_deref()
            .unwrap_or("delivery did not complete");
        return failed(InputFailed, format!("the input was not delivered: {why}"));
    }
    if let Some(error) = stream.error {
        return failed(ProviderError, error.to_owned());
    }
    if let Some(why) = &stream.malformed {
        return failed(MalformedOutput, why.clone());
    }
    match (&stream.result, status.success()) {
        (Some(result), true) if schema.is_valid(result) => (InvocationState::Succeeded, None, None),
        (Some(_), true) => failed(
            MalformedOutput,
            "the structured result does not satisfy the output schema".to_owned(),
        ),
        (Some(_), false) => failed(
            ExitStatus,
            format!("the provider {status} after its result"),
        ),
        (None, true) => failed(NoResult, format!("the provider {status} without a result")),
        (None, false) => failed(ExitStatus, format!("the provider {status}")),
    }
}

/// Serves the shim's control channel: how it says input and the provider
/// ended. Anything it does not understand is ignored, never acted on.
fn read_control(mut control: BufReader<TcpStream>, shared: &Shared) {
    let mut line = String::new();
    loop {
        line.clear();
        match control.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => match shim::Message::parse(&line) {
                Some(shim::Message::Input(Ok(()))) => shared.progress().input_delivered = true,
                Some(shim::Message::Input(Err(why))) => {
                    shared.progress().input_error.get_or_insert(why);
                }
                Some(shim::Message::Exit { raw, held }) => {
                    shared.progress().ended = Some((raw, held))
                }
                None => {}
            },
        }
    }
    shared.progress().control_closed = true;
}

fn read_events(stdout: TcpStream, shared: &Shared) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line);
        let mut p = shared.progress();
        match read {
            Ok(0) => return,
            Ok(_) => {
                p.last_output = Some(Instant::now());
                p.observe(line.trim_ascii());
            }
            // The OS error is kept: an abortive close (reset) is not an end
            // of stream, and which of them ended the read is diagnostic.
            Err(e) => {
                let seen = p.stream.result.is_some();
                return p.stream.malformed(format!(
                    "provider output could not be read ({:?}: {e}; result already seen: {seen})",
                    e.kind()
                ));
            }
        }
    }
}

/// Keeps the end of the provider's standard error.
fn read_stderr(mut stderr: TcpStream, shared: &Shared) {
    let mut buf = [0; 4096];
    while let Ok(n @ 1..) = stderr.read(&mut buf) {
        let mut p = shared.progress();
        p.last_output = Some(Instant::now());
        p.stderr.extend_from_slice(&buf[..n]);
        let excess = p.stderr.len().saturating_sub(STDERR_TAIL);
        p.stderr.drain(..excess);
    }
}

/// `text`, cut to at most `limit` bytes on a character boundary.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_needed_environment_passes() {
        let claude = &claude::ENV;
        for name in [
            "PATH",
            "Path",
            "HOME",
            "ANTHROPIC_API_KEY",
            "anthropic_base_url",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(claude.passes(name), "{name}");
        }
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "CLAUDECODE",
            "ANTHROPI",
            "É",
        ] {
            assert!(!claude.passes(name), "{name}");
        }
        let codex = &codex::ENV;
        for name in ["OPENAI_API_KEY", "CODEX_HOME", "SystemRoot"] {
            assert!(codex.passes(name), "{name}");
        }
        assert!(!codex.passes("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn events_are_typed_json_objects() {
        let (kind, event) = parse_event(br#"{"type":"x","n":1}"#).unwrap();
        assert_eq!((kind.as_str(), &event["n"]), ("x", &Value::from(1)));
        assert!(
            parse_event(b"plain prose")
                .unwrap_err()
                .contains("not JSON")
        );
        assert!(parse_event(br#"{"n":1}"#).unwrap_err().contains("no type"));
        assert!(parse_event(br#"["type"]"#).unwrap_err().contains("no type"));
    }

    #[test]
    fn clipping_respects_characters() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("ééééé", 8), "éé...");
    }

    use crate::procd::State;
    use serde_json::json;

    fn evidence(proven: bool) -> Evidence {
        Evidence {
            admission_closed: proven,
            authority_directed: true,
            emptiness_proven: proven,
            enforced: proven,
            final_state: Some(if proven {
                State::Empty
            } else {
                State::Unresolved
            }),
            detail: "fake".into(),
        }
    }

    fn status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        return std::os::unix::process::ExitStatusExt::from_raw(code << 8);
        #[cfg(windows)]
        return std::os::windows::process::ExitStatusExt::from_raw(code as u32);
    }

    /// A provider that reported a valid result, whatever its exit status.
    fn reported() -> Progress {
        let mut p = Progress::new(Decoder::Claude(claude::Decoder::default()));
        p.input_delivered = true;
        p.stream.result = Some(json!({"n": 7}));
        p
    }

    fn schema() -> Validator {
        jsonschema::validator_for(&json!({"type": "object"})).unwrap()
    }

    fn concluded(
        end: Result<End, String>,
        terminated: Result<Evidence, String>,
        p: &Progress,
    ) -> Result<Outcome, Unresolved> {
        conclude("1".parse().unwrap(), end, terminated, || true, p, &schema())
    }

    fn exited(code: i32) -> Result<End, String> {
        Ok(End::Exited {
            status: status(code),
            held: false,
        })
    }

    fn cancelled() -> Result<End, String> {
        Ok(End::Cancelled {
            status: Some(status(1)),
        })
    }

    #[test]
    fn a_valid_success_with_an_unproven_domain_is_never_settled() {
        // The provider exited 0 with a valid result, and terminating the
        // domain succeeded without proving it empty.
        let unresolved = concluded(exited(0), Ok(evidence(false)), &reported()).unwrap_err();
        assert!(unresolved.why.contains("not proven empty"), "{unresolved}");
        // Proof settles it, as before.
        let outcome = concluded(exited(0), Ok(evidence(true)), &reported()).unwrap();
        assert_eq!(outcome.end.state, InvocationState::Succeeded);
        assert_eq!(outcome.payload, Some(json!({"n": 7})));
    }

    #[test]
    fn a_known_failure_with_an_unproven_domain_stays_unresolved() {
        assert!(concluded(exited(3), Ok(evidence(false)), &reported()).is_err());
        let mut errored = reported();
        errored.stream.error = Some("the provider reported an error");
        assert!(concluded(exited(0), Ok(evidence(false)), &errored).is_err());
        let outcome = concluded(exited(3), Ok(evidence(true)), &reported()).unwrap();
        assert_eq!(outcome.end.state, InvocationState::Failed);
        assert_eq!(outcome.end.failure, Some(FailureKind::ExitStatus));
    }

    #[test]
    fn cancellation_with_an_unproven_domain_stays_unresolved() {
        assert!(concluded(cancelled(), Ok(evidence(false)), &reported()).is_err());
        let outcome = concluded(cancelled(), Ok(evidence(true)), &reported()).unwrap();
        assert_eq!(outcome.end.state, InvocationState::Cancelled);
    }

    #[test]
    fn a_lost_end_or_failed_termination_stays_unresolved() {
        // Not knowing how the provider ended is settled as interrupted only
        // where the domain is proven gone.
        let lost = || Err("the shim ended without a report".to_owned());
        assert!(concluded(lost(), Ok(evidence(false)), &reported()).is_err());
        let outcome = concluded(lost(), Ok(evidence(true)), &reported()).unwrap();
        assert_eq!(outcome.end.state, InvocationState::Interrupted);
        for end in [exited(0), cancelled(), lost()] {
            let why = concluded(end, Err("procd refused".into()), &reported())
                .unwrap_err()
                .why;
            assert!(why.contains("termination not confirmed"), "{why}");
        }
    }

    #[test]
    fn nothing_but_full_evidence_is_proof() {
        for spoil in 0..5 {
            let mut e = evidence(true);
            match spoil {
                0 => e.admission_closed = false,
                1 => e.authority_directed = false,
                2 => e.emptiness_proven = false,
                3 => e.enforced = false,
                _ => e.final_state = None,
            }
            assert!(concluded(exited(0), Ok(e), &reported()).is_err(), "{spoil}");
        }
    }

    #[test]
    fn an_abandoned_launch_is_settled_only_by_proof() {
        let why = || "no channel".to_owned();
        assert!(matches!(
            abandoned(Ok(evidence(true)), why()),
            Launched::Failed(FailureKind::SpawnFailed, _)
        ));
        assert!(matches!(
            abandoned(Ok(evidence(false)), why()),
            Launched::Unconfirmed(_)
        ));
        assert!(matches!(
            abandoned(Err(anyhow!("procd refused")), why()),
            Launched::Unconfirmed(_)
        ));
    }

    #[test]
    fn providers_are_named_as_configured() {
        for provider in [Provider::Claude, Provider::Codex] {
            assert_eq!(provider.name().parse::<Provider>().unwrap(), provider);
        }
        assert!("gemini".parse::<Provider>().is_err());
    }
}
