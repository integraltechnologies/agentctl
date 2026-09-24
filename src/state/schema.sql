-- agentctl canonical project state, schema version 2.
--
-- The schema enforces structure: references, value domains and uniqueness.
-- Lifecycle transitions are enforced by `Store`, the only writer.
-- Timestamps are Unix epoch milliseconds.

CREATE TABLE plans (
    id         INTEGER PRIMARY KEY,
    intent     TEXT    NOT NULL CHECK (intent <> ''),
    state      TEXT    NOT NULL CHECK (state IN
        ('planning', 'ready', 'running', 'paused', 'needs_attention', 'completed')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- A task's lifecycle is derived from its generations (see `Store::task`).
CREATE TABLE tasks (
    id          INTEGER PRIMARY KEY,
    plan_id     INTEGER NOT NULL REFERENCES plans (id),
    description TEXT    NOT NULL CHECK (description <> ''),
    created_at  INTEGER NOT NULL,
    UNIQUE (id, plan_id)
) STRICT;

-- Dependencies stay within one plan. Task ids carry no ordering: an edge may
-- point to any task, and `Store` refuses edges that would close a cycle.
CREATE TABLE task_dependencies (
    plan_id    INTEGER NOT NULL,
    task_id    INTEGER NOT NULL,
    depends_on INTEGER NOT NULL,
    PRIMARY KEY (task_id, depends_on),
    FOREIGN KEY (task_id, plan_id) REFERENCES tasks (id, plan_id),
    FOREIGN KEY (depends_on, plan_id) REFERENCES tasks (id, plan_id),
    CHECK (depends_on <> task_id)
) STRICT, WITHOUT ROWID;

-- One execution attempt of a task. Ended generations are never reused.
CREATE TABLE generations (
    id         INTEGER PRIMARY KEY,
    task_id    INTEGER NOT NULL REFERENCES tasks (id),
    number     INTEGER NOT NULL CHECK (number > 0),
    state      TEXT    NOT NULL CHECK (state IN ('active', 'accepted', 'rejected', 'failed')),
    started_at INTEGER NOT NULL,
    ended_at   INTEGER,
    CHECK ((state = 'active') = (ended_at IS NULL)),
    UNIQUE (task_id, number)
) STRICT;

-- A task is either running one generation or accepted once, never both.
CREATE UNIQUE INDEX generations_live ON generations (task_id)
    WHERE state IN ('active', 'accepted');

-- Logical agents. Planners serve a plan; executors and verifiers serve one
-- generation.
CREATE TABLE agents (
    id            INTEGER PRIMARY KEY,
    role          TEXT    NOT NULL CHECK (role IN ('planner', 'executor', 'verifier')),
    plan_id       INTEGER REFERENCES plans (id),
    generation_id INTEGER REFERENCES generations (id),
    created_at    INTEGER NOT NULL,
    CHECK ((role = 'planner') = (plan_id IS NOT NULL)),
    CHECK ((plan_id IS NULL) <> (generation_id IS NULL))
) STRICT;

CREATE UNIQUE INDEX agents_one_executor ON agents (generation_id)
    WHERE role = 'executor';

-- Physical provider attempts made on behalf of a logical agent. Provider
-- sessions are deliberately absent: continuity lives in the journal.
CREATE TABLE invocations (
    id         INTEGER PRIMARY KEY,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    provider   TEXT    NOT NULL CHECK (provider <> ''),
    model      TEXT    NOT NULL CHECK (model <> ''),
    started_at INTEGER NOT NULL,
    ended_at   INTEGER
) STRICT;

-- A logical agent is embodied by at most one live invocation.
CREATE UNIQUE INDEX invocations_live ON invocations (agent_id)
    WHERE ended_at IS NULL;

-- INTEND -> ACT -> RECONCILE continuation records of a logical agent.
CREATE TABLE journal (
    id         INTEGER PRIMARY KEY,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    intent     TEXT    NOT NULL CHECK (intent <> ''),
    state      TEXT    NOT NULL CHECK (state IN
        ('intended', 'attempted', 'completed', 'deviated', 'failed')),
    outcome    TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Only a reconciled entry has an outcome, and a deviation or failure
    -- must say what actually happened.
    CHECK (state IN ('completed', 'deviated', 'failed') OR outcome IS NULL),
    CHECK (state NOT IN ('deviated', 'failed') OR coalesce(outcome, '') <> '')
) STRICT;

CREATE INDEX journal_by_agent ON journal (agent_id);

CREATE TABLE decisions (
    id         INTEGER PRIMARY KEY,
    plan_id    INTEGER NOT NULL REFERENCES plans (id),
    concern    TEXT    NOT NULL CHECK (concern <> ''),
    decision   TEXT    NOT NULL CHECK (decision <> ''),
    decided_at INTEGER NOT NULL
) STRICT;

-- Runtime history. Writers are serialized, so `seq` order is commit order
-- and a reader resuming after its last seen `seq` never misses an event.
CREATE TABLE events (
    seq      INTEGER PRIMARY KEY,
    at       INTEGER NOT NULL,
    kind     TEXT    NOT NULL,
    plan_id  INTEGER REFERENCES plans (id),
    task_id  INTEGER REFERENCES tasks (id),
    agent_id INTEGER REFERENCES agents (id),
    detail   TEXT    NOT NULL
) STRICT;

CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER decisions_no_update BEFORE UPDATE ON decisions
BEGIN SELECT RAISE(ABORT, 'human decisions are immutable'); END;
CREATE TRIGGER decisions_no_delete BEFORE DELETE ON decisions
BEGIN SELECT RAISE(ABORT, 'human decisions are immutable'); END;

-- Paths owned for mutation by the generation that claimed them. Ownership
-- outlives the generation's end and is only released explicitly.
CREATE TABLE ownership (
    path          TEXT    PRIMARY KEY,
    generation_id INTEGER NOT NULL REFERENCES generations (id)
) STRICT, WITHOUT ROWID;

-- The accepted state of each tracked project path: the content hash of its
-- accepted bytes, or NULL (which the CHECK passes) when the path is accepted
-- as not existing. A hash is lowercase hex SHA-256, the name of the recovery
-- object `Store`'s callers publish before recording it. The
-- generation whose acceptance established it, or NULL for the baseline
-- accepted state that predates every generation. Untracked paths have no row.
CREATE TABLE accepted_sources (
    path          TEXT    PRIMARY KEY,
    hash          TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    generation_id INTEGER REFERENCES generations (id)
) STRICT, WITHOUT ROWID;
