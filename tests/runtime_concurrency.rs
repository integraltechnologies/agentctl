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
    fn parallel_plan_repo(&self, name: &str) -> (PathBuf, ExecutionPlan) {
        self.parallel_plan_repo_with(name, "pub fn combined() {}\n")
    }
    fn parallel_plan_repo_with(&self, name: &str, c_source: &str) -> (PathBuf, ExecutionPlan) {
        let root = self.temp.0.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(
            root.join("src/lib.rs"),
            "pub mod a;\npub mod b;\npub mod c;\n",
        )
        .unwrap();
        fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
        fs::write(root.join("src/c.rs"), c_source).unwrap();
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
        let info = RepositoryInfo::discover(&root).unwrap();
        store.register_repository(info.clone()).unwrap();
        store.index_repository(&root).unwrap();
        let prepared = store
            .prepare_plan(
                &root,
                RequestDraft {
                    objective: "Implement independent alpha and beta work, then combine".into(),
                    query: Some("alpha beta combined".into()),
                    scope: vec![ScopePath::Directory { path: "src".into() }],
                    constraints: vec![],
                    definition_of_done: vec!["all work integrated".into()],
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
        let entity = |path: &str, name: &str| {
            store
                .graph(&root)
                .unwrap()
                .locate(name, 10)
                .unwrap()
                .data
                .into_iter()
                .find(|located| {
                    located.entity.provenance.path == path && located.entity.name == name
                })
                .unwrap()
                .entity
                .id
        };
        let alpha = entity("src/a.rs", "alpha");
        let beta = entity("src/b.rs", "beta");
        let combined = entity("src/c.rs", "combined");
        for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            assert!(
                prepared
                    .request
                    .source
                    .support
                    .iter()
                    .any(|p| p.path == path)
            );
        }
        let task =
            |id: &str, path: &str, graph: GraphEntityId, dependencies: Vec<TaskId>| TaskPacket {
                version: ProtocolVersion::V1,
                task_id: TaskId::new(id).unwrap(),
                objective: format!("edit {path}"),
                read_scope: vec![ScopePath::File { path: path.into() }],
                write_scope: vec![ScopePath::File { path: path.into() }],
                graph_entities: vec![graph],
                invariant_refs: vec![],
                dependencies,
                definition_of_done: vec![format!("{path} changed")],
                verification: reqs("unit"),
            };
        let a = task("task:a", "src/a.rs", alpha.clone(), vec![]);
        let b = task("task:b", "src/b.rs", beta, vec![]);
        let c = task(
            "task:c",
            "src/c.rs",
            combined,
            vec![a.task_id.clone(), b.task_id.clone()],
        );
        let d = task("task:d", "src/a.rs", alpha, vec![]);
        let packet = PlanPacket {
            version: ProtocolVersion::V1,
            plan_id: PlanId::new(format!("plan:{name}")).unwrap(),
            objective: prepared.request.intent.objective.clone(),
            tasks: vec![a, b, c, d],
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
                    .map(|task| VerificationContract {
                        task_id: task.task_id.clone(),
                        task_packet_hash: hash(task).unwrap(),
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
/// Waits for a prelude to reach the state the assertion under test needs.
/// The budget bounds nothing the test asserts — it only has to exceed how long
/// a busy machine takes to get there (observed: ~10s for an integration
/// verifier), so it is generous on purpose.
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
                    context_request: None,
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
                    context_request: None,
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
struct CountingControlled {
    inner: Controlled,
    launches: Arc<AtomicUsize>,
}
impl ProviderAdapter for CountingControlled {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        self.inner.launch(input, process, config)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.inner.collect(output)
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

struct OverlapProcess {
    active: Arc<AtomicUsize>,
    overlapped: Arc<AtomicBool>,
    output: Option<ProcessOutput>,
}
impl RunningProcess for OverlapProcess {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        if !self.overlapped.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self.output.take().inspect(|_| {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }))
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.overlapped.store(true, Ordering::SeqCst);
        Ok(CancellationOutcome::Applied)
    }
}

#[derive(Clone)]
struct ScriptedAdapter {
    active: Arc<AtomicUsize>,
    overlapped: Arc<AtomicBool>,
    launches: Arc<AtomicUsize>,
    fail_b: bool,
    /// Reject task:a at its independent verifier, after its branch has already
    /// been reconciled into canonical source.
    reject_a: bool,
}
impl ProviderAdapter for ScriptedAdapter {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(self.clone()))
    }
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
        let value = match input.role {
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = task.write_scope[0].path().to_string();
                let mut text = fs::read_to_string(process.workspace.join(&path)).unwrap();
                text.push_str(&format!("// {}\n", task.task_id.as_str()));
                fs::write(process.workspace.join(&path), text).unwrap();
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id.clone(),
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                    context_request: None,
                })
                .unwrap()
            }
            AgentRole::Verifier => serde_json::to_value(VerificationPacket {
                version: ProtocolVersion::V1,
                verification_id: VerificationId::new(format!(
                    "verification:{}",
                    input.job_id.as_str()
                ))
                .unwrap(),
                target: serde_json::from_value(input.artifact["target"].clone()).unwrap(),
                verifier_job_id: input.job_id.clone(),
                decision: if self.reject_a
                    && input
                        .task_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == "task:a")
                {
                    VerificationDecision::Reject
                } else {
                    VerificationDecision::Pass
                },
                findings: vec![],
                evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                requirement_refs: if input.task_id.is_some() {
                    vec!["unit".into()]
                } else {
                    vec!["integration".into()]
                },
                invariant_refs: vec![],
                notes: None,
                context_request: None,
            })
            .unwrap(),
            AgentRole::Planner => unreachable!(),
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        if input.role == AgentRole::Executor
            && matches!(
                input.task_id.as_ref().map(TaskId::as_str),
                Some("task:a" | "task:b")
            )
        {
            self.launches.fetch_add(1, Ordering::SeqCst);
            let previous = self.active.fetch_add(1, Ordering::SeqCst);
            if previous >= 1 {
                self.overlapped.store(true, Ordering::SeqCst);
            }
            Ok(Box::new(OverlapProcess {
                active: self.active.clone(),
                overlapped: self.overlapped.clone(),
                output: Some(
                    if self.fail_b
                        && input
                            .task_id
                            .as_ref()
                            .is_some_and(|id| id.as_str() == "task:b")
                    {
                        ProcessOutput {
                            exit: Some(1),
                            stdout: bytes,
                            stderr: b"injected".to_vec(),
                            failure: Some("injected sibling failure".into()),
                        }
                    } else {
                        success(bytes)
                    },
                ),
            }))
        } else {
            Ok(Box::new(Done(Some(success(bytes)))))
        }
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[derive(Clone, Copy)]
enum PrelaunchCancellation {
    BeforeFirst,
    BetweenBranches,
}

