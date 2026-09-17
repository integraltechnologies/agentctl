//! Stage 1 ontology/context quality: implementation-first selection, bounded
//! relation noise, value-ordered truncation, conservative resolution, graph
//! generations and backward compatibility.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        config::{ProjectConfig, VerificationDefinition},
        graph::{
            AssociationBasis, ContextLimits, ContextPacket, EntityKind, RelationKind,
            ResolutionRule, SearchMode,
        },
        now_ms,
        planning::*,
        repository::RepositoryInfo,
        runtime::manifest,
        store::{JournalEntry, Store},
    },
    protocol::*,
};
use common::TempDir;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// The realistic, architecture-level request behind agentctl Issue #3. It names
/// behavior, not symbols or paths.
const ISSUE_THREE: &str = "Runtime workspace capture incorrectly includes Git-ignored build output such as target/, causing planning to fail when large generated artifacts exceed the workspace capture budget. Respect Git ignore semantics without weakening capture bounds, drift detection, scope enforcement, or source integrity.";

fn size(v: &impl serde::Serialize) -> usize {
    serde_json::to_vec(v).unwrap().len()
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
            "user.name=Context Test",
            "-c",
            "user.email=context@example.invalid",
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
fn intent(objective: &str) -> RequestDraft {
    RequestDraft {
        objective: objective.into(),
        query: None,
        scope: vec![],
        constraints: vec![],
        definition_of_done: vec!["Regression coverage exists".into()],
        verification: Some(VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        }),
        invariant_refs: vec![],
        provenance: PlanningProvenance {
            actor: "context-test".into(),
            source_refs: vec!["objective".into()],
            provider: None,
        },
    }
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, String)]) -> Self {
        let temp = TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        for (path, text) in files {
            write(&root, path, text);
        }
        let mut policy = ProjectConfig::default();
        policy.commands.insert(
            "unit".into(),
            CommandSpec {
                program: "cargo".into(),
                args: vec!["test".into()],
                cwd: ".".into(),
            },
        );
        policy.verification.insert(
            "unit".into(),
            VerificationDefinition {
                description: "Unit tests".into(),
                command_refs: vec!["unit".into()],
            },
        );
        write(
            &root,
            ".agentctl/project.toml",
            &toml::to_string(&policy).unwrap(),
        );
        let db = temp.0.join("state.sqlite3");
        let mut store = Store::open(&db, 5000).unwrap();
        store
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        store.index_repository(&root).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }
    fn context(&self, query: &str, limits: ContextLimits) -> ContextPacket {
        self.store()
            .graph(&self.root)
            .unwrap()
            .context(query, limits)
            .unwrap()
    }
    fn prepare(&self, objective: &str, limits: PlanningLimits) -> PlannerPacket {
        self.store()
            .prepare_plan(&self.root, intent(objective), limits)
            .unwrap()
    }
}

/// A workspace-capture crate whose tests mention every objective word, an
/// implementation spread over two files, and an entry point with many
/// unresolvable call sites.
fn capture_fixture() -> Fixture {
    let helpers: String = (0..160)
        .map(|i| format!("    helper_{i:03}();\n"))
        .collect();
    let tests: String = (0..12)
        .map(|i| {
            // A bare relative path (`capture::capture_workspace`, as if the
            // module were imported) resolves by unique suffix without naming
            // the crate: agentctl has no Cargo/workspace metadata, so a
            // crate-name prefix such as `fixture::` cannot be verified and
            // would abstain instead (see resolve.rs).
            format!("#[test]\nfn workspace_capture_respects_git_ignored_build_output_without_exceeding_the_capture_budget_{i}() {{\n    let kept = capture::capture_workspace(\".\");\n    assert!(kept.is_empty());\n}}\n")
        })
        .collect();
    Fixture::new(&[
        ("src/lib.rs", "pub mod capture;\npub mod git;\npub mod unrelated;\n".into()),
        (
            "src/capture.rs",
            format!("pub const CAPTURE_BUDGET_BYTES: u64 = 64;\npub fn capture_workspace(root: &str) -> Vec<String> {{\n    let listed = crate::git::list_tracked(root);\n    let kept = keep_unignored(listed);\n{helpers}    kept\n}}\nfn keep_unignored(paths: Vec<String>) -> Vec<String> {{\n    paths.into_iter().filter(|p| !super::git::is_ignored(p)).collect()\n}}\n"),
        ),
        (
            "src/git.rs",
            "pub struct IgnoreRules { pub patterns: Vec<String> }\npub fn list_tracked(root: &str) -> Vec<String> { vec![root.to_string()] }\npub fn is_ignored(path: &str) -> bool { path.starts_with(\"target/\") }\n".into(),
        ),
        (
            "src/unrelated.rs",
            "pub fn format_output(value: u64) -> String { value.to_string() }\npub fn exceeded_retry_limit(count: u64) -> bool { count > 3 }\n".into(),
        ),
        ("tests/regression.rs", tests),
    ])
}
const CAPTURE_OBJECTIVE: &str =
    "Workspace capture must respect Git ignored build output without exceeding the capture budget.";

