//! macOS backend: a mandatory Seatbelt (`sandbox-exec`) profile compiled from the
//! OS-neutral policy.
//!
//! File *contents* and directory listings are deny-by-default: only the policy's
//! read roots are readable (plus the root directory entry itself, which dyld needs
//! to start any process). Metadata (`stat`) stays readable so path resolution
//! works; that leaks existence/size, never contents. Writes are deny-by-default.
//! Denied control-plane/credential paths always win over grants. Tool workers also
//! lose Keychain mach services so they cannot request provider credentials.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use super::*;

pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn sbpl(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::Invalid("non-UTF-8 sandbox path".into()))?;
    require(
        !text
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control()),
        "sandbox paths cannot contain quotes, backslashes or control characters",
    )?;
    Ok(format!("\"{text}\""))
}

/// Pure SBPL compilation (unit-tested on every platform).
pub(crate) fn profile(policy: &SecurityPolicy) -> Result<String> {
    let fs = &policy.filesystem;
    let mut readable = String::from("(require-not (literal \"/\"))");
    for root in fs.read_roots.iter().chain(&fs.write_roots) {
        readable.push_str(&format!("(require-not (subpath {}))", sbpl(root)?));
    }
    for file in fs.read_files.iter().chain(&fs.write_files) {
        readable.push_str(&format!("(require-not (literal {}))", sbpl(file)?));
    }
    let mut writable = String::new();
    for root in &fs.write_roots {
        writable.push_str(&format!("(require-not (subpath {}))", sbpl(root)?));
    }
    for file in &fs.write_files {
        writable.push_str(&format!("(require-not (literal {}))", sbpl(file)?));
    }
    let mut profile = format!(
        "(version 1)(allow default)(deny file-read-data file-read-xattr (require-all {readable}))(deny file-write* (require-all {writable}))"
    );
    for denied in &fs.denied {
        let mut operations = vec![];
        if denied.read {
            operations.push(if denied.metadata {
                "file-read*"
            } else {
                "file-read-data file-read-xattr"
            });
        }
        if denied.write {
            operations.push("file-write*");
        }
        if operations.is_empty() {
            continue;
        }
        let mut filter = format!("(subpath {})", sbpl(&denied.path)?);
        for except in &denied.except {
            filter.push_str(&format!("(require-not (subpath {}))", sbpl(except)?));
        }
        profile.push_str(&format!(
            "(deny {} (require-all {filter}))",
            operations.join(" ")
        ));
    }
    profile.push_str("(deny process-info* (target others))(deny signal (target others))");
    if policy.network == NetworkPolicy::DenyAll {
        profile.push_str("(deny network*)");
    }
    if policy.class == WorkerClass::Tool {
        profile.push_str(
            "(deny mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd.xpc\"))",
        );
    }
    Ok(profile)
}

