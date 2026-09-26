-- agentctl canonical project state, schema version 1.
--
-- This is the one canonical schema, created whole in a fresh store. Until
-- agentctl is first dogfooded no store is upgraded from any other schema:
-- a change here replaces this schema outright (see `SCHEMA_VERSION`).
--
-- The schema enforces structure: references, value domains and uniqueness.
-- Lifecycle transitions are enforced by `Store`, the only writer.
-- Timestamps are Unix epoch milliseconds.

-- A plan pursues a human's intent: an objective, the constraints and
-- invariants the work must respect, and the criteria by which it is
-- complete, each a JSON array of statements as given. Intent is established
-- with the plan and never changes: planning decides how, never what.
CREATE TABLE plans (
    id                  INTEGER PRIMARY KEY,
    objective           TEXT    NOT NULL CHECK (objective <> ''),
    constraints         TEXT    NOT NULL
        CHECK (json_valid(constraints) AND json_type(constraints) = 'array'),
    completion_criteria TEXT    NOT NULL
        CHECK (json_valid(completion_criteria) AND json_type(completion_criteria) = 'array'),
    state               TEXT    NOT NULL CHECK (state IN
        ('planning', 'ready', 'running', 'paused', 'needs_attention', 'completed')),
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
) STRICT;

CREATE TRIGGER plans_intent_immutable BEFORE UPDATE ON plans
WHEN NEW.id IS NOT OLD.id OR NEW.objective IS NOT OLD.objective
    OR NEW.constraints IS NOT OLD.constraints
    OR NEW.completion_criteria IS NOT OLD.completion_criteria
BEGIN SELECT RAISE(ABORT, 'human intent is immutable'); END;

-- Planned work, named within its plan by a planner-chosen `key` so that
-- planning never depends on storage rows. `context` is what a worker is
-- told beyond the objective. A task's lifecycle is derived from its
-- generations (see `Store::task`).
CREATE TABLE tasks (
    id         INTEGER PRIMARY KEY,
    plan_id    INTEGER NOT NULL REFERENCES plans (id),
    key        TEXT    NOT NULL CHECK (length(key) <= 64
        AND key GLOB '[a-z]*' AND key NOT GLOB '*[^a-z0-9_-]*'),
    objective  TEXT    NOT NULL CHECK (objective <> ''),
    context    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (id, plan_id),
    UNIQUE (plan_id, key)
) STRICT;

-- A task is known by its plan and key, as created: its revisions,
-- authorizations and executors all name that identity, and only its
-- definition (see `task_definitions`) is ever revised.
CREATE TRIGGER tasks_identity_immutable BEFORE UPDATE ON tasks
WHEN NEW.id IS NOT OLD.id OR NEW.plan_id IS NOT OLD.plan_id
    OR NEW.key IS NOT OLD.key OR NEW.created_at IS NOT OLD.created_at
BEGIN SELECT RAISE(ABORT, 'a task''s identity is immutable'); END;

