#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        config::{Declaration, ProjectConfig, VerificationDefinition},
        graph::SearchMode,
        memory::{MemoryDraft, MemoryKind, MemoryLink},
        now_ms,
        planning::*,
        repository::{RepositoryInfo, WorkspaceId},
        store::Store,
    },
    protocol::*,
};
use common::{TempDir, decode};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const CODE: &str = "pub fn invalidate_cache(changed: bool) -> bool { changed }\npub fn cache_status() -> bool { self::invalidate_cache(false) }\n#[test] fn test_cache_invalidation() { assert!(self::invalidate_cache(true)); }\n";
struct Fixture {
    temp: TempDir,
    root: PathBuf,
    db: PathBuf,
    info: RepositoryInfo,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        write(&root, "src/cache.rs", CODE);
        let mut policy = ProjectConfig::default();
        policy.invariants.insert(
            "cache:correctness".into(),
            Declaration {
                description: "Never reuse stale cache facts".into(),
            },
        );
        for id in ["unit", "integration"] {
            policy.commands.insert(
                id.into(),
                CommandSpec {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                    cwd: ".".into(),
                },
            );
            policy.verification.insert(
                id.into(),
                VerificationDefinition {
                    description: format!("Required {id} tests"),
                    command_refs: vec![id.into()],
                },
            );
        }
        write(
            &root,
            ".agentctl/project.toml",
            &toml::to_string(&policy).unwrap(),
        );
        let db = temp.0.join("state.sqlite3");
        let info = RepositoryInfo::discover(&root).unwrap();
        let mut s = Store::open(&db, 5000).unwrap();
        s.register_repository(info.clone()).unwrap();
        s.index_repository(&root).unwrap();
        Self {
            temp,
            root,
            db,
            info,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }
    fn sql(&self) -> Connection {
        common::sql(&self.db)
    }
    fn prepare(&self) -> PlannerPacket {
        self.store()
            .prepare_plan(&self.root, intent(), PlanningLimits::default())
            .unwrap()
    }
    fn import(&self, p: &ExecutionPlan) {
        self.store().import_execution_plan(&self.root, p).unwrap();
    }
    fn activate(&self, p: &ExecutionPlan) {
        self.store()
            .activate_execution_plan(&self.root, &p.packet.plan_id)
            .unwrap();
    }
    fn tasks(&self, p: &ExecutionPlan) -> Vec<TaskInspection> {
        self.store()
            .execution_tasks(&self.root, &p.packet.plan_id)
            .unwrap()
    }
    fn symbol(&self) -> GraphEntityId {
        self.store()
            .graph(&self.root)
            .unwrap()
            .symbols("invalidate_cache", SearchMode::Exact, 1)
            .unwrap()
            .data[0]
            .id
            .clone()
    }
    fn linked(&self) -> PathBuf {
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "--quiet", "-m", "fixture"]);
        let root = self.temp.0.join("linked");
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                root.to_str().unwrap(),
            ],
        );
        self.store()
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        self.store().index_repository(&root).unwrap();
        root
    }
    fn memory(&self, trust: MemoryTrustClass) -> agentctl::local::memory::MemoryEntry {
        let job = if trust == MemoryTrustClass::AgentNote {
            let mut s = self.store();
            s.create_plan(&self.info.repository_id, &common::plan(), 1)
                .unwrap();
            let mut j = common::samples()["agent-job"].clone();
            j["state"] = json!("QUEUED");
            j["started_at_ms"] = Value::Null;
            s.register_job(&self.info.repository_id, &decode(j))
                .unwrap();
            Some(JobId::new("job:executor-a").unwrap())
        } else {
            None
        };
        self.store()
            .add_memory(
                &self.root,
                MemoryDraft {
                    kind: MemoryKind::Finding,
                    content: format!("Cache invalidation safety {trust:?}"),
                    workspace_id: None,
                    canonical_key: None,
                    actor: "test".into(),
                    author_job_id: job,
                    links: vec![MemoryLink::Graph { id: self.symbol() }],
                },
                trust,
                None,
            )
            .unwrap()
    }
}
fn write(root: &Path, path: &str, text: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args([
            "-c",
            "user.name=Plan Test",
            "-c",
            "user.email=plan@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.templateDir=",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
fn intent() -> RequestDraft {
    RequestDraft {
        objective: "Add deterministic cache invalidation to repository indexing".into(),
        query: Some("cache invalidation".into()),
        scope: vec![ScopePath::Directory { path: "src".into() }],
        constraints: vec!["No network or provider runtime".into()],
        definition_of_done: vec!["Incremental cache behavior has regression coverage".into()],
        verification: Some(requirements("integration")),
        invariant_refs: vec![],
        provenance: PlanningProvenance {
            actor: "human-test".into(),
            source_refs: vec!["user-objective".into()],
            provider: None,
        },
    }
}
fn requirements(id: &str) -> VerificationRequirements {
    VerificationRequirements {
        requirement_refs: vec![id.into()],
        evidence_required: true,
    }
}
fn artifact(prepared: &PlannerPacket, prefix: &str, independent: bool) -> ExecutionPlan {
    let names = [
        "Persist invalidation metadata",
        "Implement cache invalidation decision logic",
        "Expose cache status through CLI",
        "Add combined cache regression tests",
    ];
    let paths = [
        "src/metadata.rs",
        "src/cache.rs",
        "src/cli.rs",
        "src/regression.rs",
    ];
    let tasks = (0..4)
        .map(|i| TaskPacket {
            version: ProtocolVersion::V1,
            task_id: TaskId::new(format!("{prefix}:{i}")).unwrap(),
            objective: names[i].into(),
            read_scope: vec![ScopePath::File {
                path: "src/cache.rs".into(),
            }],
            write_scope: vec![ScopePath::File {
                path: paths[i].into(),
            }],
            graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
            invariant_refs: prepared.request.intent.invariant_refs.clone(),
            dependencies: if independent || i == 0 {
                vec![]
            } else {
                vec![TaskId::new(format!("{prefix}:{}", if i == 1 { 0 } else { 1 })).unwrap()]
            },
            definition_of_done: vec![format!("{} is implemented and tested", names[i])],
            verification: requirements("unit"),
        })
        .collect();
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new(prefix).unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks,
        integration_verification: requirements("integration"),
    };
    let contracts = packet
        .tasks
        .iter()
        .map(|t| VerificationContract {
            task_id: t.task_id.clone(),
            task_packet_hash: hash(t).unwrap(),
            independent_verifier: true,
            input: VerifierInput::PacketDiffAndEvidence,
            memory_refs: vec![],
            exclusions: vec![ScopePath::Directory {
                path: "secrets".into(),
            }],
            non_goals: vec!["Do not add execution runtime".into()],
        })
        .collect();
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "external-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: Some(ProviderMetadata {
                    provider: "opaque-producer".into(),
                    model: None,
                }),
            },
            contracts,
            integration: IntegrationVerificationContract {
                plan_id: packet.plan_id.clone(),
                plan_packet_hash: hash(&packet).unwrap(),
                independent_verifier: true,
                require_all_task_verifications: true,
                require_final_diff_and_evidence: true,
                expectations: if prepared.request.intent.definition_of_done.is_empty() {
                    vec!["Overall objective passes regression checks".into()]
                } else {
                    prepared.request.intent.definition_of_done.clone()
                },
            },
            replan: None,
        },
        packet,
    }
}
fn rehash(p: &mut ExecutionPlan) {
    for c in &mut p.metadata.contracts {
        if let Some(t) = p.packet.tasks.iter().find(|t| t.task_id == c.task_id) {
            c.task_packet_hash = hash(t).unwrap();
        }
    }
    p.metadata.integration.plan_packet_hash = hash(&p.packet).unwrap();
}
fn count(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn transition(f: &Fixture, id: &TaskId, from: TaskState, to: TaskState) {
    f.store()
        .transition_task(&f.info.repository_id, id, from, to, None, 120)
        .unwrap();
}
fn finished_job(
    f: &Fixture,
    p: &ExecutionPlan,
    task: Option<&TaskId>,
    role: AgentRole,
    id: &JobId,
) {
    finished_job_at(f, p, task, role, id, Some(&f.info.workspace_id));
}
fn finished_job_at(
    f: &Fixture,
    p: &ExecutionPlan,
    task: Option<&TaskId>,
    role: AgentRole,
    id: &JobId,
    workspace: Option<&WorkspaceId>,
) {
    let mut s = f.store();
    let job = AgentJob {
        version: ProtocolVersion::V1,
        job_id: id.clone(),
        agent_id: AgentId::new(format!("agent:{}", id.as_str())).unwrap(),
        role,
        plan_id: p.packet.plan_id.clone(),
        task_id: task.cloned(),
        state: JobState::Queued,
        provider: None,
        created_at_ms: 90,
        started_at_ms: None,
        finished_at_ms: None,
    };
    if let Some(workspace) = workspace {
        s.register_job_in_workspace(&f.info.repository_id, workspace, &job)
            .unwrap();
    } else {
        s.register_job(&f.info.repository_id, &job).unwrap();
    }
    s.transition_job(
        &f.info.repository_id,
        id,
        JobState::Queued,
        JobState::Running,
        100,
    )
    .unwrap();
    s.transition_job(
        &f.info.repository_id,
        id,
        JobState::Running,
        JobState::Succeeded,
        110,
    )
    .unwrap();
}
fn evidence(f: &Fixture, id: &str) -> EvidenceRef {
    evidence_at(f, id, Some(&f.info.workspace_id))
}
fn evidence_at(f: &Fixture, id: &str, workspace: Option<&WorkspaceId>) -> EvidenceRef {
    let mut e = common::samples()["evidence"].clone();
    e["evidence_id"] = json!(id);
    if let Some(workspace) = workspace {
        f.store()
            .record_evidence_in_workspace(&f.info.repository_id, workspace, &decode(e))
            .unwrap();
    } else {
        f.store()
            .record_evidence(&f.info.repository_id, &decode(e))
            .unwrap();
    }
    EvidenceRef(EvidenceId::new(id).unwrap())
}
fn execute(f: &Fixture, p: &ExecutionPlan, i: usize) {
    let id = &p.packet.tasks[i].task_id;
    transition(f, id, TaskState::Planned, TaskState::Ready);
    transition(f, id, TaskState::Ready, TaskState::Executing);
    finished_job(
        f,
        p,
        Some(id),
        AgentRole::Executor,
        &JobId::new(format!("{id}:exec", id = id.as_str())).unwrap(),
    );
    transition(f, id, TaskState::Executing, TaskState::AwaitingVerification);
}
fn verify(f: &Fixture, p: &ExecutionPlan, i: usize, pass: bool) {
    let v = packet_proof(f, p, i, pass);
    f.store()
        .transition_task(
            &f.info.repository_id,
            &p.packet.tasks[i].task_id,
            TaskState::Verifying,
            if pass {
                TaskState::Verified
            } else {
                TaskState::Rejected
            },
            Some(&v),
            130,
        )
        .unwrap();
}
fn packet_proof(f: &Fixture, p: &ExecutionPlan, i: usize, pass: bool) -> VerificationPacket {
    let t = &p.packet.tasks[i];
    let verifier = JobId::new(format!("{}:verify", t.task_id.as_str())).unwrap();
    finished_job(f, p, Some(&t.task_id), AgentRole::Verifier, &verifier);
    let e = evidence(f, &format!("{}:evidence", t.task_id.as_str()));
    transition(
        f,
        &t.task_id,
        TaskState::AwaitingVerification,
        TaskState::Verifying,
    );
    VerificationPacket {
        version: ProtocolVersion::V1,
        verification_id: VerificationId::new(format!("{}:proof", t.task_id.as_str())).unwrap(),
        target: VerificationTarget::Packet {
            task_id: t.task_id.clone(),
            executor_job_id: JobId::new(format!("{}:exec", t.task_id.as_str())).unwrap(),
        },
        verifier_job_id: verifier,
        decision: if pass {
            VerificationDecision::Pass
        } else {
            VerificationDecision::Reject
        },
        findings: if pass {
            vec![]
        } else {
            vec![VerificationFinding {
                severity: FindingSeverity::Error,
                requirement_refs: vec!["unit".into()],
                invariant_refs: t.invariant_refs.clone(),
                location: None,
                problem: "Stale metadata reused".into(),
            }]
        },
        evidence: vec![e],
        requirement_refs: vec!["unit".into()],
        invariant_refs: t.invariant_refs.clone(),
        notes: None,
    }
}
fn final_proof(f: &Fixture, p: &ExecutionPlan) -> VerificationPacket {
    let id = JobId::new(format!("{}:integration", p.packet.plan_id.as_str())).unwrap();
    finished_job(f, p, None, AgentRole::Verifier, &id);
    VerificationPacket {
        version: ProtocolVersion::V1,
        verification_id: VerificationId::new("verification:final").unwrap(),
        target: VerificationTarget::Integration {
            plan_id: p.packet.plan_id.clone(),
            executor_job_ids: p
                .packet
                .tasks
                .iter()
                .map(|t| JobId::new(format!("{}:exec", t.task_id.as_str())).unwrap())
                .collect(),
        },
        verifier_job_id: id,
        decision: VerificationDecision::Pass,
        findings: vec![],
        evidence: vec![evidence(f, &format!("{}:final", p.packet.plan_id.as_str()))],
        requirement_refs: vec!["integration".into()],
        invariant_refs: p.packet.tasks[0].invariant_refs.clone(),
        notes: None,
    }
}
fn final_source() -> SourceStateRef {
    decode(common::samples()["evidence"]["source_state"].clone())
}

#[test]
fn request_context_is_bounded_provenanced_and_byte_identical_on_reopen() {
    let f = Fixture::new();
    let c = f.memory(MemoryTrustClass::Canonical);
    let n = f.memory(MemoryTrustClass::AgentNote);
    let p = f.prepare();
    assert_eq!(
        p.request.source.observation.workspace_id,
        f.info.workspace_id
    );
    assert!(p.request.source.observation.worktree_fingerprint.is_none());
    assert!(!p.context.graph.primary.is_empty());
    assert!(
        p.context
            .policy
            .invariants
            .contains_key("cache:correctness")
    );
    assert!(p.context.memory.items.iter().any(|m| m.id == c.id.as_str()));
    assert!(!p.context.memory.items.iter().any(|m| m.id == n.id.as_str()));
    assert_eq!(p.serialized_bytes, size(&p).unwrap());
    assert!(p.serialized_bytes <= p.context.limits.bytes);
    for e in &p.context.excerpts {
        let source = fs::read_to_string(f.root.join(&e.provenance.path)).unwrap();
        assert_eq!(&source[e.start_byte..e.end_byte], e.text);
        assert!(e.text.len() <= p.context.limits.excerpt_bytes);
        assert!(e.text.lines().count() <= p.context.limits.excerpt_lines);
    }
    let before = count(&f.sql(), "events");
    let reopened = f
        .store()
        .planning_context(&f.root, &p.request.request_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        serde_json::to_value(reopened).unwrap()
    );
    assert_eq!(before, count(&f.sql(), "events"));
    let opted = f
        .store()
        .prepare_plan(
            &f.root,
            intent(),
            PlanningLimits {
                memory: agentctl::local::memory::MemoryLimits {
                    notes: 1,
                    ..PlanningLimits::default().memory
                },
                ..PlanningLimits::default()
            },
        )
        .unwrap();
    assert!(
        opted
            .context
            .memory
            .items
            .iter()
            .any(|m| m.id == n.id.as_str() && m.trust == MemoryTrustClass::AgentNote)
    );
}
#[test]
fn small_context_limits_report_truncation_without_exceeding_budget() {
    let f = Fixture::new();
    f.memory(MemoryTrustClass::Canonical);
    let p = f
        .store()
        .prepare_plan(
            &f.root,
            intent(),
            PlanningLimits {
                bytes: 7000,
                files: 1,
                excerpt_bytes: 24,
                excerpt_lines: 1,
                ..PlanningLimits::default()
            },
        )
        .unwrap();
    assert!(p.context.truncated);
    assert!(p.serialized_bytes <= 7000);
    assert!(p.request.source.support.len() <= 1);
    assert!(
        f.store()
            .prepare_plan(
                &f.root,
                intent(),
                PlanningLimits {
                    bytes: 100,
                    ..PlanningLimits::default()
                }
            )
            .is_err()
    );
}
#[test]
fn invalid_request_and_failed_request_event_leave_no_rows() {
    let f = Fixture::new();
    for objective in ["", " \n", "bad\0text"] {
        let mut d = intent();
        d.objective = objective.into();
        assert!(
            f.store()
                .prepare_plan(&f.root, d, PlanningLimits::default())
                .is_err()
        );
    }
    f.sql().execute_batch("CREATE TRIGGER fail_request BEFORE INSERT ON events WHEN json_extract(NEW.entry_json,'$.kind')='PLANNING_REQUEST_CREATED' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.store()
            .prepare_plan(&f.root, intent(), PlanningLimits::default())
            .is_err()
    );
    assert_eq!(count(&f.sql(), "planning_requests"), 0);
}
#[test]
fn multi_task_plan_roundtrips_canonical_packets_and_contracts() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "cache", false);
    f.import(&p);
    let v = f
        .store()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(v.state, PlanState::Validated);
    assert_eq!(
        serde_json::to_value(&v.plan).unwrap(),
        serde_json::to_value(&p).unwrap()
    );
    assert_eq!(
        f.store()
            .plan(&f.info.repository_id, &p.packet.plan_id)
            .unwrap()
            .unwrap(),
        p.packet
    );
    assert_eq!(count(&f.sql(), "tasks"), 4);
    for t in f.tasks(&p) {
        assert_eq!(t.packet_bytes, size(&t.packet).unwrap());
        assert_eq!(t.contract.task_packet_hash, hash(&t.packet).unwrap());
        assert!(t.contract.independent_verifier);
    }
    assert!(
        f.sql()
            .execute("UPDATE execution_plans SET metadata_json='{}'", [])
            .is_err()
    );
    assert!(
        f.sql()
            .execute("DELETE FROM planning_requests", [])
            .is_err()
    );
}
#[test]
fn malformed_dags_and_incomplete_contracts_publish_nothing() {
    let f = Fixture::new();
    let base = artifact(&f.prepare(), "invalid", false);
    let mut variants = vec![];
    let mut p = base.clone();
    p.packet.tasks[1].task_id = p.packet.tasks[0].task_id.clone();
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].dependencies = vec![TaskId::new("missing").unwrap()];
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].dependencies = vec![p.packet.tasks[0].task_id.clone()];
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].dependencies = vec![p.packet.tasks[1].task_id.clone()];
    variants.push(p);
    let mut p = base.clone();
    p.metadata.contracts.pop();
    variants.push(p);
    let mut p = base.clone();
    p.metadata.contracts[0].independent_verifier = false;
    variants.push(p);
    let mut p = base.clone();
    p.metadata.integration.require_all_task_verifications = false;
    variants.push(p);
    let mut p = base.clone();
    p.metadata.integration.independent_verifier = false;
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].definition_of_done.clear();
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].verification.requirement_refs.clear();
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].objective.clear();
    variants.push(p);
    for mut p in variants {
        rehash(&mut p);
        assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    }
    assert_eq!(count(&f.sql(), "execution_plans"), 0);
    assert_eq!(count(&f.sql(), "plans"), 0);
    assert_eq!(count(&f.sql(), "tasks"), 0);
    let mut json = serde_json::to_value(base).unwrap();
    json["metadata"]["integration"] = Value::Null;
    assert!(serde_json::from_value::<ExecutionPlan>(json).is_err());
}
#[test]
fn references_scopes_policy_and_packet_hashes_are_not_trusted_blindly() {
    let f = Fixture::new();
    let base = artifact(&f.prepare(), "invalid", false);
    let mut variants = vec![];
    let mut p = base.clone();
    p.packet.tasks[0].graph_entities = vec![GraphEntityId::new("graph:missing").unwrap()];
    rehash(&mut p);
    variants.push(p);
    let mut p = base.clone();
    p.metadata.contracts[0].memory_refs =
        vec![agentctl::local::memory::MemoryId::new("memory:missing").unwrap()];
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].invariant_refs.clear();
    rehash(&mut p);
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].verification.requirement_refs = vec!["arbitrary-shell-check".into()];
    rehash(&mut p);
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].write_scope = vec![ScopePath::File {
        path: "../escape".into(),
    }];
    rehash(&mut p);
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].write_scope = vec![ScopePath::File {
        path: "outside/file.rs".into(),
    }];
    rehash(&mut p);
    variants.push(p);
    let mut p = base.clone();
    p.metadata.contracts[0].exclusions = vec![ScopePath::Directory { path: "src".into() }];
    variants.push(p);
    let mut p = base.clone();
    p.metadata.contracts[0].task_packet_hash = "blake3:fake".into();
    variants.push(p);
    let mut p = base.clone();
    p.packet.tasks[0].objective = "x".repeat(17000);
    rehash(&mut p);
    variants.push(p);
    for p in variants {
        assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    }
    assert_eq!(count(&f.sql(), "tasks"), 0);
}
#[test]
fn activation_and_verified_only_dag_readiness_localize_work() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "cache", false);
    f.import(&p);
    assert_eq!(
        f.tasks(&p).iter().filter(|t| t.structurally_ready).count(),
        1
    );
    assert!(
        f.store()
            .transition_task(
                &f.info.repository_id,
                &p.packet.tasks[0].task_id,
                TaskState::Planned,
                TaskState::Ready,
                None,
                100
            )
            .is_err()
    );
    f.activate(&p);
    execute(&f, &p, 0);
    assert_eq!(f.tasks(&p)[0].state, TaskState::AwaitingVerification);
    assert!(!f.tasks(&p)[1].structurally_ready);
    verify(&f, &p, 0, true);
    assert!(f.tasks(&p)[1].structurally_ready);
    assert!(!f.tasks(&p)[2].structurally_ready);
    execute(&f, &p, 1);
    verify(&f, &p, 1, true);
    assert!(f.tasks(&p)[2].structurally_ready);
    assert!(f.tasks(&p)[3].structurally_ready);
}
#[test]
fn independent_tasks_are_simultaneously_structurally_ready() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "parallel", true);
    f.import(&p);
    assert_eq!(
        f.tasks(&p).iter().filter(|t| t.structurally_ready).count(),
        4
    );
}
#[test]
fn verification_rejection_blocks_the_entire_dependency_cascade() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "reject", false);
    f.import(&p);
    f.activate(&p);
    execute(&f, &p, 0);
    verify(&f, &p, 0, false);
    let tasks = f.tasks(&p);
    assert_eq!(tasks[0].state, TaskState::Rejected);
    assert!(tasks.iter().all(|t| !t.structurally_ready));
    assert!(tasks[1].reasons.join(" ").contains("not VERIFIED"));
}
#[test]
fn all_prerequisites_must_be_verified_not_just_one() {
    let f = Fixture::new();
    let mut p = artifact(&f.prepare(), "join", true);
    p.packet.tasks[2].dependencies = vec![
        p.packet.tasks[0].task_id.clone(),
        p.packet.tasks[1].task_id.clone(),
    ];
    rehash(&mut p);
    f.import(&p);
    f.activate(&p);
    execute(&f, &p, 0);
    verify(&f, &p, 0, true);
    assert!(!f.tasks(&p)[2].structurally_ready);
    execute(&f, &p, 1);
    verify(&f, &p, 1, true);
    assert!(f.tasks(&p)[2].structurally_ready);
}
#[test]
fn integration_pass_with_registered_evidence_is_required_for_completion() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "complete", false);
    f.import(&p);
    f.activate(&p);
    let proof = final_proof(&f, &p);
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
            .is_err()
    );
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let mut wrong = proof.clone();
    wrong.requirement_refs = vec!["unit".into()];
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &wrong, &final_source())
            .is_err()
    );
    let mut wrong = proof.clone();
    wrong.target = VerificationTarget::Packet {
        task_id: p.packet.tasks[0].task_id.clone(),
        executor_job_id: JobId::new("complete:0:exec").unwrap(),
    };
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &wrong, &final_source())
            .is_err()
    );
    let mut wrong = proof.clone();
    if let VerificationTarget::Integration {
        executor_job_ids, ..
    } = &mut wrong.target
    {
        executor_job_ids.pop();
    }
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &wrong, &final_source())
            .is_err()
    );
    let mut source = final_source();
    source.revision = "git:other".into();
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &source)
            .is_err()
    );
    f.store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
    let v = f
        .store()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(v.state, PlanState::Complete);
    assert_eq!(v.integration_proof, Some(proof));
    assert_eq!(v.final_source, Some(final_source()));
}
#[test]
fn stale_source_graph_policy_and_memory_references_are_rejected() {
    let f = Fixture::new();
    let derived = f.store().derive_memory(&f.root, "cache_status").unwrap();
    let prepared = f.prepare();
    let mut p = artifact(&prepared, "stale", false);
    p.metadata.contracts[0].memory_refs.push(derived.id.clone());
    write(
        &f.root,
        "src/cache.rs",
        &CODE.replace("{ changed }", "{ !changed }"),
    );
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    f.store().index_repository(&f.root).unwrap();
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    let fresh = f.prepare();
    let mut p = artifact(&fresh, "fresh", false);
    p.metadata.contracts[0].memory_refs.push(derived.id);
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    assert!(
        fresh
            .context
            .memory
            .items
            .iter()
            .all(|m| m.trust != MemoryTrustClass::Derived)
    );
    let mut p = artifact(&fresh, "policy", false);
    p.metadata.source.observation.workspace_id =
        RepositoryInfo::discover(&f.root).unwrap().workspace_id;
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy
        .invariants
        .get_mut("cache:correctness")
        .unwrap()
        .description = "Changed policy".into();
    write(
        &f.root,
        ".agentctl/project.toml",
        &toml::to_string(&policy).unwrap(),
    );
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
}
#[test]
fn memory_history_and_notes_never_gain_planning_authority() {
    let f = Fixture::new();
    let canonical = f.memory(MemoryTrustClass::Canonical);
    let note = f.memory(MemoryTrustClass::AgentNote);
    let prepared = f.prepare();
    let mut p = artifact(&prepared, "memory", false);
    p.metadata.contracts[0].memory_refs = vec![note.id.clone()];
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    f.store()
        .reject_memory(&f.root, &canonical.id, "reviewer")
        .unwrap();
    p.metadata.contracts[0].memory_refs = vec![canonical.id.clone()];
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    let next = f.prepare();
    assert!(
        next.context
            .memory
            .items
            .iter()
            .all(|m| m.id != canonical.id.as_str())
    );
    assert_eq!(
        f.store()
            .memory_show(&f.root, &note.id, false)
            .unwrap()
            .entry
            .provenance
            .trust_class,
        MemoryTrustClass::AgentNote
    );
}
#[test]
fn worktrees_share_repository_memory_but_not_planning_assumptions() {
    let f = Fixture::new();
    let c = f.memory(MemoryTrustClass::Canonical);
    let linked = f.linked();
    let prepared = f.prepare();
    let p = artifact(&prepared, "main", false);
    f.import(&p);
    assert!(f.store().import_execution_plan(&linked, &p).is_err());
    assert!(
        f.store()
            .execution_plan(&linked, &p.packet.plan_id)
            .is_err()
    );
    assert!(
        f.store()
            .planning_context(&linked, &prepared.request.request_id)
            .is_err()
    );
    let other = f
        .store()
        .prepare_plan(&linked, intent(), PlanningLimits::default())
        .unwrap();
    assert_ne!(
        other.request.source.observation.workspace_id,
        prepared.request.source.observation.workspace_id
    );
    assert!(
        other
            .context
            .memory
            .items
            .iter()
            .any(|m| m.id == c.id.as_str())
    );
}
#[test]
fn supersession_is_explicit_preserves_history_and_does_not_copy_task_state() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let old = artifact(&prepared, "old", false);
    f.import(&old);
    f.activate(&old);
    execute(&f, &old, 0);
    verify(&f, &old, 0, true);
    let mut new = artifact(&prepared, "new", false);
    new.metadata.replan = Some(ReplanReference {
        previous_plan_id: old.packet.plan_id.clone(),
        reason: "Split remaining work after verified metadata change".into(),
        previously_verified_tasks: vec![old.packet.tasks[0].task_id.clone()],
        replaced_tasks: vec![old.packet.tasks[1].task_id.clone()],
    });
    f.import(&new);
    f.store()
        .supersede_execution_plan(&f.root, &old.packet.plan_id, &new.packet.plan_id)
        .unwrap();
    f.activate(&new);
    let history = f
        .store()
        .execution_plan(&f.root, &old.packet.plan_id)
        .unwrap();
    assert_eq!(history.state, PlanState::Superseded);
    assert_eq!(history.superseded_by, Some(new.packet.plan_id.clone()));
    assert_eq!(f.tasks(&old)[0].state, TaskState::Verified);
    assert!(f.tasks(&new).iter().all(|t| t.state == TaskState::Planned));
    assert!(f.tasks(&old).iter().all(|t| !t.structurally_ready));
    assert!(
        f.store()
            .supersede_execution_plan(&f.root, &new.packet.plan_id, &old.packet.plan_id)
            .is_err()
    );
    assert_eq!(
        f.store().execution_plans(&f.root, false, 20).unwrap().len(),
        1
    );
}
#[test]
fn import_validation_activation_and_supersession_events_are_atomic() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let old = artifact(&prepared, "old", false);
    f.sql().execute_batch("CREATE TRIGGER fail_plan BEFORE INSERT ON events WHEN json_extract(NEW.entry_json,'$.kind') LIKE 'EXECUTION_PLAN_%' BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;").unwrap();
    assert!(f.store().import_execution_plan(&f.root, &old).is_err());
    assert_eq!(count(&f.sql(), "tasks"), 0);
    assert_eq!(count(&f.sql(), "plans"), 0);
    assert_eq!(count(&f.sql(), "execution_plans"), 0);
    f.sql().execute_batch("DROP TRIGGER fail_plan;").unwrap();
    f.import(&old);
    let mut new = artifact(&prepared, "new", false);
    new.metadata.replan = Some(ReplanReference {
        previous_plan_id: old.packet.plan_id.clone(),
        reason: "Revised scope".into(),
        previously_verified_tasks: vec![],
        replaced_tasks: vec![],
    });
    f.import(&new);
    let before = count(&f.sql(), "events");
    f.sql().execute_batch("CREATE TRIGGER fail_plan BEFORE INSERT ON events WHEN json_extract(NEW.entry_json,'$.kind') LIKE 'EXECUTION_PLAN_%' BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;").unwrap();
    assert!(
        f.store()
            .validate_execution_plan(&f.root, &old.packet.plan_id)
            .is_err()
    );
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &old.packet.plan_id)
            .is_err()
    );
    assert!(
        f.store()
            .supersede_execution_plan(&f.root, &old.packet.plan_id, &new.packet.plan_id)
            .is_err()
    );
    assert!(
        f.store()
            .cancel_execution_plan(&f.root, &old.packet.plan_id, "cancel")
            .is_err()
    );
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &old.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Validated
    );
    assert_eq!(count(&f.sql(), "events"), before);
}
#[test]
fn cancellation_and_single_active_plan_have_explicit_gates() {
    let f = Fixture::new();
    let prepared = f.prepare();
    let a = artifact(&prepared, "a", true);
    let b = artifact(&prepared, "b", true);
    f.import(&a);
    f.import(&b);
    f.activate(&a);
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &b.packet.plan_id)
            .is_err()
    );
    f.store()
        .cancel_execution_plan(&f.root, &a.packet.plan_id, "No longer needed")
        .unwrap();
    assert!(f.tasks(&a).iter().all(|t| !t.structurally_ready));
    f.activate(&b);
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &a.packet.plan_id)
            .is_err()
    );
}
fn downgrade(c: &Connection) {
    common::strip_runtime(c);
    c.execute_batch("DROP TRIGGER execution_task_gate; DROP TABLE execution_plans; DROP TABLE planning_requests; DELETE FROM schema_migrations WHERE version>=5; PRAGMA user_version=4;").unwrap();
}
#[test]
fn v4_migration_is_additive_reopen_safe_and_preserves_memory_graph_events() {
    let f = Fixture::new();
    let m = f.memory(MemoryTrustClass::Canonical);
    let c = f.sql();
    let events = count(&c, "events");
    let entities = count(&c, "graph_entities");
    let memory: String = c
        .query_row("SELECT record_json FROM memory_entries", [], |r| r.get(0))
        .unwrap();
    downgrade(&c);
    assert!(Store::read_only(&f.db, 5000).is_err());
    drop(f.store());
    assert_eq!(count(&c, "events"), events);
    assert_eq!(count(&c, "graph_entities"), entities);
    assert_eq!(
        c.query_row::<String, _, _>("SELECT record_json FROM memory_entries", [], |r| r.get(0))
            .unwrap(),
        memory
    );
    assert_eq!(
        f.store().memory_show(&f.root, &m.id, false).unwrap().entry,
        m
    );
    assert_eq!(f.store().status().unwrap().schema_version, 7);
    assert_eq!(
        c.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn migration_conflict_and_future_version_fail_without_partial_tables() {
    let f = Fixture::new();
    let c = f.sql();
    downgrade(&c);
    c.execute_batch("CREATE TABLE execution_plans(conflict TEXT);")
        .unwrap();
    assert!(Store::open(&f.db, 5000).is_err());
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='planning_requests'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        c.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        4
    );
    c.execute_batch("DROP TABLE execution_plans; PRAGMA user_version=8;")
        .unwrap();
    assert!(Store::open(&f.db, 5000).is_err());
}
fn cli(f: &Fixture, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(&f.root)
        .args(args)
        .env("HOME", f.temp.0.join("home"))
        .env("XDG_CONFIG_HOME", f.temp.0.join("config"))
        .env("XDG_DATA_HOME", f.temp.0.join("data"))
        .env("XDG_CACHE_HOME", f.temp.0.join("cache"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap()
}
fn cli_json(f: &Fixture, args: &[&str]) -> Value {
    let o = cli(f, args);
    assert!(
        o.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}
#[test]
fn real_cli_planner_artifact_boundary_roundtrips_across_processes() {
    let f = Fixture::new();
    cli_json(&f, &["init", "--json"]);
    cli_json(&f, &["repo", "init", "--json"]);
    cli_json(&f, &["repo", "index", "--json"]);
    let value = cli_json(
        &f,
        &[
            "plan",
            "prepare",
            "--objective",
            "Add deterministic cache invalidation to repository indexing",
            "--query",
            "cache invalidation",
            "--json",
        ],
    );
    let prepared: PlannerPacket = decode(value.clone());
    assert_eq!(
        cli_json(
            &f,
            &[
                "plan",
                "context",
                prepared.request.request_id.as_str(),
                "--json"
            ]
        ),
        value
    );
    let plan = artifact(&prepared, "cli-plan", false);
    let file = f.temp.0.join("plan.json");
    fs::write(&file, serde_json::to_vec(&plan).unwrap()).unwrap();
    let imported = cli_json(&f, &["plan", "import", file.to_str().unwrap(), "--json"]);
    assert_eq!(imported["state"], "VALIDATED");
    cli_json(&f, &["plan", "validate", "cli-plan", "--json"]);
    assert_eq!(
        cli_json(&f, &["plan", "export", "cli-plan", "--json"]),
        serde_json::to_value(&plan).unwrap()
    );
    assert_eq!(
        cli_json(&f, &["plan", "tasks", "cli-plan", "--json"])
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        cli_json(&f, &["plan", "ready", "cli-plan", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        cli_json(&f, &["plan", "blocked", "cli-plan", "--json"])
            .as_array()
            .unwrap()
            .len(),
        3
    );
    cli_json(&f, &["plan", "activate", "cli-plan", "--json"]);
    assert_eq!(
        cli_json(&f, &["plan", "show", "cli-plan", "--json"])["state"],
        "ACTIVE"
    );
    assert_eq!(
        cli_json(&f, &["plan", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!cli(&f, &["plan", "run", "cli-plan"]).status.success());
    assert!(
        !cli(&f, &["plan", "import", file.to_str().unwrap()])
            .status
            .success()
    );
    let events = cli_json(&f, &["events", "list", "--limit", "100", "--json"]).to_string();
    assert!(events.contains("PLANNING_REQUEST_CREATED"));
    assert!(events.contains("EXECUTION_PLAN_IMPORTED"));
    assert!(events.contains("EXECUTION_PLAN_VALIDATED"));
}
#[test]
fn stored_planner_text_and_verification_commands_are_never_executed() {
    let f = Fixture::new();
    let sentinel = f.temp.0.join("executed");
    let mut d = intent();
    d.constraints
        .push(format!("$(touch {})", sentinel.display()));
    let p = f
        .store()
        .prepare_plan(&f.root, d, PlanningLimits::default())
        .unwrap();
    let plan = artifact(&p, "untrusted", true);
    f.import(&plan);
    f.activate(&plan);
    f.tasks(&plan);
    assert!(!sentinel.exists());
    assert_eq!(count(&f.sql(), "jobs"), 0);
    assert_eq!(count(&f.sql(), "evidence"), 0);
}

#[test]
fn activation_revalidates_memory_and_prepared_source_after_import() {
    let f = Fixture::new();
    let m = f.memory(MemoryTrustClass::Canonical);
    let prepared = f.prepare();
    let mut p = artifact(&prepared, "drift", false);
    p.metadata.contracts[0].memory_refs.push(m.id.clone());
    f.import(&p);
    f.store().reject_memory(&f.root, &m.id, "reviewer").unwrap();
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &p.packet.plan_id)
            .is_err()
    );
    let q = artifact(&f.prepare(), "source-drift", false);
    f.import(&q);
    write(
        &f.root,
        "src/cache.rs",
        &format!("{CODE}\n// changed after import\n"),
    );
    f.store().index_repository(&f.root).unwrap();
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &q.packet.plan_id)
            .is_err()
    );
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &q.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Validated
    );
    assert_eq!(
        f.store()
            .planning_context(&f.root, &prepared.request.request_id)
            .unwrap()
            .serialized_bytes,
        prepared.serialized_bytes
    );
}

#[test]
fn historical_observation_remains_usable_but_superseded_memory_is_excluded() {
    let f = Fixture::new();
    let mut e = common::samples()["evidence"].clone();
    e["summary"] = json!("cache invalidation regression tests passed");
    f.store()
        .record_evidence_in_workspace(&f.info.repository_id, &f.info.workspace_id, &decode(e))
        .unwrap();
    let observed = f
        .store()
        .observe_evidence(&f.root, &EvidenceId::new("evidence:1").unwrap())
        .unwrap();
    let canonical = f.memory(MemoryTrustClass::Canonical);
    let replacement = MemoryDraft {
        kind: MemoryKind::Finding,
        content: "Revised cache invalidation safety".into(),
        workspace_id: None,
        canonical_key: None,
        actor: "reviewer".into(),
        author_job_id: None,
        links: canonical.links.clone(),
    };
    f.store()
        .add_memory(
            &f.root,
            replacement.clone(),
            MemoryTrustClass::Canonical,
            Some(&canonical.id),
        )
        .unwrap();
    write(
        &f.root,
        "src/cache.rs",
        &format!("{CODE}\n// later observation\n"),
    );
    f.store().index_repository(&f.root).unwrap();
    let prepared = f.prepare();
    assert!(
        prepared
            .context
            .memory
            .items
            .iter()
            .any(|m| m.id == observed.id.as_str()
                && m.validity == agentctl::local::memory::Validity::Historical)
    );
    assert!(
        prepared
            .context
            .memory
            .items
            .iter()
            .all(|m| m.id != canonical.id.as_str())
    );
    let mut p = artifact(&prepared, "observation", true);
    p.metadata.contracts[0]
        .memory_refs
        .push(observed.id.clone());
    f.import(&p);
    assert_eq!(
        f.store()
            .memory_show(&f.root, &observed.id, false)
            .unwrap()
            .entry
            .observed_source,
        Some(final_source())
    );
}

#[test]
fn keyed_canonical_invariant_text_is_carried_and_drift_is_rejected() {
    let f = Fixture::new();
    let d = MemoryDraft {
        kind: MemoryKind::Invariant,
        content: "Cache records must remain local".into(),
        workspace_id: None,
        canonical_key: Some("cache:local".into()),
        actor: "reviewer".into(),
        author_job_id: None,
        links: vec![],
    };
    let old = f
        .store()
        .add_memory(&f.root, d.clone(), MemoryTrustClass::Canonical, None)
        .unwrap();
    let mut request = intent();
    request.invariant_refs.push("cache:local".into());
    let prepared = f
        .store()
        .prepare_plan(&f.root, request, PlanningLimits::default())
        .unwrap();
    assert_eq!(prepared.context.invariants["cache:local"], d.content);
    let plan = artifact(&prepared, "keyed", true);
    f.import(&plan);
    assert_eq!(f.tasks(&plan)[0].invariants["cache:local"], d.content);
    assert_eq!(
        f.tasks(&plan)[0].constraints,
        prepared.request.intent.constraints
    );
    let mut new = d;
    new.content = "Cache records must be encrypted locally".into();
    f.store()
        .add_memory(&f.root, new, MemoryTrustClass::Canonical, Some(&old.id))
        .unwrap();
    assert!(
        f.store()
            .activate_execution_plan(&f.root, &plan.packet.plan_id)
            .is_err()
    );
}

#[test]
fn protected_and_symlink_scopes_are_rejected_without_execution() {
    let f = Fixture::new();
    let mut policy = ProjectConfig::load(&f.root).unwrap();
    policy
        .protected
        .push(agentctl::local::config::ProtectedRule {
            path: "src/private".into(),
            deny_read: true,
            deny_write: true,
            reason: "Private data".into(),
        });
    write(
        &f.root,
        ".agentctl/project.toml",
        &toml::to_string(&policy).unwrap(),
    );
    let mut d = intent();
    d.scope.clear();
    let prepared = f
        .store()
        .prepare_plan(&f.root, d, PlanningLimits::default())
        .unwrap();
    let mut p = artifact(&prepared, "protected", false);
    p.packet.tasks[0].write_scope = vec![ScopePath::File {
        path: "src/private/key.txt".into(),
    }];
    rehash(&mut p);
    assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&f.temp.0, f.root.join("src/alias")).unwrap();
        p.packet.tasks[0].write_scope = vec![ScopePath::File {
            path: "src/alias/escape.txt".into(),
        }];
        rehash(&mut p);
        assert!(f.store().import_execution_plan(&f.root, &p).is_err());
    }
    assert_eq!(count(&f.sql(), "jobs"), 0);
}

#[test]
fn cancellation_refuses_unfinished_jobs_and_completion_audit_rolls_back() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "atomic-complete", true);
    f.import(&p);
    f.activate(&p);
    let queued = AgentJob {
        version: ProtocolVersion::V1,
        job_id: JobId::new("job:pending").unwrap(),
        agent_id: AgentId::new("agent:pending").unwrap(),
        role: AgentRole::Planner,
        plan_id: p.packet.plan_id.clone(),
        task_id: None,
        state: JobState::Queued,
        provider: None,
        created_at_ms: 1,
        started_at_ms: None,
        finished_at_ms: None,
    };
    f.store()
        .register_job_in_workspace(&f.info.repository_id, &f.info.workspace_id, &queued)
        .unwrap();
    assert!(
        f.store()
            .cancel_execution_plan(&f.root, &p.packet.plan_id, "cancel")
            .is_err()
    );
    f.store()
        .transition_job(
            &f.info.repository_id,
            &queued.job_id,
            JobState::Queued,
            JobState::Cancelled,
            2,
        )
        .unwrap();
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let proof = final_proof(&f, &p);
    let before = count(&f.sql(), "events");
    f.sql().execute_batch("CREATE TRIGGER fail_completion BEFORE INSERT ON events WHEN json_extract(NEW.entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(
        f.store()
            .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
            .is_err()
    );
    let view = f
        .store()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(view.state, PlanState::Active);
    assert!(view.integration_proof.is_none());
    assert_eq!(count(&f.sql(), "events"), before);
}

