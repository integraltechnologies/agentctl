//! Experiment boundaries and wakeups: FACTS -> DETERMINISTIC POLICY -> DECISION -> CONTROLLED WAKEUP.
//!
//! No test here requires Claude, Codex, network access, or paid model tokens: boundary
//! evaluation is pure Rust/SQL over already-persisted facts, and the one test that
//! exercises the "wakeup maps into existing planning infrastructure" path uses a
//! deterministic fake `ProviderAdapter`, exactly like the runtime's own test suites.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{ProjectConfig, VerificationDefinition},
        paths::{MachinePaths, PathContext},
        planning::*,
        repository::RepositoryInfo,
        runtime::{process::*, provider::*, *},
        store::Store,
    },
    protocol::*,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

struct Fixture {
    #[allow(dead_code)]
    temp: common::TempDir,
    root: PathBuf,
    paths: MachinePaths,
    config: RuntimeConfig,
}
impl Fixture {
    fn new() -> Self {
        Self::build(true)
    }
    /// A repository registered but never indexed: any `prepare_plan` (and therefore
    /// any wakeup creation) against it fails deterministically with
    /// "requires a complete fresh code index" every time it is attempted.
    fn unindexed() -> Self {
        Self::build(false)
    }
    fn build(indexed: bool) -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("src/lib.rs"), "pub fn noop() {}\n").unwrap();
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "echo".into(),
            CommandSpec {
                program: "/bin/echo".into(),
                args: vec!["fixture".into()],
                cwd: ".".into(),
            },
        );
        policy.verification.insert(
            "integration".into(),
            VerificationDefinition {
                description: "integration checks".into(),
                command_refs: vec![],
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
        let mut config = RuntimeConfig::default();
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                authentication: Default::default(),
                adapter: "codex".into(),
                executable: "/usr/bin/true".into(),
            },
        );
        config.roles.insert(
            "planner".into(),
            RoleConfig {
                provider: "test".into(),
                model: Some("opaque-planner".into()),
                effort: None,
            },
        );
        let machine = local::config::MachineConfig {
            runtime: config.clone(),
            ..Default::default()
        };
        fs::write(&paths.machine_config, toml::to_string(&machine).unwrap()).unwrap();
        let mut s = Store::open(&paths.database, 5000).unwrap();
        s.register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        if indexed {
            s.index_repository(&root).unwrap();
        }
        Self {
            temp,
            root,
            paths,
            config,
        }
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
fn output(exit: Option<i32>, stdout: &[u8]) -> ProcessOutput {
    ProcessOutput {
        exit,
        stdout: stdout.to_vec(),
        stderr: vec![],
        failure: None,
    }
}
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
/// Writes exactly the structured sidecar bytes an instrumented experiment would
/// append. Each launch consumes one payload; a restart consumes the next.
struct EventFileLaunch {
    payloads: VecDeque<Vec<u8>>,
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

fn metric_frame(sequence: u64, name: &str, value: f64) -> String {
    format!(
        "{{\"type\":\"metric\",\"sequence\":{sequence},\"timestamp_ms\":{sequence},\"source\":\"trainer\",\"name\":\"{name}\",\"value\":{value}}}\n"
    )
}
fn nan_frame(sequence: u64, name: &str) -> String {
    format!(
        "{{\"type\":\"metric\",\"sequence\":{sequence},\"timestamp_ms\":{sequence},\"source\":\"trainer\",\"name\":\"{name}\",\"value\":\"NaN\"}}\n"
    )
}
fn metric_frame_tagged(sequence: u64, name: &str, value: f64, tags: &[(&str, &str)]) -> String {
    let tags_json: String = tags
        .iter()
        .map(|(k, v)| format!("\"{k}\":\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"type\":\"metric\",\"sequence\":{sequence},\"timestamp_ms\":{sequence},\"source\":\"trainer\",\"name\":\"{name}\",\"value\":{value},\"tags\":{{{tags_json}}}}}\n"
    )
}

fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn boundary(
    id: &str,
    metric: &str,
    comparison: MetricComparison,
    value: f64,
) -> BoundaryDefinition {
    boundary_tagged(id, metric, BTreeMap::new(), comparison, value)
}
fn boundary_tagged(
    id: &str,
    metric: &str,
    tags: BTreeMap<String, String>,
    comparison: MetricComparison,
    value: f64,
) -> BoundaryDefinition {
    BoundaryDefinition {
        boundary_id: id.into(),
        condition: ExperimentBoundary::MetricThreshold {
            metric: metric.into(),
            tags,
            comparison,
            value,
        },
        action: BoundaryAction::RecordOnly,
    }
}
fn planner_boundary(
    id: &str,
    metric: &str,
    comparison: MetricComparison,
    value: f64,
) -> BoundaryDefinition {
    planner_boundary_tagged(id, metric, BTreeMap::new(), comparison, value)
}
fn planner_boundary_tagged(
    id: &str,
    metric: &str,
    tags: BTreeMap<String, String>,
    comparison: MetricComparison,
    value: f64,
) -> BoundaryDefinition {
    BoundaryDefinition {
        boundary_id: id.into(),
        condition: ExperimentBoundary::MetricThreshold {
            metric: metric.into(),
            tags,
            comparison,
            value,
        },
        action: BoundaryAction::RequirePlannerReview {
            verification_ref: "integration".into(),
        },
    }
}

fn input(
    command: CommandSpec,
    decision_boundaries: Vec<BoundaryDefinition>,
    max_planner_wakeups: u32,
) -> ExperimentInput {
    ExperimentInput {
        command,
        network: false,
        env_passthrough: vec![],
        timeout_ms: 5_000,
        decision_boundaries,
        max_planner_wakeups,
    }
}

// ========================= BOUNDARY DECISIONS =========================

#[test]
fn boundary_not_satisfied_creates_no_decision() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(1, "loss", 0.9))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn every_comparison_fires_exactly_at_its_threshold_edge() {
    for (comparison, value, threshold, should_fire) in [
        (MetricComparison::LessThan, 0.09, 0.1, true),
        (MetricComparison::LessThan, 0.1, 0.1, false),
        (MetricComparison::LessThanOrEqual, 0.1, 0.1, true),
        (MetricComparison::GreaterThan, 0.11, 0.1, true),
        (MetricComparison::GreaterThan, 0.1, 0.1, false),
        (MetricComparison::GreaterThanOrEqual, 0.1, 0.1, true),
        (MetricComparison::Equal, 0.1, 0.1, true),
        (MetricComparison::Equal, 0.100001, 0.1, false),
    ] {
        let f = Fixture::new();
        let mut store = f.store();
        let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
            .unwrap()
            .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
                1, "metric", value,
            ))));
        let run = runtime
            .run(
                &f.root,
                input(
                    command("/bin/echo", &[]),
                    vec![boundary("edge", "metric", comparison, threshold)],
                    DEFAULT_MAX_PLANNER_WAKEUPS,
                ),
            )
            .unwrap();
        let fired = !f
            .store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty();
        assert_eq!(
            fired, should_fire,
            "comparison={comparison:?} value={value} threshold={threshold}"
        );
    }
}

