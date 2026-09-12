mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Barrier},
};

use agentctl::{
    local::{
        config::{MachineConfig, ProjectConfig},
        paths::{MachinePaths, PathContext},
        repository::RepositoryInfo,
        store::{DATABASE_VERSION, JournalEntry, Store},
    },
    protocol::*,
};
use common::{TempDir, decode, finding, plan, samples, verification};
use rusqlite::Connection;
use serde_json::{Value, json};

struct Fixture {
    _temp: TempDir,
    paths: MachinePaths,
    root: PathBuf,
    info: RepositoryInfo,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new();
        let paths = MachinePaths::resolve(&PathContext {
            home: Some(temp.0.join("home")),
            ..PathContext::default()
        })
        .unwrap();
        paths.create_directories().unwrap();
        MachineConfig::initialize(&paths.machine_config).unwrap();
        let root = temp.0.join("repo");
        init_git(&root);
        let info = RepositoryInfo::discover(&root).unwrap();
        let mut store = Store::open(&paths.database, 5000).unwrap();
        store.register_repository(info.clone()).unwrap();
        Self {
            _temp: temp,
            paths,
            root,
            info,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.paths.database, 5000).unwrap()
    }
    fn repo(&self) -> &agentctl::local::repository::RepositoryId {
        &self.info.repository_id
    }
}

fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Substrate Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init_git(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(
        root,
        &[
            "-c",
            "init.templateDir=",
            "init",
            "--quiet",
            "--initial-branch=main",
        ],
    );
    git(root, &["config", "core.excludesFile", "/dev/null"]);
}

fn commit(root: &Path) {
    fs::write(root.join("tracked.txt"), "initial\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "--quiet", "-m", "initial"]);
}

fn queued_job(id: &str, role: AgentRole) -> AgentJob {
    let mut value: AgentJob = decode(samples()["agent-job"].clone());
    value.job_id = JobId::new(id).unwrap();
    value.role = role;
    value.state = JobState::Queued;
    value.started_at_ms = None;
    value
}

fn prepare_proof(store: &mut Store, f: &Fixture) {
    for (id, role) in [
        ("job:executor-a", AgentRole::Executor),
        ("job:verifier", AgentRole::Verifier),
    ] {
        let job = queued_job(id, role);
        store.register_job(f.repo(), &job).unwrap();
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Queued,
                JobState::Running,
                100,
            )
            .unwrap();
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Running,
                JobState::Succeeded,
                120,
            )
            .unwrap();
    }
    let evidence: EvidenceRecord = decode(samples()["evidence"].clone());
    store.record_evidence(f.repo(), &evidence).unwrap();
}

fn move_to_verifying(store: &mut Store, f: &Fixture) {
    let mut state = TaskState::Planned;
    for next in [
        TaskState::Ready,
        TaskState::Executing,
        TaskState::AwaitingVerification,
        TaskState::Verifying,
    ] {
        store
            .transition_task(f.repo(), &TaskId::new("a").unwrap(), state, next, None, 100)
            .unwrap();
        state = next;
    }
}

#[test]
fn default_paths_are_deterministic_and_injected() {
    let temp = TempDir::new();
    let home = temp.0.join("home");
    let paths = MachinePaths::resolve(&PathContext {
        home: Some(home.clone()),
        ..PathContext::default()
    })
    .unwrap();
    assert_eq!(
        paths.machine_config,
        home.join(".config/agentctl/config.toml")
    );
    assert_eq!(
        paths.database,
        home.join(".local/share/agentctl/state.sqlite3")
    );
    assert_eq!(paths.cache_root, home.join(".cache/agentctl"));
    assert!(!home.exists()); // Resolution has no filesystem side effects.
    assert!(MachinePaths::resolve(&PathContext::default()).is_err());
}

#[test]
fn xdg_overrides_are_honored_and_relative_values_fall_back() {
    let temp = TempDir::new();
    let context = PathContext {
        home: None,
        config_home: Some(temp.0.join("config")),
        data_home: Some(temp.0.join("data")),
        cache_home: Some(temp.0.join("cache")),
    };
    let paths = MachinePaths::resolve(&context).unwrap();
    assert_eq!(paths.config_root, temp.0.join("config/agentctl"));
    assert_eq!(paths.data_root, temp.0.join("data/agentctl"));
    assert_eq!(paths.cache_root, temp.0.join("cache/agentctl"));
    let context = PathContext {
        home: Some(temp.0.clone()),
        config_home: Some("relative".into()),
        ..PathContext::default()
    };
    assert_eq!(
        MachinePaths::resolve(&context).unwrap().config_root,
        temp.0.join(".config/agentctl")
    );
    let context = PathContext {
        config_home: Some(temp.0.join("../escape")),
        ..context
    };
    assert!(MachinePaths::resolve(&context).is_err());
}

#[test]
fn machine_config_initialization_preserves_existing_policy() {
    let f = Fixture::new();
    let existing = "# preserve this comment\nversion = 1\nbusy_timeout_ms = 1234\n";
    fs::write(&f.paths.machine_config, existing).unwrap();
    assert_eq!(
        MachineConfig::initialize(&f.paths.machine_config)
            .unwrap()
            .busy_timeout_ms,
        1234
    );
    assert_eq!(
        fs::read_to_string(&f.paths.machine_config).unwrap(),
        existing
    );
    for invalid in [
        "bad = [",
        "version = 2\nbusy_timeout_ms = 1000",
        "version = 1\nbusy_timeout_ms = 0",
        "version = 1\nbusy_timeout_ms = 1000\nunknown = true",
        "busy_timeout_ms = 1000",
    ] {
        fs::write(&f.paths.machine_config, invalid).unwrap();
        assert!(MachineConfig::initialize(&f.paths.machine_config).is_err());
        assert_eq!(
            fs::read_to_string(&f.paths.machine_config).unwrap(),
            invalid
        );
    }
}

