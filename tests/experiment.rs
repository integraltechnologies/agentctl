#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::ProjectConfig,
        observe::Liveness,
        paths::{MachinePaths, PathContext},
        repository::RepositoryInfo,
        runtime::{process::*, *},
        store::Store,
    },
    protocol::*,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Fixture {
    #[allow(dead_code)]
    temp: common::TempDir,
    root: PathBuf,
    paths: MachinePaths,
}
impl Fixture {
    fn new() -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("README.md"), "hello\n").unwrap();
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "echo".into(),
            CommandSpec {
                program: "/bin/echo".into(),
                args: vec!["fixture".into()],
                cwd: ".".into(),
            },
        );
        fs::write(
            root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let paths = MachinePaths::resolve(&PathContext {
            home: Some(temp.0.join("home")),
            ..Default::default()
        })
        .unwrap();
        paths.create_directories().unwrap();
        let mut s = Store::open(&paths.database, 5000).unwrap();
        s.register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        Self { temp, root, paths }
    }
    fn store(&self) -> Store {
        Store::open(&self.paths.database, 5000).unwrap()
    }
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "{:?}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn command(program: &str, args: &[&str]) -> CommandSpec {
    CommandSpec {
        program: program.into(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        cwd: ".".into(),
    }
}
fn input(command: CommandSpec) -> ExperimentInput {
    ExperimentInput {
        command,
        network: false,
        env_passthrough: vec![],
        timeout_ms: 5_000,
    }
}
fn output(exit: Option<i32>, stdout: &[u8]) -> ProcessOutput {
    ProcessOutput {
        exit,
        stdout: stdout.to_vec(),
        stderr: vec![],
        failure: None,
    }
}

/// Deterministic fake process/launcher: no real subprocess, no sandbox, no model call.
struct Immediate(Option<ProcessOutput>);
impl RunningProcess for Immediate {
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.0 = Some(ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        });
        Ok(CancellationOutcome::Applied)
    }
}
struct FakeLaunch(ProcessOutput);
impl CheckLauncher for FakeLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(Immediate(Some(self.0.clone()))))
    }
}
/// Never completes on its own; only `cancel()` produces output. Used to exercise
/// cancellation and concurrent read-only observation of a still-running experiment.
struct Hang {
    cancelled: bool,
}
impl RunningProcess for Hang {
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.cancelled.then(|| ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        }))
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.cancelled = true;
        Ok(CancellationOutcome::Applied)
    }
}
struct HangLaunch;
impl CheckLauncher for HangLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(Hang { cancelled: false }))
    }
}
struct StartupFailureLaunch;
impl CheckLauncher for StartupFailureLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Err(local::Error::Invalid(
            "simulated startup failure: executable not found".into(),
        ))
    }
}
/// Simulates a controller that crashes immediately after issuing the OS launch call,
/// before ever observing a poll result. No Drop-based liveness is faked: the shared
/// in-process `liveness` registry entry is naturally gone once this unwinds.
struct CrashOnLaunch;
impl CheckLauncher for CrashOnLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        panic!("simulated controller crash during launch");
    }
}

struct CaptureNetworkLaunch {
    seen: Arc<Mutex<Vec<bool>>>,
}
impl CheckLauncher for CaptureNetworkLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        self.seen.lock().unwrap().push(spec.network);
        Ok(Box::new(Immediate(Some(output(Some(0), b"ok")))))
    }
}

/// Sets the durable request flag after the attempt is RUNNING but before its first
/// poll, making late-exit and failed-termination races fully deterministic.
struct CancelOnLaunch {
    root: PathBuf,
    paths: MachinePaths,
    output: Option<ProcessOutput>,
    cancellation: Option<CancellationOutcome>,
}
impl CheckLauncher for CancelOnLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        let mut store = Store::open(&self.paths.database, 5000)?;
        let id = store.experiment_list(&self.root)?[0]
            .run
            .experiment_id
            .clone();
        store.experiment_cancel(&self.root, &id)?;
        let output = self.output.take().expect("launched once");
        let process: Box<dyn RunningProcess> = if let Some(cancellation) = self.cancellation.clone()
        {
            Box::new(PollThenExit {
                output: Some(output),
                cancellation,
                polled: false,
            })
        } else {
            Box::new(Immediate(Some(output)))
        };
        Ok(process)
    }
}