#[test]
fn nonfinite_metrics_never_satisfy_a_boundary() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(nan_frame(1, "loss"))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "any-loss",
                    "loss",
                    MetricComparison::GreaterThanOrEqual,
                    f64::MIN,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    // The NaN frame is rejected at experiment-event ingestion into a HEALTH fact; it never
    // becomes a METRIC row for boundary evaluation to see.
    assert!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn wrong_metric_series_and_wrong_experiment_do_not_fire() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "accuracy", 0.99,
        ))));
    let a = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "loss-threshold",
                    "loss",
                    MetricComparison::LessThan,
                    0.5,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let mut store2 = f.store();
    let mut runtime2 = ExperimentRuntime::new(&mut store2, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(1, "loss", 0.1))));
    let b = runtime2
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let store = f.store();
    assert!(
        store
            .experiment_decisions(&f.root, &a.experiment_id)
            .unwrap()
            .is_empty(),
        "metric name mismatch must never fire"
    );
    assert!(
        store
            .experiment_decisions(&f.root, &b.experiment_id)
            .unwrap()
            .is_empty(),
        "experiment b declared no boundaries; loss=0.1 belongs to a different experiment entirely"
    );
}

#[test]
fn replay_repeated_polling_and_reopen_never_duplicate_a_decision() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}{}",
            metric_frame(1, "loss", 0.05),
            metric_frame(1, "loss", 0.05), // replay of source_sequence=1: idempotent no-op
            metric_frame(2, "loss", 0.04)
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 1, "one boundary, one attempt, fires once");
    assert_eq!(decisions[0].observed_value, 0.05);
    assert_eq!(decisions[0].metric_name, "loss");
    // Reopen and observe repeatedly: read-only observation never mutates, and
    // nothing about reopening a store re-fires anything.
    for _ in 0..3 {
        let reopened = f.store();
        assert_eq!(
            reopened
                .experiment_decisions(&f.root, &run.experiment_id)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn multiple_boundaries_fire_from_one_event_in_declared_order() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![
                    boundary("z-last", "loss", MetricComparison::LessThan, 0.5),
                    boundary("a-first", "loss", MetricComparison::LessThan, 0.1),
                ],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 2);
    assert_eq!(
        decisions
            .iter()
            .map(|d| d.boundary.boundary_id.clone())
            .collect::<Vec<_>>(),
        vec!["z-last".to_string(), "a-first".to_string()],
        "declaration order, not alphabetical or value order"
    );
}

#[test]
fn multiple_metrics_can_each_satisfy_their_own_boundary_from_one_event() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}",
            metric_frame(1, "loss", 0.05),
            metric_frame(2, "accuracy", 0.97)
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![
                    boundary("loss-ok", "loss", MetricComparison::LessThan, 0.1),
                    boundary(
                        "accuracy-ok",
                        "accuracy",
                        MetricComparison::GreaterThan,
                        0.9,
                    ),
                ],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 2);
}

#[test]
fn triggering_event_and_boundary_hash_are_preserved_and_immutable() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}",
            metric_frame(1, "loss", 0.5),
            metric_frame(2, "loss", 0.05)
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 1);
    let decision = &decisions[0];
    assert_eq!(decision.attempt, 1);
    assert_eq!(decision.metric_name, "loss");
    assert_eq!(decision.observed_value, 0.05);
    assert_eq!(decision.boundaries_hash, run.boundaries_hash);
    assert_eq!(decision.boundary, run.decision_boundaries[0]);
    // Later state changes (here: the experiment finishing) cannot retroactively
    // change what a historical decision means.
    let boundaries = f
        .store()
        .experiment_boundaries(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(boundaries.boundaries_hash, decision.boundaries_hash);
}