#[test]
fn project_config_is_structured_strict_and_not_overwritten() {
    let f = Fixture::new();
    ProjectConfig::initialize(&f.root).unwrap();
    let path = f.root.join(".agentctl/project.toml");
    let config = r#"version = 1
display_name = "Parser"
[invariants.compatibility]
description = "Preserve public input behavior"
[architecture.local]
description = "Remain local"
[commands.unit]
program = "cargo"
args = ["test"]
cwd = "."
[[protected]]
path = "data/private"
deny_read = true
deny_write = true
reason = "Private data"
[verification.unit]
description = "Unit checks"
command_refs = ["unit"]
"#;
    fs::write(&path, config).unwrap();
    let value = ProjectConfig::initialize(&f.root).unwrap();
    assert_eq!(value.commands["unit"].program, "cargo");
    assert_eq!(fs::read_to_string(&path).unwrap(), config);
    for invalid in [
        "version=9",
        "version=1\nprovider='no'",
        "version=1\n[verification.unit]\ndescription='unit'\ncommand_refs=['missing']",
        "version=1\n[commands.test]\nprogram='test'\nargs=[]\ncwd='../escape'",
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(ProjectConfig::initialize(&f.root).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
    }
}

#[test]
fn repository_discovery_supports_nested_paths_and_no_remote() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src/nested")).unwrap();
    let info = RepositoryInfo::discover(&f.root.join("src/nested")).unwrap();
    assert_eq!(&info.repository_id, f.repo());
    assert_eq!(info.workspace_id, f.info.workspace_id);
    assert_eq!(info.source.workspace_id, info.workspace_id);
    assert_eq!(info.source.repository_id, info.repository_id);
    assert_eq!(info.root, fs::canonicalize(&f.root).unwrap());
    assert!(info.remotes.is_empty());
    assert!(info.source.head_commit.is_none());
    assert!(info.source.worktree_fingerprint.is_none());
}

#[test]
fn same_basename_and_separate_clones_have_distinct_identity() {
    let f = Fixture::new();
    let second = f._temp.0.join("elsewhere/repo");
    init_git(&second);
    assert_ne!(
        RepositoryInfo::discover(&second).unwrap().workspace_id,
        f.info.workspace_id
    );
    assert_ne!(
        RepositoryInfo::discover(&second).unwrap().repository_id,
        *f.repo()
    );
    commit(&f.root);
    let clone = f._temp.0.join("clone");
    git(
        &f._temp.0,
        &[
            "clone",
            "--quiet",
            f.root.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let clone = RepositoryInfo::discover(&clone).unwrap();
    assert_ne!(clone.repository_id, *f.repo());
    assert_ne!(clone.workspace_id, f.info.workspace_id);
    assert_eq!(
        clone.source.head_commit,
        RepositoryInfo::discover(&f.root)
            .unwrap()
            .source
            .head_commit
    );
}

#[test]
fn remote_changes_are_metadata_and_do_not_change_identity() {
    let f = Fixture::new();
    git(
        &f.root,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/first.git",
        ],
    );
    let first = RepositoryInfo::discover(&f.root).unwrap();
    assert_eq!(&first.repository_id, f.repo());
    assert_eq!(
        first.remotes["origin"],
        vec!["https://example.invalid/first.git"]
    );
    git(
        &f.root,
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://host.invalid/other.git",
        ],
    );
    let second = RepositoryInfo::discover(&f.root).unwrap();
    assert_eq!(first.repository_id, second.repository_id);
    assert_ne!(first.remotes, second.remotes);
    let mut store = f.store();
    store.register_repository(second.clone()).unwrap();
    assert_eq!(store.repositories().unwrap().len(), 1);
    assert_eq!(
        store.repository(f.repo()).unwrap().unwrap().remotes,
        second.remotes
    );
}

#[test]
fn source_state_distinguishes_committed_dirty_and_untracked_state() {
    let f = Fixture::new();
    commit(&f.root);
    let clean = RepositoryInfo::discover(&f.root).unwrap();
    assert!(clean.source.head_commit.is_some());
    assert!(!clean.source.dirty);
    fs::write(f.root.join("tracked.txt"), "modified\n").unwrap();
    let dirty = RepositoryInfo::discover(&f.root).unwrap();
    assert!(dirty.source.dirty);
    assert_eq!(clean.source.head_commit, dirty.source.head_commit);
    assert!(dirty.source.worktree_fingerprint.is_none());
    fs::write(f.root.join("tracked.txt"), "initial\n").unwrap();
    fs::write(f.root.join("untracked.txt"), "new").unwrap();
    assert!(RepositoryInfo::discover(&f.root).unwrap().source.dirty);
}

#[test]
fn moved_primary_repository_gets_new_identity_and_old_record_remains() {
    let f = Fixture::new();
    let moved = f._temp.0.join("moved");
    fs::rename(&f.root, &moved).unwrap();
    let info = RepositoryInfo::discover(&moved).unwrap();
    assert_ne!(&info.repository_id, f.repo());
    let mut store = f.store();
    store.register_repository(info).unwrap();
    assert_eq!(store.repositories().unwrap().len(), 2);
    assert!(
        !store
            .workspace(&f.info.workspace_id)
            .unwrap()
            .unwrap()
            .info
            .root
            .exists()
    );
}

#[test]
fn worktrees_share_repository_identity_and_moves_retain_workspace_identity() {
    let f = Fixture::new();
    commit(&f.root);
    let worktree = f._temp.0.join("worktree");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            worktree.to_str().unwrap(),
        ],
    );
    let first = RepositoryInfo::discover(&worktree).unwrap();
    assert_eq!(&first.repository_id, f.repo());
    assert_ne!(first.workspace_id, f.info.workspace_id);
    assert_eq!(first.common_directory, f.info.common_directory);
    let mut store = f.store();
    store.register_repository(first.clone()).unwrap();
    let moved = f._temp.0.join("moved-worktree");
    git(
        &f.root,
        &[
            "worktree",
            "move",
            worktree.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let second = RepositoryInfo::discover(&moved).unwrap();
    assert_eq!(first.repository_id, second.repository_id);
    assert_eq!(first.workspace_id, second.workspace_id);
    let record = store.register_repository(second).unwrap();
    assert_eq!(record.previous_roots, vec![first.root]);
    assert_eq!(store.repositories().unwrap().len(), 1);
    assert_eq!(store.workspaces(Some(f.repo())).unwrap().len(), 2);
}

#[test]
fn fresh_database_reopens_with_expected_migration_and_wal() {
    let f = Fixture::new();
    for _ in 0..3 {
        let status = f.store().status().unwrap();
        assert_eq!(status.schema_version, DATABASE_VERSION);
        assert_eq!(status.journal_mode, "wal");
        assert_eq!(status.repositories, 1);
        assert_eq!(status.workspaces, 1);
    }
    let conn = Connection::open(&f.paths.database).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM schema_migrations", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        DATABASE_VERSION
    );
}

