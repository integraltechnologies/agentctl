use super::*;
use crate::local::security::{self, tree::TreeOutcome};
use std::{
    fs::File,
    io::{Read, Write},
    process::Child,
    thread::JoinHandle,
    time::Instant,
};

pub const MAX_OUTPUT: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone)]
pub struct ProcessSpec {
    /// Hash of the validated canonical policy used to build this specification.
    pub project_policy_hash: Option<String>,
    pub native_auth: Option<super::credentials::NativeAuth>,
    pub api_key: Option<(String, String)>,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub input: Vec<u8>,
    pub cwd: PathBuf,
    pub workspace: PathBuf,
    pub scratch: PathBuf,
    pub data_root: PathBuf,
    pub config_root: PathBuf,
    pub writable: bool,
    pub network: bool,
    pub timeout_ms: u64,
    pub git_directories: Vec<PathBuf>,
    pub protected: Vec<super::super::config::ProtectedRule>,
    pub credential_env: Vec<String>,
    /// Attempt-owned structured JSONL channel. This is not a credential and is
    /// exposed only to experiment processes that explicitly opt into Stage 9B.
    pub experiment_event_file: Option<PathBuf>,
    /// Provider frontend (network + native auth) or untrusted tool (neither).
    pub class: security::WorkerClass,
    /// agentctl cache root; like the data/config roots it is always denied.
    pub cache_root: PathBuf,
    /// Effective machine-owned security policy (a project can only tighten it).
    pub security: security::SecurityConfig,
    /// Inherited by the entire child family so a controller crash cannot release
    /// the workspace lease while an orphan can still edit files.
    pub(crate) lock_fd: Option<i32>,
}
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub failure: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationOutcome {
    Applied,
    AlreadyExited,
    Failed(String),
}
pub trait RunningProcess {
    fn pid(&self) -> Option<u32>;
    /// True only when the last poll directly confirmed the owned provider child
    /// had not exited. Output-pending, a PID, or a successful launch is not enough.
    /// Unsupported adapters conservatively provide no liveness evidence.
    fn liveness_confirmed(&self) -> bool {
        false
    }
    fn poll(&mut self) -> Result<Option<ProcessOutput>>;
    fn cancel(&mut self) -> Result<CancellationOutcome>;
}
/// Trusted host-side command launcher; injectable for offline deterministic tests.
pub trait CheckLauncher {
    fn launch(&mut self, spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>>;
    fn provenance(&self) -> &'static str;
}
pub struct NativeChecks;
pub(super) struct ScratchCleanup(pub PathBuf);
impl Drop for ScratchCleanup {
    fn drop(&mut self) {
        // Only controller-created, issued-job directories are wrapped in this
        // guard. Provider-native homes/authentication are never cleanup targets.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl CheckLauncher for NativeChecks {
    fn launch(&mut self, spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>> {
        Ok(Box::new(NativeProcess::launch(spec)?))
    }
    fn provenance(&self) -> &'static str {
        "OS_SANDBOX"
    }
}

pub struct NativeProcess {
    child: Child,
    stdout: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    input: Option<JoinHandle<std::io::Result<()>>>,
    started: Instant,
    timeout: u64,
    failure: Option<String>,
    reaped: bool,
    live_child: bool,
    timeout_termination_attempted: bool,
    secrets: Vec<String>,
    tree: security::tree::ProcessTree,
}
impl ProcessSpec {
    pub fn recheck_policy(&self) -> Result<()> {
        if let Some(expected) = &self.project_policy_hash {
            require(
                ProjectConfig::load(&self.workspace)
                    .ok()
                    .and_then(|p| planning::hash(&p).ok())
                    .as_ref()
                    == Some(expected),
                "SOURCE_DRIFT: project policy changed before process spawn; replan/revalidation required",
            )?;
        }
        Ok(())
    }
}
fn drain(mut stream: impl Read + Send + 'static) -> JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = vec![];
        let mut buf = [0; 8192];
        loop {
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if bytes.len() < MAX_OUTPUT + 1 {
                let n = n.min(MAX_OUTPUT + 1 - bytes.len());
                bytes.extend_from_slice(&buf[..n]);
            }
        }
        Ok(bytes)
    })
}
impl NativeProcess {
    /// The only native spawn path for providers, checks and experiments: the spec
    /// is compiled into an OS-neutral `SecurityPolicy`, the active platform
    /// backend must enforce every required capability (fail closed), and only
    /// then is the process started inside that backend's confinement.
    pub fn launch(spec: &ProcessSpec) -> Result<Self> {
        // Canonical policy drift is the first pre-spawn gate, then confinement.
        spec.recheck_policy()?;
        paths::ensure_directory(&spec.scratch)?;
        paths::ensure_directory(&spec.scratch.join("home"))?;
        let policy = security::compile(spec)?;
        spec.recheck_policy()?;
        let security::Spawned { mut child, tree } = security::launch(
            &policy,
            &security::Launch {
                cwd: &spec.cwd,
                lock_fd: spec.lock_fd,
            },
        )?;
        let stdout = Some(drain(child.stdout.take().expect("piped stdout")));
        let stderr = Some(drain(child.stderr.take().expect("piped stderr")));
        let mut stdin = child.stdin.take().expect("piped stdin");
        let bytes = spec.input.clone();
        let input = Some(std::thread::spawn(move || stdin.write_all(&bytes)));
        Ok(Self {
            child,
            stdout,
            stderr,
            input,
            started: Instant::now(),
            timeout: spec.timeout_ms,
            failure: None,
            reaped: false,
            live_child: false,
            timeout_termination_attempted: false,
            secrets: policy.secrets,
            tree,
        })
    }
    /// Process group on Unix, Job Object on Windows.
    fn stop_family(&mut self) -> std::io::Result<()> {
        self.tree.terminate()
    }
    fn add_failure(&mut self, text: String) {
        self.failure = Some(match self.failure.take() {
            Some(existing) => format!("{existing}; {text}"),
            None => text,
        });
    }
}
impl RunningProcess for NativeProcess {
    fn liveness_confirmed(&self) -> bool {
        self.live_child
    }
    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
    fn cancel(&mut self) -> Result<CancellationOutcome> {
        self.live_child = false;
        if self.child.try_wait()?.is_some() {
            return Ok(CancellationOutcome::AlreadyExited);
        }
        match self.stop_family() {
            Ok(()) => {
                self.failure = Some("cancelled".into());
                Ok(CancellationOutcome::Applied)
            }
            Err(error) => {
                if self.child.try_wait()?.is_some() {
                    Ok(CancellationOutcome::AlreadyExited)
                } else {
                    Ok(CancellationOutcome::Failed(error.to_string()))
                }
            }
        }
    }
    fn poll(&mut self) -> Result<Option<ProcessOutput>> {
        self.live_child = false;
        require(!self.reaped, "process output already collected")?;
        let status = match self.child.try_wait()? {
            Some(status) => status,
            None => {
                if self.started.elapsed().as_millis() >= self.timeout as u128
                    && !self.timeout_termination_attempted
                {
                    self.timeout_termination_attempted = true;
                    if self.stop_family().is_ok() {
                        self.failure = Some("timeout".into());
                    }
                }
                self.live_child = true;
                return Ok(None);
            }
        };
        // The direct child exiting proves nothing about its descendants: kill the
        // group, hunt escapees, and report any cleanup that cannot be proven.
        match self.tree.reap() {
            TreeOutcome::Clean => {}
            TreeOutcome::EscapedTerminated(count) => {
                if self.failure.is_none() {
                    self.failure = Some(format!(
                        "process tree: {count} descendant(s) escaped the job's process group and were terminated; background work outliving the job is not accepted"
                    ));
                }
            }
            TreeOutcome::Unproven(detail) => {
                self.add_failure(format!("process tree cleanup unproven: {detail}"))
            }
        }
        #[cfg(unix)]
        if self.failure.is_none() {
            use std::os::unix::process::ExitStatusExt;
            match status.signal() {
                Some(libc::SIGXCPU) => {
                    self.failure = Some("resource limit exceeded: cpu time (RLIMIT_CPU)".into())
                }
                Some(libc::SIGXFSZ) => {
                    self.failure = Some("resource limit exceeded: file size (RLIMIT_FSIZE)".into())
                }
                _ => {}
            }
        }
        let deadline = Instant::now();
        while self.stdout.as_ref().is_some_and(|h| !h.is_finished())
            || self.stderr.as_ref().is_some_and(|h| !h.is_finished())
            || self.input.as_ref().is_some_and(|h| !h.is_finished())
        {
            require(
                deadline.elapsed().as_millis() < 500,
                "child streams remained open after process-family shutdown; refusing acceptance",
            )?;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.reaped = true;
        let join = |h: Option<JoinHandle<std::io::Result<Vec<u8>>>>| -> Result<Vec<u8>> {
            Ok(h.expect("output thread")
                .join()
                .map_err(|_| Error::Invalid("output capture thread failed".into()))??)
        };
        let mut stdout = join(self.stdout.take())?;
        let mut stderr = join(self.stderr.take())?;
        if let Some(input) = self.input.take() {
            let _ = input.join();
        }
        if stdout.len() > MAX_OUTPUT || stderr.len() > MAX_OUTPUT {
            self.failure = Some("output limit exceeded".into());
            stdout.truncate(MAX_OUTPUT);
            stderr.truncate(MAX_OUTPUT);
        }
        for secret in &self.secrets {
            stdout = String::from_utf8_lossy(&stdout)
                .replace(secret, "[REDACTED]")
                .into_bytes();
            stderr = String::from_utf8_lossy(&stderr)
                .replace(secret, "[REDACTED]")
                .into_bytes();
        }
        stdout = super::credentials::redact(&stdout);
        stderr = super::credentials::redact(&stderr);
        Ok(Some(ProcessOutput {
            exit: status.code(),
            stdout,
            stderr,
            failure: self.failure.clone(),
        }))
    }
}
impl Drop for NativeProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.stop_family();
            let _ = self.child.wait();
            let _ = self.tree.reap();
        }
    }
}

