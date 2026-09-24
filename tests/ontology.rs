//! Ontology generation lifecycle and semantic deltas, over real Git
//! repositories and the real index path. Oracles are the edits each test
//! makes (which declarations it added, removed or changed), not the delta
//! implementation's own bookkeeping.
#[allow(dead_code)]
mod common;
use agentctl::{
    local::{
        config::ProjectConfig,
        graph::{
            Change, ContentChange, DecisionReason, DeltaStatus, EntityChange, EntityField,
            GenerationOrigin, GenerationState, IdentityBasis, RelationKind, SemanticDelta,
        },
        planning::{PlannerPacket, PlanningLimits, PlanningProvenance, RequestDraft},
        repository::RepositoryInfo,
        store::{JournalEntry, Store},
    },
    protocol::ProtocolVersion,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const A: &str = "pub fn capture() -> u32 {\n    1\n}\n";
const B: &str = "pub fn run() -> u32 {\n    crate::a::capture()\n}\n\npub fn idle() {}\n";

struct Fixture {
    temp: common::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    /// A registered workspace whose first complete index bootstraps the
    /// accepted generation.
    fn new(files: &[(&str, &str)]) -> Self {
        let f = Self::unindexed(files);
        f.index();
        f
    }
    fn unindexed(files: &[(&str, &str)]) -> Self {
        let temp = common::TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        ProjectConfig::initialize(&root).unwrap();
        for (path, text) in files {
            write(&root, path, text);
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "baseline"]);
        let db = temp.0.join("data/agentctl/state.sqlite3");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        Store::open(&db, 5000)
            .unwrap()
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        Self { temp, root, db }
    }
    fn store(&self) -> Store {
        Store::open(&self.db, 5000).unwrap()
    }
    fn index(&self) {
        self.store().index_repository(&self.root).unwrap();
    }
    fn write(&self, path: &str, text: &str) {
        write(&self.root, path, text);
    }
    fn accepted(&self) -> agentctl::local::graph::OntologyGeneration {
        self.store()
            .ontology_status(&self.root)
            .unwrap()
            .accepted
            .expect("an accepted generation")
    }
    fn candidate(&self) -> String {
        self.store()
            .ontology_status(&self.root)
            .unwrap()
            .candidate
            .expect("an open candidate")
            .generation_id
    }
    fn states(&self) -> Vec<(u64, GenerationState)> {
        let mut v: Vec<_> = self
            .store()
            .ontology_generations(&self.root, 100)
            .unwrap()
            .into_iter()
            .map(|g| (g.ordinal, g.state))
            .collect();
        v.reverse();
        v
    }
    /// Apply edits (None deletes), reindex, and return the new candidate's delta.
    fn observe(&self, edits: &[(&str, Option<&str>)]) -> SemanticDelta {
        for (path, text) in edits {
            match text {
                Some(text) => self.write(path, text),
                None => fs::remove_file(self.root.join(path)).unwrap(),
            }
        }
        self.index();
        self.store()
            .ontology_delta(&self.root, &self.candidate())
            .unwrap()
    }
    fn prepare(&self) -> agentctl::local::Result<PlannerPacket> {
        self.store().prepare_plan(
            &self.root,
            RequestDraft {
                objective: "Adjust capture behavior".into(),
                query: Some("capture".into()),
                scope: vec![],
                constraints: vec![],
                definition_of_done: vec!["capture works".into()],
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
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agentctl"))
            .current_dir(&self.root)
            .args(args)
            .env("HOME", self.temp.0.join("home"))
            .env("XDG_CONFIG_HOME", self.temp.0.join("config"))
            .env("XDG_DATA_HOME", self.temp.0.join("data"))
            .env("XDG_CACHE_HOME", self.temp.0.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    }
    fn cli_json(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn journal_transitions(&self) -> Vec<(String, GenerationState, Option<DecisionReason>)> {
        self.store()
            .events(None, None, None, 1000)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.entry {
                JournalEntry::OntologyGenerationChanged {
                    generation_id,
                    state,
                    reason,
                } => Some((generation_id, state, reason)),
                _ => None,
            })
            .collect()
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Ontology Test",
            "-c",
            "user.email=ontology@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `(change, qualified_name)` pairs, the oracle shape for entity changes.
fn entities(delta: &SemanticDelta) -> BTreeSet<(Change, String)> {
    delta
        .entities
        .iter()
        .map(|e| (e.change, e.qualified_name.clone()))
        .collect()
}

fn named<'d>(delta: &'d SemanticDelta, name: &str) -> Vec<&'d EntityChange> {
    delta
        .entities
        .iter()
        .filter(|e| e.qualified_name == name)
        .collect()
}

fn set(items: &[(Change, &str)]) -> BTreeSet<(Change, String)> {
    items.iter().map(|(c, n)| (*c, (*n).to_string())).collect()
}

// ---------------------------------------------------------------------------
// Semantic delta and cross-generation identity (Parts B, C, J).
// ---------------------------------------------------------------------------

#[test]
fn unchanged_reindex_records_nothing_and_first_complete_index_bootstraps() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let accepted = f.accepted();
    assert_eq!(accepted.state, GenerationState::Accepted);
    assert_eq!(accepted.ordinal, 1);
    assert_eq!(accepted.delta, DeltaStatus::NoBase);
    assert_eq!(
        accepted.acceptance.as_ref().unwrap().reason,
        DecisionReason::Bootstrap
    );
    assert_eq!(accepted.origin, GenerationOrigin::External);
    for _ in 0..3 {
        f.index();
    }
    assert_eq!(f.states(), vec![(1, GenerationState::Accepted)]);
    let status = f.store().ontology_status(&f.root).unwrap();
    assert!(status.live_accepted);
    assert!(status.candidate.is_none());
    // A HEAD-only change (same file bytes) is not a semantic change either.
    git(
        &f.root,
        &["commit", "--quiet", "--allow-empty", "-m", "empty"],
    );
    f.index();
    assert_eq!(f.states().len(), 1);
    assert!(f.prepare().is_ok());
}

#[test]
fn a_body_edit_keeps_identity_and_is_the_only_change() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let delta = f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    2\n}\n"))]);
    assert_eq!(
        entities(&delta),
        set(&[(Change::Modified, "src::a::capture")])
    );
    let change = &delta.entities[0];
    assert_eq!(change.identity, IdentityBasis::Unique);
    assert_eq!(change.fields, vec![EntityField::Text]);
    let (before, after) = (
        change.before.as_ref().unwrap(),
        change.after.as_ref().unwrap(),
    );
    assert_eq!(before.id, after.id);
    assert_eq!(before.signature, after.signature);
    assert_ne!(before.text_hash, after.text_hash);
    assert!(delta.relations.is_empty());
    assert_eq!(delta.files.len(), 1);
    assert_eq!(delta.files[0].path, "src/a.rs");
    assert_eq!(delta.files[0].content, ContentChange::Modified);
    assert_eq!((delta.summary.files, delta.summary.semantic_files), (1, 1));
}