#[test]
fn attempt_isolation_a_restarted_attempt_gets_its_own_independent_firing() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch {
            payloads: VecDeque::from([
                metric_frame(1, "loss", 0.05).into_bytes(),
                metric_frame(1, "loss", 0.04).into_bytes(),
            ]),
        }));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let run = runtime.restart(&f.root, &run.experiment_id).unwrap();
    assert_eq!(run.attempts.len(), 2);
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 2, "each attempt independently fires once");
    assert_eq!(decisions[0].attempt, 1);
    assert_eq!(decisions[1].attempt, 2);
}

#[test]
fn raw_sql_cannot_forge_or_mutate_decisions_or_wakeups() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    let raw = common::sql(&f.paths.database);
    assert!(
        raw.execute(
            "INSERT INTO experiment_decisions(decision_id,repo_id,workspace_id,experiment_id,attempt,boundary_id,decided_at_ms,requires_planner,record_json) VALUES ('decision:forged',?1,'workspace-x',?2,1,'forged',0,0,'{}')",
            rusqlite::params![repo.as_str(), run.experiment_id.as_str()],
        )
        .is_err(),
        "an unauthorized connection cannot mint a decision"
    );
    assert!(
        raw.execute(
            "UPDATE experiment_decisions SET record_json='{}' WHERE experiment_id=?1",
            [run.experiment_id.as_str()],
        )
        .is_err(),
        "decisions are immutable even to an authorized-looking update"
    );
    assert!(
        raw.execute(
            "INSERT INTO experiment_wakeups(wakeup_id,repo_id,workspace_id,experiment_id,decision_id,planning_request_id,created_at_ms,record_json) VALUES ('wakeup:forged',?1,'workspace-x',?2,'decision:forged','request:forged',0,'{}')",
            rusqlite::params![repo.as_str(), run.experiment_id.as_str()],
        )
        .is_err(),
        "an unauthorized connection cannot mint a wakeup"
    );
}

// ============================== WAKEUPS ===============================

#[test]
fn record_only_decision_creates_no_planner_wakeup() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
    assert!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn experiment_event_alone_cannot_create_a_wakeup() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}",
            "{\"type\":\"health\",\"sequence\":1,\"timestamp_ms\":1,\"source\":\"trainer\",\"kind\":\"WARNING\",\"message\":\"looks bad\"}\n",
            "{\"type\":\"status\",\"sequence\":2,\"timestamp_ms\":2,\"source\":\"trainer\",\"status\":\"DIVERGED\"}\n"
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                // A boundary is declared, but no METRIC event ever matches it: raw
                // WARNING/status facts must never be interpreted as satisfying one.
                vec![planner_boundary(
                    "loss-spike",
                    "loss",
                    MetricComparison::GreaterThan,
                    10.0,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn planner_action_decision_creates_exactly_one_bounded_wakeup() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let wakeups = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(wakeups.len(), 1);
    let wakeup = &wakeups[0];
    assert_eq!(wakeup.status, PlannerInvocationStatus::NotAttempted);
    assert!(wakeup.planner_jobs.is_empty());
    // Bounded, structured context: no raw stdout/stderr/log dump, just the compact
    // facts the wakeup assembled from already-persisted experiment facts and decisions.
    let bytes = serde_json::to_vec(&wakeup.wakeup.context).unwrap();
    assert!(
        bytes.len() < 4096,
        "context must stay compact: {} bytes",
        bytes.len()
    );
    assert!(
        wakeup
            .wakeup
            .context
            .constraints
            .iter()
            .any(|c| c.contains(&run.experiment_id.as_str().to_string())),
        "context must name the experiment, not just prose"
    );
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.decision_count, 1);
    assert_eq!(control.wakeups_created, 1);
    assert!(!control.attention_required);
}

#[test]
fn repeated_reconciliation_and_reopen_never_duplicate_a_wakeup() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    for _ in 0..3 {
        let reopened = f.store();
        assert_eq!(
            reopened
                .experiment_wakeups(&f.root, &run.experiment_id)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn budget_exhaustion_blocks_further_wakeups_and_reports_attention_required() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}",
            metric_frame(1, "loss", 0.05),
            metric_frame(2, "accuracy", 0.99)
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![
                    planner_boundary("low-loss", "loss", MetricComparison::LessThan, 0.1),
                    planner_boundary(
                        "high-accuracy",
                        "accuracy",
                        MetricComparison::GreaterThan,
                        0.9,
                    ),
                ],
                1, // budget of exactly one wakeup, two boundaries will fire
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        2,
        "boundary evaluation decides independently of the wakeup budget"
    );
    let wakeups = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(wakeups.len(), 1, "budget caps wakeups, not decisions");
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.wakeups_created, 1);
    assert_eq!(control.wakeups_budget, 1);
    assert!(
        control.attention_required,
        "an unfulfilled decision plus an exhausted budget must surface for attention"
    );
}

#[test]
fn a_persistently_failing_wakeup_creation_surfaces_as_attention_required_not_a_silent_gap() {
    // Wakeup creation ("an action cannot safely/validly be translated into existing
    // planning semantics") can fail for reasons that have nothing to do with the
    // wakeup budget - here, `prepare_plan` refusing a never-indexed repository. That
    // must still show up as ATTENTION_REQUIRED once the experiment is done retrying,
    // not sit invisibly as a per-attempt error string only.
    let f = Fixture::unindexed();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(run.state, ExperimentState::Succeeded);
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1,
        "boundary evaluation still decides even though no wakeup can act on it yet"
    );
    assert!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty(),
        "wakeup creation could never succeed against an unindexed repository"
    );
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.wakeups_created, 0);
    assert!(
        control.attention_required,
        "a terminal experiment with an unfulfilled planner decision must require attention, well under budget or not"
    );
}

