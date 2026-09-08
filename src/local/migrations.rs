use std::collections::BTreeMap;

use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    Error, Result,
    repository::{RepositoryId, RepositoryInfo, WorkspaceId},
    require,
    store::{RegisteredRepository, RegisteredWorkspace},
};

pub const SCHEMA_VERSION: i64 = 2;
pub const APPLICATION_ID: i64 = 0x41475443; // AGTC

const INITIAL: &str = r#"
CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL);
INSERT INTO schema_migrations VALUES (1, 'local_substrate');
CREATE TABLE repositories (
    repo_id TEXT PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    git_directory TEXT NOT NULL UNIQUE,
    record_json TEXT NOT NULL
);
CREATE TABLE plans (
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    plan_id TEXT NOT NULL,
    packet_json TEXT NOT NULL,
    PRIMARY KEY(repo_id, plan_id)
);
CREATE TABLE tasks (
    repo_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    state_json TEXT NOT NULL CHECK (state_json IN ('"PLANNED"','"READY"','"EXECUTING"','"AWAITING_VERIFICATION"','"VERIFYING"','"VERIFIED"','"REJECTED"','"BLOCKED"')),
    PRIMARY KEY(repo_id, task_id),
    FOREIGN KEY(repo_id, plan_id) REFERENCES plans(repo_id, plan_id)
);
CREATE INDEX tasks_by_plan ON tasks(repo_id, plan_id, task_id);
CREATE TABLE jobs (
    repo_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    task_id TEXT,
    packet_json TEXT NOT NULL,
    PRIMARY KEY(repo_id, job_id),
    FOREIGN KEY(repo_id, plan_id) REFERENCES plans(repo_id, plan_id),
    FOREIGN KEY(repo_id, task_id) REFERENCES tasks(repo_id, task_id)
);
CREATE TABLE evidence (
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    evidence_id TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(repo_id, evidence_id)
);
CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    external_event_id TEXT,
    timestamp_ms INTEGER NOT NULL CHECK(timestamp_ms >= 0),
    plan_id TEXT,
    task_id TEXT,
    job_id TEXT,
    entry_json TEXT NOT NULL,
    UNIQUE(repo_id, external_event_id),
    FOREIGN KEY(repo_id, plan_id) REFERENCES plans(repo_id, plan_id),
    FOREIGN KEY(repo_id, task_id) REFERENCES tasks(repo_id, task_id),
    FOREIGN KEY(repo_id, job_id) REFERENCES jobs(repo_id, job_id)
);
CREATE INDEX events_by_repo ON events(repo_id, sequence);
CREATE INDEX events_by_task ON events(repo_id, task_id, sequence);
CREATE INDEX events_by_job ON events(repo_id, job_id, sequence);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
PRAGMA application_id = 1095193667;
PRAGMA user_version = 1;
"#;

pub fn version(connection: &Connection) -> Result<i64> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn header(connection: &Connection) -> Result<i64> {
    let version = version(connection)?;
    let app: i64 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    require(
        version <= SCHEMA_VERSION,
        format!(
            "database schema version {version} is newer than supported {SCHEMA_VERSION}; use a compatible agentctl"
        ),
    )?;
    require(
        (version == 0 && app == 0)
            || ((1..=SCHEMA_VERSION).contains(&version) && app == APPLICATION_ID),
        format!(
            "unrecognized agentctl database (application_id={app}, schema={version}); refusing to modify it"
        ),
    )?;
    Ok(version)
}

pub fn check(connection: &Connection) -> Result<()> {
    require(
        header(connection)? == SCHEMA_VERSION,
        "database needs initialization or migration; run agentctl init",
    )?;
    check_version(connection, SCHEMA_VERSION)
}