-- Nor is it replaced. REPLACE conflict resolution (INSERT OR REPLACE,
-- REPLACE INTO) deletes a row whose id or plan and key a new row takes, and
-- fires no UPDATE or DELETE trigger doing so; this trigger runs before any
-- conflict is resolved, so a row inserted must be wholly new. An id SQLite
-- is yet to assign reads -1 here, never an id it assigned. A task actually
-- deleted (a draft's, see `Command::RemoveTask`) no longer conflicts.
CREATE TRIGGER tasks_identity_not_replaced BEFORE INSERT ON tasks
WHEN EXISTS (SELECT 1 FROM tasks WHERE id = NEW.id
        OR (plan_id = NEW.plan_id AND key = NEW.key))
BEGIN SELECT RAISE(ABORT, 'a task''s identity is immutable'); END;

-- The exact project paths a task requests to mutate: literal names, never
-- patterns.
CREATE TABLE task_scope (
    task_id INTEGER NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    path    TEXT    NOT NULL CHECK (path <> ''),
    PRIMARY KEY (task_id, path)
) STRICT, WITHOUT ROWID;

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

-- Physical provider attempts made on behalf of a logical agent. An
-- invocation is 'starting' until its process is known to have launched and
-- 'running' until agentctl establishes how it ended. 'interrupted' records
-- that agentctl lost authoritative knowledge of how it ended. A live row
-- only says that no end was recorded, never that a process is still alive.
-- A provider's own session is noncanonical metadata: continuity lives in the
-- journal, never in provider sessions.
--
-- `usage` is the provenance of the token counts. Input counts every input
-- token the provider processed, cached or not; cached input, cache writes
-- and reasoning are reported subsets where a provider distinguishes them.
CREATE TABLE invocations (
    id                  INTEGER PRIMARY KEY,
    agent_id            INTEGER NOT NULL REFERENCES agents (id),
    provider            TEXT    NOT NULL CHECK (provider <> ''),
    model               TEXT    NOT NULL CHECK (model <> ''),
    effort              TEXT    CHECK (effort <> ''),
    state               TEXT    NOT NULL CHECK (state IN
        ('starting', 'running', 'succeeded', 'failed', 'cancelled', 'interrupted')),
    failure             TEXT    CHECK (failure IN
        ('executable_missing', 'spawn_failed', 'input_failed', 'provider_error',
         'malformed_output', 'no_result', 'exit_status')),
    diagnostic          TEXT    CHECK (diagnostic <> ''),
    exit_code           INTEGER,
    provider_session    TEXT    CHECK (provider_session <> ''),
    usage               TEXT    CHECK (usage IN ('provider_reported', 'local_estimate', 'unavailable')),
    input_tokens        INTEGER CHECK (input_tokens >= 0),
    cached_input_tokens INTEGER CHECK (cached_input_tokens >= 0),
    cache_write_tokens  INTEGER CHECK (cache_write_tokens >= 0),
    output_tokens       INTEGER CHECK (output_tokens >= 0),
    reasoning_tokens    INTEGER CHECK (reasoning_tokens >= 0),
    started_at          INTEGER NOT NULL,
    ended_at            INTEGER,
    CHECK ((state IN ('starting', 'running')) = (ended_at IS NULL)),
    CHECK ((state = 'failed') = (failure IS NOT NULL)),
    CHECK (state <> 'succeeded' OR exit_code = 0),
    CHECK (state NOT IN ('failed', 'interrupted') OR diagnostic IS NOT NULL),
    CHECK (ended_at IS NOT NULL OR
        (diagnostic IS NULL AND exit_code IS NULL AND provider_session IS NULL)),
    CHECK ((ended_at IS NULL) = (usage IS NULL)),
    CHECK ((coalesce(usage, 'unavailable') <> 'unavailable') = (input_tokens IS NOT NULL)),
    CHECK ((input_tokens IS NULL) = (output_tokens IS NULL)),
    CHECK (input_tokens IS NOT NULL OR (cached_input_tokens IS NULL
        AND cache_write_tokens IS NULL AND reasoning_tokens IS NULL))
) STRICT;

-- A logical agent is embodied by at most one live invocation.
CREATE UNIQUE INDEX invocations_live ON invocations (agent_id)
    WHERE ended_at IS NULL;

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

-- Paths owned for mutation by the generation that acquired them: exclusive
-- authority to mutate that exact literal path, never any other path it
-- contains or matches. Ownership is not task scope: scope only authorizes a
-- generation of the task to acquire. Ownership outlives the generation's end,
-- and any change of the scope that authorized it, until released explicitly.
CREATE TABLE ownership (
    path          TEXT    PRIMARY KEY,
    generation_id INTEGER NOT NULL REFERENCES generations (id)
) STRICT, WITHOUT ROWID;

CREATE INDEX ownership_by_generation ON ownership (generation_id);

-- Only an active generation acquires, only paths its task's scope requests
-- once its plan is no longer being planned, and never a path another
-- generation owns: not even by replacing its row.
CREATE TRIGGER ownership_acquired BEFORE INSERT ON ownership
WHEN EXISTS (SELECT 1 FROM ownership WHERE path = NEW.path)
    OR NOT EXISTS (SELECT 1 FROM generations g
        JOIN tasks t ON t.id = g.task_id
        JOIN plans p ON p.id = t.plan_id
        JOIN task_scope s ON s.task_id = t.id
        WHERE g.id = NEW.generation_id AND g.state = 'active'
            AND p.state <> 'planning' AND s.path = NEW.path)
BEGIN SELECT RAISE(ABORT, 'ownership is not acquirable'); END;
CREATE TRIGGER ownership_not_transferred BEFORE UPDATE ON ownership
BEGIN SELECT RAISE(ABORT, 'ownership is never transferred'); END;

-- The one executor attempt of a generation, from its INTEND on, which never
-- changes. `authority` is the JSON array of literal paths the executor was
-- authorized to mutate, every one owned by the generation when intended.
-- `head` identifies the project's Git HEAD beforehand, and `started_at` is
-- when agentctl began observing the repository baseline, before the intent.
-- Its journal entry says how far it got: intended and never attempted,
-- attempted with its outcome unknown, or reconciled together with its
-- capture.
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

-- Only the generation's executor intends it, with its journal entry still
-- intended, and never in place of another execution.
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

-- The repository as agentctl observed it before an execution was intended:
-- the entry at each path Git considers repository content, outside
-- agentctl's and Git's own state. A tracked path may be absent; a regular
-- file or symlink is identified by the SHA-256 of its bytes or target. The
-- executor's workspace is copied from it, and found to hold exactly it.
-- Recorded before the execution is attempted, and never changed after.
CREATE TABLE execution_baseline (
    execution_id INTEGER NOT NULL REFERENCES executions (id),
    path         TEXT    NOT NULL CHECK (path <> ''),
    kind         TEXT    NOT NULL CHECK (kind IN ('absent', 'file', 'symlink', 'other')),
    hash         TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    CHECK ((kind IN ('file', 'symlink')) = (hash IS NOT NULL)),
    PRIMARY KEY (execution_id, path)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER execution_baseline_precedes_attempt BEFORE INSERT ON execution_baseline
WHEN NOT EXISTS (SELECT 1 FROM executions e JOIN journal j ON j.id = e.journal_id
    WHERE e.id = NEW.execution_id AND j.state = 'intended')
BEGIN SELECT RAISE(ABORT, 'an execution baseline precedes its attempt'); END;
CREATE TRIGGER execution_baseline_immutable BEFORE UPDATE ON execution_baseline
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_baseline_no_delete BEFORE DELETE ON execution_baseline
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;

-- Each path whose entry an execution's capture found different from its
-- baseline (absent when it had none), and whether the execution was
-- authorized to mutate it. Recorded once the execution's invocation ended
-- and before its capture, as the baseline and authority say, and never
-- changed after.
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

-- What capturing an attempted execution established once its invocation
-- ended: the outcome, derived from the invocation, the changes recorded in
-- the executor's workspace and the attempt's authority, and never changed
-- after. It is recorded in the transaction that reconciles the execution's
-- journal entry, and counts only once that entry is reconciled.
--
-- A 'candidate' is only structurally valid: its invocation succeeded, its
-- well-formed result reported success, and every change observed in its
-- workspace, a disposable copy of the repository outside the project that
-- only the executor is given, was authorized. That is not proof that the
-- executor process wrote every byte: agentctl observes the workspace, not
-- who writes to it. A candidate is not verified, accepted or complete, and
-- is in the project's working tree only once installed (see
-- `execution_installs`). `reported` and `claimed` are what the executor
-- said, when it said anything well formed: evidence, never proof.
-- `attribution` says why observed changes are not attributed to it;
-- 'contested' and 'concurrent' apply only to executors working in the
-- project's working tree itself, which no executor does now.
-- `head_after` is the project's Git HEAD at capture.
--
-- The triggers keep an outcome consistent with the facts recorded beside
-- it; that those facts match the repository only `Store` establishes.
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

-- An executor works in a disposable copy of the repository outside the
-- project, which is what its capture observes. Any change there is
-- unattributable when no executor process was launched. Otherwise changes
-- beyond authority violate scope, and only then does the result decide.
-- Git's HEAD is the project's, which the copy does not hold: evidence only.
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
        AND i.failure IN ('executable_missing', 'spawn_failed')
        AND EXISTS (SELECT 1 FROM execution_changes c WHERE c.execution_id = e.id);
    SELECT RAISE(ABORT, 'the outcome contradicts the changes beyond authority')
    FROM executions e
    WHERE e.id = NEW.execution_id AND NEW.outcome <> 'unattributable'
        AND (NEW.outcome = 'scope_violated') <> EXISTS (SELECT 1 FROM execution_changes c
            WHERE c.execution_id = e.id AND NOT c.authorized);
END;
CREATE TRIGGER execution_captures_immutable BEFORE UPDATE ON execution_captures
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_captures_no_delete BEFORE DELETE ON execution_captures
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;

-- An executor's journal entry is reconciled only together with its
-- execution's capture, as the captured outcome implies: until then its
-- outcome stays unknown, however its invocation ended.
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

-- Installing a candidate: copying the changes its executor made in its
-- disposable workspace into the project's working tree, where they stay
-- provisional, neither verified nor accepted, and the generation keeps its
-- ownership. Intended by the execution's executor agent, with a journal
-- entry of its own, once the candidate is captured and while the
-- generation is active; that entry is attempted before anything in the
-- working tree is written. Never changed after.
CREATE TABLE execution_installs (
    execution_id INTEGER PRIMARY KEY REFERENCES executions (id),
    journal_id   INTEGER NOT NULL UNIQUE REFERENCES journal (id)
) STRICT;

CREATE TRIGGER execution_installs_intended BEFORE INSERT ON execution_installs
WHEN EXISTS (SELECT 1 FROM execution_installs WHERE execution_id = NEW.execution_id
        OR journal_id = NEW.journal_id)
    OR NOT EXISTS (SELECT 1 FROM executions e JOIN journal r ON r.id = e.journal_id
        JOIN execution_captures c ON c.execution_id = e.id
        JOIN generations g ON g.id = e.generation_id
        JOIN journal j ON j.id = NEW.journal_id
        WHERE e.id = NEW.execution_id AND r.state = 'reconciled' AND c.outcome = 'candidate'
            AND g.state = 'active' AND j.state = 'intended' AND j.agent_id = e.agent_id)
BEGIN SELECT RAISE(ABORT, 'only a captured candidate is installed, with its journal entry'); END;
CREATE TRIGGER execution_installs_immutable BEFORE UPDATE ON execution_installs
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_installs_no_delete BEFORE DELETE ON execution_installs
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;

-- How installing a candidate ended, recorded in the transaction that
-- reconciles its journal entry, and never changed after. 'installed' once
-- every change was written. Otherwise nothing of the candidate stays
-- written: 'drifted' when the working tree no longer held what the
-- baseline observed at the paths listed, so writing them could overwrite
-- another writer's work; 'refused' when the candidate held a change
-- agentctl does not install, or the workspace no longer held what was
-- captured; 'failed' when writing failed and every path written was
-- restored. Should restoring fail, or agentctl stop, nothing is recorded:
-- the entry stays attempted, and what those paths hold is unknown.
CREATE TABLE execution_install_results (
    execution_id INTEGER PRIMARY KEY REFERENCES execution_installs (execution_id),
    outcome      TEXT    NOT NULL
        CHECK (outcome IN ('installed', 'drifted', 'refused', 'failed')),
    drifted      TEXT    CHECK (json_valid(drifted) AND json_type(drifted) = 'array'
        AND json_array_length(drifted) > 0),
    finished_at  INTEGER NOT NULL,
    CHECK ((outcome = 'drifted') = (drifted IS NOT NULL))
) STRICT;

CREATE TRIGGER execution_install_results_derived BEFORE INSERT ON execution_install_results
WHEN NOT EXISTS (SELECT 1 FROM execution_installs i JOIN journal j ON j.id = i.journal_id
        WHERE i.execution_id = NEW.execution_id AND j.state = 'attempted')
    OR EXISTS (SELECT 1 FROM execution_install_results WHERE execution_id = NEW.execution_id)
    OR EXISTS (SELECT 1 FROM json_each(NEW.drifted) d WHERE NOT EXISTS
        (SELECT 1 FROM execution_changes c
            WHERE c.execution_id = NEW.execution_id AND c.path = d.value))
BEGIN SELECT RAISE(ABORT, 'an install result follows its attempt, of changed paths'); END;
CREATE TRIGGER execution_install_results_immutable BEFORE UPDATE ON execution_install_results
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;
CREATE TRIGGER execution_install_results_no_delete BEFORE DELETE ON execution_install_results
BEGIN SELECT RAISE(ABORT, 'execution history is immutable'); END;

-- An install's journal entry is reconciled only together with its result,
-- as that implies: until then what the working tree holds stays unknown.
CREATE TRIGGER journal_reconciles_install BEFORE UPDATE ON journal
WHEN NEW.state = 'reconciled'
    AND EXISTS (SELECT 1 FROM execution_installs WHERE journal_id = NEW.id)
    AND NOT EXISTS (SELECT 1 FROM execution_installs i
        JOIN execution_install_results r ON r.execution_id = i.execution_id
        WHERE i.journal_id = NEW.id AND NEW.outcome IS CASE r.outcome
            WHEN 'installed' THEN 'completed_as_intended' ELSE 'failed' END)
BEGIN SELECT RAISE(ABORT, 'an install is reconciled only by its result'); END;

-- Independent verification of an installed candidate: a fresh verifier
-- agent, embodied by one fresh invocation, judging the exact candidate its
-- execution captured and installed, in a disposable copy of the working
-- tree. A verification is evidence for acceptance, never acceptance: it
-- changes no generation, accepted source, CodeGraph or ownership.
-- Intended with the verifier's journal entry, the agent's first and only
-- action, while the generation is active and owns its task's whole scope.
-- Numbered per candidate: another attempt, by another fresh verifier, only
-- once every earlier one is reconciled without a judgment. Never changed
-- after.
CREATE TABLE verifications (
    id           INTEGER PRIMARY KEY,
    execution_id INTEGER NOT NULL REFERENCES execution_install_results (execution_id),
    number       INTEGER NOT NULL CHECK (number > 0),
    agent_id     INTEGER NOT NULL UNIQUE REFERENCES agents (id),
    journal_id   INTEGER NOT NULL UNIQUE REFERENCES journal (id),
    started_at   INTEGER NOT NULL,
    UNIQUE (execution_id, number)
) STRICT;

-- How a verification ended, recorded in the transaction that reconciles its
-- journal entry, and never changed after. What agentctl observed:
-- `drifted`, the candidate's changed paths the working tree no longer held,
-- before any verifier ran ('candidate_drifted', attempted without an
-- invocation) or once it had ('candidate_changed'); `mutated`, repository
-- source the verifier changed in its workspace ('boundary_violated'). What
-- the verifier claimed, when its result was well formed: `verdict` with
-- `checked`, `blockers` and `non_blocking`. Only a verdict whose candidate
-- held still and whose workspace source stayed untouched is a judgment:
-- 'passed' needs checked evidence and no blocker, 'failed' at least one
-- blocker. 'invocation_failed' and 'malformed_result' judge nothing.
CREATE TABLE verification_results (
    verification_id INTEGER PRIMARY KEY REFERENCES verifications (id),
    outcome         TEXT    NOT NULL CHECK (outcome IN ('passed', 'failed',
        'candidate_drifted', 'candidate_changed', 'boundary_violated', 'invocation_failed',
        'malformed_result')),
    drifted         TEXT    CHECK (json_valid(drifted) AND json_type(drifted) = 'array'
        AND json_array_length(drifted) > 0),
    mutated         TEXT    CHECK (json_valid(mutated) AND json_type(mutated) = 'array'
        AND json_array_length(mutated) > 0),
    verdict         TEXT    CHECK (verdict IN ('pass', 'fail')),
    checked         TEXT    CHECK (json_valid(checked) AND json_type(checked) = 'array'),
    blockers        TEXT    CHECK (json_valid(blockers) AND json_type(blockers) = 'array'),
    non_blocking    TEXT    CHECK (json_valid(non_blocking) AND json_type(non_blocking) = 'array'),
    finished_at     INTEGER NOT NULL,
    CHECK ((verdict IS NULL) = (checked IS NULL) AND (verdict IS NULL) = (blockers IS NULL)
        AND (verdict IS NULL) = (non_blocking IS NULL)),
    CHECK (verdict IS NOT 'pass'
        OR (json_array_length(checked) > 0 AND json_array_length(blockers) = 0)),
    CHECK (verdict IS NOT 'fail' OR json_array_length(blockers) > 0),
    CHECK ((outcome IN ('candidate_drifted', 'candidate_changed')) = (drifted IS NOT NULL)),
    CHECK ((outcome = 'boundary_violated') = (drifted IS NULL AND mutated IS NOT NULL)),
    CHECK ((outcome = 'passed') = (verdict IS 'pass' AND drifted IS NULL AND mutated IS NULL)),
    CHECK ((outcome = 'failed') = (verdict IS 'fail' AND drifted IS NULL AND mutated IS NULL)),
    CHECK (outcome NOT IN ('candidate_drifted', 'invocation_failed', 'malformed_result')
        OR (verdict IS NULL AND mutated IS NULL))
) STRICT;

CREATE TRIGGER verifications_intended BEFORE INSERT ON verifications
WHEN EXISTS (SELECT 1 FROM verifications WHERE id = NEW.id OR agent_id = NEW.agent_id
        OR journal_id = NEW.journal_id)
    OR NEW.number <> 1 + (SELECT count(*) FROM verifications
        WHERE execution_id = NEW.execution_id)
    OR EXISTS (SELECT 1 FROM verifications v JOIN journal j ON j.id = v.journal_id
        LEFT JOIN verification_results r ON r.verification_id = v.id
        WHERE v.execution_id = NEW.execution_id
            AND (j.state <> 'reconciled' OR r.outcome IN ('passed', 'failed')))
    OR NOT EXISTS (SELECT 1 FROM executions e
        JOIN execution_installs i ON i.execution_id = e.id
        JOIN journal ij ON ij.id = i.journal_id
        JOIN execution_install_results r ON r.execution_id = e.id
        JOIN generations g ON g.id = e.generation_id
        JOIN agents a ON a.id = NEW.agent_id
        JOIN journal j ON j.id = NEW.journal_id
        WHERE e.id = NEW.execution_id AND ij.state = 'reconciled' AND r.outcome = 'installed'
            AND g.state = 'active' AND a.role = 'verifier' AND a.generation_id = g.id
            AND j.state = 'intended' AND j.agent_id = a.id
            AND NOT EXISTS (SELECT 1 FROM journal o WHERE o.agent_id = a.id AND o.id <> j.id)
            AND NOT EXISTS (SELECT 1 FROM invocations v WHERE v.agent_id = a.id)
            AND NOT EXISTS (SELECT 1 FROM json_each(e.authority) p
                WHERE NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = p.value AND o.generation_id = g.id))
            AND NOT EXISTS (SELECT 1 FROM task_scope s WHERE s.task_id = g.task_id
                AND NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = s.path AND o.generation_id = g.id)))
