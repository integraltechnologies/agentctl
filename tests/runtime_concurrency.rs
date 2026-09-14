//! Focused coverage for the alpha-safety hard concurrent-agent limit
//! (`[runtime.concurrency] max_agents`). Kept separate from tests/runtime.rs so
//! this narrowly-scoped feature has its own fixture rather than growing an
//! already-large shared one.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{MachineConfig, ProjectConfig, VerificationDefinition},
        paths::{MachinePaths, PathContext},
        planning::*,
        repository::RepositoryInfo,
        runtime::{process::*, provider::*, routing, *},
        store::Store,
    },
    protocol::*,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    temp: common::TempDir,
    paths: MachinePaths,
    config: RuntimeConfig,
}
impl Fixture {
    fn new(max_agents: usize) -> Self {
        let temp = common::TempDir::new();
        let paths = MachinePaths::resolve(&PathContext {
            home: Some(temp.0.join("home")),
            ..Default::default()
        })
        .unwrap();
        paths.create_directories().unwrap();
        let mut config = RuntimeConfig::default();
        config.concurrency.max_agents = max_agents;
        for name in ["test", "fallback"] {
            config.providers.insert(
                name.into(),
                ProviderConfig {
                    authentication: Default::default(),
                    adapter: "codex".into(),
                    executable: "/usr/bin/true".into(),
                },
            );
        }
        for role in ["planner", "executor", "verifier"] {
            config.roles.insert(
                role.into(),
                RoleConfig {
                    provider: "test".into(),
                    model: Some(format!("opaque-{role}")),
                    effort: None,
                },
            );
        }
        let machine = MachineConfig {
            runtime: config.clone(),
            ..Default::default()
        };
        fs::write(&paths.machine_config, toml::to_string(&machine).unwrap()).unwrap();
        Self {
            temp,
            paths,
            config,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.paths.database, 5000).unwrap()
    }
    /// A bare, indexed repository suitable only for planner-role invocations
    /// (no execution plan is built for it).
    fn repo(&self, name: &str) -> PathBuf {
        let root = self.temp.0.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("src/lib.rs"), "pub fn cached() {}\n").unwrap();
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: ".".into(),
            },
        );
        policy.verification.insert(
            "integration".into(),
            VerificationDefinition {
                description: "integration tests".into(),
                command_refs: vec!["unit".into()],
            },
        );
        fs::write(
            root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let mut s = self.store();
        s.register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        s.index_repository(&root).unwrap();
        root
    }
    fn request(&self, root: &Path) -> PlanningRequestId {
        self.store()
            .prepare_plan(
                root,
                RequestDraft {
                    objective: "Implement cache support".into(),
                    query: None,
                    scope: vec![ScopePath::Directory { path: "src".into() }],
                    constraints: vec![],
                    definition_of_done: vec!["works".into()],
                    verification: Some(reqs("integration")),
                    invariant_refs: vec![],
                    provenance: PlanningProvenance {
                        actor: "human".into(),
                        source_refs: vec!["objective".into()],
                        provider: None,
                    },
                },
                PlanningLimits::default(),
            )
            .unwrap()
            .request
            .request_id
    }
    /// A repository with one active, single-task ExecutionPlan, so a full
    /// executor -> verifier -> integration-verifier run() can be driven.
    fn plan_repo(&self, name: &str) -> (PathBuf, ExecutionPlan) {
        let root = self.temp.0.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(root.join("src/api.rs"), "pub fn cache_api() {}\n").unwrap();
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: ".".into(),
            },
        );
        for kind in ["unit", "integration"] {
            policy.verification.insert(
                kind.into(),
                VerificationDefinition {
                    description: format!("{kind} tests"),
                    command_refs: vec!["unit".into()],
                },
            );
        }
        fs::write(
            root.join(".agentctl/project.toml"),
            toml::to_string(&policy).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let mut store = self.store();
        store
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        store.index_repository(&root).unwrap();
        let prepared = store
            .prepare_plan(
                &root,
                RequestDraft {
                    objective: "Implement cache api support".into(),
                    query: Some("cache".into()),
                    scope: vec![ScopePath::Directory { path: "src".into() }],
                    constraints: vec![],
                    definition_of_done: vec!["cache api works".into()],
                    verification: Some(reqs("integration")),
                    invariant_refs: vec![],
                    provenance: PlanningProvenance {
                        actor: "human".into(),
                        source_refs: vec!["objective".into()],
                        provider: None,
                    },
                },
                PlanningLimits::default(),
            )
            .unwrap();
        let task = TaskPacket {
            version: ProtocolVersion::V1,
            task_id: TaskId::new("task:0").unwrap(),
            objective: "Implement cache api".into(),
            read_scope: vec![ScopePath::Directory { path: "src".into() }],
            write_scope: vec![ScopePath::File {
                path: "src/api.rs".into(),
            }],
            graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
            invariant_refs: prepared.request.intent.invariant_refs.clone(),
            dependencies: vec![],
            definition_of_done: vec!["api works".into()],
            verification: reqs("unit"),
        };
        let packet = PlanPacket {
            version: ProtocolVersion::V1,
            plan_id: PlanId::new(format!("plan:{name}")).unwrap(),
            objective: prepared.request.intent.objective.clone(),
            tasks: vec![task],
            integration_verification: reqs("integration"),
        };
        let plan = ExecutionPlan {
            metadata: PlanMetadata {
                version: ProtocolVersion::V1,
                request_id: prepared.request.request_id.clone(),
                source: prepared.request.source.clone(),
                created_at_ms: local::now_ms().unwrap(),
                provenance: PlanningProvenance {
                    actor: "fixture-planner".into(),
                    source_refs: vec![prepared.request.request_id.as_str().into()],
                    provider: None,
                },
                contracts: packet
                    .tasks
                    .iter()
                    .map(|t| VerificationContract {
                        task_id: t.task_id.clone(),
                        task_packet_hash: hash(t).unwrap(),
                        independent_verifier: true,
                        input: VerifierInput::PacketDiffAndEvidence,
                        memory_refs: vec![],
                        exclusions: vec![],
                        non_goals: vec![],
                    })
                    .collect(),
                integration: IntegrationVerificationContract {
                    plan_id: packet.plan_id.clone(),
                    plan_packet_hash: hash(&packet).unwrap(),
                    independent_verifier: true,
                    require_all_task_verifications: true,
                    require_final_diff_and_evidence: true,
                    expectations: prepared.request.intent.definition_of_done.clone(),
                },
                replan: None,
            },
            packet,
        };
        store.import_execution_plan(&root, &plan).unwrap();
        store
            .activate_execution_plan(&root, &plan.packet.plan_id)
            .unwrap();
        (root, plan)
    }
}
fn reqs(name: &str) -> VerificationRequirements {
    VerificationRequirements {
        requirement_refs: vec![name.into()],
        evidence_required: true,
    }
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
fn wait_for_running(store: &Store, root: &Path, timeout: Duration) -> RuntimeJob {
    wait_for_running_matching(store, root, timeout, |_| true)
}
/// Some roles (e.g. the executor in the DAG fixtures) complete near-instantly,
/// so a bare "any RUNNING row" poll can transiently observe the wrong job.
/// Wait specifically for the job this test means to hold open.
fn wait_for_running_matching(
    store: &Store,
    root: &Path,
    timeout: Duration,
    matches: impl Fn(&RuntimeJob) -> bool,
) -> RuntimeJob {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(job) = store
            .runtime_jobs(root, None)
            .unwrap()
            .into_iter()
            .find(|j| j.state == RuntimeJobState::Running && matches(j))
        {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a matching RUNNING job"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn success(stdout: Vec<u8>) -> ProcessOutput {
    ProcessOutput {
        exit: Some(0),
        stdout,
        stderr: vec![],
        failure: None,
    }
}
/// Completes on its very first poll.
struct Done(Option<ProcessOutput>);
impl RunningProcess for Done {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        Ok(CancellationOutcome::Applied)
    }
}
/// Never completes on its own; only once `release` is flipped, at which point
/// it yields the (role-appropriate, precomputed) `output`.
struct Blocking {
    release: Arc<AtomicBool>,
    output: Vec<u8>,
}
impl RunningProcess for Blocking {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self
            .release
            .load(Ordering::SeqCst)
            .then(|| success(self.output.clone())))
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.release.store(true, Ordering::SeqCst);
        Ok(CancellationOutcome::Applied)
    }
}
/// Launches immediately and returns intentionally-malformed output: fine for
/// every test here, since none of them need the job to actually *succeed* --
/// only to occupy, then release, a concurrency slot.
#[derive(Default)]
struct FastAdapter {
    launches: Arc<AtomicUsize>,
}
impl ProviderAdapter for FastAdapter {
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
        _: &JobInput,
        _: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Done(Some(success(b"{}".to_vec())))))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}