#[test]
fn missing_planning_guards_are_not_silently_repaired() {
    let f = Fixture::new();
    f.sql()
        .execute_batch("DROP TRIGGER execution_task_gate;")
        .unwrap();
    assert!(Store::open(&f.db, 5000).is_err());
    assert!(Store::read_only(&f.db, 5000).is_err());
}

#[test]
fn linked_worktree_packet_jobs_and_evidence_cannot_unlock_dependencies() {
    let f = Fixture::new();
    let sibling = RepositoryInfo::discover(&f.linked()).unwrap();
    assert_eq!(f.info.repository_id, sibling.repository_id);
    assert_ne!(f.info.workspace_id, sibling.workspace_id);
    let p = artifact(&f.prepare(), "bound-packet", false);
    f.import(&p);
    f.activate(&p);
    execute(&f, &p, 0);
    let valid = packet_proof(&f, &p, 0, true);
    let task = &p.packet.tasks[0].task_id;
    for (binding, workspace) in [("sibling", Some(&sibling.workspace_id)), ("unbound", None)] {
        for role in [AgentRole::Executor, AgentRole::Verifier] {
            let mut wrong = valid.clone();
            let id = JobId::new(format!("job:{binding}:{role:?}")).unwrap();
            finished_job_at(&f, &p, Some(task), role, &id, workspace);
            if role == AgentRole::Verifier {
                wrong.verifier_job_id = id;
            } else if let VerificationTarget::Packet {
                executor_job_id, ..
            } = &mut wrong.target
            {
                *executor_job_id = id;
            }
            assert_packet_binding_rejected(&f, &p, &wrong);
        }
        let mut wrong = valid.clone();
        wrong.evidence = vec![evidence_at(&f, &format!("evidence:{binding}"), workspace)];
        assert_packet_binding_rejected(&f, &p, &wrong);
    }
    // Identical proof requirements succeed when every concrete owner matches.
    f.store()
        .transition_task(
            &f.info.repository_id,
            task,
            TaskState::Verifying,
            TaskState::Verified,
            Some(&valid),
            140,
        )
        .unwrap();
    assert_eq!(f.tasks(&p)[0].state, TaskState::Verified);
    assert!(f.tasks(&p)[1].structurally_ready);
}