BEGIN
    SELECT RAISE(ABORT, 'only an installed candidate of an owning active generation is verified, by a fresh verifier');
END;
CREATE TRIGGER verifications_immutable BEFORE UPDATE ON verifications
BEGIN SELECT RAISE(ABORT, 'verification history is immutable'); END;
CREATE TRIGGER verifications_no_delete BEFORE DELETE ON verifications
BEGIN SELECT RAISE(ABORT, 'verification history is immutable'); END;

-- A result follows its attempt: without an invocation only when the
-- candidate drifted before any verifier ran, otherwise once the verifier's
-- invocation ended, which succeeded exactly when the result holds a verdict
-- or was malformed. A pass needs a check that passed; drifted paths are
-- changed paths of the candidate.
CREATE TRIGGER verification_results_derived BEFORE INSERT ON verification_results
WHEN EXISTS (SELECT 1 FROM verification_results WHERE verification_id = NEW.verification_id)
    OR NOT EXISTS (SELECT 1 FROM verifications v JOIN journal j ON j.id = v.journal_id
        LEFT JOIN invocations i ON i.id = j.invocation_id
        WHERE v.id = NEW.verification_id AND j.state = 'attempted'
            AND CASE WHEN NEW.outcome = 'candidate_drifted' THEN j.invocation_id IS NULL
                ELSE i.agent_id = v.agent_id AND i.state NOT IN ('starting', 'running')
                    AND (NEW.verdict IS NULL OR i.state = 'succeeded')
                    AND (NEW.outcome NOT IN ('passed', 'failed', 'malformed_result')
                        OR i.state = 'succeeded')
                    AND (NEW.outcome <> 'invocation_failed' OR i.state <> 'succeeded') END)
    OR (NEW.verdict = 'pass' AND NOT EXISTS (SELECT 1 FROM json_each(NEW.checked) c
        WHERE c.value ->> 'outcome' = 'passed'))
    OR EXISTS (SELECT 1 FROM json_each(NEW.drifted) d WHERE NOT EXISTS
        (SELECT 1 FROM verifications v JOIN execution_changes c
            ON c.execution_id = v.execution_id
            WHERE v.id = NEW.verification_id AND c.path = d.value))
