CREATE TABLE memory_entries (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id TEXT NOT NULL UNIQUE,
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    workspace_id TEXT REFERENCES workspaces(workspace_id),
    trust TEXT NOT NULL CHECK(trust IN ('CANONICAL','DERIVED','OBSERVED','AGENT_NOTE')),
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('ACTIVE','SUPERSEDED','REJECTED')),
    canonical_key TEXT,
    dedupe_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    superseded_by TEXT REFERENCES memory_entries(memory_id) DEFERRABLE INITIALLY DEFERRED,
    record_json TEXT NOT NULL,
    FOREIGN KEY(repo_id,workspace_id) REFERENCES workspaces(repo_id,workspace_id),
    CHECK((status='SUPERSEDED') = (superseded_by IS NOT NULL)),
    CHECK(trust='CANONICAL' OR canonical_key IS NULL)
);
CREATE UNIQUE INDEX memory_canonical_key ON memory_entries(repo_id,canonical_key) WHERE status='ACTIVE' AND canonical_key IS NOT NULL;
CREATE UNIQUE INDEX memory_active_dedupe ON memory_entries(repo_id,dedupe_key) WHERE status='ACTIVE';
CREATE INDEX memory_retrieval ON memory_entries(repo_id,status,trust,kind,created_at_ms DESC,memory_id);
CREATE INDEX memory_workspace ON memory_entries(repo_id,workspace_id,status);
CREATE TABLE memory_links (
    memory_id TEXT NOT NULL REFERENCES memory_entries(memory_id),
    repo_id TEXT NOT NULL REFERENCES repositories(repo_id),
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    PRIMARY KEY(memory_id,kind,target)
);
CREATE INDEX memory_link_lookup ON memory_links(repo_id,kind,target,memory_id);
CREATE VIRTUAL TABLE memory_fts USING fts5(tokens, tokenize='unicode61');
CREATE TRIGGER memory_no_delete BEFORE DELETE ON memory_entries BEGIN SELECT RAISE(ABORT,'memory history cannot be deleted'); END;
CREATE TRIGGER memory_immutable BEFORE UPDATE ON memory_entries
WHEN OLD.seq IS NOT NEW.seq OR OLD.memory_id IS NOT NEW.memory_id OR OLD.repo_id IS NOT NEW.repo_id
 OR OLD.workspace_id IS NOT NEW.workspace_id OR OLD.trust IS NOT NEW.trust OR OLD.kind IS NOT NEW.kind
 OR OLD.canonical_key IS NOT NEW.canonical_key OR OLD.dedupe_key IS NOT NEW.dedupe_key
 OR OLD.created_at_ms IS NOT NEW.created_at_ms OR OLD.record_json IS NOT NEW.record_json
 OR OLD.status!='ACTIVE' OR NEW.status NOT IN ('SUPERSEDED','REJECTED')
 OR NEW.updated_at_ms < OLD.updated_at_ms
BEGIN SELECT RAISE(ABORT,'memory payloads are immutable; only active supersession/rejection is allowed'); END;
CREATE TRIGGER memory_links_no_update BEFORE UPDATE ON memory_links BEGIN SELECT RAISE(ABORT,'memory links are immutable'); END;
CREATE TRIGGER memory_links_no_delete BEFORE DELETE ON memory_links BEGIN SELECT RAISE(ABORT,'memory links are historical'); END;
INSERT INTO schema_migrations VALUES (4,'engineering_memory');
PRAGMA user_version=4;