#[test]
fn future_or_foreign_databases_fail_without_migration() {
    let f = Fixture::new();
    let conn = Connection::open(&f.paths.database).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    assert!(Store::open(&f.paths.database, 5000).is_err());
    assert!(Store::read_only(&f.paths.database, 5000).is_err());
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        99
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM repositories", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let foreign = f.paths.data_root.join("foreign.sqlite3");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (value TEXT);")
        .unwrap();
    assert!(Store::open(&foreign, 5000).is_err());
    let corrupt = f.paths.data_root.join("corrupt.sqlite3");
    fs::write(&corrupt, "not a database").unwrap();
    assert!(Store::open(&corrupt, 5000).is_err());
}

#[test]
fn missing_migration_history_or_event_guards_are_not_silently_repaired() {
    let f = Fixture::new();
    let conn = Connection::open(&f.paths.database).unwrap();
    conn.execute("DELETE FROM schema_migrations", []).unwrap();
    assert!(Store::open(&f.paths.database, 5000).is_err());
    conn.execute(
        "INSERT INTO schema_migrations VALUES (1,'local_substrate'), (2,'repository_workspaces'), (3,'code_graph'), (4,'engineering_memory'), (5,'planning_substrate'), (6,'guarded_plan_completion'), (7,'provider_runtime')",
        [],
    )
    .unwrap();
    conn.execute_batch("DROP TRIGGER events_no_delete").unwrap();
    assert!(Store::open(&f.paths.database, 5000).is_err());
}

#[test]
fn tasks_survive_reopen_and_invalid_or_stale_transitions_do_not_persist() {
    let f = Fixture::new();
    let task_id = TaskId::new("a").unwrap();
    {
        let mut store = f.store();
        store.create_plan(f.repo(), &plan(), 10).unwrap();
        assert_eq!(store.tasks(f.repo(), None).unwrap().len(), 2);
        assert!(
            store
                .transition_task(
                    f.repo(),
                    &task_id,
                    TaskState::Planned,
                    TaskState::Verified,
                    None,
                    20
                )
                .is_err()
        );
        store
            .transition_task(
                f.repo(),
                &task_id,
                TaskState::Planned,
                TaskState::Ready,
                None,
                20,
            )
            .unwrap();
        assert!(
            store
                .transition_task(
                    f.repo(),
                    &task_id,
                    TaskState::Planned,
                    TaskState::Ready,
                    None,
                    20
                )
                .is_err()
        );
    }
    let store = f.store();
    let task = store.task(f.repo(), &task_id).unwrap().unwrap();
    assert_eq!(task.packet, plan().tasks[0]);
    assert_eq!(task.state, TaskState::Ready);
    assert_eq!(
        store.plan(f.repo(), &plan().plan_id).unwrap().unwrap(),
        plan()
    );
}

#[test]
fn verified_only_dependencies_are_enforced_using_durable_state() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    move_to_verifying(&mut store, &f);
    let a = TaskId::new("a").unwrap();
    let b = TaskId::new("b").unwrap();
    assert!(
        store
            .transition_task(
                f.repo(),
                &b,
                TaskState::Planned,
                TaskState::Ready,
                None,
                110
            )
            .is_err()
    );
    assert!(
        store
            .transition_task(
                f.repo(),
                &a,
                TaskState::Verifying,
                TaskState::Verified,
                Some(&verification(false)),
                120
            )
            .is_err()
    );
    prepare_proof(&mut store, &f);
    store
        .transition_task(
            f.repo(),
            &a,
            TaskState::Verifying,
            TaskState::Verified,
            Some(&verification(false)),
            130,
        )
        .unwrap();
    drop(store);
    let mut reopened = f.store();
    reopened
        .transition_task(
            f.repo(),
            &b,
            TaskState::Planned,
            TaskState::Ready,
            None,
            140,
        )
        .unwrap();
    let events = reopened
        .events(Some(f.repo()), Some(&a), None, 100)
        .unwrap();
    assert!(events.iter().any(|e| matches!(&e.entry, JournalEntry::TaskStateChanged { to: TaskState::Verified, verification: Some(v), .. } if v == &verification(false))));
}

#[test]
fn rejected_tasks_do_not_restart_and_verifier_identity_is_checked() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    move_to_verifying(&mut store, &f);
    prepare_proof(&mut store, &f);
    let a = TaskId::new("a").unwrap();
    let mut proof = verification(false);
    proof.verifier_job_id = JobId::new("job:executor-a").unwrap();
    assert!(
        store
            .transition_task(
                f.repo(),
                &a,
                TaskState::Verifying,
                TaskState::Verified,
                Some(&proof),
                130
            )
            .is_err()
    );
    let mut proof = verification(false);
    proof.decision = VerificationDecision::Reject;
    proof.findings.push(finding());
    store
        .transition_task(
            f.repo(),
            &a,
            TaskState::Verifying,
            TaskState::Rejected,
            Some(&proof),
            130,
        )
        .unwrap();
    assert!(
        store
            .transition_task(
                f.repo(),
                &a,
                TaskState::Rejected,
                TaskState::Executing,
                None,
                140
            )
            .is_err()
    );
}

