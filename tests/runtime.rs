#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{MachineConfig, ProjectConfig, VerificationDefinition},
        paths::{MachinePaths, PathContext},
        planning::*,
        repository::RepositoryInfo,
        runtime::{process::*, provider::*, *},
        store::Store,
    },
    protocol::*,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

struct Fixture {
    temp: common::TempDir,
    root: PathBuf,
    paths: MachinePaths,
    config: RuntimeConfig,
}
impl Fixture {
    fn new() -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        for file in ["api", "graph", "cli", "regression"] {
            fs::write(
                root.join(format!("src/{file}.rs")),
                format!("pub fn cache_{file}() {{}}\n"),
            )
            .unwrap();
        }
        let mut policy = ProjectConfig::initialize(&root).unwrap();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "/usr/bin/true".into(),
                args: vec![],
                cwd: ".".into(),
            },
        );
        for name in ["unit", "integration"] {
            policy.verification.insert(
                name.into(),
                VerificationDefinition {
                    description: format!("{name} tests"),
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
        let mut s = Store::open(&paths.database, 5000).unwrap();
        s.register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        s.index_repository(&root).unwrap();
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
    fn prepare(&self) -> PlannerPacket {
        self.store()
            .prepare_plan(
                &self.root,
                RequestDraft {
                    objective: "Implement cache persistence graph CLI regression support".into(),
                    query: Some("cache".into()),
                    scope: vec![ScopePath::Directory { path: "src".into() }],
                    constraints: vec!["No Git history mutation".into()],
                    definition_of_done: vec!["Cache API graph CLI regression checks pass".into()],
                    verification: Some(requirements("integration")),
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
    }
    fn plan(&self) -> ExecutionPlan {
        let p = artifact(&self.prepare());
        self.store().import_execution_plan(&self.root, &p).unwrap();
        self.store()
            .activate_execution_plan(&self.root, &p.packet.plan_id)
            .unwrap();
        p
    }
    fn run(
        &self,
        p: &ExecutionPlan,
        mode: Mode,
        seen: Arc<Mutex<Vec<JobInput>>>,
        fail_check: bool,
    ) -> local::Result<RunRecord> {
        let mut s = self.store();
        Runtime::new(
            &mut s,
            self.paths.clone(),
            self.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Fake { mode, seen }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: fail_check }))
        .run(&self.root, &p.packet.plan_id)
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
fn requirements(name: &str) -> VerificationRequirements {
    VerificationRequirements {
        requirement_refs: vec![name.into()],
        evidence_required: true,
    }
}
fn artifact(prepared: &PlannerPacket) -> ExecutionPlan {
    let tasks: Vec<_> = ["api", "graph", "cli", "regression"]
        .iter()
        .enumerate()
        .map(|(i, name)| TaskPacket {
            version: ProtocolVersion::V1,
            task_id: TaskId::new(format!("task:{i}")).unwrap(),
            objective: format!("Implement cache {name}"),
            read_scope: vec![ScopePath::Directory { path: "src".into() }],
            write_scope: vec![ScopePath::File {
                path: format!("src/{name}.rs"),
            }],
            graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
            invariant_refs: prepared.request.intent.invariant_refs.clone(),
            dependencies: if i == 0 {
                vec![]
            } else {
                vec![TaskId::new(format!("task:{}", if i == 1 { 0 } else { 1 })).unwrap()]
            },
            definition_of_done: vec![format!("{name} works")],
            verification: requirements("unit"),
        })
        .collect();
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:runtime").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks,
        integration_verification: requirements("integration"),
    };
    ExecutionPlan {
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
    }
}
#[derive(Clone)]
enum Mode {
    Panic,
    Usage(TokenUsageProvenance),
    Pass,
    Reject,
    Malformed,
    Crash,
    Timeout,
    WrongJob,
    WrongEvidence,
    Scope,
    VerifierDrift,
    IntegrationDrift,
    Planner(Box<ExecutionPlan>),
}
struct Fake {
    mode: Mode,
    seen: Arc<Mutex<Vec<JobInput>>>,
}
impl ProviderAdapter for Fake {
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
        self.seen.lock().unwrap().push(input.clone());
        if matches!(self.mode, Mode::Panic) {
            panic!("simulated abrupt controller loss");
        }
        if matches!(self.mode, Mode::Timeout) {
            return Ok(Box::new(Hang { cancelled: false }));
        }
        if matches!(self.mode, Mode::Crash) {
            return Ok(Box::new(Immediate(Some(ProcessOutput {
                exit: Some(9),
                stdout: vec![],
                stderr: b"fixture crash".to_vec(),
                failure: None,
            }))));
        }
        if matches!(self.mode, Mode::Malformed) {
            return Ok(Box::new(Immediate(Some(success(b"not JSON".to_vec())))));
        }
        let value = match input.role {
            AgentRole::Planner => match &self.mode {
                Mode::Planner(p) => serde_json::to_value(p).unwrap(),
                _ => json!({}),
            },
            AgentRole::Executor => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = if matches!(self.mode, Mode::Scope) {
                    "outside.txt"
                } else {
                    task.write_scope[0].path()
                };
                let prior = fs::read_to_string(process.workspace.join(path)).unwrap_or_default();
                fs::write(
                    process.workspace.join(path),
                    format!(
                        "{prior}// accepted fixture change for {}\n",
                        task.task_id.as_str()
                    ),
                )
                .unwrap();
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id,
                    executor_job_id: if matches!(self.mode, Mode::WrongJob) {
                        JobId::new("spoofed:executor").unwrap()
                    } else {
                        input.job_id.clone()
                    },
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path.into()],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                })
                .unwrap()
            }
            AgentRole::Verifier => {
                if matches!(self.mode, Mode::VerifierDrift)
                    || matches!(self.mode, Mode::IntegrationDrift) && input.task_id.is_none()
                {
                    fs::write(process.workspace.join("drift.txt"), "external edit").unwrap();
                }
                let reject = matches!(self.mode, Mode::Reject);
                let target: VerificationTarget =
                    serde_json::from_value(input.artifact["target"].clone()).unwrap();
                let requirements = if input.task_id.is_some() {
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
                    decision: if reject {
                        VerificationDecision::Reject
                    } else {
                        VerificationDecision::Pass
                    },
                    findings: if reject {
                        vec![VerificationFinding {
                            severity: FindingSeverity::Error,
                            requirement_refs: requirements.clone(),
                            invariant_refs: vec![],
                            location: None,
                            problem: "fixture rejection".into(),
                        }]
                    } else {
                        vec![]
                    },
                    evidence: if matches!(self.mode, Mode::WrongEvidence) {
                        vec![EvidenceRef(EvidenceId::new("evidence:spoofed").unwrap())]
                    } else {
                        serde_json::from_value(input.artifact["evidence"].clone()).unwrap()
                    },
                    requirement_refs: requirements,
                    invariant_refs: vec![],
                    notes: None,
                })
                .unwrap()
            }
        };
        Ok(Box::new(Immediate(Some(success(
            serde_json::to_vec(&value).unwrap(),
        )))))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
    fn usage(&self, _: &ProcessOutput) -> local::Result<Usage> {
        Ok(match self.mode {
            Mode::Usage(provenance) if provenance != TokenUsageProvenance::Unknown => Usage {
                provenance,
                input: Some(17),
                output: Some(5),
                cached: Some(3),
            },
            _ => Usage::default(),
        })
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
struct Immediate(Option<ProcessOutput>);
impl RunningProcess for Immediate {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<()> {
        self.0 = Some(ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        });
        Ok(())
    }
}
struct Hang {
    cancelled: bool,
}
impl RunningProcess for Hang {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.cancelled.then(|| ProcessOutput {
            exit: None,
            stdout: vec![],
            stderr: vec![],
            failure: Some("cancelled".into()),
        }))
    }
    fn cancel(&mut self) -> local::Result<()> {
        self.cancelled = true;
        Ok(())
    }
}
struct Checks {
    fail: bool,
}
impl CheckLauncher for Checks {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        assert!(!spec.writable);
        assert!(!spec.network);
        assert!(spec.credential_env.is_empty());
        Ok(Box::new(Immediate(Some(ProcessOutput {
            exit: Some(if self.fail { 1 } else { 0 }),
            stdout: b"captured check output".to_vec(),
            stderr: vec![],
            failure: None,
        }))))
    }
}
fn seen() -> Arc<Mutex<Vec<JobInput>>> {
    Arc::new(Mutex::new(vec![]))
}

