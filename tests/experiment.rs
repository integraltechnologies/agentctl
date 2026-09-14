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
    collections::VecDeque,
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

/// Writes exactly the structured sidecar bytes an instrumented experiment would
/// append. Each launch consumes one payload, which also models restart attempts.
struct EventFileLaunch {
    payloads: VecDeque<Vec<u8>>,
}

struct EventHangLaunch(Vec<u8>);
impl CheckLauncher for EventHangLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_LIVE_STRUCTURED_EVENT_FILE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        fs::write(
            spec.experiment_event_file
                .as_ref()
                .expect("experiment event path"),
            &self.0,
        )?;
        Ok(Box::new(Hang { cancelled: false }))
    }
}

struct FailEventInsertLaunch {
    database: PathBuf,
    payload: Vec<u8>,
}
impl CheckLauncher for FailEventInsertLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_EVENT_STORAGE_FAILURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        fs::write(
            spec.experiment_event_file
                .as_ref()
                .expect("experiment event path"),
            &self.payload,
        )?;
        common::sql(&self.database).execute_batch(
            "CREATE TRIGGER fail_fixture_event_insert BEFORE INSERT ON experiment_events BEGIN SELECT RAISE(ABORT,'fixture event storage failure'); END;",
        )?;
        Ok(Box::new(Immediate(Some(output(Some(0), b"ok")))))
    }
}
impl EventFileLaunch {
    fn one(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            payloads: VecDeque::from([payload.into()]),
        }
    }
}
impl CheckLauncher for EventFileLaunch {
    fn provenance(&self) -> &'static str {
        "TEST_STRUCTURED_EVENT_FILE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        let path = spec
            .experiment_event_file
            .as_ref()
            .expect("experiment event path");
        fs::write(
            path,
            self.payloads.pop_front().expect("one payload per launch"),
        )?;
        Ok(Box::new(Immediate(Some(output(Some(0), b"ok")))))
    }
}