#[derive(Clone)]
struct CancellingScriptedAdapter {
    inner: ScriptedAdapter,
    database: PathBuf,
    root: PathBuf,
    plan: PlanId,
    timing: PrelaunchCancellation,
    preflights: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
}
impl CancellingScriptedAdapter {
    fn request_cancel(&self) {
        let mut store = Store::open(&self.database, 5_000).unwrap();
        store.runtime_cancel(&self.root, &self.plan).unwrap();
        self.cancelled.store(true, Ordering::SeqCst);
    }
    fn wait_for(&self, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out at prelaunch test boundary"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}
impl ProviderAdapter for CancellingScriptedAdapter {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(self.clone()))
    }
    fn preflight(&self) -> local::Result<()> {
        let ordinal = self.preflights.fetch_add(1, Ordering::SeqCst);
        match self.timing {
            PrelaunchCancellation::BeforeFirst => {
                if ordinal == 0 {
                    self.request_cancel();
                } else {
                    self.wait_for(|| self.cancelled.load(Ordering::SeqCst));
                }
            }
            PrelaunchCancellation::BetweenBranches if ordinal == 1 => {
                self.wait_for(|| self.inner.launches.load(Ordering::SeqCst) == 1);
                self.request_cancel();
            }
            PrelaunchCancellation::BetweenBranches => {}
        }
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        self.inner.launch(input, process, config)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.inner.collect(output)
    }
}

#[test]
fn concurrent_compatible_tasks_overlap_in_isolated_worktrees_and_reconcile_before_dependents() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent");
    let store = fixture.store();
    let info = RepositoryInfo::discover(&root).unwrap();
    let a = &plan.packet.tasks[0];
    let b = &plan.packet.tasks[1];
    let c = &plan.packet.tasks[2];
    let d = &plan.packet.tasks[3];
    let compatible = concurrency::decide(&store, &info, &plan, a, b).unwrap();
    assert_eq!(
        compatible.decision,
        concurrency::Compatibility::Compatible,
        "{compatible:?}"
    );
    let dependency = concurrency::decide(&store, &info, &plan, a, c).unwrap();
    assert_eq!(
        dependency.decision,
        concurrency::Compatibility::DependencyBlocked
    );
    let conflict = concurrency::decide(&store, &info, &plan, a, d).unwrap();
    assert_eq!(conflict.decision, concurrency::Compatibility::Conflict);
    let mut ambiguous = b.clone();
    ambiguous.task_id = TaskId::new("task:ambiguous").unwrap();
    ambiguous.graph_entities.clear();
    let unknown = concurrency::decide(&store, &info, &plan, a, &ambiguous).unwrap();
    assert_eq!(unknown.decision, concurrency::Compatibility::Unknown);
    drop(store);

    let active = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: active.clone(),
                overlapped: overlapped.clone(),
                launches: Arc::new(AtomicUsize::new(0)),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id);
    let run = result.unwrap_or_else(|error| {
        panic!(
            "{error}; run={:?}; tasks={:?}; jobs={:?}; events={:?}",
            store.runtime_status(&root, &plan.packet.plan_id).unwrap(),
            store
                .tasks(&info.repository_id, Some(&plan.packet.plan_id))
                .unwrap(),
            store
                .runtime_jobs(&root, Some(&plan.packet.plan_id))
                .unwrap(),
            store
                .events(Some(&info.repository_id), None, None, 50)
                .unwrap()
        )
    });
    assert_eq!(run.state, RunState::Complete);
    assert!(
        overlapped.load(Ordering::SeqCst),
        "independent executor jobs never overlapped"
    );
    assert!(run.branches.is_empty());
    assert!(plan.packet.tasks.iter().all(|task| {
        store
            .task(&info.repository_id, &task.task_id)
            .unwrap()
            .unwrap()
            .state
            == TaskState::Verified
    }));
    assert!(
        fs::read_to_string(root.join("src/a.rs"))
            .unwrap()
            .contains("task:a")
    );
    assert!(
        fs::read_to_string(root.join("src/b.rs"))
            .unwrap()
            .contains("task:b")
    );
    assert!(
        fs::read_to_string(root.join("src/c.rs"))
            .unwrap()
            .contains("task:c")
    );
}