#[test]
fn jobs_survive_reopen_and_mutate_only_through_valid_transitions() {
    let f = Fixture::new();
    let job = queued_job("job:executor-a", AgentRole::Executor);
    {
        let mut store = f.store();
        store.create_plan(f.repo(), &plan(), 10).unwrap();
        store.register_job(f.repo(), &job).unwrap();
        assert!(
            store
                .transition_job(
                    f.repo(),
                    &job.job_id,
                    JobState::Queued,
                    JobState::Succeeded,
                    100
                )
                .is_err()
        );
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Queued,
                JobState::Running,
                100,
            )
            .unwrap();
    }
    let mut store = f.store();
    assert_eq!(
        store
            .job(f.repo(), &job.job_id)
            .unwrap()
            .unwrap()
            .started_at_ms,
        Some(100)
    );
    assert_eq!(store.jobs(f.repo()).unwrap().len(), 1);
    store
        .transition_job(
            f.repo(),
            &job.job_id,
            JobState::Running,
            JobState::Succeeded,
            120,
        )
        .unwrap();
    assert!(
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Succeeded,
                JobState::Running,
                130
            )
            .is_err()
    );
    assert_eq!(
        store
            .task(f.repo(), &TaskId::new("a").unwrap())
            .unwrap()
            .unwrap()
            .state,
        TaskState::Planned
    );
    let mut bad = job;
    bad.state = JobState::Succeeded;
    bad.started_at_ms = Some(100);
    bad.finished_at_ms = Some(120);
    assert!(store.register_job(f.repo(), &bad).is_err());
}

#[test]
fn evidence_is_immutable_compact_metadata_with_external_log_references() {
    let f = Fixture::new();
    let record: EvidenceRecord = decode(samples()["evidence"].clone());
    {
        let mut store = f.store();
        store.record_evidence(f.repo(), &record).unwrap();
        assert!(store.record_evidence(f.repo(), &record).is_err());
        let mut large = record.clone();
        large.evidence_id = EvidenceId::new("evidence:large").unwrap();
        large.summary = "x".repeat(100_000);
        assert!(store.record_evidence(f.repo(), &large).is_err());
        large.summary = "compact".into();
        large.full_log_ref = Some("data:text/plain,embedded-log".into());
        assert!(store.record_evidence(f.repo(), &large).is_err());
        large.full_log_ref = Some("DATA:text/plain,embedded-log".into());
        assert!(store.record_evidence(f.repo(), &large).is_err());
    }
    assert_eq!(
        f.store()
            .evidence(f.repo(), &record.evidence_id)
            .unwrap()
            .unwrap(),
        record
    );
    let artifact = f.paths.evidence_directory(f.repo(), &record.evidence_id);
    assert!(artifact.starts_with(f.paths.data_root.join("artifacts")));
    assert!(!artifact.exists());
}

#[test]
fn events_preserve_wire_packets_order_and_task_job_queries_after_reopen() {
    let f = Fixture::new();
    let job = queued_job("job:executor-a", AgentRole::Executor);
    let mut event: AgentEvent = decode(samples()["agent-event"].clone());
    let (first, second);
    {
        let mut store = f.store();
        store.create_plan(f.repo(), &plan(), 10).unwrap();
        store.register_job(f.repo(), &job).unwrap();
        first = store.append_agent_event(f.repo(), &event).unwrap();
        assert!(store.append_agent_event(f.repo(), &event).is_err());
        event.event_id = "event:2".into();
        event.timestamp_ms = 50; // Sequence, not wall clock, defines append order.
        second = store.append_agent_event(f.repo(), &event).unwrap();
    }
    assert!(second > first);
    let store = f.store();
    let recent = store
        .events(
            Some(f.repo()),
            Some(&TaskId::new("a").unwrap()),
            Some(&job.job_id),
            2,
        )
        .unwrap();
    assert_eq!(
        recent.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(
        recent[1].entry,
        JournalEntry::Agent {
            event: Box::new(event)
        }
    );
    assert_eq!(
        store.events(Some(f.repo()), None, None, 1).unwrap()[0].sequence,
        second
    );
    assert!(store.events(None, None, None, 0).is_err());
    assert!(store.events(None, None, None, 1001).is_err());
}

#[test]
fn events_reject_unknown_or_conflicting_durable_associations() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    let mut event: AgentEvent = decode(samples()["agent-event"].clone());
    assert!(store.append_agent_event(f.repo(), &event).is_err()); // Unregistered job.
    store
        .register_job(f.repo(), &queued_job("job:executor-a", AgentRole::Executor))
        .unwrap();
    event.context.role = Some(AgentRole::Verifier);
    assert!(store.append_agent_event(f.repo(), &event).is_err());
    event.context.role = Some(AgentRole::Executor);
    event.context.task_id = Some(TaskId::new("b").unwrap());
    event.context.packet_id = event.context.task_id.clone();
    assert!(store.append_agent_event(f.repo(), &event).is_err());
    event.context = common::context();
    event.event = AgentEventKind::CommandFinished {
        invocation_id: "cmd:1".into(),
        exit_status: 0,
        evidence: vec![EvidenceRef(EvidenceId::new("missing").unwrap())],
    };
    assert!(store.append_agent_event(f.repo(), &event).is_err());
}

#[test]
fn event_body_associations_are_indexed_even_without_optional_context() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    let event: AgentEvent = decode(
        json!({"version":"1","event_id":"loaded","timestamp_ms":10,"context":{},"event":{"kind":"TASK_PACKET_LOADED","task_id":"a"}}),
    );
    store.append_agent_event(f.repo(), &event).unwrap();
    let events = store
        .events(Some(f.repo()), Some(&TaskId::new("a").unwrap()), None, 1)
        .unwrap();
    assert_eq!(events[0].plan_id, Some(plan().plan_id));
    assert_eq!(
        events[0].entry,
        JournalEntry::Agent {
            event: Box::new(event)
        }
    );
}

#[test]
fn append_only_guards_reject_sql_updates_and_deletes() {
    let f = Fixture::new();
    let connection = Connection::open(&f.paths.database).unwrap();
    assert!(
        connection
            .execute("UPDATE events SET timestamp_ms=0", [])
            .is_err()
    );
    assert!(connection.execute("DELETE FROM events", []).is_err());
    assert_eq!(f.store().status().unwrap().events, 1);
}

