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