#[test]
fn full_diamond_runtime_captures_verifies_refreshes_and_completes() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    let result = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    assert_eq!(result.state, RunState::Complete);
    assert_eq!(result.accepted.len(), 4);
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 9);
    let sessions: std::collections::BTreeSet<_> = inputs.iter().map(|i| &i.session_id).collect();
    assert_eq!(sessions.len(), 9);
    for (index, input) in inputs.iter().enumerate() {
        assert_eq!(
            input.role,
            if index < 8 && index % 2 == 0 {
                AgentRole::Executor
            } else {
                AgentRole::Verifier
            }
        );
        assert_eq!(input.workspace_id, result.workspace_id);
    }
    assert!(
        inputs[2].artifact["files"]
            .to_string()
            .contains("accepted fixture change for task:0")
    );
    assert!(inputs[1].artifact.get("notes").is_none());
    assert!(
        inputs[1].artifact["diff"]["changes"]
            .to_string()
            .contains("accepted fixture change")
    );
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Complete
    );
    drop(inputs);
    let before = seen();
    assert_eq!(
        f.run(&p, Mode::Pass, before.clone(), false).unwrap().state,
        RunState::Complete
    );
    assert!(before.lock().unwrap().is_empty());
}
#[test]
fn verifier_rejection_stops_a_b_c_d_cascade() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    assert!(f.run(&p, Mode::Reject, inputs.clone(), false).is_err());
    assert_eq!(inputs.lock().unwrap().len(), 2);
    let tasks = f
        .store()
        .tasks(
            &RepositoryInfo::discover(&f.root).unwrap().repository_id,
            Some(&p.packet.plan_id),
        )
        .unwrap();
    assert_eq!(tasks[0].state, TaskState::Rejected);
    assert!(tasks[1..].iter().all(|t| t.state == TaskState::Planned));
}
#[test]
fn executor_failures_and_spoofing_never_launch_dependents() {
    for mode in [Mode::Malformed, Mode::Crash, Mode::WrongJob, Mode::Scope] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        assert!(f.run(&p, mode, inputs.clone(), false).is_err());
        assert_eq!(inputs.lock().unwrap().len(), 1);
        assert_eq!(
            f.store()
                .runtime_status(&f.root, &p.packet.plan_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Blocked
        );
    }
}
#[test]
fn forged_evidence_and_source_drift_cannot_verify() {
    for mode in [
        Mode::WrongEvidence,
        Mode::VerifierDrift,
        Mode::IntegrationDrift,
    ] {
        let f = Fixture::new();
        let p = f.plan();
        assert!(f.run(&p, mode, seen(), false).is_err());
        assert_eq!(
            f.store()
                .execution_plan(&f.root, &p.packet.plan_id)
                .unwrap()
                .state,
            PlanState::Active
        );
    }
}
#[test]
fn source_changes_after_activation_block_before_launch() {
    let f = Fixture::new();
    let p = f.plan();
    fs::write(f.root.join("src/api.rs"), "pub fn changed(){}\n").unwrap();
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
    let run = f
        .store()
        .runtime_status(&f.root, &p.packet.plan_id)
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Blocked);
    assert!(run.reason.unwrap().contains("SOURCE_DRIFT"));
}
#[test]
fn deterministic_check_failure_cannot_be_overridden_by_model() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), true).is_err());
    assert_eq!(inputs.lock().unwrap().len(), 1);
}
#[test]
fn provider_timeout_is_terminal_and_resume_does_not_retry() {
    let mut f = Fixture::new();
    f.config.timeout_ms = 10;
    let p = f.plan();
    assert!(f.run(&p, Mode::Timeout, seen(), false).is_err());
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
}
#[test]
fn planner_output_reuses_stage4_import_and_never_activates_partially() {
    for valid in [true, false] {
        let f = Fixture::new();
        let prepared = f.prepare();
        let p = artifact(&prepared);
        let mut store = f.store();
        let input = seen();
        let mode = if valid {
            Mode::Planner(Box::new(p.clone()))
        } else {
            Mode::Malformed
        };
        let mut runtime = Runtime::new(
            &mut store,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Fake {
                    mode,
                    seen: input.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap();
        let result = runtime.plan(&f.root, &prepared.request.request_id);
        assert_eq!(result.is_ok(), valid);
        if let Ok(v) = result {
            assert_eq!(v.state, PlanState::Validated);
        }
        assert_eq!(input.lock().unwrap()[0].role, AgentRole::Planner);
    }
}
#[test]
fn runtime_owned_plan_rejects_legacy_api_verifier_and_completion_spoofs() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    let info = RepositoryInfo::discover(&f.root).unwrap();
    let mut s = f.store();
    let job = AgentJob {
        version: ProtocolVersion::V1,
        job_id: JobId::new("spoof:job").unwrap(),
        agent_id: AgentId::new("spoof:agent").unwrap(),
        role: AgentRole::Verifier,
        plan_id: p.packet.plan_id.clone(),
        task_id: Some(p.packet.tasks[0].task_id.clone()),
        state: JobState::Queued,
        provider: None,
        created_at_ms: local::now_ms().unwrap(),
        started_at_ms: None,
        finished_at_ms: None,
    };
    assert!(
        s.register_job_in_workspace(&info.repository_id, &info.workspace_id, &job)
            .is_err()
    );
    let c = common::sql(&f.paths.database);
    assert!(
        c.execute("UPDATE tasks SET state_json='\"VERIFIED\"'", [])
            .is_err()
    );
    assert!(c.execute("UPDATE execution_plans SET state='COMPLETE',integration_json='{}',final_source_json='{}'",[]).is_err());
}
#[test]
fn sibling_workspace_cannot_run_or_cancel_another_plan() {
    let f = Fixture::new();
    let p = f.plan();
    let linked = f.temp.0.join("linked");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    f.store()
        .register_repository(RepositoryInfo::discover(&linked).unwrap())
        .unwrap();
    let mut s = f.store();
    let mut runtime =
        Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new()).unwrap();
    assert!(runtime.run(&linked, &p.packet.plan_id).is_err());
    assert!(
        f.store()
            .runtime_cancel(&linked, &p.packet.plan_id)
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
fn native_sandboxed_checks_complete_the_fake_provider_diamond() {
    let f = Fixture::new();
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy.commands.insert(
        "unit".into(),
        CommandSpec {
            program: "/usr/bin/grep".into(),
            args: vec![
                "-q".into(),
                "accepted fixture change".into(),
                "src/api.rs".into(),
            ],
            cwd: ".".into(),
        },
    );
    fs::write(
        f.root.join(".agentctl/project.toml"),
        toml::to_string(&policy).unwrap(),
    )
    .unwrap();
    git(&f.root, &["add", ".agentctl/project.toml"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "real source assertion"],
    );
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    let mut s = f.store();
    let mut runtime = Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Pass,
                seen: seen(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap();
    assert_eq!(
        runtime.run(&f.root, &p.packet.plan_id).unwrap().state,
        RunState::Complete
    );
}

struct CrashCheck {
    count: usize,
    at: usize,
}
impl CheckLauncher for CrashCheck {
    fn provenance(&self) -> &'static str {
        "CRASH_TEST_FIXTURE"
    }
    fn launch(&mut self, spec: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        self.count += 1;
        assert_ne!(self.count, self.at, "controller loss at durable checkpoint");
        Checks { fail: false }.launch(spec)
    }
}

#[test]
fn reopen_resumes_pending_verification_and_integration_without_reexecuting_verified_tasks() {
    for at in [1, 2, 5] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = f.store();
            Runtime::new(
                &mut s,
                f.paths.clone(),
                f.config.clone(),
                BTreeMap::from([(
                    "test".into(),
                    Box::new(Fake {
                        mode: Mode::Pass,
                        seen: inputs.clone(),
                    }) as Box<dyn ProviderAdapter>,
                )]),
            )
            .unwrap()
            .with_check_launcher(Box::new(CrashCheck { count: 0, at }))
            .run(&f.root, &p.packet.plan_id)
            .unwrap();
        }));
        assert!(crash.is_err());
        assert_eq!(
            f.store()
                .runtime_status(&f.root, &p.packet.plan_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Running
        );
        assert_eq!(
            f.run(&p, Mode::Pass, inputs.clone(), false).unwrap().state,
            RunState::Complete
        );
        let inputs = inputs.lock().unwrap();
        assert_eq!(inputs.len(), 9);
        for task in &p.packet.tasks {
            assert_eq!(
                inputs
                    .iter()
                    .filter(|i| i.role == AgentRole::Executor
                        && i.task_id.as_ref() == Some(&task.task_id))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn interrupted_provider_is_persisted_and_never_relaunched_automatically() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.run(
            &p,
            Mode::Panic,
            seen(),
            false
        )))
        .is_err()
    );
    let inputs = seen();
    assert!(f.run(&p, Mode::Pass, inputs.clone(), false).is_err());
    assert!(inputs.lock().unwrap().is_empty());
    let jobs = f
        .store()
        .runtime_jobs(&f.root, Some(&p.packet.plan_id))
        .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, RuntimeJobState::Interrupted);
    assert_eq!(
        f.store()
            .job(
                &RepositoryInfo::discover(&f.root).unwrap().repository_id,
                &jobs[0].job_id
            )
            .unwrap()
            .unwrap()
            .state,
        JobState::Failed
    );
}