BEGIN SELECT RAISE(ABORT, 'a verification result follows its attempt, consistently'); END;
CREATE TRIGGER verification_results_immutable BEFORE UPDATE ON verification_results
BEGIN SELECT RAISE(ABORT, 'verification history is immutable'); END;
CREATE TRIGGER verification_results_no_delete BEFORE DELETE ON verification_results
BEGIN SELECT RAISE(ABORT, 'verification history is immutable'); END;

-- A verification's journal entry is reconciled only together with its
-- result, as that implies: a judgment completes as intended, a verdict
-- about something other than the candidate, or amid mutated source, with
-- deviation, and anything else failed.
CREATE TRIGGER journal_reconciles_verification BEFORE UPDATE ON journal
WHEN NEW.state = 'reconciled'
    AND EXISTS (SELECT 1 FROM verifications WHERE journal_id = NEW.id)
    AND NOT EXISTS (SELECT 1 FROM verifications v
        JOIN verification_results r ON r.verification_id = v.id
        WHERE v.journal_id = NEW.id AND NEW.outcome IS CASE
            WHEN r.outcome IN ('passed', 'failed') THEN 'completed_as_intended'
            WHEN r.outcome IN ('candidate_changed', 'boundary_violated')
                THEN 'completed_with_deviation'
            ELSE 'failed' END)
BEGIN SELECT RAISE(ABORT, 'a verification is reconciled only by its result'); END;

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

-- Accepting a verified candidate: the exact bytes an independent
-- verification passed become accepted source, CodeGraph is synchronized
-- with them, and only then is the generation's ownership released and the
-- generation accepted, completing its task. Each phase is recorded in its
-- own transaction, in order, so that whatever an interruption left
-- unfinished is known; a verification's pass alone changes none of it.
--
-- An acceptance binds a generation to its execution and to the one
-- verification whose pass it acts on: the latest of the installed
-- candidate, while the generation is active, owns its task's whole scope
-- and the execution's authority, and its plan is ready or running. It is
-- recorded together with the identities it publishes and the phase
-- 'published', in the transaction that makes them accepted source, so a
-- candidate is never accepted in part. Never changed after.
CREATE TABLE acceptances (
    generation_id   INTEGER PRIMARY KEY REFERENCES generations (id),
    execution_id    INTEGER NOT NULL UNIQUE REFERENCES executions (id),
    verification_id INTEGER NOT NULL UNIQUE REFERENCES verifications (id),
    started_at      INTEGER NOT NULL
) STRICT;

-- The accepted identity an acceptance publishes at each changed path of
-- its candidate, as the execution captured it: the candidate's content
-- hash, or NULL for accepted absence. `prior_known` and `prior_hash` are
-- the accepted identity it replaced, if the path had any. Never changed
-- after.
CREATE TABLE acceptance_sources (
    generation_id INTEGER NOT NULL REFERENCES acceptances (generation_id),
    path          TEXT    NOT NULL CHECK (path <> ''),
    hash          TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    prior_known   INTEGER NOT NULL CHECK (prior_known IN (0, 1)),
    prior_hash    TEXT    CHECK (length(prior_hash) = 64 AND prior_hash NOT GLOB '*[^0-9a-f]*'),
    CHECK (prior_known OR prior_hash IS NULL),
    PRIMARY KEY (generation_id, path)
) STRICT, WITHOUT ROWID;

-- How far an acceptance got, one phase after another: 'published' once
-- every identity it publishes is accepted source, 'synchronized' once no
-- changed path has a graph derived from anything else, and 'completed'
-- once its generation is accepted and owns nothing. Never changed after.
CREATE TABLE acceptance_phases (
    generation_id INTEGER NOT NULL REFERENCES acceptances (generation_id),
    phase         TEXT    NOT NULL
        CHECK (phase IN ('published', 'synchronized', 'completed')),
    at            INTEGER NOT NULL,
    PRIMARY KEY (generation_id, phase)
) STRICT, WITHOUT ROWID;

-- Leave to complete one synchronized acceptance, taken by the transaction
-- that completes it and by nothing else: only while it is held is the
-- generation accepted and its ownership released, and recording the phase
-- 'completed' gives it back. It never outlives that transaction: it
-- references the phase, which is recorded only as it is given back, so a
-- transaction still holding it cannot commit.
CREATE TABLE acceptance_completions (
    generation_id INTEGER PRIMARY KEY,
    phase         TEXT    NOT NULL DEFAULT 'completed' CHECK (phase = 'completed'),
    FOREIGN KEY (generation_id, phase) REFERENCES acceptance_phases (generation_id, phase)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TRIGGER acceptances_intended BEFORE INSERT ON acceptances
WHEN EXISTS (SELECT 1 FROM acceptances WHERE generation_id = NEW.generation_id
        OR execution_id = NEW.execution_id OR verification_id = NEW.verification_id)
    OR NOT EXISTS (SELECT 1 FROM executions e
        JOIN journal ej ON ej.id = e.journal_id
        JOIN execution_captures c ON c.execution_id = e.id
        JOIN execution_installs i ON i.execution_id = e.id
        JOIN journal ij ON ij.id = i.journal_id
        JOIN execution_install_results ir ON ir.execution_id = e.id
        JOIN verifications v ON v.execution_id = e.id
        JOIN journal vj ON vj.id = v.journal_id
        JOIN verification_results vr ON vr.verification_id = v.id
        JOIN generations g ON g.id = e.generation_id
        JOIN tasks t ON t.id = g.task_id
        JOIN plans p ON p.id = t.plan_id
        WHERE e.id = NEW.execution_id AND g.id = NEW.generation_id AND v.id = NEW.verification_id
            AND ej.state = 'reconciled' AND c.outcome = 'candidate'
            AND ij.state = 'reconciled' AND ir.outcome = 'installed'
            AND vj.state = 'reconciled' AND vr.outcome = 'passed'
            AND NOT EXISTS (SELECT 1 FROM verifications l
                WHERE l.execution_id = e.id AND l.number > v.number)
            AND g.state = 'active' AND p.state IN ('ready', 'running')
            AND NOT EXISTS (SELECT 1 FROM execution_changes x WHERE x.execution_id = e.id
                AND (NOT x.authorized OR x.after_kind NOT IN ('file', 'absent')))
            AND NOT EXISTS (SELECT 1 FROM json_each(e.authority) a
                WHERE NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = a.value AND o.generation_id = g.id))
            AND NOT EXISTS (SELECT 1 FROM task_scope s WHERE s.task_id = g.task_id
                AND NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = s.path AND o.generation_id = g.id)))
BEGIN
    SELECT RAISE(ABORT, 'only the passed verification of an installed candidate of an owning active generation is accepted, once');
END;
CREATE TRIGGER acceptances_immutable BEFORE UPDATE ON acceptances
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;
CREATE TRIGGER acceptances_no_delete BEFORE DELETE ON acceptances
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;

