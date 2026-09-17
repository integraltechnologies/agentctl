//! Part M: a realistic Stage 2 dogfood over agentctl's own source tree.
//!
//! The planner deliberately issues an incomplete executor context (one symbol
//! plus the file the task may write), the executor asks for the symbol's
//! callers, the relay grants them inside the task envelope, and a fresh second
//! job finishes the work. Every measurement the stage asks for is printed with
//! `--nocapture`, and the assertions compare the result against what the
//! pre-Stage-2 runtime would have injected for the same task.
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
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

/// The symbol the planner selects: the context-manifest builder.
const SYMBOL: &str = "for_job";
/// The file the task may change.
const WRITE_TARGET: &str = "src/local/runtime/manifest.rs";
/// The task's authorization envelope.
const READ_SCOPE: &str = "src/local/runtime";
/// Repository text that is relevant to neither the symbol nor the write target;
/// none of it may reach a worker. `ScratchCleanup` lives *inside* the task's
/// Directory read scope and the pre-Stage-2 fill would have injected its file,
/// so its absence is what proves the envelope injects nothing.
const UNRELATED: [&str; 5] = [
    "ScratchCleanup",
    "STOPWORDS",
    "Seatbelt",
    "objective_query",
    "Landlock",
];
/// A line from the body of the caller's file. The relay issues callers as
/// identity only, so their bodies must never appear.
const CALLER_BODY: &str = "let mut launched = false";

enum Reply {
    Plan(Box<ExecutionPlan>),
    Callers(GraphEntityId),
    Edit,
    Pass,
}

struct Scripted {
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<JobInput>>>,
}

impl ProviderAdapter for Scripted {
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
        let reply = {
            let mut replies = self.replies.lock().unwrap();
            assert!(!replies.is_empty(), "unscripted {:?} job", input.role);
            replies.remove(0)
        };
        let value = match (input.role, reply) {
            (AgentRole::Planner, Reply::Plan(plan)) => serde_json::to_value(&*plan).unwrap(),
            (AgentRole::Executor, Reply::Callers(entity)) => {
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: input.task_id.clone().unwrap(),
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Blocked,
                    changed_paths: vec![],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: Some(FailureInfo {
                        code: CONTEXT_REQUIRED.into(),
                        summary: "the callers of this function are not issued".into(),
                    }),
                    context_request: Some(ContextRequest {
                        version: ProtocolVersion::V1,
                        task_id: input.task_id.clone(),
                        job_id: input.job_id.clone(),
                        reason: "I must keep every caller of the manifest builder compiling, and none is issued".into(),
                        items: vec![ContextRequestItem::SymbolRelations {
                            entity_id: entity,
                            relation: RelationDirection::Callers,
                        }],
                        max_bytes: 8192,
                    }),
                })
                .unwrap()
            }
            (AgentRole::Executor, Reply::Edit) => {
                let task: TaskPacket =
                    serde_json::from_value(input.artifact["task"].clone()).unwrap();
                let path = task.write_scope[0].path();
                let prior = fs::read_to_string(process.workspace.join(path)).unwrap();
                fs::write(
                    process.workspace.join(path),
                    format!("{prior}// Stage 2 dogfood: context round recorded here.\n"),
                )
                .unwrap();
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: task.task_id,
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![path.into()],
                    changed_entities: vec![],
                    evidence: vec![],
                    notes: None,
                    failure: None,
                    context_request: None,
                })
                .unwrap()
            }
            (AgentRole::Verifier, reply) => {
                let target: VerificationTarget =
                    serde_json::from_value(input.artifact["target"].clone()).unwrap();
                // A verifier request derived from verifier-visible material: the
                // diff changes a function, so who calls it? Identity facts from
                // files this change did not touch resolve; the definition of the
                // changed function itself could not be reissued, because its
                // ontology row no longer binds to the captured source.
                let request = match reply {
                    Reply::Callers(entity) => Some(ContextRequest {
                        version: ProtocolVersion::V1,
                        task_id: input.task_id.clone(),
                        job_id: input.job_id.clone(),
                        reason: "the changed function has callers I must check for breakage".into(),
                        items: vec![ContextRequestItem::SymbolRelations {
                            entity_id: entity,
                            relation: RelationDirection::Callers,
                        }],
                        max_bytes: 8192,
                    }),
                    _ => None,
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
                    decision: if request.is_some() {
                        VerificationDecision::Blocked
                    } else {
                        VerificationDecision::Pass
                    },
                    findings: vec![],
                    evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                    requirement_refs: vec![if input.task_id.is_some() {
                        "unit".into()
                    } else {
                        "integration".into()
                    }],
                    invariant_refs: vec![],
                    notes: None,
                    context_request: request,
                })
                .unwrap()
            }
            (role, _) => panic!("scripted reply does not match a {role:?} job"),
        };
        Ok(Box::new(Done(Some(ProcessOutput {
            exit: Some(0),
            stdout: serde_json::to_vec(&value).unwrap(),
            stderr: vec![],
            failure: None,
        }))))
    }
    fn collect(&self, output: &ProcessOutput) -> local::Result<Value> {
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

struct Done(Option<ProcessOutput>);
impl RunningProcess for Done {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn poll(&mut self) -> local::Result<Option<ProcessOutput>> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) -> local::Result<CancellationOutcome> {
        Ok(CancellationOutcome::AlreadyExited)
    }
}