/// Graph context JSON without the wall-clock time of the freshness observation,
/// the one field that legitimately differs between identical queries.
fn selection(v: &impl serde::Serialize) -> Value {
    let mut v = serde_json::to_value(v).unwrap();
    v["freshness"]["current_source"]["observed_at_ms"] = json!(0);
    v
}

fn test_side(e: &agentctl::local::graph::Entity) -> bool {
    e.kind == EntityKind::Test
        || e.provenance.path.starts_with("tests/")
        || e.qualified_name.split("::").any(|s| s == "tests")
}

#[test]
fn long_test_names_cannot_monopolize_implementation_primary() {
    let f = capture_fixture();
    let context = f.context(CAPTURE_OBJECTIVE, ContextLimits::default());
    assert!(!context.primary.is_empty());
    assert!(context.primary.iter().all(|p| !test_side(&p.entity)));
    assert_eq!(context.primary[0].entity.name, "capture_workspace");
    // Implementation reachable through resolved relations spans both files.
    let files: BTreeSet<_> = context
        .primary
        .iter()
        .map(|p| &p.entity)
        .chain(&context.neighbors)
        .map(|e| e.provenance.path.as_str())
        .collect();
    assert!(
        files.contains("src/capture.rs") && files.contains("src/git.rs"),
        "{files:?}"
    );
    // The regression tests are offered, as tests, with a structural basis: each
    // calls the entry point through the library crate path.
    assert!(!context.tests.is_empty());
    assert!(
        context
            .tests
            .iter()
            .all(|t| t.provenance.path == "tests/regression.rs")
    );
    assert!(
        context
            .associations
            .iter()
            .any(|a| a.basis == AssociationBasis::Calls)
    );
}

#[test]
fn primary_selection_reserves_a_slot_for_another_qualifying_file() {
    let names = ["one", "two", "three", "four", "five"];
    let alpha: String = names
        .iter()
        .map(|n| format!("pub fn ignored_capture_{n}() {{}}\n"))
        .collect();
    let f = Fixture::new(&[
        ("src/alpha.rs", alpha),
        ("src/beta.rs", "pub fn ignored_capture_beta() {}\n".into()),
    ]);
    let limits = ContextLimits {
        primary: 4,
        ..ContextLimits::default()
    };
    let first = f.context("ignored capture", limits);
    let per_file = |path: &str| {
        first
            .primary
            .iter()
            .filter(|p| p.entity.provenance.path == path)
            .count()
    };
    assert_eq!((per_file("src/alpha.rs"), per_file("src/beta.rs")), (3, 1));
    // With a single slot there is nothing to diversify; with alpha alone, alpha
    // fills every slot.
    let second = f.context("ignored capture", limits);
    assert_eq!(selection(&first), selection(&second));
}

