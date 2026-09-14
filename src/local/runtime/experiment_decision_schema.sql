CREATE TABLE experiment_decisions (
 arrival_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 decision_id TEXT NOT NULL UNIQUE,
 repo_id TEXT NOT NULL,
 workspace_id TEXT NOT NULL,
 experiment_id TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt > 0),
 boundary_id TEXT NOT NULL,
 decided_at_ms INTEGER NOT NULL CHECK(decided_at_ms >= 0),
 requires_planner INTEGER NOT NULL CHECK(requires_planner IN (0,1)),
 record_json TEXT NOT NULL,
 UNIQUE(repo_id,experiment_id,attempt,boundary_id),
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
 FOREIGN KEY(experiment_id) REFERENCES experiment_runs(experiment_id)
);
CREATE INDEX experiment_decisions_by_experiment
 ON experiment_decisions(repo_id,workspace_id,experiment_id,arrival_sequence);
CREATE INDEX experiment_decisions_pending_planner
 ON experiment_decisions(repo_id,workspace_id,experiment_id,arrival_sequence)
 WHERE requires_planner=1;
CREATE TRIGGER experiment_decisions_insert BEFORE INSERT ON experiment_decisions
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment decision requires controller authorization'); END;
CREATE TRIGGER experiment_decisions_update BEFORE UPDATE ON experiment_decisions
BEGIN SELECT RAISE(ABORT,'experiment decisions are immutable'); END;
CREATE TRIGGER experiment_decisions_delete BEFORE DELETE ON experiment_decisions
BEGIN SELECT RAISE(ABORT,'experiment decision history is retained'); END;
CREATE TABLE experiment_wakeups (
 arrival_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 wakeup_id TEXT NOT NULL UNIQUE,
 repo_id TEXT NOT NULL,
 workspace_id TEXT NOT NULL,
 experiment_id TEXT NOT NULL,
 decision_id TEXT NOT NULL UNIQUE,
 planning_request_id TEXT NOT NULL,
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
 record_json TEXT NOT NULL,
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
 FOREIGN KEY(experiment_id) REFERENCES experiment_runs(experiment_id),
 FOREIGN KEY(decision_id) REFERENCES experiment_decisions(decision_id),
 FOREIGN KEY(planning_request_id) REFERENCES planning_requests(request_id)
);
CREATE INDEX experiment_wakeups_by_experiment
 ON experiment_wakeups(repo_id,workspace_id,experiment_id,arrival_sequence);
CREATE INDEX experiment_wakeups_by_request ON experiment_wakeups(planning_request_id);
CREATE TRIGGER experiment_wakeups_insert BEFORE INSERT ON experiment_wakeups
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment wakeup requires controller authorization'); END;
CREATE TRIGGER experiment_wakeups_update BEFORE UPDATE ON experiment_wakeups
BEGIN SELECT RAISE(ABORT,'experiment wakeups are immutable'); END;
CREATE TRIGGER experiment_wakeups_delete BEFORE DELETE ON experiment_wakeups
BEGIN SELECT RAISE(ABORT,'experiment wakeup history is retained'); END;
INSERT INTO schema_migrations VALUES (10,'experiment_decisions');
PRAGMA user_version=10;
