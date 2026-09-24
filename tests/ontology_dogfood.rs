//! Ontology-lifecycle dogfood over agentctl's own source tree.
//!
//! A scripted executor performs a realistic refactor of agentctl's file
//! discovery (extracting the protected-path test into a helper). The runtime
//! verifies it, observes it as a candidate, and accepts it only when the
//! plan's integration verification passes. The check launcher looks at the
//! ontology from a separate connection while the run is in flight. Every
//! measurement is printed with `--nocapture`.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        self,
        config::{MachineConfig, ProjectConfig, VerificationDefinition},
        graph::{Change, DecisionReason, GenerationState, IdentityBasis, RelationKind},
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
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Instant,
};

const TARGET: &str = "src/local/graph/files.rs";
const CALLER: &str = "src::local::graph::files::discover_selected";
const HELPER: &str = "src::local::graph::files::is_protected";
const INLINE: &str = "            if protected.iter().any(|p| {
                relative == p || relative.strip_prefix(p).is_some_and(|s| s.starts_with('/'))
            }) {
                return false;
            }";
const EXTRACTED: &str = "            if is_protected(relative, &protected) {
                return false;
            }";
const HELPER_SOURCE: &str = "
/// Whether `relative` is a protected path or lies beneath one.
fn is_protected(relative: &str, protected: &[String]) -> bool {
    protected.iter().any(|p| {
        relative == p.as_str() || relative.strip_prefix(p.as_str()).is_some_and(|s| s.starts_with('/'))
    })
}
";

