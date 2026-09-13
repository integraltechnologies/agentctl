CREATE TABLE experiment_runs (
 experiment_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
 record_json TEXT NOT NULL, cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id)
);
CREATE INDEX experiment_runs_by_workspace ON experiment_runs(repo_id,workspace_id,created_at_ms);
CREATE TRIGGER experiment_runs_insert BEFORE INSERT ON experiment_runs
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment run requires controller authorization'); END;
CREATE TRIGGER experiment_runs_update BEFORE UPDATE ON experiment_runs
WHEN OLD.experiment_id IS NOT NEW.experiment_id OR OLD.repo_id IS NOT NEW.repo_id OR OLD.workspace_id IS NOT NEW.workspace_id OR OLD.created_at_ms IS NOT NEW.created_at_ms
 OR (agentctl_runtime_authorized(OLD.repo_id,OLD.experiment_id) IS NOT 1 AND NOT(NEW.cancel_requested=1 AND OLD.record_json=NEW.record_json))
BEGIN SELECT RAISE(ABORT,'experiment run requires controller authorization'); END;
CREATE TRIGGER experiment_runs_delete BEFORE DELETE ON experiment_runs BEGIN SELECT RAISE(ABORT,'experiment history is retained'); END;
INSERT INTO schema_migrations VALUES (8,'experiment_runtime');
PRAGMA user_version=8;