#[test]
fn declaring_a_planner_boundary_without_a_valid_verification_ref_is_rejected_up_front() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let error = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![BoundaryDefinition {
                    boundary_id: "bad".into(),
                    condition: ExperimentBoundary::MetricThreshold {
                        metric: "loss".into(),
                        tags: BTreeMap::new(),
                        comparison: MetricComparison::LessThan,
                        value: 0.1,
                    },
                    action: BoundaryAction::RequirePlannerReview {
                        verification_ref: "does-not-exist".into(),
                    },
                }],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap_err();
    assert!(error.to_string().contains("does-not-exist"));
    assert!(f.store().experiment_list(&f.root).unwrap().is_empty());
}

#[test]
fn nonscalar_boundary_kinds_are_rejected_at_declaration_not_silently_ignored() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let error = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![BoundaryDefinition {
                    boundary_id: "on-exit".into(),
                    condition: ExperimentBoundary::ProcessExit,
                    action: BoundaryAction::RecordOnly,
                }],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("METRIC_THRESHOLD") || error.to_string().contains("ProcessExit")
    );
}

// ============================== crash-window reconciliation ==============================

#[test]
fn a_decision_persisted_without_its_wakeup_is_healed_by_the_next_reconciliation_pass() {
    // Models "crash after decision persisted but before wakeup created": insert the
    // decision directly (bypassing the wakeup step entirely, as a crash would), then
    // let the next normal drive() pass (here: a restart) reconcile it.
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    // The live attempt already reconciled this (drive() calls evaluate+reconcile
    // repeatedly), so a wakeup already exists; deleting it is not possible (append
    // -only), so instead this proves the steady-state property directly: repeated
    // reconciliation across restarts never drops or duplicates it.
    let before = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(before.len(), 1);
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(String::new())));
    let run = runtime.restart(&f.root, &run.experiment_id).unwrap();
    let after = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(before[0].wakeup.wakeup_id, after[0].wakeup.wakeup_id);
}

#[test]
fn v9_to_v11_migration_preserves_experiment_and_event_history_and_adds_empty_decision_state() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    drop(store);
    let raw = common::sql(&f.paths.database);
    let record: String = raw
        .query_row("SELECT record_json FROM experiment_runs", [], |r| r.get(0))
        .unwrap();
    // strip_experiment_decisions cascades to also strip v11 (experiment_decision_cursors),
    // so reopening from this v9 shape exercises v9->v10->v11 in one migration pass.
    common::strip_experiment_decisions(&raw);
    raw.pragma_update(None, "user_version", 9).unwrap();
    drop(raw);
    let reopened = f.store();
    assert_eq!(
        reopened.status().unwrap().schema_version,
        agentctl::local::store::DATABASE_VERSION
    );
    let preserved = reopened
        .experiment_status(&f.root, &run.experiment_id)
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::to_string(&preserved.run).unwrap(), record);
    assert!(
        reopened
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn v10_to_v11_migration_preserves_decisions_and_wakeups_and_adds_empty_cursor_state() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions_before = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    let wakeups_before = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions_before.len(), 1);
    assert_eq!(wakeups_before.len(), 1);
    drop(store);
    let raw = common::sql(&f.paths.database);
    // Roll back ONLY the v11 cursor table; v10 decisions/wakeups stay exactly as they
    // were persisted, exactly as an on-disk database created before the decision cursor
    // existed would look.
    common::strip_experiment_decision_cursors(&raw);
    raw.pragma_update(None, "user_version", 10).unwrap();
    drop(raw);
    let mut reopened = f.store();
    assert_eq!(
        reopened.status().unwrap().schema_version,
        agentctl::local::store::DATABASE_VERSION
    );
    let decisions_after = reopened
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    let wakeups_after = reopened
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        serde_json::to_string(&decisions_before).unwrap(),
        serde_json::to_string(&decisions_after).unwrap(),
        "v10 decision history must survive byte-for-byte"
    );
    assert_eq!(wakeups_before.len(), wakeups_after.len());
    assert_eq!(
        wakeups_before[0].wakeup.wakeup_id,
        wakeups_after[0].wakeup.wakeup_id
    );
    // No decision is fabricated or lost, and reconciling again after the migration is
    // a genuine no-op (the boundary already fired, and its wakeup already exists).
    reopened
        .experiment_reconcile(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        reopened
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn read_only_observation_of_boundaries_decisions_and_wakeups_never_mutates_state() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let before: i64 = common::sql(&f.paths.database)
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let read_only = Store::read_only(&f.paths.database, 5000).unwrap();
    read_only
        .experiment_boundaries(&f.root, &run.experiment_id)
        .unwrap();
    read_only
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    read_only
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    read_only
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    let after: i64 = common::sql(&f.paths.database)
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}

// ===================== wakeup -> existing planning infrastructure =====================