#[test]
fn a_signature_edit_is_reported_as_a_signature_change_of_the_same_entity() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let delta = f.observe(&[(
        "src/b.rs",
        Some(&B.replace("pub fn idle() {}", "pub fn idle(level: u8) {}")),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "src::b::idle")]));
    let change = &delta.entities[0];
    assert_eq!(
        change.fields,
        vec![EntityField::Signature, EntityField::Text]
    );
    assert_eq!(change.before.as_ref().unwrap().signature, "pub fn idle()");
    assert_eq!(
        change.after.as_ref().unwrap().signature,
        "pub fn idle(level: u8)"
    );
    // Visibility is a separate fact.
    let delta = f.observe(&[(
        "src/b.rs",
        Some(&B.replace("pub fn idle() {}", "fn idle(level: u8) {}")),
    )]);
    assert_eq!(
        named(&delta, "src::b::idle")[0].fields.first(),
        Some(&EntityField::Signature)
    );
    assert!(
        named(&delta, "src::b::idle")[0]
            .fields
            .contains(&EntityField::Visibility)
    );
}

#[test]
fn line_movement_is_not_a_change_and_never_becomes_remove_plus_add() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    // Blank lines inserted above every declaration: all ranges move, nothing else.
    let delta = f.observe(&[("src/b.rs", Some(&format!("\n\n\n{B}")))]);
    assert!(delta.entities.is_empty(), "{:?}", delta.entities);
    assert!(delta.relations.is_empty());
    assert_eq!(delta.files[0].content, ContentChange::Modified);
    assert_eq!(delta.summary.semantic_files, 0);
    // Lines inserted inside `run` move `idle`; only `run` changed.
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let delta = f.observe(&[(
        "src/b.rs",
        Some(&B.replace(
            "    crate::a::capture()",
            "    // why\n    // more\n    crate::a::capture()",
        )),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "src::b::run")]));
    assert!(delta.relations.is_empty());
}

#[test]
fn doc_comments_and_attributes_belong_to_the_declaration_below_them() {
    let src = "use std::fmt;\n\n/// Runs.\npub fn run() {}\n\n#[inline]\npub fn idle() {}\n";
    let f = Fixture::new(&[("src/d.rs", src)]);
    // Only a doc comment changes: the documented function changed, not its module.
    let delta = f.observe(&[(
        "src/d.rs",
        Some(&src.replace("/// Runs.", "/// Runs quickly.")),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "src::d::run")]));
    assert_eq!(delta.entities[0].fields, vec![EntityField::Text]);
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    // Only an attribute changes.
    let src = src.replace("/// Runs.", "/// Runs quickly.");
    let delta = f.observe(&[(
        "src/d.rs",
        Some(&src.replace("#[inline]", "#[inline(always)]")),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "src::d::idle")]));
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    // Module-level text that belongs to no declaration is the module's own.
    let src = src.replace("#[inline]", "#[inline(always)]");
    let delta = f.observe(&[(
        "src/d.rs",
        Some(&src.replace("use std::fmt;", "use std::io;")),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "src::d")]));
    // A new documented function adds exactly that function.
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    let src = src.replace("use std::fmt;", "use std::io;");
    let delta = f.observe(&[(
        "src/d.rs",
        Some(&format!(
            "{src}\n/// New.\n#[must_use]\npub fn fresh() -> u8 {{ 0 }}\n"
        )),
    )]);
    assert_eq!(entities(&delta), set(&[(Change::Added, "src::d::fresh")]));
}