#[test]
fn unresolved_call_sites_are_summarized_not_serialized_per_site() {
    let f = capture_fixture();
    let p = f.prepare(CAPTURE_OBJECTIVE, PlanningLimits::default());
    let g = &p.context.graph;
    assert!(
        g.relations
            .iter()
            .all(|r| r.target.is_some() && r.resolution.is_some())
    );
    assert!(
        g.relations
            .iter()
            .any(|r| r.resolution == Some(ResolutionRule::QualifiedPath))
    );
    let entry = g
        .primary
        .iter()
        .find(|e| e.entity.name == "capture_workspace")
        .unwrap();
    let summary = g
        .unresolved
        .iter()
        .find(|u| u.entity == entry.entity.id && u.kind == RelationKind::Calls)
        .unwrap();
    assert!(summary.count >= 100 && summary.names.len() <= 6);
    // Every relation record is resolved structure; unresolved syntax is a
    // bounded digest only.
    let noise = size(&g.unresolved);
    assert!(
        noise * 10 < p.serialized_bytes,
        "unresolved summaries {noise} of {}",
        p.serialized_bytes
    );
    // The same 160 call sites as individual relation records would not even fit
    // the budget; the summary costs a few hundred bytes.
    assert!(size(summary) < 600);
}

#[test]
fn truncation_sheds_graph_noise_before_implementation_excerpts() {
    let f = capture_fixture();
    let full = f.prepare(CAPTURE_OBJECTIVE, PlanningLimits::default());
    assert!(!full.context.truncated || full.serialized_bytes <= 32768);
    let entry_excerpt = |p: &PlannerPacket| {
        let id = &p.context.graph.primary.first().map(|e| e.entity.id.clone());
        p.context
            .excerpts
            .iter()
            .any(|x| x.entity.as_ref() == id.as_ref() && !x.text.is_empty())
    };
    assert!(entry_excerpt(&full));
    let mut budget = full.serialized_bytes;
    let mut checked = 0;
    while budget >= 4096 {
        let Ok(p) = f.store().prepare_plan(
            &f.root,
            intent(CAPTURE_OBJECTIVE),
            PlanningLimits {
                bytes: budget,
                ..PlanningLimits::default()
            },
        ) else {
            break;
        };
        checked += 1;
        let g = &p.context.graph;
        assert!(p.serialized_bytes <= budget);
        // Shedding order is an observable contract: nothing more valuable goes
        // while less valuable material remains.
        if g.neighbors.len() < full.context.graph.neighbors.len() {
            assert!(
                g.unresolved.is_empty() && g.relations.is_empty(),
                "{budget}"
            );
        }
        // While every neighbor survives, only test excerpts may have been shed.
        let implementation_excerpts = |p: &PlannerPacket| -> BTreeSet<GraphEntityId> {
            let tests: BTreeSet<_> = p.context.graph.tests.iter().map(|t| t.id.clone()).collect();
            p.context
                .excerpts
                .iter()
                .filter_map(|x| x.entity.clone())
                .filter(|id| !tests.contains(id))
                .collect()
        };
        if g.neighbors.len() == full.context.graph.neighbors.len() {
            assert_eq!(
                implementation_excerpts(&p),
                implementation_excerpts(&full),
                "{budget}"
            );
        }
        if !g.primary.is_empty() && (g.primary.len() > 1 || !g.neighbors.is_empty()) {
            assert!(
                entry_excerpt(&p),
                "implementation excerpt shed early at {budget}"
            );
        }
        for x in &p.context.excerpts {
            let ids = g.entity_ids();
            assert!(x.entity.as_ref().is_some_and(|id| ids.contains(id)));
        }
        budget -= 1500;
    }
    assert!(checked >= 5);
}

#[test]
fn larger_budgets_add_implementation_context_not_unresolved_noise() {
    let f = capture_fixture();
    let small = f.prepare(CAPTURE_OBJECTIVE, PlanningLimits::default());
    let large = f.prepare(
        CAPTURE_OBJECTIVE,
        PlanningLimits {
            bytes: 131072,
            files: 16,
            graph: ContextLimits {
                primary: 8,
                depth: 2,
                neighbors: 16,
                tests: 8,
            },
            ..PlanningLimits::default()
        },
    );
    let impl_ids = |p: &PlannerPacket| -> BTreeSet<_> {
        p.context
            .graph
            .primary
            .iter()
            .map(|e| &e.entity)
            .chain(&p.context.graph.neighbors)
            .filter(|e| !test_side(e))
            .map(|e| e.id.clone())
            .collect()
    };
    assert!(impl_ids(&small).is_subset(&impl_ids(&large)));
    let g = &large.context.graph;
    assert!(g.relations.iter().all(|r| r.target.is_some()));
    assert!(size(&g.unresolved) * 5 < large.serialized_bytes);
}