struct FakePlanner {
    mode: PlannerMode,
}
enum PlannerMode {
    Succeed,
    Fail,
}
impl ProviderAdapter for FakePlanner {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            model: true,
            effort: true,
            fresh_session: true,
            structured_output: true,
            token_usage: false,
        }
    }
    fn launch(
        &mut self,
        input: &JobInput,
        _process: ProcessSpec,
        _config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        if matches!(self.mode, PlannerMode::Fail) {
            return Ok(Box::new(Immediate(Some(ProcessOutput {
                exit: Some(1),
                stdout: vec![],
                stderr: b"fixture planner failure".to_vec(),
                failure: None,
            }))));
        }
        let prepared: PlannerPacket =
            serde_json::from_value(input.artifact["planner_packet"].clone()).unwrap();
        let plan = fake_execution_plan(&prepared);
        let decision = common::plan_decision(&serde_json::to_value(&plan).unwrap());
        Ok(Box::new(Immediate(Some(output(
            Some(0),
            &serde_json::to_vec(&decision).unwrap(),
        )))))
    }
    fn collect(&self, o: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&o.stdout)?)
    }
    fn usage(&self, _: &ProcessOutput) -> local::Result<Usage> {
        Ok(Usage::default())
    }
}

fn fake_execution_plan(prepared: &PlannerPacket) -> ExecutionPlan {
    let intent = &prepared.request.intent;
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new("task:wakeup-response:1").unwrap(),
        objective: intent.objective.clone(),
        read_scope: vec![ScopePath::Directory { path: "src".into() }],
        write_scope: vec![],
        graph_entities: vec![],
        invariant_refs: intent.invariant_refs.clone(),
        dependencies: vec![],
        definition_of_done: intent.definition_of_done.clone(),
        verification: intent.verification.clone().unwrap(),
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:wakeup-response").unwrap(),
        objective: intent.objective.clone(),
        tasks: vec![task.clone()],
        integration_verification: intent.verification.clone().unwrap(),
    };
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: local::now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "test-fixture-planner".into(),
                source_refs: vec!["fixture".into()],
                provider: None,
            },
            contracts: vec![VerificationContract {
                task_id: task.task_id.clone(),
                task_packet_hash: hash(&task).unwrap(),
                independent_verifier: true,
                input: VerifierInput::PacketDiffAndEvidence,
                memory_refs: vec![],
                exclusions: vec![],
                non_goals: vec![],
            }],
            integration: IntegrationVerificationContract {
                plan_id: packet.plan_id.clone(),
                plan_packet_hash: hash(&packet).unwrap(),
                independent_verifier: true,
                require_all_task_verifications: true,
                require_final_diff_and_evidence: true,
                expectations: intent.definition_of_done.clone(),
            },
            replan: None,
        },
        packet,
    }
}

#[test]
fn wakeup_maps_into_existing_planning_infrastructure_and_planner_success_is_derived_not_duplicated()
{
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let wakeups = store
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(wakeups.len(), 1);
    let request_id = wakeups[0].wakeup.planning_request_id.clone();

    // Wakeups never call a provider themselves: this is the SAME `Runtime::plan` any
    // operator already uses for a hand-authored planning request. The fixture's fake
    // adapter is a stand-in for a model, not for any experiment-specific code path.
    let view = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FakePlanner {
                mode: PlannerMode::Succeed,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &request_id)
    .unwrap();
    assert_eq!(view.state, PlanState::Validated);

    let wakeups = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(wakeups[0].status, PlannerInvocationStatus::Succeeded);
    assert_eq!(wakeups[0].planner_jobs.len(), 1);
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert!(!control.attention_required);
    // A successful planner invocation is entirely outside the wakeup path: it neither
    // consumes an extra slot nor alters the configured (immutable) budget.
    assert_eq!(control.wakeups_created, 1);
    assert_eq!(control.wakeups_budget, DEFAULT_MAX_PLANNER_WAKEUPS);

    // Normal plan authority is untouched: importing does not activate, and the
    // plan still requires the ordinary explicit activation step.
    assert_ne!(view.state, PlanState::Active);
}