fn report() -> CapabilityReport {
    use Capability::*;
    use CapabilityStatus::*;
    let available = Path::new(SANDBOX_EXEC).is_file();
    let seatbelt = if available { Enforced } else { Unsupported };
    let why = if available {
        ""
    } else {
        "sandbox-exec is missing; every worker is refused"
    };
    let mut capabilities = vec![
        CapabilityReport::entry(
            FilesystemRead,
            seatbelt,
            "Seatbelt deny-default file-read-data",
            if available {
                "only OS/toolchain roots, the workspace, scratch, Git metadata, the program's install directory and operator read_roots are readable; stat metadata remains visible"
            } else {
                why
            },
        ),
        CapabilityReport::entry(
            FilesystemWrite,
            seatbelt,
            "Seatbelt deny-default file-write*",
            if available {
                "scratch (and the workspace for executors/experiments) only; .git/.agentctl/agentctl state/credential stores always denied"
            } else {
                why
            },
        ),
        CapabilityReport::entry(
            FilesystemMetadataWrite,
            seatbelt,
            "Seatbelt file-write* (mode/owner/times/xattr)",
            why,
        ),
        CapabilityReport::entry(
            NetworkDeny,
            seatbelt,
            "Seatbelt (deny network*)",
            if available {
                "includes localhost and Unix-domain sockets"
            } else {
                why
            },
        ),
        CapabilityReport::entry(
            ProcessTree,
            if available { BestEffort } else { Unsupported },
            "process group + job marker/sentinel sweep",
            "setsid/double-fork escapees holding the inherited job sentinel are found via libproc descriptor scans and killed, and a still-held sentinel is reported as unproven cleanup; a daemon that closes every inherited descriptor is undetectable",
        ),
        CapabilityReport::entry(
            EnvironmentIsolation,
            Enforced,
            "allowlisted environment builder",
            "workers never inherit ambient environment",
        ),
        CapabilityReport::entry(
            CredentialIsolation,
            seatbelt,
            "environment allowlist + read confinement + Keychain mach-lookup denial for tools",
            "provider frontends and their own tool subprocesses share one sandbox and can read the frontend's auth files",
        ),
        CapabilityReport::entry(
            MemoryLimit,
            Unsupported,
            "none",
            "XNU does not enforce RLIMIT_AS/RLIMIT_DATA",
        ),
        CapabilityReport::entry(
            CpuLimit,
            BestEffort,
            "RLIMIT_CPU",
            "per process, not aggregated across the job",
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
        backend: "macos-seatbelt",
        platform: "macos",
        running_as_root: running_privileged(),
        capabilities,
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct Seatbelt;

#[cfg(target_os = "macos")]
impl PlatformSecurityBackend for Seatbelt {
    fn capabilities(&self) -> &CapabilityReport {
        static REPORT: std::sync::OnceLock<CapabilityReport> = std::sync::OnceLock::new();
        REPORT.get_or_init(report)
    }
    fn spawn(&self, policy: &SecurityPolicy, launch: &Launch<'_>) -> Result<Spawned> {
        require(
            Path::new(SANDBOX_EXEC).is_file(),
            "SECURITY_CAPABILITY_UNSUPPORTED: mandatory Seatbelt isolation (sandbox-exec) is unavailable; refusing unsandboxed execution",
        )?;
        let profile = profile(policy)?;
        let sentinel = tree::Sentinel::new()?;
        let mut args: Vec<OsString> = vec![
            "-p".into(),
            profile.into(),
            policy.program.clone().into_os_string(),
        ];
        args.extend(policy.args.iter().map(OsString::from));
        let command = unix::command(
            Path::new(SANDBOX_EXEC),
            &args,
            policy,
            launch,
            sentinel.child_fd(),
            false,
            || Ok(()),
        );
        unix::spawn(command, policy, sentinel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SecurityPolicy {
        SecurityPolicy {
            class: WorkerClass::Tool,
            network: NetworkPolicy::DenyAll,
            filesystem: FilesystemPolicy {
                workspace: "/w".into(),
                scratch: "/s/data/scratch".into(),
                read_roots: vec!["/usr".into(), "/w".into()],
                read_files: vec![],
                write_roots: vec!["/s/data/scratch".into()],
                write_files: vec!["/dev/null".into()],
                denied: vec![
                    DeniedPath {
                        path: "/s/data".into(),
                        read: true,
                        metadata: true,
                        write: true,
                        except: vec!["/s/data/scratch".into()],
                        reason: "state",
                    },
                    DeniedPath {
                        path: "/w/.git".into(),
                        read: false,
                        metadata: false,
                        write: true,
                        except: vec![],
                        reason: "git",
                    },
                ],
            },
            environment: BTreeMap::new(),
            resources: ResourceLimits::default(),
            secrets: vec![],
            program: "/bin/sh".into(),
            args: vec![],
            marker: "m".into(),
        }
    }

    #[test]
    fn seatbelt_profile_denies_reads_by_default_and_protects_control_plane() {
        let text = profile(&policy()).unwrap();
        assert!(!text.contains("(allow file-read"));
        assert!(text.contains("(deny file-read-data file-read-xattr (require-all (require-not (literal \"/\"))(require-not (subpath \"/usr\"))(require-not (subpath \"/w\"))"));
        assert!(text.contains("(deny file-read* file-write* (require-all (subpath \"/s/data\")(require-not (subpath \"/s/data/scratch\"))))"));
        assert!(text.contains("(deny file-write* (require-all (subpath \"/w/.git\")))"));
        assert!(text.contains("(deny network*)"));
        assert!(text.contains("com.apple.SecurityServer"));
        let mut provider = policy();
        provider.class = WorkerClass::ProviderFrontend;
        provider.network = NetworkPolicy::AllowAll;
        let text = profile(&provider).unwrap();
        assert!(!text.contains("(deny network*)") && !text.contains("SecurityServer"));
        let mut hostile = policy();
        hostile
            .filesystem
            .read_roots
            .push("/x\")(allow default".into());
        assert!(profile(&hostile).is_err());
    }
}