-- Exactly the candidate's content at one of its changed paths, and what
-- that path's accepted identity is until then, recorded before anything
-- is published.
CREATE TRIGGER acceptance_sources_derived BEFORE INSERT ON acceptance_sources
WHEN EXISTS (SELECT 1 FROM acceptance_phases WHERE generation_id = NEW.generation_id)
    OR EXISTS (SELECT 1 FROM acceptance_sources
        WHERE generation_id = NEW.generation_id AND path = NEW.path)
    OR NOT EXISTS (SELECT 1 FROM acceptances a JOIN execution_changes x
        ON x.execution_id = a.execution_id AND x.path = NEW.path
        WHERE a.generation_id = NEW.generation_id AND NEW.hash IS x.after_hash)
    OR NEW.prior_known IS NOT EXISTS (SELECT 1 FROM accepted_sources WHERE path = NEW.path)
    OR NEW.prior_hash IS NOT (SELECT hash FROM accepted_sources WHERE path = NEW.path)
BEGIN SELECT RAISE(ABORT, 'an acceptance publishes exactly its candidate'); END;
CREATE TRIGGER acceptance_sources_immutable BEFORE UPDATE ON acceptance_sources
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;
CREATE TRIGGER acceptance_sources_no_delete BEFORE DELETE ON acceptance_sources
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;

-- Phases follow one another, each only once what it says holds.
CREATE TRIGGER acceptance_phases_ordered BEFORE INSERT ON acceptance_phases
BEGIN
    SELECT RAISE(ABORT, 'acceptance phases advance one at a time')
    WHERE NOT EXISTS (SELECT 1 FROM acceptances WHERE generation_id = NEW.generation_id)
        OR (SELECT count(*) FROM acceptance_phases WHERE generation_id = NEW.generation_id)
            IS NOT CASE NEW.phase
                WHEN 'published' THEN 0 WHEN 'synchronized' THEN 1 WHEN 'completed' THEN 2 END;
    SELECT RAISE(ABORT, 'source is published only as the whole candidate')
    WHERE NEW.phase = 'published' AND (
        (SELECT count(*) FROM acceptance_sources WHERE generation_id = NEW.generation_id)
            IS NOT (SELECT count(*) FROM acceptances a JOIN execution_changes x
                ON x.execution_id = a.execution_id WHERE a.generation_id = NEW.generation_id)
        OR EXISTS (SELECT 1 FROM acceptance_sources s
            LEFT JOIN accepted_sources c ON c.path = s.path
            WHERE s.generation_id = NEW.generation_id AND (c.path IS NULL
                OR c.hash IS NOT s.hash OR c.generation_id IS NOT s.generation_id))
        OR NOT EXISTS (SELECT 1 FROM generations
            WHERE id = NEW.generation_id AND state = 'active'));
    SELECT RAISE(ABORT, 'CodeGraph is synchronized only with the published source')
    WHERE NEW.phase = 'synchronized' AND (
        EXISTS (SELECT 1 FROM acceptance_sources s
            LEFT JOIN accepted_sources c ON c.path = s.path
            LEFT JOIN graph_sources g ON g.path = s.path
            WHERE s.generation_id = NEW.generation_id AND (c.hash IS NOT s.hash
                OR c.generation_id IS NOT s.generation_id
                OR (g.id IS NOT NULL AND g.hash IS NOT s.hash)))
        OR NOT EXISTS (SELECT 1 FROM generations
            WHERE id = NEW.generation_id AND state = 'active'));
    SELECT RAISE(ABORT, 'an acceptance completes only once its generation is accepted and owns nothing')
    WHERE NEW.phase = 'completed' AND (
        NOT EXISTS (SELECT 1 FROM acceptance_completions WHERE generation_id = NEW.generation_id)
        OR NOT EXISTS (SELECT 1 FROM generations
            WHERE id = NEW.generation_id AND state = 'accepted')
        OR EXISTS (SELECT 1 FROM ownership WHERE generation_id = NEW.generation_id));
END;
CREATE TRIGGER acceptance_phases_completed AFTER INSERT ON acceptance_phases
WHEN NEW.phase = 'completed'
BEGIN DELETE FROM acceptance_completions WHERE generation_id = NEW.generation_id; END;
CREATE TRIGGER acceptance_phases_immutable BEFORE UPDATE ON acceptance_phases
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;
CREATE TRIGGER acceptance_phases_no_delete BEFORE DELETE ON acceptance_phases
BEGIN SELECT RAISE(ABORT, 'acceptance history is immutable'); END;

-- Leave is taken only while the acceptance is synchronized, its
-- generation active and owning its task's whole scope and the execution's
-- authority, and is given back only by completing it.
CREATE TRIGGER acceptance_completions_taken BEFORE INSERT ON acceptance_completions
WHEN NOT EXISTS (SELECT 1 FROM acceptances a
        JOIN executions e ON e.id = a.execution_id
        JOIN generations g ON g.id = a.generation_id
        JOIN acceptance_phases s ON s.generation_id = a.generation_id
        WHERE a.generation_id = NEW.generation_id
            AND s.phase = 'synchronized' AND g.state = 'active'
            AND NOT EXISTS (SELECT 1 FROM acceptance_phases c
                WHERE c.generation_id = a.generation_id AND c.phase = 'completed')
            AND NOT EXISTS (SELECT 1 FROM json_each(e.authority) x
                WHERE NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = x.value AND o.generation_id = g.id))
            AND NOT EXISTS (SELECT 1 FROM task_scope t WHERE t.task_id = g.task_id
                AND NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = t.path AND o.generation_id = g.id)))
BEGIN SELECT RAISE(ABORT, 'only a synchronized acceptance of a generation owning its whole scope is completed'); END;
CREATE TRIGGER acceptance_completions_held BEFORE UPDATE ON acceptance_completions
BEGIN SELECT RAISE(ABORT, 'leave to complete an acceptance is given back only by completing it'); END;
CREATE TRIGGER acceptance_completions_given_back BEFORE DELETE ON acceptance_completions
WHEN NOT EXISTS (SELECT 1 FROM acceptance_phases
    WHERE generation_id = OLD.generation_id AND phase = 'completed')
BEGIN SELECT RAISE(ABORT, 'leave to complete an acceptance is given back only by completing it'); END;

-- What an unfinished acceptance published stays accepted source until it
-- completes: nothing replaces it meanwhile.
CREATE TRIGGER accepted_sources_held_by_acceptance BEFORE UPDATE ON accepted_sources
WHEN EXISTS (SELECT 1 FROM acceptance_sources s
    WHERE s.path = OLD.path
        AND (NEW.hash IS NOT s.hash OR NEW.generation_id IS NOT s.generation_id)
        AND NOT EXISTS (SELECT 1 FROM acceptance_phases p
            WHERE p.generation_id = s.generation_id AND p.phase = 'completed'))
BEGIN SELECT RAISE(ABORT, 'an unfinished acceptance holds its accepted source'); END;

-- A generation that executed is accepted only by completing its
-- acceptance, and one being accepted ends no other way.
CREATE TRIGGER generations_accepted_by_acceptance BEFORE UPDATE ON generations
WHEN NEW.state IS NOT OLD.state AND CASE WHEN NEW.state = 'accepted'
    THEN (EXISTS (SELECT 1 FROM executions WHERE generation_id = OLD.id)
            OR EXISTS (SELECT 1 FROM acceptances WHERE generation_id = OLD.id))
        AND NOT EXISTS (SELECT 1 FROM acceptance_completions WHERE generation_id = OLD.id)
    ELSE EXISTS (SELECT 1 FROM acceptances WHERE generation_id = OLD.id) END
BEGIN SELECT RAISE(ABORT, 'a generation that executed is accepted only by its acceptance'); END;

-- A generation being accepted keeps its ownership until completing its
-- acceptance has accepted it.
CREATE TRIGGER ownership_held_through_acceptance BEFORE DELETE ON ownership
WHEN EXISTS (SELECT 1 FROM acceptances WHERE generation_id = OLD.generation_id)
    AND NOT EXISTS (SELECT 1 FROM acceptance_completions c
        JOIN generations g ON g.id = c.generation_id
        WHERE c.generation_id = OLD.generation_id AND g.state = 'accepted')
