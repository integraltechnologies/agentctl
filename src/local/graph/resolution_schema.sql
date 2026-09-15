-- Workspace-level relation resolution, derived from graph_edges/graph_entities and
-- rebuilt in the same transaction whenever an index pass re-derives any file.
-- Per-file extraction facts stay in graph_edges; cross-file targets live here so
-- re-deriving one file cannot cascade away another file's edges. The migration
-- adds graph_edges.path_hint (if absent) before this script, and afterwards
-- backfills hints from each edge's record and rebuilds every workspace.
CREATE INDEX graph_edges_hinted ON graph_edges(workspace_id, edge_id)
    WHERE target_id IS NULL AND path_hint IS NOT NULL;
CREATE TABLE graph_resolutions (
    workspace_id TEXT NOT NULL,
    edge_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    rule TEXT NOT NULL,
    PRIMARY KEY(workspace_id, edge_id),
    FOREIGN KEY(workspace_id, edge_id) REFERENCES graph_edges(workspace_id, edge_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, target_id) REFERENCES graph_entities(workspace_id, entity_id) ON DELETE CASCADE
);
CREATE INDEX graph_resolutions_outgoing ON graph_resolutions(workspace_id, source_id, kind, edge_id);
CREATE INDEX graph_resolutions_incoming ON graph_resolutions(workspace_id, target_id, kind, edge_id);
INSERT INTO schema_migrations VALUES (12, 'graph_resolution');
PRAGMA user_version=12;