fn check_version(connection: &Connection, version: i64) -> Result<()> {
    let migrations: Vec<(i64, String)> = connection
        .prepare("SELECT version, name FROM schema_migrations ORDER BY version")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    require(
        migrations
            == if version == 1 {
                vec![(1, "local_substrate".into())]
            } else {
                vec![
                    (1, "local_substrate".into()),
                    (2, "repository_workspaces".into()),
                ]
            },
        "database migration history does not match schema version",
    )?;
    // Check required schema objects; ordinary open must not silently repair corruption.
    for (kind, name) in [
        ("table", "repositories"),
        ("table", "plans"),
        ("table", "tasks"),
        ("table", "jobs"),
        ("table", "evidence"),
        ("table", "events"),
        ("trigger", "events_no_update"),
        ("trigger", "events_no_delete"),
    ] {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type=?1 AND name=?2",
            [kind, name],
            |row| row.get(0),
        )?;
        require(count == 1, format!("database is missing {kind} {name}"))?;
    }
    if version >= 2 {
        connection.prepare(
            "SELECT workspace_id,repo_id,root,git_directory,record_json FROM workspaces LIMIT 0",
        )?;
        for table in ["jobs", "evidence", "events"] {
            connection.prepare(&format!("SELECT workspace_id FROM {table} LIMIT 0"))?;
        }
    }
    Ok(())
}

pub fn migrate(connection: &mut Connection) -> Result<()> {
    // SQLite's documented table-rebuild pattern: disable FK enforcement only around
    // the migration transaction, explicitly check all references, then restore it.
    connection.pragma_update(None, "foreign_keys", false)?;
    let result = migrate_transaction(connection);
    connection.pragma_update(None, "foreign_keys", true)?;
    result
}

fn migrate_transaction(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if header(&transaction)? == 0 {
        let tables: i64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        require(
            tables == 0,
            "unversioned nonempty database; refusing to adopt unrelated state",
        )?;
        transaction.execute_batch(INITIAL)?;
    }
    if header(&transaction)? == 1 {
        check_version(&transaction, 1)?;
        split_workspaces(&transaction).map_err(|e| Error::Invalid(format!("repository/workspace migration failed; no changes committed (resolve conflicting repository-scoped IDs before retrying): {e}")))?;
    }
    check(&transaction)?;
    require(
        transaction
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "migration foreign-key check failed",
    )?;
    transaction.commit()?;
    Ok(())
}

