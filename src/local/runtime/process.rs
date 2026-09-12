use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    process::{Child, Command, Stdio},
    thread::JoinHandle,
    time::Instant,
};

pub const MAX_OUTPUT: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone)]
pub struct ProcessSpec {
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
    /// Inherited by the entire child family so a controller crash cannot release
    /// the workspace lease while an orphan can still edit files.
    pub(super) lock_fd: Option<i32>,
}
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub failure: Option<String>,
}
pub trait RunningProcess {
    fn pid(&self) -> Option<u32>;
    fn poll(&mut self) -> Result<Option<ProcessOutput>>;
    fn cancel(&mut self) -> Result<()>;
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
    secrets: Vec<String>,
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
    pub fn launch(spec: &ProcessSpec) -> Result<Self> {
        paths::ensure_directory(&spec.scratch)?;
        let home = spec.scratch.join("home");
        paths::ensure_directory(&home)?;
        let mut command = sandbox_command(spec)?;
        command
            .current_dir(&spec.cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("TMPDIR", &spec.scratch)
            .env("CODEX_HOME", home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_CACHE_HOME", &home)
            .env("XDG_DATA_HOME", &home)
            .env("CARGO_TARGET_DIR", spec.scratch.join("target"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LANG", "en_US.UTF-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut secrets = vec![];
        if let Some(native) = &spec.native_auth {
            native.environment(&mut command);
        }
        if let Some((source, target)) = &spec.api_key {
            let value = std::env::var(source)
                .map_err(|_| Error::Invalid("configured API-key environment disappeared".into()))?;
            command.env(target, &value);
            secrets.push(value);
        }
        for name in &spec.credential_env {
            if let Ok(value) = std::env::var(name) {
                command.env(name, &value);
                if !value.is_empty() {
                    secrets.push(value);
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let lock_fd = spec.lock_fd;
            // SAFETY: only async-signal-safe fcntl is called before exec. The owned
            // lease descriptor remains live in the parent for the whole run.
            unsafe {
                command.pre_exec(move || {
                    if let Some(fd) = lock_fd {
                        if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
        let mut child = command.spawn()?;
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
            secrets,
        })
    }
    fn stop_family(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: negative PID addresses only our freshly spawned process group.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
    }
}
impl RunningProcess for NativeProcess {
    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
    fn cancel(&mut self) -> Result<()> {
        self.failure = Some("cancelled".into());
        self.stop_family();
        Ok(())
    }
    fn poll(&mut self) -> Result<Option<ProcessOutput>> {
        require(!self.reaped, "process output already collected")?;
        if self.started.elapsed().as_millis() >= self.timeout as u128 && self.failure.is_none() {
            self.failure = Some("timeout".into());
            self.stop_family();
        }
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        // Remove any background descendants before accepting output/source state.
        self.stop_family();
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
            self.stop_family();
            let _ = self.child.wait();
        }
    }
}

fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(path.to_str().ok_or_else(|| {
        Error::Invalid("non-UTF-8 sandbox path".into())
    })?)?)
}
fn resolved(path: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(path) {
        return Ok(path);
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("cannot resolve sandbox path".into()))?;
    Ok(resolved(parent)?.join(
        path.file_name()
            .ok_or_else(|| Error::Invalid("sandbox path needs a name".into()))?,
    ))
}
pub fn sandbox_available() -> bool {
    cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file()
}
fn sandbox_command(spec: &ProcessSpec) -> Result<Command> {
    require(
        sandbox_available(),
        "mandatory runtime process isolation is unavailable (Stage 5 requires macOS sandbox-exec); refusing unsandboxed execution",
    )?;
    let scratch = quote(&spec.scratch)?;
    let mut exceptions =
        format!("(require-not (subpath {scratch}))(require-not (literal \"/dev/null\"))");
    if spec.writable {
        exceptions.push_str(&format!(
            "(require-not (subpath {}))",
            quote(&spec.workspace)?
        ));
    }
    let auth_files: Vec<_> = spec
        .native_auth
        .as_ref()
        .map(|a| a.readable_files())
        .unwrap_or_default()
        .iter()
        .map(|p| resolved(p))
        .collect::<Result<_>>()?;
    for path in &auth_files {
        require(
            !path.starts_with(&spec.workspace)
                && !path.starts_with(&spec.data_root)
                && !path.starts_with(&spec.config_root),
            "provider-native authentication must remain outside workspace/agentctl state",
        )?;
        exceptions.push_str(&format!("(require-not (literal {}))", quote(path)?));
    }
    let mut profile = format!(
        "(version 1)(allow default)(deny file-write* (require-all {exceptions}))(deny process-info* (target others))(deny signal (target others))"
    );
    for root in [&spec.data_root, &spec.config_root] {
        profile.push_str(&format!("(deny file-read* file-write* (require-all (subpath {})(require-not (subpath {scratch}))))", quote(root)?));
    }
    for p in spec.git_directories.iter().cloned().chain(
        [".git", ".agentctl", ".codex", ".claude"]
            .iter()
            .map(|p| spec.workspace.join(p)),
    ) {
        profile.push_str(&format!("(deny file-write* (subpath {}))", quote(&p)?));
    }
    let mut private_roots = vec![];
    if let Some(home) = std::env::var_os("HOME") {
        for p in [".codex", ".claude"] {
            private_roots.push(PathBuf::from(&home).join(p));
        }
    }
    if let Some(native) = &spec.native_auth {
        private_roots.push(native.provider_home.clone());
    }
    for root in private_roots {
        let root = resolved(&root)?;
        let mut except = format!("(require-not (subpath {scratch}))");
        for file in &auth_files {
            except.push_str(&format!("(require-not (literal {}))", quote(file)?));
        }
        profile.push_str(&format!(
            "(deny file-read-data file-write* (require-all (subpath {}){except}))",
            quote(&root)?
        ));
    }
    for rule in &spec.protected {
        profile.push_str(&format!(
            "(deny {} (subpath {}))",
            if rule.deny_read {
                "file-read* file-write*"
            } else {
                "file-write*"
            },
            quote(&spec.workspace.join(&rule.path))?
        ));
    }
    if !spec.network {
        profile.push_str("(deny network*)");
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", &profile])
        .arg(&spec.executable)
        .args(&spec.args);
    Ok(command)
}

pub(super) struct WorkspaceLease {
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
            return Err(Error::Invalid(
                "runtime workspace leases require Unix".into(),
            ));
        }
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
            for name in ["repo", "repo/.git", "data", "config", "scratch"] {
                std::fs::create_dir_all(f.0.join(name)).unwrap();
            }
            std::fs::write(f.0.join("config/secret"), "fixture secret").unwrap();
            f
        }
        fn spec(&self, writable: bool) -> ProcessSpec {
            ProcessSpec {
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
            let input = JobInput {
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
        let output = finish(&mut NativeProcess::launch(&spec).unwrap());
        assert_eq!(output.exit, Some(0), "native status process failed");
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            status["loggedIn"], true,
            "native Keychain authentication unavailable in worker sandbox"
        );
        // Raw status/account data is discarded, not put in an artifact or event.
    }
}
