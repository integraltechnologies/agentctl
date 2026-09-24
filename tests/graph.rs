#[allow(dead_code)]
mod common;

use agentctl::local::{
    graph::{self, ContextLimits, EntityKind, RelationKind, SearchMode},
    repository::RepositoryInfo,
    store::{DATABASE_VERSION, JournalEntry, Store},
};
use common::TempDir;
use rusqlite::Connection;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const RUST: &str = r#"
use std::collections::BTreeMap;
pub mod matching {
    pub trait Confirm { fn confirm(&self) -> bool; }
    pub struct Vehicle { pub name: String }
    impl Confirm for Vehicle { fn confirm(&self) -> bool { !self.name.is_empty() } }
    pub const MAX_CANDIDATES: usize = 8;
    pub enum Verdict { Accept, Reject }
    pub fn resolve_candidate(name: &str) -> bool { !name.is_empty() }
    pub fn confirm_vehicle(name: &str) -> bool { self::resolve_candidate(name) }
    pub fn ambiguous(resolve_candidate: fn(&str) -> bool) -> bool { resolve_candidate("x") }
    #[test]
    fn test_confirmation() { assert!(self::resolve_candidate("car")); }
}
pub mod other { pub fn resolve_candidate(_: &str) -> bool { false } }
"#;
const PYTHON: &str = r#"
from collections import defaultdict
from .helpers import resolve_candidate as helper

class VehicleMatcher:
    def __init__(self, threshold: int = 2):
        self.threshold = threshold

    def confirm_vehicle(self, name: str) -> bool:
        return helper(name)

def resolve_candidate(name: str) -> bool:
    return bool(name)

def ambiguous(resolve_candidate):
    return resolve_candidate("unknown")
"#;
const PYTEST: &str = r#"
import unittest
from package.matching import VehicleMatcher

class TestVehicle(unittest.TestCase):
    def test_confirm_vehicle(self):
        matcher = VehicleMatcher()
        self.assertTrue(matcher.confirm_vehicle("car"))

def test_default_matcher():
    assert VehicleMatcher().threshold == 2