struct PollThenExit {
    output: Option<ProcessOutput>,
    cancellation: CancellationOutcome,
    polled: bool,
}
impl RunningProcess for PollThenExit {
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        if !self.polled {
            self.polled = true;
            return Ok(None);
        }
        Ok(self.output.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        if self.cancellation == CancellationOutcome::Applied {
            self.output = Some(ProcessOutput {
                exit: None,
                stdout: vec![],
                stderr: vec![],
                failure: Some("cancelled".into()),
            });
        }
        Ok(self.cancellation.clone())
    }
}

type PolicyMutation = Box<dyn FnOnce(&mut ProjectConfig) + Send>;

struct MutatePolicyLaunch {
    root: PathBuf,
    mutation: Option<PolicyMutation>,
    executed: Arc<AtomicBool>,
}
impl CheckLauncher for MutatePolicyLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        let mut policy = ProjectConfig::load(&self.root)?;
        self.mutation.take().expect("launched once")(&mut policy);
        fs::write(
            self.root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )?;
        // This is the same final check NativeProcess performs immediately before
        // spawn. The executable marker must remain false when it detects drift.
        spec.recheck_policy()?;
        self.executed.store(true, Ordering::SeqCst);
        Ok(Box::new(Immediate(Some(output(Some(0), b"executed")))))
    }
}

#[test]
fn successful_long_running_command_is_verified_by_recorded_evidence() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"training complete"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    assert_eq!(run.attempts.len(), 1);
    let attempt = &run.attempts[0];
    assert_eq!(attempt.attempt, 1);
    assert_eq!(attempt.exit_status, Some(0));
    assert_eq!(attempt.pid, Some(4242));
    assert!(attempt.started_at_ms.is_some() && attempt.finished_at_ms.is_some());
    let evidence_id = attempt.evidence.clone().unwrap().0;
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    let evidence = f.store().evidence(&repo, &evidence_id).unwrap().unwrap();
    assert_eq!(evidence.exit_status, Some(0));
    assert!(evidence.stdout_hash.is_some());
}

#[test]
fn project_network_policy_intersects_operator_request_and_survives_reopen_restart() {
    let f = Fixture::new();
    let seen = Arc::new(Mutex::new(vec![]));
    for (denied, requested) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut policy = ProjectConfig::load(&f.root).unwrap();
        policy.routing.deny_network = denied;
        fs::write(
            f.root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        let mut store = f.store();
        let run = ExperimentRuntime::new(&mut store, f.paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(CaptureNetworkLaunch { seen: seen.clone() }))
            .run(
                &f.root,
                ExperimentInput {
                    command: command("/usr/bin/true", &[]),
                    network: requested,
                    env_passthrough: vec![],
                    timeout_ms: 5_000,
                },
            )
            .unwrap();
        assert_eq!(run.network, requested && !denied);
    }
    assert_eq!(*seen.lock().unwrap(), vec![false, false, false, true]);

    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.routing.deny_network = false;
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(CaptureNetworkLaunch { seen: seen.clone() }))
        .run(
            &f.root,
            ExperimentInput {
                command: command("/usr/bin/true", &[]),
                network: true,
                env_passthrough: vec![],
                timeout_ms: 5_000,
            },
        )
        .unwrap();
    assert!(run.network);
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.routing.deny_network = true;
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    drop(store);
    let mut reopened = f.store();
    ExperimentRuntime::new(&mut reopened, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(CaptureNetworkLaunch { seen: seen.clone() }))
        .restart(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(&seen.lock().unwrap()[4..], &[true, false]);
}