fn assert_packet_binding_rejected(f: &Fixture, p: &ExecutionPlan, proof: &VerificationPacket) {
    let events = count(&f.sql(), "events");
    let error = f
        .store()
        .transition_task(
            &f.info.repository_id,
            &p.packet.tasks[0].task_id,
            TaskState::Verifying,
            TaskState::Verified,
            Some(proof),
            140,
        )
        .unwrap_err();
    assert!(error.to_string().contains("workspace"), "{error}");
    assert_eq!(f.tasks(p)[0].state, TaskState::Verifying);
    assert!(!f.tasks(p)[1].structurally_ready);
    assert!(
        f.store()
            .transition_task(
                &f.info.repository_id,
                &p.packet.tasks[1].task_id,
                TaskState::Planned,
                TaskState::Ready,
                None,
                140
            )
            .is_err()
    );
    assert_eq!(count(&f.sql(), "events"), events);
}

#[test]
fn linked_worktree_integration_jobs_and_final_evidence_are_rejected() {
    let f = Fixture::new();
    let sibling = RepositoryInfo::discover(&f.linked()).unwrap();
    assert_eq!(f.info.repository_id, sibling.repository_id);
    let p = artifact(&f.prepare(), "bound-final", false);
    f.import(&p);
    f.activate(&p);
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let valid = final_proof(&f, &p);
    for (binding, workspace) in [("sibling", Some(&sibling.workspace_id)), ("unbound", None)] {
        let mut wrong = valid.clone();
        wrong.verifier_job_id = JobId::new(format!("job:integration:{binding}")).unwrap();
        finished_job_at(
            &f,
            &p,
            None,
            AgentRole::Verifier,
            &wrong.verifier_job_id,
            workspace,
        );
        assert_completion_binding_rejected(&f, &p, &wrong);
        let mut wrong = valid.clone();
        // Same final SourceStateRef as the accepted workspace is not enough.
        wrong.evidence = vec![evidence_at(
            &f,
            &format!("evidence:integration:{binding}"),
            workspace,
        )];
        assert_completion_binding_rejected(&f, &p, &wrong);
    }
    f.store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, &valid, &final_source())
        .unwrap();
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Complete
    );
}

