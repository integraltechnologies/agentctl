-- agentctl canonical project state, schema version 3.
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

-- The CodeGraph contribution of one tracked source: the facts a language
-- frontend derived from its accepted content `hash`. It is current only
-- while `hash` is still the path's accepted hash, and is replaced or removed
-- only as a whole, with everything it asserts.
CREATE TABLE graph_sources (
    id       INTEGER PRIMARY KEY,
    path     TEXT    NOT NULL UNIQUE REFERENCES accepted_sources (path),
    hash     TEXT    NOT NULL CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    language TEXT    NOT NULL
        CHECK (language GLOB '[a-z]*' AND language NOT GLOB '*[^a-z0-9_]*')
) STRICT;

-- An entity a contribution defines, identified by its source's path, its
-- kind and a frontend-defined symbol. The span is a half-open byte range of
-- the accepted content.
CREATE TABLE graph_entities (
    source_id  INTEGER NOT NULL REFERENCES graph_sources (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind GLOB '[a-z]*' AND kind NOT GLOB '*[^a-z0-9_]*'),
    symbol     TEXT    NOT NULL CHECK (symbol <> ''),
    span_start INTEGER NOT NULL CHECK (span_start >= 0),
    span_end   INTEGER NOT NULL CHECK (span_end >= span_start),
    PRIMARY KEY (source_id, kind, symbol)
) STRICT, WITHOUT ROWID;

-- A direct relation a contribution asserts. Each end is an entity, named by
-- identity rather than row so that no source's replacement can leave it
-- dangling, or an external symbol (NULL path and kind) with an optional
-- namespace and ecosystem. At least one end is an entity the asserting
-- source defines; an entity of another source was resolved against that
-- source's accepted content `foreign_hash`. Uniqueness of (from, kind, to)
-- within a source is enforced by the CodeGraph writer.
CREATE TABLE graph_relations (
    id             INTEGER PRIMARY KEY,
    source_id      INTEGER NOT NULL REFERENCES graph_sources (id) ON DELETE CASCADE,
    kind           TEXT    NOT NULL CHECK (kind GLOB '[a-z]*' AND kind NOT GLOB '*[^a-z0-9_]*'),
    evidence       TEXT    NOT NULL CHECK (evidence IN ('proven', 'inferred')),
    from_path      TEXT,
    from_kind      TEXT    CHECK (from_kind GLOB '[a-z]*' AND from_kind NOT GLOB '*[^a-z0-9_]*'),
    from_symbol    TEXT    NOT NULL CHECK (from_symbol <> ''),
    from_namespace TEXT    CHECK (from_namespace <> ''),
    from_ecosystem TEXT    CHECK (from_ecosystem <> ''),
    to_path        TEXT,
    to_kind        TEXT    CHECK (to_kind GLOB '[a-z]*' AND to_kind NOT GLOB '*[^a-z0-9_]*'),
    to_symbol      TEXT    NOT NULL CHECK (to_symbol <> ''),
    to_namespace   TEXT    CHECK (to_namespace <> ''),
    to_ecosystem   TEXT    CHECK (to_ecosystem <> ''),
    foreign_hash   TEXT
        CHECK (length(foreign_hash) = 64 AND foreign_hash NOT GLOB '*[^0-9a-f]*'),
    CHECK ((from_path IS NULL) = (from_kind IS NULL)),
    CHECK (from_path IS NULL OR (from_namespace IS NULL AND from_ecosystem IS NULL)),
    CHECK ((to_path IS NULL) = (to_kind IS NULL)),
    CHECK (to_path IS NULL OR (to_namespace IS NULL AND to_ecosystem IS NULL)),
    CHECK (from_path IS NOT NULL OR to_path IS NOT NULL)
) STRICT;

CREATE INDEX graph_relations_by_source ON graph_relations (source_id);
CREATE INDEX graph_relations_from ON graph_relations (from_symbol, from_path, from_kind);
CREATE INDEX graph_relations_to ON graph_relations (to_symbol, to_path, to_kind);

-- Where in the asserting source's accepted content a relation is evidenced,
-- as half-open byte ranges.
CREATE TABLE graph_sites (
    relation_id INTEGER NOT NULL REFERENCES graph_relations (id) ON DELETE CASCADE,
    span_start  INTEGER NOT NULL CHECK (span_start >= 0),
    span_end    INTEGER NOT NULL CHECK (span_end >= span_start),
    PRIMARY KEY (relation_id, span_start, span_end)
) STRICT, WITHOUT ROWID;