#[test]
fn failed_journal_append_rolls_back_task_and_job_updates() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    let job = queued_job("job:executor-a", AgentRole::Executor);
    store.register_job(f.repo(), &job).unwrap();
    let before = store.status().unwrap().events;
    let connection = Connection::open(&f.paths.database).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_append BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected journal failure'); END;").unwrap();
    let a = TaskId::new("a").unwrap();
    assert!(
        store
            .transition_task(
                f.repo(),
                &a,
                TaskState::Planned,
                TaskState::Ready,
                None,
                100
            )
            .is_err()
    );
    assert!(
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Queued,
                JobState::Running,
                100
            )
            .is_err()
    );
    drop(store);
    let reopened = f.store();
    assert_eq!(
        reopened.task(f.repo(), &a).unwrap().unwrap().state,
        TaskState::Planned
    );
    assert_eq!(
        reopened.job(f.repo(), &job.job_id).unwrap().unwrap().state,
        JobState::Queued
    );
    assert_eq!(reopened.status().unwrap().events, before);
}

#[test]
fn failed_verified_transition_never_commits_without_its_event() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    move_to_verifying(&mut store, &f);
    prepare_proof(&mut store, &f);
    let before = store.status().unwrap().events;
    let connection = Connection::open(&f.paths.database).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_append BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected journal failure'); END;").unwrap();
    let a = TaskId::new("a").unwrap();
    assert!(
        store
            .transition_task(
                f.repo(),
                &a,
                TaskState::Verifying,
                TaskState::Verified,
                Some(&verification(false)),
                130
            )
            .is_err()
    );
    assert_eq!(
        store.task(f.repo(), &a).unwrap().unwrap().state,
        TaskState::Verifying
    );
    assert_eq!(store.status().unwrap().events, before);
}

#[test]
fn failed_plan_creation_rolls_back_all_rows_and_events() {
    let f = Fixture::new();
    let mut store = f.store();
    store.create_plan(f.repo(), &plan(), 10).unwrap();
    let before = store.status().unwrap();
    let mut conflicting = plan();
    conflicting.plan_id = PlanId::new("plan:2").unwrap();
    assert!(store.create_plan(f.repo(), &conflicting, 20).is_err());
    assert!(
        store
            .plan(f.repo(), &conflicting.plan_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.status().unwrap().events, before.events);
    assert_eq!(store.status().unwrap().plans, before.plans);
    let mut invalid = plan();
    invalid.tasks[0]
        .dependencies
        .push(TaskId::new("b").unwrap());
    assert!(store.create_plan(f.repo(), &invalid, 20).is_err());
}

#[test]
fn independent_connections_serialize_writes_and_reject_stale_state() {
    let f = Fixture::new();
    f.store().create_plan(f.repo(), &plan(), 10).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let database = f.paths.database.clone();
            let repo = f.repo().clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&database, 5000).unwrap();
                barrier.wait();
                store
                    .transition_task(
                        &repo,
                        &TaskId::new("a").unwrap(),
                        TaskState::Planned,
                        TaskState::Ready,
                        None,
                        20,
                    )
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap() as usize)
            .sum::<usize>(),
        1
    );
    let state_events = f
        .store()
        .events(Some(f.repo()), Some(&TaskId::new("a").unwrap()), None, 100)
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event.entry, JournalEntry::TaskStateChanged { .. }))
        .count();
    assert_eq!(state_events, 1);
}

#[test]
fn concurrent_event_writers_preserve_every_observation() {
    let f = Fixture::new();
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = (0..3).map(|writer| {
        let barrier = barrier.clone(); let database = f.paths.database.clone(); let repo = f.repo().clone();
        std::thread::spawn(move || {
            let mut store = Store::open(&database, 5000).unwrap();
            barrier.wait();
            for i in 0..10 {
                let event: AgentEvent = decode(json!({"version":"1","event_id":format!("writer:{writer}:{i}"),"timestamp_ms":i,"context":{},"event":{"kind":"AGENT_STARTED"}}));
                store.append_agent_event(&repo, &event).unwrap();
            }
        })
    }).collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let events = f.store().events(Some(f.repo()), None, None, 100).unwrap();
    assert_eq!(events.len(), 31);
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
}

#[cfg(unix)]
#[test]
fn config_and_database_writes_reject_symlink_targets() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f._temp.0.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, f.root.join(".agentctl")).unwrap();
    assert!(ProjectConfig::initialize(&f.root).is_err());
    assert!(!outside.join("project.toml").exists());
    let target = outside.join("untouched");
    fs::write(&target, "original").unwrap();
    let fake = f.paths.data_root.join("fake.sqlite3");
    symlink(&target, &fake).unwrap();
    assert!(Store::open(&fake, 5000).is_err());
    let fake_config = f.paths.config_root.join("fake.toml");
    symlink(&target, &fake_config).unwrap();
    assert!(MachineConfig::initialize(&fake_config).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
    let hardlink = f.paths.data_root.join("hardlink.sqlite3");
    fs::hard_link(&target, &hardlink).unwrap();
    assert!(Store::open(&hardlink, 5000).is_err());
}

#[cfg(unix)]
#[test]
fn new_machine_files_have_private_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    for path in [
        &f.paths.config_root,
        &f.paths.data_root,
        &f.paths.cache_root,
    ] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    for path in [&f.paths.machine_config, &f.paths.database] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
}

