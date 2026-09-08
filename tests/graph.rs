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
        Connection::open(&self.database).unwrap()
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
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .current_dir(root)
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
    assert!(
        store
            .graph(&f.root)
            .unwrap()
            .relations("resolveCandidate", true, Some(RelationKind::Calls), 10)
            .unwrap()
            .data
            .is_empty()
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
    f.store()
        .create_plan(&info.repository_id, &common::plan(), 10)
        .unwrap();
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