#[test]
fn identical_queries_and_preparations_are_byte_deterministic() {
    let f = capture_fixture();
    let a = f.context(CAPTURE_OBJECTIVE, ContextLimits::default());
    let b = f.context(CAPTURE_OBJECTIVE, ContextLimits::default());
    assert_eq!(selection(&a), selection(&b));
    let p = f.prepare(CAPTURE_OBJECTIVE, PlanningLimits::default());
    let q = f.prepare(CAPTURE_OBJECTIVE, PlanningLimits::default());
    assert_eq!(selection(&p.context.graph), selection(&q.context.graph));
    assert_eq!(
        serde_json::to_value(&p.context.excerpts).unwrap(),
        serde_json::to_value(&q.context.excerpts).unwrap()
    );
    assert_eq!(p.serialized_bytes, q.serialized_bytes);
    let m = manifest::for_packet(&p).unwrap();
    assert_eq!(m, manifest::for_packet(&p).unwrap());
    assert_eq!(m.bytes.total, p.serialized_bytes);
    assert_eq!(
        m.bytes.categories.iter().map(|c| c.bytes).sum::<usize>(),
        p.serialized_bytes
    );
    let text = serde_json::to_string(&m).unwrap();
    for x in &p.context.excerpts {
        assert!(!text.contains(x.text.trim()), "manifest copied source text");
    }
}

