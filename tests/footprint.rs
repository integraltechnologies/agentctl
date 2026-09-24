//! Structural footprint economy: concrete delta facts, conservative
//! classification, bounded review signals, and unchanged authority/lifecycle.
#[allow(dead_code)]
mod common;

use agentctl::{
    local::{
        config::{ProjectConfig, VerificationDefinition},
        graph::{
            Change, FootprintLimits, FootprintRequest, IdentityBasis, ReviewEvidence,
            ReviewSignalKind, StructuralFootprint, StructuralRole, Surface,
        },
        now_ms,
        planning::{
            ExecutionPlan, IntegrationVerificationContract, PlanMetadata, PlannerPacket,
            PlanningLimits, PlanningProvenance, RequestDraft, VerificationContract, VerifierInput,
            hash,
        },
        repository::RepositoryInfo,
        store::Store,
    },
    protocol::*,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const BASE: &str = "pub fn capture() -> u32 {\n    1\n}\n\nfn local() {}\n";

struct Fixture {
    _temp: common::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        let mut policy = ProjectConfig::initialize(&root).unwrap();
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
                    description: id.into(),
                    command_refs: vec![id.into()],
                },
            );
        }
        write(
            &root,
            ".agentctl/project.toml",
            &toml::to_string(&policy).unwrap(),
        );
        for (path, source) in files {
            write(&root, path, source);
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let db = temp.0.join("data/agentctl/state.sqlite3");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        Store::open(&db, 5000)
            .unwrap()
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        let fixture = Self {
            _temp: temp,
            root,
            db,
        };
        fixture.index();
        fixture
    }

    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }

    fn index(&self) {
        self.store().index_repository(&self.root).unwrap();
    }

    fn candidate(&self) -> String {
        self.store()
            .ontology_status(&self.root)
            .unwrap()
            .candidate
            .expect("candidate")
            .generation_id
    }

    fn report(&self, limits: FootprintLimits) -> StructuralFootprint {
        self.store()
            .ontology_footprint(
                &self.root,
                &FootprintRequest::Generation(self.candidate()),
                limits,
            )
            .unwrap()
    }

    fn edit(&self, path: &str, source: Option<&str>) {
        match source {
            Some(source) => write(&self.root, path, source),
            None => fs::remove_file(self.root.join(path)).unwrap(),
        }
    }

    fn observe(&self, edits: &[(&str, Option<&str>)]) -> StructuralFootprint {
        for (path, source) in edits {
            self.edit(path, *source);
        }
        self.index();
        self.report(FootprintLimits::default())
    }

    fn events(&self) -> usize {
        self.store().events(None, None, None, 1000).unwrap().len()
    }

    fn prepare(&self, scope: Vec<ScopePath>) -> PlannerPacket {
        self.store()
            .prepare_plan(
                &self.root,
                RequestDraft {
                    objective: "Extend capture".into(),
                    query: Some("capture".into()),
                    scope,
                    constraints: vec![],
                    definition_of_done: vec!["capture remains verified".into()],
                    verification: None,
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

    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&self.root)
            .args(args)
            .env("HOME", self._temp.0.join("home"))
            .env("XDG_CONFIG_HOME", self._temp.0.join("config"))
            .env("XDG_DATA_HOME", self._temp.0.join("data"))
            .env("XDG_CACHE_HOME", self._temp.0.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    }
}

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "agentctl-test")
        .env("GIT_AUTHOR_EMAIL", "agentctl@example.invalid")
        .env("GIT_COMMITTER_NAME", "agentctl-test")
        .env("GIT_COMMITTER_EMAIL", "agentctl@example.invalid")
        .status()
        .unwrap();
    assert!(status.success());
}

fn has_signal(report: &StructuralFootprint, kind: ReviewSignalKind) -> bool {
    report.signals.iter().any(|signal| signal.kind == kind)
}

#[test]
fn local_body_change_modifies_structure_but_manufactures_no_growth() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let report = f.observe(&[(
        "src/core.rs",
        Some("pub fn capture() -> u32 {\n    2\n}\n\nfn local() {}\n"),
    )]);
    assert_eq!(report.summary.production_entities_added, 0);
    assert_eq!(report.summary.production_entities_removed, 0);
    assert_eq!(report.summary.production_entities_modified, 1);
    assert!(report.signals.is_empty());
}

#[test]
fn added_removed_and_public_declarations_are_distinguished() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let added = f.observe(&[(
        "src/core.rs",
        Some("pub fn capture() -> u32 { local(); 1 }\n\nfn local() {}\n\npub struct Added;\n"),
    )]);
    assert_eq!(added.summary.production_entities_added, 1);
    assert_eq!(added.summary.public_surface_added, 1);
    assert_eq!(added.summary.relations_added, 1);
    assert!(has_signal(&added, ReviewSignalKind::PublicSurfaceGrowth));
    assert!(has_signal(
        &added,
        ReviewSignalKind::AbstractionSurfaceGrowth
    ));
    assert!(
        added
            .signals
            .iter()
            .all(|signal| !signal.evidence.is_empty())
    );

    let removed_fixture = Fixture::new(&[(
        "src/core.rs",
        "pub fn capture() -> u32 { 1 }\n\npub struct Removed;\n",
    )]);
    let removed =
        removed_fixture.observe(&[("src/core.rs", Some("pub fn capture() -> u32 { 1 }\n"))]);
    assert_eq!(removed.summary.production_entities_removed, 1);
    assert_eq!(removed.summary.public_surface_removed, 1);
}