BEGIN SELECT RAISE(ABORT, 'ownership is held until its acceptance is completed'); END;

-- A task is completed exactly when one of its generations completed a
-- verified candidate's acceptance: accepted, with its acceptance's phase
-- 'completed'. Only that satisfies a dependency on the task. An executor's
-- candidate, its install, a verification's pass, an acceptance that only
-- published or synchronized, or a generation accepted without executing,
-- satisfies nothing.
CREATE VIEW completed_tasks (task_id, generation_id) AS
SELECT g.task_id, g.id FROM generations g
JOIN acceptance_phases p ON p.generation_id = g.id AND p.phase = 'completed'
WHERE g.state = 'accepted';

-- A scheduler's claim on a task: the generation it started for it, which
-- owns the task's whole scope, and one unit of the project's scheduling
-- capacity, held until the claim is released. Recorded in the transaction
-- that starts the generation, binds it to its task's current definition
-- (`generation_revisions`) and acquires its ownership, and only for a
-- task of a running plan that is not cancelled, whose every dependency is
-- completed and that no generation ever served, unless a planner's retry
-- authorization, after its latest, abandoned generation, of the very
-- revision this one is bound to, is used by this one
-- (`retry_authorizations`), while fewer than `capacity` claims are held:
-- the concurrency ceiling the scheduler worked under. Never changed after.
CREATE TABLE scheduler_claims (
    generation_id INTEGER PRIMARY KEY REFERENCES generations (id),
    task_id       INTEGER NOT NULL REFERENCES tasks (id),
    capacity      INTEGER NOT NULL CHECK (capacity > 0),
    claimed_at    INTEGER NOT NULL
) STRICT;

-- Releasing a claim's capacity, once its pipeline has no work that may
-- still be live and how it ended is established, as `scheduler_outcomes`
-- derives it. Releasing says nothing of acceptance: only 'accepted' is a
-- completed task. Never changed after.
CREATE TABLE scheduler_releases (
    generation_id INTEGER PRIMARY KEY REFERENCES scheduler_claims (generation_id),
    outcome       TEXT    NOT NULL CHECK (outcome IN ('accepted', 'not_executed',
        'execution_failed', 'install_failed', 'verification_failed',
        'verification_inconclusive', 'acceptance_declined')),
    released_at   INTEGER NOT NULL
) STRICT;

-- How each claimed generation's pipeline ended, or NULL while it may not
-- be released: while an acceptance of it is unfinished, any action of its
-- agents is attempted with its outcome unknown, or any of their
-- invocations has no recorded end.
CREATE VIEW scheduler_outcomes (generation_id, outcome) AS
SELECT c.generation_id, CASE
    WHEN EXISTS (SELECT 1 FROM acceptance_phases p
        WHERE p.generation_id = c.generation_id AND p.phase = 'completed') THEN 'accepted'
    WHEN EXISTS (SELECT 1 FROM acceptances a WHERE a.generation_id = c.generation_id)
        OR EXISTS (SELECT 1 FROM agents a JOIN journal j ON j.agent_id = a.id
            WHERE a.generation_id = c.generation_id AND j.state = 'attempted')
        OR EXISTS (SELECT 1 FROM agents a JOIN invocations i ON i.agent_id = a.id
            WHERE a.generation_id = c.generation_id AND i.ended_at IS NULL) THEN NULL
    WHEN NOT EXISTS (SELECT 1 FROM executions e JOIN journal j ON j.id = e.journal_id
        WHERE e.generation_id = c.generation_id AND j.state = 'reconciled') THEN 'not_executed'
    WHEN NOT EXISTS (SELECT 1 FROM executions e
        JOIN execution_captures x ON x.execution_id = e.id
        WHERE e.generation_id = c.generation_id AND x.outcome = 'candidate')
        THEN 'execution_failed'
    WHEN NOT EXISTS (SELECT 1 FROM executions e
        JOIN execution_install_results r ON r.execution_id = e.id
        WHERE e.generation_id = c.generation_id AND r.outcome = 'installed')
        THEN 'install_failed'
    ELSE coalesce((SELECT CASE r.outcome WHEN 'passed' THEN 'acceptance_declined'
            WHEN 'failed' THEN 'verification_failed' ELSE 'verification_inconclusive' END
        FROM executions e JOIN verifications v ON v.execution_id = e.id
        LEFT JOIN verification_results r ON r.verification_id = v.id
        WHERE e.generation_id = c.generation_id ORDER BY v.number DESC LIMIT 1),
        'verification_inconclusive')
    END
FROM scheduler_claims c;

CREATE TRIGGER scheduler_claims_taken BEFORE INSERT ON scheduler_claims
WHEN EXISTS (SELECT 1 FROM scheduler_claims WHERE generation_id = NEW.generation_id)
    OR NOT EXISTS (SELECT 1 FROM generations g
        JOIN tasks t ON t.id = g.task_id
        JOIN plans p ON p.id = t.plan_id
        WHERE g.id = NEW.generation_id AND t.id = NEW.task_id
            AND g.state = 'active' AND p.state = 'running'
            AND NOT EXISTS (SELECT 1 FROM task_cancellations x WHERE x.task_id = t.id)
            AND EXISTS (SELECT 1 FROM generation_revisions v WHERE v.generation_id = g.id)
            AND (NOT EXISTS (SELECT 1 FROM generations o
                    WHERE o.task_id = t.id AND o.id <> g.id)
                OR EXISTS (SELECT 1 FROM retry_authorizations r
                    JOIN generation_revisions v ON v.generation_id = r.generation_id
                    WHERE r.task_id = t.id AND r.generation_id = g.id
                        AND v.revision = r.revision))
            AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.generation_id = g.id)
            AND NOT EXISTS (SELECT 1 FROM task_dependencies d WHERE d.task_id = t.id
                AND NOT EXISTS (SELECT 1 FROM completed_tasks c
                    WHERE c.task_id = d.depends_on))
            AND NOT EXISTS (SELECT 1 FROM task_scope s WHERE s.task_id = t.id
                AND NOT EXISTS (SELECT 1 FROM ownership o
                    WHERE o.path = s.path AND o.generation_id = g.id)))
    OR (SELECT count(*) FROM scheduler_claims c WHERE NOT EXISTS
        (SELECT 1 FROM scheduler_releases r WHERE r.generation_id = c.generation_id))
        >= NEW.capacity
BEGIN
    SELECT RAISE(ABORT, 'only an eligible task owning its whole scope is claimed, within capacity');
END;
CREATE TRIGGER scheduler_claims_immutable BEFORE UPDATE ON scheduler_claims
BEGIN SELECT RAISE(ABORT, 'scheduler history is immutable'); END;
CREATE TRIGGER scheduler_claims_no_delete BEFORE DELETE ON scheduler_claims
BEGIN SELECT RAISE(ABORT, 'scheduler history is immutable'); END;

CREATE TRIGGER scheduler_releases_derived BEFORE INSERT ON scheduler_releases
WHEN EXISTS (SELECT 1 FROM scheduler_releases WHERE generation_id = NEW.generation_id)
    OR NEW.outcome IS NOT (SELECT outcome FROM scheduler_outcomes
        WHERE generation_id = NEW.generation_id)
BEGIN SELECT RAISE(ABORT, 'a claim is released once, with how its pipeline ended'); END;
CREATE TRIGGER scheduler_releases_immutable BEFORE UPDATE ON scheduler_releases
BEGIN SELECT RAISE(ABORT, 'scheduler history is immutable'); END;
CREATE TRIGGER scheduler_releases_no_delete BEFORE DELETE ON scheduler_releases
BEGIN SELECT RAISE(ABORT, 'scheduler history is immutable'); END;

-- A plan's replanning: planner commands, validated as a whole against the
-- plan's state and applied in one transaction, after its planning was
-- finalized. `basis` identifies the plan's replanning state the proposal
-- was made against (see `Store::replan_basis`), which the transaction found
-- unchanged; `journal_id` is the planner action that proposed it, when a
-- planner invocation did. What a replan did is recorded where it did it:
-- the revisions, retry authorizations and cancellations naming it. Never
-- changed after.
CREATE TABLE replans (
    id         INTEGER PRIMARY KEY,
    plan_id    INTEGER NOT NULL REFERENCES plans (id),
    journal_id INTEGER UNIQUE REFERENCES journal (id),
    basis      TEXT    NOT NULL CHECK (length(basis) = 64 AND basis NOT GLOB '*[^0-9a-f]*'),
    commands   INTEGER NOT NULL CHECK (commands > 0),
    applied_at INTEGER NOT NULL
) STRICT;