fn event_query(
    store: &Store,
    root: &Path,
    id: &ExperimentId,
    attempt: Option<u32>,
    event_type: Option<&str>,
    metric_name: Option<&str>,
    limit: usize,
) -> Vec<ExperimentRuntimeEvent> {
    store
        .experiment_events(
            root,
            id,
            &ExperimentEventQuery {
                attempt,
                event_type: event_type.map(str::to_owned),
                metric_name: metric_name.map(str::to_owned),
                limit,
            },
        )
        .unwrap()
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

#[test]
fn structured_metrics_are_ordered_filtered_deduplicated_and_attempt_owned() {
    let f = Fixture::new();
    let first = concat!(
        "{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":101,\"source\":\"trainer\",\"name\":\"loss\",\"value\":1.0,\"step\":1}\n",
        "{\"type\":\"metric\",\"sequence\":2,\"timestamp_ms\":102,\"source\":\"trainer\",\"name\":\"val_loss\",\"value\":0.9,\"epoch\":1}\n",
        "{\"type\":\"metric\",\"sequence\":3,\"timestamp_ms\":103,\"source\":\"trainer\",\"name\":\"accuracy\",\"value\":0.7}\n",
        "{\"type\":\"metric\",\"sequence\":4,\"timestamp_ms\":104,\"source\":\"trainer\",\"name\":\"lr\",\"value\":0.0003}\n",
        "{\"type\":\"metric\",\"sequence\":5,\"timestamp_ms\":105,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.8,\"step\":2}\n",
        "{\"type\":\"metric\",\"sequence\":5,\"timestamp_ms\":105,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.8,\"step\":2}\n",
        "{\"type\":\"metric\",\"sequence\":3,\"timestamp_ms\":106,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.1}\n",
        "{\"type\":\"metric\",\"sequence\":6,\"timestamp_ms\":107,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.6,\"step\":3}\n"
    );
    let second = concat!(
        "{\"type\":\"metric\",\"sequence\":2,\"timestamp_ms\":202,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.4,\"step\":2}\n",
        "{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":201,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.5,\"step\":1}\n",
        "{\"type\":\"health\",\"sequence\":3,\"timestamp_ms\":203,\"source\":\"trainer\",\"kind\":\"WARNING\",\"message\":\"data loader retry\"}\n",
        "{\"type\":\"status\",\"sequence\":4,\"timestamp_ms\":204,\"source\":\"trainer\",\"status\":\"VALIDATING\"}\n"
    );
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch {
            payloads: VecDeque::from([first.as_bytes().to_vec(), second.as_bytes().to_vec()]),
        }));
    let run = runtime
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let run = runtime.restart(&f.root, &run.experiment_id).unwrap();
    let store = f.store();
    let attempt_1_loss = event_query(
        &store,
        &f.root,
        &run.experiment_id,
        Some(1),
        Some("METRIC"),
        Some("loss"),
        100,
    );
    let values: Vec<f64> = attempt_1_loss
        .iter()
        .map(|event| match event.event {
            ExperimentEventData::Metric { value, .. } => value,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(values, vec![1.0, 0.8, 0.6]);
    // A conflicting reuse of source sequence 3 is rejected as an ingestion fact;
    // exact sequence 5 replay is idempotent and produces no second metric.
    let attempt_1 = event_query(
        &store,
        &f.root,
        &run.experiment_id,
        Some(1),
        None,
        None,
        100,
    );
    assert_eq!(
        attempt_1
            .iter()
            .filter(|e| matches!(e.event, ExperimentEventData::Metric { .. }))
            .count(),
        6
    );
    assert!(attempt_1.iter().any(|e| matches!(
        e.event,
        ExperimentEventData::Health {
            kind: ExperimentHealthKind::IngestionError,
            ..
        }
    )));
    let attempt_2 = event_query(
        &store,
        &f.root,
        &run.experiment_id,
        Some(2),
        Some("METRIC"),
        Some("loss"),
        100,
    );
    assert_eq!(attempt_2.len(), 2);
    assert_eq!(
        attempt_2
            .iter()
            .map(|e| e.source_sequence)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert!(attempt_2.iter().all(|event| event.attempt == 2));
    let all_attempt_2 = event_query(
        &store,
        &f.root,
        &run.experiment_id,
        Some(2),
        None,
        None,
        100,
    );
    assert!(all_attempt_2.iter().any(|event| matches!(
        event.event,
        ExperimentEventData::Health {
            kind: ExperimentHealthKind::Warning,
            ..
        }
    )));
    assert!(
        all_attempt_2
            .iter()
            .any(|event| matches!(event.event, ExperimentEventData::ProcessStatus { .. }))
    );
}

#[test]
fn malformed_oversized_partial_and_nonfinite_frames_are_factual_health_events() {
    let f = Fixture::new();
    let mut payload = Vec::new();
    payload.extend_from_slice(b"not-json\n");
    payload.extend_from_slice(
        b"{\"type\":\"metric\",\"sequence\":2,\"timestamp_ms\":2,\"source\":\"trainer\",\"name\":\"loss\",\"value\":\"NaN\"}\n",
    );
    payload.extend(std::iter::repeat_n(b'x', MAX_EVENT_FRAME_BYTES + 1));
    payload.push(b'\n');
    payload.extend_from_slice(b"{\"type\":\"metric\"");
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    let events = event_query(
        &f.store(),
        &f.root,
        &run.experiment_id,
        None,
        None,
        None,
        100,
    );
    assert_eq!(events.len(), 4);
    assert!(
        events
            .iter()
            .all(|event| matches!(event.event, ExperimentEventData::Health { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event.event,
        ExperimentEventData::Health {
            kind: ExperimentHealthKind::NonfiniteMetric,
            ..
        }
    )));
    assert_eq!(run.attempts[0].state, ExperimentState::Succeeded);
}

#[test]
fn checkpoint_events_capture_safe_historical_metadata_and_reject_escapes() {
    let f = Fixture::new();
    fs::write(f.root.join("model.bin"), b"weights-v1").unwrap();
    fs::write(f.root.join("protected.bin"), b"private").unwrap();
    let outside = f.temp.0.join("outside.bin");
    fs::write(&outside, b"secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, f.root.join("escape.bin")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("protected.bin", f.root.join("protected-link.bin")).unwrap();
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.protected.push(local::config::ProtectedRule {
        path: "protected.bin".into(),
        deny_read: true,
        deny_write: true,
        reason: "fixture".into(),
    });
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    let payload = concat!(
        "{\"type\":\"checkpoint\",\"sequence\":1,\"timestamp_ms\":10,\"source\":\"trainer\",\"name\":\"best\",\"path\":\"model.bin\",\"step\":10}\n",
        "{\"type\":\"checkpoint\",\"sequence\":2,\"timestamp_ms\":11,\"source\":\"trainer\",\"name\":\"outside\",\"path\":\"../outside.bin\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":3,\"timestamp_ms\":12,\"source\":\"trainer\",\"name\":\"absolute\",\"path\":\"/tmp/outside.bin\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":4,\"timestamp_ms\":13,\"source\":\"trainer\",\"name\":\"symlink\",\"path\":\"escape.bin\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":5,\"timestamp_ms\":14,\"source\":\"trainer\",\"name\":\"protected\",\"path\":\"protected.bin\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":6,\"timestamp_ms\":15,\"source\":\"trainer\",\"name\":\"protected-link\",\"path\":\"protected-link.bin\"}\n"
    );
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload.as_bytes().to_vec())))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let events = event_query(
        &f.store(),
        &f.root,
        &run.experiment_id,
        None,
        None,
        None,
        100,
    );
    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            ExperimentEventData::Checkpoint {
                path,
                byte_size,
                content_hash,
                ..
            } => Some((path, byte_size, content_hash)),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].0, "model.bin");
    assert_eq!(*checkpoints[0].1, 10);
    assert!(checkpoints[0].2.as_ref().unwrap().starts_with("blake3:"));
    assert_eq!(events.len(), 6);
    fs::write(f.root.join("model.bin"), b"weights-v2").unwrap();
    let persisted = event_query(
        &f.store(),
        &f.root,
        &run.experiment_id,
        None,
        Some("CHECKPOINT"),
        None,
        100,
    );
    assert_eq!(
        persisted,
        events
            .into_iter()
            .filter(|e| matches!(e.event, ExperimentEventData::Checkpoint { .. }))
            .collect::<Vec<_>>()
    );
}