#[test]
fn visibility_expansion_is_reported_only_where_the_index_records_it() {
    let f = Fixture::new(&[
        ("src/core.rs", "fn hidden() {}\n"),
        ("pkg/tool.py", "def hidden():\n    return 1\n"),
    ]);
    let report = f.observe(&[
        (
            "src/core.rs",
            Some("pub fn hidden() {}\npub(crate) fn restricted() {}\n"),
        ),
        ("pkg/tool.py", Some("def visible():\n    return 1\n")),
    ]);
    assert_eq!(report.summary.public_surface_expanded, 1);
    assert_eq!(report.summary.public_surface_added, 0);
    assert!(report.entities.iter().any(|entity| {
        entity.qualified_name.ends_with("restricted")
            && entity.after_surface == Some(Surface::Restricted)
    }));
    assert!(report.summary.surface_unknown >= 1);
    let public = report
        .signals
        .iter()
        .find(|s| s.kind == ReviewSignalKind::PublicSurfaceGrowth)
        .unwrap();
    assert!(
        public
            .evidence
            .iter()
            .all(|e| matches!(e, ReviewEvidence::Entity { fact } if fact.path == "src/core.rs"))
    );
}

#[test]
fn test_structure_is_not_counted_as_production_machinery() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let report = f.observe(&[(
        "tests/core_test.rs",
        Some("#[test]\nfn capture_works() { assert!(true); }\n"),
    )]);
    assert_eq!(report.summary.production_files_added, 0);
    assert_eq!(report.summary.production_entities_added, 0);
    assert_eq!(report.summary.test_files_added, 1);
    assert_eq!(report.summary.test_entities_added, 1);
    assert!(
        report
            .entities
            .iter()
            .filter(|e| e.qualified_name.contains("capture_works"))
            .all(|e| e.role == StructuralRole::Test)
    );
}

#[test]
fn rename_move_and_duplicate_identity_remain_conservative() {
    let f = Fixture::new(&[
        ("src/old.rs", "pub fn old_name() {}\n"),
        ("src/dupe.rs", "fn same() { 1; }\nfn same() { 2; }\n"),
    ]);
    let report = f.observe(&[
        ("src/old.rs", None),
        ("src/new.rs", Some("pub fn new_name() {}\n")),
        ("src/dupe.rs", Some("fn same() { 3; }\nfn same() { 2; }\n")),
    ]);
    assert!(report.entities.iter().any(|e| {
        e.path == "src/old.rs"
            && e.qualified_name.ends_with("old_name")
            && e.change == Change::Removed
    }));
    assert!(report.entities.iter().any(|e| {
        e.path == "src/new.rs"
            && e.qualified_name.ends_with("new_name")
            && e.change == Change::Added
    }));
    assert!(report.summary.unproven_identity >= 4);
    assert!(
        report
            .signals
            .iter()
            .filter(|signal| matches!(
                signal.kind,
                ReviewSignalKind::PublicSurfaceGrowth | ReviewSignalKind::AbstractionSurfaceGrowth
            ))
            .flat_map(|signal| &signal.evidence)
            .all(|evidence| matches!(
                evidence,
                ReviewEvidence::Entity { fact } if fact.identity == IdentityBasis::Unique
            ))
    );
}

#[test]
fn stale_generation_is_rejected_and_output_is_deterministic() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    f.edit("src/core.rs", Some("pub fn capture() -> u32 { 2 }\n"));
    f.index();
    let stale = f.candidate();
    let first = f.report(FootprintLimits::default());
    let second = f.report(FootprintLimits::default());
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    f.edit("src/core.rs", Some("pub fn capture() -> u32 { 3 }\n"));
    f.index();
    let error = f
        .store()
        .ontology_footprint(
            &f.root,
            &FootprintRequest::Generation(stale),
            FootprintLimits::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("not the indexed generation"), "{error}");
}

#[test]
fn output_and_signal_evidence_are_bounded_under_large_change_sets() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let mut source = String::new();
    for i in 0..80 {
        source.push_str(&format!("pub struct Type{i};\n"));
    }
    f.edit("src/many.rs", Some(&source));
    f.index();
    let report = f.report(FootprintLimits {
        files: 1,
        entities: 3,
        relations: 1,
        signals: 1,
        evidence_per_signal: 2,
    });
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.entities.len(), 3);
    assert_eq!(report.signals.len(), 1);
    assert!(report.summary.entities_omitted > 70);
    assert!(report.signals[0].evidence.len() <= 2);
    assert!(report.signals[0].evidence_omitted > 70);
}