struct Scripted;
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
        let value = match input.role {
            AgentRole::Executor => {
                let path = process.workspace.join(TARGET);
                let text = fs::read_to_string(&path).unwrap();
                assert!(text.contains(INLINE), "the refactor target moved");
                fs::write(
                    &path,
                    format!("{}{HELPER_SOURCE}", text.replace(INLINE, EXTRACTED)),
                )
                .unwrap();
                serde_json::to_value(ResultPacket {
                    version: ProtocolVersion::V1,
                    task_id: input.task_id.clone().unwrap(),
                    executor_job_id: input.job_id.clone(),
                    status: ResultStatus::Succeeded,
                    changed_paths: vec![TARGET.into()],
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
                decision: VerificationDecision::Pass,
                findings: vec![],
                evidence: serde_json::from_value(input.artifact["evidence"].clone()).unwrap(),
                requirement_refs: vec![if input.task_id.is_some() {
                    "unit".into()
                } else {
                    "integration".into()
                }],
                invariant_refs: vec![],
                notes: None,
                context_request: None,
            })
            .unwrap(),
            role => panic!("unscripted {role:?} job"),
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

/// What a separate reader saw at each check: (accepted, open candidate).
type Sightings = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct Observing {
    database: PathBuf,
    root: PathBuf,
    seen: Sightings,
}
impl CheckLauncher for Observing {
    fn provenance(&self) -> &'static str {
        "DETERMINISTIC_TEST_FIXTURE"
    }
    fn launch(&mut self, _: &ProcessSpec) -> local::Result<Box<dyn RunningProcess>> {
        let status = Store::read_only(&self.database, 5000)?.ontology_status(&self.root)?;
        self.seen.lock().unwrap().push((
            status.accepted.unwrap().generation_id,
            status.candidate.map(|c| c.generation_id),
        ));
        Ok(Box::new(Done(Some(ProcessOutput {
            exit: Some(0),
            stdout: b"dogfood check".to_vec(),
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

fn blob_totals(database: &Path) -> (i64, i64) {
    common::sql(database)
        .query_row(
            "SELECT count(*), coalesce(sum(bytes),0) FROM ontology_blobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn plan_for(prepared: &PlannerPacket, entity: GraphEntityId) -> ExecutionPlan {
    let requirements = |name: &str| VerificationRequirements {
        requirement_refs: vec![name.into()],
        evidence_required: true,
    };
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new("task:extract-protected").unwrap(),
        objective: "Extract the protected-path test from discovery into a helper".into(),
        read_scope: vec![ScopePath::File {
            path: TARGET.into(),
        }],
        write_scope: vec![ScopePath::File {
            path: TARGET.into(),
        }],
        graph_entities: vec![entity],
        invariant_refs: prepared.request.intent.invariant_refs.clone(),
        dependencies: vec![],
        definition_of_done: vec!["discovery behavior is unchanged".into()],
        verification: requirements("unit"),
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:ontology-dogfood").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: requirements("integration"),
    };
    ExecutionPlan {
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
    }
}

#[test]
fn ontology_dogfood_over_agentctls_own_source() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if RepositoryInfo::discover(&manifest_dir).is_err() {
        eprintln!("skipping: {} is not a Git checkout", manifest_dir.display());
        return;
    }
    let temp = common::TempDir::new();
    let root = temp.0.join("repo");
    for dir in ["src", "tests"] {
        fs::create_dir_all(root.join(dir)).unwrap();
        copy_tree(&manifest_dir.join(dir), &root.join(dir));
    }
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
    let mut store = Store::open(&paths.database, 5000).unwrap();
    store
        .register_repository(RepositoryInfo::discover(&root).unwrap())
        .unwrap();

    // 1. Accepted baseline generation.
    let started = Instant::now();
    let stats = store.index_repository(&root).unwrap();
    let bootstrap_ms = started.elapsed().as_millis();
    assert_eq!(stats.failed, 0);
    let base = store.ontology_status(&root).unwrap().accepted.unwrap();
    assert_eq!(
        base.acceptance.as_ref().unwrap().reason,
        DecisionReason::Bootstrap
    );
    let (base_blobs, base_bytes) = blob_totals(&paths.database);
    eprintln!(
        "baseline: {} files indexed, {} entities, {} edges ({} resolved workspace-wide); snapshot: {} files, {} entities, {} distinct resolved relations; index+snapshot {} ms; {} ontology blobs, {} bytes",
        stats.discovered,
        stats.entities,
        stats.edges,
        stats.resolved,
        base.files,
        base.entities,
        base.relations,
        bootstrap_ms,
        base_blobs,
        base_bytes
    );
    // A no-op reindex records nothing.
    let started = Instant::now();
    store.index_repository(&root).unwrap();
    eprintln!("unchanged reindex: {} ms", started.elapsed().as_millis());
    assert_eq!(store.ontology_generations(&root, 10).unwrap().len(), 1);

    // 2–3. Plan and run: a real refactor of agentctl's discovery.
    let caller = store
        .graph(&root)
        .unwrap()
        .symbols(CALLER, local::graph::SearchMode::Exact, 2)
        .unwrap()
        .data;
    assert_eq!(caller.len(), 1);
    let prepared = store
        .prepare_plan(
            &root,
            RequestDraft {
                objective: "Extract protected path matching from discovery".into(),
                query: Some("discover protected".into()),
                scope: vec![ScopePath::Directory {
                    path: "src/local/graph".into(),
                }],
                constraints: vec![],
                definition_of_done: vec!["discovery behavior is unchanged".into()],
                verification: Some(VerificationRequirements {
                    requirement_refs: vec!["integration".into()],
                    evidence_required: true,
                }),
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
    assert_eq!(
        prepared.request.source.graph_generation.as_ref(),
        Some(&base.generation)
    );
    let plan = plan_for(&prepared, caller[0].id.clone());
    store.import_execution_plan(&root, &plan).unwrap();
    store
        .activate_execution_plan(&root, &plan.packet.plan_id)
        .unwrap();
    let sightings: Sightings = Arc::default();
    let started = Instant::now();
    let run = Runtime::new(
        &mut store,
        paths.clone(),
        config,
        std::collections::BTreeMap::from([(
            "codex".into(),
            Box::new(Scripted) as Box<dyn ProviderAdapter>,
        )]),
    )
    .unwrap()
    .with_check_launcher(Box::new(Observing {
        database: paths.database.clone(),
        root: root.clone(),
        seen: sightings.clone(),
    }))
    .run(&root, &plan.packet.plan_id)
    .unwrap();
    let run_ms = started.elapsed().as_millis();
    assert_eq!(run.state, RunState::Complete);

    // 5. While in flight, accepted truth never moved: at the task check the
    // edit was not yet observed; at the integration check it was a candidate.
    let sightings = sightings.lock().unwrap().clone();
    assert_eq!(sightings.len(), 2, "{sightings:?}");
    assert_eq!(sightings[0], (base.generation_id.clone(), None));
    assert_eq!(sightings[1].0, base.generation_id);
    let candidate_id = sightings[1]
        .1
        .clone()
        .expect("a runtime candidate before integration");

    // 7–8. Accepted through the integration boundary.
    let status = store.ontology_status(&root).unwrap();
    assert!(status.live_accepted);
    let accepted = status.accepted.unwrap();
    assert_eq!(accepted.generation_id, candidate_id);
    assert_eq!(
        accepted.acceptance.as_ref().unwrap().reason,
        DecisionReason::IntegrationVerified
    );
    assert!(accepted.generation.sequence > base.generation.sequence);

    // 4, 6, 10. The delta is exactly the refactor.
    let delta = store
        .ontology_delta(&root, &accepted.generation_id)
        .unwrap();
    let changes: BTreeSet<_> = delta
        .entities
        .iter()
        .map(|e| (e.change, e.qualified_name.as_str(), e.identity))
        .collect();
    assert_eq!(
        changes,
        BTreeSet::from([
            (Change::Modified, CALLER, IdentityBasis::Unique),
            (Change::Added, HELPER, IdentityBasis::Unique),
        ]),
        "no unrelated entity may be reported"
    );
    let caller_change = delta
        .entities
        .iter()
        .find(|e| e.qualified_name == CALLER)
        .unwrap();
    assert_eq!(caller_change.fields, vec![local::graph::EntityField::Text]);
    assert_eq!(delta.relations.len(), 1, "{:?}", delta.relations);
    let relation = &delta.relations[0];
    assert_eq!(
        (relation.change, relation.kind),
        (Change::Added, RelationKind::Calls)
    );
    assert_eq!(relation.source, caller[0].id);
    assert_eq!(
        (relation.source_path.as_str(), relation.target_path.as_str()),
        (TARGET, TARGET)
    );
    assert_eq!(delta.files.len(), 1);
    assert_eq!(delta.files[0].path, TARGET);
    let delta_bytes = serde_json::to_vec(&delta).unwrap().len();
    let (blobs, bytes) = blob_totals(&paths.database);
    eprintln!(
        "run (task + integration verification, one reindex): {run_ms} ms; generations: {:?}",
        store
            .ontology_generations(&root, 10)
            .unwrap()
            .iter()
            .map(|g| (g.ordinal, g.state))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "delta {} -> {}: entities +{} -{} ~{}, relations +{} -{}, files {} ({} semantic), unproven identity {}; delta artifact {} bytes",
        delta.from.generation_id,
        delta.to.generation_id,
        delta.summary.entities_added,
        delta.summary.entities_removed,
        delta.summary.entities_modified,
        delta.summary.relations_added,
        delta.summary.relations_removed,
        delta.summary.files,
        delta.summary.semantic_files,
        delta.summary.unproven_identity,
        delta_bytes
    );
    eprintln!(
        "storage after acceptance: {} ontology blobs (+{}), {} bytes (+{})",
        blobs,
        blobs - base_blobs,
        bytes,
        bytes - base_bytes
    );

    // 9. The old generation and its relation to the new one stay inspectable.
    let retired = store
        .ontology_generation(&root, &base.generation_id)
        .unwrap();
    assert_eq!(retired.state, GenerationState::Retired);
    assert_eq!(
        retired.closure.unwrap().by.as_deref(),
        Some(accepted.generation_id.as_str())
    );
    let reverse = store
        .ontology_diff(&root, &accepted.generation_id, &base.generation_id)
        .unwrap();
    assert_eq!(reverse.summary.entities_removed, 1);
    assert_eq!(reverse.summary.entities_modified, 1);
    assert_eq!(reverse.summary.relations_removed, 1);
    // The next plan binds to the new accepted generation.
    drop(store);
    let mut store = Store::open(&paths.database, 5000).unwrap();
    store.index_repository(&root).unwrap();
    assert_eq!(store.ontology_generations(&root, 10).unwrap().len(), 2);
}
