-- Schema version 7 -> 8: executor attempts, the repository baseline each
-- was intended against, and the changes captured afterwards. No existing
-- state implies any attempt. The definitions are version 8's, which
-- version 9 reshapes.
-- The one executor attempt of a generation, from its INTEND on. Its journal
-- entry says how far it got: intended and never attempted, attempted with
-- its outcome unknown, or reconciled together with the recording of
-- `outcome`, once agentctl captured what changed in the repository.
-- `authority` is the JSON array of literal paths the executor was
-- authorized to mutate, every one owned by the generation when intended.
-- `head` and `head_after` identify Git's HEAD before and after.
--
-- A 'candidate' is only structurally valid: its invocation succeeded, its
-- well-formed result reported success, and every observed change was
-- authorized and attributable to it. It is not verified, accepted or
-- complete. `reported` and `claimed` are what the executor said, when it
-- said anything well formed: evidence, never proof. `attribution` says why
-- observed changes could not be attributed to the executor.
CREATE TABLE executions (
    id            INTEGER PRIMARY KEY,
    generation_id INTEGER NOT NULL UNIQUE REFERENCES generations (id),
    agent_id      INTEGER NOT NULL UNIQUE REFERENCES agents (id),
    journal_id    INTEGER NOT NULL UNIQUE REFERENCES journal (id),
    authority     TEXT    NOT NULL
        CHECK (json_valid(authority) AND json_type(authority) = 'array'),
    head          TEXT    NOT NULL CHECK (head <> ''),
    started_at    INTEGER NOT NULL,
    outcome       TEXT    CHECK (outcome IN ('candidate', 'reported_failed',
        'malformed_result', 'invocation_failed', 'scope_violated', 'unattributable')),
    attribution   TEXT    CHECK (attribution IN
        ('unsettled', 'never_launched', 'contested', 'concurrent')),
    reported      TEXT    CHECK (reported IN ('succeeded', 'failed')),
    claimed       TEXT    CHECK (json_valid(claimed) AND json_type(claimed) = 'array'),
    head_after    TEXT    CHECK (head_after <> ''),
    captured_at   INTEGER,
    CHECK ((outcome IS NULL) = (captured_at IS NULL)),
    CHECK ((outcome IS NULL) = (head_after IS NULL)),
    CHECK ((coalesce(outcome, '') = 'unattributable') = (attribution IS NOT NULL)),
    CHECK ((reported IS NULL) = (claimed IS NULL)),
    CHECK (outcome IS NOT NULL OR reported IS NULL),
    CHECK (coalesce(outcome, '') <> 'candidate' OR reported = 'succeeded'),
    CHECK (coalesce(outcome, '') <> 'reported_failed' OR reported = 'failed'),
    CHECK (coalesce(outcome, '') NOT IN ('invocation_failed', 'malformed_result')
        OR reported IS NULL)
) STRICT;

CREATE TRIGGER executions_captured_once BEFORE UPDATE ON executions
WHEN OLD.outcome IS NOT NULL OR NEW.id IS NOT OLD.id
    OR NEW.generation_id IS NOT OLD.generation_id OR NEW.agent_id IS NOT OLD.agent_id
    OR NEW.journal_id IS NOT OLD.journal_id OR NEW.authority IS NOT OLD.authority
    OR NEW.head IS NOT OLD.head OR NEW.started_at IS NOT OLD.started_at
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER executions_no_delete BEFORE DELETE ON executions
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;

-- The repository as agentctl observed it before an execution was intended:
-- the entry at each path Git considers repository content, outside
-- agentctl's and Git's own state. A tracked path may be absent; a regular
-- file or symlink is identified by the SHA-256 of its bytes or target.
CREATE TABLE execution_baseline (
    execution_id INTEGER NOT NULL REFERENCES executions (id),
    path         TEXT    NOT NULL CHECK (path <> ''),
    kind         TEXT    NOT NULL CHECK (kind IN ('absent', 'file', 'symlink', 'other')),
    hash         TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    CHECK ((kind IN ('file', 'symlink')) = (hash IS NOT NULL)),
    PRIMARY KEY (execution_id, path)
) STRICT, WITHOUT ROWID;

-- Each path whose entry an execution's capture found different from its
-- baseline (absent when it had none), and whether the execution was
-- authorized to mutate it.
CREATE TABLE execution_changes (
    execution_id INTEGER NOT NULL REFERENCES executions (id),
    path         TEXT    NOT NULL CHECK (path <> ''),
    before_kind  TEXT    NOT NULL
        CHECK (before_kind IN ('absent', 'file', 'symlink', 'other')),
    before_hash  TEXT
        CHECK (length(before_hash) = 64 AND before_hash NOT GLOB '*[^0-9a-f]*'),
    after_kind   TEXT    NOT NULL
        CHECK (after_kind IN ('absent', 'file', 'symlink', 'other')),
    after_hash   TEXT
        CHECK (length(after_hash) = 64 AND after_hash NOT GLOB '*[^0-9a-f]*'),
    authorized   INTEGER NOT NULL CHECK (authorized IN (0, 1)),
    CHECK ((before_kind IN ('file', 'symlink')) = (before_hash IS NOT NULL)),
    CHECK ((after_kind IN ('file', 'symlink')) = (after_hash IS NOT NULL)),
    CHECK (before_kind IS NOT after_kind OR before_hash IS NOT after_hash),
    PRIMARY KEY (execution_id, path)
) STRICT, WITHOUT ROWID;
