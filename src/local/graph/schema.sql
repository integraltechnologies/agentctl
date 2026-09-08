CREATE TABLE graph_indexes (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id),
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    metadata_json TEXT NOT NULL,
    FOREIGN KEY(repo_id, workspace_id) REFERENCES workspaces(repo_id, workspace_id)
);
CREATE TABLE indexed_files (
    workspace_id TEXT NOT NULL REFERENCES graph_indexes(workspace_id),
    path TEXT NOT NULL,
    content_hash TEXT,
    backend TEXT NOT NULL,
    diagnostic TEXT,
    PRIMARY KEY(workspace_id, path)
);
CREATE TABLE graph_entities (
    workspace_id TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(workspace_id, entity_id),
    FOREIGN KEY(workspace_id, path) REFERENCES indexed_files(workspace_id, path) ON DELETE CASCADE
);
CREATE INDEX graph_entities_name ON graph_entities(workspace_id, name);
CREATE INDEX graph_entities_qualified ON graph_entities(workspace_id, qualified_name);
CREATE INDEX graph_entities_path ON graph_entities(workspace_id, path, entity_id);
CREATE TABLE graph_edges (
    workspace_id TEXT NOT NULL,
    edge_id TEXT NOT NULL,
    path TEXT NOT NULL,
    source_id TEXT NOT NULL,
    target_id TEXT,
    kind TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(workspace_id, edge_id),
    FOREIGN KEY(workspace_id, path) REFERENCES indexed_files(workspace_id, path) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, source_id) REFERENCES graph_entities(workspace_id, entity_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, target_id) REFERENCES graph_entities(workspace_id, entity_id) ON DELETE CASCADE
);
CREATE INDEX graph_edges_outgoing ON graph_edges(workspace_id, source_id, kind, edge_id);
CREATE INDEX graph_edges_incoming ON graph_edges(workspace_id, target_id, kind, edge_id);
CREATE INDEX graph_edges_path ON graph_edges(workspace_id, path);
INSERT INTO schema_migrations VALUES (3, 'code_graph');
PRAGMA user_version=3;