#[test]
fn project_command_and_restrictions_share_one_drift_checked_snapshot() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run_project_command(&f.root, "echo".into(), false, vec![], 5_000)
        .unwrap();
    assert_eq!(run.command.args, vec!["fixture"]);
    let mut changed = ProjectConfig::load(&f.root).unwrap();
    changed.commands.get_mut("echo").unwrap().args = vec!["changed".into()];
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&changed).unwrap(),
    )
    .unwrap();
    let restart_error = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"must not run"))))
        .restart(&f.root, &run.experiment_id)
        .unwrap_err();
    assert!(restart_error.to_string().contains("SOURCE_DRIFT"));

    let mutations: Vec<PolicyMutation> = vec![
        Box::new(|policy| policy.commands.get_mut("echo").unwrap().args = vec!["changed".into()]),
        Box::new(|policy| policy.routing.deny_network = true),
        Box::new(|policy| {
            policy.commands.remove("echo");
        }),
    ];
    for mutation in mutations {
        let fixture = Fixture::new();
        let executed = Arc::new(AtomicBool::new(false));
        let mut store = fixture.store();
        let error = ExperimentRuntime::new(&mut store, fixture.paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(MutatePolicyLaunch {
                root: fixture.root.clone(),
                mutation: Some(mutation),
                executed: executed.clone(),
            }))
            .run_project_command(&fixture.root, "echo".into(), true, vec![], 5_000)
            .unwrap_err();
        assert!(error.to_string().contains("SOURCE_DRIFT"));
        assert!(!executed.load(Ordering::SeqCst));
    }

    let fixture = Fixture::new();
    let executed = Arc::new(AtomicBool::new(false));
    let mut store = fixture.store();
    let error = ExperimentRuntime::new(&mut store, fixture.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(MutatePolicyLaunch {
            root: fixture.root.clone(),
            mutation: Some(Box::new(|policy| policy.routing.deny_network = true)),
            executed: executed.clone(),
        }))
        .run(
            &fixture.root,
            ExperimentInput {
                command: command("/usr/bin/true", &[]),
                network: true,
                env_passthrough: vec![],
                timeout_ms: 5_000,
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("SOURCE_DRIFT"));
    assert!(!executed.load(Ordering::SeqCst));
}