#[test]
fn added_and_removed_declarations_and_their_relations() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let added = format!("{B}\npub fn extra() -> u32 {{\n    run()\n}}\n");
    let delta = f.observe(&[("src/b.rs", Some(&added))]);
    assert_eq!(entities(&delta), set(&[(Change::Added, "src::b::extra")]));
    assert_eq!(delta.entities[0].identity, IdentityBasis::Unique);
    assert!(delta.entities[0].before.is_none() && delta.entities[0].after.is_some());
    assert_eq!(delta.relations.len(), 1);
    let r = &delta.relations[0];
    assert_eq!((r.change, r.kind), (Change::Added, RelationKind::Calls));
    assert_eq!(r.source, delta.entities[0].id);
    assert_eq!(
        (r.source_path.as_str(), r.target_path.as_str()),
        ("src/b.rs", "src/b.rs")
    );
    // Accept, then remove it again.
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    let delta = f.observe(&[("src/b.rs", Some(B))]);
    assert_eq!(entities(&delta), set(&[(Change::Removed, "src::b::extra")]));
    assert!(delta.entities[0].before.is_some() && delta.entities[0].after.is_none());
    assert_eq!(delta.relations.len(), 1);
    assert_eq!(delta.relations[0].change, Change::Removed);
}

#[test]
fn a_cross_file_relation_appears_and_disappears_and_moves_with_the_resolution() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let calls = |d: &SemanticDelta| -> BTreeSet<(Change, String, String)> {
        d.relations
            .iter()
            .map(|r| (r.change, r.source_path.clone(), r.target_path.clone()))
            .collect()
    };
    // Removing the qualified call removes exactly one cross-file relation.
    let delta = f.observe(&[("src/b.rs", Some(&B.replace("crate::a::capture()", "7")))]);
    assert_eq!(
        calls(&delta),
        BTreeSet::from([(Change::Removed, "src/b.rs".into(), "src/a.rs".into())])
    );
    assert_eq!(
        delta.relations[0].rules,
        vec![agentctl::local::graph::ResolutionRule::QualifiedPath]
    );
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    let delta = f.observe(&[("src/b.rs", Some(B))]);
    assert_eq!(
        calls(&delta),
        BTreeSet::from([(Change::Added, "src/b.rs".into(), "src/a.rs".into())])
    );
}

#[test]
fn a_relation_can_disappear_from_a_file_whose_bytes_did_not_change() {
    // `helpers::go()` resolves by unique key suffix. A second `helpers::go`
    // elsewhere makes it ambiguous, so the index abstains, and the delta must
    // say the relation disappeared from main.rs even though main.rs is untouched.
    let main = "mod helpers;\npub fn start() {\n    helpers::go();\n}\n";
    let f = Fixture::new(&[
        ("app/src/main.rs", main),
        ("app/src/helpers.rs", "pub fn go() {}\n"),
    ]);
    // A second `helpers::go` inside the same crate makes the suffix ambiguous.
    let before = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("start", false, Some(RelationKind::Calls), 5)
        .unwrap();
    assert!(
        before.data[0].target.is_some(),
        "fixture must resolve first"
    );
    let delta = f.observe(&[("app/src/other/helpers.rs", Some("pub fn go() {}\n"))]);
    // A new file adds its file, file-scope module and function entities only.
    assert_eq!(
        entities(&delta),
        set(&[
            (Change::Added, "app/src/other/helpers.rs"),
            (Change::Added, "app::src::other::helpers"),
            (Change::Added, "app::src::other::helpers::go"),
        ])
    );
    assert_eq!(delta.relations.len(), 1);
    let r = &delta.relations[0];
    assert_eq!(r.change, Change::Removed);
    assert_eq!(r.source_path, "app/src/main.rs");
    let main_file = delta
        .files
        .iter()
        .find(|f| f.path == "app/src/main.rs")
        .unwrap();
    assert_eq!(main_file.content, ContentChange::Unchanged);
    assert_eq!((main_file.entities, main_file.relations), (0, 1));
    // Precision is preserved: the call was not retargeted to either `go`.
    assert!(!delta.relations.iter().any(|r| r.change == Change::Added));
}