#[test]
fn analysis_abstains_without_supported_evidence_and_is_read_only() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let before_events = f.events();
    let before_states: Vec<_> = f
        .store()
        .ontology_generations(&f.root, 100)
        .unwrap()
        .into_iter()
        .map(|g| (g.generation_id, g.state))
        .collect();
    let before_tables = table_count(&f.db);
    let report = f.observe(&[(
        "src/config.rs",
        Some("pub struct DatabaseConfig;\npub fn persist_state() {}\n"),
    )]);
    let indexed_events = f.events();
    let candidate_states: Vec<_> = f
        .store()
        .ontology_generations(&f.root, 100)
        .unwrap()
        .into_iter()
        .map(|g| (g.generation_id, g.state))
        .collect();
    let again = f.report(FootprintLimits::default());
    assert_eq!(report, again);
    assert_eq!(
        f.events(),
        indexed_events,
        "analysis appends no journal event"
    );
    assert_eq!(
        table_count(&f.db),
        before_tables,
        "analysis adds no persistence"
    );
    assert_eq!(
        f.store()
            .ontology_generations(&f.root, 100)
            .unwrap()
            .into_iter()
            .map(|g| (g.generation_id, g.state))
            .collect::<Vec<_>>(),
        candidate_states,
        "analysis mutates no lifecycle state"
    );
    assert!(
        f.events() > before_events,
        "only indexing records the candidate"
    );
    assert_ne!(before_states, candidate_states);
    assert!(!report.meaning.contains("detected persistence"));
    assert!(
        report
            .signals
            .iter()
            .all(|s| !format!("{:?}", s.kind).contains("Persistence"))
    );
}

#[test]
fn plan_review_refuses_an_external_candidate_instead_of_attaching_plan_metadata() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    let scope = vec![ScopePath::File {
        path: "src/core.rs".into(),
    }];
    let prepared = f.prepare(scope.clone());
    let plan = one_task_plan(&prepared, "plan:footprint", "src/core.rs");
    f.store().import_execution_plan(&f.root, &plan).unwrap();
    let before = f
        .store()
        .execution_plan(&f.root, &plan.packet.plan_id)
        .unwrap();
    f.edit("src/outside.rs", Some("pub struct Outside;\n"));
    f.index();
    let error = f
        .store()
        .plan_footprint(
            &f.root,
            &plan.packet.plan_id,
            &FootprintRequest::Generation(f.candidate()),
            FootprintLimits {
                files: 1,
                entities: 1,
                ..FootprintLimits::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("plan-linked footprint requires the live candidate"),
        "{error}"
    );
    let report = f.report(FootprintLimits::default());
    assert!(!has_signal(
        &report,
        ReviewSignalKind::UndeclaredStructuralGrowth
    ));
    assert!(!has_signal(
        &report,
        ReviewSignalKind::VerificationEvidencePending
    ));
    let after = f
        .store()
        .execution_plan(&f.root, &plan.packet.plan_id)
        .unwrap();
    assert_eq!(
        before.plan.packet.tasks[0].write_scope,
        after.plan.packet.tasks[0].write_scope
    );
}

#[test]
fn cli_exposes_the_bounded_structural_report() {
    let f = Fixture::new(&[("src/core.rs", BASE)]);
    f.observe(&[(
        "src/added.rs",
        Some("pub struct Added;\npub fn use_added() {}\n"),
    )]);
    assert!(f.cli(&["init"]).status.success());
    let output = f.cli(&["ontology", "footprint", "--limit", "2", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["limits"]["files"], 2);
    assert!(value["files"].as_array().unwrap().len() <= 2);
    assert!(value["entities"].as_array().unwrap().len() <= 2);
    assert_eq!(
        value["summary"]["production_entities_added"]
            .as_u64()
            .unwrap(),
        2
    );
}
fn table_count(path: &Path) -> usize {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn one_task_plan(prepared: &PlannerPacket, id: &str, write: &str) -> ExecutionPlan {
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new(format!("{id}:0")).unwrap(),
        objective: prepared.request.intent.objective.clone(),
        read_scope: vec![ScopePath::File { path: write.into() }],
        write_scope: vec![ScopePath::File { path: write.into() }],
        graph_entities: vec![prepared.context.graph.primary[0].entity.id.clone()],
        invariant_refs: vec![],
        dependencies: vec![],
        definition_of_done: prepared.request.intent.definition_of_done.clone(),
        verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new(id).unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["integration".into()],
            evidence_required: true,
        },
    };
    ExecutionPlan {
        metadata: PlanMetadata {
            version: ProtocolVersion::V1,
            request_id: prepared.request.request_id.clone(),
            source: prepared.request.source.clone(),
            created_at_ms: now_ms().unwrap(),
            provenance: PlanningProvenance {
                actor: "external-planner".into(),
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
    }
}