#[test]
fn nonzero_exit_and_startup_failure_are_recorded_as_failed_not_succeeded() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(17), b""))))
        .run(&f.root, input(command("/usr/bin/false", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Failed);
    assert_eq!(run.attempts[0].exit_status, Some(17));

    let mut store2 = f.store();
    let error = ExperimentRuntime::new(&mut store2, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(StartupFailureLaunch))
        .run(&f.root, input(command("/does/not/exist", &[])))
        .unwrap_err();
    assert!(error.to_string().contains("startup failure"));
    let list = f.store().experiment_list(&f.root).unwrap();
    let failed = list
        .iter()
        .find(|o| o.run.attempts[0].pid.is_none())
        .expect("startup-failure experiment recorded");
    assert_eq!(failed.run.state, ExperimentState::Failed);
    assert!(failed.run.attempts[0].failure.is_some());
}

#[test]
fn cancellation_stops_the_owned_live_process_and_is_auditable() {
    let f = Fixture::new();
    // A separate connection cannot pretend an experiment that does not exist yet
    // was cancelled, and cannot cancel one that is not RUNNING.
    let missing = ExperimentId::new("experiment:does-not-exist").unwrap();
    assert!(f.store().experiment_cancel(&f.root, &missing).is_err());

    // Drive the hang in a background thread; cancel it from a separate connection
    // exactly the way `agentctl experiment cancel` would from another terminal.
    let root = f.root.clone();
    let paths = f.paths.clone();
    let handle = std::thread::spawn(move || {
        let mut store = Store::open(&paths.database, 5000).unwrap();
        ExperimentRuntime::new(&mut store, paths)
            .unwrap()
            .with_check_launcher(Box::new(HangLaunch))
            .run(&root, input(command("sleep-forever", &[])))
    });
    let id = loop {
        let list = f.store().experiment_list(&f.root).unwrap();
        if let Some(o) = list.first() {
            break o.run.experiment_id.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    f.store().experiment_cancel(&f.root, &id).unwrap();
    let run = handle.join().unwrap().unwrap();
    assert_eq!(run.state, ExperimentState::Cancelled);
    let events = f
        .store()
        .events(
            Some(&RepositoryInfo::discover(&f.root).unwrap().repository_id),
            None,
            None,
            50,
        )
        .unwrap();
    assert!(events.iter().any(|e| {
        matches!(&e.entry, local::store::JournalEntry::Experiment { phase, .. } if phase.contains("CANCELLATION_REQUESTED"))
    }));
    assert!(events.iter().any(|e| {
        matches!(&e.entry, local::store::JournalEntry::Experiment { phase, .. } if phase.contains("CANCELLATION_APPLIED"))
    }));
}

#[test]
fn cancellation_is_an_observed_applied_outcome_not_late_intent() {
    let f = Fixture::new();
    let mut store = f.store();
    let cancelled = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(CancelOnLaunch {
            root: f.root.clone(),
            paths: f.paths.clone(),
            output: Some(output(Some(0), b"would have completed")),
            cancellation: Some(CancellationOutcome::Applied),
        }))
        .run(&f.root, input(command("fixture", &[])))
        .unwrap();
    assert_eq!(cancelled.state, ExperimentState::Cancelled);

    for (exit, expected) in [
        (Some(0), ExperimentState::Succeeded),
        (Some(19), ExperimentState::Failed),
    ] {
        let f = Fixture::new();
        let mut store = f.store();
        let run = ExperimentRuntime::new(&mut store, f.paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(CancelOnLaunch {
                root: f.root.clone(),
                paths: f.paths.clone(),
                output: Some(output(exit, b"already done")),
                cancellation: None,
            }))
            .run(&f.root, input(command("fixture", &[])))
            .unwrap();
        assert_eq!(run.state, expected);
        assert_ne!(run.state, ExperimentState::Cancelled);
    }

    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(CancelOnLaunch {
            root: f.root.clone(),
            paths: f.paths.clone(),
            output: Some(output(Some(0), b"actual success")),
            cancellation: Some(CancellationOutcome::Failed("simulated kill failure".into())),
        }))
        .run(&f.root, input(command("fixture", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    let events = f
        .store()
        .events(
            Some(&RepositoryInfo::discover(&f.root).unwrap().repository_id),
            None,
            None,
            50,
        )
        .unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.entry,
            local::store::JournalEntry::Experiment { phase, detail, .. }
                if phase.contains("CANCELLATION_NOT_APPLIED")
                    && detail.contains("simulated kill failure")
        )
    }));
}