#[test]
fn renames_moves_and_container_changes_abstain_as_removal_plus_addition() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    // Rename.
    let delta = f.observe(&[("src/b.rs", Some(&B.replace("idle", "rest")))]);
    assert_eq!(
        entities(&delta),
        set(&[
            (Change::Removed, "src::b::idle"),
            (Change::Added, "src::b::rest")
        ])
    );
    assert!(
        delta
            .entities
            .iter()
            .all(|e| e.identity == IdentityBasis::Unique)
    );
    assert!(delta.entities.iter().all(|e| e.change != Change::Modified));
    // Container change: the same function moved into an inline module.
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let nested = B.replace(
        "pub fn idle() {}",
        "pub mod quiet {\n    pub fn idle() {}\n}",
    );
    let delta = f.observe(&[("src/b.rs", Some(&nested))]);
    assert_eq!(
        entities(&delta),
        set(&[
            (Change::Removed, "src::b::idle"),
            (Change::Added, "src::b::quiet"),
            (Change::Added, "src::b::quiet::idle"),
        ])
    );
    // File move: every declaration of the file is removed and re-added under
    // its new path; the qualified call into it no longer resolves.
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    git(&f.root, &["mv", "src/a.rs", "src/z.rs"]);
    let delta = f.observe(&[]);
    let removed: Vec<_> = delta
        .entities
        .iter()
        .filter(|e| e.change == Change::Removed)
        .collect();
    let added: Vec<_> = delta
        .entities
        .iter()
        .filter(|e| e.change == Change::Added)
        .collect();
    assert!(removed.iter().all(|e| e.path == "src/a.rs") && !removed.is_empty());
    assert!(added.iter().all(|e| e.path == "src/z.rs") && added.len() == removed.len());
    assert!(delta.entities.iter().all(|e| e.change != Change::Modified));
    let paths: Vec<_> = delta
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.content))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("src/a.rs", ContentChange::Removed),
            ("src/b.rs", ContentChange::Unchanged),
            ("src/z.rs", ContentChange::Added),
        ]
    );
    assert_eq!(delta.relations.len(), 1);
    assert_eq!(delta.relations[0].change, Change::Removed);
}

#[test]
fn duplicate_declarations_never_fabricate_identity() {
    // Python permits redefinition: both `f`s share path, kind and name, so
    // their entity IDs are ordinals in file order.
    let two = "def g():\n    return 0\n\ndef f():\n    return 1\n\ndef f():\n    return 2\n";
    let f = Fixture::new(&[("pkg/m.py", two)]);
    // Deleting the first `f`: the survivor now carries ordinal 0, the ID the
    // deleted one had. An ID diff would call that a modification; it is not.
    let delta = f.observe(&[(
        "pkg/m.py",
        Some("def g():\n    return 0\n\ndef f():\n    return 2\n"),
    )]);
    let fs_ = named(&delta, "pkg::m::f");
    assert!(
        fs_.iter()
            .all(|e| e.identity == IdentityBasis::DuplicateOrdinal)
    );
    assert!(fs_.iter().all(|e| e.change != Change::Modified), "{fs_:?}");
    assert_eq!(
        fs_.iter().filter(|e| e.change == Change::Removed).count(),
        2
    );
    assert_eq!(fs_.iter().filter(|e| e.change == Change::Added).count(), 1);
    assert!(named(&delta, "pkg::m::g").is_empty());
    assert_eq!(delta.summary.unproven_identity, 3);
    // An edit elsewhere leaves an unchanged duplicate group unreported.
    let f = Fixture::new(&[("pkg/m.py", two)]);
    let delta = f.observe(&[("pkg/m.py", Some(&two.replace("return 0", "return 9")))]);
    assert_eq!(entities(&delta), set(&[(Change::Modified, "pkg::m::g")]));
    // Relations whose endpoint is an unproven duplicate are reported as
    // removal plus addition, never as unchanged.
    let calling =
        "def g():\n    return 0\n\ndef f():\n    return g()\n\ndef f():\n    return g()\n";
    let f = Fixture::new(&[("pkg/m.py", calling)]);
    let delta = f.observe(&[(
        "pkg/m.py",
        Some(
            "def g():\n    return 0\n\ndef f():\n    return g() + 1\n\ndef f():\n    return g()\n",
        ),
    )]);
    assert!(!delta.relations.is_empty());
    assert!(
        delta
            .relations
            .iter()
            .all(|r| r.identity == IdentityBasis::DuplicateOrdinal)
    );
    let removed = delta
        .relations
        .iter()
        .filter(|r| r.change == Change::Removed)
        .count();
    let added = delta
        .relations
        .iter()
        .filter(|r| r.change == Change::Added)
        .count();
    assert_eq!((removed, added), (2, 2));
}

#[test]
fn same_named_methods_on_different_types_are_distinct_entities() {
    let src = "pub struct A;\npub struct B;\nimpl A {\n    pub fn new() -> Self { A }\n}\nimpl B {\n    pub fn new() -> Self { B }\n}\n";
    let f = Fixture::new(&[("src/t.rs", src)]);
    let delta = f.observe(&[(
        "src/t.rs",
        Some(&src.replace("-> Self { B }", "-> Self { let b = B; b }")),
    )]);
    let changed: Vec<_> = delta
        .entities
        .iter()
        .map(|e| (e.change, e.qualified_name.as_str(), e.identity))
        .collect();
    assert_eq!(
        changed,
        vec![(
            Change::Modified,
            "src::t::impl B::new",
            IdentityBasis::Unique
        )]
    );
}

#[test]
fn added_and_deleted_files_are_whole_file_additions_and_removals() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let delta = f.observe(&[("src/c.rs", Some("pub fn fresh() {}\n"))]);
    assert_eq!(delta.files.len(), 1);
    assert_eq!(delta.files[0].content, ContentChange::Added);
    assert!(
        delta
            .entities
            .iter()
            .all(|e| e.change == Change::Added && e.path == "src/c.rs")
    );
    assert!(entities(&delta).contains(&(Change::Added, "src::c::fresh".into())));
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    let delta = f.observe(&[("src/c.rs", None)]);
    assert_eq!(delta.files[0].content, ContentChange::Removed);
    assert!(
        delta
            .entities
            .iter()
            .all(|e| e.change == Change::Removed && e.path == "src/c.rs")
    );
}