"#;
const TS: &str = r#"
import { normalize } from './helpers';
export interface Confirmable { confirm(name: string): boolean; }
export class VehicleMatcher implements Confirmable {
    confirm(name: string): boolean { return normalize(name).length > 0; }
}
export function resolveCandidate(name: string): boolean { return name.length > 0; }
export const confirmVehicle = (name: string): boolean => resolveCandidate(name);
export { normalize } from './helpers';
"#;
const JS: &str = r#"
import { resolveCandidate } from './matching';
import { test } from '@jest/globals';
test('vehicle confirmation', () => { resolveCandidate('car'); });
export function unresolved(resolveCandidate) { return resolveCandidate('unknown'); }
"#;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    database: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = TempDir::new();
        let root = temp.0.join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        for (path, source) in files {
            write(&root, path, source);
        }
        let database = temp.0.join("state.sqlite3");
        Store::open(&database, 5000)
            .unwrap()
            .register_repository(RepositoryInfo::discover(&root).unwrap())
            .unwrap();
        Self {
            temp,
            root,
            database,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.database, 5000).unwrap()
    }
    fn index(&self) -> graph::IndexStats {
        self.store().index_repository(&self.root).unwrap()
    }
    fn sql(&self) -> Connection {
        common::sql(&self.database)
    }
}
fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}
fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Graph Test",
            "-c",
            "user.email=graph@example.invalid",
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
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn cli(f: &Fixture, root: &Path, args: &[&str]) -> Output {
    cli_env(f, root, args, &[])
}
fn cli_env(f: &Fixture, root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(root)
        .args(args)
        .envs(env.iter().copied())
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
fn cli_json(f: &Fixture, root: &Path, args: &[&str]) -> Value {
    let output = cli(f, root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn discovery_is_sorted_ignored_and_language_bounded() {
    let f = Fixture::new(&[
        ("src/lib.rs", RUST),
        ("package/matching.py", PYTHON),
        ("web/matching.ts", TS),
        (".gitignore", "ignored/\n*.generated.py\n"),
        ("ignored/no.py", "invalid"),
        ("target/no.rs", "invalid"),
        ("node_modules/no.js", "invalid"),
        (".git/no.py", "invalid"),
        ("README.md", "not source"),
        ("foo.generated.py", "invalid"),
    ]);
    let paths = graph::files::discover(&f.root).unwrap();
    assert_eq!(
        paths,
        vec!["package/matching.py", "src/lib.rs", "web/matching.ts"]
    );
    assert_eq!(paths, graph::files::discover(&f.root).unwrap());
    assert_eq!(f.index().discovered, 3);
}

#[cfg(unix)]
#[test]
fn symlinks_and_protected_read_paths_are_not_indexed() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(&[
        ("safe.py", "def safe(): pass"),
        ("private/secret.py", "def secret(): pass"),
        (
            ".agentctl/project.toml",
            "version=1\n[[protected]]\npath='private'\ndeny_read=true\ndeny_write=false\nreason='private'\n",
        ),
    ]);
    symlink(f.root.join("safe.py"), f.root.join("link.py")).unwrap();
    symlink(&f.root, f.root.join("cycle")).unwrap();
    symlink(f.temp.0.join("missing"), f.root.join("broken.py")).unwrap();
    assert_eq!(graph::files::discover(&f.root).unwrap(), vec!["safe.py"]);
    assert_eq!(f.index().indexed, 1);
}

#[test]
fn binary_invalid_utf8_and_oversized_sources_are_truthful_failures() {
    let f = Fixture::new(&[("good.py", "def good(): pass")]);
    fs::write(f.root.join("binary.py"), [0, 1, 2]).unwrap();
    fs::write(f.root.join("encoding.rs"), [0xff, 0xfe]).unwrap();
    fs::write(
        f.root.join("huge.ts"),
        vec![b'x'; graph::files::MAX_FILE_BYTES as usize + 1],
    )
    .unwrap();
    let stats = f.index();
    assert_eq!((stats.discovered, stats.failed), (4, 3));
    let status = f.store().index_status(&f.root).unwrap();
    assert!(!status.fresh);
    assert_eq!(status.failed_files.len(), 3);
    let store = f.store();
    let good = store
        .graph(&f.root)
        .unwrap()
        .symbols("good", SearchMode::Exact, 10)
        .unwrap();
    assert!(good.data.iter().any(|e| e.kind == EntityKind::Function));
    assert!(good.data.iter().all(|e| e.provenance.path == "good.py"));
    assert!(!good.freshness.fresh);
    assert_eq!(stats.entities, 3);
}

#[test]
fn rust_extracts_structural_symbols_imports_tests_and_explicit_local_calls() {
    let f = Fixture::new(&[("src/lib.rs", RUST)]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    for (name, kind) in [
        ("Vehicle", EntityKind::Type),
        ("Verdict", EntityKind::Enum),
        ("Confirm", EntityKind::Trait),
        ("MAX_CANDIDATES", EntityKind::Constant),
        ("test_confirmation", EntityKind::Test),
        ("matching", EntityKind::Module),
    ] {
        assert_eq!(
            store
                .graph(&f.root)
                .unwrap()
                .symbols(name, SearchMode::Exact, 10)
                .unwrap()
                .data[0]
                .kind,
            kind
        );
    }
    let candidates = store
        .graph(&f.root)
        .unwrap()
        .symbols("resolve_candidate", SearchMode::Exact, 10)
        .unwrap()
        .data;
    assert_eq!(candidates.len(), 2);
    let resolved = candidates
        .iter()
        .find(|e| e.qualified_name.contains("matching"))
        .unwrap();
    let calls = store
        .graph(&f.root)
        .unwrap()
        .relations(resolved.id.as_str(), true, Some(RelationKind::Calls), 20)
        .unwrap()
        .data;
    assert_eq!(calls.len(), 1); // macro contents are not expanded by the Rust grammar.
    assert_eq!(calls[0].target.as_ref(), Some(&resolved.id));
    let ambiguous = store
        .graph(&f.root)
        .unwrap()
        .relations("ambiguous", false, Some(RelationKind::Calls), 20)
        .unwrap()
        .data;
    assert_eq!(ambiguous.len(), 1);
    assert!(ambiguous[0].target.is_none());
    let all = store
        .graph(&f.root)
        .unwrap()
        .entities_in_file("src/lib.rs", 100)
        .unwrap()
        .data;
    assert!(all.iter().any(|e| e.kind == EntityKind::Method));
    let file = all.iter().find(|e| e.kind == EntityKind::File).unwrap();
    let module = all
        .iter()
        .find(|e| e.parent.as_ref() == Some(&file.id))
        .unwrap();
    let imports = store
        .graph(&f.root)
        .unwrap()
        .relations(module.id.as_str(), false, Some(RelationKind::Imports), 20)
        .unwrap()
        .data;
    assert_eq!(imports.len(), 1);
    assert!(imports[0].target.is_none());
    assert!(
        all.iter()
            .all(|e| e.signature.len() <= 960 && e.provenance.content_hash.starts_with("blake3:"))
    );
}

#[test]
fn python_extracts_nested_classes_methods_imports_and_test_conventions() {
    let f = Fixture::new(&[
        ("package/matching.py", PYTHON),
        ("tests/test_matching.py", PYTEST),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("VehicleMatcher", SearchMode::Exact, 10)
            .unwrap()
            .data[0]
            .kind,
        EntityKind::Type
    );
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("confirm_vehicle", SearchMode::Exact, 10)
            .unwrap()
            .data[0]
            .kind,
        EntityKind::Method
    );
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("test_confirm_vehicle", SearchMode::Exact, 10)
            .unwrap()
            .data[0]
            .kind,
        EntityKind::Test
    );
    let outgoing = store
        .graph(&f.root)
        .unwrap()
        .relations("ambiguous", false, Some(RelationKind::Calls), 20)
        .unwrap()
        .data;
    assert_eq!(outgoing.len(), 1);
    assert!(outgoing[0].target.is_none());
    assert!(
        store
            .graph(&f.root)
            .unwrap()
            .relations("resolve_candidate", true, Some(RelationKind::Calls), 20)
            .unwrap()
            .data
            .is_empty()
    );
    let tests = store
        .graph(&f.root)
        .unwrap()
        .related_tests("TestVehicle", 10)
        .unwrap()
        .data;
    assert_eq!(tests.len(), 1);
}

#[test]
fn typescript_javascript_and_tsx_adapters_extract_core_structure() {
    let f = Fixture::new(&[
        ("web/matching.ts", TS),
        ("web/matching.test.js", JS),
        (
            "web/View.tsx",
            "export function View() { return <div>Hello</div>; }",
        ),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    for (name, kind) in [
        ("Confirmable", EntityKind::Trait),
        ("VehicleMatcher", EntityKind::Type),
        ("confirm", EntityKind::Method),
        ("confirmVehicle", EntityKind::Function),
        ("View", EntityKind::Function),
    ] {
        assert_eq!(
            store
                .graph(&f.root)
                .unwrap()
                .symbols(name, SearchMode::Exact, 20)
                .unwrap()
                .data[0]
                .kind,
            kind
        );
    }
    let all = store
        .graph(&f.root)
        .unwrap()
        .entities_in_file("web/matching.test.js", 100)
        .unwrap()
        .data;
    assert!(all.iter().any(|e| e.kind == EntityKind::Test));
    // Deterministic TS/JS resolution: the same-file call and the imported call
    // both bind; the parameter that shadows the name must not.
    let callers = store
        .graph(&f.root)
        .unwrap()
        .relations("resolveCandidate", true, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    let sources: Vec<&str> = callers
        .iter()
        .filter_map(|e| e.resolution.map(|_| e.provenance.path.as_str()))
        .collect();
    assert!(
        sources.contains(&"web/matching.ts"),
        "same-file call must resolve: {callers:?}"
    );
    assert!(
        sources.contains(&"web/matching.test.js"),
        "imported call must resolve: {callers:?}"
    );
    assert!(
        callers.iter().all(|e| e.resolution.is_some()),
        "a shadowing parameter must never be resolved as the import: {callers:?}"
    );
    assert_eq!(
        callers.len(),
        2,
        "exactly the two provable call sites: {callers:?}"
    );
    let repeated = f.index();
    assert_eq!((repeated.indexed, repeated.reused), (0, 3));
}

#[test]
fn hashes_and_incremental_refresh_preserve_unchanged_derivations_and_ids() {
    assert_eq!(graph::content_hash(b"same"), graph::content_hash(b"same"));
    assert_ne!(
        graph::content_hash(b"same"),
        graph::content_hash(b"changed")
    );
    let f = Fixture::new(&[("src/lib.rs", RUST), ("package/matching.py", PYTHON)]);
    assert_eq!(f.index().indexed, 2);
    let before = f
        .store()
        .graph(&f.root)
        .unwrap()
        .symbols("VehicleMatcher", SearchMode::Exact, 10)
        .unwrap()
        .data
        .remove(0);
    assert_eq!((f.index().indexed, f.index().reused), (0, 2));
    write(
        &f.root,
        "package/matching.py",
        &format!("# shifted lines\n{PYTHON}"),
    );
    let stale = f.store().index_status(&f.root).unwrap();
    assert_eq!(stale.stale_files, vec!["package/matching.py"]);
    assert!(f.store().graph(&f.root).is_err());
    let stats = f.index();
    assert_eq!((stats.indexed, stats.reused, stats.changed), (1, 1, 1));
    let after = f
        .store()
        .graph(&f.root)
        .unwrap()
        .symbols("VehicleMatcher", SearchMode::Exact, 10)
        .unwrap()
        .data
        .remove(0);
    assert_eq!(before.id, after.id);
    assert_ne!(before.range, after.range);
    assert_ne!(
        before.provenance.content_hash,
        after.provenance.content_hash
    );
    assert!(f.store().index_status(&f.root).unwrap().fresh);
}

#[test]
fn deleting_renaming_and_ignoring_files_cleans_all_supported_facts() {
    let f = Fixture::new(&[("src/lib.rs", RUST), ("matching.py", PYTHON)]);
    f.index();
    fs::rename(f.root.join("matching.py"), f.root.join("renamed.py")).unwrap();
    let stats = f.index();
    assert_eq!((stats.deleted, stats.indexed, stats.reused), (1, 1, 1));
    write(&f.root, ".gitignore", "renamed.py\n");
    fs::remove_file(f.root.join("src/lib.rs")).unwrap();
    let stats = f.index();
    assert_eq!((stats.deleted, stats.entities, stats.edges), (2, 0, 0));
    assert!(f.store().index_status(&f.root).unwrap().fresh);
    assert!(
        f.sql()
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
fn invalid_parse_removes_old_facts_and_recovers_after_fix() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    f.index();
    write(&f.root, "matching.py", "def broken(:\n");
    assert!(f.store().graph(&f.root).is_err());
    let stats = f.index();
    assert_eq!((stats.failed, stats.entities, stats.edges), (1, 0, 0));
    let result = f
        .store()
        .graph(&f.root)
        .unwrap()
        .locate("VehicleMatcher", 10)
        .unwrap();
    assert!(result.data.is_empty());
    assert!(!result.freshness.fresh);
    assert!(
        result.freshness.failed_files[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("syntax error")
    );
    write(&f.root, "matching.py", PYTHON);
    assert_eq!((f.index().failed, f.index().reused), (0, 1));
}

#[test]
fn backend_and_index_versions_force_reparse() {
    let f = Fixture::new(&[("matching.py", PYTHON), ("src/lib.rs", RUST)]);
    f.index();
    f.sql()
        .execute(
            "UPDATE indexed_files SET backend='old-parser' WHERE path='matching.py'",
            [],
        )
        .unwrap();
    assert_eq!(
        f.store().index_status(&f.root).unwrap().stale_files,
        vec!["matching.py"]
    );
    let stats = f.index();
    assert_eq!((stats.indexed, stats.reused), (1, 1));
    f.sql().execute("UPDATE graph_indexes SET metadata_json=json_set(metadata_json,'$.version','old-index')", []).unwrap();
    assert!(f.store().graph(&f.root).is_err());
    assert_eq!(f.index().indexed, 2);
}

#[test]
fn duplicate_names_remain_distinct_and_do_not_guess_call_targets() {
    let f = Fixture::new(&[
        (
            "lib.rs",
            "mod a { fn same() {} } mod b { fn same() {} } fn caller() { same(); }",
        ),
        ("a.py", "def same(): pass\ndef caller(same): return same()"),
        ("b.py", "def same(): pass"),
    ]);
    f.index();
    let store = f.store();
    let entities = store
        .graph(&f.root)
        .unwrap()
        .symbols("same", SearchMode::Exact, 20)
        .unwrap()
        .data;
    assert_eq!(entities.len(), 4);
    let ids: std::collections::BTreeSet<_> = entities.iter().map(|e| &e.id).collect();
    assert_eq!(ids.len(), 4);
    for entity in entities {
        assert!(
            store
                .graph(&f.root)
                .unwrap()
                .relations(entity.id.as_str(), true, Some(RelationKind::Calls), 20)
                .unwrap()
                .data
                .is_empty()
        );
    }
    assert!(
        store
            .graph(&f.root)
            .unwrap()
            .impact("same", ContextLimits::default())
            .is_err()
    );
}

#[test]
fn locate_is_deterministic_exact_first_normalized_and_bounded() {
    let f = Fixture::new(&[
        ("web/matching.ts", TS),
        ("package/matching.py", PYTHON),
        ("src/lib.rs", RUST),
    ]);
    f.index();
    let store = f.store();
    let first = store
        .graph(&f.root)
        .unwrap()
        .locate("confirm_vehicle", 10)
        .unwrap();
    assert_eq!(first.data[0].entity.name, "confirm_vehicle");
    assert_eq!(
        first.data,
        store
            .graph(&f.root)
            .unwrap()
            .locate("confirm_vehicle", 10)
            .unwrap()
            .data
    );
    let camel = store
        .graph(&f.root)
        .unwrap()
        .locate("confirm vehicle", 10)
        .unwrap();
    assert!(camel.data.iter().any(|e| e.entity.name == "confirmVehicle"));
    assert!(
        camel
            .data
            .iter()
            .any(|e| e.entity.name == "confirm_vehicle")
    );
    let path = store
        .graph(&f.root)
        .unwrap()
        .locate("package matching", 1)
        .unwrap();
    assert_eq!(path.data.len(), 1);
    assert_eq!(path.data[0].entity.provenance.path, "package/matching.py");
    assert!(store.graph(&f.root).unwrap().locate("", 10).is_err());
    assert!(
        store
            .graph(&f.root)
            .unwrap()
            .locate("matching", 101)
            .is_err()
    );
}

#[test]
fn context_neighborhood_and_impact_are_bounded_and_provenanced() {
    let source = "mod matching { pub fn resolve_candidate() {} fn caller() { self::resolve_candidate(); } #[test] fn test_candidate() { self::resolve_candidate(); } }";
    let f = Fixture::new(&[("src/lib.rs", source)]);
    f.index();
    let store = f.store();
    let limits = ContextLimits {
        primary: 1,
        depth: 1,
        neighbors: 3,
        tests: 1,
    };
    let context = store
        .graph(&f.root)
        .unwrap()
        .context("resolve_candidate", limits)
        .unwrap();
    assert_eq!(context.primary.len(), 1);
    assert!(context.neighbors.len() <= 3);
    assert_eq!(context.tests.len(), 1);
    assert!(context.freshness.fresh);
    assert!(
        context.primary[0]
            .entity
            .provenance
            .backend
            .contains("tree-sitter")
    );
    assert!(serde_json::to_string(&context).unwrap().len() < 25_000);
    let impact = store
        .graph(&f.root)
        .unwrap()
        .impact("resolve_candidate", limits)
        .unwrap();
    assert!(impact.meaning.contains("not semantic impact"));
    assert_eq!(impact.neighbors.len(), 2);
    assert!(
        impact
            .relations
            .iter()
            .all(|e| e.target.is_some() && e.kind == RelationKind::Calls)
    );
    let none = store
        .graph(&f.root)
        .unwrap()
        .neighborhood("resolve_candidate", ContextLimits { depth: 0, ..limits })
        .unwrap();
    assert!(none.neighbors.is_empty());
    assert!(
        store
            .graph(&f.root)
            .unwrap()
            .context("resolve", ContextLimits { depth: 4, ..limits })
            .is_err()
    );
}

#[test]
fn workspace_graphs_share_logical_ids_but_never_share_freshness() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture"]);
    f.index();
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
    let main_info = RepositoryInfo::discover(&f.root).unwrap();
    let linked_info = RepositoryInfo::discover(&linked).unwrap();
    assert_eq!(main_info.repository_id, linked_info.repository_id);
    assert_ne!(main_info.workspace_id, linked_info.workspace_id);
    let mut store = f.store();
    store.register_repository(linked_info).unwrap();
    assert!(store.graph(&linked).is_err());
    store.index_repository(&linked).unwrap();
    let main = store
        .graph(&f.root)
        .unwrap()
        .symbols("VehicleMatcher", SearchMode::Exact, 10)
        .unwrap()
        .data
        .remove(0);
    let other = store
        .graph(&linked)
        .unwrap()
        .symbols("VehicleMatcher", SearchMode::Exact, 10)
        .unwrap()
        .data
        .remove(0);
    assert_eq!(main.id, other.id);
    assert_ne!(main.provenance.workspace_id, other.provenance.workspace_id);
    write(&linked, "matching.py", "def linked_only(): pass");
    assert!(store.graph(&linked).is_err());
    assert!(store.index_status(&f.root).unwrap().fresh);
    store.index_repository(&linked).unwrap();
    assert!(
        store
            .graph(&linked)
            .unwrap()
            .symbols("VehicleMatcher", SearchMode::Exact, 10)
            .unwrap()
            .data
            .is_empty()
    );
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("VehicleMatcher", SearchMode::Exact, 10)
            .unwrap()
            .data
            .len(),
        1
    );
    drop(store);
    assert!(f.store().index_status(&linked).unwrap().fresh);
}

#[test]
fn graph_and_aggregate_events_rollback_together_on_storage_failure() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    f.index();
    let before = f
        .store()
        .graph(&f.root)
        .unwrap()
        .locate("VehicleMatcher", 1)
        .unwrap()
        .data;
    let count: i64 = f
        .sql()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    f.sql().execute_batch("CREATE TRIGGER reject_graph_event BEFORE INSERT ON events WHEN NEW.entry_json LIKE '%INDEX_COMPLETED%' BEGIN SELECT RAISE(ABORT, 'test journal failure'); END;").unwrap();
    write(&f.root, "matching.py", "def changed(): pass");
    assert!(f.store().index_repository(&f.root).is_err());
    assert_eq!(
        f.sql()
            .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        count
    );
    write(&f.root, "matching.py", PYTHON);
    assert_eq!(
        before,
        f.store()
            .graph(&f.root)
            .unwrap()
            .locate("VehicleMatcher", 1)
            .unwrap()
            .data
    );
    let events = f.store().events(None, None, None, 100).unwrap();
    let event = events
        .iter()
        .find(|e| matches!(e.entry, JournalEntry::IndexCompleted { .. }))
        .unwrap();
    assert!(event.workspace_id.is_some());
}

#[test]
fn cli_indexes_reopens_reuses_modifies_deletes_and_queries_without_runtime_services() {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub fn stable() {} fn caller() { self::stable(); }",
        ),
        ("matching.py", PYTHON),
        ("web/matching.ts", TS),
    ]);
    cli_json(&f, &f.root, &["init", "--json"]);
    cli_json(&f, &f.root, &["repo", "init", "--json"]);
    assert_eq!(
        cli_json(&f, &f.root, &["repo", "index", "--json"])["indexed"],
        3
    );
    assert_eq!(
        cli_json(&f, &f.root, &["repo", "index", "--json"])["reused"],
        3
    );
    assert_eq!(
        cli_json(&f, &f.root, &["repo", "index", "--status", "--json"])["fresh"],
        true
    );
    for (command, query) in [
        ("symbol", "stable"),
        ("search", "stable"),
        ("file", "src/lib.rs"),
        ("locate", "vehicle matcher"),
        ("context", "stable"),
        ("impact", "stable"),
        ("neighbors", "stable"),
        ("callers", "stable"),
        ("refs", "stable"),
        ("tests", "stable"),
    ] {
        cli_json(&f, &f.root, &["code", command, query, "--json"]);
        assert!(cli(&f, &f.root, &["code", command, query]).status.success());
    }
    write(&f.root, "matching.py", "def changed(): pass");
    assert!(
        !cli(&f, &f.root, &["code", "locate", "changed", "--json"])
            .status
            .success()
    );
    let refreshed = cli_json(&f, &f.root, &["repo", "index", "--json"]);
    assert_eq!(
        (refreshed["indexed"].as_u64(), refreshed["reused"].as_u64()),
        (Some(1), Some(2))
    );
    fs::remove_file(f.root.join("web/matching.ts")).unwrap();
    assert_eq!(
        cli_json(&f, &f.root, &["repo", "index", "--json"])["deleted"],
        1
    );
    write(&f.root, "matching.py", "def broken(:");
    let failed = cli(&f, &f.root, &["repo", "index", "--json"]);
    assert!(!failed.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&failed.stdout).unwrap()["failed"],
        1
    );
    assert_eq!(
        cli_json(&f, &f.root, &["repo", "index", "--status", "--json"])["failed_files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        cli_json(&f, &f.root, &["code", "symbol", "changed", "--json"])["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for args in [
        &["code", "impact", "absent"][..],
        &["code", "locate", "stable", "--limit", "0"],
        &["code", "context", "stable", "--depth", "9"],
    ] {
        assert!(!cli(&f, &f.root, args).status.success());
    }
}

#[test]
fn future_graph_database_version_is_rejected() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    assert_eq!(f.store().status().unwrap().schema_version, DATABASE_VERSION);
    f.sql()
        .pragma_update(None, "user_version", DATABASE_VERSION + 1)
        .unwrap();
    assert!(Store::open(&f.database, 5000).is_err());
    assert!(Store::read_only(&f.database, 5000).is_err());
}

fn restore_v2_schema(f: &Fixture) {
    common::strip_runtime(&f.sql());
    f.sql().execute_batch("DROP TRIGGER execution_task_gate; DROP TABLE execution_plans; DROP TABLE planning_requests; DELETE FROM schema_migrations WHERE version>=5;").unwrap();
    f.sql().execute_batch("DROP TABLE memory_links; DROP TABLE memory_fts; DROP TABLE memory_entries; DELETE FROM schema_migrations WHERE version=4;").unwrap();
    // v3 is strictly additive: removing only its tables/history restores the
    // accepted v2 schema, leaving real repository, workspace, plan and journal rows.
    f.sql().execute_batch("DROP TABLE graph_edges; DROP TABLE graph_entities; DROP TABLE indexed_files; DROP TABLE graph_indexes; DELETE FROM schema_migrations WHERE version=3; PRAGMA user_version=2;").unwrap();
}

fn legacy_rows(f: &Fixture) -> Vec<String> {
    let connection = f.sql();
    let mut rows = vec![];
    for sql in [
        "SELECT record_json FROM repositories ORDER BY repo_id",
        "SELECT record_json FROM workspaces ORDER BY workspace_id",
        "SELECT packet_json FROM plans ORDER BY repo_id,plan_id",
        "SELECT entry_json FROM events ORDER BY sequence",
    ] {
        rows.extend(
            connection
                .prepare(sql)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    rows
}

#[test]
fn accepted_v2_migration_is_additive_and_preserves_payloads_and_history() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    let info = RepositoryInfo::discover(&f.root).unwrap();
    common::seed_plan(&f.database, info.repository_id.as_str(), &common::plan());
    restore_v2_schema(&f);
    let before = legacy_rows(&f);
    assert!(Store::read_only(&f.database, 5000).is_err());
    for _ in 0..2 {
        let store = f.store();
        assert_eq!(store.status().unwrap().schema_version, DATABASE_VERSION);
        assert_eq!(store.tasks(&info.repository_id, None).unwrap().len(), 2);
        assert_eq!(before, legacy_rows(&f));
    }
    assert_eq!(f.index().indexed, 1);
    assert!(
        f.sql()
            .execute("UPDATE events SET timestamp_ms=20", [])
            .is_err()
    );
    assert!(f.sql().execute("DELETE FROM events", []).is_err());
    assert!(
        f.sql()
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
fn v2_migration_conflict_rolls_back_without_partial_graph_schema() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    restore_v2_schema(&f);
    let before = legacy_rows(&f);
    f.sql()
        .execute_batch("CREATE TABLE graph_edges (unrelated TEXT);")
        .unwrap();
    assert!(Store::open(&f.database, 5000).is_err());
    assert_eq!(
        f.sql()
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        f.sql()
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='graph_indexes'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(before, legacy_rows(&f));
}

#[test]
fn query_failure_diagnostics_are_bounded_without_hiding_failure_count() {
    let f = Fixture::new(&[("healthy.py", "def useful(): pass")]);
    for n in 0..15 {
        write(&f.root, &format!("failed_{n}.py"), "def broken(:");
    }
    assert_eq!(f.index().failed, 15);
    let context = f
        .store()
        .graph(&f.root)
        .unwrap()
        .context("useful", ContextLimits::default())
        .unwrap();
    assert_eq!(context.freshness.failed_file_count, 15);
    assert_eq!(context.freshness.failed_files.len(), 10);
    assert!(context.freshness.diagnostics_truncated);
    assert!(!context.freshness.fresh);
}

#[test]
fn ignore_policy_failure_does_not_publish_partial_discovery() {
    let f = Fixture::new(&[("matching.py", PYTHON)]);
    f.index();
    write(&f.root, ".gitignore", "[z-a]\n");
    assert!(graph::files::discover(&f.root).is_err());
    assert!(f.store().index_repository(&f.root).is_err());
    fs::remove_file(f.root.join(".gitignore")).unwrap();
    assert!(f.store().index_status(&f.root).unwrap().fresh);
}

#[test]
fn explicit_local_rust_type_and_trait_paths_have_resolved_relations() {
    let f = Fixture::new(&[(
        "lib.rs",
        "pub struct Vehicle; pub trait Confirm {} impl self::Confirm for Vehicle {} fn inspect(_: self::Vehicle) {}",
    )]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    let refs = store
        .graph(&f.root)
        .unwrap()
        .relations("Vehicle", true, Some(RelationKind::References), 10)
        .unwrap()
        .data;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].target_name, "self::Vehicle");
    let implementations = store
        .graph(&f.root)
        .unwrap()
        .relations("Confirm", true, Some(RelationKind::Implements), 10)
        .unwrap()
        .data;
    assert_eq!(implementations.len(), 1);
}

#[test]
fn every_language_adapter_invalidates_changed_failed_and_deleted_files() {
    for (extension, valid, changed, invalid) in [
        ("rs", "fn original() {}", "fn changed() {}", "fn broken( {"),
        (
            "py",
            "def original(): pass",
            "def changed(): pass",
            "def broken(:",
        ),
        (
            "ts",
            "function original(): void {}",
            "function changed(): void {}",
            "function broken( {",
        ),
        (
            "js",
            "function original() {}",
            "function changed() {}",
            "function broken( {",
        ),
        (
            "tsx",
            "function original() { return <div/>; }",
            "function changed() { return <span/>; }",
            "function broken( {",
        ),
    ] {
        let path = format!("source.{extension}");
        let f = Fixture::new(&[(&path, valid), ("unchanged.py", "def unrelated(): pass")]);
        assert_eq!(f.index().indexed, 2);
        assert_eq!(f.index().reused, 2);
        write(&f.root, &path, changed);
        let stats = f.index();
        assert_eq!((stats.indexed, stats.reused), (1, 1));
        write(&f.root, &path, invalid);
        assert_eq!(f.index().failed, 1);
        assert!(
            f.store()
                .graph(&f.root)
                .unwrap()
                .symbols("changed", SearchMode::Exact, 10)
                .unwrap()
                .data
                .is_empty()
        );
        fs::remove_file(f.root.join(&path)).unwrap();
        assert_eq!(f.index().deleted, 1);
        assert!(f.store().index_status(&f.root).unwrap().fresh);
    }
}

// Regression coverage for the QUALIFIED_PATH resolver: it must never drop a
// qualified path's leading segment and treat a unique match on the remaining
// suffix as proof of identity. agentctl has no Cargo.toml/workspace metadata,
// so an unrecognized leading segment (an external crate, an unresolved
// re-export, or simply a typo) can never be safely distinguished from a real
// local crate name — the resolver must abstain rather than guess.

#[test]
fn unmatched_qualified_path_prefix_does_not_resolve_across_unrelated_crate_roots() {
    // Two independent crate-shaped roots in one workspace. `crate_a` calls
    // `ext::helpers::run()`; `ext` resolves to nothing agentctl knows about,
    // but `crate_b` happens to declare an unrelated `helpers::run`.
    let f = Fixture::new(&[
        (
            "crate_a/src/lib.rs",
            "pub fn caller() { ext::helpers::run(); }",
        ),
        ("crate_b/src/lib.rs", "pub mod helpers { pub fn run() {} }"),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    // The unrelated declaration exists and is exactly the kind of target the
    // old first-segment-drop fallback would have guessed.
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("run", SearchMode::Exact, 10)
            .unwrap()
            .data
            .len(),
        1
    );
    let calls = store
        .graph(&f.root)
        .unwrap()
        .relations("caller", false, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_name, "ext::helpers::run");
    assert!(
        calls[0].target.is_none(),
        "an unresolved external-looking prefix must never fall back to an unrelated crate's declaration: {calls:#?}"
    );
}

#[test]
fn unmatched_qualified_path_abstains_even_when_the_deep_suffix_is_unique() {
    // A longer unmatched prefix whose remaining suffix (after dropping just
    // the first segment) would still uniquely identify workspace code. The
    // resolver must not walk further down the path looking for a unique
    // match either.
    let f = Fixture::new(&[
        (
            "crate_a/src/lib.rs",
            "pub fn caller() { unknown::modx::suby::run_deep(); }",
        ),
        (
            "crate_b/src/lib.rs",
            "pub mod modx { pub mod suby { pub fn run_deep() {} } }",
        ),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    assert_eq!(
        store
            .graph(&f.root)
            .unwrap()
            .symbols("run_deep", SearchMode::Exact, 10)
            .unwrap()
            .data
            .len(),
        1
    );
    let calls = store
        .graph(&f.root)
        .unwrap()
        .relations("caller", false, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_name, "unknown::modx::suby::run_deep");
    assert!(calls[0].target.is_none(), "{calls:#?}");
}

#[test]
fn qualified_path_naming_an_actual_local_crate_directory_still_abstains() {
    // Even when the leading segment textually matches the directory name of
    // a real crate elsewhere in the workspace, agentctl has no Cargo.toml or
    // workspace metadata proving that segment names that crate rather than
    // being coincidental. Resolution must not be granted on name coincidence
    // alone.
    let f = Fixture::new(&[
        (
            "crate_a/src/lib.rs",
            "pub fn caller() { crate_b::helpers::run(); }",
        ),
        ("crate_b/src/lib.rs", "pub mod helpers { pub fn run() {} }"),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    let calls = store
        .graph(&f.root)
        .unwrap()
        .relations("caller", false, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_name, "crate_b::helpers::run");
    assert!(
        calls[0].target.is_none(),
        "a crate-shaped leading segment must not resolve without structural proof of crate identity: {calls:#?}"
    );
}

/// A caller query answers only with edges it proved. An empty or partial answer
/// must never read as "nothing calls this": sites that name the symbol but were
/// not resolved are reported as open questions instead of being dropped.
///
/// The deterministic pass now resolves the imported call, so the unproven case
/// here is a method call on a value — which needs type information agentctl
/// deliberately does not infer.
#[test]
fn caller_query_reports_unresolved_sites_instead_of_implying_none() {
    let f = Fixture::new(&[
        ("src/calc.py", "def add(a, b):\n    return a + b\n"),
        (
            "src/api.py",
            "from calc import add\n\n\ndef describe(a, b):\n    return str(add(a, b))\n\n\ndef via_value(box):\n    return box.add(1)\n",
        ),
    ]);
    f.index();

    let callers = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("add", true, Some(RelationKind::Calls), 20)
        .unwrap();
    // The import binding is proven...
    assert!(
        callers
            .data
            .iter()
            .any(|e| e.resolution == Some(graph::ResolutionRule::ImportBinding)),
        "the imported call must resolve deterministically: {:?}",
        callers.data
    );
    // ...and the member call on a value is still stated as unproven, not absent.
    let unresolved = callers
        .unresolved
        .expect("a partial caller answer must state what it could not resolve");
    assert!(unresolved.sites >= 1, "{unresolved:?}");
    assert!(
        unresolved.paths.iter().any(|p| p == "src/api.py"),
        "the file holding the unproven site must be named: {:?}",
        unresolved.paths
    );
    assert!(!unresolved.meaning.is_empty());

    // A symbol nothing names at all reports no unresolved sites, so "none" and
    // "not proven" stay distinguishable.
    let quiet = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("describe", true, Some(RelationKind::Calls), 20)
        .unwrap();
    assert!(quiet.data.is_empty());
    assert!(
        quiet.unresolved.is_none(),
        "nothing names describe, so there is nothing unproven to report"
    );
}

/// Phase A/1B: tree-sitter reports the whole callee expression for a member
/// call, so a chain was observed as the target name `a.b().c`. That string
/// names nothing, resolves to nothing, and crowds out real names in the
/// unresolved digests shown to planners and verifiers.
#[test]
fn chained_calls_are_observed_as_bounded_member_targets() {
    let f = Fixture::new(&[(
        "src/lib.rs",
        r#"
pub struct B;
impl B { pub fn c(&self) -> B { B } pub fn d(&self) {} }
pub fn make() -> B { B }
pub fn chain() { make().c().d(); }
pub fn simple(b: &B) { b.d(); }
"#,
    )]);
    f.index();
    let sql = f.sql();
    let mut statement = sql
        .prepare(
            "SELECT json_extract(record_json,'$.target_name') FROM graph_edges \
             WHERE json_extract(record_json,'$.kind')='CALLS'",
        )
        .unwrap();
    let names: Vec<String> = statement
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    for name in &names {
        assert!(
            !name.contains('(') || name.starts_with("()."),
            "a call target must not carry a chained expression: {name:?}"
        );
        assert!(
            !name.contains(char::is_whitespace),
            "a call target must not carry whitespace: {name:?}"
        );
        assert!(name.len() <= 80, "unbounded call target: {name:?}");
    }
    // A computed receiver is stated as such rather than invented.
    assert!(names.iter().any(|n| n == "().c"), "{names:?}");
    assert!(names.iter().any(|n| n == "().d"), "{names:?}");
    // A named receiver is still named, so `self.m`/`Type::m` stay resolvable.
    assert!(names.iter().any(|n| n == "b.d"), "{names:?}");
}

/// Deterministic cross-file resolution, stated as ground truth per language.
/// Every case is either provable from syntax plus module/import structure, or
/// deliberately UNKNOWN. A wrong edge is worse than an unknown one, so the
/// UNKNOWN rows are assertions too.
#[test]
fn fast_resolution_ground_truth_across_languages() {
    fn resolved(f: &Fixture, caller: &str) -> Option<(String, graph::ResolutionRule)> {
        let store = f.store();
        let edges = store
            .graph(&f.root)
            .unwrap()
            .relations(caller, false, Some(RelationKind::Calls), 20)
            .unwrap()
            .data;
        let edge = edges.iter().find(|e| e.target.is_some())?;
        let target = edge.target.clone().unwrap();
        let qualified: String = f
            .sql()
            .query_row(
                "SELECT qualified_name FROM graph_entities WHERE entity_id=?1",
                [target.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        Some((qualified, edge.resolution?))
    }

    use graph::ResolutionRule::*;

    // ---- Rust ----
    let rust = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod util;\npub mod deep;\nuse util::helper;\nuse util::helper as aliased;\n\
             pub fn local_callee() -> u32 { 1 }\n\
             pub fn r_same() -> u32 { local_callee() }\n\
             pub fn r_import() -> u32 { helper() }\n\
             pub fn r_alias() -> u32 { aliased() }\n\
             pub fn r_qualified() -> u32 { util::helper() }\n\
             pub fn r_crate() -> u32 { crate::util::helper() }\n\
             pub fn r_nested() -> u32 { deep::inner::buried() }\n\
             pub fn r_external() -> String { String::from(\"x\") }\n",
        ),
        ("src/util.rs", "pub fn helper() -> u32 { 1 }\n"),
        ("src/deep.rs", "pub mod inner;\n"),
        ("src/deep/inner.rs", "pub fn buried() -> u32 { 3 }\n"),
        (
            "src/shapes.rs",
            "pub struct S { pub e: u32 }\nimpl S { pub fn area(&self) -> u32 { self.e } }\n\
             pub fn r_method(s: &S) -> u32 { s.area() }\n",
        ),
    ]);
    assert_eq!(rust.index().failed, 0);
    for (caller, target, rule) in [
        ("r_same", "src::lib::local_callee", LexicalScope),
        ("r_import", "src::util::helper", ImportBinding),
        ("r_alias", "src::util::helper", AliasBinding),
        ("r_qualified", "src::util::helper", QualifiedPath),
        ("r_crate", "src::util::helper", QualifiedPath),
        ("r_nested", "src::deep::inner::buried", QualifiedPath),
    ] {
        assert_eq!(
            resolved(&rust, caller),
            Some((target.into(), rule)),
            "rust {caller}"
        );
    }
    // Out of the repository, and needing receiver types: both stay unknown.
    assert_eq!(resolved(&rust, "r_external"), None, "external call");
    assert_eq!(resolved(&rust, "r_method"), None, "method dispatch");

    // ---- Python ----
    let py = Fixture::new(&[
        (
            "util.py",
            "def helper():\n    return 1\n\n\nclass Shape:\n    pass\n",
        ),
        (
            "main.py",
            "import util\nimport util as u\nfrom util import helper\nfrom util import Shape\nimport json\n\n\
             def p_local():\n    return 7\n\n\
             def p_same():\n    return p_local()\n\n\
             def p_import():\n    return helper()\n\n\
             def p_qualified():\n    return util.helper()\n\n\
             def p_alias():\n    return u.helper()\n\n\
             def p_class():\n    return Shape()\n\n\
             def p_external():\n    return json.dumps({})\n\n\
             def p_method(s):\n    return s.helper()\n",
        ),
    ]);
    assert_eq!(py.index().failed, 0);
    for (caller, target, rule) in [
        ("p_same", "main::p_local", LexicalScope),
        ("p_import", "util::helper", ImportBinding),
        ("p_qualified", "util::helper", ImportBinding),
        ("p_alias", "util::helper", AliasBinding),
        ("p_class", "util::Shape", ImportBinding),
    ] {
        assert_eq!(
            resolved(&py, caller),
            Some((target.into(), rule)),
            "python {caller}"
        );
    }
    assert_eq!(resolved(&py, "p_external"), None, "external call");
    assert_eq!(resolved(&py, "p_method"), None, "attribute on a value");

    // ---- TypeScript ----
    let ts = Fixture::new(&[
        (
            "src/util.ts",
            "export function helper(): number { return 1; }\n\
             export function other(): number { return 2; }\n",
        ),
        (
            "src/main.ts",
            "import { helper } from \"./util\";\n\
             import { other as renamed } from \"./util\";\n\
             import * as fs from \"node:fs\";\n\
             function localCallee(): number { return 7; }\n\
             export function t_same(): number { return localCallee(); }\n\
             export function t_import(): number { return helper(); }\n\
             export function t_alias(): number { return renamed(); }\n\
             export function t_external(): string { return fs.realpathSync(\".\"); }\n\
             export function t_dynamic(o: any): number { return o.whatever(); }\n",
        ),
    ]);
    assert_eq!(ts.index().failed, 0);
    for (caller, target, rule) in [
        ("t_same", "src::main::localCallee", LexicalScope),
        ("t_import", "src::util::helper", ImportBinding),
        ("t_alias", "src::util::other", AliasBinding),
    ] {
        assert_eq!(
            resolved(&ts, caller),
            Some((target.into(), rule)),
            "typescript {caller}"
        );
    }
    assert_eq!(resolved(&ts, "t_external"), None, "package import");
    assert_eq!(resolved(&ts, "t_dynamic"), None, "dynamic member call");
}

/// Native: the real Rust semantic provider proves relations the deterministic
/// pass cannot, and its absence or failure never destroys the graph.
///
/// Ignored by default because it shells out to an optional external tool that
/// is not present in every environment; the enrichment logic itself is covered
/// deterministically by the in-crate `graph::semantic` tests.
#[test]
#[ignore = "native: requires an installed rust-analyzer semantic provider"]
fn native_rust_semantic_provider_proves_method_dispatch_and_degrades_safely() {
    let f = Fixture::new(&[
        (
            "Cargo.toml",
            "[package]\nname=\"fx\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        ),
        ("src/lib.rs", "pub mod shapes;\n"),
        (
            "src/shapes.rs",
            "pub trait Area { fn area(&self) -> u32; }\n\
             pub struct Square { pub e: u32 }\n\
             impl Area for Square { fn area(&self) -> u32 { self.e } }\n\
             pub fn measure(s: &Square) -> u32 { s.area() }\n",
        ),
    ]);
    assert_eq!(f.index().failed, 0);

    // Syntax alone cannot bind a method call through a trait.
    let method = "src::shapes::impl Area for Square::area";
    let before = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations(method, true, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    assert!(before.is_empty(), "{before:?}");

    let outcome = f.store().enrich_semantic(&f.root).unwrap();
    let [graph::SemanticOutcome::Current { resolved, .. }] = &outcome[..] else {
        panic!("rust-analyzer must be installed for this native test: {outcome:?}");
    };
    assert!(*resolved >= 1, "{outcome:?}");
    let after = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations(method, true, Some(RelationKind::Calls), 10)
        .unwrap()
        .data;
    assert_eq!(after.len(), 1, "the trait-dispatched call must be proven");
    assert_eq!(
        after[0].resolution,
        Some(graph::ResolutionRule::SemanticProvider)
    );

    // Broken source: the structural graph survives, and whatever the provider
    // can no longer prove degrades to unknown rather than to "no relation".
    write(&f.root, "src/broken.rs", "pub fn broken( { let x = ;\n");
    let stats = f.store().index_repository(&f.root).unwrap();
    assert_eq!(stats.failed, 1, "the broken file fails on its own");
    assert!(
        stats.entities > 0 && stats.reused > 0,
        "unaffected files keep their facts: {stats:?}"
    );
    // Enrichment over broken source must not remove what is already known.
    let entities_before = f.store().index_status(&f.root).unwrap().entities;
    let _ = f.store().enrich_semantic(&f.root);
    assert_eq!(
        f.store().index_status(&f.root).unwrap().entities,
        entities_before,
        "a provider run must never cost the graph facts it already had"
    );
}

/// Every IMPORTS relation a file observes, as `(imported path, resolved target
/// key, rule)`. The whole set is compared, so an unexpected resolution fails as
/// loudly as a missing one.
fn imports_of(f: &Fixture, path: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let mut rows: Vec<(String, Option<String>, Option<String>)> = f
        .sql()
        .prepare(
            "SELECT json_extract(e.record_json,'$.target_name'), \
                    (SELECT json_extract(t.record_json,'$.key') FROM graph_entities t \
                      WHERE t.workspace_id=r.workspace_id AND t.entity_id=r.target_id), \
                    r.rule \
             FROM graph_edges e LEFT JOIN graph_resolutions r \
               ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id \
             WHERE e.path=?1 AND e.kind='\"IMPORTS\"'",
        )
        .unwrap()
        .query_map([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

fn expect(rows: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>, Option<String>)> {
    let mut rows: Vec<_> = rows
        .iter()
        .map(|(name, key)| {
            (
                name.to_string(),
                key.map(str::to_string),
                key.map(|_| "IMPORT_BINDING".to_string()),
            )
        })
        .collect();
    rows.sort();
    rows
}

/// IMPORTS relations resolve to the declaration the import path proves, per
/// language, and to nothing when the path leaves the repository or is
/// ambiguous. Duplicate names elsewhere in the workspace are deliberate traps:
/// resolving by name instead of by path would pick them.
#[test]
fn imports_resolve_to_the_proven_target_and_nothing_else() {
    // ---- Rust ----
    let rust = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod util;\npub mod other;\npub mod shapes;\npub mod deep;\npub mod consumer;\n\
             pub mod a;\npub mod b;\n",
        ),
        ("src/util.rs", "pub fn helper() -> u32 { 1 }\n"),
        // Same leaf name, different module: a name match would be ambiguous.
        ("src/other.rs", "pub fn helper() -> u32 { 2 }\n"),
        (
            "src/shapes.rs",
            "pub struct Square;\nimpl Square { pub fn side(&self) -> u32 { 1 } }\n\
             pub struct Circle;\n",
        ),
        ("src/deep.rs", "pub mod inner;\n"),
        ("src/deep/inner.rs", "pub fn buried() -> u32 { 3 }\n"),
        ("src/a/dup.rs", "pub struct Thing;\n"),
        ("src/b/dup.rs", "pub struct Thing;\n"),
        (
            "src/consumer.rs",
            "use crate::shapes;\n\
             use crate::shapes::{Square, Circle as Round};\n\
             use super::util::helper as h;\n\
             use crate::deep::inner::{self};\n\
             use serde::Serialize;\n\
             use std::collections::HashMap;\n\
             use ::ext::thing;\n\
             use crate::shapes::*;\n\
             use dup::Thing;\n",
        ),
    ]);
    assert_eq!(rust.index().failed, 0);
    let mut rows = imports_of(&rust, "src/consumer.rs");
    // Statement-level observations of what cannot be followed keep their text.
    let unfollowable: Vec<_> = rows
        .iter()
        .filter(|(name, ..)| name.starts_with("use "))
        .cloned()
        .collect();
    assert_eq!(
        unfollowable.len(),
        2,
        "extern-root and glob: {unfollowable:?}"
    );
    assert!(unfollowable.iter().all(|(_, key, _)| key.is_none()));
    rows.retain(|(name, ..)| !name.starts_with("use "));
    assert_eq!(
        rows,
        expect(&[
            // module import
            ("crate::shapes", Some("src::shapes")),
            // item imports, one relation each, from a use list
            ("crate::shapes::Square", Some("src::shapes::Square")),
            // an alias never replaces what was imported
            ("crate::shapes::Circle", Some("src::shapes::Circle")),
            ("super::util::helper", Some("src::util::helper")),
            // `{self}` imports the module itself
            ("crate::deep::inner", Some("src::deep::inner")),
            // external crates and std stay unknown
            ("serde::Serialize", None),
            ("std::collections::HashMap", None),
            // two in-crate `dup::Thing`s: ambiguous, so unknown
            ("dup::Thing", None),
        ])
    );

    // ---- Python ----
    let py = Fixture::new(&[
        (
            "pkg/__init__.py",
            "from .shapes import Square\nfrom .missing import nothing\n",
        ),
        (
            "pkg/shapes.py",
            "class Square:\n    pass\n\n\nclass Circle:\n    pass\n",
        ),
        ("pkg/util.py", "def helper():\n    return 1\n"),
        // Same leaf names at the root: a trap for name matching.
        ("util.py", "def helper():\n    return 2\n"),
        ("a/common.py", "def f():\n    return 1\n"),
        ("b/common.py", "def f():\n    return 2\n"),
        (
            "app.py",
            "import pkg.util\n\
             import pkg.shapes as sh\n\
             from pkg.util import helper\n\
             from pkg.shapes import Circle as C\n\
             from pkg import Square\n\
             import util\n\
             import requests\n\
             from os.path import join\n\
             from common import f\n\
             from pkg.shapes import *\n\n\
             def run():\n    return pkg.util.helper() + helper() + util.helper()\n",
        ),
    ]);
    assert_eq!(py.index().failed, 0);
    let mut rows = imports_of(&py, "app.py");
    let glob = rows
        .iter()
        .position(|(n, ..)| n.starts_with("from pkg.shapes import *"));
    assert!(glob.is_some_and(|i| rows[i].1.is_none()), "{rows:?}");
    rows.remove(glob.unwrap());
    assert_eq!(
        rows,
        expect(&[
            // module import
            ("pkg::util", Some("pkg::util")),
            // aliased module import: the alias is not the target
            ("pkg::shapes", Some("pkg::shapes")),
            // from-import of an item, not the root `util.helper` trap
            ("pkg::util::helper", Some("pkg::util::helper")),
            ("pkg::shapes::Circle", Some("pkg::shapes::Circle")),
            // re-exported through `pkg/__init__.py`: syntax cannot follow it
            ("pkg::Square", None),
            // a top-level module is anchored at the repository root
            ("util", Some("util")),
            // external packages
            ("requests", None),
            ("os::path::join", None),
            // `common` exists twice; which one Python finds is not provable
            ("common::f", None),
        ])
    );
    // Relative imports are anchored at the importing file's package.
    assert_eq!(
        imports_of(&py, "pkg/__init__.py"),
        expect(&[
            ("pkg::shapes::Square", Some("pkg::shapes::Square")),
            ("pkg::missing::nothing", None),
        ])
    );
    // `import pkg.util` binds `pkg`, so the qualified call still resolves, and
    // the from-import wins over the same-named root module.
    let callees: Vec<String> = py
        .store()
        .graph(&py.root)
        .unwrap()
        .relations("app::run", false, Some(RelationKind::Calls), 10)
        .unwrap()
        .data
        .iter()
        .filter_map(|e| e.target.clone())
        .map(|t| {
            py.sql()
                .query_row(
                    "SELECT qualified_name FROM graph_entities WHERE entity_id=?1",
                    [t.as_str()],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .collect();
    let mut callees = callees;
    callees.sort();
    assert_eq!(
        callees,
        ["pkg::util::helper", "pkg::util::helper", "util::helper"]
    );

    // ---- TypeScript ----
    let ts = Fixture::new(&[
        (
            "src/util.ts",
            "export function helper(): number { return 1; }\n\
             export function other(): number { return 2; }\n",
        ),
        // Same file stem and function name in another directory.
        (
            "src/lib/util.ts",
            "export function helper(): number { return 3; }\n",
        ),
        (
            "src/lib/index.ts",
            "export function entry(): number { return 4; }\n",
        ),
        (
            "src/main.ts",
            "import { helper } from \"./util\";\n\
             import { other as renamed } from \"./util\";\n\
             import * as lib from \"./lib/util\";\n\
             import entry from \"./lib\";\n\
             import \"./util.js\";\n\
             import React from \"react\";\n\
             import { missing } from \"./util\";\n",
        ),
        ("src/lib/deep.ts", "import { helper } from \"../util\";\n"),
    ]);
    assert_eq!(ts.index().failed, 0);
    let mut rows = imports_of(&ts, "src/main.ts");
    let package = rows.iter().position(|(n, ..)| n.contains("react"));
    assert!(package.is_some_and(|i| rows[i].1.is_none()), "{rows:?}");
    rows.remove(package.unwrap());
    assert_eq!(
        rows,
        expect(&[
            // named import, anchored at the importing file: not src/lib/util.ts
            ("src::util::helper", Some("src::util::helper")),
            // aliased named import
            ("src::util::other", Some("src::util::other")),
            // relative module imports (namespace, directory index, side effect)
            ("src::lib::util", Some("src::lib::util")),
            ("src::lib", Some("src::lib")),
            ("src::util", Some("src::util")),
            // a name the module does not declare
            ("src::util::missing", None),
        ])
    );
    assert_eq!(
        imports_of(&ts, "src/lib/deep.ts"),
        expect(&[("src::util::helper", Some("src::util::helper"))])
    );
}

// ---- Python semantic provider ----
//
// The same fixture the recorded `tests/fixtures/python-semantic.scip` was
// produced from by `scip-python 0.6.6` (Pyright), invoked exactly as agentctl
// invokes it. Every relation below is either provable from syntax (and must
// stay a fast-pass relation), provable only by a type-aware engine, or
// unprovable by either (and must stay UNKNOWN).
const PY_APP: &str = "import json\nfrom pkg import Square\nfrom pkg.shapes import Tile\nfrom pkg.util import helper\n\n\n\
def fast():\n    return helper()\n\n\n\
def annotated(s: Square):\n    return s.area()\n\n\n\
def inferred():\n    sq = Square(2)\n    return sq.area()\n\n\n\
def inherited():\n    return Tile(3).area()\n\n\n\
def duck(x):\n    return x.area()\n\n\n\
def dynamic(obj):\n    return getattr(obj, \"area\")()\n\n\n\
def external():\n    return json.dumps({})\n";
const PY_FILES: &[(&str, &str)] = &[
    ("app.py", PY_APP),
    ("pkg/__init__.py", "from .shapes import Square\n"),
    (
        "pkg/shapes.py",
        "class Square:\n    def __init__(self, e):\n        self.e = e\n\n    def area(self):\n        return self.e * self.e\n\n\n\
class Tile(Square):\n    pass\n\n\nclass Circle:\n    def area(self):\n        return 3\n",
    ),
    ("pkg/util.py", "def helper():\n    return 1\n"),
];

/// Where syntax and the provider each stand, per relation site.
type Row = (String, String, usize, String, String, Option<String>);

fn relation_table(c: &Connection) -> Vec<Row> {
    let mut rows: Vec<Row> = c
        .prepare(
            "SELECT e.path, replace(e.kind,'\"',''), json_extract(e.record_json,'$.range.start_line'), \
                    json_extract(e.record_json,'$.target_name'), \
                    coalesce(r.rule, json_extract(e.record_json,'$.resolution'), 'UNKNOWN'), t.qualified_name \
             FROM graph_edges e LEFT JOIN graph_resolutions r ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id \
             LEFT JOIN graph_entities t ON t.workspace_id=e.workspace_id AND t.entity_id=coalesce(e.target_id,r.target_id) \
             WHERE e.kind IN ('\"CALLS\"','\"IMPORTS\"','\"REFERENCES\"')",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

fn row(path: &str, kind: &str, line: usize, name: &str, rule: &str, target: Option<&str>) -> Row {
    (
        path.into(),
        kind.into(),
        line,
        name.into(),
        rule.into(),
        target.map(Into::into),
    )
}

/// The fast pass alone: what syntax, modules and imports prove.
fn python_fast_table() -> Vec<Row> {
    let mut rows = vec![
        row("app.py", "IMPORTS", 1, "json", "UNKNOWN", None),
        // A re-export through `pkg/__init__.py`: syntax does not follow it.
        row("app.py", "IMPORTS", 2, "pkg::Square", "UNKNOWN", None),
        row(
            "app.py",
            "IMPORTS",
            3,
            "pkg::shapes::Tile",
            "IMPORT_BINDING",
            Some("pkg::shapes::Tile"),
        ),
        row(
            "app.py",
            "IMPORTS",
            4,
            "pkg::util::helper",
            "IMPORT_BINDING",
            Some("pkg::util::helper"),
        ),
        row(
            "app.py",
            "CALLS",
            8,
            "helper",
            "IMPORT_BINDING",
            Some("pkg::util::helper"),
        ),
        // Receiver types: annotated, inferred, inherited.
        row("app.py", "CALLS", 12, "s.area", "UNKNOWN", None),
        row("app.py", "CALLS", 16, "Square", "UNKNOWN", None),
        row("app.py", "CALLS", 17, "sq.area", "UNKNOWN", None),
        row("app.py", "CALLS", 21, "().area", "UNKNOWN", None),
        row(
            "app.py",
            "CALLS",
            21,
            "Tile",
            "IMPORT_BINDING",
            Some("pkg::shapes::Tile"),
        ),
        // Unprovable by anyone: duck typing, reflection, outside the repo.
        row("app.py", "CALLS", 25, "x.area", "UNKNOWN", None),
        row("app.py", "CALLS", 29, "getattr", "UNKNOWN", None),
        row(
            "app.py",
            "CALLS",
            29,
            "getattr(obj, \"area\")",
            "UNKNOWN",
            None,
        ),
        row("app.py", "CALLS", 33, "json.dumps", "UNKNOWN", None),
        row(
            "pkg/__init__.py",
            "IMPORTS",
            1,
            "pkg::shapes::Square",
            "IMPORT_BINDING",
            Some("pkg::shapes::Square"),
        ),
        row("pkg/shapes.py", "REFERENCES", 9, "Square", "UNKNOWN", None),
    ];
    rows.sort();
    rows
}

/// What a type-aware engine adds: exactly the receiver-type, re-export and
/// inheritance relations, and nothing where Python itself cannot know.
fn python_semantic_table() -> Vec<Row> {
    let semantic = |path, kind, line, name, target| {
        row(path, kind, line, name, "SEMANTIC_PROVIDER", Some(target))
    };
    let proven = [
        semantic("app.py", "IMPORTS", 2, "pkg::Square", "pkg::shapes::Square"),
        semantic("app.py", "CALLS", 12, "s.area", "pkg::shapes::Square::area"),
        semantic("app.py", "CALLS", 16, "Square", "pkg::shapes::Square"),
        semantic(
            "app.py",
            "CALLS",
            17,
            "sq.area",
            "pkg::shapes::Square::area",
        ),
        semantic(
            "app.py",
            "CALLS",
            21,
            "().area",
            "pkg::shapes::Square::area",
        ),
        semantic(
            "pkg/shapes.py",
            "REFERENCES",
            9,
            "Square",
            "pkg::shapes::Square",
        ),
    ];
    let mut rows: Vec<Row> = python_fast_table()
        .into_iter()
        .filter(|r| {
            !proven
                .iter()
                .any(|p| (&p.0, &p.1, p.2, &p.3) == (&r.0, &r.1, r.2, &r.3))
        })
        .collect();
    rows.extend(proven);
    rows.sort();
    rows
}

/// A stand-in for `scip-python` on PATH, so the provider boundary itself is
/// exercised through the real CLI without the real tool.
fn fake_provider(f: &Fixture, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = f.temp.0.join(format!("fake-bin-{}", body.len()));
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("scip-python");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'scip-python fake'; exit 0; fi\n\
             for a; do out=$a; done\n{body}\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), path_without_scip_python())
}

/// This process's PATH minus any directory holding a real `scip-python`.
fn path_without_scip_python() -> String {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !Path::new(dir).join("scip-python").exists())
        .collect::<Vec<_>>()
        .join(":")
}

fn recorded_index() -> String {
    format!(
        "{}/tests/fixtures/python-semantic.scip",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn python_fixture() -> Fixture {
    let f = Fixture::new(PY_FILES);
    cli_json(&f, &f.root, &["init", "--json"]);
    cli_json(&f, &f.root, &["repo", "init", "--json"]);
    cli_json(&f, &f.root, &["repo", "index", "--json"]);
    f
}

fn cli_state(f: &Fixture) -> Connection {
    common::sql(&f.temp.0.join("data/agentctl/state.sqlite3"))
}

fn table_via_cli(f: &Fixture) -> Vec<Row> {
    relation_table(&cli_state(f))
}

fn enrich(f: &Fixture, path: &str) -> Value {
    cli_json(f, &f.root, &["repo", "enrich", "--json"])
        .as_array()
        .cloned()
        .map(Value::Array)
        .unwrap_or_else(|| panic!("{path}"))
}

/// Python semantic facts enter the one canonical graph through the same
/// provider boundary as Rust: attributed rows in `graph_resolutions`, no new
/// table, and nothing beyond what the engine proved.
#[test]
fn python_semantic_facts_enrich_the_same_graph_through_the_provider_boundary() {
    let f = python_fixture();
    assert_eq!(table_via_cli(&f), python_fast_table(), "fast pass");
    let tables = |c: &Connection| -> Vec<String> {
        c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let before = tables(&cli_state(&f));

    let path = fake_provider(&f, "cp \"$AGENTCTL_TEST_SCIP\" \"$out\"");
    let index = recorded_index();
    let output = cli_env(
        &f,
        &f.root,
        &["repo", "enrich", "--json"],
        &[("PATH", &path), ("AGENTCTL_TEST_SCIP", &index)],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome[0]["state"], "CURRENT", "{outcome}");
    assert_eq!(outcome[0]["languages"], serde_json::json!(["python"]));
    assert_eq!(outcome[0]["resolved"], 6, "{outcome}");

    assert_eq!(table_via_cli(&f), python_semantic_table(), "enriched");
    // Attributed, and in the canonical tables only.
    let providers: Vec<Option<String>> = cli_state(&f)
        .prepare("SELECT DISTINCT provider FROM graph_resolutions WHERE rule='SEMANTIC_PROVIDER'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(providers, [Some("scip-python fake".to_string())]);
    assert_eq!(tables(&cli_state(&f)), before, "no parallel semantic state");

    // The ordinary relation query sees the proven callers, with no special case.
    let callers = cli_json(
        &f,
        &f.root,
        &["code", "callers", "pkg::shapes::Square::area", "--json"],
    );
    let mut sources: Vec<String> = callers["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target_name"].as_str().unwrap().to_string())
        .collect();
    sources.sort();
    assert_eq!(sources, ["().area", "s.area", "sq.area"]);

    // Idempotent: a second run proves nothing new and duplicates nothing.
    let again: Value = serde_json::from_slice(
        &cli_env(
            &f,
            &f.root,
            &["repo", "enrich", "--json"],
            &[("PATH", &path), ("AGENTCTL_TEST_SCIP", &index)],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(again[0]["resolved"], 0);
    assert_eq!(table_via_cli(&f), python_semantic_table());
}

/// A provider that is absent, crashes, emits garbage, or runs while the
/// sources change proves nothing: UNKNOWN stays UNKNOWN, never becomes
/// "no relation", and the structural graph is untouched.
#[test]
fn semantic_provider_failures_leave_unknown_unknown_and_the_graph_intact() {
    let f = python_fixture();
    let fast = python_fast_table();
    let entities = |f: &Fixture| -> usize {
        cli_state(f)
            .query_row("SELECT count(*) FROM graph_entities", [], |r| r.get(0))
            .unwrap()
    };
    let before = entities(&f);
    for (body, reason) in [
        ("", "not installed"),
        ("exit 3", "exited 3 without a readable index"),
        (
            "printf '\\377\\377\\377' > \"$out\"",
            "not a readable SCIP document",
        ),
        // Last: it leaves the index stale on purpose.
        (
            "echo '# edited' >> app.py; cp \"$AGENTCTL_TEST_SCIP\" \"$out\"",
            "sources changed while",
        ),
    ] {
        if reason == "sources changed while" {
            // UNKNOWN is reported as unknown, not as "no callers".
            let callers = cli_json(
                &f,
                &f.root,
                &["code", "callers", "pkg::shapes::Square::area", "--json"],
            );
            assert!(callers["data"].as_array().unwrap().is_empty());
            assert!(
                callers["unresolved"]["sites"].as_u64().unwrap() >= 3,
                "{callers}"
            );
        }
        let path = if body.is_empty() {
            path_without_scip_python()
        } else {
            fake_provider(&f, body)
        };
        let output = cli_env(
            &f,
            &f.root,
            &["repo", "enrich", "--json"],
            &[("PATH", &path), ("AGENTCTL_TEST_SCIP", &recorded_index())],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(outcome[0]["state"], "UNAVAILABLE", "{body}: {outcome}");
        assert!(
            outcome[0]["reason"].as_str().unwrap().contains(reason),
            "{body}: {outcome}"
        );
        assert_eq!(
            table_via_cli(&f),
            fast,
            "{body}: nothing proven, nothing lost"
        );
        assert_eq!(entities(&f), before, "{body}: structural graph intact");
    }

    // The source edited mid-run is now stale: enrichment refuses outright
    // rather than attach facts to a generation that no longer exists.
    let stale = cli(&f, &f.root, &["repo", "enrich", "--json"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("fresh index"));

    // Broken source: indexing keeps the other files' facts, and enrichment
    // refuses rather than enrich a partial generation.
    fs::write(f.root.join("app.py"), "def broken(:\n").unwrap();
    let partial = cli(&f, &f.root, &["repo", "index", "--json"]);
    assert!(!partial.status.success());
    assert!(String::from_utf8_lossy(&partial.stderr).contains("partial"));
    assert!(entities(&f) > 0, "other files keep their facts");
    assert!(
        !cli(&f, &f.root, &["repo", "enrich", "--json"])
            .status
            .success()
    );

    // A language with no provider at all says so, per language.
    let ts = Fixture::new(&[("web/a.ts", "export function f(): number { return 1; }\n")]);
    cli_json(&ts, &ts.root, &["init", "--json"]);
    cli_json(&ts, &ts.root, &["repo", "init", "--json"]);
    cli_json(&ts, &ts.root, &["repo", "index", "--json"]);
    let outcome = enrich(&ts, "ts");
    assert_eq!(outcome[0]["state"], "UNAVAILABLE");
    assert_eq!(outcome[0]["languages"], serde_json::json!(["type_script"]));
}

/// An index recorded against other source lines — here, every line of
/// `app.py` shifted by one — attaches nothing to the file it no longer
/// describes, while the unchanged files it still describes are proven.
#[test]
fn a_stale_semantic_index_attaches_nothing_to_source_it_does_not_describe() {
    let mut files = PY_FILES.to_vec();
    let shifted = format!("# a new first line\n{PY_APP}");
    files[0] = ("app.py", &shifted);
    let f = Fixture::new(&files);
    cli_json(&f, &f.root, &["init", "--json"]);
    cli_json(&f, &f.root, &["repo", "init", "--json"]);
    cli_json(&f, &f.root, &["repo", "index", "--json"]);
    let path = fake_provider(&f, "cp \"$AGENTCTL_TEST_SCIP\" \"$out\"");
    let output = cli_env(
        &f,
        &f.root,
        &["repo", "enrich", "--json"],
        &[("PATH", &path), ("AGENTCTL_TEST_SCIP", &recorded_index())],
    );
    assert!(output.status.success());
    let semantic: Vec<Row> = table_via_cli(&f)
        .into_iter()
        .filter(|r| r.4 == "SEMANTIC_PROVIDER")
        .collect();
    assert_eq!(
        semantic,
        [row(
            "pkg/shapes.py",
            "REFERENCES",
            9,
            "Square",
            "SEMANTIC_PROVIDER",
            Some("pkg::shapes::Square")
        )]
    );
}

/// Native: the real Pyright-based provider proves exactly what the recorded
/// index does. Ignored by default: it needs `scip-python` on PATH.
#[test]
#[ignore = "native: requires scip-python (npm @sourcegraph/scip-python) on PATH"]
fn native_python_semantic_provider_proves_what_syntax_cannot() {
    let f = python_fixture();
    let outcome = enrich(&f, "native");
    assert_eq!(
        outcome[0]["state"], "CURRENT",
        "scip-python must be on PATH: {outcome}"
    );
    assert_eq!(table_via_cli(&f), python_semantic_table());
}

const MACROS: &str = r#"pub mod a;

pub fn foo() -> u32 {
    1
}

pub fn bar(x: u32) -> u32 {
    x
}

pub fn quiet() -> u32 {
    7
}

pub fn uses() -> String {
    assert!(foo() > 0);
    assert_eq!(foo(), bar(1));
    let s = format!("{} {}", foo(), a::name().len());
    let j = json!({"x": bar(2)});
    debug_assert!(matches!(Some(a::foo()), Some(2)));
    let _plain = vec![1, 2, 3];
    println!("{}", "x".len());
    weird!(=> foo() ;; bar ,, [a::name()]);
    format!("{s}{j}")
}
"#;
const MACROS_A: &str =
    "pub fn name() -> String {\n    String::new()\n}\n\npub fn foo() -> u32 {\n    2\n}\n";

/// `(line, target name)` of every unresolved CALLS site in a file.
fn unresolved_calls(f: &Fixture, path: &str) -> Vec<(i64, String)> {
    let mut rows: Vec<(i64, String)> = f
        .sql()
        .prepare(
            "SELECT json_extract(e.record_json,'$.range.start_line'), json_extract(e.record_json,'$.target_name') \
             FROM graph_edges e WHERE e.path=?1 AND e.kind='\"CALLS\"' AND e.target_id IS NULL \
               AND json_extract(e.record_json,'$.path_hint') IS NULL \
               AND NOT EXISTS (SELECT 1 FROM graph_resolutions r WHERE r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id)",
        )
        .unwrap()
        .query_map([path], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

/// Calls inside macro invocations are never silently absent. Each
/// call-shaped token sequence becomes an unresolved site (never a fabricated
/// target), including nested macros and recoverable odd syntax; token trees
/// without calls, and a function nothing names, stay quiet.
#[test]
fn macro_contained_calls_are_honest_unknown_sites_never_guessed_targets() {
    let f = Fixture::new(&[("src/lib.rs", MACROS), ("src/a.rs", MACROS_A)]);
    assert_eq!(f.index().failed, 0);
    let sites = unresolved_calls(&f, "src/lib.rs");
    let at = |line: i64| -> Vec<&str> {
        sites
            .iter()
            .filter(|(l, _)| *l == line)
            .map(|(_, n)| n.as_str())
            .collect()
    };
    assert_eq!(at(16), ["foo"], "assert!");
    assert_eq!(at(17), ["bar", "foo"], "assert_eq!");
    assert_eq!(at(18), ["().len", "a::name", "foo"], "format! with a chain");
    assert_eq!(at(19), ["bar"], "json!");
    assert_eq!(
        at(20),
        ["Some", "Some", "a::foo"],
        "nested macro, duplicate name kept qualified"
    );
    assert!(
        at(21).is_empty(),
        "a token tree without calls observes nothing"
    );
    assert_eq!(at(22), ["().len"], "external-only content stays unknown");
    assert_eq!(at(23), ["a::name", "foo"], "recoverable odd syntax");

    // No macro site was resolved syntactically: none has a target.
    let store = f.store();
    let graph = store.graph(&f.root).unwrap();
    let callers = graph
        .relations("src::lib::foo", true, Some(RelationKind::Calls), 20)
        .unwrap();
    assert!(callers.data.is_empty(), "{:?}", callers.data);
    let unresolved = callers
        .unresolved
        .expect("macro calls are reported unproven");
    assert!(unresolved.sites >= 4, "{unresolved:?}");
    assert!(unresolved.paths.iter().any(|p| p == "src/lib.rs"));

    // Impact states the open question instead of claiming no dependents.
    let report = f
        .store()
        .ontology_impact(
            &f.root,
            &graph::ImpactRequest::Symbols(vec!["src::lib::bar".into()]),
            graph::ImpactLimits::default(),
        )
        .unwrap();
    let text = serde_json::to_string(&report.boundaries).unwrap();
    assert!(text.contains("UNRESOLVED_REFERENCES"), "{text}");

    // A function nothing names at all has nothing unproven to report.
    let quiet = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("src::lib::quiet", true, Some(RelationKind::Calls), 20)
        .unwrap();
    assert!(quiet.data.is_empty() && quiet.unresolved.is_none());
}

/// Known boundary, deliberately not modelled: `#[cfg]`-selected re-exports.
/// Syntax sees both platform variants, so a call through the re-export is left
/// unresolved rather than bound to either, and each variant reports the open
/// question. (A semantic provider follows the host configuration only.)
#[test]
fn cfg_selected_reexport_stays_unknown_for_every_variant() {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "pub mod plat;\n\npub fn top() -> u32 {\n    plat::open()\n}\n",
        ),
        (
            "src/plat/mod.rs",
            "#[cfg(unix)]\nmod unix;\n#[cfg(windows)]\nmod windows;\n#[cfg(unix)]\npub use unix::*;\n#[cfg(windows)]\npub use windows::*;\n",
        ),
        ("src/plat/unix.rs", "pub fn open() -> u32 {\n    10\n}\n"),
        ("src/plat/windows.rs", "pub fn open() -> u32 {\n    20\n}\n"),
    ]);
    assert_eq!(f.index().failed, 0);
    for variant in ["src::plat::unix::open", "src::plat::windows::open"] {
        let callers = f
            .store()
            .graph(&f.root)
            .unwrap()
            .relations(variant, true, Some(RelationKind::Calls), 20)
            .unwrap();
        assert!(callers.data.is_empty(), "{variant}: {:?}", callers.data);
        assert!(
            callers.unresolved.is_some(),
            "{variant} must report the open question"
        );
    }
}

/// Semantic enrichment is explicit graph state bound to one
/// generation; a re-index that re-derives anything drops it together with the
/// semantic rows, and one that changes nothing keeps it.
#[test]
#[ignore = "native: requires an installed rust-analyzer semantic provider"]
fn native_semantic_enrichment_is_generation_bound_and_proves_macro_calls() {
    let f = Fixture::new(&[
        (
            "Cargo.toml",
            "[package]\nname=\"fx\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod a;\npub fn foo() -> u32 { 1 }\npub fn uses() -> String {\n    assert_eq!(foo(), 1);\n    format!(\"{}{}\", foo(), a::foo())\n}\n",
        ),
        ("src/a.rs", "pub fn foo() -> u32 { 2 }\n"),
    ]);
    assert_eq!(f.index().failed, 0);
    assert!(
        f.store()
            .index_status(&f.root)
            .unwrap()
            .semantic()
            .is_none()
    );
    let outcome = f.store().enrich_semantic(&f.root).unwrap();
    assert!(
        matches!(&outcome[..], [graph::SemanticOutcome::Current { .. }]),
        "{outcome:?}"
    );
    assert!(
        f.store()
            .index_status(&f.root)
            .unwrap()
            .semantic()
            .is_some()
    );
    // The provider proves the macro-contained calls, each to its own target.
    let store = f.store();
    let graph = store.graph(&f.root).unwrap();
    let root_callers = graph
        .relations("src::lib::foo", true, Some(RelationKind::Calls), 20)
        .unwrap();
    assert_eq!(root_callers.data.len(), 2, "{:?}", root_callers.data);
    assert!(
        root_callers
            .data
            .iter()
            .all(|e| e.resolution == Some(graph::ResolutionRule::SemanticProvider))
    );
    let a_callers = f
        .store()
        .graph(&f.root)
        .unwrap()
        .relations("src::a::foo", true, Some(RelationKind::Calls), 20)
        .unwrap();
    assert_eq!(a_callers.data.len(), 1, "{:?}", a_callers.data);
    // Re-indexing unchanged source keeps the stamp; a change drops it.
    f.index();
    assert!(
        f.store()
            .index_status(&f.root)
            .unwrap()
            .semantic()
            .is_some()
    );
    write(&f.root, "src/a.rs", "pub fn foo() -> u32 { 3 }\n");
    f.index();
    assert!(
        f.store()
            .index_status(&f.root)
            .unwrap()
            .semantic()
            .is_none()
    );
    let packet = f
        .store()
        .graph(&f.root)
        .unwrap()
        .context("foo", ContextLimits::default())
        .unwrap();
    assert!(
        !packet.coverage.semantic,
        "structural only after re-derivation"
    );
}

/// A provider may name a symbol the way its source language does.
/// `crate::…` and `<crate name>::…` paths, module-relative paths and
/// `Type::method` map deterministically onto the one canonical entity; a short
/// name shared by two entities stays ambiguous and an unknown path finds
/// nothing. No path is ever resolved by guessing.
#[test]
fn source_language_paths_resolve_to_one_canonical_entity_or_nothing() {
    let f = Fixture::new(&[
        (
            "Cargo.toml",
            "[package]\nname=\"textstats\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        ),
        (
            "src/lib.rs",
            "pub mod numbers;\npub mod words;\npub fn summary() {}\n",
        ),
        (
            "src/words.rs",
            "pub fn shortest() {}\npub struct Counter;\nimpl Counter {\n    pub fn count(&self) {}\n}\n",
        ),
        ("src/numbers.rs", "pub fn shortest() {}\n"),
    ]);
    assert_eq!(f.index().failed, 0);
    let store = f.store();
    let graph = store.graph(&f.root).unwrap();
    let one = |name: &str| -> Vec<String> {
        graph
            .exact_matches(name, 8)
            .unwrap()
            .into_iter()
            .map(|e| e.qualified_name)
            .collect()
    };
    for path in [
        "crate::words::shortest",
        "textstats::words::shortest",
        "words::shortest",
        "src::words::shortest",
        "textstats::words::shortest()",
    ] {
        assert_eq!(one(path), ["src::words::shortest"], "{path}");
    }
    assert_eq!(one("textstats::summary"), ["src::lib::summary"]);
    assert_eq!(one("crate::summary"), ["src::lib::summary"]);
    for path in [
        "Counter::count",
        "words::Counter::count",
        "textstats::words::Counter::count",
    ] {
        assert_eq!(one(path), ["src::words::impl Counter::count"], "{path}");
    }
    // A short name shared by two entities stays ambiguous, never guessed; a
    // path that names one of them is not.
    assert_eq!(one("shortest").len(), 2);
    assert_eq!(one("numbers::shortest"), ["src::numbers::shortest"]);
    // Unknown or near-miss paths find nothing.
    for path in [
        "textstats::nope::shortest",
        "words::longest",
        "numbers::Counter::count",
    ] {
        assert!(one(path).is_empty(), "{path}");
    }
}