#[test]
fn token_observations_preserve_exact_estimated_and_unknown_without_fabrication() {
    use local::store::JournalEntry;
    for provenance in [
        TokenUsageProvenance::Exact,
        TokenUsageProvenance::Estimated,
        TokenUsageProvenance::Unknown,
    ] {
        let f = Fixture::new();
        let p = f.plan();
        f.run(&p, Mode::Usage(provenance), seen(), false).unwrap();
        let events = f.store().events(None, None, None, 1000).unwrap();
        let usages: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.entry {
                JournalEntry::Agent { event } => match &event.event {
                    AgentEventKind::TokenUsageObserved { usage } => Some(usage),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 9);
        for u in usages {
            assert_eq!(u.provenance, provenance);
            assert_eq!(
                u.input_tokens,
                (provenance != TokenUsageProvenance::Unknown).then_some(17)
            );
            assert!(u.total_tokens.is_none());
        }
    }
}

#[test]
fn captured_artifacts_and_command_metadata_are_hash_bound_and_tamper_evident() {
    let f = Fixture::new();
    let p = f.plan();
    let inputs = seen();
    let run = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    let artifacts = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
    let before: SourceSnapshot = artifacts.decode(&run.baseline).unwrap();
    let after: SourceSnapshot = artifacts.decode(&run.expected).unwrap();
    assert_eq!(before.head, after.head);
    assert_ne!(before.files, after.files);
    for accepted in run.accepted.values() {
        let diff: CapturedDiff = artifacts.decode(&accepted.diff).unwrap();
        assert_eq!(diff.workspace_id, run.workspace_id);
        assert_eq!(diff.plan_id, p.packet.plan_id);
        assert_eq!(diff.executor_job_id.as_ref(), Some(&accepted.executor));
        assert_eq!(diff.changes.len(), 1);
        assert!(diff.scope_violations.is_empty());
    }
    let inputs = inputs.lock().unwrap();
    let evidence = &inputs[1].artifact["evidence_records"];
    assert_eq!(evidence[1]["exit_status"], 0);
    assert_eq!(evidence[1]["command"]["program"], "/usr/bin/true");
    assert!(
        evidence[1]["stdout_hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );
    let path = artifacts.path(&run.baseline).unwrap();
    fs::write(path, b"tampered").unwrap();
    assert!(artifacts.get(&run.baseline).is_err());
}

#[test]
fn v6_runtime_migration_is_additive_atomic_and_missing_guards_fail_closed() {
    let f = Fixture::new();
    let p = f.plan();
    let c = common::sql(&f.paths.database);
    let old: String = c
        .query_row("SELECT metadata_json FROM execution_plans", [], |r| {
            r.get(0)
        })
        .unwrap();
    common::strip_runtime(&c);
    c.pragma_update(None, "user_version", 6).unwrap();
    c.execute_batch("CREATE TABLE runtime_jobs(block_upgrade TEXT)")
        .unwrap();
    drop(c);
    assert!(Store::open(&f.paths.database, 5000).is_err());
    let c = common::sql(&f.paths.database);
    assert_eq!(
        c.query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))
            .unwrap(),
        6
    );
    assert_eq!(
        c.query_row::<u32, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='runtime_runs'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    c.execute_batch("DROP TABLE runtime_jobs").unwrap();
    drop(c);
    assert_eq!(f.store().status().unwrap().schema_version, 7);
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Active
    );
    let c = common::sql(&f.paths.database);
    assert_eq!(
        c.query_row::<String, _, _>("SELECT metadata_json FROM execution_plans", [], |r| r
            .get(0))
            .unwrap(),
        old
    );
    c.execute_batch("DROP TRIGGER runtime_task_gate").unwrap();
    drop(c);
    assert!(Store::open(&f.paths.database, 5000).is_err());
}

fn cli(f: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(&f.root)
        .args(args)
        .env("HOME", f.temp.0.join("home"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .output()
        .unwrap()
}
#[test]
fn cli_dry_run_and_provider_inspection_are_isolated_and_nonexecuting() {
    let f = Fixture::new();
    let p = f.plan();
    for args in [
        vec!["provider", "list", "--json"],
        vec!["provider", "doctor", "--json"],
        vec![
            "run",
            "plan",
            p.packet.plan_id.as_str(),
            "--dry-run",
            "--json",
        ],
    ] {
        let output = cli(&f, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
    }
    assert!(f.store().runtime_jobs(&f.root, None).unwrap().is_empty());
    assert!(!f.paths.data_root.join("runtime").exists());
    assert!(!RepositoryInfo::discover(&f.root).unwrap().source.dirty);
}

struct Controlled {
    paths: MachinePaths,
    config: RuntimeConfig,
    cancel: bool,
    seen: Arc<Mutex<Vec<JobInput>>>,
}
impl ProviderAdapter for Controlled {
    fn capabilities(&self) -> Capabilities {
        Fake {
            mode: Mode::Pass,
            seen: self.seen.clone(),
        }
        .capabilities()
    }
    fn launch(
        &mut self,
        input: &JobInput,
        spec: ProcessSpec,
        role: &RoleConfig,
    ) -> local::Result<Box<dyn RunningProcess>> {
        let mut store = Store::open(&self.paths.database, 5000)?;
        let err = Runtime::new(
            &mut store,
            self.paths.clone(),
            self.config.clone(),
            BTreeMap::new(),
        )?
        .run(&spec.workspace, input.plan_id.as_ref().unwrap())
        .unwrap_err();
        assert!(err.to_string().contains("lease"), "{err}");
        if self.cancel {
            store.runtime_cancel(&spec.workspace, input.plan_id.as_ref().unwrap())?;
            self.seen.lock().unwrap().push(input.clone());
            return Ok(Box::new(Hang { cancelled: false }));
        }
        Fake {
            mode: Mode::Pass,
            seen: self.seen.clone(),
        }
        .launch(input, spec, role)
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[test]
fn workspace_lease_prevents_concurrent_launch_and_cancellation_stops_dependents() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let p = f.plan();
        let inputs = seen();
        let mut s = f.store();
        let result = Runtime::new(
            &mut s,
            f.paths.clone(),
            f.config.clone(),
            BTreeMap::from([(
                "test".into(),
                Box::new(Controlled {
                    paths: f.paths.clone(),
                    config: f.config.clone(),
                    cancel,
                    seen: inputs.clone(),
                }) as Box<dyn ProviderAdapter>,
            )]),
        )
        .unwrap()
        .with_check_launcher(Box::new(Checks { fail: false }))
        .run(&f.root, &p.packet.plan_id);
        assert_eq!(result.is_err(), cancel);
        assert_eq!(inputs.lock().unwrap().len(), if cancel { 1 } else { 9 });
        if cancel {
            assert_eq!(
                s.runtime_status(&f.root, &p.packet.plan_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::Cancelled
            );
            assert_eq!(
                s.runtime_jobs(&f.root, Some(&p.packet.plan_id)).unwrap()[0].state,
                RuntimeJobState::Cancelled
            );
        }
    }
}

#[test]
fn ignored_files_are_not_an_escape_from_actual_diff_scope_checks() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "outside.txt\n").unwrap();
    git(&f.root, &["add", ".gitignore"]);
    git(&f.root, &["commit", "--quiet", "-m", "ignore fixture"]);
    f.store().index_repository(&f.root).unwrap();
    let p = f.plan();
    assert!(f.run(&p, Mode::Scope, seen(), false).is_err());
    assert!(!RepositoryInfo::discover(&f.root).unwrap().source.dirty);
    let events = f.store().events(None, None, None, 1000).unwrap();
    let hash = events
        .iter()
        .find_map(|e| match &e.entry {
            local::store::JournalEntry::Runtime { phase, detail, .. }
                if phase == "DIFF_CAPTURED" =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .unwrap();
    let bytes = fs::read(
        f.paths
            .data_root
            .join("runtime/blobs")
            .join(hash.strip_prefix("blake3:").unwrap()),
    )
    .unwrap();
    let diff: CapturedDiff = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(diff.scope_violations, vec!["outside.txt"]);
    assert_eq!(diff.changes[0].path, "outside.txt");
    assert!(diff.changes[0].before.is_none());
}

#[test]
fn full_fake_planner_to_integration_flow_and_hash_helper_use_canonical_contracts() {
    use std::io::Write;
    let f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut s = f.store();
    let result = Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p.clone())),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    assert_eq!(result.state, PlanState::Validated);
    s.activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(
        f.run(&p, Mode::Pass, inputs.clone(), false).unwrap().state,
        RunState::Complete
    );
    assert_eq!(inputs.lock().unwrap().len(), 10);
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .args(["run", "packet-hashes"])
        .env("HOME", f.temp.0.join("nonexistent-home"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec_pretty(&p.packet).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let hashes: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(hashes["plan_packet_hash"], hash(&p.packet).unwrap());
    assert!(!f.temp.0.join("nonexistent-home").exists());
}

#[test]
fn correction_requires_explicit_replacement_and_stops_at_configured_bound() {
    let mut f = Fixture::new();
    f.config.max_correction_rounds = 1;
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    // A human chooses to preserve the rejected edit as the next committed
    // baseline. agentctl neither resets nor commits the user's checkout.
    git(&f.root, &["add", "src/api.rs"]);
    git(
        &f.root,
        &["commit", "--quiet", "-m", "human replan baseline"],
    );
    f.store().index_repository(&f.root).unwrap();
    let mut replacement = artifact(&f.prepare());
    replacement.packet.plan_id = PlanId::new("plan:correction").unwrap();
    for task in &mut replacement.packet.tasks {
        task.task_id = TaskId::new(format!("{}:r", task.task_id.as_str())).unwrap();
        for dep in &mut task.dependencies {
            *dep = TaskId::new(format!("{}:r", dep.as_str())).unwrap();
        }
    }
    for (contract, task) in replacement
        .metadata
        .contracts
        .iter_mut()
        .zip(&replacement.packet.tasks)
    {
        contract.task_id = task.task_id.clone();
        contract.task_packet_hash = hash(task).unwrap();
    }
    replacement.metadata.integration.plan_id = replacement.packet.plan_id.clone();
    replacement.metadata.integration.plan_packet_hash = hash(&replacement.packet).unwrap();
    replacement.metadata.replan = Some(ReplanReference {
        previous_plan_id: p.packet.plan_id.clone(),
        reason: "explicit corrective decomposition".into(),
        previously_verified_tasks: vec![],
        replaced_tasks: p.packet.tasks.iter().map(|t| t.task_id.clone()).collect(),
    });
    let mut s = f.store();
    s.import_execution_plan(&f.root, &replacement).unwrap();
    assert!(
        s.supersede_execution_plan(&f.root, &p.packet.plan_id, &replacement.packet.plan_id)
            .is_err()
    );
    Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new())
        .unwrap()
        .replace(&f.root, &p.packet.plan_id, &replacement.packet.plan_id)
        .unwrap();
    s.activate_execution_plan(&f.root, &replacement.packet.plan_id)
        .unwrap();
    assert!(f.run(&replacement, Mode::Reject, seen(), false).is_err());
    assert_eq!(
        s.runtime_status(&f.root, &replacement.packet.plan_id)
            .unwrap()
            .unwrap()
            .correction_round,
        1
    );
    let error = Runtime::new(&mut s, f.paths.clone(), f.config.clone(), BTreeMap::new())
        .unwrap()
        .replace(
            &f.root,
            &replacement.packet.plan_id,
            &PlanId::new("plan:another").unwrap(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("correction limit"));
}

#[test]
fn shared_artifact_publication_is_atomic_across_concurrent_workspace_controllers() {
    let temp = common::TempDir::new();
    let root = temp.0.join("blobs");
    let artifacts = Arc::new(Artifacts::new(&root).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let a = artifacts.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                let bytes = vec![42; 1024 * 1024];
                let r = a.put(&bytes).unwrap();
                assert_eq!(a.get(&r).unwrap(), bytes);
                r
            })
        })
        .collect();
    let refs: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(refs.iter().all(|r| r == &refs[0]));
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn planner_default_topology_is_session_native_with_fresh_instances_and_parentage() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let p = artifact(&prepared);
    let inputs = seen();
    let mut s = f.store();
    Runtime::new(
        &mut s,
        f.paths.clone(),
        f.config.clone(),
        BTreeMap::from([(
            "test".into(),
            Box::new(Fake {
                mode: Mode::Planner(Box::new(p.clone())),
                seen: inputs.clone(),
            }) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .plan(&f.root, &prepared.request.request_id)
    .unwrap();
    s.activate_execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    let run = f.run(&p, Mode::Pass, inputs.clone(), false).unwrap();
    let session = run.engineering_session.unwrap();
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 10);
    let planner = &inputs[0];
    let instances: std::collections::BTreeSet<_> = inputs
        .iter()
        .map(|i| &i.ownership.agent_instance_id)
        .collect();
    assert_eq!(instances.len(), 10);
    for input in inputs.iter() {
        assert_eq!(input.ownership.engineering_session_id, session.id);
        assert_eq!(input.ownership.lifetime, AgentLifetime::SessionNative);
    }
    for pair in inputs[1..9].chunks_exact(2) {
        assert_eq!(pair[0].role, AgentRole::Executor);
        assert_eq!(pair[1].role, AgentRole::Verifier);
        assert_eq!(
            pair[0].ownership.parent_agent_instance_id.as_ref(),
            Some(&planner.ownership.agent_instance_id)
        );
        assert_eq!(
            pair[1].ownership.parent_agent_instance_id.as_ref(),
            Some(&pair[0].ownership.agent_instance_id)
        );
        assert_ne!(pair[0].session_id, pair[1].session_id);
        assert_ne!(pair[0].job_id, pair[1].job_id);
    }
    let integration = &inputs[9];
    assert_eq!(integration.role, AgentRole::Verifier);
    assert!(integration.task_id.is_none());
    assert_eq!(
        integration.ownership.parent_agent_instance_id.as_ref(),
        Some(&planner.ownership.agent_instance_id)
    );
    let helper = inputs[1]
        .ownership
        .child(AgentId::new("agent:reserved-helper").unwrap());
    let grandchild = helper.child(AgentId::new("agent:reserved-grandchild").unwrap());
    assert_eq!(grandchild.engineering_session_id, session.id);
    assert_eq!(grandchild.lifetime, AgentLifetime::SessionNative);
    assert_eq!(s.runtime_jobs(&f.root, None).unwrap().len(), 10); // metadata construction never launches a helper
    assert_eq!(
        fs::read_dir(f.paths.data_root.join("runtime/scratch"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn same_provider_policy_never_reuses_another_engineering_sessions_workers() {
    let a = Fixture::new();
    let b = Fixture::new();
    assert_eq!(a.config, b.config);
    let pa = a.plan();
    let pb = b.plan();
    let ia = seen();
    let ib = seen();
    let ra = a.run(&pa, Mode::Pass, ia.clone(), false).unwrap();
    let rb = b.run(&pb, Mode::Pass, ib.clone(), false).unwrap();
    assert_ne!(ra.engineering_session, rb.engineering_session);
    let ia = ia.lock().unwrap();
    let ib = ib.lock().unwrap();
    for x in ia.iter() {
        for y in ib.iter() {
            assert_ne!(x.session_id, y.session_id);
            assert_ne!(x.ownership.agent_instance_id, y.ownership.agent_instance_id);
        }
    }
    // A packet produced in B cannot be applied to A through normal interfaces.
    let b_job = b
        .store()
        .runtime_jobs(&b.root, Some(&pb.packet.plan_id))
        .unwrap()
        .into_iter()
        .find(|j| j.role == AgentRole::Verifier)
        .unwrap();
    let proof: VerificationPacket = Artifacts::new(&b.paths.data_root.join("runtime/blobs"))
        .unwrap()
        .decode(&b_job.output.unwrap())
        .unwrap();
    assert!(
        a.store()
            .complete_execution_plan(&a.root, &pa.packet.plan_id, &proof, &ia[0].source)
            .is_err()
    );
}

#[test]
fn old_v7_runtime_metadata_remains_inspectable_without_silent_ownership_backfill() {
    let f = Fixture::new();
    let p = f.plan();
    assert!(f.run(&p, Mode::Reject, seen(), false).is_err());
    let c = common::sql(&f.paths.database);
    // Simulate an already-applied unaccepted v7 database, without changing its
    // schema or inventing historical ownership. This is privileged fixture setup.
    c.create_scalar_function(
        "agentctl_runtime_authorized",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(true),
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_runs SET record_json=json_remove(record_json,'$.engineering_session')",
        [],
    )
    .unwrap();
    c.execute(
        "UPDATE runtime_jobs SET record_json=json_remove(record_json,'$.ownership','$.task_id')",
        [],
    )
    .unwrap();
    let before: String = c
        .query_row("SELECT record_json FROM runtime_runs", [], |r| r.get(0))
        .unwrap();
    drop(c);
    assert!(
        f.store()
            .runtime_status(&f.root, &p.packet.plan_id)
            .unwrap()
            .unwrap()
            .engineering_session
            .is_none()
    );
    let error = f.run(&p, Mode::Pass, seen(), false).unwrap_err();
    assert!(error.to_string().contains("ownership"));
    let c = common::sql(&f.paths.database);
    assert_eq!(
        before,
        c.query_row::<String, _, _>("SELECT record_json FROM runtime_runs", [], |r| r.get(0))
            .unwrap()
    );
    assert_eq!(f.store().status().unwrap().schema_version, 7);
}

#[test]
fn native_auth_preflight_prefers_provider_login_without_importing_api_environment() {
    use local::runtime::credentials::*;
    use std::os::unix::fs::PermissionsExt;
    let temp = common::TempDir::new();
    let executable = temp.0.join("provider");
    fs::write(&executable,"#!/bin/sh\n[ -z \"${ANTHROPIC_API_KEY-}\" ] || exit 8\n[ -z \"${CODEX_API_KEY-}\" ] || exit 9\nprintf '%s' '{\"loggedIn\":true}'\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    for provider in ["codex", "claude"] {
        let native = NativeAuth {
            provider: provider.into(),
            home: temp.0.clone(),
            provider_home: temp.0.join("auth"),
            config_override: true,
        };
        for mode in [AuthMode::Auto, AuthMode::Native] {
            let auth = Authentication {
                mode,
                api_key_env: Some("PATH".into()),
            };
            let status = auth.preflight(&executable, &native).unwrap();
            assert!(status.authenticated);
            assert_eq!(status.method, "NATIVE");
            assert!(!status.api_key_required);
        }
    }
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1); // no auth files copied/created
}

#[test]
fn optional_api_key_auth_is_explicit_and_missing_auth_is_actionable() {
    use local::runtime::credentials::*;
    let temp = common::TempDir::new();
    for provider in ["codex", "claude"] {
        let native = NativeAuth {
            provider: provider.into(),
            home: temp.0.clone(),
            provider_home: temp.0.join("auth"),
            config_override: false,
        };
        let unavailable = Authentication::default()
            .preflight(Path::new("/usr/bin/false"), &native)
            .unwrap();
        assert!(!unavailable.authenticated);
        assert!(unavailable.guidance.contains("login"));
        for mode in [AuthMode::Auto, AuthMode::ApiKey] {
            // PATH is a harmless fixture value, not a live provider key. No model is called.
            let auth = Authentication {
                mode,
                api_key_env: Some("PATH".into()),
            };
            let status = auth
                .preflight(Path::new("/usr/bin/false"), &native)
                .unwrap();
            assert_eq!(status.method, "API_KEY");
            assert!(status.authenticated);
        }
        let forced = Authentication {
            mode: AuthMode::Native,
            api_key_env: Some("PATH".into()),
        };
        assert!(
            !forced
                .preflight(Path::new("/usr/bin/false"), &native)
                .unwrap()
                .authenticated
        );
        assert!(
            Authentication {
                mode: AuthMode::ApiKey,
                api_key_env: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Authentication {
                mode: AuthMode::ApiKey,
                api_key_env: Some("sk-secret-value".into())
            }
            .validate()
            .is_err()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires macOS sandbox capability outside a nested host sandbox"]
fn native_auth_same_login_fresh_sessions_and_optional_key_cli_dag_never_persist_credentials() {
    use local::runtime::credentials::*;
    use std::os::unix::fs::PermissionsExt;
    const CANARY: &str = "sk-ant-agentctl-fixture-secret-never-persist-123456789";
    for provider in ["codex", "claude"] {
        for key_mode in [false, true] {
            let mut f = Fixture::new();
            let home = f.temp.0.join("home");
            let native = home.join(if provider == "codex" {
                ".codex"
            } else {
                ".claude"
            });
            fs::create_dir_all(&native).unwrap();
            let credential = native.join(if provider == "codex" {
                "auth.json"
            } else {
                ".credentials.json"
            });
            fs::write(&credential, CANARY).unwrap();
            fs::write(
                native.join("history.jsonl"),
                "UNRELATED_CONVERSATION_CANARY",
            )
            .unwrap();
            let executable = f.temp.0.join("mock-provider");
            let script = r#"#!/usr/bin/python3
import sys,os,json,pathlib
if '--version' in sys.argv: print('fixture provider');sys.exit(0)
if 'status' in sys.argv:
    assert not os.environ.get('CODEX_API_KEY') and not os.environ.get('ANTHROPIC_API_KEY')
    print(json.dumps({'loggedIn':True}));sys.exit(0)
i=json.loads(sys.stdin.read().split('\n',1)[1]); a=i['artifact']
is_claude='--safe-mode' in sys.argv
assert '--bare' not in sys.argv and 'resume' not in sys.argv and '--continue' not in sys.argv
assert ('--no-session-persistence' if is_claude else '--ephemeral') in sys.argv
key=os.environ.get('ANTHROPIC_API_KEY' if is_claude else 'CODEX_API_KEY')
if key is not None: assert key=='sk-ant-agentctl-fixture-secret-never-persist-123456789'
native=pathlib.Path(os.environ.get('CLAUDE_CONFIG_DIR',str(pathlib.Path.home()/'.claude'))) if is_claude else pathlib.Path(os.environ['CODEX_HOME'])
if key is None: assert (native/('.credentials.json' if is_claude else 'auth.json')).read_text()=='sk-ant-agentctl-fixture-secret-never-persist-123456789'
try:
    (native/'history.jsonl').read_text()
    raise AssertionError('unrelated provider history readable')
except (PermissionError,FileNotFoundError): pass
sys.stderr.write('sk-ant-agentctl-fixture-secret-never-persist-123456789')
if i['role']=='EXECUTOR':
    p=a['task']['write_scope'][0]['path'];pathlib.Path(p).write_text(pathlib.Path(p).read_text()+'// native fixture edit\n')
    value=dict(version='1',task_id=i['task_id'],executor_job_id=i['job_id'],status='SUCCEEDED',changed_paths=[p],changed_entities=[],evidence=[],notes=None,failure=None)
else:
    value=dict(version='1',verification_id='verification:'+i['job_id'],target=a['target'],verifier_job_id=i['job_id'],decision='PASS',findings=[],evidence=a['evidence'],requirement_refs=['unit' if i['task_id'] else 'integration'],invariant_refs=[],notes=None)
print(json.dumps({'result':json.dumps(value)}) if is_claude else json.dumps(value))
"#;
            fs::write(&executable, script).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            f.config.providers.insert(
                "test".into(),
                ProviderConfig {
                    adapter: provider.into(),
                    executable,
                    authentication: Authentication {
                        mode: if key_mode {
                            AuthMode::ApiKey
                        } else {
                            AuthMode::Auto
                        },
                        api_key_env: key_mode.then(|| "AGENTCTL_FIXTURE_KEY".into()),
                    },
                },
            );
            fs::write(
                &f.paths.machine_config,
                toml::to_string(&MachineConfig {
                    runtime: f.config.clone(),
                    ..Default::default()
                })
                .unwrap(),
            )
            .unwrap();
            let p = f.plan();
            let invoke = |args: &[&str]| {
                Command::new(env!("CARGO_BIN_EXE_agentctl"))
                    .current_dir(&f.root)
                    .args(args)
                    .env("HOME", &home)
                    .env("CODEX_HOME", home.join(".codex"))
                    .env_remove("CLAUDE_CONFIG_DIR")
                    .env_remove("XDG_CONFIG_HOME")
                    .env_remove("XDG_DATA_HOME")
                    .env_remove("XDG_CACHE_HOME")
                    .env_remove("ANTHROPIC_API_KEY")
                    .env_remove("CODEX_API_KEY")
                    .env_remove("OPENAI_API_KEY")
                    .env("AGENTCTL_FIXTURE_KEY", CANARY)
                    .output()
                    .unwrap()
            };
            let doctor = invoke(&["provider", "doctor", "--json"]);
            assert!(
                doctor.status.success(),
                "{}",
                String::from_utf8_lossy(&doctor.stderr)
            );
            let status: Value = serde_json::from_slice(&doctor.stdout).unwrap();
            assert_eq!(status[0]["authentication"]["authenticated"], true);
            assert_eq!(
                status[0]["authentication"]["method"],
                if key_mode { "API_KEY" } else { "NATIVE" }
            );
            let output = invoke(&["run", "plan", p.packet.plan_id.as_str(), "--json"]);
            if !output.status.success() {
                let a = Artifacts::new(&f.paths.data_root.join("runtime/blobs")).unwrap();
                let errors: Vec<_> = f
                    .store()
                    .runtime_jobs(&f.root, Some(&p.packet.plan_id))
                    .unwrap()
                    .iter()
                    .filter_map(|j| j.stderr.as_ref())
                    .map(|r| String::from_utf8_lossy(&a.get(r).unwrap()).to_string())
                    .collect();
                panic!("fixture provider={provider} api_key={key_mode}: {errors:?}");
            }
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let jobs = f
                .store()
                .runtime_jobs(&f.root, Some(&p.packet.plan_id))
                .unwrap();
            assert_eq!(jobs.len(), 9);
            assert_eq!(
                jobs.iter()
                    .map(|j| &j.session_id)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                9
            );
            let mut pending = vec![f.paths.data_root.clone(), f.paths.config_root.clone()];
            while let Some(path) = pending.pop() {
                for e in fs::read_dir(path).unwrap() {
                    let e = e.unwrap();
                    if e.file_type().unwrap().is_dir() {
                        pending.push(e.path());
                    } else {
                        let bytes = fs::read(e.path()).unwrap();
                        assert!(
                            !String::from_utf8_lossy(&bytes).contains(CANARY),
                            "credential leaked into {}",
                            e.path().display()
                        );
                        assert!(
                            !String::from_utf8_lossy(&bytes)
                                .contains("UNRELATED_CONVERSATION_CANARY")
                        );
                    }
                }
            }
            assert_eq!(fs::read_to_string(credential).unwrap(), CANARY);
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "checks installed native login status and requires Keychain access; no model call"]
fn installed_native_auth_preflight_without_api_keys() {
    use local::runtime::credentials::*;
    for provider in ["codex", "claude"] {
        let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(provider))
            .find(|p| p.is_file())
            .expect("install provider CLI for this opt-in native login test");
        let status = Authentication {
            mode: AuthMode::Native,
            api_key_env: None,
        }
        .preflight(&path, &NativeAuth::discover(provider).unwrap())
        .unwrap();
        assert!(status.authenticated, "{provider}: {}", status.guidance);
        assert!(!status.api_key_required);
    }
}