fn split_workspaces(connection: &Connection) -> Result<()> {
    let old_rows: Vec<(String, String)> = connection
        .prepare("SELECT repo_id,record_json FROM repositories ORDER BY repo_id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    connection.execute_batch(r#"
CREATE TABLE repositories_v2 (repo_id TEXT PRIMARY KEY, common_directory TEXT NOT NULL UNIQUE, record_json TEXT NOT NULL);
CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    root TEXT NOT NULL UNIQUE,
    git_directory TEXT NOT NULL UNIQUE,
    record_json TEXT NOT NULL,
    UNIQUE(repo_id,workspace_id)
);
CREATE INDEX workspaces_by_repo ON workspaces(repo_id,workspace_id);
CREATE TEMP TABLE identity_map (old_repo_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL);
ALTER TABLE jobs ADD COLUMN workspace_id TEXT REFERENCES workspaces(workspace_id);
ALTER TABLE evidence ADD COLUMN workspace_id TEXT REFERENCES workspaces(workspace_id);
ALTER TABLE events ADD COLUMN workspace_id TEXT REFERENCES workspaces(workspace_id);
ALTER TABLE events ADD COLUMN legacy_repository_id TEXT;
DROP TRIGGER events_no_update;
"#)?;
    let mut repositories: BTreeMap<RepositoryId, RegisteredRepository> = BTreeMap::new();
    for (old_id, json) in old_rows {
        // Convert only registration metadata. Protocol packets and journal payloads
        // remain byte-for-byte intact; no live filesystem access is needed.
        let mut value: serde_json::Value = serde_json::from_str(&json)?;
        let git_directory: std::path::PathBuf =
            serde_json::from_value(value["info"]["git_directory"].clone())?;
        let common_directory: std::path::PathBuf =
            serde_json::from_value(value["info"]["common_directory"].clone())?;
        let repo = RepositoryId::for_common_directory(&common_directory);
        let workspace = WorkspaceId::for_git_directory(&git_directory);
        require(
            old_id == RepositoryId::for_common_directory(&git_directory).as_str(),
            "legacy repository ID does not match its stored Git directory",
        )?;
        let common_identity = if git_directory == common_directory {
            value["info"]["git_directory_identity"].clone()
        } else {
            serde_json::Value::Null
        };
        value["info"]["repository_id"] = serde_json::to_value(&repo)?;
        value["info"]["workspace_id"] = serde_json::to_value(&workspace)?;
        value["info"]["common_directory_identity"] = common_identity;
        value["info"]["source"]["repository_id"] = serde_json::to_value(&repo)?;
        value["info"]["source"]["workspace_id"] = serde_json::to_value(&workspace)?;
        let record: RegisteredWorkspace = serde_json::from_value(value)?;
        record.info.validate()?;
        let info: &RepositoryInfo = &record.info;
        let logical = repositories
            .entry(repo.clone())
            .or_insert_with(|| RegisteredRepository {
                repository_id: repo.clone(),
                common_directory: common_directory.clone(),
                common_directory_identity: info.common_directory_identity.clone(),
                remotes: info.remotes.clone(),
                first_seen_ms: record.first_seen_ms,
                last_seen_ms: record.last_seen_ms,
            });
        logical.first_seen_ms = logical.first_seen_ms.min(record.first_seen_ms);
        if record.last_seen_ms >= logical.last_seen_ms {
            logical.last_seen_ms = record.last_seen_ms;
            logical.remotes = info.remotes.clone();
        }
        if info.common_directory_identity.is_some() {
            logical.common_directory_identity = info.common_directory_identity.clone();
        }
        connection.execute(
            "INSERT INTO workspaces VALUES (?1,?2,?3,?4,?5)",
            params![
                workspace.as_str(),
                repo.as_str(),
                info.root
                    .to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF-8 workspace root".into()))?,
                git_directory
                    .to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF-8 Git directory".into()))?,
                serde_json::to_string(&record)?
            ],
        )?;
        connection.execute(
            "INSERT INTO identity_map VALUES (?1,?2,?3)",
            params![old_id, repo.as_str(), workspace.as_str()],
        )?;
    }
    for logical in repositories.values() {
        logical.validate()?;
        connection.execute(
            "INSERT INTO repositories_v2 VALUES (?1,?2,?3)",
            params![
                logical.repository_id.as_str(),
                logical
                    .common_directory
                    .to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF-8 common directory".into()))?,
                serde_json::to_string(logical)?
            ],
        )?;
    }
    for table in ["plans", "tasks", "jobs", "evidence", "events"] {
        if matches!(table, "jobs" | "evidence" | "events") {
            connection.execute(&format!("UPDATE {table} SET workspace_id=(SELECT workspace_id FROM identity_map WHERE old_repo_id={table}.repo_id)"), [])?;
        }
        if table == "events" {
            connection.execute("UPDATE events SET legacy_repository_id=repo_id", [])?;
        }
        // A uniqueness collision aborts the entire migration; never choose one task,
        // job, evidence record, or event over another silently.
        connection.execute(&format!("UPDATE {table} SET repo_id=(SELECT repo_id FROM identity_map WHERE old_repo_id={table}.repo_id)"), [])?;
    }
    connection.execute_batch(r#"
DROP TABLE repositories;
ALTER TABLE repositories_v2 RENAME TO repositories;
DROP TABLE identity_map;
CREATE INDEX events_by_workspace ON events(workspace_id,sequence);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
INSERT INTO schema_migrations VALUES (2,'repository_workspaces');
PRAGMA user_version=2;
"#)?;
    Ok(())
}