#[test]
fn deltas_are_deterministic_roundtrip_and_reject_unknown_shapes() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let base = f.accepted().generation_id;
    let recorded = f.observe(&[
        (
            "src/a.rs",
            Some("pub fn capture() -> u32 {\n    3\n}\npub fn more() {}\n"),
        ),
        ("src/b.rs", Some(&B.replace("idle", "rest"))),
    ]);
    let candidate = f.candidate();
    let on_demand = f.store().ontology_diff(&f.root, &base, &candidate).unwrap();
    assert_eq!(recorded, on_demand);
    let bytes = serde_json::to_vec(&recorded).unwrap();
    assert_eq!(
        bytes,
        serde_json::to_vec(&f.store().ontology_diff(&f.root, &base, &candidate).unwrap()).unwrap()
    );
    let back: SemanticDelta = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(back, recorded);
    assert_eq!(recorded.version, ProtocolVersion::V1);
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    value["surprise"] = Value::Bool(true);
    assert!(serde_json::from_value::<SemanticDelta>(value.clone()).is_err());
    value.as_object_mut().unwrap().remove("surprise");
    value["version"] = Value::String("2".into());
    assert!(serde_json::from_value::<SemanticDelta>(value).is_err());
    // The reverse direction is the exact mirror.
    let reverse = f.store().ontology_diff(&f.root, &candidate, &base).unwrap();
    let mirror = |c: Change| match c {
        Change::Added => Change::Removed,
        Change::Removed => Change::Added,
        Change::Modified => Change::Modified,
    };
    assert_eq!(
        entities(&reverse),
        entities(&recorded)
            .into_iter()
            .map(|(c, n)| (mirror(c), n))
            .collect()
    );
    // A generation compared with itself is empty.
    assert!(
        f.store()
            .ontology_diff(&f.root, &base, &base)
            .unwrap()
            .is_empty()
    );
    // The CLI projection filters without changing the summary.
    let only_added = recorded.select(Some(Change::Added), None, 100);
    assert!(
        only_added
            .entities
            .iter()
            .all(|e| e.change == Change::Added)
    );
    assert_eq!(only_added.summary, recorded.summary);
    let only_b = recorded.select(None, Some("src/b.rs"), 100);
    assert!(only_b.entities.iter().all(|e| e.path == "src/b.rs"));
    assert_eq!(only_b.files.len(), 1);
}

// ---------------------------------------------------------------------------
// Lifecycle: external changes, acceptance, rejection, staleness (Parts A, F, G).
// ---------------------------------------------------------------------------

#[test]
fn an_external_edit_is_observed_but_never_accepted_until_explicitly_accepted() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let base = f.accepted();
    assert!(f.prepare().is_ok());
    let delta = f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    5\n}\n"))]);
    assert!(!delta.is_empty());
    let status = f.store().ontology_status(&f.root).unwrap();
    assert!(!status.live_accepted);
    assert_eq!(
        status.accepted.as_ref().unwrap(),
        &base,
        "accepted truth is untouched"
    );
    let candidate = status.candidate.unwrap();
    assert_eq!(candidate.base.as_deref(), Some(base.generation_id.as_str()));
    assert_eq!(candidate.origin, GenerationOrigin::External);
    let DeltaStatus::Recorded { summary, .. } = &candidate.delta else {
        panic!("{:?}", candidate.delta)
    };
    assert_eq!(summary, &delta.summary);
    // Planning refuses an unaccepted observation, however many times it is indexed.
    for _ in 0..2 {
        let error = f.prepare().unwrap_err().to_string();
        assert!(error.contains("not the accepted generation"), "{error}");
        f.index();
    }
    assert_eq!(
        f.states().len(),
        2,
        "re-indexing the same state records nothing"
    );
    // Deliberate acceptance.
    let accepted = f
        .store()
        .accept_generation(&f.root, &candidate.generation_id, Some("reviewed"))
        .unwrap();
    assert_eq!(accepted.state, GenerationState::Accepted);
    let decision = accepted.acceptance.as_ref().unwrap();
    assert_eq!(decision.reason, DecisionReason::ManualAcceptance);
    assert_eq!(decision.note.as_deref(), Some("reviewed"));
    assert_eq!(f.accepted().generation_id, candidate.generation_id);
    assert_eq!(
        f.states(),
        vec![
            (1, GenerationState::Retired),
            (2, GenerationState::Accepted)
        ]
    );
    assert!(f.prepare().is_ok());
    // History stays inspectable: the retired generation, its replacement
    // decision, and the delta between them.
    let old = f
        .store()
        .ontology_generation(&f.root, &base.generation_id)
        .unwrap();
    assert_eq!(
        old.closure.as_ref().unwrap().reason,
        DecisionReason::Replaced
    );
    assert_eq!(
        old.closure.as_ref().unwrap().by.as_deref(),
        Some(candidate.generation_id.as_str())
    );
    assert_eq!(old.acceptance, base.acceptance);
    assert_eq!(
        f.store()
            .ontology_delta(&f.root, &candidate.generation_id)
            .unwrap(),
        delta
    );
    assert!(
        f.store()
            .ontology_diff(&f.root, &base.generation_id, &candidate.generation_id)
            .is_ok()
    );
    // Repeated acceptance is idempotent and journals once.
    f.store()
        .accept_generation(&f.root, &candidate.generation_id, None)
        .unwrap();
    let accepts = f
        .journal_transitions()
        .into_iter()
        .filter(|(id, state, _)| {
            id == &candidate.generation_id && *state == GenerationState::Accepted
        })
        .count();
    assert_eq!(accepts, 1);
}

