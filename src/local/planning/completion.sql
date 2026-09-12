-- SQL alone cannot mint a connection-local completion capability. Connections
-- without the application predicate fail closed (no such function).
CREATE TRIGGER execution_completion_guard BEFORE UPDATE ON execution_plans
WHEN NEW.state='COMPLETE'
BEGIN
    SELECT CASE WHEN agentctl_completion_authorized(NEW.repo_id,NEW.plan_id,NEW.workspace_id,NEW.integration_json,NEW.final_source_json) IS NOT 1
        THEN RAISE(ABORT,'completion requires the guarded operation') END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM events WHERE repo_id=NEW.repo_id AND plan_id=NEW.plan_id AND workspace_id=NEW.workspace_id
        AND json_extract(entry_json,'$.kind')='EXECUTION_PLAN_COMPLETED'
        AND json_extract(entry_json,'$.plan_id')=NEW.plan_id
        AND json_extract(entry_json,'$.verification')=NEW.integration_json
        AND json_extract(entry_json,'$.source')=NEW.final_source_json
    ) THEN RAISE(ABORT,'completion requires its matching audit event') END;
END;
-- Also reject INSERT/REPLACE shortcuts into a completed lifecycle.
CREATE TRIGGER execution_initial_state BEFORE INSERT ON execution_plans
WHEN NEW.state!='VALIDATED'
BEGIN SELECT RAISE(ABORT,'execution plans must start VALIDATED'); END;
INSERT INTO schema_migrations VALUES (6,'guarded_plan_completion');
PRAGMA user_version=6;