/// Hangs on every launch until `release` is flipped by the test.
struct Hold {
    release: Arc<AtomicBool>,
    launches: Arc<AtomicUsize>,
}
impl ProviderAdapter for Hold {
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
        _: &JobInput,
        _: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Blocking {
            release: self.release.clone(),
            output: b"{}".to_vec(),
        }))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}
/// Drives a real executor -> verifier -> integration-verifier plan, hanging
/// only at the selected point so the shared slot is genuinely held by that
/// specific role.
#[derive(Clone, Copy, PartialEq)]
enum HangAt {
    TaskVerifier,
    IntegrationVerifier,
}
struct Controlled {
    hang_at: HangAt,
    release: Arc<AtomicBool>,
}
impl ProviderAdapter for Controlled {
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
        process: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let should_hang = match self.hang_at {
            HangAt::TaskVerifier => input.role == AgentRole::Verifier && input.task_id.is_some(),
            HangAt::IntegrationVerifier => {
                input.role == AgentRole::Verifier && input.task_id.is_none()
            }
        };
        let value = match input.role {
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = task.write_scope[0].path().to_string();
                let prior = fs::read_to_string(process.workspace.join(&path)).unwrap_or_default();
                fs::write(
                    process.workspace.join(&path),
                    format!("{prior}// accepted change for {}\n", task.task_id.as_str()),
                )
                .unwrap();
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id,
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                })
                .unwrap()
            }
            AgentRole::Verifier => {
                let target: VerificationTarget =
                    serde_json::from_value(input.artifact["target"].clone()).unwrap();
                let requirement_refs = if input.task_id.is_some() {
                    vec!["unit".into()]
                } else {
                    vec!["integration".into()]
                };
                serde_json::to_value(VerificationPacket {
                    version: ProtocolVersion::V1,
                    verification_id: VerificationId::new(format!(
                        "verification:{}",
                        input.job_id.as_str()
                    ))
                    .unwrap(),
                    target,
                    verifier_job_id: input.job_id.clone(),
                    decision: VerificationDecision::Pass,
                    findings: vec![],
                    evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                    requirement_refs,
                    invariant_refs: vec![],
                    notes: None,
                })
                .unwrap()
            }
            AgentRole::Planner => unreachable!("fixture imports plans directly"),
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        if should_hang {
            Ok(Box::new(Blocking {
                release: self.release.clone(),
                output: bytes,
            }))
        } else {
            Ok(Box::new(Done(Some(success(bytes)))))
        }
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}
struct Checks;
impl CheckLauncher for Checks {
    fn provenance(&self) -> &'static str {
        "TEST_FIXTURE"
    }
    fn launch(&mut self, _: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(Done(Some(success(b"ok".to_vec())))))
    }
}