fn assert_completion_binding_rejected(f: &Fixture, p: &ExecutionPlan, proof: &VerificationPacket) {
    let events = count(&f.sql(), "events");
    let error = f
        .store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, proof, &final_source())
        .unwrap_err();
    assert!(error.to_string().contains("workspace"), "{error}");
    let view = f
        .store()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(view.state, PlanState::Active);
    assert!(view.integration_proof.is_none());
    assert!(view.final_source.is_none());
    assert_eq!(count(&f.sql(), "events"), events);
}

#[test]
fn integration_rechecks_workspace_ownership_of_historically_accepted_packets() {
    let f = Fixture::new();
    let sibling = RepositoryInfo::discover(&f.linked()).unwrap();
    let p = artifact(&f.prepare(), "historical-proof", false);
    f.import(&p);
    f.activate(&p);
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let proof = final_proof(&f, &p);
    let c = f.sql();
    // Simulate records accepted by the old Stage 4 gate; immutable event payloads
    // are untouched. Completion must not grandfather the old ownership defect.
    for (table, key, id) in [
        ("jobs", "job_id", "historical-proof:0:exec"),
        ("jobs", "job_id", "historical-proof:0:verify"),
        ("evidence", "evidence_id", "historical-proof:0:evidence"),
    ] {
        for workspace in [Some(sibling.workspace_id.as_str()), None] {
            c.execute(
                &format!("UPDATE {table} SET workspace_id=?1 WHERE {key}=?2"),
                rusqlite::params![workspace, id],
            )
            .unwrap();
            assert_completion_binding_rejected(&f, &p, &proof);
        }
        c.execute(
            &format!("UPDATE {table} SET workspace_id=?1 WHERE {key}=?2"),
            [f.info.workspace_id.as_str(), id],
        )
        .unwrap();
    }
    f.store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
}