#[test]
fn concurrent_executor_failure_is_branch_local_and_does_not_discard_verified_sibling() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-failure");
    let info = RepositoryInfo::discover(&root).unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active,
                overlapped: overlapped.clone(),
                launches: Arc::new(AtomicUsize::new(0)),
                fail_b: true,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert!(overlapped.load(Ordering::SeqCst));
    assert_eq!(
        store
            .task(&info.repository_id, &TaskId::new("task:a").unwrap())
            .unwrap()
            .unwrap()
            .state,
        TaskState::Verified
    );
    assert_eq!(
        store
            .task(&info.repository_id, &TaskId::new("task:b").unwrap())
            .unwrap()
            .unwrap()
            .state,
        TaskState::Blocked
    );
    assert!(
        fs::read_to_string(root.join("src/a.rs"))
            .unwrap()
            .contains("task:a")
    );
    // The failed branch published nothing: its sibling is accepted and it is
    // not, whatever a later bounded relaunch may have left in the tree for
    // `agentctl run restore` to discard.
    let run = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert!(run.accepted.contains_key(&TaskId::new("task:a").unwrap()));
    assert!(!run.accepted.contains_key(&TaskId::new("task:b").unwrap()));
    assert!(run.branches.is_empty() && run.reconciliation.is_none());
}

#[test]
fn source_change_after_selection_before_claim_prevents_executor_launch() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-stale-source");
    let launches = Arc::new(AtomicUsize::new(0));
    let mutate_root = root.clone();
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: launches.clone(),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .with_boundary_observer(move |boundary| {
        if boundary == "batch_preclaim" {
            fs::write(mutate_root.join("src/a.rs"), "external drift\n").unwrap();
        }
    })
    .run(&root, &plan.packet.plan_id);
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("stale")
            || error.to_string().contains("STALE_CONCURRENCY_AUTHORITY"),
        "{error}"
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    let durable = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert!(durable.batch_authority.is_none());
    assert!(durable.branches.is_empty());
}

#[test]
fn ontology_change_after_revalidation_before_claim_prevents_executor_launch() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-stale-ontology");
    let launches = Arc::new(AtomicUsize::new(0));
    let database = fixture.paths.database.clone();
    let info = RepositoryInfo::discover(&root).unwrap();
    let workspace = info.workspace_id.as_str().to_string();
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: launches.clone(),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .with_boundary_observer(move |boundary| {
        if boundary == "batch_revalidated" {
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute(
                    "UPDATE graph_indexes SET metadata_json=json_set(metadata_json,'$.generation.sequence',json_extract(metadata_json,'$.generation.sequence')+1,'$.generation.fingerprint','blake3:stale-window') WHERE workspace_id=?1",
                    [&workspace],
                )
                .unwrap();
        }
    })
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    let durable = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert!(durable.batch_authority.is_none());
    assert!(durable.branches.is_empty());
}

#[test]
fn lifecycle_change_after_revalidation_is_rejected_in_atomic_batch_claim() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-stale-task");
    let launches = Arc::new(AtomicUsize::new(0));
    let paths = fixture.paths.clone();
    let mutate_root = root.clone();
    let mutate_plan = plan.packet.plan_id.clone();
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: launches.clone(),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .with_boundary_observer(move |boundary| {
        if boundary == "batch_revalidated" {
            let mut other = Store::open(&paths.database, 5000).unwrap();
            other.runtime_cancel(&mutate_root, &mutate_plan).unwrap();
        }
    })
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert!(
        store
            .runtime_status(&root, &plan.packet.plan_id)
            .unwrap()
            .unwrap()
            .batch_authority
            .is_none()
    );
}