struct Checks;
impl CheckLauncher for Checks {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, _: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        Ok(Box::new(Done(Some(ProcessOutput {
            exit: Some(0),
            stdout: b"dogfood check output".to_vec(),
            stderr: vec![],
            failure: None,
        }))))
    }
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Dogfood")
        .env("GIT_AUTHOR_EMAIL", "dogfood@example.invalid")
        .env("GIT_COMMITTER_NAME", "Dogfood")
        .env("GIT_COMMITTER_EMAIL", "dogfood@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Copies this repository's own sources into a disposable checkout.
fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&source, &target);
        } else {
            fs::copy(&source, &target).unwrap();
        }
    }
}

fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

/// Category bytes of one part of an issued job input.
fn category(m: &manifest::ContextManifest, prefix: &str) -> usize {
    m.bytes
        .categories
        .iter()
        .filter(|c| c.category == prefix || c.category.starts_with(&format!("{prefix}.")))
        .map(|c| c.bytes)
        .sum()
}

/// What the pre-Stage-2 runtime would have injected for this task: the first
/// sixteen files under the Directory read scope in path order, each truncated
/// to 4096 characters, chosen by the runtime rather than by the planner.
fn legacy_directory_fill(root: &Path) -> (usize, usize, Vec<String>) {
    let mut files: Vec<PathBuf> = fs::read_dir(root.join(READ_SCOPE))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut total = 0;
    let mut names = vec![];
    for path in files.iter().take(16) {
        let text = fs::read_to_string(path).unwrap_or_default();
        total += text.chars().take(4096).count();
        names.push(path.strip_prefix(root).unwrap().display().to_string());
    }
    (total, names.len(), names)
}

