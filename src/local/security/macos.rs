//! macOS backend: a mandatory Seatbelt (`sandbox-exec`) profile compiled from the
//! OS-neutral policy.
//!
//! File *contents* and directory listings are deny-by-default: only the policy's
//! read roots are readable (plus the root directory entry itself, which dyld needs
//! to start any process). Metadata (`stat`) stays readable so path resolution
//! works; that leaks existence/size, never contents. Writes are deny-by-default.
//! Denied control-plane/credential paths always win over grants. Mach bootstrap is
//! denied by default, so no worker can have a system broker (LaunchServices) start
//! a process outside this confinement; only a provider frontend that authenticates
//! through the login Keychain regains the two securityd services it needs.
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

/// `<file>.<suffix>` siblings of an authorized file, as an anchored regex.
fn siblings(path: &Path) -> Result<String> {
    let text = sbpl(path)?;
    let text = &text[1..text.len() - 1];
    let mut escaped = String::new();
    for c in text.chars() {
        if ".^$*+?()[]{}|".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    Ok(format!("(regex #\"^{escaped}\\.[^/]+$\")"))
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
    for file in &fs.write_siblings {
        readable.push_str(&format!("(require-not {})", siblings(file)?));
    }
    let mut writable = String::new();
    for root in &fs.write_roots {
        writable.push_str(&format!("(require-not (subpath {}))", sbpl(root)?));
    }
    for file in &fs.write_files {
        writable.push_str(&format!("(require-not (literal {}))", sbpl(file)?));
    }
    for file in &fs.write_siblings {
        writable.push_str(&format!("(require-not {})", siblings(file)?));
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
            // A denial that re-allows an atomically replaceable file re-allows
            // its temporaries too, and nothing else beside it.
            if fs.write_siblings.contains(except) {
                filter.push_str(&format!("(require-not {})", siblings(except)?));
            }
        }
        for except in &denied.except_files {
            filter.push_str(&format!("(require-not (literal {}))", sbpl(except)?));
        }
        profile.push_str(&format!(
            "(deny {} (require-all {filter}))",
            operations.join(" ")
        ));
        // A denied tree that hides even metadata also hides the directories
        // leading to its re-allowed subpaths. Toolchains canonicalize the paths
        // they write (cargo's dep-info, for one), and `realpath` stats every
        // ancestor, so the carve-out is unusable unless exactly those ancestors
        // answer `stat`. Metadata only: their contents stay unreadable.
        if denied.read && denied.metadata {
            for except in &denied.except {
                for ancestor in except
                    .ancestors()
                    .skip(1)
                    .take_while(|a| a.starts_with(&denied.path))
                {
                    profile.push_str(&format!(
                        "(allow file-read-metadata (literal {}))",
                        sbpl(ancestor)?
                    ));
                }
            }
        }
    }
    profile.push_str("(deny process-info* (target others))(deny signal (target others))");
    if policy.network == NetworkPolicy::DenyAll {
        profile.push_str("(deny network*)");
    }
    // Mach bootstrap is deny-by-default. Without this, `(allow default)` leaves
    // LaunchServices reachable, and `/usr/bin/open` has launchd start a process
    // that is outside this sandbox, outside the job's process group, and holds
    // neither the job marker nor the sentinel descriptor: every filesystem,
    // network and credential denial above is voided at once and the
    // process-tree sweep reports a clean job over a live escapee.
    //
    // A narrower denial of the LaunchServices global names is NOT sufficient
    // (measured: the escape still succeeds), so the default is a full denial
    // and only the services a worker provably needs are re-allowed. SBPL takes
    // the last matching rule, so these allowances must follow the denial.
    profile.push_str("(deny mach-lookup)");
    if policy.keychain {
        // Native provider login reads the login Keychain, and securityd is the
        // only broker it needs. Measured minimum: without exactly these two
        // names `SecKeychainCopySearchList` already fails.
        profile.push_str(
            "(allow mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd.xpc\"))",
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
            ServiceBrokerDeny,
            seatbelt,
            "Seatbelt deny-default (deny mach-lookup)",
            if available {
                "Mach bootstrap is denied by default, so LaunchServices/launchd cannot start a process on a worker's behalf; only native-login securityd is re-allowed, and only for provider frontends that authenticate through the Keychain"
            } else {
                why
            },
        ),
        CapabilityReport::entry(
            ProcessTree,
            if available { BestEffort } else { Unsupported },
            "process group + job marker/sentinel sweep",
            "setsid/double-fork escapees holding the inherited job sentinel are found via libproc descriptor scans and killed, and a still-held sentinel is reported as unproven cleanup; all three identities (process group, marker, sentinel) depend on inheritance across fork, so a process started by a system broker rather than by the worker would be undetectable - that path is closed separately and unconditionally by SERVICE_BROKER_DENY, without which this capability would be LOGICAL_ONLY; the residual is a daemon that closes every inherited descriptor",
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
            "environment allowlist + read confinement + deny-default Mach bootstrap (Keychain reachable only for native-login frontends)",
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
                write_siblings: vec![],
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
                        except_files: vec![],
                        reason: "state",
                    },
                    DeniedPath {
                        path: "/w/.git".into(),
                        read: false,
                        metadata: false,
                        write: true,
                        except: vec![],
                        except_files: vec![],
                        reason: "git",
                    },
                ],
            },
            environment: BTreeMap::new(),
            resources: ResourceLimits::default(),
            secrets: vec![],
            keychain: false,
            program: "/bin/sh".into(),
            args: vec![],
            marker: "m".into(),
        }
    }

    /// An open Mach bootstrap (`(allow default)`) would let any worker reach
    /// LaunchServices and have launchd start an unsandboxed process. The
    /// bootstrap is deny-by-default, and the ONLY re-allowance is the
    /// securityd pair a native-login frontend needs.
    #[test]
    fn mach_bootstrap_is_denied_by_default_with_keychain_as_the_only_allowance() {
        const KEYCHAIN: &str = "(allow mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.securityd.xpc\"))";
        // Every class, network grant and keychain state gets the blanket denial.
        for class in [WorkerClass::Tool, WorkerClass::ProviderFrontend] {
            for network in [NetworkPolicy::DenyAll, NetworkPolicy::AllowAll] {
                let mut p = policy();
                p.class = class;
                p.network = network;
                let text = profile(&p).unwrap();
                assert!(
                    text.contains("(deny mach-lookup)"),
                    "{class:?}/{network:?} must deny the Mach bootstrap by default"
                );
                // No allowance without native login, whatever the class.
                assert!(
                    !text.contains("(allow mach-lookup"),
                    "{class:?}/{network:?} must not re-allow any service"
                );
            }
        }
        // Native login re-allows securityd, and nothing else.
        let mut keychain = policy();
        keychain.class = WorkerClass::ProviderFrontend;
        keychain.keychain = true;
        let text = profile(&keychain).unwrap();
        assert!(text.contains(KEYCHAIN));
        assert_eq!(text.matches("(allow mach-lookup").count(), 1);
        // SBPL takes the LAST matching rule: the allowance must follow the denial,
        // or the frontend would lose the Keychain instead of regaining it.
        assert!(
            text.find("(deny mach-lookup)").unwrap() < text.find(KEYCHAIN).unwrap(),
            "the keychain allowance must come after the blanket denial"
        );
        // A tool never gets it, even if the rest of the policy is identical.
        let mut tool = keychain;
        tool.class = WorkerClass::Tool;
        tool.keychain = false;
        assert!(!profile(&tool).unwrap().contains("(allow mach-lookup"));
    }

    /// The directories leading to a carved-out subpath of a
    /// metadata-hidden tree answer `stat` (and nothing else), or `realpath` on
    /// anything inside the carve-out fails for every toolchain.
    #[test]
    fn carve_out_ancestors_are_metadata_readable_and_nothing_else() {
        let mut p = policy();
        p.filesystem.denied[0].except = vec!["/s/data/runtime/scratch/id".into()];
        let text = profile(&p).unwrap();
        for ancestor in ["/s/data", "/s/data/runtime", "/s/data/runtime/scratch"] {
            assert!(text.contains(&format!(
                "(allow file-read-metadata (literal \"{ancestor}\"))"
            )));
        }
        // Only the tree's own ancestors, only metadata, and only literals.
        assert_eq!(text.matches("(allow file-read-metadata").count(), 3);
        assert!(!text.contains("(allow file-read-data") && !text.contains("(allow file-write"));
        // Never above the denied tree, and never for a tree that hides less.
        assert!(!text.contains("(literal \"/s\")"));
        let denial = text
            .find("(deny file-read* file-write* (require-all (subpath \"/s/data\")")
            .unwrap();
        assert!(denial < text.find("(allow file-read-metadata").unwrap());
        let mut git_only = policy();
        git_only.filesystem.denied[1].except = vec!["/w/.git/x".into()];
        git_only.filesystem.denied.remove(0);
        assert!(
            !profile(&git_only)
                .unwrap()
                .contains("(allow file-read-metadata")
        );
    }

    #[test]
    fn seatbelt_profile_denies_reads_by_default_and_protects_control_plane() {
        let text = profile(&policy()).unwrap();
        // The only read-side allowance is `stat` of a carve-out's ancestor
        // directories (see `carve_out_ancestors_are_metadata_readable_and_nothing_else`).
        assert!(
            text.match_indices("(allow file-read")
                .all(|(i, _)| text[i..].starts_with("(allow file-read-metadata (literal ")),
            "reads are denied by default; nothing but literal stat allowances may grant them"
        );
        assert!(text.contains("(deny file-read-data file-read-xattr (require-all (require-not (literal \"/\"))(require-not (subpath \"/usr\"))(require-not (subpath \"/w\"))"));
        assert!(text.contains("(deny file-read* file-write* (require-all (subpath \"/s/data\")(require-not (subpath \"/s/data/scratch\"))))"));
        assert!(text.contains("(deny file-write* (require-all (subpath \"/w/.git\")))"));
        assert!(text.contains("(deny network*)"));
        // Stronger than the former Tool-only two-name denial: nothing is reachable.
        assert!(text.contains("(deny mach-lookup)") && !text.contains("(allow mach-lookup"));
        let mut provider = policy();
        provider.class = WorkerClass::ProviderFrontend;
        provider.network = NetworkPolicy::AllowAll;
        let text = profile(&provider).unwrap();
        assert!(!text.contains("(deny network*)") && !text.contains("SecurityServer"));
        assert!(text.contains("(deny mach-lookup)"));
        let mut hostile = policy();
        hostile
            .filesystem
            .read_roots
            .push("/x\")(allow default".into());
        assert!(profile(&hostile).is_err());
    }
}
