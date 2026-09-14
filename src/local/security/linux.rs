//! Linux backend: unprivileged Landlock + seccomp-BPF + no_new_privs + rlimits.
//! No helper binary and no user namespaces are required.
//!
//! * Landlock (ABI >= 1) confines file reads and writes to explicit allow rules.
//!   Landlock is allow-only, so a denied control-plane path inside a granted root
//!   (e.g. `.git` in a writable workspace) is carved out by granting the root's
//!   other entries individually. Consequence: entries cannot be created or removed
//!   directly in such a carved directory (the workspace root); everything below
//!   ordinary subdirectories is unaffected. Landlock also blocks ptrace/`/proc/PID`
//!   inspection of processes outside the sandbox domain.
//! * seccomp denies `socket(2)` for every address family when the network is
//!   denied (socketpair stays available), io_uring (which bypasses seccomp),
//!   kernel keyring access, and `truncate(2)` on Landlock ABIs without TRUNCATE.
//! * Not mediated: chmod/chown/utimes/xattr metadata changes (reported as
//!   UNSUPPORTED `FILESYSTEM_METADATA_WRITE`), and `stat` of unreadable paths.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
use super::*;

pub(crate) mod access {
    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const READ_DIR: u64 = 1 << 3;
    pub const REFER: u64 = 1 << 13;
    pub const TRUNCATE: u64 = 1 << 14;
    pub const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
    /// Rights valid on a non-directory rule.
    pub const FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE;
}

/// All filesystem rights this backend handles for a given Landlock ABI.
pub(crate) fn handled_access(abi: u32) -> u64 {
    let mut handled = (1 << 13) - 1; // ABI 1: EXECUTE..=MAKE_SYM
    if abi >= 2 {
        handled |= access::REFER;
    }
    if abi >= 3 {
        handled |= access::TRUNCATE;
    }
    handled
}

const MAX_RULES: usize = 8192;

/// Grants `grant` minus `holes`. Directories that had to be split ("chains")
/// are reported separately: they receive no recursive right, only (for reads)
/// READ_DIR so they stay listable. Names inside a read-denied path therefore
/// remain listable on Linux; contents do not.
fn decompose(
    grant: &Path,
    holes: &[&DeniedPath],
    out: &mut Vec<PathBuf>,
    chains: &mut Vec<PathBuf>,
    budget: &mut usize,
) -> Result<()> {
    if holes
        .iter()
        .any(|h| grant.starts_with(&h.path) && !h.except.iter().any(|e| grant.starts_with(e)))
    {
        return Ok(());
    }
    if !holes
        .iter()
        .any(|h| h.path.starts_with(grant) && h.path != grant)
    {
        require(
            *budget > 0,
            "Landlock rule plan exceeds 8192 rules; narrow protected paths",
        )?;
        *budget -= 1;
        out.push(grant.to_path_buf());
        return Ok(());
    }
    // A denied path lies strictly inside this grant: grant each child instead.
    let Ok(meta) = fs::symlink_metadata(grant) else {
        return Ok(());
    };
    if !meta.is_dir() {
        return Ok(());
    }
    require(
        *budget > 0,
        "Landlock rule plan exceeds 8192 rules; narrow protected paths",
    )?;
    *budget -= 1;
    chains.push(grant.to_path_buf());
    for entry in fs::read_dir(grant)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            continue; // reachable through its target's own grant, if any
        }
        decompose(&entry.path(), holes, out, chains, budget)?;
    }
    Ok(())
}

