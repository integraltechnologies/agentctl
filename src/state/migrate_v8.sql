-- Schema version 8 -> 9: an execution's attempt never changes, and what its
-- capture established moves to `execution_captures`, which exists only as
-- derived: consistent with how the invocation ended, the changes recorded
-- against the baseline, HEAD, ownership and the actions journaled alongside,
-- and reconciled together with the journal. The definitions must match
-- schema.sql.
--
-- Nothing uncaptured becomes captured and no evidence is invented: an
-- outcome only counts with its journal entry reconciled, which `Store`
-- always did in the same transaction, so one without is dropped. Captured
-- outcomes are kept, except that version 8 doubted only changes beyond
-- authority while another action was in flight: any other outcome with
-- changes observed while another journaled action was in flight between
-- its baseline and its capture becomes unattributable, as a capture would
-- now. The journal keeps what was reconciled then.
DROP TRIGGER executions_captured_once;
DROP TRIGGER executions_no_delete;
ALTER TABLE execution_changes RENAME TO execution_changes_v8;
ALTER TABLE execution_baseline RENAME TO execution_baseline_v8;
ALTER TABLE executions RENAME TO executions_v8;

CREATE TABLE executions (
    id            INTEGER PRIMARY KEY,
    generation_id INTEGER NOT NULL UNIQUE REFERENCES generations (id),
    agent_id      INTEGER NOT NULL UNIQUE REFERENCES agents (id),
    journal_id    INTEGER NOT NULL UNIQUE REFERENCES journal (id),
    authority     TEXT    NOT NULL
        CHECK (json_valid(authority) AND json_type(authority) = 'array'),
    head          TEXT    NOT NULL CHECK (head <> ''),
    started_at    INTEGER NOT NULL
) STRICT;
INSERT INTO executions
    SELECT id, generation_id, agent_id, journal_id, authority, head, started_at
    FROM executions_v8;

CREATE TABLE execution_baseline (
    execution_id INTEGER NOT NULL REFERENCES executions (id),
    path         TEXT    NOT NULL CHECK (path <> ''),
    kind         TEXT    NOT NULL CHECK (kind IN ('absent', 'file', 'symlink', 'other')),
    hash         TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    CHECK ((kind IN ('file', 'symlink')) = (hash IS NOT NULL)),
    PRIMARY KEY (execution_id, path)
) STRICT, WITHOUT ROWID;
INSERT INTO execution_baseline SELECT execution_id, path, kind, hash FROM execution_baseline_v8;

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
INSERT INTO execution_changes
    SELECT execution_id, path, before_kind, before_hash, after_kind, after_hash, authorized
    FROM execution_changes_v8;

CREATE TABLE execution_captures (
    execution_id INTEGER PRIMARY KEY REFERENCES executions (id),
    outcome      TEXT    NOT NULL CHECK (outcome IN ('candidate', 'reported_failed',
        'malformed_result', 'invocation_failed', 'scope_violated', 'unattributable')),
    attribution  TEXT    CHECK (attribution IN
        ('unsettled', 'never_launched', 'contested', 'concurrent')),
    reported     TEXT    CHECK (reported IN ('succeeded', 'failed')),
    claimed      TEXT    CHECK (json_valid(claimed) AND json_type(claimed) = 'array'),
    head_after   TEXT    NOT NULL CHECK (head_after <> ''),
    captured_at  INTEGER NOT NULL,
    CHECK ((outcome = 'unattributable') = (attribution IS NOT NULL)),
    CHECK ((reported IS NULL) = (claimed IS NULL)),
    CHECK (outcome <> 'candidate' OR reported = 'succeeded'),
    CHECK (outcome <> 'reported_failed' OR reported = 'failed'),
    CHECK (outcome NOT IN ('invocation_failed', 'malformed_result') OR reported IS NULL)
) STRICT;
INSERT INTO execution_captures
    SELECT id, CASE WHEN doubted THEN 'unattributable' ELSE outcome END,
           CASE WHEN doubted THEN 'concurrent' ELSE attribution END,
           reported, claimed, head_after, captured_at
    FROM (SELECT e.*, e.outcome <> 'unattributable'
            AND (e.head_after <> e.head
                OR EXISTS (SELECT 1 FROM execution_changes c WHERE c.execution_id = e.id))
            AND EXISTS (SELECT 1 FROM journal o WHERE o.id <> e.journal_id
                AND o.attempted_at <= e.captured_at
                AND coalesce(o.reconciled_at >= e.started_at, 1)) AS doubted
          FROM executions_v8 e JOIN journal j ON j.id = e.journal_id
          WHERE e.outcome IS NOT NULL AND j.state = 'reconciled');

DROP TABLE execution_changes_v8;
DROP TABLE execution_baseline_v8;
DROP TABLE executions_v8;

CREATE TRIGGER executions_intended BEFORE INSERT ON executions
WHEN EXISTS (SELECT 1 FROM executions WHERE id = NEW.id
        OR generation_id = NEW.generation_id OR agent_id = NEW.agent_id
        OR journal_id = NEW.journal_id)
    OR NOT EXISTS (SELECT 1 FROM journal j JOIN agents a ON a.id = j.agent_id
        WHERE j.id = NEW.journal_id AND j.state = 'intended' AND a.id = NEW.agent_id
            AND a.role = 'executor' AND a.generation_id = NEW.generation_id
            AND NEW.started_at <= j.intended_at)
