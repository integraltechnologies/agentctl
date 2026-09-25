-- Schema version 4 -> 5: journal entries become structured. Version 4
-- recorded an intent as prose, and allowed reconciling an entry that was
-- never attempted with an unevidenced prose outcome. That prose is kept as
-- the `description` of a `legacy.v4` action. Migration may forget what
-- version 4 recorded but never claims more than it established, and it
-- established no outcome, since outcomes were unevidenced prose:
--   'intended'  was never attempted: it stays intended.
--   'failed'    was reachable straight from 'intended' for an action that
--               never started, so it does not establish an attempt: it
--               becomes intended.
--   'attempted' was recorded as attempted, and 'completed' and 'deviated'
--               say it was acted on: each becomes attempted with its
--               outcome unknown, as of when it was last updated, by which
--               time it had been attempted.
-- Nothing references the journal. The definitions must match schema.sql.
DROP INDEX journal_by_agent;
ALTER TABLE journal RENAME TO journal_v4;

-- The engineering-control actions of a logical agent, journaled INTEND ->
-- ACT -> RECONCILE so that a replacement can continue from this state
-- alone. An entry is 'intended' until agentctl records, before acting, that
-- it is being attempted (by `invocation_id`, when one is involved). From
-- then on it may have been acted on, and its outcome is unknown until
-- reconciliation records the outcome and the evidence establishing it.
-- `parameters` and `evidence` are JSON written only by `Store`, which
-- bounds them. Only an intended entry's action may be revised; everything
-- recorded after that is final, which the trigger enforces beneath `Store`.
CREATE TABLE journal (
    id            INTEGER PRIMARY KEY,
    agent_id      INTEGER NOT NULL REFERENCES agents (id),
    action        TEXT    NOT NULL CHECK (length(action) <= 64
        AND action GLOB '[a-z]*' AND action NOT GLOB '*[^a-z0-9_.]*'),
    parameters    TEXT    NOT NULL
        CHECK (json_valid(parameters) AND json_type(parameters) = 'object'),
    state         TEXT    NOT NULL CHECK (state IN ('intended', 'attempted', 'reconciled')),
    invocation_id INTEGER REFERENCES invocations (id),
    outcome       TEXT    CHECK (outcome IN
        ('completed_as_intended', 'completed_with_deviation', 'failed')),
    evidence      TEXT    CHECK (json_valid(evidence) AND json_type(evidence) = 'array'
        AND json_array_length(evidence) > 0),
    intended_at   INTEGER NOT NULL,
    attempted_at  INTEGER,
    reconciled_at INTEGER,
    CHECK ((state = 'intended') = (attempted_at IS NULL)),
    CHECK (attempted_at IS NOT NULL OR invocation_id IS NULL),
    CHECK ((state = 'reconciled') = (reconciled_at IS NOT NULL)),
    CHECK ((state = 'reconciled') = (outcome IS NOT NULL)),
    CHECK ((outcome IS NULL) = (evidence IS NULL))
) STRICT;

CREATE INDEX journal_by_agent ON journal (agent_id);

CREATE TRIGGER journal_forward_only BEFORE UPDATE ON journal
WHEN NEW.id IS NOT OLD.id OR NEW.agent_id IS NOT OLD.agent_id
    OR NEW.intended_at IS NOT OLD.intended_at OR OLD.state = 'reconciled'
    OR (OLD.state = 'intended' AND NEW.state = 'reconciled')
    OR (OLD.state = 'attempted' AND (NEW.state <> 'reconciled'
        OR NEW.action IS NOT OLD.action OR NEW.parameters IS NOT OLD.parameters
        OR NEW.invocation_id IS NOT OLD.invocation_id
        OR NEW.attempted_at IS NOT OLD.attempted_at))
BEGIN SELECT RAISE(ABORT, 'journal history is immutable'); END;
CREATE TRIGGER journal_no_delete BEFORE DELETE ON journal
BEGIN SELECT RAISE(ABORT, 'journal history is immutable'); END;

INSERT INTO journal (id, agent_id, action, parameters, state, intended_at, attempted_at)
SELECT id, agent_id, 'legacy.v4', json_object('description', intent),
       iif(attempted, 'attempted', 'intended'), created_at, iif(attempted, updated_at, NULL)
FROM (SELECT *, state IN ('attempted', 'completed', 'deviated') AS attempted FROM journal_v4);

DROP TABLE journal_v4;
