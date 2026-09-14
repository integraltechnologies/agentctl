CREATE TABLE experiment_decision_cursors (
 repo_id TEXT NOT NULL,
 workspace_id TEXT NOT NULL,
 experiment_id TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt > 0),
 boundaries_hash TEXT NOT NULL,
 last_evaluated_arrival_sequence INTEGER NOT NULL DEFAULT 0 CHECK(last_evaluated_arrival_sequence >= 0),
 updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
 PRIMARY KEY(repo_id,experiment_id,attempt),
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
 FOREIGN KEY(experiment_id) REFERENCES experiment_runs(experiment_id)
);
CREATE TRIGGER experiment_decision_cursors_insert BEFORE INSERT ON experiment_decision_cursors
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment decision cursor requires controller authorization'); END;
CREATE TRIGGER experiment_decision_cursors_update BEFORE UPDATE ON experiment_decision_cursors
WHEN OLD.repo_id IS NOT NEW.repo_id OR OLD.workspace_id IS NOT NEW.workspace_id
 OR OLD.experiment_id IS NOT NEW.experiment_id OR OLD.attempt IS NOT NEW.attempt
 OR OLD.boundaries_hash IS NOT NEW.boundaries_hash
 OR NEW.last_evaluated_arrival_sequence < OLD.last_evaluated_arrival_sequence
 OR agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment decision cursor requires controller authorization'); END;
CREATE TRIGGER experiment_decision_cursors_delete BEFORE DELETE ON experiment_decision_cursors
BEGIN SELECT RAISE(ABORT,'experiment decision cursor history is retained'); END;
INSERT INTO schema_migrations VALUES (11,'experiment_decision_cursors');
PRAGMA user_version=11;