#[test]
fn direct_sql_cannot_complete_a_plan_even_with_non_null_payloads_or_replace() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "sql-complete", false);
    f.import(&p);
    f.activate(&p);
    let c = f.sql();
    let events = count(&c, "events");
    assert!(c.execute("UPDATE execution_plans SET state='COMPLETE',integration_json='{}',final_source_json='{}'", []).is_err());
    assert!(c.execute("INSERT OR REPLACE INTO execution_plans SELECT repo_id,plan_id,request_id,workspace_id,metadata_json,'COMPLETE',updated_at_ms,NULL,'{}','{}' FROM execution_plans", []).is_err());
    let view = f
        .store()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(view.state, PlanState::Active);
    assert!(view.integration_proof.is_none());
    assert!(view.final_source.is_none());
    assert_eq!(count(&c, "events"), events);
    assert_eq!(completion_events(&c), 0);
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let proof = final_proof(&f, &p);
    // Actual validated payloads alone cannot mint SQL authorization either.
    assert!(
        c.execute(
            "UPDATE execution_plans SET state='COMPLETE',integration_json=?1,final_source_json=?2",
            [
                serde_json::to_string(&proof).unwrap(),
                serde_json::to_string(&final_source()).unwrap()
            ]
        )
        .is_err()
    );
    assert_eq!(completion_events(&c), 0);
    f.store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
    let reopened = Store::read_only(&f.db, 5000)
        .unwrap()
        .execution_plan(&f.root, &p.packet.plan_id)
        .unwrap();
    assert_eq!(reopened.state, PlanState::Complete);
    assert_eq!(reopened.integration_proof, Some(proof.clone()));
    assert_eq!(reopened.final_source, Some(final_source()));
    assert_eq!(completion_events(&c), 1);
    let event: agentctl::local::store::JournalEntry = serde_json::from_str(&c.query_row::<String, _, _>(
        "SELECT entry_json FROM events WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'", [], |r| r.get(0)).unwrap()).unwrap();
    assert!(
        matches!(event, agentctl::local::store::JournalEntry::ExecutionPlanCompleted { verification, source, .. }
        if verification == proof && source == final_source())
    );
}