BEGIN SELECT RAISE(ABORT, 'an execution is intended with its journal entry'); END;
CREATE TRIGGER executions_immutable BEFORE UPDATE ON executions
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER executions_no_delete BEFORE DELETE ON executions
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_baseline_precedes_attempt BEFORE INSERT ON execution_baseline
WHEN NOT EXISTS (SELECT 1 FROM executions e JOIN journal j ON j.id = e.journal_id
    WHERE e.id = NEW.execution_id AND j.state = 'intended')
BEGIN SELECT RAISE(ABORT, 'an execution baseline precedes its attempt'); END;
CREATE TRIGGER execution_baseline_immutable BEFORE UPDATE ON execution_baseline
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_baseline_no_delete BEFORE DELETE ON execution_baseline
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_changes_derived BEFORE INSERT ON execution_changes
WHEN NOT EXISTS (SELECT 1 FROM executions e JOIN journal j ON j.id = e.journal_id
        JOIN invocations i ON i.id = j.invocation_id
        WHERE e.id = NEW.execution_id AND j.state = 'attempted'
            AND i.state NOT IN ('starting', 'running')
            AND NEW.authorized = EXISTS
                (SELECT 1 FROM json_each(e.authority) WHERE value = NEW.path))
    OR EXISTS (SELECT 1 FROM execution_captures WHERE execution_id = NEW.execution_id)
    OR NEW.before_kind IS NOT coalesce((SELECT kind FROM execution_baseline
        WHERE execution_id = NEW.execution_id AND path = NEW.path), 'absent')
    OR NEW.before_hash IS NOT (SELECT hash FROM execution_baseline
        WHERE execution_id = NEW.execution_id AND path = NEW.path)
BEGIN SELECT RAISE(ABORT, 'execution changes derive from its baseline and authority'); END;
CREATE TRIGGER execution_changes_immutable BEFORE UPDATE ON execution_changes
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_changes_no_delete BEFORE DELETE ON execution_changes
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_captures_derived BEFORE INSERT ON execution_captures
BEGIN
    SELECT RAISE(ABORT, 'only an attempted execution whose invocation ended is captured')
    WHERE NOT EXISTS (SELECT 1 FROM executions e JOIN journal j ON j.id = e.journal_id
        JOIN invocations i ON i.id = j.invocation_id
        WHERE e.id = NEW.execution_id AND j.state = 'attempted'
            AND i.state NOT IN ('starting', 'running'));
    SELECT RAISE(ABORT, 'the outcome contradicts how the invocation ended')
    FROM executions e JOIN journal j ON j.id = e.journal_id
        JOIN invocations i ON i.id = j.invocation_id
    WHERE e.id = NEW.execution_id AND ((i.state <> 'succeeded' AND (NEW.reported IS NOT NULL
            OR NEW.outcome IN ('candidate', 'reported_failed', 'malformed_result')))
        OR (i.state = 'succeeded' AND NEW.outcome = 'invocation_failed'));
    SELECT RAISE(ABORT, 'the changes cannot be attributed to the executor')
    FROM executions e JOIN journal j ON j.id = e.journal_id
        JOIN invocations i ON i.id = j.invocation_id
    WHERE e.id = NEW.execution_id AND NEW.outcome <> 'unattributable'
        AND (NEW.head_after <> e.head
            OR EXISTS (SELECT 1 FROM execution_changes c WHERE c.execution_id = e.id))
        AND (i.failure IN ('executable_missing', 'spawn_failed')
            OR EXISTS (SELECT 1 FROM execution_changes c JOIN ownership o ON o.path = c.path
                WHERE c.execution_id = e.id AND NOT c.authorized
                    AND o.generation_id <> e.generation_id)
            OR EXISTS (SELECT 1 FROM journal o WHERE o.id <> e.journal_id
                AND o.attempted_at IS NOT NULL
                AND coalesce(o.reconciled_at >= e.started_at, 1)));
    SELECT RAISE(ABORT, 'the outcome contradicts the changes beyond authority')
    FROM executions e
    WHERE e.id = NEW.execution_id AND NEW.outcome <> 'unattributable'
        AND (NEW.outcome = 'scope_violated') <> (NEW.head_after <> e.head
            OR EXISTS (SELECT 1 FROM execution_changes c
                WHERE c.execution_id = e.id AND NOT c.authorized));
END;
CREATE TRIGGER execution_captures_immutable BEFORE UPDATE ON execution_captures
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_captures_no_delete BEFORE DELETE ON execution_captures
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER journal_reconciles_execution BEFORE UPDATE ON journal
WHEN NEW.state = 'reconciled' AND EXISTS (SELECT 1 FROM executions WHERE journal_id = NEW.id)
    AND NOT EXISTS (SELECT 1 FROM executions e
        JOIN execution_captures c ON c.execution_id = e.id
        WHERE e.journal_id = NEW.id AND NEW.outcome IS CASE c.outcome
            WHEN 'candidate' THEN 'completed_as_intended'
            WHEN 'scope_violated' THEN 'completed_with_deviation'
            WHEN 'unattributable' THEN 'completed_with_deviation'
            ELSE 'failed' END)
BEGIN SELECT RAISE(ABORT, 'an execution is reconciled only by its capture'); END;