/// `.agentctl` is agentctl's own control-plane/state/config directory and must be as
/// hard-blocked for checkpoints as `.git`, independent of user-configured protected paths.
#[test]
fn checkpoint_events_hard_block_agentctl_control_plane_paths() {
    let f = Fixture::new();
    fs::write(f.root.join("model.bin"), b"weights-v1").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(".agentctl/project.toml", f.root.join("agentctl-link.toml"))
        .unwrap();
    let payload = concat!(
        "{\"type\":\"checkpoint\",\"sequence\":1,\"timestamp_ms\":10,\"source\":\"trainer\",\"name\":\"file\",\"path\":\".agentctl/project.toml\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":2,\"timestamp_ms\":11,\"source\":\"trainer\",\"name\":\"dir\",\"path\":\".agentctl\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":3,\"timestamp_ms\":12,\"source\":\"trainer\",\"name\":\"dot-relative\",\"path\":\"./.agentctl/project.toml\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":4,\"timestamp_ms\":13,\"source\":\"trainer\",\"name\":\"traversal\",\"path\":\"foo/../.agentctl/project.toml\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":5,\"timestamp_ms\":14,\"source\":\"trainer\",\"name\":\"symlink\",\"path\":\"agentctl-link.toml\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":6,\"timestamp_ms\":15,\"source\":\"trainer\",\"name\":\"git-config\",\"path\":\".git/config\"}\n",
        "{\"type\":\"checkpoint\",\"sequence\":7,\"timestamp_ms\":16,\"source\":\"trainer\",\"name\":\"normal\",\"path\":\"model.bin\"}\n"
    );
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload.as_bytes().to_vec())))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let events = event_query(
        &f.store(),
        &f.root,
        &run.experiment_id,
        None,
        None,
        None,
        100,
    );
    assert_eq!(events.len(), 7);
    // Exactly one checkpoint (the normal workspace file) is ever accepted; every
    // `.agentctl`/`.git` attempt is rejected before it can become a checkpoint row or hash.
    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            ExperimentEventData::Checkpoint { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints, vec!["model.bin"]);
    let rejections: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            ExperimentEventData::Health {
                kind: ExperimentHealthKind::IngestionError,
                message: Some(message),
            } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(rejections.len(), 6);
    for message in &rejections {
        // Rejection facts stay bounded and describe only the safety rule, never the
        // rejected file's own contents (e.g. project.toml's policy configuration).
        assert!(message.len() <= 512);
        assert!(!message.contains("[commands"));
        assert!(!message.contains("[[protected]]"));
    }
    assert!(
        rejections
            .iter()
            .filter(|m| m.contains("Git or agentctl administrative storage"))
            .count()
            >= 4
    );
}