fn completion_events(c: &Connection) -> i64 {
    c.query_row("SELECT count(*) FROM events WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'", [], |r| r.get(0)).unwrap()
}

#[test]
fn failed_authorized_update_rolls_back_audit_and_allows_retry_on_same_store() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "rollback-complete", false);
    f.import(&p);
    f.activate(&p);
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let proof = final_proof(&f, &p);
    let c = f.sql();
    let events = count(&c, "events");
    // Fails after the matching event was appended and the capability installed.
    c.execute_batch("CREATE TRIGGER fail_complete_update AFTER UPDATE ON execution_plans WHEN NEW.state='COMPLETE' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let mut s = f.store();
    assert!(
        s.complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
            .is_err()
    );
    assert_eq!(
        s.execution_plan(&f.root, &p.packet.plan_id).unwrap().state,
        PlanState::Active
    );
    assert_eq!(count(&c, "events"), events);
    assert_eq!(completion_events(&c), 0);
    c.execute_batch("DROP TRIGGER fail_complete_update;")
        .unwrap();
    s.complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
    assert_eq!(completion_events(&c), 1);
}

fn downgrade_completion(c: &Connection) {
    common::strip_runtime(c);
    c.execute_batch("DROP TRIGGER execution_completion_guard; DROP TRIGGER execution_initial_state; DELETE FROM schema_migrations WHERE version=6; PRAGMA user_version=5;").unwrap();
}

