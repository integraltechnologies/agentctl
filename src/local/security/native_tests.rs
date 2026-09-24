//! Host-native enforcement tests. Each spawns real processes through the active
//! backend (macOS Seatbelt, Linux Landlock + seccomp) with deterministic local
//! fixtures: no model call, no network beyond a loopback listener. They are
//! ignored by default because a nested host sandbox (CI container, sandboxed
//! terminal) cannot apply a second sandbox; run them with `--ignored`.
use super::*;
use crate::local::config::ProtectedRule;
use crate::local::runtime::process::{
    CancellationOutcome, NativeProcess, ProcessOutput, ProcessSpec, RunningProcess,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const NATIVE: &str =
    "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)";

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "agentctl-native-security-{}-{}-{}",
            std::process::id(),
            crate::local::now_ms().unwrap(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for dir in [
            "repo/.git/hooks",
            "repo/.agentctl",
            "repo/src",
            "repo/private",
            "state/data/scratch",
            "state/config",
            "state/cache",
            "outside/other-repo",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        for (file, text) in [
            ("repo/allowed", "allowed-content"),
            ("repo/.git/HEAD", "ref: refs/heads/main\n"),
            ("repo/.agentctl/project.toml", "version = 1\n"),
            ("repo/private/key", "protected secret"),
            ("outside/secret", "planted secret"),
            ("outside/other-repo/source.rs", "other repository"),
            ("state/data/state.sqlite3", "canonical"),
            ("state/config/config.toml", "machine config"),
            ("state/cache/blob", "cache"),
        ] {
            fs::write(base.join(file), text).unwrap();
        }
        Self(fs::canonicalize(base).unwrap())
    }
    fn path(&self, relative: &str) -> String {
        self.0.join(relative).display().to_string()
    }
    fn spec(&self, script: &str, args: &[String], writable: bool, network: bool) -> ProcessSpec {
        let mut argv = vec!["-c".into(), script.into(), "fixture".into()];
        argv.extend(args.iter().cloned());
        ProcessSpec {
            project_policy_hash: None,
            native_auth: None,
            api_key: None,
            executable: "/bin/sh".into(),
            project_executable: false,
            args: argv,
            input: vec![],
            cwd: self.0.join("repo"),
            workspace: self.0.join("repo"),
            scratch: self.0.join("state/data/scratch"),
            data_root: self.0.join("state/data"),
            config_root: self.0.join("state/config"),
            cache_root: self.0.join("state/cache"),
            writable,
            network,
            timeout_ms: 20_000,
            git_directories: vec![self.0.join("repo/.git")],
            protected: vec![ProtectedRule {
                path: "private".into(),
                deny_read: true,
                deny_write: true,
                reason: "fixture".into(),
            }],
            credential_env: vec![],
            experiment_event_file: None,
            class: WorkerClass::Tool,
            security: SecurityConfig::default(),
            issued: None,
            lock_fd: None,
        }
    }
}

fn finish(process: &mut NativeProcess) -> ProcessOutput {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(output) = process.poll().unwrap() {
            return output;
        }
        assert!(Instant::now() < deadline, "native fixture did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run(spec: &ProcessSpec) -> ProcessOutput {
    finish(&mut NativeProcess::launch(spec).unwrap())
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn gone_within(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_worker_cannot_read_secrets_state_or_other_repositories() {
    let _ = NATIVE;
    let f = Fixture::new();
    let targets: Vec<String> = [
        "outside/secret",
        "outside/other-repo/source.rs",
        "state/data/state.sqlite3",
        "state/config/config.toml",
        "state/cache/blob",
        "repo/private/key",
    ]
    .iter()
    .map(|p| f.path(p))
    .collect();
    let script = "for f in \"$@\"; do if cat \"$f\" >/dev/null 2>&1; then echo \"LEAK:$f\"; fi; done; \
                  ls \"$(dirname \"$1\")\" >/dev/null 2>&1 && echo LISTED_OUTSIDE; cat allowed; cat .git/HEAD >/dev/null && echo GIT_READABLE";
    for writable in [false, true] {
        let output = run(&f.spec(script, &targets, writable, false));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.exit, Some(0), "{output:?}");
        assert!(
            !stdout.contains("LEAK") && !stdout.contains("LISTED_OUTSIDE"),
            "{stdout}"
        );
        assert!(
            stdout.contains("allowed-content") && stdout.contains("GIT_READABLE"),
            "{stdout}"
        );
    }
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_writes_are_confined_and_control_plane_is_protected() {
    let f = Fixture::new();
    let forbidden = [
        "outside/written",
        "state/data/written",
        "state/config/written",
        "state/cache/written",
        "repo/.git/written",
        "repo/.git/hooks/pre-commit",
        "repo/.agentctl/written",
        "repo/private/written",
    ];
    let mut args: Vec<String> = forbidden.iter().map(|p| f.path(p)).collect();
    args.push(f.path("repo/.agentctl/project.toml"));
    args.push(f.path("repo/.git/HEAD"));
    let script = "for f in \"$@\"; do (printf pwned >> \"$f\") 2>/dev/null && echo \"WROTE:$f\"; done; \
                  printf ok > src/edited && printf ok > \"$TMPDIR/scratch-ok\" && echo ALLOWED_OK";
    let output = run(&f.spec(script, &args, true, false));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("WROTE"), "{stdout}");
    assert!(stdout.contains("ALLOWED_OK"), "{output:?}");
    for path in forbidden {
        assert!(!f.0.join(path).exists(), "{path} was created");
    }
    assert_eq!(
        fs::read_to_string(f.0.join("repo/.agentctl/project.toml")).unwrap(),
        "version = 1\n"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("repo/.git/HEAD")).unwrap(),
        "ref: refs/heads/main\n"
    );
    // Read-only role: even the workspace is not writable.
    let output = run(&f.spec("printf x > src/readonly && echo WROTE", &[], false, false));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("WROTE"));
    assert!(!f.0.join("repo/src/readonly").exists());
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_ambient_environment_and_credentials_are_not_inherited() {
    let f = Fixture::new();
    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/env".into();
    spec.args = vec![];
    let output = run(&spec);
    assert_eq!(output.exit, Some(0), "{output:?}");
    let allowed = [
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "CARGO_TARGET_DIR",
        "PYTHONDONTWRITEBYTECODE",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_OPTIONAL_LOCKS",
        "GIT_TERMINAL_PROMPT",
        "LANG",
        tree::MARKER_ENV,
    ];
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let name = line.split('=').next().unwrap();
        assert!(
            allowed.contains(&name),
            "ambient variable {name} reached a tool worker"
        );
    }
    assert!(stdout.contains(&format!("HOME={}", f.path("state/data/scratch/home"))));
    // The test harness itself carries many ambient variables (CARGO_*, HOME,
    // USER, possibly SSH_AUTH_SOCK/AWS_*); none of them leaked.
    assert!(std::env::vars().any(|(k, _)| k.starts_with("CARGO_")));
    assert!(!stdout.contains("CARGO_PKG"));
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_network_denied_worker_cannot_connect_even_to_localhost() {
    let f = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connect = format!(
        "use IO::Socket::INET; IO::Socket::INET->new(PeerAddr => '127.0.0.1:{port}', Timeout => 5) or exit 7; exit 0"
    );
    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/perl".into();
    spec.args = vec!["-e".into(), connect.clone()];
    let denied = run(&spec);
    assert_eq!(
        denied.exit,
        Some(7),
        "network-denied worker connected: {denied:?}"
    );
    spec.network = true;
    let allowed = run(&spec);
    assert_eq!(
        allowed.exit,
        Some(0),
        "control: network-allowed worker must connect: {allowed:?}"
    );
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_resource_limits_terminate_or_refuse_runaway_workers() {
    let f = Fixture::new();
    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/perl".into();
    spec.args = vec![
        "-e".into(),
        "my @h; for (1..500) { open(my $f, '<', '/dev/null') or exit 9; push @h, $f } exit 0"
            .into(),
    ];
    spec.security.resources.max_open_files = Some(64);
    assert_eq!(run(&spec).exit, Some(9), "RLIMIT_NOFILE was not applied");
    spec.security.resources.max_open_files = None;
    spec.security.resources.max_cpu_seconds = Some(1);
    spec.args = vec!["-e".into(), "1 while 1".into()];
    let started = Instant::now();
    let output = run(&spec);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(
        output.failure.as_deref(),
        Some("resource limit exceeded: cpu time (RLIMIT_CPU)"),
        "{output:?}"
    );
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_cancellation_terminates_ordinary_descendants() {
    let f = Fixture::new();
    let spec = f.spec(
        "sleep 60 & echo $! > \"$TMPDIR/descendant\"; sleep 60",
        &[],
        false,
        false,
    );
    let pid_file = f.0.join("state/data/scratch/descendant");
    let mut process = NativeProcess::launch(&spec).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(&pid_file)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        assert!(Instant::now() < deadline);
        assert!(process.poll().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(process.cancel().unwrap(), CancellationOutcome::Applied);
    let output = finish(&mut process);
    assert_eq!(output.failure.as_deref(), Some("cancelled"), "{output:?}");
    assert!(
        gone_within(descendant, Duration::from_secs(5)),
        "ordinary descendant survived"
    );
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_daemonized_descendant_is_found_terminated_and_reported() {
    let f = Fixture::new();
    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/perl".into();
    spec.args = vec![
        "-e".into(),
        "use POSIX; exit 0 if fork; POSIX::setsid(); my $p = fork; if ($p) { open(my $h, '>', \"$ENV{TMPDIR}/daemon\"); print $h $p; close $h; exit 0 } sleep 60".into(),
    ];
    let output = run(&spec);
    let daemon: i32 = fs::read_to_string(f.0.join("state/data/scratch/daemon"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        output
            .failure
            .as_deref()
            .is_some_and(|f| f.contains("escaped the job's process group")),
        "exited direct child must not imply a dead tree: {output:?}"
    );
    assert!(
        gone_within(daemon, Duration::from_secs(5)),
        "setsid daemon survived"
    );
}

#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_marker_clearing_daemon_is_never_reported_as_clean() {
    let f = Fixture::new();
    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/perl".into();
    // Double-fork, setsid, then exec with an EMPTY environment (drops the job
    // marker) while keeping inherited descriptors (the sentinel).
    spec.args = vec![
        "-e".into(),
        "use POSIX; exit 0 if fork; POSIX::setsid(); my $p = fork; if ($p) { open(my $h, '>', \"$ENV{TMPDIR}/daemon\"); print $h $p; close $h; exit 0 } %ENV = (); exec { '/bin/sleep' } 'sleep', '60'".into(),
    ];
    let output = run(&spec);
    let daemon: i32 = fs::read_to_string(f.0.join("state/data/scratch/daemon"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The marker is gone, but the inherited sentinel still identifies it
    // (Linux: /proc fd links; macOS: libproc descriptor scan).
    assert!(
        output
            .failure
            .as_deref()
            .is_some_and(|f| f.contains("escaped the job's process group")),
        "{output:?}"
    );
    assert!(
        gone_within(daemon, Duration::from_secs(5)),
        "marker-clearing daemon survived"
    );
}

/// A worker that asks LaunchServices to start a process would get one created
/// by launchd outside the sandbox, outside the job's process group, without
/// the job marker and without the sentinel descriptor: it could read a planted
/// secret the sandbox refuses, write outside every write root, and outlive a
/// job the tree sweep calls clean. The broker must therefore be shut. Unit
/// evidence is not enough here: only a real launch proves the broker is shut.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_service_broker_cannot_start_a_process_outside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    // A background-only app bundle inside the workspace. If LaunchServices can
    // be reached, launchd runs this unsandboxed as the operator.
    let app = f.0.join("repo/Escape.app/Contents/MacOS");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        f.0.join("repo/Escape.app/Contents/Info.plist"),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
         <key>CFBundleExecutable</key><string>escape</string>\
         <key>CFBundleIdentifier</key><string>local.agentctl.native.escape</string>\
         <key>CFBundlePackageType</key><string>APPL</string>\
         <key>LSBackgroundOnly</key><string>1</string></dict></plist>\n",
    )
    .unwrap();
    let executable = app.join("escape");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\ncat {secret} > {canary} 2>&1\nsleep 45 &\nprintf '%s' \"$!\" > {survivor}\n",
            secret = f.path("outside/secret"),
            canary = f.path("outside/broker-canary"),
            survivor = f.path("outside/broker-survivor"),
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let mut spec = f.spec("", &[], false, false);
    spec.executable = "/usr/bin/open".into();
    spec.args = vec![
        "-g".into(),
        "-j".into(),
        f.path("repo/Escape.app"),
        // `open` must not be allowed to wait on a broker it cannot reach.
        "--wait-apps".into(),
    ];
    spec.timeout_ms = 8_000;
    let output = run(&spec);

    // Give a successful escape every chance to land before asserting absence.
    std::thread::sleep(Duration::from_secs(3));
    let canary = f.0.join("outside/broker-canary");
    let survivor = f.0.join("outside/broker-survivor");
    if let Ok(text) = fs::read_to_string(&survivor)
        && let Ok(pid) = text.trim().parse::<i32>()
    {
        // SAFETY: only reached when the escape succeeded; clean up the proof.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        !canary.exists(),
        "LaunchServices escape: a broker-started process read {} and wrote outside every write root ({output:?})",
        f.path("outside/secret"),
    );
    assert!(
        !survivor.exists(),
        "LaunchServices escape: a broker-started process outlived the job ({output:?})"
    );
}

/// Scratch lives inside the denied canonical state tree. `realpath` (cargo
/// canonicalizes every dep-info path) stats each ancestor, so if the denial
/// also hid the ancestors' metadata a build in scratch would fail with
/// `could not parse/generate dep info ... Operation not permitted` whatever
/// the toolchain. The ancestors must answer `stat` while
/// everything else in the state tree stays unreadable and unlistable.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_scratch_paths_can_be_canonicalized_without_opening_state() {
    let f = Fixture::new();
    let script = "mkdir -p \"$1/target/debug\" && : > \"$1/target/debug/x\" || exit 30; \
        /usr/bin/perl -MCwd=realpath -e 'defined realpath($ARGV[0]) or exit 20' \"$1/target/debug/x\" || exit 20; \
        if ls \"$2\" >/dev/null 2>&1; then exit 21; fi; \
        if cat \"$2/state.sqlite3\" >/dev/null 2>&1; then exit 22; fi; \
        exit 0";
    let spec = f.spec(
        script,
        &[f.path("state/data/scratch"), f.path("state/data")],
        false,
        false,
    );
    let output = run(&spec);
    assert_eq!(output.exit, Some(0), "{output:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn strict_unsupported_resource_limit_refuses_before_launch() {
    let f = Fixture::new();
    let mut spec = f.spec("printf launched > \"$TMPDIR/launched\"", &[], false, false);
    spec.security.resources.max_memory_bytes = Some(1 << 30);
    spec.security.resources.strict = true;
    let error = NativeProcess::launch(&spec)
        .err()
        .expect("launch must be refused");
    assert!(
        error
            .to_string()
            .starts_with("SECURITY_CAPABILITY_UNSUPPORTED"),
        "{error}"
    );
    assert!(error.to_string().contains("MemoryLimit"));
    assert!(!f.0.join("state/data/scratch/launched").exists());
}

/// Issued visibility is a *denial*, not merely a missing grant. A broader read
/// root that happens to contain the checkout — an operator `read_roots` entry,
/// or the platform temp directory when the repository lives under it — would
/// otherwise silently restore workspace-wide access and the mode would confine
/// nothing. This is compile-time policy, so it needs no host enforcement.
#[test]
fn issued_visibility_denies_unissued_workspace_under_a_broader_read_root() {
    let fixture = Fixture::new();
    let workspace = fixture.0.join("repo");
    let issued = workspace.join("src/issued.rs");
    let unissued = workspace.join("src/unissued.rs");
    fs::write(&issued, "pub fn issued() {}\n").unwrap();
    fs::write(&unissued, "pub fn unissued() {}\n").unwrap();

    let mut spec = fixture.spec("true", &[], true, false);
    spec.class = WorkerClass::ProviderFrontend;
    // The overlap under test: a broader root that contains the checkout.
    spec.security.read_roots = vec![fixture.0.clone()];
    spec.issued = Some(IssuedVisibility {
        read_files: vec![issued.clone()],
        write_paths: vec![issued.clone()],
    });
    let policy = compile(&spec).unwrap();

    let canonical = fs::canonicalize(&workspace).unwrap();
    let issued = fs::canonicalize(&issued).unwrap();
    let unissued = fs::canonicalize(&unissued).unwrap();

    // The workspace is never granted wholesale under issued visibility.
    assert!(
        !policy
            .filesystem
            .read_roots
            .iter()
            .any(|r| canonical.starts_with(r) && *r != canonical)
            || policy
                .filesystem
                .denied
                .iter()
                .any(|d| d.read && d.path == canonical),
        "a broader read root contains the workspace, so an explicit denial is required"
    );

    let read_denial = policy
        .filesystem
        .denied
        .iter()
        .find(|d| d.read && d.path == canonical)
        .expect("issued visibility must deny unissued workspace reads");
    assert!(
        read_denial.except.iter().any(|e| issued.starts_with(e)),
        "the issued file must stay readable"
    );
    assert!(
        !read_denial.except.iter().any(|e| unissued.starts_with(e)),
        "an unissued sibling must not be re-allowed: {:?}",
        read_denial.except
    );
    assert!(
        !read_denial.except_files.contains(&unissued),
        "except_files re-allows exact directories for traversal, never file contents"
    );

    let write_denial = policy
        .filesystem
        .denied
        .iter()
        .find(|d| d.write && d.path == canonical)
        .expect("issued visibility must deny writes outside the issued write targets");
    assert!(!write_denial.except.iter().any(|e| unissued.starts_with(e)));
}

/// An editor's normal atomic replacement — write `<file>.tmp.*` beside the
/// target, then rename it into place — works for an authorized file under
/// issued visibility, while every other sibling stays unwritable: it cannot be
/// created, overwritten, removed, or clobbered by renaming a temporary onto it.
#[test]
#[ignore = "native: requires host sandbox enforcement (macOS Seatbelt / Linux Landlock+seccomp)"]
fn native_issued_visibility_permits_atomic_replacement_of_authorized_files_only() {
    let fixture = Fixture::new();
    let workspace = fixture.0.join("repo");
    let target = workspace.join("src/edit.rs");
    let keep = workspace.join("src/keep.rs");
    fs::write(&target, "old\n").unwrap();
    fs::write(&keep, "keep\n").unwrap();
    let script = "cd src \
        && (printf 'new\\n' > edit.rs.tmp.1 && mv edit.rs.tmp.1 edit.rs && echo ATOMIC_OK || echo ATOMIC_DENIED); \
        (printf 'x' > other.rs 2>/dev/null && echo CREATE_WRITTEN || echo CREATE_DENIED); \
        (printf 'x' > keep.rs 2>/dev/null && echo KEEP_WRITTEN || echo KEEP_DENIED); \
        (rm -f keep.rs 2>/dev/null && [ ! -e keep.rs ] && echo KEEP_REMOVED || echo REMOVE_DENIED); \
        (printf 'y' > edit.rs.tmp.2 && mv edit.rs.tmp.2 keep.rs 2>/dev/null && echo KEEP_CLOBBERED || echo CLOBBER_DENIED); \
        rm -f edit.rs.tmp.2; true";
    let mut spec = fixture.spec(script, &[], true, false);
    // Issued visibility confines provider frontends: executors are one.
    spec.class = WorkerClass::ProviderFrontend;
    spec.issued = Some(IssuedVisibility {
        read_files: vec![target.clone()],
        write_paths: vec![target.clone()],
    });
    let output = run(&spec);
    let stdout = format!(
        "{}\nstderr: {}\nexit: {:?} failure: {:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        output.exit,
        output.failure
    );
    for denied in [
        "CREATE_DENIED",
        "KEEP_DENIED",
        "REMOVE_DENIED",
        "CLOBBER_DENIED",
    ] {
        assert!(stdout.contains(denied), "{denied} expected: {stdout}");
    }
    assert_eq!(fs::read_to_string(&keep).unwrap(), "keep\n");
    assert!(!workspace.join("src/other.rs").exists());
    // Seatbelt can match sibling names; Landlock cannot, so there the grant
    // is exact-file only and an atomic replacement is refused (documented).
    if cfg!(target_os = "macos") {
        assert!(stdout.contains("ATOMIC_OK"), "{stdout}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
    }
}