/// Pure rule planning: (path, rights) pairs, with denied paths carved out.
pub(crate) fn plan_rules(fs_policy: &FilesystemPolicy, abi: u32) -> Result<Vec<(PathBuf, u64)>> {
    let read_holes: Vec<&DeniedPath> = fs_policy.denied.iter().filter(|d| d.read).collect();
    let write_holes: Vec<&DeniedPath> = fs_policy.denied.iter().filter(|d| d.write).collect();
    let mut rules: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut budget = MAX_RULES;
    let readable = fs_policy
        .read_roots
        .iter()
        .chain(&fs_policy.read_files)
        .chain(&fs_policy.write_roots)
        .chain(&fs_policy.write_files);
    let writable = fs_policy.write_roots.iter().chain(&fs_policy.write_files);
    for (grants, holes, rights, chain_rights) in [
        (
            readable.collect::<Vec<_>>(),
            &read_holes,
            access::READ,
            access::READ_DIR,
        ),
        (
            writable.collect::<Vec<_>>(),
            &write_holes,
            handled_access(abi),
            0,
        ),
    ] {
        for grant in grants {
            let (mut out, mut chains) = (vec![], vec![]);
            decompose(grant, holes, &mut out, &mut chains, &mut budget)?;
            for path in out {
                *rules.entry(path).or_default() |= rights;
            }
            if chain_rights != 0 {
                for path in chains {
                    *rules.entry(path).or_default() |= chain_rights;
                }
            }
        }
    }
    Ok(rules.into_iter().collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    fn audit(self) -> u32 {
        match self {
            Self::X86_64 => 0xC000_003E,
            Self::Aarch64 => 0xC000_00B7,
        }
    }
    /// (socket, keyctl, add_key, request_key, truncate)
    fn numbers(self) -> [u32; 5] {
        match self {
            Self::X86_64 => [41, 250, 248, 249, 76],
            Self::Aarch64 => [198, 219, 217, 218, 45],
        }
    }
    pub(crate) fn host() -> Option<Self> {
        if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else {
            None
        }
    }
}

const IO_URING: [u32; 3] = [425, 426, 427];
const EPERM: u32 = 1;
const EACCES: u32 = 13;
const ENOSYS: u32 = 38;
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// Layout-identical to `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Filter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// Pure seccomp program generation. Foreign-architecture syscalls (and the x32
/// ABI on x86_64) kill the process instead of bypassing the table.
pub(crate) fn seccomp_program(arch: Arch, deny_network: bool, deny_truncate: bool) -> Vec<Filter> {
    let op = |code, jt, jf, k| Filter { code, jt, jf, k };
    let [socket, keyctl, add_key, request_key, truncate] = arch.numbers();
    let mut denied: Vec<(u32, u32)> = IO_URING.iter().map(|nr| (*nr, ENOSYS)).collect();
    denied.extend([(keyctl, EPERM), (add_key, EPERM), (request_key, EPERM)]);
    if deny_network {
        denied.push((socket, EACCES));
    }
    if deny_truncate {
        denied.push((truncate, EPERM));
    }
    let mut program = vec![
        op(LD_W_ABS, 0, 0, 4),
        op(JEQ_K, 1, 0, arch.audit()),
        op(RET_K, 0, 0, RET_KILL_PROCESS),
        op(LD_W_ABS, 0, 0, 0),
    ];
    if arch == Arch::X86_64 {
        program.push(op(JGE_K, 0, 1, 0x4000_0000));
        program.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
    }
    for (nr, errno) in denied {
        program.push(op(JEQ_K, 0, 1, nr));
        program.push(op(RET_K, 0, 0, RET_ERRNO | errno));
    }
    program.push(op(RET_K, 0, 0, RET_ALLOW));
    program
}

fn report(abi: u32, seccomp: bool, arch: Option<Arch>) -> CapabilityReport {
    use Capability::*;
    use CapabilityStatus::*;
    let landlock = if abi >= 1 { Enforced } else { Unsupported };
    let filter = if seccomp && arch.is_some() {
        Enforced
    } else {
        Unsupported
    };
    // Without Landlock TRUNCATE (ABI < 3) write confinement relies on seccomp
    // denying truncate(2).
    let write = if abi >= 3 || (abi >= 1 && filter == Enforced) {
        Enforced
    } else {
        Unsupported
    };
    let landlock_detail = if abi >= 1 {
        format!("Landlock ABI v{abi}")
    } else {
        "Landlock is unavailable (kernel < 5.13, not compiled, or not in the LSM list); every worker is refused".into()
    };
    let mut capabilities = vec![
        CapabilityReport::entry(
            FilesystemRead,
            landlock,
            "Landlock allow-list (read/execute)",
            &landlock_detail,
        ),
        CapabilityReport::entry(
            FilesystemWrite,
            write,
            "Landlock allow-list (write) + seccomp truncate(2) on ABI < 3",
            &format!(
                "{landlock_detail}; denied paths inside a granted root are carved out, so entries cannot be created or removed directly in the workspace root"
            ),
        ),
        CapabilityReport::entry(
            FilesystemMetadataWrite,
            Unsupported,
            "none",
            "Landlock does not mediate chmod/chown/utimes/xattr; file contents stay protected",
        ),
        CapabilityReport::entry(
            NetworkDeny,
            filter,
            "seccomp: socket(2) denied for every family, io_uring denied",
            if filter == Enforced {
                "socketpair(AF_UNIX) remains for local pipes"
            } else {
                "seccomp filter mode or this CPU architecture is unsupported"
            },
        ),
        CapabilityReport::entry(
            ProcessTree,
            BestEffort,
            "process group + job marker/sentinel sweep via /proc",
            "no cgroup or PID namespace; escapees are found by the job marker in /proc/PID/environ or the inherited sentinel in /proc/PID/fd and killed; a daemon that clears its environment AND closes every inherited descriptor is undetectable",
        ),
        CapabilityReport::entry(
            EnvironmentIsolation,
            Enforced,
            "allowlisted environment builder",
            "workers never inherit ambient environment",
        ),
        CapabilityReport::entry(
            CredentialIsolation,
            landlock,
            "environment allowlist + Landlock read confinement",
            "provider frontends and their own tool subprocesses share one sandbox and can read the frontend's auth files",
        ),
        CapabilityReport::entry(
            MemoryLimit,
            BestEffort,
            "RLIMIT_AS",
            "virtual address space per process, not aggregated",
        ),
        CapabilityReport::entry(
            CpuLimit,
            BestEffort,
            "RLIMIT_CPU",
            "per process, not aggregated",
        ),
        CapabilityReport::entry(
            ProcessLimit,
            BestEffort,
            "RLIMIT_NPROC",
            "per user: current process count + max_processes",
        ),
        CapabilityReport::entry(OpenFileLimit, BestEffort, "RLIMIT_NOFILE", "per process"),
        CapabilityReport::entry(
            FileSizeLimit,
            BestEffort,
            "RLIMIT_FSIZE",
            "per file, per process",
        ),
    ];
    capabilities.extend(CapabilityReport::controller_entries());
    CapabilityReport {
        backend: "linux-landlock-seccomp",
        platform: "linux",
        running_as_root: running_privileged(),
        capabilities,
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::LandlockSeccomp;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::OnceLock;

    pub(crate) struct LandlockSeccomp;

    struct Probe {
        abi: u32,
        seccomp: bool,
        report: CapabilityReport,
    }

    fn probe() -> &'static Probe {
        static PROBE: OnceLock<Probe> = OnceLock::new();
        PROBE.get_or_init(|| {
            // SAFETY: LANDLOCK_CREATE_RULESET_VERSION with a null attribute only
            // queries the ABI version.
            let version = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    1u32,
                )
            };
            let abi = u32::try_from(version).unwrap_or(0);
            // SAFETY: PR_GET_SECCOMP has no pointer arguments.
            let mode = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
            let errno_action = fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail")
                .map(|s| s.split_whitespace().any(|a| a == "errno"))
                .unwrap_or(true);
            let seccomp = (mode == 0 || mode == 2) && errno_action;
            Probe {
                abi,
                seccomp,
                report: report(abi, seccomp, Arch::host()),
            }
        })
    }

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }

    fn build_ruleset(fs_policy: &FilesystemPolicy, abi: u32) -> Result<OwnedFd> {
        let handled = handled_access(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        // SAFETY: valid attribute pointer and exact size.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: the kernel returned a fresh descriptor we now own.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        for (path, rights) in plan_rules(fs_policy, abi)? {
            let c_path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| Error::Invalid("sandbox path contains NUL".into()))?;
            // SAFETY: valid C string; O_PATH does not read the file.
            let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if raw < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(Error::Invalid(format!("{}: {error}", path.display())));
            }
            // SAFETY: fresh descriptor from open.
            let owned = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: zeroed stat is a valid out-parameter.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(owned.as_raw_fd(), &mut stat) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let directory = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
            let allowed = handled
                & if directory {
                    rights
                } else {
                    rights & access::FILE
                };
            if allowed == 0 {
                continue;
            }
            let beneath = PathBeneath {
                allowed_access: allowed,
                parent_fd: owned.as_raw_fd(),
            };
            // SAFETY: LANDLOCK_RULE_PATH_BENEATH (1) with a packed attribute.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    1u32,
                    &beneath as *const PathBeneath,
                    0u32,
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                return Err(Error::Invalid(format!(
                    "Landlock rule for {} failed: {error}",
                    path.display()
                )));
            }
        }
        Ok(ruleset)
    }

    impl PlatformSecurityBackend for LandlockSeccomp {
        fn capabilities(&self) -> &CapabilityReport {
            &probe().report
        }
        fn spawn(&self, policy: &SecurityPolicy, launch: &Launch<'_>) -> Result<Spawned> {
            let probe = probe();
            let arch = Arch::host().ok_or_else(|| {
                Error::Invalid(
                    "SECURITY_CAPABILITY_UNSUPPORTED: no seccomp table for this CPU architecture"
                        .into(),
                )
            })?;
            require(
                probe.abi >= 1 && probe.seccomp,
                "SECURITY_CAPABILITY_UNSUPPORTED: Landlock and seccomp are both required on Linux",
            )?;
            let ruleset = build_ruleset(&policy.filesystem, probe.abi)?;
            let ruleset_fd = ruleset.as_raw_fd();
            let filter: Box<[Filter]> = seccomp_program(
                arch,
                policy.network == NetworkPolicy::DenyAll,
                probe.abi < 3,
            )
            .into_boxed_slice();
            let sentinel = tree::Sentinel::new()?;
            let args: Vec<OsString> = policy.args.iter().map(OsString::from).collect();
            let command = unix::command(
                &policy.program,
                &args,
                policy,
                launch,
                sentinel.child_fd(),
                true,
                move || {
                    let program = libc::sock_fprog {
                        len: filter.len() as u16,
                        filter: filter.as_ptr() as *mut libc::sock_filter,
                    };
                    // SAFETY: async-signal-safe syscalls on data prepared before fork.
                    unsafe {
                        if libc::prctl(
                            libc::PR_SET_NO_NEW_PRIVS,
                            1 as libc::c_ulong,
                            0 as libc::c_ulong,
                            0 as libc::c_ulong,
                            0 as libc::c_ulong,
                        ) != 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::prctl(
                            libc::PR_SET_SECCOMP,
                            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                            &program as *const libc::sock_fprog,
                        ) != 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                },
            );
            let spawned = unix::spawn(command, policy, sentinel);
            drop(ruleset);
            spawned
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn hardcoded_syscall_table_matches_libc_for_this_architecture() {
            let [socket, keyctl, add_key, request_key, truncate] = Arch::host().unwrap().numbers();
            assert_eq!(socket as libc::c_long, libc::SYS_socket);
            assert_eq!(keyctl as libc::c_long, libc::SYS_keyctl);
            assert_eq!(add_key as libc::c_long, libc::SYS_add_key);
            assert_eq!(request_key as libc::c_long, libc::SYS_request_key);
            assert_eq!(truncate as libc::c_long, libc::SYS_truncate);
            assert_eq!(IO_URING[0] as libc::c_long, libc::SYS_io_uring_setup);
            assert_eq!(IO_URING[1] as libc::c_long, libc::SYS_io_uring_enter);
            assert_eq!(IO_URING[2] as libc::c_long, libc::SYS_io_uring_register);
            assert_eq!(
                std::mem::size_of::<Filter>(),
                std::mem::size_of::<libc::sock_filter>()
            );
            assert_eq!(std::mem::size_of::<PathBeneath>(), 12);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal classic-BPF interpreter for the opcodes the generator emits.
    fn run(program: &[Filter], arch: u32, nr: u32) -> u32 {
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let f = program[pc];
            match f.code {
                LD_W_ABS => {
                    acc = if f.k == 4 { arch } else { nr };
                    pc += 1;
                }
                JEQ_K => pc += 1 + if acc == f.k { f.jt } else { f.jf } as usize,
                JGE_K => pc += 1 + if acc >= f.k { f.jt } else { f.jf } as usize,
                RET_K => return f.k,
                other => panic!("unexpected opcode {other:#x}"),
            }
        }
    }

    #[test]
    fn seccomp_program_denies_network_io_uring_keyrings_and_foreign_abis() {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            let [socket, keyctl, _, _, truncate] = arch.numbers();
            let deny = seccomp_program(arch, true, true);
            let allow = seccomp_program(arch, false, false);
            let audit = arch.audit();
            assert_eq!(run(&deny, audit, socket), RET_ERRNO | EACCES);
            assert_eq!(run(&allow, audit, socket), RET_ALLOW);
            assert_eq!(run(&deny, audit, 425), RET_ERRNO | ENOSYS);
            assert_eq!(run(&allow, audit, keyctl), RET_ERRNO | EPERM);
            assert_eq!(run(&deny, audit, truncate), RET_ERRNO | EPERM);
            assert_eq!(run(&allow, audit, truncate), RET_ALLOW);
            assert_eq!(run(&deny, audit, 0), RET_ALLOW);
            assert_eq!(run(&deny, 0x4000_0003, socket), RET_KILL_PROCESS);
        }
        let x86 = seccomp_program(Arch::X86_64, false, false);
        assert_eq!(
            run(&x86, Arch::X86_64.audit(), 0x4000_0029),
            RET_KILL_PROCESS
        );
    }

    #[test]
    fn landlock_plan_carves_control_plane_out_of_writable_workspace() {
        let base = std::env::temp_dir().join(format!(
            "agentctl-landlock-plan-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap()
        ));
        for dir in [
            "ws/.git/hooks",
            "ws/src/deep",
            "ws/secrets",
            "state/data/scratch",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::write(base.join("ws/Cargo.toml"), "").unwrap();
        fs::write(base.join("ws/secrets/key"), "").unwrap();
        fs::write(base.join("ws/secrets/public"), "").unwrap();
        let ws = base.join("ws");
        let hole = |path: PathBuf, read: bool| DeniedPath {
            path,
            read,
            metadata: false,
            write: true,
            except: vec![],
            reason: "fixture",
        };
        let policy = FilesystemPolicy {
            workspace: ws.clone(),
            scratch: base.join("state/data/scratch"),
            read_roots: vec![ws.clone(), base.join("state")],
            read_files: vec![],
            write_roots: vec![ws.clone(), base.join("state/data/scratch")],
            write_files: vec![],
            denied: vec![
                hole(ws.join(".git"), false),
                hole(ws.join("secrets/key"), true),
                DeniedPath {
                    except: vec![base.join("state/data/scratch")],
                    metadata: true,
                    ..hole(base.join("state/data"), true)
                },
            ],
        };
        let rules: BTreeMap<PathBuf, u64> = plan_rules(&policy, 3).unwrap().into_iter().collect();
        let writes = |p: &Path| rules.get(p).is_some_and(|r| r & access::WRITE_FILE != 0);
        let reads = |p: &Path| rules.get(p).is_some_and(|r| r & access::READ_FILE != 0);
        let lists = |p: &Path| rules.get(p).is_some_and(|r| r & access::READ_DIR != 0);
        assert!(lists(&ws) && !reads(&ws), "carved root stays listable only");
        assert!(
            !writes(&ws),
            "workspace root is carved, not granted recursively"
        );
        assert!(
            reads(&ws.join(".git")) && !writes(&ws.join(".git")),
            ".git readable, never writable"
        );
        assert!(writes(&ws.join("src")) && writes(&ws.join("Cargo.toml")));
        assert!(writes(&ws.join("secrets/public")) && !rules.contains_key(&ws.join("secrets/key")));
        assert!(!reads(&base.join("state")) && !rules.contains_key(&base.join("state/data")));
        assert!(writes(&base.join("state/data/scratch")));
        let _ = fs::remove_dir_all(&base);
    }
}