#[test]
fn stage_two_dogfood_over_agentctls_own_source() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if RepositoryInfo::discover(&manifest_dir).is_err() {
        eprintln!("skipping: {} is not a Git checkout", manifest_dir.display());
        return;
    }
    let temp = common::TempDir::new();
    let root = temp.0.join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    copy_tree(&manifest_dir.join("src"), &root.join("src"));
    copy_tree(&manifest_dir.join("tests"), &root.join("tests"));
    fs::copy(manifest_dir.join("Cargo.toml"), root.join("Cargo.toml")).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
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
                description: format!("{name} checks"),
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
    git(&root, &["commit", "--quiet", "-m", "agentctl sources"]);
    let paths = MachinePaths::resolve(&PathContext {
        home: Some(temp.0.join("home")),
        ..Default::default()
    })
    .unwrap();
    paths.create_directories().unwrap();
    let mut config = RuntimeConfig::default();
    config.providers.insert(
        "codex".into(),
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
                provider: "codex".into(),
                model: Some(format!("opaque-{role}")),
                effort: None,
            },
        );
    }
    fs::write(
        &paths.machine_config,
        toml::to_string(&MachineConfig {
            runtime: config.clone(),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    let store = || Store::open(&paths.database, 5000).unwrap();
    let mut initial = store();
    initial
        .register_repository(RepositoryInfo::discover(&root).unwrap())
        .unwrap();
    let indexed = initial.index_repository(&root).unwrap();
    assert_eq!(indexed.failed, 0, "agentctl's own sources index cleanly");
    drop(initial);

    // A realistic Stage 2 change: record the context round in the manifest the
    // engine builds. The planner selects one symbol and one write target, and
    // deliberately leaves the callers out of the issued context.
    let prepared = store()
        .prepare_plan(
            &root,
            RequestDraft {
                objective:
                    "Record the context round and issued authorities in the runtime context manifest"
                        .into(),
                query: Some(SYMBOL.into()),
                scope: vec![ScopePath::Directory {
                    path: READ_SCOPE.into(),
                }],
                constraints: vec!["Preserve exact byte accounting".into()],
                definition_of_done: vec!["The manifest records the context round".into()],
                verification: Some(VerificationRequirements {
                    requirement_refs: vec!["integration".into()],
                    evidence_required: true,
                }),
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "human".into(),
                    source_refs: vec!["stage-2".into()],
                    provider: None,
                },
            },
            PlanningLimits::default(),
        )
        .unwrap();
    let selected = prepared
        .context
        .graph
        .primary
        .iter()
        .find(|p| p.entity.name == SYMBOL)
        .unwrap_or_else(|| panic!("the planner packet must select {SYMBOL}"))
        .entity
        .clone();
    assert_eq!(selected.provenance.path, WRITE_TARGET);
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new("task:manifest-round").unwrap(),
        objective: "Record the context round in the issued manifest".into(),
        read_scope: vec![ScopePath::Directory {
            path: READ_SCOPE.into(),
        }],
        write_scope: vec![ScopePath::File {
            path: WRITE_TARGET.into(),
        }],
        graph_entities: vec![selected.id.clone()],
        invariant_refs: prepared.request.intent.invariant_refs.clone(),
        dependencies: vec![],
        definition_of_done: vec!["The manifest records the context round".into()],
        verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:dogfood").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["integration".into()],
            evidence_required: true,
        },
    };
    let plan = ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: local::now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "dogfood-planner".into(),
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
    let seen = Arc::new(Mutex::new(vec![]));
    let replies = Arc::new(Mutex::new(vec![
        Reply::Plan(Box::new(plan.clone())),
        Reply::Callers(selected.id.clone()),
        Reply::Edit,
        Reply::Callers(selected.id.clone()),
        Reply::Pass,
        Reply::Pass,
    ]));
    let adapters = || -> BTreeMap<String, Box<dyn ProviderAdapter>> {
        BTreeMap::from([(
            "codex".to_string(),
            Box::new(Scripted {
                replies: replies.clone(),
                seen: seen.clone(),
            }) as Box<dyn ProviderAdapter>,
        )])
    };
    let mut planning_store = store();
    let imported = Runtime::new(
        &mut planning_store,
        paths.clone(),
        config.clone(),
        adapters(),
    )
    .unwrap()
    .plan(&root, &prepared.request.request_id)
    .unwrap();
    assert_eq!(imported.state, PlanState::Validated);
    drop(planning_store);
    store()
        .activate_execution_plan(&root, &plan.packet.plan_id)
        .unwrap();
    let mut run_store = store();
    let outcome = Runtime::new(&mut run_store, paths.clone(), config.clone(), adapters())
        .unwrap()
        .with_check_launcher(Box::new(Checks))
        .run(&root, &plan.packet.plan_id)
        .unwrap();
    assert_eq!(outcome.state, RunState::Complete);
    drop(run_store);

    // ---- measurements -----------------------------------------------------
    let inputs = seen.lock().unwrap();
    let jobs = store().runtime_jobs(&root, None).unwrap();
    let manifest_of = |id: &JobId| {
        jobs.iter()
            .find(|j| &j.job_id == id)
            .unwrap()
            .context_manifest
            .clone()
            .unwrap()
    };
    let planner = inputs
        .iter()
        .find(|i| i.role == AgentRole::Planner)
        .expect("a planner job ran");
    let executors: Vec<&JobInput> = inputs
        .iter()
        .filter(|i| i.role == AgentRole::Executor)
        .collect();
    let packet_verifiers: Vec<&JobInput> = inputs
        .iter()
        .filter(|i| i.role == AgentRole::Verifier && i.task_id.is_some())
        .collect();
    assert_eq!(executors.len(), 2, "one context round, two executor jobs");
    assert_eq!(packet_verifiers.len(), 2, "the verifier relayed once too");
    let (round_one, round_two) = (
        manifest_of(&executors[0].job_id),
        manifest_of(&executors[1].job_id),
    );
    let (verify_one, verify_two) = (
        manifest_of(&packet_verifiers[0].job_id),
        manifest_of(&packet_verifiers[1].job_id),
    );
    let ledger = outcome
        .context
        .get("executor:task:manifest-round")
        .expect("executor ledger");
    let verifier_ledger = outcome
        .context
        .get("verifier:task:manifest-round")
        .expect("verifier ledger");
    let request: ContextRequest = {
        let artifacts = Artifacts::new(&paths.data_root.join("runtime/blobs")).unwrap();
        artifacts.decode(&ledger.rounds[0].request).unwrap()
    };
    let delta: Value = executors[1].artifact["deltas"][0].clone();
    let verifier_delta: Value = packet_verifiers[1].artifact["deltas"][0].clone();
    let (legacy_bytes, legacy_files, legacy_names) = legacy_directory_fill(&root);
    let target_size = fs::metadata(root.join(WRITE_TARGET)).unwrap().len() as usize;
    let issued_count = |m: &manifest::ContextManifest, authority: manifest::Authority| {
        m.issued.iter().filter(|i| i.authority == authority).count()
    };

    eprintln!("\n===== Stage 2 dogfood: agentctl's own source tree =====");
    eprintln!(
        "repository: {} indexed files, {} entities, {} edges",
        indexed.indexed + indexed.reused,
        indexed.entities,
        indexed.edges
    );
    eprintln!(
        "PLANNER            prompt {} B (context {} B, instructions {} B)",
        manifest_of(&planner.job_id).bytes.total,
        manifest_of(&planner.job_id).bytes.context,
        manifest_of(&planner.job_id).bytes.instructions
    );
    eprintln!(
        "EXECUTOR ROUND 1   prompt {} B; base context {} B; {} symbol(s), {} file(s), {} memory, {} check(s); round {:?}",
        round_one.bytes.total,
        category(&round_one, "artifact.context"),
        issued_count(&round_one, manifest::Authority::PlannerGraphEntity),
        issued_count(&round_one, manifest::Authority::PlannerWriteTarget)
            + issued_count(&round_one, manifest::Authority::PlannerReadFile),
        issued_count(&round_one, manifest::Authority::PlannerMemoryRef),
        issued_count(&round_one, manifest::Authority::PlannerVerificationRef),
        round_one.context_round,
    );
    eprintln!(
        "  issued paths: {:?}",
        round_one
            .paths
            .iter()
            .map(|p| format!("{} ({:?})", p.path, p.kind))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "CONTEXT REQUEST    {} item(s) {:?}, max_bytes {}, reason {} B",
        request.items.len(),
        request
            .items
            .iter()
            .map(|i| serde_json::to_value(i).unwrap()["kind"].clone())
            .collect::<Vec<_>>(),
        request.max_bytes,
        request.reason.len()
    );
    eprintln!(
        "CONTEXT DELTA      {} B, {} item(s), grant {}, delta {}",
        ledger.rounds[0].delta.as_ref().unwrap().bytes,
        delta["items"].as_array().unwrap().len(),
        delta["grant"]["kind"],
        ledger.rounds[0].delta.as_ref().unwrap().delta_id,
    );
    eprintln!(
        "EXECUTOR ROUND 2   prompt {} B (base {} B + delta {} B); round {:?}",
        round_two.bytes.total,
        category(&round_two, "artifact.context"),
        category(&round_two, "artifact.deltas"),
        round_two.context_round,
    );
    eprintln!(
        "VERIFIER ROUND 1   prompt {} B; diff {} B for a {} B file",
        verify_one.bytes.total,
        bytes(&packet_verifiers[0].artifact["diff"]),
        target_size
    );
    eprintln!(
        "VERIFIER ROUND 2   prompt {} B; verifier delta {} B, {} item(s)",
        verify_two.bytes.total,
        verifier_ledger.rounds[0].delta.as_ref().unwrap().bytes,
        verifier_delta["items"].as_array().unwrap().len()
    );
    eprintln!(
        "PRE-STAGE-2        directory fill would have injected {} B from {} runtime-chosen files: {:?}",
        legacy_bytes, legacy_files, legacy_names
    );
    eprintln!(
        "PRE-STAGE-2        verifier diff would have carried before+after \u{2248} {} B for this file",
        2 * target_size
    );
    eprintln!("=======================================================\n");

    // ---- assertions -------------------------------------------------------
    // The planner's references are the only source of initial context.
    assert_eq!(round_one.context_round, Some(0));
    assert_eq!(round_two.context_round, Some(1));
    assert_eq!(
        issued_count(&round_one, manifest::Authority::PlannerGraphEntity),
        1
    );
    assert_eq!(
        round_one
            .issued
            .iter()
            .filter(|i| i.authority == manifest::Authority::PlannerWriteTarget)
            .map(|i| i.reference.clone())
            .collect::<Vec<_>>(),
        [WRITE_TARGET]
    );
    // A Directory read scope holding the whole runtime module injects nothing.
    // What remains is the planner's own references, which here is less than half
    // of what the pre-Stage-2 directory fill would have sent -- and that fill
    // was chosen by the runtime, not by the plan.
    assert!(
        category(&round_one, "artifact.context") * 2 < legacy_bytes,
        "base context {} B vs legacy fill {} B",
        category(&round_one, "artifact.context"),
        legacy_bytes
    );
    assert!(
        legacy_files == 16 && legacy_names.iter().any(|n| n.contains("experiment")),
        "the legacy fill really did reach unrelated files: {legacy_names:?}"
    );
    // Round 2 is the same base plus exactly the granted delta.
    assert_eq!(
        executors[0].artifact["context"],
        executors[1].artifact["context"]
    );
    assert_eq!(executors[1].artifact["deltas"].as_array().unwrap().len(), 1);
    assert_eq!(
        round_two.context_deltas[0].delta_id,
        ledger.rounds[0].delta.as_ref().unwrap().delta_id
    );
    // The delta names the callers by identity; no unrelated file content and no
    // caller body reaches the worker in either round.
    let round_two_prompt =
        String::from_utf8_lossy(&executors[1].compiled.as_ref().unwrap().bytes).into_owned();
    assert!(
        round_two_prompt.contains("invoke_attempt"),
        "the caller is named"
    );
    assert!(
        !round_two_prompt.contains(CALLER_BODY),
        "a caller's body leaked into the issued context"
    );
    for executor in &executors {
        let text = String::from_utf8_lossy(&executor.compiled.as_ref().unwrap().bytes);
        for unrelated in UNRELATED {
            assert!(
                !text.contains(unrelated),
                "{unrelated} leaked into the executor"
            );
        }
    }
    // Verifier context scales with the change, not the file.
    let diff_bytes = bytes(&packet_verifiers[0].artifact["diff"]);
    assert!(
        diff_bytes <= 8 * 1024 && diff_bytes * 4 < 2 * target_size,
        "verifier diff {diff_bytes} B for a {target_size} B file"
    );
    // The verifier relayed on its own budget without executor history.
    assert_eq!(verifier_ledger.rounds_used(), 1);
    for verifier in &packet_verifiers {
        let text = String::from_utf8_lossy(&verifier.compiled.as_ref().unwrap().bytes);
        assert!(!text.contains(&request.reason), "executor reason leaked");
        assert!(
            !text.contains(&ledger.rounds[0].delta.as_ref().unwrap().delta_id),
            "executor delta leaked"
        );
    }
    // Everything the two providers received stays inside the issued budget.
    assert!(round_two.bytes.total < 256 * 1024);
    assert_eq!(
        round_two
            .bytes
            .categories
            .iter()
            .map(|c| c.bytes)
            .sum::<usize>(),
        round_two.bytes.total
    );
}