#[test]
fn event_queries_are_read_only_and_direct_sql_cannot_forge_or_mutate_events() {
    let f = Fixture::new();
    let payload = b"{\"type\":\"health\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"trainer\",\"kind\":\"HEARTBEAT\"}\n";
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload.to_vec())))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let raw = common::sql(&f.paths.database);
    let before: Vec<(i64, String)> = raw
        .prepare(
            "SELECT arrival_sequence,event_json FROM experiment_events ORDER BY arrival_sequence",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let ro = Store::read_only(&f.paths.database, 5000).unwrap();
    assert_eq!(
        event_query(&ro, &f.root, &run.experiment_id, None, None, None, 0).len(),
        1
    );
    assert_eq!(
        ro.experiment_status(&f.root, &run.experiment_id)
            .unwrap()
            .unwrap()
            .events
            .event_count,
        1
    );
    let after: Vec<(i64, String)> = raw
        .prepare(
            "SELECT arrival_sequence,event_json FROM experiment_events ORDER BY arrival_sequence",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(before, after);
    assert!(
        raw.execute("UPDATE experiment_events SET event_json='{}'", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM experiment_events", []).is_err());
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    assert!(raw.execute("INSERT INTO experiment_events(repo_id,workspace_id,experiment_id,attempt,channel,source_sequence,event_type,timestamp_ms,observed_at_ms,frame_hash,event_json) VALUES (?1,?2,?3,1,'EVENT_FILE',99,'HEALTH',1,1,'x','{}')", rusqlite::params![repo.as_str(), run.workspace_id.as_str(), run.experiment_id.as_str()]).is_err());
}

#[test]
fn v8_to_v9_migration_preserves_experiment_history_and_fabricates_no_events() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FakeLaunch(output(Some(0), b"ok"))))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    drop(store);
    let raw = common::sql(&f.paths.database);
    let record: String = raw
        .query_row("SELECT record_json FROM experiment_runs", [], |row| {
            row.get(0)
        })
        .unwrap();
    common::strip_experiment_events(&raw);
    raw.pragma_update(None, "user_version", 8).unwrap();
    drop(raw);
    let reopened = f.store();
    assert_eq!(reopened.status().unwrap().schema_version, 9);
    let preserved = reopened
        .experiment_status(&f.root, &run.experiment_id)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_string(&preserved.run).unwrap(), record);
    assert!(
        event_query(
            &reopened,
            &f.root,
            &run.experiment_id,
            None,
            None,
            None,
            100
        )
        .is_empty()
    );
}

