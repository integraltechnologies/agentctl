CREATE TABLE experiment_events (
 arrival_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 repo_id TEXT NOT NULL,
 workspace_id TEXT NOT NULL,
 experiment_id TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt > 0),
 channel TEXT NOT NULL,
 source_sequence INTEGER NOT NULL CHECK(source_sequence >= 0),
 event_type TEXT NOT NULL CHECK(event_type IN ('METRIC','CHECKPOINT','HEALTH','PROCESS_STATUS')),
 metric_name TEXT,
 timestamp_ms INTEGER NOT NULL CHECK(timestamp_ms >= 0),
 observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
 frame_hash TEXT NOT NULL,
 event_json TEXT NOT NULL,
 UNIQUE(repo_id,experiment_id,attempt,channel,source_sequence),
 FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
 FOREIGN KEY(experiment_id) REFERENCES experiment_runs(experiment_id)
);
CREATE INDEX experiment_events_by_experiment
 ON experiment_events(repo_id,workspace_id,experiment_id,attempt,arrival_sequence);
CREATE INDEX experiment_metrics_by_name
 ON experiment_events(repo_id,workspace_id,experiment_id,attempt,metric_name,arrival_sequence)
 WHERE event_type='METRIC';
CREATE INDEX experiment_events_by_type
 ON experiment_events(repo_id,workspace_id,experiment_id,event_type,arrival_sequence);
CREATE TRIGGER experiment_events_insert BEFORE INSERT ON experiment_events
WHEN agentctl_runtime_authorized(NEW.repo_id,NEW.experiment_id) IS NOT 1
BEGIN SELECT RAISE(ABORT,'experiment event requires controller authorization'); END;
CREATE TRIGGER experiment_events_update BEFORE UPDATE ON experiment_events
BEGIN SELECT RAISE(ABORT,'experiment events are append-only'); END;
CREATE TRIGGER experiment_events_delete BEFORE DELETE ON experiment_events
BEGIN SELECT RAISE(ABORT,'experiment event history is retained'); END;
INSERT INTO schema_migrations VALUES (9,'experiment_events');
PRAGMA user_version=9;