#[test]
fn cross_file_qualified_paths_resolve_uniquely_and_survive_incremental_reindex() {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\npub mod nested;\n".into()),
        (
            "src/a.rs",
            "pub fn capture() {}\npub struct Snapshot;\nimpl Snapshot { pub fn new() -> Self { Snapshot } }\n".into(),
        ),
        (
            "src/b.rs",
            "pub fn run() {\n    crate::a::capture();\n    super::a::capture();\n    a::capture();\n    let _ = Snapshot::new();\n    let _ = std::fs::read(\"x\");\n    c::same();\n}\n".into(),
        ),
        ("src/c.rs", "pub fn same() {}\n".into()),
        ("src/nested/c.rs", "pub fn same() {}\n".into()),
        ("src/nested/mod.rs", "pub mod c;\n".into()),
        // `fixture` names no module or crate agentctl can identify from this
        // file's own structure (root `tests::it` has no `src` segment), so
        // this must stay unresolved rather than guessing it means the crate
        // under test — the same class of guess the verifier flagged as a
        // false positive for an unrelated external/sibling crate.
        ("tests/it.rs", "#[test]\nfn integration() { fixture::a::capture(); }\n".into()),
    ]);
    let callers = |f: &Fixture| {
        f.store()
            .graph(&f.root)
            .unwrap()
            .relations("src::a::capture", true, Some(RelationKind::Calls), 20)
            .unwrap()
            .data
    };
    let incoming = callers(&f);
    assert_eq!(incoming.len(), 3, "{incoming:#?}");
    assert!(
        incoming
            .iter()
            .all(|e| e.resolution == Some(ResolutionRule::QualifiedPath))
    );
    let integration_calls = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("integration", false, Some(RelationKind::Calls), 20)
        .unwrap()
        .data;
    assert_eq!(integration_calls.len(), 1);
    assert_eq!(integration_calls[0].target_name, "fixture::a::capture");
    assert!(
        integration_calls[0].target.is_none(),
        "an unverified crate-name prefix must not resolve on suffix uniqueness alone: {integration_calls:#?}"
    );
    let outgoing = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("run", false, Some(RelationKind::Calls), 20)
        .unwrap()
        .data;
    let unresolved: BTreeSet<_> = outgoing
        .iter()
        .filter(|e| e.target.is_none())
        .map(|e| e.target_name.as_str())
        .collect();
    // An ambiguous suffix and a standard-library path stay syntax.
    assert_eq!(unresolved, BTreeSet::from(["c::same", "std::fs::read"]));
    assert!(
        outgoing
            .iter()
            .any(|e| e.target_name == "Snapshot::new" && e.target.is_some())
    );
    // Re-deriving the target file (stable entity IDs) must not cascade away the
    // unchanged caller's resolutions.
    write(
        &f.root,
        "src/a.rs",
        "// shifted\npub fn capture() { let _ = 1; }\npub struct Snapshot;\nimpl Snapshot { pub fn new() -> Self { Snapshot } }\n",
    );
    let stats = f.store().index_repository(&f.root).unwrap();
    assert_eq!((stats.indexed, stats.failed), (1, 0));
    assert_eq!(callers(&f).len(), 3);
    fs::remove_file(f.root.join("src/a.rs")).unwrap();
    let stats = f.store().index_repository(&f.root).unwrap();
    assert_eq!(stats.deleted, 1);
    assert!(
        f.store()
            .graph(&f.root)
            .unwrap()
            .relations("run", false, Some(RelationKind::Calls), 20)
            .unwrap()
            .data
            .iter()
            .all(|e| e.target.is_none())
    );
    assert!(
        common::sql(&f.db)
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn tests_are_associated_through_resolved_calls_and_helpers() {
    let f = Fixture::new(&[(
        "src/lib.rs",
        "pub fn normalize_label(x: &str) -> String { x.trim().to_string() }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    fn helper() -> String { normalize_label(\" a \") }\n    #[test]\n    fn direct_normalization() { let v = normalize_label(\" b \"); assert!(v == \"b\"); }\n    #[test]\n    fn helper_normalization() { let v = helper(); assert!(v == \"a\"); }\n}\n".into(),
    )]);
    let context = f.context(
        "normalize_label",
        ContextLimits {
            primary: 1,
            ..ContextLimits::default()
        },
    );
    assert_eq!(context.primary[0].entity.name, "normalize_label");
    let basis = |name: &str| {
        let test = context.tests.iter().find(|t| t.name == name).unwrap();
        context
            .associations
            .iter()
            .find(|a| a.test == test.id)
            .unwrap()
            .basis
    };
    assert_eq!(basis("direct_normalization"), AssociationBasis::Calls);
    assert_eq!(
        basis("helper_normalization"),
        AssociationBasis::CallsViaHelper
    );
    let related = f
        .store()
        .graph(&f.root)
        .unwrap()
        .related_tests("normalize_label", 10)
        .unwrap()
        .data;
    assert!(related.iter().any(|t| t.name == "direct_normalization"));
}

#[test]
fn graph_generations_are_content_derived_monotonic_persisted_and_bound() {
    let f = Fixture::new(&[("src/lib.rs", "pub fn stable_generation() {}\n".into())]);
    let status = f.store().index_status(&f.root).unwrap();
    let first = status.generation().cloned().unwrap();
    assert_eq!(first.sequence, 1);
    let again = f.store().index_repository(&f.root).unwrap();
    assert_eq!(again.generation.as_ref(), Some(&first));
    write(
        &f.root,
        "src/lib.rs",
        "pub fn stable_generation() { let _ = 2; }\n",
    );
    let changed = f
        .store()
        .index_repository(&f.root)
        .unwrap()
        .generation
        .unwrap();
    assert_eq!(changed.sequence, 2);
    assert_ne!(changed.fingerprint, first.fingerprint);
    write(&f.root, "src/lib.rs", "pub fn stable_generation() {}\n");
    let reverted = f
        .store()
        .index_repository(&f.root)
        .unwrap()
        .generation
        .unwrap();
    assert_eq!(
        (reverted.sequence, &reverted.fingerprint),
        (3, &first.fingerprint)
    );
    // Bound into graph context, the frozen planning source and the audit trail.
    let context = f.context("stable generation", ContextLimits::default());
    assert_eq!(context.generation.as_ref(), Some(&reverted));
    let p = f.prepare(
        "Keep the stable generation function",
        PlanningLimits::default(),
    );
    assert_eq!(p.request.source.graph_generation.as_ref(), Some(&reverted));
    assert_eq!(p.context.graph.generation.as_ref(), Some(&reverted));
    let reopened = f
        .store()
        .planning_context(&f.root, &p.request.request_id)
        .unwrap();
    assert_eq!(
        reopened.request.source.graph_generation,
        Some(reverted.clone())
    );
    let events = f.store().events(None, None, None, 100).unwrap();
    let recorded: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.entry {
            JournalEntry::IndexCompleted { stats } => stats.generation.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.last(), Some(&reverted));
    // Metadata written before generations existed is still readable and fresh;
    // the next pass starts a lineage rather than trusting a missing identity.
    common::sql(&f.db)
        .execute(
            "UPDATE graph_indexes SET metadata_json=json_remove(metadata_json,'$.generation')",
            [],
        )
        .unwrap();
    let legacy = f.store().index_status(&f.root).unwrap();
    assert!(legacy.fresh && legacy.generation().is_none());
    assert!(
        f.context("stable generation", ContextLimits::default())
            .generation
            .is_none()
    );
    assert_eq!(
        f.store()
            .index_repository(&f.root)
            .unwrap()
            .generation
            .unwrap()
            .sequence,
        1
    );
}

fn plan_for(prepared: &PlannerPacket, entity: GraphEntityId) -> ExecutionPlan {
    let task = TaskPacket {
        version: ProtocolVersion::V1,
        task_id: TaskId::new("quality:1").unwrap(),
        objective: "Adjust the function".into(),
        read_scope: vec![ScopePath::Directory { path: "src".into() }],
        write_scope: vec![ScopePath::File {
            path: "src/lib.rs".into(),
        }],
        graph_entities: vec![entity],
        invariant_refs: vec![],
        dependencies: vec![],
        definition_of_done: vec!["Adjusted".into()],
        verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
            evidence_required: true,
        },
    };
    let packet = PlanPacket {
        version: ProtocolVersion::V1,
        plan_id: PlanId::new("plan:quality").unwrap(),
        objective: prepared.request.intent.objective.clone(),
        tasks: vec![task],
        integration_verification: VerificationRequirements {
            requirement_refs: vec!["unit".into()],
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
                actor: "quality-planner".into(),
                source_refs: vec![prepared.request.request_id.as_str().into()],
                provider: None,
            },
            contracts: vec![VerificationContract {
                task_id: packet.tasks[0].task_id.clone(),
                task_packet_hash: hash(&packet.tasks[0]).unwrap(),
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
                expectations: prepared.request.intent.definition_of_done.clone(),
            },
            replan: None,
        },
        packet,
    }
}