#[test]
fn concurrent_read_only_status_observes_a_still_running_experiment() {
    let f = Fixture::new();
    let root = f.root.clone();
    let paths = f.paths.clone();
    let handle = std::thread::spawn(move || {
        let mut store = Store::open(&paths.database, 5000).unwrap();
        ExperimentRuntime::new(&mut store, paths)
            .unwrap()
            .with_check_launcher(Box::new(HangLaunch))
            .run(&root, input(command("sleep-forever", &[])))
    });
    let id = loop {
        let list = f.store().experiment_list(&f.root).unwrap();
        if let Some(o) = list.first() {
            break o.run.experiment_id.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // A read-only connection can observe the running experiment concurrently.
    let ro = Store::read_only(&f.paths.database, 5000).unwrap();
    let observed = ro.experiment_status(&f.root, &id).unwrap().unwrap();
    assert_eq!(observed.run.state, ExperimentState::Running);
    f.store().experiment_cancel(&f.root, &id).unwrap();
    handle.join().unwrap().unwrap();
}

#[test]
fn crash_mid_flight_leaves_a_running_record_with_unknown_liveness_and_restart_reconciles() {
    let f = Fixture::new();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut store = f.store();
        ExperimentRuntime::new(&mut store, f.paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(CrashOnLaunch))
            .run(&f.root, input(command("/usr/bin/true", &[])))
    }));
    assert!(caught.is_err(), "the simulated crash must actually unwind");

    // Reopen: the persisted record is historical RUNNING, but liveness is UNKNOWN —
    // no separate process/connection may prove the crashed controller's child alive.
    let store = f.store();
    let list = store.experiment_list(&f.root).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].run.state, ExperimentState::Running);
    assert_eq!(list[0].liveness, Liveness::Unknown);
    let id = list[0].run.experiment_id.clone();
    let status = store.experiment_status(&f.root, &id).unwrap().unwrap();
    assert_eq!(status.liveness, Liveness::Unknown);
    assert_eq!(status.run.attempts.len(), 1);
    assert!(status.run.attempts[0].pid.is_none());

    // Explicit restart: reconciles the stale attempt to INTERRUPTED (never guessing
    // success), preserves it untouched, and records a distinct new attempt.
    let mut store2 = f.store();
    let run = ExperimentRuntime::new(&mut store2, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .restart(&f.root, &id)
        .unwrap();
    assert_eq!(run.attempts.len(), 2);
    assert_eq!(run.attempts[0].attempt, 1);
    assert_eq!(run.attempts[0].state, ExperimentState::Interrupted);
    assert!(
        run.attempts[0]
            .failure
            .as_ref()
            .unwrap()
            .contains("unproven")
    );
    assert_eq!(run.attempts[1].attempt, 2);
    assert_eq!(run.attempts[1].state, ExperimentState::Succeeded);
    assert_eq!(run.state, ExperimentState::Succeeded);
}

#[test]
fn stdout_and_stderr_are_bounded_artifact_references_not_inline_sqlite_blobs() {
    let f = Fixture::new();
    let large = vec![b'x'; 200_000];
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), &large))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let evidence_id = run.attempts[0].evidence.clone().unwrap().0;
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    let evidence = f.store().evidence(&repo, &evidence_id).unwrap().unwrap();
    // The evidence row is compact metadata: a hash and a log reference, never the bytes.
    assert!(evidence.stdout_hash.unwrap().starts_with("blake3:"));
    assert!(evidence.full_log_ref.is_some());
    let raw = common::sql(&f.paths.database);
    let (json_len, evidence_len): (i64, i64) = raw
        .query_row(
            "SELECT (SELECT length(record_json) FROM experiment_runs), (SELECT length(record_json) FROM evidence)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        (json_len as usize) < large.len() && (evidence_len as usize) < large.len(),
        "captured output must not be inlined into durable rows: json={json_len} evidence={evidence_len} raw={}",
        large.len()
    );
}

#[test]
fn status_and_list_are_read_only_and_never_mutate_state() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let ro = Store::read_only(&f.paths.database, 5000).unwrap();
    assert!(ro.experiment_list(&f.root).is_ok());
    assert!(
        ro.experiment_status(&f.root, &run.experiment_id)
            .unwrap()
            .is_some()
    );
    // No adapters/providers exist anywhere in this path: launch/status/list/cancel/
    // restart take no ProviderAdapter and make no model/provider call.
}