#[test]
fn twenty_thousand_metric_events_use_bounded_indexed_queries() {
    let f = Fixture::new();
    let mut payload = Vec::new();
    for sequence in 1..=20_000_u64 {
        payload.extend_from_slice(
            format!("{{\"type\":\"metric\",\"sequence\":{sequence},\"timestamp_ms\":{sequence},\"source\":\"trainer\",\"name\":\"loss\",\"value\":{}}}\n", 1.0 / sequence as f64).as_bytes(),
        );
    }
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    let raw = common::sql(&f.paths.database);
    assert_eq!(
        raw.query_row::<i64, _, _>(
            "SELECT count(*) FROM experiment_events WHERE event_type='METRIC'",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        20_000
    );
    let plan = raw
        .prepare("EXPLAIN QUERY PLAN SELECT arrival_sequence,event_json FROM experiment_events WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND attempt=1 AND event_type='METRIC' AND metric_name='loss' ORDER BY arrival_sequence DESC LIMIT 10000")
        .unwrap()
        .query_map(
            rusqlite::params![
                RepositoryInfo::discover(&f.root).unwrap().repository_id.as_str(),
                run.workspace_id.as_str(),
                run.experiment_id.as_str()
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(" ");
    assert!(plan.contains("experiment_metrics_by_name"), "{plan}");
    let latest = event_query(
        &f.store(),
        &f.root,
        &run.experiment_id,
        Some(1),
        Some("METRIC"),
        Some("loss"),
        10_000,
    );
    assert_eq!(latest.len(), 10_000);
    assert_eq!(latest.first().unwrap().source_sequence, 10_001);
    assert_eq!(latest.last().unwrap().source_sequence, 20_000);
}

#[test]
fn structured_events_are_queryable_while_the_attempt_is_running() {
    let f = Fixture::new();
    let root = f.root.clone();
    let paths = f.paths.clone();
    let worker = std::thread::spawn(move || {
        let mut store = Store::open(&paths.database, 5000).unwrap();
        ExperimentRuntime::new(&mut store, paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(EventHangLaunch(
                b"{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.5}\n".to_vec(),
            )))
            .run(&root, input(command("/usr/bin/true", &[])))
    });
    let id = loop {
        let store = f.store();
        if let Some(observation) = store.experiment_list(&f.root).unwrap().first() {
            let events = event_query(
                &store,
                &f.root,
                &observation.run.experiment_id,
                None,
                Some("METRIC"),
                None,
                100,
            );
            if !events.is_empty() {
                break observation.run.experiment_id.clone();
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    f.store().experiment_cancel(&f.root, &id).unwrap();
    assert_eq!(
        worker.join().unwrap().unwrap().state,
        ExperimentState::Cancelled
    );
}

#[test]
fn event_storage_failure_is_visible_without_changing_process_outcome() {
    let f = Fixture::new();
    let payload = b"{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.5}\n";
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(FailEventInsertLaunch {
            database: f.paths.database.clone(),
            payload: payload.to_vec(),
        }))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    assert_eq!(run.attempts[0].exit_status, Some(0));
    assert!(
        run.attempts[0]
            .event_ingestion_error
            .as_deref()
            .unwrap()
            .contains("event ingestion failed")
    );
    assert!(
        event_query(
            &f.store(),
            &f.root,
            &run.experiment_id,
            None,
            None,
            None,
            100
        )
        .is_empty()
    );
}

#[test]
fn metric_checkpoint_and_event_cli_queries_are_read_only_json() {
    let f = Fixture::new();
    local::config::MachineConfig::initialize(&f.paths.machine_config).unwrap();
    fs::write(f.root.join("model.bin"), b"checkpoint").unwrap();
    let payload = concat!(
        "{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"trainer\",\"name\":\"loss\",\"value\":0.5}\n",
        "{\"type\":\"checkpoint\",\"sequence\":2,\"timestamp_ms\":2,\"source\":\"trainer\",\"name\":\"best\",\"path\":\"model.bin\"}\n"
    );
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload.as_bytes().to_vec())))
        .run(&f.root, input(command("/usr/bin/true", &[])))
        .unwrap();
    for (subcommand, expected_type) in [
        ("metrics", "METRIC"),
        ("checkpoints", "CHECKPOINT"),
        ("events", "METRIC"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&f.root)
            .args([
                "experiment",
                subcommand,
                run.experiment_id.as_str(),
                "--limit",
                "10",
                "--json",
            ])
            .env("HOME", f.temp.0.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CACHE_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json.as_array().unwrap()[0]["event"]["type"], expected_type);
    }
}

#[test]
#[ignore = "requires the native macOS sandbox"]
fn native_process_can_emit_live_structured_event_file_frames() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let script = "printf '%s\\n' '{\"type\":\"metric\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"native-fixture\",\"name\":\"loss\",\"value\":0.5}' >> \"$AGENTCTL_EVENT_FILE\"";
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .run(&f.root, input(command("/bin/sh", &["-c", script])))
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    assert_eq!(
        event_query(
            &f.store(),
            &f.root,
            &run.experiment_id,
            None,
            Some("METRIC"),
            None,
            100
        )
        .len(),
        1
    );
}