// ============================== requirement 1 ==============================

#[test]
fn default_max_agents_applies_when_concurrency_section_is_omitted() {
    // A machine config written before this feature existed has no
    // [runtime.concurrency] section at all; it must still get a finite,
    // safety-conscious default rather than silently becoming unlimited.
    let toml = r#"
        version = 1
        busy_timeout_ms = 5000
        [runtime]
        timeout_ms = 600000
        max_correction_rounds = 2
    "#;
    let machine: MachineConfig = toml::from_str(toml).unwrap();
    assert_eq!(machine.runtime.concurrency.max_agents, 4);
    machine.validate().unwrap();
    assert_eq!(RuntimeConfig::default().concurrency.max_agents, 4);
}

// =================== max_agents range correction: 1..=256 ===================

const RANGE_MESSAGE: &str = "runtime.concurrency.max_agents must be between 1 and 256 inclusive";

#[test]
fn max_agents_zero_is_rejected_with_the_expected_validation_error() {
    let config = ConcurrencyConfig { max_agents: 0 };
    let error = config.validate().unwrap_err();
    assert_eq!(error.to_string(), RANGE_MESSAGE, "{error}");
}

#[test]
fn max_agents_one_is_accepted() {
    ConcurrencyConfig { max_agents: 1 }.validate().unwrap();
}

