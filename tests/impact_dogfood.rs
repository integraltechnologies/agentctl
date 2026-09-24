//! Impact-analysis dogfood over agentctl's own source tree.
//!
//! A realistic contract change is made to a helper whose important dependents
//! live in a dozen other files, and the observed delta is analyzed. Every
//! measurement is a byte or record count taken from the artifacts themselves;
//! nothing is estimated. Run with `--nocapture` to read them.
#[allow(dead_code)]
mod common;
use agentctl::local::{
    config::ProjectConfig,
    graph::{ContextLimits, ImpactClass, ImpactLimits, ImpactRequest},
    repository::RepositoryInfo,
    store::Store,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// A private helper every workspace-scoped Store entry point calls. Its
/// callers are spread across memory, planning, runtime and graph modules.
const TARGET: &str = "src/local/graph/mod.rs";
const BEFORE: &str =
    "pub(super) fn checked_workspace(store: &Store, start: &Path) -> Result<RepositoryInfo> {";
/// A contract change: one more required argument.
const AFTER: &str = "pub(super) fn checked_workspace(store: &Store, start: &Path, strict: bool) -> Result<RepositoryInfo> {";

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if source.is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&source, &target);
        } else {
            fs::copy(&source, &target).unwrap();
        }
    }
}

#[test]
fn impact_dogfood_over_agentctls_own_source() {
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
    ProjectConfig::initialize(&root).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "agentctl sources"]);
    let db = temp.0.join("state.sqlite3");
    let mut store = Store::open(&db, 5000).unwrap();
    store
        .register_repository(RepositoryInfo::discover(&root).unwrap())
        .unwrap();
    store.index_repository(&root).unwrap();

    // A realistic signature change in one file.
    let source = fs::read_to_string(root.join(TARGET)).unwrap();
    assert!(source.contains(BEFORE), "the dogfood target moved");
    fs::write(root.join(TARGET), source.replace(BEFORE, AFTER)).unwrap();
    store.index_repository(&root).unwrap();
    let candidate = store
        .ontology_status(&root)
        .unwrap()
        .candidate
        .expect("the edit is an open candidate")
        .generation_id;

    let limits = ImpactLimits {
        depth: 2,
        items: 64,
        ..ImpactLimits::default()
    };
    let report = store
        .ontology_impact(&root, &ImpactRequest::Generation(candidate.clone()), limits)
        .unwrap();
    let report_bytes = serde_json::to_vec(&report).unwrap().len();

    let seed_files: BTreeSet<&str> = report.seeds.iter().map(|s| s.path.as_str()).collect();
    let impacted_files: BTreeSet<&str> = report.items.iter().map(|i| i.path.as_str()).collect();
    let outside: Vec<&&str> = impacted_files
        .iter()
        .filter(|p| !seed_files.contains(**p))
        .collect();

    // Naive baseline: the whole graph neighborhood a "pull in what looks
    // related" strategy would expand into, at the same depth and a bound
    // generous enough not to truncate it.
    let naive = store
        .graph(&root)
        .unwrap()
        .impact(
            "src::local::graph::mod::checked_workspace",
            ContextLimits {
                primary: 1,
                depth: 2,
                neighbors: 100,
                tests: 20,
            },
        )
        .unwrap();
    let naive_bytes = serde_json::to_vec(&naive).unwrap().len();
    // And the file-level expansion that same strategy implies: every entity of
    // every file that holds any dependent.
    let mut file_entities = 0usize;
    let mut file_entity_bytes = 0usize;
    for path in &impacted_files {
        let rows = store
            .graph(&root)
            .unwrap()
            .entities_in_file(path, 100)
            .unwrap();
        file_entities += rows.data.len();
        file_entity_bytes += serde_json::to_vec(&rows.data).unwrap().len();
    }

    eprintln!("--- Impact dogfood: agentctl's own ontology ---");
    eprintln!(
        "seeds: {} ({} not traversed) in {:?}",
        report.summary.seeds, report.summary.seeds_skipped, seed_files
    );
    eprintln!(
        "items: {} ({} direct, {} contract, {} verification, {} containment, {} omitted) in {} files; max distance {}",
        report.summary.items,
        report.summary.direct,
        report.summary.contract,
        report.summary.verification,
        report.summary.containment,
        report.summary.items_omitted,
        report.summary.files,
        report.summary.max_distance
    );
    eprintln!(
        "cross-file items (invisible to file-local reasoning): {}; files outside the edited file: {:?}",
        report.summary.cross_file, outside
    );
    eprintln!(
        "boundaries: {} ({} omitted)",
        report.summary.boundaries, report.summary.boundaries_omitted
    );
    for boundary in report.boundaries.iter().take(4) {
        eprintln!(
            "  boundary {} {} {}",
            boundary.path,
            boundary.qualified_name,
            serde_json::to_string(&boundary.reason).unwrap()
        );
    }
    for item in report
        .items
        .iter()
        .filter(|i| i.class == ImpactClass::DirectDependency)
        .take(3)
    {
        eprintln!(
            "  evidence {} {} <- {} hops",
            item.path,
            item.qualified_name,
            item.evidence.len()
        );
    }
    eprintln!(
        "bytes: impact report {report_bytes}; naive 2-hop neighborhood packet {naive_bytes}; \
         all entities of every impacted file {file_entity_bytes} ({file_entities} entities)"
    );

    assert!(
        report.summary.cross_file >= 8,
        "the change reaches well beyond its own file: {:?}",
        report.summary
    );
    assert!(
        outside.len() >= 5,
        "important dependents are not in the edited file: {outside:?}"
    );
    assert!(
        report.items.iter().all(|i| !i.evidence.is_empty()),
        "every claim carries evidence"
    );
    assert!(
        report_bytes < file_entity_bytes,
        "the bounded report is smaller than the file-level expansion it replaces: \
         {report_bytes} vs {file_entity_bytes}"
    );
    assert!(naive_bytes > 0);
}
