//! Stage 5 dogfood over agentctl's own source ontology. The candidate mixes
//! production declarations, test structure, and non-structural text so the
//! footprint demonstrates information that raw diff size cannot provide.
#[allow(dead_code)]
mod common;

use agentctl::local::{
    config::ProjectConfig,
    graph::{FootprintLimits, FootprintRequest, ReviewSignalKind, StructuralRole},
    repository::RepositoryInfo,
    store::Store,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "agentctl-dogfood")
        .env("GIT_AUTHOR_EMAIL", "dogfood@example.invalid")
        .env("GIT_COMMITTER_NAME", "agentctl-dogfood")
        .env("GIT_COMMITTER_EMAIL", "dogfood@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&source, &target);
        } else {
            fs::copy(source, target).unwrap();
        }
    }
}

#[test]
fn stage_five_dogfood_over_agentctls_own_source() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if RepositoryInfo::discover(&manifest).is_err() {
        eprintln!("skipping: {} is not a Git checkout", manifest.display());
        return;
    }
    let temp = common::TempDir::new();
    let root = temp.0.join("repo");
    for directory in ["src", "tests"] {
        fs::create_dir_all(root.join(directory)).unwrap();
        copy_tree(&manifest.join(directory), &root.join(directory));
    }
    fs::copy(manifest.join("Cargo.toml"), root.join("Cargo.toml")).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    ProjectConfig::initialize(&root).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "agentctl stage five"]);

    let db = temp.0.join("state.sqlite3");
    let mut store = Store::open(&db, 5000).unwrap();
    store
        .register_repository(RepositoryInfo::discover(&root).unwrap())
        .unwrap();
    store.index_repository(&root).unwrap();

    let production = format!(
        "{}\npub struct FootprintProbe;\n\nimpl FootprintProbe {{\n    pub fn inspect() {{}}\n}}\n",
        (0..24)
            .map(|n| format!("// explanatory line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let verification = "#[test]\nfn footprint_probe_is_inspectable() { assert!(true); }\n";
    fs::write(root.join("src/local/graph/footprint_probe.rs"), &production).unwrap();
    fs::write(root.join("tests/footprint_probe.rs"), verification).unwrap();
    store.index_repository(&root).unwrap();
    let candidate = store
        .ontology_status(&root)
        .unwrap()
        .candidate
        .expect("dogfood candidate")
        .generation_id;
    let report = store
        .ontology_footprint(
            &root,
            &FootprintRequest::Generation(candidate),
            FootprintLimits {
                files: 12,
                entities: 24,
                relations: 24,
                signals: 8,
                evidence_per_signal: 8,
            },
        )
        .unwrap();
    let report_bytes = serde_json::to_vec(&report).unwrap().len();
    let raw_lines = production.lines().count() + verification.lines().count();

    eprintln!("--- Stage 5 dogfood: agentctl's own ontology ---");
    eprintln!(
        "raw changed lines: {raw_lines}; production files +{}; test files +{}; production declarations +{}; test declarations +{}",
        report.summary.production_files_added,
        report.summary.test_files_added,
        report.summary.production_entities_added,
        report.summary.test_entities_added,
    );
    eprintln!(
        "public surface +{}; resolved relations +{}; signals {:?}; report bytes {report_bytes}",
        report.summary.public_surface_added,
        report.summary.relations_added,
        report
            .signals
            .iter()
            .map(|signal| signal.kind)
            .collect::<Vec<_>>(),
    );

    assert_eq!(report.summary.production_files_added, 1);
    assert_eq!(report.summary.test_files_added, 1);
    assert_eq!(report.summary.production_entities_added, 3);
    assert_eq!(report.summary.test_entities_added, 1);
    assert_eq!(report.summary.public_surface_added, 2);
    assert!(raw_lines > report.summary.production_entities_added * 8);
    assert!(report.signals.iter().any(|signal| {
        signal.kind == ReviewSignalKind::PublicSurfaceGrowth && !signal.evidence.is_empty()
    }));
    assert!(report.signals.iter().any(|signal| {
        signal.kind == ReviewSignalKind::AbstractionSurfaceGrowth && !signal.evidence.is_empty()
    }));
    assert!(
        report
            .entities
            .iter()
            .filter(|entity| entity.path == "tests/footprint_probe.rs")
            .all(|entity| entity.role == StructuralRole::Test)
    );
    assert_eq!(report.summary.files_omitted, 0);
    assert_eq!(report.summary.entities_omitted, 0);
    assert!(report_bytes < 32_000);
}