#[test]
fn max_agents_256_is_accepted() {
    ConcurrencyConfig { max_agents: 256 }.validate().unwrap();
}

#[test]
fn max_agents_257_is_rejected_with_the_expected_validation_error() {
    let config = ConcurrencyConfig { max_agents: 257 };
    let error = config.validate().unwrap_err();
    assert_eq!(error.to_string(), RANGE_MESSAGE, "{error}");
    // Nothing larger is silently clamped back into range either.
    let error = ConcurrencyConfig {
        max_agents: 1_000_000,
    }
    .validate()
    .unwrap_err();
    assert_eq!(error.to_string(), RANGE_MESSAGE, "{error}");
}

#[test]
fn omitted_concurrency_config_still_uses_the_existing_default() {
    let toml = r#"
        version = 1
        busy_timeout_ms = 5000
        [runtime]
        timeout_ms = 600000
        max_correction_rounds = 2
    "#;
    let machine: MachineConfig = toml::from_str(toml).unwrap();
    assert_eq!(
        machine.runtime.concurrency.max_agents, 4,
        "the default must be preserved, not shifted, by the valid range"
    );
    machine.validate().unwrap();
}

#[test]
fn project_lowering_still_obeys_machine_ceiling_at_new_bounds() {
    let machine = RuntimeConfig {
        concurrency: ConcurrencyConfig { max_agents: 256 },
        ..RuntimeConfig::default()
    };
    let project = routing::ProjectRoles {
        max_agents: Some(100),
        ..Default::default()
    };
    assert_eq!(
        machine.effective_max_agents(&project),
        100,
        "machine=256, project=100 must lower to 100"
    );

    let machine = RuntimeConfig {
        concurrency: ConcurrencyConfig { max_agents: 100 },
        ..RuntimeConfig::default()
    };
    let project = routing::ProjectRoles {
        max_agents: Some(256),
        ..Default::default()
    };
    assert_eq!(
        machine.effective_max_agents(&project),
        100,
        "machine=100, project=256 must stay clamped at the machine ceiling, 100"
    );
}

// ============================== requirement 2 ==============================

#[test]
fn max_agents_one_blocks_a_second_simultaneous_agent_across_workspaces() {
    let f = Fixture::new(1);
    let root_a = f.repo("a");
    let root_b = f.repo("b");
    let request_a = f.request(&root_a);
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let launches = Arc::new(AtomicUsize::new(0));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let launches = launches.clone();
        let root_a = root_a.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold { release, launches }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root_a, &request_a)
        })
    };
    wait_for_running(&f.store(), &root_a, Duration::from_secs(5));

    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&root_b, &request_b);
    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "{result:?}"
    );

    release.store(true, Ordering::SeqCst);
    handle.join().unwrap().unwrap_err(); // malformed planner output; launch itself succeeded
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