#[test]
fn v5_completion_guard_migration_preserves_plan_payloads_and_events() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "migration-complete", false);
    f.import(&p);
    f.activate(&p);
    let c = f.sql();
    let events = count(&c, "events");
    downgrade_completion(&c);
    assert!(Store::read_only(&f.db, 5000).is_err());
    let s = f.store();
    assert_eq!(s.status().unwrap().schema_version, 7);
    let view = s.execution_plan(&f.root, &p.packet.plan_id).unwrap();
    assert_eq!(
        serde_json::to_value(&view.plan).unwrap(),
        serde_json::to_value(&p).unwrap()
    );
    assert_eq!(view.state, PlanState::Active);
    assert_eq!(count(&c, "events"), events);
    assert!(c.execute("UPDATE execution_plans SET state='COMPLETE',integration_json='{}',final_source_json='{}'", []).is_err());
    assert_eq!(completion_events(&c), 0);
}

// Completion artifacts/audits have the same representation in v5 and v6. Build
// trusted history through the public application gate, then restore the v5 schema.
fn completed_plan(f: &Fixture, prefix: &str) -> ExecutionPlan {
    let p = artifact(&f.prepare(), prefix, false);
    f.import(&p);
    f.activate(&p);
    for i in 0..4 {
        execute(f, &p, i);
        verify(f, &p, i, true);
    }
    let proof = final_proof(f, &p);
    f.store()
        .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
    p
}

fn legacy_snapshot(c: &Connection) -> Vec<Vec<Vec<String>>> {
    [
        "sqlite_schema",
        "schema_migrations",
        "execution_plans",
        "plans",
        "tasks",
        "jobs",
        "evidence",
        "events",
    ]
    .iter()
    .map(|table| {
        let mut statement = c
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get_ref(i).map(|v| format!("{v:?}")))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}

fn assert_legacy_rejected(f: &Fixture, p: &ExecutionPlan, reason: &str) {
    let before = legacy_snapshot(&f.sql());
    for _ in 0..2 {
        let error = Store::open(&f.db, 5000)
            .err()
            .expect("invalid legacy completion must fail")
            .to_string();
        assert!(
            error.contains(p.packet.plan_id.as_str())
                && error.contains("migration aborted")
                && error.contains(reason),
            "{error}"
        );
        assert!(Store::read_only(&f.db, 5000).is_err());
        let reopened = f.sql();
        assert_eq!(
            reopened
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            5
        );
        assert_eq!(legacy_snapshot(&reopened), before);
        assert_eq!(reopened.query_row::<i64, _, _>("SELECT count(*) FROM sqlite_schema WHERE name IN ('execution_completion_guard','execution_initial_state')", [], |r| r.get(0)).unwrap(), 0);
    }
}

// Deliberately construct corrupted legacy fixtures, restoring the exact v5 guard
// definitions afterward so rejection is about history, not missing schema objects.
fn corrupt_legacy(c: &Connection, sql: &str) {
    let guards = [
        "execution_plans_immutable",
        "execution_task_gate",
        "events_no_update",
        "events_no_delete",
    ];
    let definitions: Vec<String> = guards
        .iter()
        .map(|name| {
            c.query_row("SELECT sql FROM sqlite_schema WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
        })
        .collect();
    for name in guards {
        c.execute_batch(&format!("DROP TRIGGER {name};")).unwrap();
    }
    c.execute_batch(sql).unwrap();
    for definition in definitions {
        c.execute_batch(&definition).unwrap();
    }
}

#[test]
fn legacy_complete_valid_history_migrates_without_live_checkout_and_preserves_bytes() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "valid-legacy", false);
    f.import(&p);
    f.activate(&p);
    for i in 0..4 {
        execute(&f, &p, i);
        verify(&f, &p, i, true);
    }
    let proof = final_proof(&f, &p);
    let mut application = f.store();
    let c = f.sql();
    downgrade_completion(&c);
    // Exercise validated application completion while the database is still v5,
    // retaining the already-open connection so no implicit migration happens.
    application
        .complete_execution_plan(&f.root, &p.packet.plan_id, &proof, &final_source())
        .unwrap();
    drop(application);
    assert_eq!(
        c.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        5
    );
    let before = legacy_snapshot(&c);
    // A historical completion must not depend on current policy/source freshness.
    fs::rename(&f.root, f.temp.0.join("moved-checkout")).unwrap();
    assert_eq!(f.store().status().unwrap().schema_version, 7);
    let after = legacy_snapshot(&c);
    assert_eq!(&after[2..], &before[2..]);
    assert_eq!(completion_events(&c), 1);
    assert_eq!(
        c.query_row::<String, _, _>(
            "SELECT state FROM execution_plans WHERE plan_id=?1",
            [p.packet.plan_id.as_str()],
            |r| r.get(0)
        )
        .unwrap(),
        "COMPLETE"
    );
    assert_eq!(
        Store::read_only(&f.db, 5000)
            .unwrap()
            .status()
            .unwrap()
            .schema_version,
        7
    );
}

#[test]
fn legacy_complete_sql_bypass_fails_atomically_and_reopen_stays_v5() {
    let f = Fixture::new();
    let p = artifact(&f.prepare(), "bypass-legacy", false);
    f.import(&p);
    f.activate(&p);
    let c = f.sql();
    downgrade_completion(&c);
    c.execute(
        "UPDATE execution_plans SET state='COMPLETE',integration_json='{}',final_source_json='{}'",
        [],
    )
    .unwrap();
    assert_legacy_rejected(&f, &p, "missing field");
    assert_eq!(completion_events(&c), 0);
}

