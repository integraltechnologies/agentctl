CREATE TABLE planning_requests (
    request_id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    workspace_id TEXT NOT NULL,
    packet_json TEXT NOT NULL,
    UNIQUE(request_id,repo_id,workspace_id),
    FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id)
);
CREATE TABLE execution_plans (
    repo_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    request_id TEXT NOT NULL REFERENCES planning_requests(request_id),
    workspace_id TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('VALIDATED','ACTIVE','COMPLETE','SUPERSEDED','CANCELLED')),
    updated_at_ms INTEGER NOT NULL,
    superseded_by TEXT,
    integration_json TEXT,
    final_source_json TEXT,
    PRIMARY KEY(repo_id,plan_id),
    FOREIGN KEY(repo_id,plan_id) REFERENCES plans(repo_id,plan_id),
    FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
    FOREIGN KEY(request_id,repo_id,workspace_id) REFERENCES planning_requests(request_id,repo_id,workspace_id),
    FOREIGN KEY(repo_id,superseded_by) REFERENCES execution_plans(repo_id,plan_id),
    CHECK((state='SUPERSEDED')=(superseded_by IS NOT NULL)),
    CHECK((state='COMPLETE')=(integration_json IS NOT NULL AND final_source_json IS NOT NULL))
);
CREATE INDEX planning_by_workspace ON planning_requests(repo_id,workspace_id,request_id);
CREATE INDEX execution_by_workspace ON execution_plans(repo_id,workspace_id,state,updated_at_ms,plan_id);
CREATE UNIQUE INDEX one_active_execution_plan ON execution_plans(repo_id,workspace_id) WHERE state='ACTIVE';
CREATE TRIGGER planning_requests_immutable BEFORE UPDATE ON planning_requests BEGIN SELECT RAISE(ABORT,'planning requests are immutable'); END;
CREATE TRIGGER planning_requests_no_delete BEFORE DELETE ON planning_requests BEGIN SELECT RAISE(ABORT,'planning requests are historical'); END;
CREATE TRIGGER execution_plans_no_delete BEFORE DELETE ON execution_plans BEGIN SELECT RAISE(ABORT,'execution plans are historical'); END;
CREATE TRIGGER execution_plans_immutable BEFORE UPDATE ON execution_plans
WHEN OLD.repo_id IS NOT NEW.repo_id OR OLD.plan_id IS NOT NEW.plan_id OR OLD.request_id IS NOT NEW.request_id
 OR OLD.workspace_id IS NOT NEW.workspace_id OR OLD.metadata_json IS NOT NEW.metadata_json
 OR OLD.state NOT IN ('VALIDATED','ACTIVE')
 OR NEW.updated_at_ms < OLD.updated_at_ms
 OR (NEW.state='COMPLETE' AND OLD.state!='ACTIVE')
BEGIN SELECT RAISE(ABORT,'execution plan payload/history is immutable'); END;
-- Only Stage 4-owned task rows gain the activation gate. Legacy Stage 1 plans are unchanged.
CREATE TRIGGER execution_task_gate BEFORE UPDATE ON tasks
WHEN EXISTS(SELECT 1 FROM execution_plans e WHERE e.repo_id=OLD.repo_id AND e.plan_id=OLD.plan_id AND e.state!='ACTIVE')
BEGIN SELECT RAISE(ABORT,'task belongs to an inactive execution plan'); END;
INSERT INTO schema_migrations VALUES (5,'planning_substrate');
PRAGMA user_version=5;