// ============================== requirement 3 ==============================

#[test]
fn completed_agent_frees_capacity_for_the_next_launch() {
    let f = Fixture::new(1);
    let root = f.repo("solo");
    for _ in 0..3 {
        let request = f.request(&root);
        let mut store = f.store();
        let result = Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .plan(&root, &request);
        assert!(
            !matches!(result, Err(local::Error::CapacityExceeded { .. })),
            "each attempt runs after the previous one finished and must never be refused: {result:?}"
        );
    }
    let jobs = f.store().runtime_jobs(&root, None).unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(
        jobs.iter()
            .all(|j| j.state != RuntimeJobState::Running && j.state != RuntimeJobState::Queued),
        "no job should be left active: {jobs:?}"
    );
}

// ============================== requirement 4 ==============================

#[test]
fn verifier_obeys_the_same_concurrency_limit() {
    let f = Fixture::new(1);
    let (root_a, plan_a) = f.plan_repo("a");
    let root_b = f.repo("b");
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root_a = root_a.clone();
        let plan_id = plan_a.packet.plan_id.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Controlled {
                        hang_at: HangAt::TaskVerifier,
                        release,
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .with_check_launcher(Box::new(Checks))
            .run(&root_a, &plan_id)
        })
    };
    let job = wait_for_running_matching(&f.store(), &root_a, Duration::from_secs(20), |j| {
        j.role == AgentRole::Verifier && j.task_id.is_some()
    });
    assert_eq!(job.role, AgentRole::Verifier);
    assert!(
        job.task_id.is_some(),
        "the held job must be the task verifier"
    );

    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&root_b, &request_b);
    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "an active verifier job must occupy the same shared slot as any other role: {result:?}"
    );

    release.store(true, Ordering::SeqCst);
    assert_eq!(handle.join().unwrap().unwrap().state, RunState::Complete);
}

// ============================== requirement 5 ==============================

#[test]
fn integration_verifier_obeys_the_same_concurrency_limit() {
    let f = Fixture::new(1);
    let (root_a, plan_a) = f.plan_repo("a");
    let root_b = f.repo("b");
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root_a = root_a.clone();
        let plan_id = plan_a.packet.plan_id.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Controlled {
                        hang_at: HangAt::IntegrationVerifier,
                        release,
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .with_check_launcher(Box::new(Checks))
            .run(&root_a, &plan_id)
        })
    };
    let job = wait_for_running_matching(&f.store(), &root_a, Duration::from_secs(20), |j| {
        j.role == AgentRole::Verifier && j.task_id.is_none()
    });
    assert_eq!(job.role, AgentRole::Verifier);
    assert!(
        job.task_id.is_none(),
        "the held job must be the integration verifier, not a per-task one"
    );

    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&root_b, &request_b);
    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "an active integration verifier job must occupy the same shared slot: {result:?}"
    );

    release.store(true, Ordering::SeqCst);
    assert_eq!(handle.join().unwrap().unwrap().state, RunState::Complete);
}

// ============================== requirement 6 ==============================