#[test]
fn legacy_complete_missing_or_mismatched_completion_audit_is_rejected() {
    for edit in [
        "DELETE FROM events WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'",
        "UPDATE events SET entry_json=json_set(entry_json,'$.source.revision','git:other') WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'",
        "UPDATE events SET workspace_id=NULL WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'",
    ] {
        let f = Fixture::new();
        let p = completed_plan(&f, "audit-legacy");
        let c = f.sql();
        downgrade_completion(&c);
        corrupt_legacy(&c, edit);
        assert_legacy_rejected(&f, &p, "completion audit");
    }
}

#[test]
fn legacy_complete_incomplete_or_unproven_packet_verification_is_rejected() {
    for (edit, reason) in [
        (
            "UPDATE tasks SET state_json='\"PLANNED\"'",
            "every task must be VERIFIED",
        ),
        (
            "DELETE FROM events WHERE json_extract(entry_json,'$.kind')='TASK_STATE_CHANGED'",
            "task verification history",
        ),
        (
            "UPDATE events SET entry_json=json_set(entry_json,'$.verification.requirement_refs',json('[\"other\"]')) WHERE json_extract(entry_json,'$.to')='VERIFIED'",
            "required check",
        ),
        (
            "UPDATE events SET entry_json=json_set(entry_json,'$.verification.invariant_refs',json('[]')) WHERE json_extract(entry_json,'$.to')='VERIFIED'",
            "invariant",
        ),
    ] {
        let f = Fixture::new();
        let p = completed_plan(&f, "packet-legacy");
        let c = f.sql();
        downgrade_completion(&c);
        corrupt_legacy(&c, edit);
        assert_legacy_rejected(&f, &p, reason);
    }
}

#[test]
fn legacy_complete_sibling_and_unbound_packet_ownership_fail_migration() {
    let f = Fixture::new();
    let sibling = RepositoryInfo::discover(&f.linked()).unwrap();
    assert_eq!(sibling.repository_id, f.info.repository_id);
    let p = completed_plan(&f, "ownership-legacy");
    let c = f.sql();
    downgrade_completion(&c);
    for (table, key, id) in [
        ("jobs", "job_id", "ownership-legacy:0:exec"),
        ("jobs", "job_id", "ownership-legacy:0:verify"),
        ("evidence", "evidence_id", "ownership-legacy:0:evidence"),
    ] {
        for owner in [Some(sibling.workspace_id.as_str()), None] {
            c.execute(
                &format!("UPDATE {table} SET workspace_id=?1 WHERE {key}=?2"),
                rusqlite::params![owner, id],
            )
            .unwrap();
            assert_legacy_rejected(&f, &p, "workspace");
        }
        c.execute(
            &format!("UPDATE {table} SET workspace_id=?1 WHERE {key}=?2"),
            [f.info.workspace_id.as_str(), id],
        )
        .unwrap();
    }
    // Explicitly restoring the genuine original records permits a later retry.
    assert_eq!(f.store().status().unwrap().schema_version, 7);
    assert_eq!(completion_events(&c), 1);
}

#[test]
fn legacy_complete_missing_evidence_and_unsuccessful_jobs_are_rejected() {
    for (edit, reason) in [
        (
            "DELETE FROM evidence WHERE evidence_id='records-legacy:0:evidence'",
            "evidence",
        ),
        (
            "UPDATE jobs SET packet_json=json_set(packet_json,'$.state','FAILED') WHERE job_id='records-legacy:0:exec'",
            "successfully finished executor",
        ),
        (
            "UPDATE jobs SET packet_json=json_set(packet_json,'$.state','FAILED') WHERE job_id='records-legacy:0:verify'",
            "successfully finished verifier",
        ),
        (
            "UPDATE jobs SET packet_json=json_set(packet_json,'$.state','FAILED') WHERE job_id='records-legacy:integration'",
            "successful plan-level verifier",
        ),
    ] {
        let f = Fixture::new();
        let p = completed_plan(&f, "records-legacy");
        let c = f.sql();
        downgrade_completion(&c);
        c.execute_batch(edit).unwrap();
        assert_legacy_rejected(&f, &p, reason);
    }
}

#[test]
fn legacy_complete_final_evidence_source_and_workspace_must_match() {
    for edit in [
        "UPDATE evidence SET record_json=json_set(record_json,'$.source_state.revision','git:other') WHERE evidence_id='final-legacy:final'",
        "UPDATE evidence SET workspace_id=NULL WHERE evidence_id='final-legacy:final'",
        "UPDATE jobs SET workspace_id=NULL WHERE job_id='final-legacy:integration'",
    ] {
        let f = Fixture::new();
        let p = completed_plan(&f, "final-legacy");
        let c = f.sql();
        downgrade_completion(&c);
        c.execute_batch(edit).unwrap();
        assert_legacy_rejected(&f, &p, "workspace");
    }
}

#[test]
fn legacy_complete_integration_pass_requirements_and_executor_set_are_validated() {
    for (expression, reason) in [
        (
            "json_set(integration_json,'$.decision','BLOCKED','$.notes','blocked fixture')",
            "integration PASS",
        ),
        (
            "json_set(integration_json,'$.requirement_refs',json('[\"other\"]'))",
            "required check",
        ),
        (
            "json_remove(integration_json,'$.target.executor_job_ids[0]')",
            "executor set",
        ),
        (
            "json_set(integration_json,'$.target.plan_id','wrong-plan')",
            "for this plan",
        ),
    ] {
        let f = Fixture::new();
        let p = completed_plan(&f, "integration-legacy");
        let c = f.sql();
        downgrade_completion(&c);
        // Even an audit matching the invalid payload cannot establish validity.
        corrupt_legacy(
            &c,
            &format!(
                "UPDATE execution_plans SET integration_json={expression}; UPDATE events SET entry_json=json_set(entry_json,'$.verification',json((SELECT integration_json FROM execution_plans))) WHERE json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED';"
            ),
        );
        assert_legacy_rejected(&f, &p, reason);
    }
}

#[test]
fn legacy_complete_mixed_valid_and_invalid_plans_roll_back_entire_upgrade() {
    let f = Fixture::new();
    completed_plan(&f, "a-valid-legacy");
    let invalid = completed_plan(&f, "z-invalid-legacy");
    let c = f.sql();
    downgrade_completion(&c);
    corrupt_legacy(
        &c,
        "DELETE FROM events WHERE plan_id='z-invalid-legacy' AND json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'",
    );
    assert_legacy_rejected(&f, &invalid, "completion audit is missing");
    assert_eq!(completion_events(&c), 1);
}

#[test]
fn legacy_complete_empty_migration_still_allows_normal_v6_completion() {
    let f = Fixture::new();
    downgrade_completion(&f.sql());
    assert_eq!(f.store().status().unwrap().schema_version, 7);
    let p = completed_plan(&f, "after-migration");
    assert_eq!(
        f.store()
            .execution_plan(&f.root, &p.packet.plan_id)
            .unwrap()
            .state,
        PlanState::Complete
    );
    assert_eq!(completion_events(&f.sql()), 1);
}

#[test]
fn completion_migration_conflicts_roll_back_and_missing_guards_fail_closed() {
    let f = Fixture::new();
    let c = f.sql();
    downgrade_completion(&c);
    c.execute_batch("CREATE TRIGGER execution_initial_state BEFORE INSERT ON execution_plans BEGIN SELECT RAISE(ABORT,'conflict'); END;").unwrap();
    assert!(Store::open(&f.db, 5000).is_err());
    assert_eq!(
        c.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        5
    );
    assert_eq!(
        c.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_schema WHERE name='execution_completion_guard'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    c.execute_batch("DROP TRIGGER execution_initial_state;")
        .unwrap();
    drop(f.store());
    for name in ["execution_completion_guard", "execution_initial_state"] {
        c.execute_batch(&format!("DROP TRIGGER {name};")).unwrap();
        assert!(Store::open(&f.db, 5000).is_err());
        assert!(Store::read_only(&f.db, 5000).is_err());
        // Restore only the test fixture by returning to known v5 and migrating.
        common::strip_runtime(&c);
        c.execute_batch(&format!("DROP TRIGGER {}; DELETE FROM schema_migrations WHERE version=6; PRAGMA user_version=5;",
            if name == "execution_completion_guard" { "execution_initial_state" } else { "execution_completion_guard" })).unwrap();
        drop(f.store());
    }
}
