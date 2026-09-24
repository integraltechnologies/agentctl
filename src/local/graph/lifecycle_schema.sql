-- Ontology generation lifecycle (schema 13). The live graph tables keep
-- materializing the most recent index pass; accepted truth is a separate,
-- durable pointer to an immutable snapshot, so an observed change never
-- becomes canonical by being indexed. The migration adds graph_entities.text_hash
-- (if absent) before this script. Legacy rows keep NULL hashes and are
-- re-derived by the next index pass before any snapshot is built.

-- Content-addressed, immutable ontology artifacts (per-file fact chunks,
-- snapshot manifests, semantic deltas). Readers verify hash and length.
CREATE TABLE ontology_blobs (
    hash TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL CHECK(bytes >= 0),
    body TEXT NOT NULL
);
CREATE TRIGGER ontology_blobs_no_update BEFORE UPDATE ON ontology_blobs
BEGIN SELECT RAISE(ABORT, 'ontology artifacts are immutable'); END;
CREATE TRIGGER ontology_blobs_no_delete BEFORE DELETE ON ontology_blobs
BEGIN SELECT RAISE(ABORT, 'ontology artifacts are immutable'); END;

-- One row per recorded observation. `ordinal` orders the workspace's lifecycle
-- independently of the content fingerprint, so a revert to historical content
-- is a new row. Identity columns never change; the only legal transitions are
-- CANDIDATE -> ACCEPTED | REJECTED | ABANDONED and ACCEPTED -> RETIRED.
CREATE TABLE ontology_generations (
    generation_id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 1),
    sequence INTEGER NOT NULL CHECK(sequence >= 1),
    fingerprint TEXT NOT NULL,
    snapshot TEXT NOT NULL REFERENCES ontology_blobs(hash),
    plan_id TEXT,
    state TEXT NOT NULL CHECK(state IN ('CANDIDATE','ACCEPTED','RETIRED','REJECTED','ABANDONED')),
    record_json TEXT NOT NULL,
    UNIQUE(workspace_id, ordinal),
    FOREIGN KEY(repo_id, workspace_id) REFERENCES workspaces(repo_id, workspace_id)
);
-- The canonical pointer: at most one ACCEPTED and one open CANDIDATE per workspace.
CREATE UNIQUE INDEX ontology_generations_accepted ON ontology_generations(workspace_id)
    WHERE state='ACCEPTED';
CREATE UNIQUE INDEX ontology_generations_candidate ON ontology_generations(workspace_id)
    WHERE state='CANDIDATE';
CREATE INDEX ontology_generations_by_plan ON ontology_generations(workspace_id, plan_id, ordinal)
    WHERE plan_id IS NOT NULL;
CREATE TRIGGER ontology_generations_insert BEFORE INSERT ON ontology_generations
WHEN NEW.state NOT IN ('CANDIDATE','ACCEPTED')
BEGIN SELECT RAISE(ABORT, 'a generation is recorded as CANDIDATE or ACCEPTED'); END;
CREATE TRIGGER ontology_generations_update BEFORE UPDATE ON ontology_generations
WHEN NEW.generation_id IS NOT OLD.generation_id
  OR NEW.repo_id IS NOT OLD.repo_id
  OR NEW.workspace_id IS NOT OLD.workspace_id
  OR NEW.ordinal IS NOT OLD.ordinal
  OR NEW.sequence IS NOT OLD.sequence
  OR NEW.fingerprint IS NOT OLD.fingerprint
  OR NEW.snapshot IS NOT OLD.snapshot
  OR NEW.plan_id IS NOT OLD.plan_id
  OR NOT ((OLD.state = 'CANDIDATE' AND NEW.state IN ('ACCEPTED','REJECTED','ABANDONED'))
       OR (OLD.state = 'ACCEPTED' AND NEW.state = 'RETIRED'))
BEGIN SELECT RAISE(ABORT, 'illegal ontology generation transition'); END;
CREATE TRIGGER ontology_generations_delete BEFORE DELETE ON ontology_generations
BEGIN SELECT RAISE(ABORT, 'ontology generations are retained'); END;
INSERT INTO schema_migrations VALUES (13, 'ontology_lifecycle');
PRAGMA user_version=13;