#[test]
fn concurrent_spawn_attempts_cannot_exceed_the_cap() {
    let max_agents = 3;
    let n = 8;
    let f = Fixture::new(max_agents);
    let repos: Vec<_> = (0..n).map(|i| f.repo(&format!("r{i}"))).collect();
    let requests: Vec<_> = repos.iter().map(|r| f.request(r)).collect();
    let barrier = Arc::new(Barrier::new(n));
    let refused = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let mut handles = vec![];
    for (root, request) in repos.iter().cloned().zip(requests) {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let barrier = barrier.clone();
        let refused = refused.clone();
        let release = release.clone();
        handles.push(thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            barrier.wait();
            let result = Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold {
                        release,
                        launches: Arc::new(AtomicUsize::new(0)),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root, &request);
            if matches!(result, Err(local::Error::CapacityExceeded { .. })) {
                refused.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }

    // Settle: every attempt has either been refused already (fast path) or is
    // genuinely admitted and now durably RUNNING (holding the slot open).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let running = repos
            .iter()
            .filter(|r| {
                f.store()
                    .runtime_jobs(r, None)
                    .unwrap()
                    .iter()
                    .any(|j| j.state == RuntimeJobState::Running)
            })
            .count();
        if refused.load(Ordering::SeqCst) + running == n {
            assert_eq!(
                running, max_agents,
                "cap={max_agents} with {n} racing attempts must admit exactly {max_agents}, never more"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the race to settle"
        );
        thread::sleep(Duration::from_millis(10));
    }
    release.store(true, Ordering::SeqCst);
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(refused.load(Ordering::SeqCst), n - max_agents);
}

// ============================== requirement 7 ==============================

#[test]
fn capacity_exhaustion_is_not_treated_as_provider_fallback() {
    let mut f = Fixture::new(1);
    f.config.profiles.insert(
        "planner".into(),
        routing::RolePatch {
            fallbacks: Some(vec![RoleConfig {
                provider: "fallback".into(),
                model: None,
                effort: None,
            }]),
            ..Default::default()
        },
    );
    let root_a = f.repo("a");
    let root_b = f.repo("b");
    let request_a = f.request(&root_a);
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root_a = root_a.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold {
                        release,
                        launches: Arc::new(AtomicUsize::new(0)),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root_a, &request_a)
        })
    };
    wait_for_running(&f.store(), &root_a, Duration::from_secs(5));

    let primary = Arc::new(AtomicUsize::new(0));
    let fallback = Arc::new(AtomicUsize::new(0));
    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([
            (
                "test".into(),
                Box::new(FastAdapter {
                    launches: primary.clone(),
                }) as Box<dyn ProviderAdapter>,
            ),
            (
                "fallback".into(),
                Box::new(FastAdapter {
                    launches: fallback.clone(),
                }) as Box<dyn ProviderAdapter>,
            ),
        ]),
    )
    .unwrap()
    .plan(&root_b, &request_b);

    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "capacity refusal must surface as a distinct result, not a routing failure: {result:?}"
    );
    assert_eq!(
        primary.load(Ordering::SeqCst),
        0,
        "capacity is refused before any provider is ever launched"
    );
    assert_eq!(
        fallback.load(Ordering::SeqCst),
        0,
        "capacity exhaustion must never trigger a fallback attempt"
    );

    release.store(true, Ordering::SeqCst);
    handle.join().unwrap().unwrap_err();
}

// ============================== requirement 8 ==============================

#[test]
fn explicit_role_overrides_cannot_raise_the_configured_ceiling() {
    let f = Fixture::new(1);
    let root_a = f.repo("a");
    let root_b = f.repo("b");
    let request_a = f.request(&root_a);
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root_a = root_a.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold {
                        release,
                        launches: Arc::new(AtomicUsize::new(0)),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root_a, &request_a)
        })
    };
    wait_for_running(&f.store(), &root_a, Duration::from_secs(5));

    // The most powerful caller-facing config layer -- an explicit runtime role
    // override -- rewrites every other aspect of the route. It has no field
    // that can touch the concurrency ceiling, and must not move it.
    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([
            (
                "test".into(),
                Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
            ),
            (
                "fallback".into(),
                Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
            ),
        ]),
    )
    .unwrap()
    .with_role_overrides(BTreeMap::from([(
        "planner".into(),
        routing::RolePatch {
            provider: Some("fallback".into()),
            model: Some("override-model".into()),
            effort: Some("high".into()),
            objective: Some("adversarial override".into()),
            fallbacks: Some(vec![RoleConfig {
                provider: "test".into(),
                model: None,
                effort: None,
            }]),
            max_fallback_attempts: Some(4),
            ..Default::default()
        },
    )]))
    .unwrap()
    .plan(&root_b, &request_b);

    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "no combination of provider/model/effort/fallback overrides can raise the ceiling: {result:?}"
    );

    release.store(true, Ordering::SeqCst);
    handle.join().unwrap().unwrap_err();
}