#[test]
fn stale_and_superseded_candidates_cannot_be_accepted() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    5\n}\n"))]);
    let first = f.candidate();
    // The worktree moves on without a reindex: the candidate no longer
    // describes it.
    f.write("src/a.rs", "pub fn capture() -> u32 {\n    6\n}\n");
    let error = f
        .store()
        .accept_generation(&f.root, &first, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no longer matches"), "{error}");
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &first)
            .unwrap()
            .state,
        GenerationState::Candidate
    );
    // Observing the new state supersedes the first candidate for good.
    f.index();
    let second = f.candidate();
    assert_ne!(first, second);
    let superseded = f.store().ontology_generation(&f.root, &first).unwrap();
    assert_eq!(superseded.state, GenerationState::Abandoned);
    assert_eq!(
        superseded.closure.as_ref().unwrap().reason,
        DecisionReason::Superseded
    );
    assert_eq!(
        superseded.closure.as_ref().unwrap().by.as_deref(),
        Some(second.as_str())
    );
    let error = f
        .store()
        .accept_generation(&f.root, &first, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("only an open CANDIDATE"), "{error}");
    // Even if the source returns to the first candidate's bytes, the first
    // record stays closed; the new observation is a new record.
    f.write("src/a.rs", "pub fn capture() -> u32 {\n    5\n}\n");
    f.index();
    let third = f.candidate();
    assert_ne!(third, first);
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &third)
            .unwrap()
            .generation
            .fingerprint,
        superseded.generation.fingerprint
    );
    assert!(f.store().accept_generation(&f.root, &first, None).is_err());
    assert!(f.store().accept_generation(&f.root, &third, None).is_ok());
    // An unknown or foreign ID fails closed.
    assert!(
        f.store()
            .accept_generation(&f.root, "gen:0000000000000000:1", None)
            .is_err()
    );
}

#[test]
fn rejection_is_durable_idempotent_and_a_reobservation_reopens_a_decision() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    5\n}\n"))]);
    let candidate = f.candidate();
    assert!(
        f.store()
            .reject_generation(&f.root, &candidate, "  ")
            .is_err()
    );
    let rejected = f
        .store()
        .reject_generation(&f.root, &candidate, "not wanted")
        .unwrap();
    assert_eq!(rejected.state, GenerationState::Rejected);
    assert_eq!(
        rejected.closure.as_ref().unwrap().reason,
        DecisionReason::ManualRejection
    );
    assert!(
        f.store()
            .reject_generation(&f.root, &candidate, "again")
            .is_ok()
    );
    assert!(
        f.store()
            .accept_generation(&f.root, &candidate, None)
            .is_err()
    );
    // The live index still materializes the rejected facts: planning refuses.
    let status = f.store().ontology_status(&f.root).unwrap();
    assert!(!status.live_accepted);
    assert_eq!(status.observed.unwrap().state, GenerationState::Rejected);
    assert!(f.prepare().is_err());
    // An explicit new observation of the same state is a new, undecided record.
    f.index();
    let reopened = f.candidate();
    assert_ne!(reopened, candidate);
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &candidate)
            .unwrap()
            .state,
        GenerationState::Rejected,
        "rejection evidence is retained"
    );
    f.store()
        .accept_generation(&f.root, &reopened, None)
        .unwrap();
    assert!(f.prepare().is_ok());
}

#[test]
fn a_revert_returns_the_fingerprint_but_not_the_lifecycle_position() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    let original = f.accepted();
    let packet = f.prepare().unwrap();
    f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    5\n}\n"))]);
    let edited = f.candidate();
    f.write("src/a.rs", A);
    f.index();
    let now = f.accepted();
    // Same facts as the original, so it is accepted mechanically...
    assert_eq!(now.generation.fingerprint, original.generation.fingerprint);
    assert_eq!(
        now.acceptance.as_ref().unwrap().reason,
        DecisionReason::IdenticalToAccepted
    );
    let DeltaStatus::Recorded { summary, .. } = &now.delta else {
        panic!("{:?}", now.delta)
    };
    assert_eq!(summary, &Default::default());
    // ...but it is a new position in the lifecycle, distinct from the old one.
    assert_ne!(now.generation_id, original.generation_id);
    assert_eq!(now.ordinal, 3);
    assert!(now.generation.sequence > original.generation.sequence);
    assert_eq!(
        f.states(),
        vec![
            (1, GenerationState::Retired),
            (2, GenerationState::Abandoned),
            (3, GenerationState::Accepted),
        ]
    );
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &edited)
            .unwrap()
            .closure
            .unwrap()
            .reason,
        DecisionReason::Superseded
    );
    // The old packet names the old occurrence; a fresh one names the new.
    assert_eq!(
        packet.request.source.graph_generation.as_ref(),
        Some(&original.generation)
    );
    assert_eq!(
        f.prepare()
            .unwrap()
            .request
            .source
            .graph_generation
            .as_ref(),
        Some(&now.generation)
    );
}