#[test]
fn foreign_generation_bindings_are_rejected_at_import() {
    let f = Fixture::new(&[("src/lib.rs", "pub fn bound_generation() {}\n".into())]);
    let p = f.prepare(
        "Adjust the bound generation function",
        PlanningLimits::default(),
    );
    let entity = p.context.graph.primary[0].entity.id.clone();
    // A workspace whose recorded lineage is behind the bound generation (a reset
    // or swapped state database) is not the ontology the packet was built from.
    common::sql(&f.db)
        .execute(
            "UPDATE graph_indexes SET metadata_json=json_set(metadata_json,'$.generation.sequence',0)",
            [],
        )
        .unwrap();
    let error = f
        .store()
        .import_execution_plan(&f.root, &plan_for(&p, entity.clone()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("ontology generation"), "{error}");
    common::sql(&f.db)
        .execute(
            "UPDATE graph_indexes SET metadata_json=json_set(metadata_json,'$.generation.sequence',1)",
            [],
        )
        .unwrap();
    f.store()
        .import_execution_plan(&f.root, &plan_for(&p, entity))
        .unwrap();
}

/// Rewrites a current packet into the pre-Stage-1 persisted form: full
/// provenance on every graph record, no source table, generation, associations,
/// summaries or excerpt entity bindings, and the previous graph version.
fn legacy(packet: &PlannerPacket) -> Value {
    let mut v = serde_json::to_value(packet).unwrap();
    let graph = &mut v["context"]["graph"];
    let sources = graph["sources"].clone();
    let provenance = |path: &Value| {
        let file = sources["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| &f["path"] == path)
            .unwrap();
        json!({"repository_id": sources["repository_id"], "workspace_id": sources["workspace_id"], "path": path, "content_hash": file["content_hash"], "language": file["language"], "backend": file["backend"]})
    };
    for list in ["neighbors", "tests", "relations"] {
        for record in graph[list].as_array_mut().unwrap() {
            let p = provenance(&record["path"]);
            let o = record.as_object_mut().unwrap();
            o.remove("path");
            o.remove("resolution");
            o.insert("provenance".into(), p);
        }
    }
    for located in graph["primary"].as_array_mut().unwrap() {
        let p = provenance(&located["entity"]["path"]);
        let o = located["entity"].as_object_mut().unwrap();
        o.remove("path");
        o.insert("provenance".into(), p);
    }
    for key in ["sources", "generation", "associations", "unresolved"] {
        graph.as_object_mut().unwrap().remove(key);
    }
    graph["version"] = json!("agentctl-graph-1");
    for x in v["context"]["excerpts"].as_array_mut().unwrap() {
        x.as_object_mut().unwrap().remove("entity");
    }
    let source = v["request"]["source"].as_object_mut().unwrap();
    source.remove("graph_generation");
    source.insert("graph_version".into(), json!("agentctl-graph-1"));
    v
}

#[test]
fn pre_stage_one_planner_packets_stay_readable_but_cannot_seed_new_plans() {
    let f = Fixture::new(&[("src/lib.rs", "pub fn legacy_packet_target() {}\n".into())]);
    let p = f.prepare("Adjust the legacy packet target", PlanningLimits::default());
    let mut old = legacy(&p);
    let id = "request:00000000000000000000000000000001";
    old["request"]["request_id"] = json!(id);
    let info = RepositoryInfo::discover(&f.root).unwrap();
    common::sql(&f.db)
        .execute(
            "INSERT INTO planning_requests VALUES (?1,?2,?3,?4)",
            rusqlite::params![
                id,
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                old.to_string()
            ],
        )
        .unwrap();
    let request = PlanningRequestId::new(id).unwrap();
    let read = f.store().planning_context(&f.root, &request).unwrap();
    assert_eq!(
        read.context.graph.primary[0].entity.provenance,
        p.context.graph.primary[0].entity.provenance
    );
    assert!(read.request.source.graph_generation.is_none());
    assert!(read.context.excerpts.iter().all(|x| x.entity.is_none()));
    assert!(manifest::for_packet(&read).is_ok());
    let entity = p.context.graph.primary[0].entity.id.clone();
    let error = f
        .store()
        .import_execution_plan(&f.root, &plan_for(&read, entity))
        .unwrap_err()
        .to_string();
    assert!(error.contains("version"), "{error}");
}

#[test]
fn v11_to_v12_migration_is_lossless_and_derives_resolution_immediately() {
    let f = Fixture::new(&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n".into()),
        ("src/a.rs", "pub fn capture() {}\n".into()),
        ("src/b.rs", "pub fn run() { crate::a::capture(); }\n".into()),
    ]);
    let before = f.store().index_status(&f.root).unwrap();
    let rows = |c: &rusqlite::Connection| -> Vec<String> {
        c.prepare("SELECT record_json FROM graph_entities UNION ALL SELECT record_json FROM graph_edges ORDER BY 1")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let c = common::sql(&f.db);
    let payloads = rows(&c);
    common::strip_graph_resolutions(&c);
    c.pragma_update(None, "user_version", 11).unwrap();
    drop(c);
    assert_eq!(
        f.store().status().unwrap().schema_version,
        agentctl::local::store::DATABASE_VERSION
    );
    assert_eq!(rows(&common::sql(&f.db)), payloads);
    // The facts did not change: still fresh, same generation, and the
    // cross-file relation is resolved without a reindex.
    let migrated = f.store().index_status(&f.root).unwrap();
    assert!(migrated.fresh);
    assert_eq!(migrated.generation(), before.generation());
    let resolution = |f: &Fixture| {
        f.store()
            .graph(&f.root)
            .unwrap()
            .relations("run", false, Some(RelationKind::Calls), 5)
            .unwrap()
            .data[0]
            .resolution
    };
    assert_eq!(resolution(&f), Some(ResolutionRule::QualifiedPath));
    let next = f.store().index_repository(&f.root).unwrap();
    // Schema 13 leaves pre-snapshot entities without body hashes, so this pass
    // re-derives both files once; the facts and the generation do not change.
    assert_eq!(
        (next.indexed, next.changed, next.resolved),
        (next.discovered, next.discovered, 1)
    );
    assert_eq!(next.generation.as_ref(), before.generation());
    assert_eq!(resolution(&f), Some(ResolutionRule::QualifiedPath));
    assert!(
        common::sql(&f.db)
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

/// Issue #3 regression on this repository itself: the realistic request, with
/// no handcrafted query, must reach the runtime source-capture implementation,
/// the Git classification machinery it uses, and the regression tests, with an
/// implementation excerpt inside the default 32 KiB budget.
#[test]
fn issue_three_request_reaches_the_capture_implementation_in_this_repository() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Ok(info) = RepositoryInfo::discover(&root) else {
        eprintln!("skipping: {} is not a Git checkout", root.display());
        return;
    };
    let temp = TempDir::new();
    let db = temp.0.join("state.sqlite3");
    let mut store = Store::open(&db, 5000).unwrap();
    store.register_repository(info).unwrap();
    assert_eq!(store.index_repository(&root).unwrap().failed, 0);
    let mut request = intent(ISSUE_THREE);
    request.verification = Some(VerificationRequirements {
        requirement_refs: vec!["test".into()],
        evidence_required: true,
    });
    let p = store
        .prepare_plan(&root, request, PlanningLimits::default())
        .unwrap();
    assert!(p.request.intent.query.is_none());
    assert!(p.serialized_bytes <= 32768);
    let g = &p.context.graph;
    assert!(
        g.primary.iter().all(|e| !test_side(&e.entity)),
        "tests took primary"
    );
    let implementation: Vec<_> = g
        .primary
        .iter()
        .map(|e| &e.entity)
        .chain(&g.neighbors)
        .collect();
    let named = |path: &str, name: &str| {
        implementation
            .iter()
            .any(|e| e.provenance.path == path && e.name == name)
    };
    assert!(named("src/local/runtime/source.rs", "capture"));
    assert!(named("src/local/runtime/source.rs", "collect"));
    assert!(
        implementation
            .iter()
            .any(|e| e.provenance.path == "src/local/repository.rs")
    );
    assert!(g.tests.iter().any(|t| {
        t.kind == EntityKind::Test
            && (t.provenance.path == "tests/runtime.rs"
                || t.qualified_name
                    .starts_with("src::local::runtime::source::tests"))
            && (t.name.contains("ignored") || t.name.contains("capture"))
    }));
    let primary: BTreeSet<_> = g.primary.iter().map(|e| e.entity.id.clone()).collect();
    assert!(p.context.excerpts.iter().any(|x| {
        x.provenance.path == "src/local/runtime/source.rs"
            && x.entity.as_ref().is_some_and(|id| primary.contains(id))
    }));
    assert!(g.relations.iter().all(|r| r.target.is_some()));
    assert!(size(&g.unresolved) * 10 < p.serialized_bytes);
    let m = manifest::for_packet(&p).unwrap();
    assert_eq!(m.bytes.total, p.serialized_bytes);
    assert!(
        m.paths
            .iter()
            .any(|s| s.path == "src/local/runtime/source.rs"
                && s.kind == manifest::SuppliedKind::Excerpt)
    );
    let symbol = store
        .graph(&root)
        .unwrap()
        .symbols("src::local::runtime::source::capture", SearchMode::Exact, 1)
        .unwrap()
        .data;
    assert_eq!(symbol.len(), 1);
}