fn cli(temp: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentctl"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", temp.join("home"))
        .env("XDG_CONFIG_HOME", temp.join("config"))
        .env("XDG_DATA_HOME", temp.join("data"))
        .env("XDG_CACHE_HOME", temp.join("cache"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn cli_json(temp: &Path, cwd: &Path, args: &[&str]) -> Value {
    let output = cli(temp, cwd, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_initializes_reopens_registers_and_inspects_isolated_state() {
    let temp = TempDir::new();
    let root = temp.0.join("repo");
    init_git(&root);
    let first = cli_json(&temp.0, &root, &["init", "--json"]);
    let second = cli_json(&temp.0, &root, &["init", "--json"]);
    assert_eq!(first, second);
    assert_eq!(
        cli_json(&temp.0, &root, &["doctor", "--json"])["healthy"],
        true
    );
    let initialized = cli_json(&temp.0, &root, &["repo", "init", "--json"]);
    let config = root.join(".agentctl/project.toml");
    fs::write(&config, "# preserve me\nversion=1\ndisplay_name='Custom'\n").unwrap();
    let again = cli_json(&temp.0, &root, &["repo", "init", "--json"]);
    assert_eq!(
        initialized["info"]["repository_id"],
        again["info"]["repository_id"]
    );
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "# preserve me\nversion=1\ndisplay_name='Custom'\n"
    );
    let status = cli_json(&temp.0, &root, &["repo", "status", "--json"]);
    assert_eq!(status["registered"], true);
    assert_eq!(status["project_config"]["display_name"], "Custom");
    assert_eq!(
        cli_json(&temp.0, &root, &["repo", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        cli_json(&temp.0, &root, &["state", "status", "--json"])["repositories"],
        1
    );
    assert_eq!(
        cli_json(&temp.0, &root, &["events", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(cli(&temp.0, &root, &["doctor"]).status.success());
    assert!(cli(&temp.0, &root, &["repo", "status"]).status.success());
    assert!(cli(&temp.0, &root, &["repo", "list"]).status.success());
}

#[test]
fn cli_failures_are_actionable_and_do_not_reset_config_or_database() {
    let temp = TempDir::new();
    let root = temp.0.join("repo");
    init_git(&root);
    assert!(!cli(&temp.0, &root, &["doctor", "--json"]).status.success());
    cli_json(&temp.0, &root, &["init", "--json"]);
    let config = temp.0.join("config/agentctl/config.toml");
    fs::write(&config, "version=999\nbusy_timeout_ms=5000\n").unwrap();
    let doctor = cli(&temp.0, &root, &["doctor", "--json"]);
    assert!(!doctor.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&doctor.stdout).unwrap()["healthy"],
        false
    );
    assert!(!cli(&temp.0, &root, &["init"]).status.success());
    assert!(fs::read_to_string(&config).unwrap().contains("999"));
    fs::write(&config, "version=1\nbusy_timeout_ms=5000\n").unwrap();
    cli_json(&temp.0, &root, &["repo", "init", "--json"]);
    fs::write(root.join(".agentctl/project.toml"), "version=99\n").unwrap();
    assert!(
        !cli(&temp.0, &root, &["repo", "status", "--json"])
            .status
            .success()
    );
    assert!(
        !cli(&temp.0, &root, &["repo", "init", "--json"])
            .status
            .success()
    );
    let database = temp.0.join("data/agentctl/state.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);
    for args in [
        &["init"][..],
        &["doctor", "--json"],
        &["state", "status", "--json"],
    ] {
        let output = cli(&temp.0, &root, args);
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(
        Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}

#[test]
fn cli_lists_missing_registered_roots_without_silently_rebinding_them() {
    let temp = TempDir::new();
    let root = temp.0.join("repo");
    init_git(&root);
    cli_json(&temp.0, &root, &["init", "--json"]);
    cli_json(&temp.0, &root, &["repo", "init", "--json"]);
    fs::rename(&root, temp.0.join("moved")).unwrap();
    let listing = cli_json(&temp.0, &temp.0, &["repo", "list", "--json"]);
    assert_eq!(listing[0]["workspaces"][0]["health"], "unavailable");
    assert_eq!(
        listing[0]["workspaces"][0]["registration"]["info"]["root"],
        json!(fs::canonicalize(&temp.0).unwrap().join("repo"))
    );
}

#[test]
fn concurrent_first_open_applies_the_migration_exactly_once() {
    let temp = TempDir::new();
    let database = temp.0.join("new/state.sqlite3");
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let database = database.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(&database, 5000)
                    .unwrap()
                    .status()
                    .unwrap()
                    .schema_version
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), DATABASE_VERSION);
    }
    let connection = Connection::open(database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM schema_migrations", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        DATABASE_VERSION
    );
}

#[test]
fn concurrent_default_config_publication_is_non_destructive() {
    let temp = TempDir::new();
    let path = temp.0.join("config/config.toml");
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                MachineConfig::initialize(&path).unwrap()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), MachineConfig::default());
    }
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn replacing_git_metadata_at_the_same_path_is_reported_and_not_merged() {
    let f = Fixture::new();
    fs::rename(f.root.join(".git"), f._temp.0.join("old-git")).unwrap();
    init_git(&f.root);
    let replacement = RepositoryInfo::discover(&f.root).unwrap();
    assert_eq!(&replacement.repository_id, f.repo());
    assert_ne!(
        replacement.git_directory_identity,
        f.info.git_directory_identity
    );
    assert!(f.store().register_repository(replacement).is_err());
    assert_eq!(
        f.store()
            .workspace(&f.info.workspace_id)
            .unwrap()
            .unwrap()
            .info,
        f.info
    );
}

#[test]
fn inspection_does_not_create_a_missing_database() {
    let temp = TempDir::new();
    let path = temp.0.join("missing.sqlite3");
    assert!(Store::read_only(&path, 5000).is_err());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn sqlite_sidecar_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let path = temp.0.join("state.sqlite3");
    Store::open(&path, 5000).unwrap();
    let target = temp.0.join("target");
    fs::write(&target, "unchanged").unwrap();
    symlink(&target, temp.0.join("state.sqlite3-wal")).unwrap();
    assert!(Store::open(&path, 5000).is_err());
    assert!(Store::read_only(&path, 5000).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "unchanged");
}

fn linked_workspace(f: &Fixture) -> RepositoryInfo {
    commit(&f.root);
    let root = f._temp.0.join("linked");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            root.to_str().unwrap(),
        ],
    );
    RepositoryInfo::discover(&root).unwrap()
}

#[test]
fn workspace_source_and_job_evidence_locations_survive_reopen() {
    let f = Fixture::new();
    let linked = linked_workspace(&f);
    fs::write(linked.root.join("linked.txt"), "linked commit").unwrap();
    git(&linked.root, &["add", "linked.txt"]);
    git(&linked.root, &["commit", "--quiet", "-m", "linked"]);
    fs::write(linked.root.join("untracked"), "dirty").unwrap();
    let linked = RepositoryInfo::discover(&linked.root).unwrap();
    let main = RepositoryInfo::discover(&f.root).unwrap();
    let repeated = RepositoryInfo::discover(&linked.root).unwrap();
    assert_eq!(linked.repository_id, repeated.repository_id);
    assert_eq!(linked.workspace_id, repeated.workspace_id);
    assert_ne!(main.source.head_commit, linked.source.head_commit);
    assert!(!main.source.dirty);
    assert!(linked.source.dirty);
    let job = queued_job("job:executor-a", AgentRole::Executor);
    let evidence: EvidenceRecord = decode(samples()["evidence"].clone());
    let event: AgentEvent = decode(samples()["agent-event"].clone());
    {
        let mut store = f.store();
        store.register_repository(main.clone()).unwrap();
        store.register_repository(linked.clone()).unwrap();
        store.create_plan(f.repo(), &plan(), 10).unwrap();
        assert_eq!(store.tasks(&linked.repository_id, None).unwrap().len(), 2);
        store
            .register_job_in_workspace(f.repo(), &linked.workspace_id, &job)
            .unwrap();
        store
            .record_evidence_in_workspace(f.repo(), &main.workspace_id, &evidence)
            .unwrap();
        assert!(
            store
                .append_agent_event_in_workspace(f.repo(), &main.workspace_id, &event)
                .is_err()
        );
        store.append_agent_event(f.repo(), &event).unwrap();
        store
            .transition_job(
                f.repo(),
                &job.job_id,
                JobState::Queued,
                JobState::Running,
                100,
            )
            .unwrap();

        let other = f._temp.0.join("independent");
        init_git(&other);
        let other = RepositoryInfo::discover(&other).unwrap();
        store.register_repository(other.clone()).unwrap();
        assert!(
            store
                .register_job_in_workspace(
                    f.repo(),
                    &other.workspace_id,
                    &queued_job("wrong", AgentRole::Executor)
                )
                .is_err()
        );
        let mut another = evidence.clone();
        another.evidence_id = EvidenceId::new("wrong").unwrap();
        assert!(
            store
                .record_evidence_in_workspace(f.repo(), &other.workspace_id, &another)
                .is_err()
        );
        assert!(
            store
                .append_agent_event_in_workspace(f.repo(), &other.workspace_id, &event)
                .is_err()
        );
        let mut inconsistent = linked.clone();
        inconsistent.source.workspace_id = main.workspace_id.clone();
        assert!(store.register_repository(inconsistent).is_err());
    }
    let store = f.store();
    assert_eq!(store.workspaces(Some(f.repo())).unwrap().len(), 2);
    assert_eq!(
        store
            .workspace(&main.workspace_id)
            .unwrap()
            .unwrap()
            .info
            .source,
        main.source
    );
    assert_eq!(
        store
            .workspace(&linked.workspace_id)
            .unwrap()
            .unwrap()
            .info
            .source,
        linked.source
    );
    assert_eq!(
        store.job_workspace(f.repo(), &job.job_id).unwrap(),
        Some(linked.workspace_id.clone())
    );
    assert_eq!(
        store
            .evidence_workspace(f.repo(), &evidence.evidence_id)
            .unwrap(),
        Some(main.workspace_id)
    );
    let events = store
        .events(Some(f.repo()), None, Some(&job.job_id), 100)
        .unwrap();
    assert_eq!(events.len(), 3);
    assert!(
        events
            .iter()
            .all(|entry| entry.workspace_id.as_ref() == Some(&linked.workspace_id))
    );
    assert!(
        store
            .events(Some(f.repo()), None, None, 100)
            .unwrap()
            .iter()
            .any(
                |entry| matches!(entry.entry, JournalEntry::PlanCreated { .. })
                    && entry.workspace_id.is_none()
            )
    );
}

#[test]
fn cli_groups_linked_workspaces_under_one_logical_repository() {
    let f = Fixture::new();
    let linked = linked_workspace(&f);
    cli_json(&f._temp.0, &f.root, &["init", "--json"]);
    cli_json(&f._temp.0, &f.root, &["repo", "init", "--json"]);
    cli_json(&f._temp.0, &linked.root, &["repo", "init", "--json"]);
    let main_status = cli_json(&f._temp.0, &f.root, &["repo", "status", "--json"]);
    let linked_status = cli_json(&f._temp.0, &linked.root, &["repo", "status", "--json"]);
    assert_eq!(main_status["repository_id"], linked_status["repository_id"]);
    assert_ne!(main_status["workspace_id"], linked_status["workspace_id"]);
    assert_eq!(linked_status["workspace"]["root"], json!(linked.root));
    let listing = cli_json(&f._temp.0, &f.root, &["repo", "list", "--json"]);
    assert_eq!(listing.as_array().unwrap().len(), 1);
    assert_eq!(listing[0]["workspaces"].as_array().unwrap().len(), 2);
    let human = cli(&f._temp.0, &f.root, &["repo", "list"]);
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("2 workspace"));
    assert!(text.contains(f.info.workspace_id.as_str()));
    assert!(text.contains(linked.workspace_id.as_str()));
}

// Freeze the actual v1 storage shape, not a database built using today's Store API.
fn insert_v1_repository(connection: &Connection, info: &RepositoryInfo) -> (String, Value) {
    use agentctl::local::repository::RepositoryId;
    let old_id = RepositoryId::for_common_directory(&info.git_directory)
        .as_str()
        .to_owned();
    let mut value = serde_json::to_value(info).unwrap();
    value["repository_id"] = json!(old_id);
    value["source"]["repository_id"] = json!(old_id);
    value.as_object_mut().unwrap().remove("workspace_id");
    value
        .as_object_mut()
        .unwrap()
        .remove("common_directory_identity");
    value["source"]
        .as_object_mut()
        .unwrap()
        .remove("workspace_id");
    let record = json!({"info":value,"first_seen_ms":10,"last_seen_ms":20,"previous_roots":[]});
    connection
        .execute(
            "INSERT INTO repositories VALUES (?1,?2,?3,?4)",
            rusqlite::params![
                old_id,
                info.root.to_str().unwrap(),
                info.git_directory.to_str().unwrap(),
                record.to_string()
            ],
        )
        .unwrap();
    (old_id, record)
}

fn insert_v1_plan(connection: &Connection, repo: &str) {
    let packet = plan();
    connection
        .execute(
            "INSERT INTO plans VALUES (?1,?2,?3)",
            rusqlite::params![
                repo,
                packet.plan_id.as_str(),
                serde_json::to_string(&packet).unwrap()
            ],
        )
        .unwrap();
    for task in &packet.tasks {
        connection
            .execute(
                "INSERT INTO tasks VALUES (?1,?2,?3,'\"PLANNED\"')",
                rusqlite::params![repo, task.task_id.as_str(), packet.plan_id.as_str()],
            )
            .unwrap();
    }
}

#[test]
fn v1_migration_preserves_shared_ownership_workspace_links_and_historical_bytes() {
    let f = Fixture::new();
    let linked = linked_workspace(&f);
    let path = f._temp.0.join("v1.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("fixtures/substrate-v1.sql"))
        .unwrap();
    insert_v1_repository(&connection, &f.info);
    let (legacy_id, record) = insert_v1_repository(&connection, &linked);
    insert_v1_plan(&connection, &legacy_id);
    let job = queued_job("job:executor-a", AgentRole::Executor);
    let job_json = serde_json::to_string_pretty(&job).unwrap();
    connection
        .execute(
            "INSERT INTO jobs VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                legacy_id,
                job.job_id.as_str(),
                job.plan_id.as_str(),
                job.task_id.as_ref().unwrap().as_str(),
                job_json
            ],
        )
        .unwrap();
    let evidence: EvidenceRecord = decode(samples()["evidence"].clone());
    let evidence_json = serde_json::to_string_pretty(&evidence).unwrap();
    connection
        .execute(
            "INSERT INTO evidence VALUES (?1,?2,?3)",
            rusqlite::params![legacy_id, evidence.evidence_id.as_str(), evidence_json],
        )
        .unwrap();
    let historical =
        serde_json::to_string_pretty(&json!({"kind":"REPOSITORY_OBSERVED","repository":record}))
            .unwrap();
    connection
        .execute(
            "INSERT INTO events(repo_id,timestamp_ms,entry_json) VALUES (?1,20,?2)",
            rusqlite::params![legacy_id, historical],
        )
        .unwrap();
    let event: AgentEvent = decode(samples()["agent-event"].clone());
    let event_json = serde_json::to_string_pretty(&JournalEntry::Agent {
        event: Box::new(event),
    })
    .unwrap();
    connection.execute("INSERT INTO events(repo_id,timestamp_ms,plan_id,task_id,job_id,entry_json) VALUES (?1,30,?2,?3,?4,?5)", rusqlite::params![legacy_id,job.plan_id.as_str(),job.task_id.as_ref().unwrap().as_str(),job.job_id.as_str(),event_json]).unwrap();
    drop(connection);
    assert!(Store::read_only(&path, 5000).is_err());
    // Migration must also work offline, without rediscovering the checkout.
    fs::rename(&linked.root, f._temp.0.join("unavailable-linked")).unwrap();
    for _ in 0..2 {
        let store = Store::open(&path, 5000).unwrap();
        assert_eq!(store.repositories().unwrap().len(), 1);
        assert_eq!(store.workspaces(Some(f.repo())).unwrap().len(), 2);
        assert_eq!(store.tasks(f.repo(), None).unwrap().len(), 2);
        assert_eq!(store.job(f.repo(), &job.job_id).unwrap(), Some(job.clone()));
        assert_eq!(
            store.evidence(f.repo(), &evidence.evidence_id).unwrap(),
            Some(evidence.clone())
        );
        assert_eq!(
            store.job_workspace(f.repo(), &job.job_id).unwrap(),
            Some(linked.workspace_id.clone())
        );
        assert_eq!(
            store
                .evidence_workspace(f.repo(), &evidence.evidence_id)
                .unwrap(),
            Some(linked.workspace_id.clone())
        );
        let events = store.events(Some(f.repo()), None, None, 100).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
        assert_eq!(
            events[0].entry,
            JournalEntry::RepositoryObserved {
                repository: record.clone()
            }
        );
        assert!(events.iter().all(|e| e.repository_id == *f.repo()
            && e.workspace_id.as_ref() == Some(&linked.workspace_id)
            && e.legacy_repository_id.as_ref().unwrap().as_str() == legacy_id));
    }
    let connection = Connection::open(&path).unwrap();
    for (sql, original) in [
        ("SELECT entry_json FROM events WHERE sequence=1", historical),
        ("SELECT entry_json FROM events WHERE sequence=2", event_json),
        ("SELECT packet_json FROM jobs", job_json),
        ("SELECT record_json FROM evidence", evidence_json),
        (
            "SELECT packet_json FROM plans",
            serde_json::to_string(&plan()).unwrap(),
        ),
    ] {
        assert_eq!(
            connection
                .query_row(sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            original
        );
    }
    assert!(
        connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
    assert!(
        connection
            .execute("UPDATE events SET timestamp_ms=99", [])
            .is_err()
    );
    assert!(connection.execute("DELETE FROM events", []).is_err());
    // A post-migration write exercises restored ownership FKs and sequence allocation.
    let mut store = Store::open(&path, 5000).unwrap();
    store
        .register_repository(RepositoryInfo::discover(&f.root).unwrap())
        .unwrap();
    assert_eq!(
        store.events(Some(f.repo()), None, None, 1).unwrap()[0].sequence,
        3
    );
}

#[test]
fn v1_worktree_ownership_collision_rolls_back_entire_migration() {
    let f = Fixture::new();
    let linked = linked_workspace(&f);
    let path = f._temp.0.join("conflict.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("fixtures/substrate-v1.sql"))
        .unwrap();
    for info in [&f.info, &linked] {
        let (id, _) = insert_v1_repository(&connection, info);
        insert_v1_plan(&connection, &id);
    }
    let before: Vec<String> = connection
        .prepare("SELECT record_json FROM repositories ORDER BY repo_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(connection);
    assert!(
        Store::open(&path, 5000)
            .err()
            .unwrap()
            .to_string()
            .contains("no changes committed")
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let after: Vec<String> = connection
        .prepare("SELECT record_json FROM repositories ORDER BY repo_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM plans", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM schema_migrations", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='workspaces'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='events_no_update'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}