#[test]
fn a_partial_observation_is_never_bootstrapped_or_accepted() {
    let huge = format!(
        "pub fn big() {{}}\n//{}\n",
        "x".repeat(2 * 1024 * 1024 + 16)
    );
    let f = Fixture::unindexed(&[("src/a.rs", A), ("src/huge.rs", &huge)]);
    assert!(f.store().index_repository(&f.root).is_ok());
    let status = f.store().ontology_status(&f.root).unwrap();
    assert!(status.accepted.is_none());
    let candidate = status.candidate.unwrap();
    assert_eq!(candidate.failed_files, 1);
    assert_eq!(candidate.delta, DeltaStatus::NoBase);
    assert!(
        f.store()
            .accept_generation(&f.root, &candidate.generation_id, None)
            .is_err()
    );
    assert!(f.prepare().is_err());
    // Fixing the failure yields a complete observation, which bootstraps.
    fs::remove_file(f.root.join("src/huge.rs")).unwrap();
    f.index();
    let accepted = f.accepted();
    assert_eq!(
        accepted.acceptance.unwrap().reason,
        DecisionReason::Bootstrap
    );
    assert_eq!(
        f.store()
            .ontology_generation(&f.root, &candidate.generation_id)
            .unwrap()
            .state,
        GenerationState::Abandoned
    );
}

// ---------------------------------------------------------------------------
// Persistence: guards, corruption, migration (Parts K, L).
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_rows_and_artifacts_are_guarded_against_out_of_band_changes() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
    // One row in each of RETIRED, ACCEPTED and CANDIDATE.
    f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    5\n}\n"))]);
    f.store()
        .accept_generation(&f.root, &f.candidate(), None)
        .unwrap();
    f.observe(&[("src/a.rs", Some("pub fn capture() -> u32 {\n    6\n}\n"))]);
    assert_eq!(
        f.states(),
        vec![
            (1, GenerationState::Retired),
            (2, GenerationState::Accepted),
            (3, GenerationState::Candidate),
        ]
    );
    let c = common::sql(&f.db);
    for sql in [
        "UPDATE ontology_generations SET state='CANDIDATE' WHERE state='ACCEPTED'",
        "UPDATE ontology_generations SET state='REJECTED' WHERE state='ACCEPTED'",
        "UPDATE ontology_generations SET state='CANDIDATE' WHERE state='RETIRED'",
        "UPDATE ontology_generations SET state='ACCEPTED' WHERE state='RETIRED'",
        "UPDATE ontology_generations SET fingerprint='x' WHERE state='CANDIDATE'",
        "DELETE FROM ontology_generations",
        "UPDATE ontology_blobs SET body='{}'",
        "DELETE FROM ontology_blobs",
        "INSERT INTO ontology_generations SELECT 'gen:x:9',repo_id,workspace_id,9,sequence,fingerprint,snapshot,plan_id,'REJECTED',record_json FROM ontology_generations LIMIT 1",
        // A second ACCEPTED row violates the canonical-pointer index.
        "INSERT INTO ontology_generations SELECT 'gen:x:9',repo_id,workspace_id,9,sequence,fingerprint,snapshot,plan_id,'ACCEPTED',record_json FROM ontology_generations LIMIT 1",
    ] {
        assert!(c.execute(sql, []).is_err(), "{sql}");
    }
    // Out-of-band corruption (guard removed first) is detected on read.
    let candidate = f.candidate();
    c.execute_batch(
        "DROP TRIGGER ontology_blobs_no_update;
         UPDATE ontology_blobs SET body=body || ' ' WHERE hash=(SELECT json_extract(record_json,'$.delta.artifact.hash') FROM ontology_generations WHERE state='CANDIDATE');
         CREATE TRIGGER ontology_blobs_no_update BEFORE UPDATE ON ontology_blobs
         BEGIN SELECT RAISE(ABORT, 'ontology artifacts are immutable'); END;",
    )
    .unwrap();
    let error = f
        .store()
        .ontology_delta(&f.root, &candidate)
        .unwrap_err()
        .to_string();
    assert!(error.contains("corrupt"), "{error}");
}