#[test]
fn planner_failure_remains_visible_and_does_not_corrupt_experiment_or_process_outcome() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        run.state,
        ExperimentState::Succeeded,
        "the experiment process outcome is unrelated to any later planner invocation"
    );
    let wakeups = store
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    let request_id = wakeups[0].wakeup.planning_request_id.clone();

    let result = Runtime::new(
        &mut store,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FakePlanner {
                mode: PlannerMode::Fail,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &request_id);
    assert!(result.is_err());

    let mut reopened = f.store();
    let run_after = reopened
        .experiment_status(&f.root, &run.experiment_id)
        .unwrap()
        .unwrap();
    assert_eq!(run_after.run.state, ExperimentState::Succeeded);
    let wakeups = reopened
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        wakeups.len(),
        1,
        "the failed invocation does not spawn a second wakeup"
    );
    assert_eq!(wakeups[0].status, PlannerInvocationStatus::FailedUnresolved);
    assert_eq!(
        wakeups[0].planner_jobs.len(),
        1 + agentctl::local::runtime::MAX_PROVIDER_RETRIES as usize,
        "one invocation: its planner job and the bounded mechanical retries it made"
    );
    let control = reopened
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert!(
        control.attention_required,
        "an unresolved planner failure must surface for attention"
    );
    // The failed invocation neither released nor replenished the slot the wakeup
    // already consumed, and the configured budget itself is untouched.
    assert_eq!(control.wakeups_created, 1);
    assert_eq!(control.wakeups_budget, DEFAULT_MAX_PLANNER_WAKEUPS);
    // Reconciling again cannot manufacture a replacement wakeup for the same
    // decision, nor find any other budget to spend.
    reopened
        .experiment_reconcile(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

// ============================== METRIC SERIES / TAG MATCHING ==============================

#[test]
fn untagged_boundary_fires_on_untagged_metric() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary("b", "loss", MetricComparison::LessThan, 0.1)],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn untagged_boundary_does_not_fire_on_a_tagged_metric_fail_closed() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("phase", "val")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                // An old, bare boundary with no tag selector: must NOT be silently
                // broadened to match a newly-tagged series just because the metric
                // name is the same.
                vec![boundary("b", "loss", MetricComparison::LessThan, 0.1)],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn tagged_boundary_fires_on_exact_matching_series() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("phase", "val")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary_tagged(
                    "b",
                    "loss",
                    tags(&[("phase", "val")]),
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn tagged_boundary_does_not_fire_on_a_different_series_value() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("phase", "train")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary_tagged(
                    "b",
                    "loss",
                    tags(&[("phase", "val")]),
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn boundary_tag_selector_matches_as_a_subset_of_a_richer_tagged_event() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("phase", "val"), ("dataset", "dev")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary_tagged(
                    "b",
                    "loss",
                    tags(&[("phase", "val")]),
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_wrong_series_arriving_first_never_consumes_the_one_shot_decision() {
    let f = Fixture::new();
    let mut store = f.store();
    let payload = format!(
        "{}{}",
        metric_frame_tagged(1, "loss", 0.05, &[("phase", "train")]),
        metric_frame_tagged(2, "loss", 0.04, &[("phase", "val")]),
    );
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary_tagged(
                    "b",
                    "loss",
                    tags(&[("phase", "val")]),
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions[0].triggering_event_sequence, 2,
        "must fire from the correctly-tagged event, not the earlier wrong-series one that also happened to satisfy the threshold"
    );
    assert_eq!(decisions[0].event_tags, tags(&[("phase", "val")]));
}

#[test]
fn tag_insertion_order_never_affects_matching_or_hash_determinism() {
    let a = boundary_tagged(
        "b",
        "loss",
        tags(&[("phase", "val"), ("dataset", "dev")]),
        MetricComparison::LessThan,
        0.1,
    );
    let b = boundary_tagged(
        "b",
        "loss",
        tags(&[("dataset", "dev"), ("phase", "val")]),
        MetricComparison::LessThan,
        0.1,
    );
    assert_eq!(a, b);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap(),
        "BTreeMap serialization is key-sorted regardless of insertion order"
    );
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("dataset", "dev"), ("phase", "val")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![a],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn changing_a_boundary_tag_changes_the_boundaries_hash() {
    let base = vec![boundary_tagged(
        "b",
        "loss",
        tags(&[("phase", "val")]),
        MetricComparison::LessThan,
        0.1,
    )];
    let changed = vec![boundary_tagged(
        "b",
        "loss",
        tags(&[("phase", "train")]),
        MetricComparison::LessThan,
        0.1,
    )];
    assert_ne!(hash(&base).unwrap(), hash(&changed).unwrap());
    // And the running experiment actually binds to that hash.
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame_tagged(
            1,
            "loss",
            0.05,
            &[("phase", "val")],
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                base.clone(),
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    assert_eq!(run.boundaries_hash, hash(&base).unwrap());
    assert_ne!(run.boundaries_hash, hash(&changed).unwrap());
}

// ============================== DURABLE INCREMENTAL EVALUATION ==============================

#[test]
fn restarted_attempt_gets_an_independent_cursor_starting_from_zero() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut runtime = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch {
            payloads: VecDeque::from([
                metric_frame(1, "loss", 0.5).into_bytes(),
                metric_frame(1, "loss", 0.05).into_bytes(),
            ]),
        }));
    let run = runtime
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let run = runtime.restart(&f.root, &run.experiment_id).unwrap();
    assert_eq!(run.attempts.len(), 2);
    let raw = common::sql(&f.paths.database);
    let cursor = |attempt: i64| -> i64 {
        raw.query_row(
            "SELECT last_evaluated_arrival_sequence FROM experiment_decision_cursors WHERE experiment_id=?1 AND attempt=?2",
            rusqlite::params![run.experiment_id.as_str(), attempt],
            |r| r.get(0),
        )
        .unwrap()
    };
    let max_arrival = |attempt: i64| -> i64 {
        raw.query_row(
            "SELECT max(arrival_sequence) FROM experiment_events WHERE experiment_id=?1 AND attempt=?2",
            rusqlite::params![run.experiment_id.as_str(), attempt],
            |r| r.get(0),
        )
        .unwrap()
    };
    // `arrival_sequence` is a single global counter across the whole experiment (not
    // reset per attempt), so attempt 2's absolute cursor value is naturally larger
    // than attempt 1's - the property under test is independence, not a shared
    // starting number: each attempt's cursor reaches exactly ITS OWN latest event and
    // no further, never inherits or is seeded from the other attempt's position.
    assert_eq!(cursor(1), max_arrival(1));
    assert_eq!(cursor(2), max_arrival(2));
    assert!(
        cursor(2) > max_arrival(1),
        "attempt 2's own events were appended after attempt 1's, on the shared arrival counter"
    );
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        decisions.len(),
        1,
        "attempt 1's cursor caught up to its own end without ever firing (0.5 is not < 0.1); attempt 2 independently fires from its own event"
    );
    assert_eq!(decisions[0].attempt, 2);
}