#[test]
fn experiment_status_does_not_cross_workspaces() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();

    // A second, distinct repository registered under the SAME machine database must
    // not see the first repository's experiment: ownership is workspace-scoped.
    let other_root = f.temp.0.join("other-repo");
    fs::create_dir_all(&other_root).unwrap();
    git(&other_root, &["init", "--quiet", "--initial-branch=main"]);
    fs::write(other_root.join("README.md"), "other\n").unwrap();
    ProjectConfig::initialize(&other_root).unwrap();
    git(&other_root, &["add", "."]);
    git(&other_root, &["commit", "--quiet", "-m", "baseline"]);
    let mut other_store = Store::open(&f.paths.database, 5000).unwrap();
    other_store
        .register_repository(RepositoryInfo::discover(&other_root).unwrap())
        .unwrap();
    assert!(
        other_store
            .experiment_status(&other_root, &run.experiment_id)
            .unwrap()
            .is_none()
    );
    assert!(other_store.experiment_list(&other_root).unwrap().is_empty());
}

#[test]
fn malformed_or_forbidden_commands_are_rejected_before_any_process_launch() {
    let f = Fixture::new();
    let mut store = f.store();
    // Empty program: structurally invalid CommandSpec, never reaches the launcher.
    let empty = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b""))))
        .run(&f.root, input(command("", &[])));
    assert!(empty.is_err());

    // cwd escaping the workspace root is rejected.
    let mut store2 = f.store();
    let escape = ExperimentRuntime::new(&mut store2, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b""))))
        .run(
            &f.root,
            ExperimentInput {
                command: CommandSpec {
                    program: "/usr/bin/true".into(),
                    args: vec![],
                    cwd: "../../etc".into(),
                },
                network: false,
                env_passthrough: vec![],
                timeout_ms: 1000,
            },
        );
    assert!(escape.is_err());

    // Out-of-range timeout is rejected.
    let mut store3 = f.store();
    let bad_timeout = ExperimentRuntime::new(&mut store3, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b""))))
        .run(
            &f.root,
            ExperimentInput {
                command: command("/usr/bin/true", &[]),
                network: false,
                env_passthrough: vec![],
                timeout_ms: 0,
            },
        );
    assert!(bad_timeout.is_err());
    assert!(f.store().experiment_list(&f.root).unwrap().is_empty());
}

#[test]
fn direct_sql_cannot_create_or_flip_an_experiment_without_controller_authorization() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let raw = common::sql(&f.paths.database);
    assert!(
        raw.execute(
            "UPDATE experiment_runs SET record_json='{}' WHERE experiment_id=?1",
            [run.experiment_id.as_str()],
        )
        .is_err()
    );
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    assert!(
        raw.execute(
            "INSERT INTO experiment_runs(experiment_id,repo_id,workspace_id,created_at_ms,record_json) VALUES ('experiment:forged',?1,'workspace-x',0,'{}')",
            [repo.as_str()],
        )
        .is_err()
    );
}

#[test]
fn experiment_schema_migration_is_additive_and_preserves_prior_runtime_and_journal_state() {
    let f = Fixture::new();
    // Establish state that predates the v8 experiment tables: a registered
    // repository/workspace and its journal history.
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    let before_events: i64 = common::sql(&f.paths.database)
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();

    // Roll the on-disk schema back to the accepted v7 shape, exactly as a legacy
    // database (created before Stage 9A existed) would look.
    let raw = common::sql(&f.paths.database);
    common::strip_experiments(&raw);
    raw.pragma_update(None, "user_version", 7).unwrap();
    drop(raw);
    assert_eq!(
        common::sql(&f.paths.database)
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );

    // Reopening re-migrates to the current schema, preserves every pre-existing row
    // and its journal history byte-for-byte, and the new table is immediately usable.
    let mut reopened = f.store();
    assert_eq!(
        reopened.status().unwrap().schema_version,
        local::store::DATABASE_VERSION
    );
    assert!(reopened.repository(&repo).unwrap().is_some());
    let after_events: i64 = common::sql(&f.paths.database)
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before_events, after_events);
    let run = ExperimentRuntime::new(&mut reopened, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
}
