CREATE TABLE runtime_runs (
 repo_id TEXT NOT NULL, plan_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
 record_json TEXT NOT NULL, cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
 PRIMARY KEY(repo_id,plan_id), FOREIGN KEY(repo_id,plan_id) REFERENCES execution_plans(repo_id,plan_id),
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id)
);
CREATE TABLE runtime_jobs (
 job_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
 plan_id TEXT, request_id TEXT, record_json TEXT NOT NULL,
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
 FOREIGN KEY(repo_id,plan_id) REFERENCES plans(repo_id,plan_id),
 FOREIGN KEY(request_id) REFERENCES planning_requests(request_id),
 CHECK((plan_id IS NOT NULL) != (request_id IS NOT NULL))
);
CREATE INDEX runtime_jobs_by_plan ON runtime_jobs(repo_id,plan_id);
CREATE TRIGGER runtime_runs_insert BEFORE INSERT ON runtime_runs
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.plan_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime-owned state requires controller authorization'); END;
CREATE TRIGGER runtime_runs_update BEFORE UPDATE ON runtime_runs
WHEN OLD.repo_id IS NOT NEW.repo_id OR OLD.plan_id IS NOT NEW.plan_id OR OLD.workspace_id IS NOT NEW.workspace_id
 OR (agentctl_runtime_authorized(OLD.repo_id,OLD.plan_id) IS NOT 1 AND NOT(NEW.cancel_requested=1 AND OLD.record_json=NEW.record_json))
BEGIN SELECT RAISE(ABORT,'runtime-owned state requires controller authorization'); END;
CREATE TRIGGER runtime_runs_delete BEFORE DELETE ON runtime_runs BEGIN SELECT RAISE(ABORT,'runtime history is retained'); END;
CREATE TRIGGER runtime_jobs_insert BEFORE INSERT ON runtime_jobs
WHEN agentctl_runtime_authorized(NEW.repo_id,coalesce(NEW.plan_id,NEW.request_id)) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime job requires controller authorization'); END;
CREATE TRIGGER runtime_jobs_update BEFORE UPDATE ON runtime_jobs
WHEN OLD.job_id IS NOT NEW.job_id OR OLD.repo_id IS NOT NEW.repo_id OR OLD.workspace_id IS NOT NEW.workspace_id OR OLD.plan_id IS NOT NEW.plan_id OR OLD.request_id IS NOT NEW.request_id
 OR agentctl_runtime_authorized(OLD.repo_id,coalesce(OLD.plan_id,OLD.request_id)) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime job requires controller authorization'); END;
CREATE TRIGGER runtime_jobs_delete BEFORE DELETE ON runtime_jobs BEGIN SELECT RAISE(ABORT,'runtime job history is retained'); END;
CREATE TRIGGER runtime_task_gate BEFORE UPDATE ON tasks
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE repo_id=OLD.repo_id AND plan_id=OLD.plan_id) AND agentctl_runtime_authorized(OLD.repo_id,OLD.plan_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime task requires controller authorization'); END;
CREATE TRIGGER runtime_job_create_gate BEFORE INSERT ON jobs
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE repo_id=NEW.repo_id AND plan_id=NEW.plan_id) AND agentctl_runtime_authorized(NEW.repo_id,NEW.plan_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime plan jobs must be issued by the controller'); END;
CREATE TRIGGER runtime_job_update_gate BEFORE UPDATE ON jobs
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE repo_id=OLD.repo_id AND plan_id=OLD.plan_id) AND agentctl_runtime_authorized(OLD.repo_id,OLD.plan_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime job success requires controller authorization'); END;
CREATE TRIGGER runtime_plan_gate BEFORE UPDATE ON execution_plans
WHEN EXISTS(SELECT 1 FROM runtime_runs WHERE repo_id=OLD.repo_id AND plan_id=OLD.plan_id) AND agentctl_runtime_authorized(OLD.repo_id,OLD.plan_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'runtime plan requires controller authorization'); END;
INSERT INTO schema_migrations VALUES (7,'provider_runtime');
PRAGMA user_version=7;