// ============================== requirement 9 ==============================

#[test]
fn project_config_cannot_raise_the_machine_ceiling() {
    let machine = RuntimeConfig {
        concurrency: ConcurrencyConfig { max_agents: 2 },
        ..RuntimeConfig::default()
    };
    let mut project = routing::ProjectRoles::default();
    assert_eq!(machine.effective_max_agents(&project), 2);
    project.max_agents = Some(1);
    assert_eq!(
        machine.effective_max_agents(&project),
        1,
        "a project may lower the ceiling"
    );
    project.max_agents = Some(10);
    assert_eq!(
        machine.effective_max_agents(&project),
        2,
        "a project must never raise the ceiling above the machine value"
    );
}

#[test]
fn project_override_cannot_raise_the_ceiling_end_to_end() {
    let f = Fixture::new(1);
    let root_a = f.repo("a");
    let root_b = f.repo("b");
    // root_b's own project policy tries to widen the shared ceiling to 10.
    let mut policy = ProjectConfig::load(&root_b).unwrap();
    policy.routing.max_agents = Some(10);
    policy.validate().unwrap();
    fs::write(
        root_b.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    git(&root_b, &["add", "."]);
    git(&root_b, &["commit", "--quiet", "-m", "widen ceiling"]);
    f.store().index_repository(&root_b).unwrap();

    let request_a = f.request(&root_a);
    let request_b = f.request(&root_b);

    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root_a = root_a.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold {
                        release,
                        launches: Arc::new(AtomicUsize::new(0)),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root_a, &request_a)
        })
    };
    wait_for_running(&f.store(), &root_a, Duration::from_secs(5));

    let mut store_b = f.store();
    let result = Runtime::new(
        &mut store_b,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&root_b, &request_b);
    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "root_b's own routing.max_agents=10 must not widen the shared machine ceiling: {result:?}"
    );

    release.store(true, Ordering::SeqCst);
    handle.join().unwrap().unwrap_err();
}

// ============================== requirement 10 ==============================

#[test]
fn historical_jobs_do_not_count_toward_active_capacity() {
    let f = Fixture::new(1);
    let root = f.repo("solo");
    for _ in 0..5 {
        let request = f.request(&root);
        let mut store = f.store();
        Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .plan(&root, &request)
        .unwrap_err(); // malformed planner output; job still completes as FAILED
    }
    let jobs = f.store().runtime_jobs(&root, None).unwrap();
    assert_eq!(jobs.len(), 5);
    assert!(jobs.iter().all(|j| j.state == RuntimeJobState::Failed));

    // If those 5 historical rows counted toward capacity, cap=1 would already
    // be "full" and this 6th, genuinely new admission would be refused.
    let request = f.request(&root);
    let release = Arc::new(AtomicBool::new(false));
    let handle = {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let release = release.clone();
        let root = root.clone();
        thread::spawn(move || {
            let mut store = Store::open(&paths.database, 5000).unwrap();
            Runtime::new(
                &mut store,
                paths,
                config,
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Hold {
                        release,
                        launches: Arc::new(AtomicUsize::new(0)),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .plan(&root, &request)
        })
    };
    wait_for_running(&f.store(), &root, Duration::from_secs(5));

    // Now exactly one slot is genuinely active; a concurrent 7th attempt
    // (a different, unrelated repo) must be refused -- proving the 6th really
    // did consume the single slot, not the 5 historical rows.
    let other = f.repo("other");
    let other_request = f.request(&other);
    let mut store_other = f.store();
    let result = Runtime::new(
        &mut store_other,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(FastAdapter::default()) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&other, &other_request);
    assert!(
        matches!(
            result,
            Err(local::Error::CapacityExceeded {
                active: 1,
                limit: 1
            })
        ),
        "{result:?}"
    );

    release.store(true, Ordering::SeqCst);
    handle.join().unwrap().unwrap_err();
}