CREATE TRIGGER replans_applied BEFORE INSERT ON replans
WHEN NOT EXISTS (SELECT 1 FROM plans p WHERE p.id = NEW.plan_id
        AND p.state IN ('ready', 'running', 'paused'))
    OR (NEW.journal_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM journal j
        JOIN agents a ON a.id = j.agent_id
        WHERE j.id = NEW.journal_id AND j.state = 'attempted'
            AND a.role = 'planner' AND a.plan_id = NEW.plan_id))
BEGIN SELECT RAISE(ABORT, 'only a finalized plan is replanned, by its planner'); END;
CREATE TRIGGER replans_immutable BEFORE UPDATE ON replans
BEGIN SELECT RAISE(ABORT, 'replanning history is immutable'); END;
CREATE TRIGGER replans_no_delete BEFORE DELETE ON replans
BEGIN SELECT RAISE(ABORT, 'replanning history is immutable'); END;

-- Each task's current definition, as a revision records it: its objective,
-- its context, and its scope and dependencies as JSON arrays, in order.
CREATE VIEW task_definitions (task_id, objective, context, scope, depends_on) AS
SELECT t.id, t.objective, t.context,
    (SELECT json_group_array(s.path ORDER BY s.path) FROM task_scope s WHERE s.task_id = t.id),
    (SELECT json_group_array(d.depends_on ORDER BY d.depends_on) FROM task_dependencies d
        WHERE d.task_id = t.id)
FROM tasks t;

-- The definitions a task has had since one of its generations was first
-- scheduled or it was first replanned, numbered from 1, each exactly as
-- `task_definitions` held it when recorded: the latest is the definition
-- then current. `replan_id` is the replan that made it current; NULL when
-- it was planned before any replan revised the task, and recorded only
-- once needed. Never changed after.
CREATE TABLE task_revisions (
    task_id     INTEGER NOT NULL REFERENCES tasks (id),
    number      INTEGER NOT NULL CHECK (number > 0),
    objective   TEXT    NOT NULL,
    context     TEXT    NOT NULL,
    scope       TEXT    NOT NULL,
    depends_on  TEXT    NOT NULL,
    replan_id   INTEGER REFERENCES replans (id),
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, number)
) STRICT;

CREATE TRIGGER task_revisions_recorded BEFORE INSERT ON task_revisions
WHEN NEW.number IS NOT (SELECT coalesce(max(number), 0) + 1 FROM task_revisions
        WHERE task_id = NEW.task_id)
    OR NOT EXISTS (SELECT 1 FROM task_definitions d WHERE d.task_id = NEW.task_id
        AND d.objective = NEW.objective AND d.context = NEW.context
        AND d.scope = NEW.scope AND d.depends_on = NEW.depends_on)
    OR (NEW.replan_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM replans r
        JOIN tasks t ON t.plan_id = r.plan_id WHERE r.id = NEW.replan_id AND t.id = NEW.task_id))
BEGIN SELECT RAISE(ABORT, 'a revision records its task''s current definition, in order'); END;
CREATE TRIGGER task_revisions_immutable BEFORE UPDATE ON task_revisions
BEGIN SELECT RAISE(ABORT, 'task revisions are immutable'); END;
CREATE TRIGGER task_revisions_no_delete BEFORE DELETE ON task_revisions
BEGIN SELECT RAISE(ABORT, 'task revisions are immutable'); END;

-- The definition a scheduled generation was started to execute: its task's
-- latest revision, then its current definition, recorded in the
-- transaction that claims it, before any agent serves it. A generation
-- started unscheduled (see `Store::start_generation`) has none: what it
-- executed is not known. Never changed after.
CREATE TABLE generation_revisions (
    generation_id INTEGER PRIMARY KEY REFERENCES generations (id),
    task_id       INTEGER NOT NULL,
    revision      INTEGER NOT NULL,
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions (task_id, number)
) STRICT;

CREATE TRIGGER generation_revisions_bound BEFORE INSERT ON generation_revisions
WHEN NOT EXISTS (SELECT 1 FROM generations g WHERE g.id = NEW.generation_id
        AND g.task_id = NEW.task_id AND g.state = 'active'
        AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.generation_id = g.id))
    OR NEW.revision IS NOT (SELECT max(number) FROM task_revisions WHERE task_id = NEW.task_id)
    OR NOT EXISTS (SELECT 1 FROM task_revisions v
        JOIN task_definitions d ON d.task_id = v.task_id
        WHERE v.task_id = NEW.task_id AND v.number = NEW.revision
            AND d.objective = v.objective AND d.context = v.context
            AND d.scope = v.scope AND d.depends_on = v.depends_on)
BEGIN SELECT RAISE(ABORT, 'a generation is bound to its task''s current definition'); END;
CREATE TRIGGER generation_revisions_immutable BEFORE UPDATE ON generation_revisions
BEGIN SELECT RAISE(ABORT, 'generation revisions are immutable'); END;
CREATE TRIGGER generation_revisions_no_delete BEFORE DELETE ON generation_revisions
BEGIN SELECT RAISE(ABORT, 'generation revisions are immutable'); END;

-- A planner's explicit authorization of one more attempt at a task, after
-- its latest generation: a scheduled one whose pipeline conclusively
-- stopped short of acceptance, which a replan abandoned (see
-- `generation_abandonments`). It authorizes one exact definition of the
-- task: `revision`, current when authorized. A scheduler's claim uses it
-- for exactly one fresh generation, the next, bound to that very revision,
-- recording it in `generation_id`. Once the task is revised again it is
-- stale: never retargeted, and never usable, so that only another
-- authorization runs the new definition. Nothing else ever lets a task
-- that a generation served be claimed again. Never withdrawn, and never
-- changed otherwise.
CREATE TABLE retry_authorizations (
    id               INTEGER PRIMARY KEY,
    after_generation INTEGER NOT NULL REFERENCES generations (id),
    task_id          INTEGER NOT NULL REFERENCES tasks (id),
    revision         INTEGER NOT NULL,
    replan_id        INTEGER NOT NULL REFERENCES replans (id),
    generation_id    INTEGER UNIQUE REFERENCES generations (id),
    authorized_at    INTEGER NOT NULL,
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions (task_id, number),
    UNIQUE (after_generation, revision),
    UNIQUE (after_generation, replan_id)
) STRICT;

CREATE TRIGGER retry_authorizations_given BEFORE INSERT ON retry_authorizations
WHEN NEW.generation_id IS NOT NULL
    OR NOT EXISTS (SELECT 1 FROM generations g
        JOIN tasks t ON t.id = g.task_id
        JOIN replans p ON p.id = NEW.replan_id AND p.plan_id = t.plan_id
        JOIN generation_abandonments b ON b.generation_id = g.id
        JOIN scheduler_releases r ON r.generation_id = g.id
        JOIN scheduler_outcomes o ON o.generation_id = g.id
        WHERE g.id = NEW.after_generation AND g.task_id = NEW.task_id
            AND g.state IN ('failed', 'rejected')
            AND r.outcome <> 'accepted' AND o.outcome IS NOT NULL
            AND g.number = (SELECT max(number) FROM generations WHERE task_id = g.task_id)
            AND NOT EXISTS (SELECT 1 FROM ownership w WHERE w.generation_id = g.id))
    OR NOT EXISTS (SELECT 1 FROM task_revisions v
        JOIN task_definitions d ON d.task_id = v.task_id
        WHERE v.task_id = NEW.task_id AND v.number = NEW.revision
            AND v.number = (SELECT max(number) FROM task_revisions WHERE task_id = v.task_id)
            AND d.objective = v.objective AND d.context = v.context
            AND d.scope = v.scope AND d.depends_on = v.depends_on)
    OR EXISTS (SELECT 1 FROM task_cancellations WHERE task_id = NEW.task_id)
    OR EXISTS (SELECT 1 FROM completed_tasks WHERE task_id = NEW.task_id)