/// True when this host's security backend enforces the baseline every worker
/// requires (filesystem read/write confinement and environment isolation).
pub fn sandbox_available() -> bool {
    security::baseline_enforced()
}

pub(super) struct WorkspaceLease {
    #[cfg_attr(not(unix), allow(dead_code))]
    file: File,
}
impl WorkspaceLease {
    pub(super) fn acquire(path: &Path) -> Result<Self> {
        use std::fs::OpenOptions;
        paths::ensure_directory(
            path.parent()
                .ok_or_else(|| Error::Invalid("lease parent missing".into()))?,
        )?;
        paths::check_file(path, true)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd; // SAFETY: valid owned descriptor; nonblocking exclusive lock.
            require(
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "workspace has a live controller/provider lease; no duplicate launch",
            )?;
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Err(Error::Invalid(
                "runtime workspace leases require Unix".into(),
            ))
        }
        #[cfg(unix)]
        Ok(Self { file })
    }
    pub(super) fn fd(&self) -> Option<i32> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            Some(self.file.as_raw_fd())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::super::provider::{ClaudeAdapter, CodexAdapter, JobInput, ProviderAdapter};
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agentctl-native-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let f = Self(std::fs::canonicalize(path).unwrap());
            for name in ["repo", "repo/.git", "data", "config", "cache", "scratch"] {
                std::fs::create_dir_all(f.0.join(name)).unwrap();
            }
            std::fs::write(f.0.join("config/secret"), "fixture secret").unwrap();
            f
        }
        fn spec(&self, writable: bool) -> ProcessSpec {
            ProcessSpec {
                project_policy_hash: None,
                native_auth: None,
                api_key: None,
                executable: "/usr/bin/true".into(),
                args: vec![],
                input: vec![],
                cwd: self.0.join("repo"),
                workspace: self.0.join("repo"),
                scratch: self.0.join("scratch"),
                data_root: self.0.join("data"),
                config_root: self.0.join("config"),
                writable,
                network: false,
                timeout_ms: 3000,
                git_directories: vec![self.0.join("repo/.git")],
                protected: vec![],
                credential_env: vec![],
                experiment_event_file: None,
                class: security::WorkerClass::Tool,
                cache_root: self.0.join("cache"),
                security: Default::default(),
                lock_fd: None,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn finish(p: &mut dyn RunningProcess) -> ProcessOutput {
        loop {
            if let Some(o) = p.poll().unwrap() {
                return o;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
    fn native_liveness_requires_owned_child_poll_and_is_cleared_on_exit() {
        let f = Fixture::new();
        let mut spec = f.spec(false);
        spec.executable = "/bin/sleep".into();
        spec.args = vec!["30".into()];
        let mut child = NativeProcess::launch(&spec).unwrap();
        assert!(!child.liveness_confirmed()); // spawn/PID alone is not evidence
        assert!(child.poll().unwrap().is_none());
        assert!(child.liveness_confirmed());
        child.cancel().unwrap();
        assert!(!child.liveness_confirmed());
        finish(&mut child);
        assert!(!child.liveness_confirmed());
    }
    #[test]
    #[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
    fn native_write_read_boundaries_timeout_cancel_and_output_limits() {
        let f = Fixture::new();
        for writable in [false, true] {
            let mut spec = f.spec(writable);
            spec.executable = "/bin/sh".into();
            spec.args=vec!["-c".into(),"if cat \"$1\"; then exit 10; fi; if printf bad > \"$2\"; then exit 11; fi; if printf bad > \"$3\"; then exit 12; fi; printf allowed > \"$4\"".into(),"fixture".into(),f.0.join("config/secret").display().to_string(),f.0.join("data/forbidden").display().to_string(),f.0.join("repo/.git/forbidden").display().to_string(),f.0.join("repo/result").display().to_string()];
            let output = finish(&mut NativeProcess::launch(&spec).unwrap());
            assert_eq!(output.exit == Some(0), writable, "{output:?}");
            assert!(!f.0.join("data/forbidden").exists());
            assert!(!f.0.join("repo/.git/forbidden").exists());
        }
        let mut spec = f.spec(false);
        spec.executable = "/bin/sleep".into();
        spec.args = vec!["10".into()];
        spec.timeout_ms = 30;
        assert_eq!(
            finish(&mut NativeProcess::launch(&spec).unwrap())
                .failure
                .as_deref(),
            Some("timeout")
        );
        spec.timeout_ms = 3000;
        let mut p = NativeProcess::launch(&spec).unwrap();
        p.cancel().unwrap();
        assert_eq!(finish(&mut p).failure.as_deref(), Some("cancelled"));
        spec.executable = "/usr/bin/head".into();
        spec.args = vec!["-c".into(), "5000000".into(), "/dev/zero".into()];
        let output = finish(&mut NativeProcess::launch(&spec).unwrap());
        assert_eq!(output.failure.as_deref(), Some("output limit exceeded"));
        assert_eq!(output.stdout.len(), MAX_OUTPUT);
    }
    #[test]
    #[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
    fn native_adapter_argv_fresh_homes_and_strict_json_without_model_calls() {
        let f = Fixture::new();
        for claude in [false, true] {
            let path = f.0.join(if claude { "mock claude" } else { "mock codex" });
            let response = if claude {
                r#"{"result":"{\"fixture\":true}","usage":{"input_tokens":12,"output_tokens":4}}"#
            } else {
                r#"{"fixture":true}"#
            };
            std::fs::write(&path,format!("#!/bin/sh\ncase \" $* \" in *' status '*) printf '%s' '{{\"loggedIn\":true}}'; exit 0;; esac\nprintf '%s\\n' \"$@\" > \"$TMPDIR/argv\"\ncat > \"$TMPDIR/input\"\nprintf '%s' \"$HOME\" > \"$TMPDIR/home-path\"\nprintf '%s' '{response}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut adapter: Box<dyn ProviderAdapter> = if claude {
                Box::new(ClaudeAdapter {
                    executable: path,
                    authentication: Default::default(),
                })
            } else {
                Box::new(CodexAdapter {
                    executable: path,
                    authentication: Default::default(),
                })
            };
            let mut input = JobInput {
                compiled: None,
                ownership: EngineeringSession {
                    id: "engineering:fixture".into(),
                    supervisor_instance_id: AgentId::new("agent:supervisor").unwrap(),
                }
                .worker(AgentId::new("agent:runtime:fixture").unwrap()),
                job_id: JobId::new("runtime:fixture").unwrap(),
                session_id: "12345678-1234-4123-a123-123456789012".into(),
                role: AgentRole::Verifier,
                plan_id: Some(PlanId::new("plan:fixture").unwrap()),
                task_id: Some(TaskId::new("task:fixture").unwrap()),
                repository_id: RepositoryId::for_common_directory(&f.0.join("repo/.git")),
                workspace_id: serde_json::from_str(&format!("\"workspace-{}\"", "a".repeat(64)))
                    .unwrap(),
                source: SourceStateRef {
                    revision: "fixture".into(),
                    worktree_diff_hash: None,
                },
                artifact: serde_json::json!({"task":"bounded fixture"}),
            };
            input.compiled = Some(
                super::super::prompt::compile(
                    &super::super::routing::builtin("verifier", &RuntimeConfig::default()),
                    "fixture",
                    &input,
                )
                .unwrap(),
            );
            let role = RoleConfig {
                provider: "fixture".into(),
                model: Some("opaque model ; not shell".into()),
                effort: Some("low".into()),
            };
            let output = finish(
                adapter
                    .launch(&input, f.spec(false), &role)
                    .unwrap()
                    .as_mut(),
            );
            assert_eq!(output.exit, Some(0), "{output:?}");
            assert_eq!(
                adapter.collect(&output).unwrap(),
                serde_json::json!({"fixture":true})
            );
            let argv = std::fs::read_to_string(f.0.join("scratch/argv")).unwrap();
            assert!(argv.contains("opaque model ; not shell\n"));
            assert!(!argv.contains("resume"));
            assert!(argv.contains(if claude {
                "--no-session-persistence"
            } else {
                "--ephemeral"
            }));
            let home = std::fs::read_to_string(f.0.join("scratch/home-path")).unwrap();
            assert_eq!(
                PathBuf::from(home),
                PathBuf::from(std::env::var_os("HOME").unwrap())
            );
            assert_eq!(adapter.usage(&output).unwrap().input, claude.then_some(12));
            assert!(
                adapter
                    .collect(&ProcessOutput {
                        stdout: b"vague prose".to_vec(),
                        ..output
                    })
                    .is_err()
            );
        }
    }

    #[test]
    #[ignore = "requires installed Claude native login and macOS Keychain access; no model call"]
    fn native_claude_keychain_is_accessible_inside_worker_sandbox_without_history() {
        let f = Fixture::new();
        let mut spec = f.spec(false);
        spec.executable = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join("claude"))
            .find(|p| p.is_file())
            .expect("install Claude for this opt-in authentication test");
        spec.args = vec![
            "--safe-mode".into(),
            "auth".into(),
            "status".into(),
            "--json".into(),
        ];
        spec.native_auth = Some(super::super::credentials::NativeAuth::discover("claude").unwrap());
        // Adapters mark provider launches; only frontends may carry native auth.
        spec.class = security::WorkerClass::ProviderFrontend;
        let output = finish(&mut NativeProcess::launch(&spec).unwrap());
        // Report only the exit code and the loggedIn flag, never account data.
        let logged_in = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|v| v.get("loggedIn").cloned());
        assert_eq!(
            output.exit,
            Some(0),
            "native status process failed: exit {:?}, loggedIn {logged_in:?}, failure {:?}, stderr {}",
            output.exit,
            output.failure,
            String::from_utf8_lossy(&output.stderr)
        );
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            status["loggedIn"], true,
            "native Keychain authentication unavailable in worker sandbox"
        );
        // Raw status/account data is discarded, not put in an artifact or event.
    }
}