#[test]
fn a_pre_lifecycle_database_migrates_losslessly_and_bootstraps_on_the_next_index() {
    let f = Fixture::new(&[("src/a.rs", A), ("src/b.rs", B)]);
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
    common::strip_ontology_lifecycle(&c);
    c.pragma_update(None, "user_version", 12).unwrap();
    drop(c);
    // Reopen migrates to 13 without touching facts or synthesizing history.
    assert_eq!(
        f.store().status().unwrap().schema_version,
        agentctl::local::store::DATABASE_VERSION
    );
    let c = common::sql(&f.db);
    assert_eq!(rows(&c), payloads);
    let unhashed: i64 = c
        .query_row(
            "SELECT count(*) FROM graph_entities WHERE text_hash IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(unhashed > 0);
    let generations: i64 = c
        .query_row("SELECT count(*) FROM ontology_generations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(generations, 0);
    let migrated = f.store().index_status(&f.root).unwrap();
    assert!(migrated.fresh);
    assert_eq!(migrated.generation(), before.generation());
    // Legacy facts are readable, but planning needs an accepted generation.
    assert!(f.store().graph(&f.root).is_ok());
    let error = f.prepare().unwrap_err().to_string();
    assert!(error.contains("no accepted ontology generation"), "{error}");
    // One index pass re-derives the legacy rows and bootstraps.
    let stats = f.store().index_repository(&f.root).unwrap();
    assert_eq!(stats.indexed, stats.discovered);
    assert_eq!(stats.generation.as_ref(), before.generation());
    assert_eq!(rows(&common::sql(&f.db)), payloads);
    assert_eq!(
        f.accepted().acceptance.unwrap().reason,
        DecisionReason::Bootstrap
    );
    assert!(f.prepare().is_ok());
    let c = common::sql(&f.db);
    assert!(
        c.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    // Reopening again is a no-op.
    drop(c);
    assert_eq!(
        f.store().status().unwrap().schema_version,
        agentctl::local::store::DATABASE_VERSION
    );
    assert_eq!(f.states().len(), 1);
}

#[test]
fn a_database_with_a_missing_lifecycle_guard_refuses_to_open() {
    let f = Fixture::new(&[("src/a.rs", A)]);
    common::sql(&f.db)
        .execute_batch("DROP TRIGGER ontology_generations_update;")
        .unwrap();
    let error = Store::open(&f.db, 5000).err().unwrap().to_string();
    assert!(error.contains("ontology_generations_update"), "{error}");
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

#[test]
fn the_ontology_cli_observes_inspects_and_accepts() {
    let f = Fixture::unindexed(&[("src/a.rs", A), ("src/b.rs", B)]);
    f.cli_json(&["init", "--json"]);
    f.cli_json(&["repo", "init", "--json"]);
    let index = f.cli(&["repo", "index"]);
    assert!(index.status.success());
    assert!(
        String::from_utf8_lossy(&index.stdout)
            .contains("Indexed generation is the accepted generation")
    );
    let status = f.cli_json(&["ontology", "status", "--json"]);
    assert_eq!(status["live_accepted"], true);
    assert_eq!(status["accepted"]["acceptance"]["reason"], "BOOTSTRAP");
    let base = status["accepted"]["generation_id"]
        .as_str()
        .unwrap()
        .to_string();
    // No candidate yet.
    assert!(!f.cli(&["ontology", "delta", "--json"]).status.success());
    f.write("src/b.rs", &format!("{B}pub fn extra() {{}}\n"));
    let index = f.cli(&["repo", "index"]);
    let text = String::from_utf8_lossy(&index.stdout).to_string();
    assert!(
        text.contains("NOT accepted") && text.contains("agentctl ontology accept"),
        "{text}"
    );
    let status = f.cli_json(&["ontology", "status", "--json"]);
    assert_eq!(status["live_accepted"], false);
    let id = status["candidate"]["generation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let delta = f.cli_json(&["ontology", "delta", "--json"]);
    assert_eq!(delta["summary"]["entities_added"], 1);
    assert_eq!(delta["entities"][0]["qualified_name"], "src::b::extra");
    assert_eq!(delta["from"]["generation_id"], base.as_str());
    let filtered = f.cli_json(&["ontology", "delta", &id, "--change", "REMOVED", "--json"]);
    assert_eq!(filtered["entities"], serde_json::json!([]));
    let between = f.cli_json(&["ontology", "delta", "--from", &id, "--to", &base, "--json"]);
    assert_eq!(between["summary"]["entities_removed"], 1);
    let human = f.cli(&["ontology", "delta", &id]);
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(
        human.contains("entity    ADDED      Function src::b::extra  src/b.rs"),
        "{human}"
    );
    assert!(
        !f.cli(&["ontology", "delta", "--change", "RENAMED", "--json"])
            .status
            .success()
    );
    assert!(
        !f.cli(&["ontology", "delta", "--bogus", "1"])
            .status
            .success()
    );
    assert!(
        !f.cli(&["ontology", "reject", &id]).status.success(),
        "reject needs --reason"
    );
    assert_eq!(
        f.cli_json(&["ontology", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.cli_json(&["ontology", "show", &id, "--json"])["state"],
        "CANDIDATE"
    );
    let accepted = f.cli_json(&["ontology", "accept", &id, "--reason", "reviewed", "--json"]);
    assert_eq!(accepted["state"], "ACCEPTED");
    assert_eq!(
        f.cli_json(&["ontology", "show", &base, "--json"])["state"],
        "RETIRED"
    );
    assert!(f.cli(&["ontology", "status"]).status.success());
}