BEGIN
    SELECT RAISE(ABORT, 'only the current revision of a task whose abandoned, conclusively stopped latest generation ended is retried');
END;
CREATE TRIGGER retry_authorizations_used BEFORE UPDATE ON retry_authorizations
WHEN OLD.generation_id IS NOT NULL
    OR NEW.id IS NOT OLD.id OR NEW.after_generation IS NOT OLD.after_generation
    OR NEW.task_id IS NOT OLD.task_id OR NEW.revision IS NOT OLD.revision
    OR NEW.replan_id IS NOT OLD.replan_id OR NEW.authorized_at IS NOT OLD.authorized_at
    OR NOT EXISTS (SELECT 1 FROM generations g
        JOIN generations a ON a.id = OLD.after_generation
        JOIN generation_revisions v ON v.generation_id = g.id
        WHERE g.id = NEW.generation_id AND g.task_id = OLD.task_id AND g.state = 'active'
            AND g.number = a.number + 1 AND v.revision = OLD.revision
            AND NOT EXISTS (SELECT 1 FROM agents x WHERE x.generation_id = g.id))
    OR EXISTS (SELECT 1 FROM task_cancellations WHERE task_id = OLD.task_id)
BEGIN
    SELECT RAISE(ABORT, 'a retry authorization starts one fresh generation of the revision it authorized, once');
END;
CREATE TRIGGER retry_authorizations_no_delete BEFORE DELETE ON retry_authorizations
BEGIN SELECT RAISE(ABORT, 'retry authorizations are never withdrawn'); END;

-- A task a replan cancelled: superseded, never claimed again, and never
-- depended on, while everything it and its generations recorded stays. Only
-- a task no generation is running or accepted is cancelled. Never changed
-- after.
CREATE TABLE task_cancellations (
    task_id      INTEGER PRIMARY KEY REFERENCES tasks (id),
    replan_id    INTEGER NOT NULL REFERENCES replans (id),
    cancelled_at INTEGER NOT NULL,
    UNIQUE (task_id, replan_id)
) STRICT;

CREATE TRIGGER task_cancellations_recorded BEFORE INSERT ON task_cancellations
WHEN EXISTS (SELECT 1 FROM generations g WHERE g.task_id = NEW.task_id
        AND g.state IN ('active', 'accepted'))
    OR NOT EXISTS (SELECT 1 FROM replans p JOIN tasks t ON t.plan_id = p.plan_id
        WHERE p.id = NEW.replan_id AND t.id = NEW.task_id)
BEGIN SELECT RAISE(ABORT, 'only a task with no active or accepted generation is cancelled'); END;
CREATE TRIGGER task_cancellations_immutable BEFORE UPDATE ON task_cancellations
BEGIN SELECT RAISE(ABORT, 'cancellations are immutable'); END;
CREATE TRIGGER task_cancellations_no_delete BEFORE DELETE ON task_cancellations
BEGIN SELECT RAISE(ABORT, 'cancellations are immutable'); END;

-- A replan's abandonment of a scheduled generation whose pipeline
-- conclusively stopped short of acceptance, as its released claim and
-- `scheduler_outcomes` establish, with no acceptance begun: its planner
-- decided that attempt is never accepted, and retried (`retried`, the
-- generation) or cancelled (`cancelled`, its task) the task. Recorded in
-- the replan's transaction, once the candidate it installed, if any, was
-- restored in the working tree to the last accepted state, and before the
-- generation ends as `state` and releases its ownership, which nothing
-- else does to a scheduled generation short of acceptance. It cannot
-- commit without the whole of that: its deferred references require the
-- generation ended as `state`, which it does only owning nothing, and the
-- replan's retry authorization after it or cancellation of its task.
-- Never changed after.
CREATE TABLE generation_abandonments (
    generation_id INTEGER PRIMARY KEY,
    state         TEXT    NOT NULL CHECK (state IN ('failed', 'rejected')),
    replan_id     INTEGER NOT NULL REFERENCES replans (id),
    retried       INTEGER,
    cancelled     INTEGER,
    abandoned_at  INTEGER NOT NULL,
    CHECK ((retried IS NULL) <> (cancelled IS NULL)),
    FOREIGN KEY (generation_id, state) REFERENCES generations (id, state)
        DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (retried, replan_id) REFERENCES retry_authorizations (after_generation, replan_id)
        DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (cancelled, replan_id) REFERENCES task_cancellations (task_id, replan_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

-- What an abandonment's deferred reference to its generation needs.
CREATE UNIQUE INDEX generations_state ON generations (id, state);

-- Only the latest replan of the generation's plan abandons it, and only
-- the latest generation of its task, while scheduled, released and
-- conclusively stopped short of acceptance: nothing of it is live or
-- unknown and no acceptance of it began. It ends rejected when a verifier
-- failed it, failed otherwise; one that ended already stays as it ended.
CREATE TRIGGER generation_abandonments_authorized BEFORE INSERT ON generation_abandonments
WHEN NOT EXISTS (SELECT 1 FROM generations g
        JOIN tasks t ON t.id = g.task_id
        JOIN replans p ON p.id = NEW.replan_id AND p.plan_id = t.plan_id
        JOIN scheduler_releases r ON r.generation_id = g.id
        JOIN scheduler_outcomes o ON o.generation_id = g.id
        WHERE g.id = NEW.generation_id
            AND NEW.state IS CASE WHEN g.state <> 'active' THEN g.state
                WHEN o.outcome = 'verification_failed' THEN 'rejected' ELSE 'failed' END
            AND r.outcome <> 'accepted' AND o.outcome IS NOT NULL AND o.outcome <> 'accepted'
            AND NOT EXISTS (SELECT 1 FROM acceptances a WHERE a.generation_id = g.id)
            AND g.number = (SELECT max(number) FROM generations WHERE task_id = g.task_id)
            AND p.id = (SELECT max(id) FROM replans WHERE plan_id = t.plan_id)
            AND (NEW.retried IS g.id OR NEW.cancelled IS g.task_id))
BEGIN SELECT RAISE(ABORT, 'only a replan abandons a conclusively stopped scheduled generation'); END;
CREATE TRIGGER generation_abandonments_immutable BEFORE UPDATE ON generation_abandonments
BEGIN SELECT RAISE(ABORT, 'abandonments are immutable'); END;
CREATE TRIGGER generation_abandonments_no_delete BEFORE DELETE ON generation_abandonments
BEGIN SELECT RAISE(ABORT, 'abandonments are immutable'); END;

-- A scheduled generation ends short of acceptance only as its abandonment
-- says, once it owns nothing.
CREATE TRIGGER generations_abandoned_by_replan BEFORE UPDATE ON generations
WHEN NEW.state IS NOT OLD.state AND NEW.state IN ('failed', 'rejected')
    AND EXISTS (SELECT 1 FROM scheduler_claims WHERE generation_id = OLD.id)
    AND (NOT EXISTS (SELECT 1 FROM generation_abandonments
            WHERE generation_id = OLD.id AND state = NEW.state)
        OR EXISTS (SELECT 1 FROM ownership WHERE generation_id = OLD.id))
BEGIN
    SELECT RAISE(ABORT, 'a scheduled generation ends short of acceptance only as a replan abandons it, owning nothing');
END;

-- A scheduled generation keeps its ownership until accepted (see
-- `ownership_held_through_acceptance`) or abandoned.
CREATE TRIGGER ownership_held_until_abandoned BEFORE DELETE ON ownership
WHEN EXISTS (SELECT 1 FROM scheduler_claims WHERE generation_id = OLD.generation_id)
    AND NOT EXISTS (SELECT 1 FROM acceptances WHERE generation_id = OLD.generation_id)
    AND NOT EXISTS (SELECT 1 FROM generation_abandonments
        WHERE generation_id = OLD.generation_id)
BEGIN
    SELECT RAISE(ABORT, 'a scheduled generation''s ownership is released only by accepting or abandoning it');
END;