#[test]
fn cancellation_after_batch_authorization_before_first_launch_prevents_all_launches() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-cancel-before-first-launch");
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = CancellingScriptedAdapter {
        inner: ScriptedAdapter {
            active: Arc::new(AtomicUsize::new(0)),
            overlapped: Arc::new(AtomicBool::new(false)),
            launches: launches.clone(),
            fail_b: false,
            reject_a: false,
        },
        database: fixture.paths.database.clone(),
        root: root.clone(),
        plan: plan.packet.plan_id.clone(),
        timing: PrelaunchCancellation::BeforeFirst,
        preflights: Arc::new(AtomicUsize::new(0)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([("test".into(), Box::new(adapter) as Box<dyn ProviderAdapter>)]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_between_branch_prelaunch_boundaries_prevents_second_launch() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-cancel-between-launches");
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = CancellingScriptedAdapter {
        inner: ScriptedAdapter {
            active: Arc::new(AtomicUsize::new(0)),
            overlapped: Arc::new(AtomicBool::new(false)),
            launches: launches.clone(),
            fail_b: false,
            reject_a: false,
        },
        database: fixture.paths.database.clone(),
        root: root.clone(),
        plan: plan.packet.plan_id.clone(),
        timing: PrelaunchCancellation::BetweenBranches,
        preflights: Arc::new(AtomicUsize::new(0)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([("test".into(), Box::new(adapter) as Box<dyn ProviderAdapter>)]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_at_task_verifier_prelaunch_boundary_prevents_verifier_issuance() {
    let mut fixture = Fixture::new(1);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.plan_repo("concurrent-cancel-before-verifier");
    let launches = Arc::new(AtomicUsize::new(0));
    let boundaries = Arc::new(AtomicUsize::new(0));
    let observer_boundaries = boundaries.clone();
    let database = fixture.paths.database.clone();
    let cancel_root = root.clone();
    let cancel_plan = plan.packet.plan_id.clone();
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(CountingControlled {
                inner: Controlled {
                    hang_at: HangAt::IntegrationVerifier,
                    release: Arc::new(AtomicBool::new(true)),
                },
                launches: launches.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .with_policy_observer(move |boundary| {
        if boundary == "prelaunch" && observer_boundaries.fetch_add(1, Ordering::SeqCst) == 1 {
            let mut other = Store::open(&database, 5_000).unwrap();
            other.runtime_cancel(&cancel_root, &cancel_plan).unwrap();
        }
    })
    .run(&root, &plan.packet.plan_id);
    assert!(result.is_err());
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(boundaries.load(Ordering::SeqCst), 2);
}

#[test]
fn restart_recovers_durable_intent_before_first_write_without_rerunning_executors() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-restart-intent");
    let launches = Arc::new(AtomicUsize::new(0));
    let adapter = ScriptedAdapter {
        active: Arc::new(AtomicUsize::new(0)),
        overlapped: Arc::new(AtomicBool::new(false)),
        launches: launches.clone(),
        fail_b: false,
        reject_a: false,
    };
    let mut store = fixture.store();
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Runtime::new(
            &mut store,
            fixture.paths.clone(),
            fixture.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(adapter.clone()) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks))
        .with_boundary_observer(|boundary| {
            if boundary == "reconciliation_intent_durable" {
                panic!("injected crash before first publication write");
            }
        })
        .run(&root, &plan.packet.plan_id)
    }));
    assert!(interrupted.is_err());
    let durable = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert!(durable.reconciliation.is_some());
    assert_eq!(launches.load(Ordering::SeqCst), 2);
    drop(store);

    let mut reopened = fixture.store();
    let recovered = Runtime::new(
        &mut reopened,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([("test".into(), Box::new(adapter) as Box<dyn ProviderAdapter>)]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id)
    .unwrap();
    assert_eq!(recovered.state, RunState::Complete);
    assert!(recovered.reconciliation.is_none());
    assert_eq!(
        launches.load(Ordering::SeqCst),
        2,
        "captured concurrent executors were not rerun"
    );
}

#[test]
fn unresolved_publication_blocks_verifier_and_dependent_release() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("concurrent-unresolved-intent");
    let mutate_root = root.clone();
    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: Arc::new(AtomicUsize::new(0)),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .with_boundary_observer(move |boundary| {
        if boundary == "reconciliation_intent_durable" {
            fs::write(mutate_root.join("src/a.rs"), "third-party state\n").unwrap();
        }
    })
    .run(&root, &plan.packet.plan_id);
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("RECONCILIATION_UNRESOLVED")
    );
    let durable = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(durable.state, RunState::Running);
    assert!(durable.reconciliation.is_some());
    assert!(durable.pending.is_none());
    assert_eq!(
        fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "third-party state\n"
    );
    assert!(
        store
            .runtime_jobs(&root, Some(&plan.packet.plan_id))
            .unwrap()
            .iter()
            .all(|job| job.role != AgentRole::Verifier)
    );
    let dependent = store
        .task(
            &RepositoryInfo::discover(&root).unwrap().repository_id,
            &TaskId::new("task:c").unwrap(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(dependent.state, TaskState::Planned);
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
    // Malformed output is a bounded, retryable provider failure: each attempt
    // is its own launch, and none of them exceeded capacity.
    assert_eq!(
        launches.load(Ordering::SeqCst),
        1 + agentctl::local::runtime::MAX_PROVIDER_RETRIES as usize
    );
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
    // Each malformed planner reply is retried within its bound, sequentially.
    assert_eq!(
        jobs.len(),
        3 * (1 + agentctl::local::runtime::MAX_PROVIDER_RETRIES as usize)
    );
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
    let job = wait_for_running_matching(&f.store(), &root_a, Duration::from_secs(120), |j| {
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
    let job = wait_for_running_matching(&f.store(), &root_a, Duration::from_secs(120), |j| {
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
    assert_eq!(
        jobs.len(),
        5 * (1 + agentctl::local::runtime::MAX_PROVIDER_RETRIES as usize)
    );
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

/// Every other test in this file drives concurrency through adapters that
/// discard the `ProcessSpec`, so none of them reaches `security::compile`.
/// That is exactly how managed worktrees shipped inside `data_root` — a
/// location the security compiler refuses — while all of them passed.
///
/// This one launches real OS processes for both branches through
/// `NativeProcess`, so the branch spec is compiled and capability-checked for
/// real, and asserts the two workers actually overlap.
struct NativeBranch {
    active: Arc<AtomicUsize>,
    overlapped: Arc<AtomicBool>,
}
impl Clone for NativeBranch {
    fn clone(&self) -> Self {
        Self {
            active: self.active.clone(),
            overlapped: self.overlapped.clone(),
        }
    }
}
impl ProviderAdapter for NativeBranch {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(self.clone()))
    }
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
        mut process: ProcessSpec,
        _: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let (script, hold) = match input.role {
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = task.write_scope[0].path().to_string();
                let reply = serde_json::to_string(&ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id.clone(),
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path.clone()],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                    context_request: None,
                })
                .unwrap();
                (
                    format!(
                        "printf '// %s\\n' '{}' >> '{path}'; sleep 2; printf '%s' '{reply}'",
                        task.task_id.as_str()
                    ),
                    true,
                )
            }
            _ => {
                let reply = serde_json::to_string(&VerificationPacket {
                    version: ProtocolVersion::V1,
                    verification_id: VerificationId::new(format!(
                        "verification:{}",
                        input.job_id.as_str()
                    ))
                    .unwrap(),
                    target: serde_json::from_value(input.artifact["target"].clone()).unwrap(),
                    verifier_job_id: input.job_id.clone(),
                    decision: VerificationDecision::Pass,
                    findings: vec![],
                    evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                    requirement_refs: if input.task_id.is_some() {
                        vec!["unit".into()]
                    } else {
                        vec!["integration".into()]
                    },
                    invariant_refs: vec![],
                    notes: None,
                    context_request: None,
                })
                .unwrap();
                (format!("printf '%s' '{reply}'"), false)
            }
        };
        process.executable = PathBuf::from("/bin/sh");
        process.args = vec!["-c".into(), script];
        process.input = vec![];
        // The real compile/check gate runs inside NativeProcess::launch.
        let running = NativeProcess::launch(&process)?;
        if hold && self.active.fetch_add(1, Ordering::SeqCst) + 1 > 1 {
            self.overlapped.store(true, Ordering::SeqCst);
        }
        Ok(Box::new(Counted {
            inner: Box::new(running),
            active: hold.then(|| self.active.clone()),
        }))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}
struct Counted {
    inner: Box<dyn RunningProcess>,
    active: Option<Arc<AtomicUsize>>,
}
impl RunningProcess for Counted {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }
    fn liveness_confirmed(&self) -> bool {
        self.inner.liveness_confirmed()
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        let polled = self.inner.poll()?;
        if polled.is_some()
            && let Some(active) = self.active.take()
        {
            active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(polled)
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        self.inner.cancel()
    }
}

#[test]
#[ignore = "launches real sandboxed OS processes"]
fn native_compatible_branches_launch_through_the_security_gate_and_overlap() {
    let fixture = Fixture::new(4);
    let (root, plan) = fixture.parallel_plan_repo("native-branches");
    let info = RepositoryInfo::discover(&root).unwrap();
    let mut store = fixture.store();
    let overlapped = Arc::new(AtomicBool::new(false));
    let outcome = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(NativeBranch {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: overlapped.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .run(&root, &plan.packet.plan_id)
    .unwrap();

    assert_eq!(outcome.state, RunState::Complete);
    assert!(
        overlapped.load(Ordering::SeqCst),
        "two sandboxed worker processes must have been alive at once"
    );
    // Each branch ran in its own managed worktree, outside agentctl state.
    let roots: Vec<String> = store
        .runtime_jobs(&root, Some(&plan.packet.plan_id))
        .unwrap()
        .into_iter()
        .filter(|job| job.role == AgentRole::Executor)
        .filter_map(|job| job.execution_root)
        .filter(|path| Path::new(path) != root)
        .collect();
    let distinct: std::collections::BTreeSet<&String> = roots.iter().collect();
    assert!(
        distinct.len() >= 2,
        "each concurrent branch needs its own worktree: {roots:?}"
    );
    // macOS resolves the temp root through /private, so compare canonically.
    let worktree_root = fs::canonicalize(&fixture.paths.worktree_root).unwrap();
    let data_root = fs::canonicalize(&fixture.paths.data_root).unwrap();
    for branch in &distinct {
        let branch = Path::new(branch.as_str());
        assert!(
            branch.starts_with(&worktree_root),
            "{branch:?} must be a managed worktree under {worktree_root:?}"
        );
        assert!(!branch.starts_with(&data_root));
    }
    // Both results reconciled into canonical source, and nothing leaked.
    for task in &plan.packet.tasks {
        let path = task.write_scope[0].path();
        assert!(
            fs::read_to_string(root.join(path))
                .unwrap()
                .contains(task.task_id.as_str())
        );
        assert_eq!(
            store
                .task(&info.repository_id, &task.task_id)
                .unwrap()
                .unwrap()
                .state,
            TaskState::Verified
        );
    }
    assert!(
        fs::read_dir(worktree_root.join(info.workspace_id.as_str()))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "managed worktrees must not accumulate"
    );
}

/// A-1: reconciliation publishes a concurrent branch into canonical source
/// before its verifier runs, so the run's `expected` source carries work no
/// verifier has accepted. Discarding a refused result must rewind to the last
/// *verified* source, not to that publication.
#[test]
fn rejected_reconciled_branch_restores_to_the_last_verified_source() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("reject-restore");
    let info = RepositoryInfo::discover(&root).unwrap();
    let id = plan.packet.plan_id.clone();
    let baseline = fs::read_to_string(root.join("src/a.rs")).unwrap();

    let mut store = fixture.store();
    let result = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: Arc::new(AtomicUsize::new(0)),
                fail_b: false,
                reject_a: true,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &id);
    assert!(result.is_err(), "a rejected task must stop the run");

    // The refused branch really was published into canonical source first.
    let published = fs::read_to_string(root.join("src/a.rs")).unwrap();
    assert!(
        published.contains("task:a") && published != baseline,
        "reconciliation should have published the branch: {published:?}"
    );

    // An unrelated operator edit must stop the discard naming that file.
    fs::write(root.join("src/c.rs"), "pub fn combined() {}\n// operator\n").unwrap();
    let mut runtime = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    let refused = runtime.restore(&root, &id).unwrap_err().to_string();
    assert!(refused.contains("src/c.rs"), "{refused}");
    fs::write(root.join("src/c.rs"), "pub fn combined() {}\n").unwrap();

    // Now the discard rewinds exactly to the last verified source.
    let mut runtime = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    let run = runtime.restore(&root, &id).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("src/a.rs")).unwrap(),
        baseline,
        "restore must discard the reconciled but unverified branch"
    );
    assert_eq!(run.state, RunState::Blocked);
    assert!(run.refused.is_none() && run.pending.is_none());

    // No acceptance happened, and the dependent stays locked.
    let state = |id: &str| {
        store
            .task(&info.repository_id, &TaskId::new(id).unwrap())
            .unwrap()
            .unwrap()
            .state
    };
    assert!(
        matches!(state("task:a"), TaskState::Rejected | TaskState::Blocked),
        "refused task must not be verified: {:?}",
        state("task:a")
    );
    assert!(matches!(
        state("task:c"),
        TaskState::Planned | TaskState::Blocked
    ));
    assert!(!run.accepted.contains_key(&TaskId::new("task:a").unwrap()));
}

/// NEW-1: a concurrent executor's diff is captured in its isolated worktree,
/// while the deterministic checks run on canonical source after reconciliation.
/// The task verifier must be able to establish that the canonical source it was
/// issued, and that its check evidence is bound to, is exactly the result of
/// publishing that diff — rather than inferring it. Asserted at the real
/// artifact boundary: the stored verifier JobInput.
#[test]
fn concurrent_task_verifier_artifact_carries_the_reconciliation_chain() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("recon-chain");
    let id = plan.packet.plan_id.clone();

    let mut store = fixture.store();
    let run = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(ScriptedAdapter {
                active: Arc::new(AtomicUsize::new(0)),
                overlapped: Arc::new(AtomicBool::new(false)),
                launches: Arc::new(AtomicUsize::new(0)),
                fail_b: false,
                reject_a: false,
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &id)
    .unwrap();
    assert_eq!(run.state, RunState::Complete);

    let artifacts = source::Artifacts::new(&fixture.paths.data_root.join("runtime/blobs")).unwrap();
    let jobs = store.runtime_jobs(&root, Some(&id)).unwrap();
    let verifier_input = |task: &str| -> Value {
        let job = jobs
            .iter()
            .find(|j| {
                j.role == AgentRole::Verifier
                    && j.task_id.as_ref().is_some_and(|t| t.as_str() == task)
            })
            .unwrap_or_else(|| panic!("no verifier job for {task}"));
        artifacts
            .decode::<JobInput>(&job.input)
            .unwrap()
            .artifact
            .clone()
    };

    // task:a and task:b ran concurrently in isolated worktrees.
    let mut diverged = 0;
    for task in ["task:a", "task:b"] {
        let artifact = verifier_input(task);
        let chain = &artifact["reconciliation"];
        assert!(
            chain.is_object(),
            "{task} ran in a worktree and must carry its publication chain: {artifact}"
        );
        assert_eq!(chain["task_id"].as_str(), Some(task));
        assert_eq!(
            chain["executor_job_id"],
            artifact["target"]["executor_job_id"]
        );
        assert!(
            chain["execution_workspace"].is_string(),
            "the isolated worktree must be named: {chain}"
        );

        // Every identity the verifier needs, and they must actually agree.
        let branch = &chain["branch_result"];
        let before = &chain["canonical_before"];
        let after = &chain["canonical_after"];
        for (name, value) in [
            ("branch_result", branch),
            ("canonical_before", before),
            ("canonical_after", after),
        ] {
            assert!(
                value.is_object(),
                "{name} must be a source identity: {chain}"
            );
        }
        assert_ne!(
            before, after,
            "publication must have changed canonical source"
        );
        if branch != after {
            diverged += 1;
        }

        // The chain names the diff the verifier was shown...
        assert_eq!(chain["diff"], artifact["diff"]["artifact"], "{chain}");

        // ...and canonical_after is the source every check evidence is bound to.
        let records = artifact["evidence_records"]
            .as_array()
            .expect("evidence records");
        assert!(!records.is_empty());
        let checks: Vec<&Value> = records
            .iter()
            .filter(|r| r["command"].is_object())
            .collect();
        assert!(!checks.is_empty(), "no check evidence: {records:?}");
        for record in checks {
            assert_eq!(
                &record["source_state"], after,
                "check evidence must be bound to the reconciled canonical source"
            );
        }
    }

    // The first branch to reconcile lands on unchanged canonical source, so its
    // result and the canonical result coincide. The second does not — that is
    // exactly the divergence the real verifier refused to infer across.
    assert!(
        diverged >= 1,
        "no concurrent branch diverged from canonical source; NEW-1 was not exercised"
    );

    // The serial negative (a run that never leaves canonical source must not
    // claim a publication chain) is asserted in tests/runtime.rs, where serial
    // execution is the default.
}

/// Two tasks that look independent only because the one relation
/// between them sits inside a macro. The call `crate::a::alpha()` inside
/// `format!` is invisible as a call expression; it must still surface as an
/// unresolved site naming `alpha`, so the gate cannot claim proven
/// non-interference and the tasks serialize. The same fixture without that
/// call stays compatible (see the concurrent tests above).
#[test]
fn macro_hidden_relation_prevents_a_false_non_interference_decision() {
    let fixture = Fixture::new(4);
    let (root, plan) = fixture.parallel_plan_repo("macro-hidden");
    let a = &plan.packet.tasks[0];
    let b = &plan.packet.tasks[1];
    {
        let store = fixture.store();
        let info = RepositoryInfo::discover(&root).unwrap();
        let before = concurrency::decide(&store, &info, &plan, a, b).unwrap();
        assert_eq!(
            before.decision,
            concurrency::Compatibility::Compatible,
            "{before:?}"
        );
    }
    fs::write(
        root.join("src/b.rs"),
        "pub fn beta() -> String {\n    format!(\"{:?}\", crate::a::alpha())\n}\n",
    )
    .unwrap();
    git(&root, &["commit", "--quiet", "-am", "beta formats alpha"]);
    fixture.store().index_repository(&root).unwrap();
    let store = fixture.store();
    let info = RepositoryInfo::discover(&root).unwrap();
    let after = concurrency::decide(&store, &info, &plan, a, b).unwrap();
    assert_eq!(
        after.decision,
        concurrency::Compatibility::Unknown,
        "{after:?}"
    );
    let reasons = serde_json::to_string(&after.reasons).unwrap();
    assert!(reasons.contains("UnresolvedReferences"), "{reasons}");
    assert!(
        !reasons.contains("ONTOLOGY_PROVES_NO_INTERFERENCE"),
        "{reasons}"
    );
}

/// Refuses the first verifier launch with a provider quota fault, then
/// behaves exactly like the scripted adapter.
#[derive(Clone)]
struct QuotaAtFirstVerifier {
    inner: ScriptedAdapter,
    tripped: Arc<AtomicBool>,
}
impl ProviderAdapter for QuotaAtFirstVerifier {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(self.clone()))
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        if input.role == AgentRole::Verifier && !self.tripped.swap(true, Ordering::SeqCst) {
            return Ok(Box::new(Done(Some(ProcessOutput {
                exit: Some(1),
                stdout: vec![],
                stderr: b"You've hit your session limit".to_vec(),
                failure: None,
            }))));
        }
        self.inner.launch(input, process, config)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.inner.collect(output)
    }
    fn fault(&self, output: &ProcessOutput) -> Option<(bool, String)> {
        String::from_utf8_lossy(&output.stderr)
            .contains("session limit")
            .then(|| (false, "rate or usage limit".into()))
    }
}