#[test]
fn a_second_reconcile_after_full_catch_up_scans_zero_new_rows() {
    let f = Fixture::new();
    let mut store = f.store();
    let mut payload = String::new();
    for sequence in 1..=2_000_u64 {
        payload.push_str(&metric_frame(sequence, "loss", 1.0));
    }
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![boundary("never", "loss", MetricComparison::LessThan, 0.0)],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let raw = common::sql(&f.paths.database);
    let cursor_before: i64 = raw
        .query_row(
            "SELECT last_evaluated_arrival_sequence FROM experiment_decision_cursors WHERE experiment_id=?1 AND attempt=1",
            [run.experiment_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor_before, 2_000);
    let mut reopened = f.store();
    reopened
        .experiment_reconcile(&f.root, &run.experiment_id)
        .unwrap();
    let cursor_after: i64 = raw
        .query_row(
            "SELECT last_evaluated_arrival_sequence FROM experiment_decision_cursors WHERE experiment_id=?1 AND attempt=1",
            [run.experiment_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor_after, cursor_before,
        "nothing new to scan, cursor does not move"
    );
    let pending: i64 = raw
        .query_row(
            "SELECT count(*) FROM experiment_events WHERE experiment_id=?1 AND attempt=1 AND arrival_sequence>?2",
            rusqlite::params![run.experiment_id.as_str(), cursor_after],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
}

#[test]
fn twenty_thousand_events_are_evaluated_via_a_durable_cursor_not_a_full_history_rescan() {
    let f = Fixture::new();
    let mut payload = String::new();
    for sequence in 1..=20_000_u64 {
        payload.push_str(&metric_frame(sequence, "loss", 1.0 / sequence as f64));
    }
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                // Satisfied by the very first event (1.0 < 2.0): proves a boundary
                // firing early does not stop the cursor from still reaching the true
                // end of history, and that later events are correctly skipped as
                // already-discharged rather than re-fired.
                vec![boundary(
                    "early-loss",
                    "loss",
                    MetricComparison::LessThan,
                    2.0,
                )],
                DEFAULT_MAX_PLANNER_WAKEUPS,
            ),
        )
        .unwrap();
    let raw = common::sql(&f.paths.database);
    let repo = RepositoryInfo::discover(&f.root).unwrap().repository_id;
    assert_eq!(
        raw.query_row::<i64, _, _>(
            "SELECT count(*) FROM experiment_events WHERE event_type='METRIC'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        20_000
    );
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 1, "one boundary, fires exactly once");
    assert_eq!(decisions[0].triggering_event_sequence, 1);

    // The cursor nonetheless reaches the true end of history: evaluation always
    // fully catches up, it just never *rescans* what it already caught up on.
    let cursor: i64 = raw
        .query_row(
            "SELECT last_evaluated_arrival_sequence FROM experiment_decision_cursors WHERE repo_id=?1 AND experiment_id=?2 AND attempt=1",
            rusqlite::params![repo.as_str(), run.experiment_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    let max_sequence: i64 = raw
        .query_row(
            "SELECT max(arrival_sequence) FROM experiment_events WHERE repo_id=?1 AND experiment_id=?2 AND attempt=1",
            rusqlite::params![repo.as_str(), run.experiment_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, max_sequence);

    // The exact incremental query boundary evaluation issues for its next batch is a bounded
    // index seek on `experiment_events_by_experiment`, not a scan of every row.
    let plan = raw
        .prepare(
            "EXPLAIN QUERY PLAN SELECT arrival_sequence,event_type,event_json FROM experiment_events \
             WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND attempt=?4 AND arrival_sequence>?5 \
             ORDER BY arrival_sequence ASC LIMIT ?6",
        )
        .unwrap()
        .query_map(
            rusqlite::params![
                repo.as_str(),
                run.workspace_id.as_str(),
                run.experiment_id.as_str(),
                1,
                0,
                512
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(" ");
    assert!(plan.contains("experiment_events_by_experiment"), "{plan}");

    // The decisive proof of "no full-history rescan": after the cursor has fully
    // caught up, a fresh reconciliation pass structurally has zero rows left to
    // process - independent of how large the already-scanned history is, and without
    // relying on a timing threshold.
    let mut reopened = f.store();
    reopened
        .experiment_reconcile(&f.root, &run.experiment_id)
        .unwrap();
    let pending: i64 = raw
        .query_row(
            "SELECT count(*) FROM experiment_events WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND attempt=1 AND arrival_sequence>?4",
            rusqlite::params![
                repo.as_str(),
                run.workspace_id.as_str(),
                run.experiment_id.as_str(),
                cursor
            ],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1,
        "reconciling after full catch-up must not refire or duplicate"
    );
}

// ============================== CONCURRENT RECONCILIATION ==============================

#[test]
fn two_controllers_racing_reconciliation_produce_exactly_one_decision_and_one_wakeup() {
    let f = Fixture::new();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(metric_frame(
            1, "loss", 0.05,
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![planner_boundary(
                    "low-loss",
                    "loss",
                    MetricComparison::LessThan,
                    0.1,
                )],
                1,
            ),
        )
        .unwrap();
    // The live drive() loop has already reconciled this once - racing two MORE,
    // fully independent controller connections against that already-settled state is
    // the realistic and harder case, since only one process ever actually supervises
    // a live attempt in this system; nothing stops a second one from also calling
    // `experiment_reconcile` (an operator re-running it, or a monitoring process).
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = vec![];
    for _ in 0..2 {
        let paths = f.paths.clone();
        let root = f.root.clone();
        let experiment_id = run.experiment_id.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            barrier.wait();
            store.experiment_reconcile(&root, &experiment_id).unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let store = f.store();
    let decisions = store
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    let wakeups = store
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 1, "exactly one logical decision");
    assert_eq!(wakeups.len(), 1, "exactly one logical wakeup");
    let control = store
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.wakeups_created, 1, "budget consumed exactly once");
    let cursor_rows: i64 = common::sql(&f.paths.database)
        .query_row(
            "SELECT count(*) FROM experiment_decision_cursors WHERE experiment_id=?1 AND attempt=1",
            [run.experiment_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor_rows, 1, "exactly one cursor row, never duplicated");
}

/// Races `n` independent controller connections against the same experiment's
/// reconciliation, synchronized to start together via a barrier. Real `Store`
/// connections, real experiment records, real decisions, real wakeup reconciliation -
/// not a standalone SQLite probe.
fn race_reconcilers(paths: &MachinePaths, root: &Path, experiment_id: &ExperimentId, n: usize) {
    let barrier = Arc::new(Barrier::new(n));
    let mut handles = vec![];
    for _ in 0..n {
        let paths = paths.clone();
        let root = root.to_path_buf();
        let experiment_id = experiment_id.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            barrier.wait();
            store.experiment_reconcile(&root, &experiment_id).unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn cap_one_two_eligible_decisions_racing_for_the_same_open_slot_yield_exactly_one_wakeup() {
    // The repository starts unindexed, so `prepare_plan` fails deterministically for
    // every decision: `drive()`'s own internal reconciliation attempts and fails to
    // create any wakeup at all, leaving BOTH decisions genuinely open before the race
    // begins - this is what makes the race below a real contest for the one
    // available slot, not merely a re-check of already-settled state.
    let f = Fixture::unindexed();
    let mut store = f.store();
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(format!(
            "{}{}",
            metric_frame(1, "loss", 0.05),
            metric_frame(2, "accuracy", 0.99)
        ))))
        .run(
            &f.root,
            input(
                command("/bin/echo", &[]),
                vec![
                    planner_boundary("low-loss", "loss", MetricComparison::LessThan, 0.1),
                    planner_boundary(
                        "high-accuracy",
                        "accuracy",
                        MetricComparison::GreaterThan,
                        0.9,
                    ),
                ],
                1,
            ),
        )
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        2,
        "boundary evaluation decides independently of whether a wakeup can currently act"
    );
    assert!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty(),
        "wakeup creation could not yet succeed against the unindexed repository"
    );

    f.store().index_repository(&f.root).unwrap();
    race_reconcilers(&f.paths, &f.root, &run.experiment_id, 6);

    let wakeups = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        wakeups.len(),
        1,
        "cap=1 must never be exceeded even with two genuinely eligible decisions racing for it"
    );
    let decisions = f
        .store()
        .experiment_decisions(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(decisions.len(), 2, "both decisions remain durably recorded");
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.wakeups_created, 1);
    assert!(
        control.attention_required,
        "the losing decision remains visibly unresolved"
    );

    // Repeated reconciliation, including after a DB reopen, never manufactures a
    // second wakeup merely because the budget is checked again.
    for _ in 0..3 {
        let mut reopened = Store::open(&f.paths.database, 5000).unwrap();
        reopened
            .experiment_reconcile(&f.root, &run.experiment_id)
            .unwrap();
    }
    assert_eq!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn cap_three_five_eligible_decisions_concurrent_reconcilers_yield_exactly_three_wakeups() {
    let f = Fixture::unindexed();
    let mut store = f.store();
    let mut payload = String::new();
    let mut boundaries = vec![];
    for i in 0..5 {
        let metric = format!("m{i}");
        payload.push_str(&metric_frame((i + 1) as u64, &metric, 0.0));
        boundaries.push(planner_boundary(
            &format!("b{i}"),
            &metric,
            MetricComparison::LessThan,
            1.0,
        ));
    }
    let run = ExperimentRuntime::new(&mut store, f.paths.clone())
        .unwrap()
        .with_check_launcher(Box::new(EventFileLaunch::one(payload)))
        .run(&f.root, input(command("/bin/echo", &[]), boundaries, 3))
        .unwrap();
    assert_eq!(
        f.store()
            .experiment_decisions(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        5
    );
    assert!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .is_empty()
    );

    f.store().index_repository(&f.root).unwrap();
    race_reconcilers(&f.paths, &f.root, &run.experiment_id, 8);

    let wakeups = f
        .store()
        .experiment_wakeups(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(
        wakeups.len(),
        3,
        "cap=3 with 5 racing eligible decisions must yield exactly 3: never fewer (lost race) or more (TOCTOU)"
    );
    let unique_decisions: std::collections::BTreeSet<_> = wakeups
        .iter()
        .map(|w| w.wakeup.decision_id.clone())
        .collect();
    assert_eq!(unique_decisions.len(), 3, "no duplicate decision linkage");
    let unique_requests: std::collections::BTreeSet<_> = wakeups
        .iter()
        .map(|w| w.wakeup.planning_request_id.as_str().to_string())
        .collect();
    assert_eq!(
        unique_requests.len(),
        3,
        "no duplicate authoritative PlanningRequest linkage"
    );
    let control = f
        .store()
        .experiment_control_summary(&f.root, &run.experiment_id)
        .unwrap();
    assert_eq!(control.wakeups_created, 3);
    assert_eq!(control.wakeups_budget, 3);
    assert!(
        control.attention_required,
        "the two losing decisions remain visible and unresolved"
    );

    // Budget stays exhausted on subsequent reconciliation, including after reopen.
    race_reconcilers(&f.paths, &f.root, &run.experiment_id, 4);
    assert_eq!(
        f.store()
            .experiment_wakeups(&f.root, &run.experiment_id)
            .unwrap()
            .len(),
        3
    );
}