/// Observed in a real run: a provider quota refusal at the first verifier,
/// after the concurrent batch captured both branches and reconciled one. No
/// uncertain change exists — the other branch is durable in its worktree — so
/// the run stays resumable, and resuming completes it through ordinary
/// reconciliation without re-executing either branch.
#[test]
fn quota_refusal_after_a_concurrent_batch_pauses_and_resumes_to_completion() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    let (root, plan) = fixture.parallel_plan_repo("quota-pause");
    let info = RepositoryInfo::discover(&root).unwrap();
    let scripted = || ScriptedAdapter {
        active: Arc::new(AtomicUsize::new(0)),
        overlapped: Arc::new(AtomicBool::new(false)),
        launches: Arc::new(AtomicUsize::new(0)),
        fail_b: false,
        reject_a: false,
    };
    let first = scripted();
    let launches = first.launches.clone();
    let mut store = fixture.store();
    let error = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(QuotaAtFirstVerifier {
                inner: first,
                tripped: Arc::new(AtomicBool::new(false)),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id)
    .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("NONRETRYABLE_PROVIDER_FAILURE"),
        "{error}"
    );
    let paused = store
        .runtime_status(&root, &plan.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(paused.state, RunState::Running, "{:?}", paused.reason);
    assert!(paused.pending.is_some() || !paused.branches.is_empty());
    assert!(paused.accepted.is_empty());
    let executed = launches.load(Ordering::SeqCst);
    assert_eq!(executed, 2, "both branches ran once");

    let second = scripted();
    let relaunched = second.launches.clone();
    let run = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([("test".into(), Box::new(second) as Box<dyn ProviderAdapter>)]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id)
    .unwrap();
    assert_eq!(run.state, RunState::Complete);
    assert_eq!(
        relaunched.load(Ordering::SeqCst),
        0,
        "captured branches are reconciled, never re-executed"
    );
    assert!(run.branches.is_empty());
    for task in &plan.packet.tasks {
        assert_eq!(
            store
                .task(&info.repository_id, &task.task_id)
                .unwrap()
                .unwrap()
                .state,
            TaskState::Verified
        );
    }
}

/// Executor for task:a additionally declares `alpha2`; everything else is the
/// scripted adapter.
#[derive(Clone)]
struct DeclaresAlpha2(ScriptedAdapter);
impl ProviderAdapter for DeclaresAlpha2 {
    fn fork(&self) -> Option<Box<dyn ProviderAdapter>> {
        Some(Box::new(self.clone()))
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        process: ProcessSpec,
        config: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let workspace = process.workspace.clone();
        let running = self.0.launch(input, process, config)?;
        if input.role == AgentRole::Executor
            && input
                .task_id
                .as_ref()
                .is_some_and(|t| t.as_str() == "task:a")
        {
            let path = workspace.join("src/a.rs");
            let text = fs::read_to_string(&path).unwrap();
            fs::write(&path, format!("{text}pub fn alpha2() {{}}\n")).unwrap();
        }
        Ok(running)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        self.0.collect(output)
    }
}

/// Real-run finding: a sibling verified first can change facts the branch's
/// compatibility proof relied on. When publishing the captured branch can no
/// longer be proven safe, it is withdrawn and the task re-executes serially on
/// the current source; nothing unproven is published and the plan completes.
#[test]
fn an_unprovable_captured_branch_is_serialized_not_published() {
    let mut fixture = Fixture::new(4);
    fixture.config.timeout_ms = 5_000;
    // A macro call to `alpha2`, which nothing declares yet: invisible to the
    // batch decision, an open question once task a declares it.
    let (root, plan) = fixture.parallel_plan_repo_with(
        "serialize",
        "pub fn combined() -> String {\n    format!(\"{:?}\", crate::a::alpha2())\n}\n",
    );
    let info = RepositoryInfo::discover(&root).unwrap();
    let adapter = ScriptedAdapter {
        active: Arc::new(AtomicUsize::new(0)),
        overlapped: Arc::new(AtomicBool::new(false)),
        launches: Arc::new(AtomicUsize::new(0)),
        fail_b: false,
        reject_a: false,
    };
    let launches = adapter.launches.clone();
    let mut store = fixture.store();
    let run = Runtime::new(
        &mut store,
        fixture.paths.clone(),
        fixture.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(DeclaresAlpha2(adapter)) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Checks))
    .run(&root, &plan.packet.plan_id)
    .unwrap();
    assert_eq!(run.state, RunState::Complete);
    assert_eq!(
        launches.load(Ordering::SeqCst),
        3,
        "a and b concurrently, then b again serially"
    );
    let serialized = store
        .events(Some(&info.repository_id), None, None, 1000)
        .unwrap()
        .iter()
        .filter(|e| {
            serde_json::to_string(e)
                .unwrap()
                .contains("BRANCH_SERIALIZED")
        })
        .count();
    assert!(serialized >= 1);
    for task in &plan.packet.tasks {
        assert_eq!(
            store
                .task(&info.repository_id, &task.task_id)
                .unwrap()
                .unwrap()
                .state,
            TaskState::Verified
        );
    }
    assert!(run.branches.is_empty());
}
