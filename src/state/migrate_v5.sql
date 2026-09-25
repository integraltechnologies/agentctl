-- Schema version 5 -> 6: a plan's human intent becomes structured and
-- immutable, and tasks become addressable by planners. Version 5 held intent
-- as one statement, which becomes the objective, with no constraints or
-- completion criteria stated. Its tasks had only a description, which
-- becomes the objective; each is keyed `task-<id>`, with no context and no
-- requested paths. Nothing more is claimed than version 5 recorded.
--
-- `Store` migrates without enforcing foreign keys, and the legacy rename
-- leaves other tables' references naming `plans` and `tasks`, which are
-- rebuilt in place; `Store` checks every reference afterwards. The
-- definitions must match schema.sql.
PRAGMA legacy_alter_table = ON;

ALTER TABLE plans RENAME TO plans_v5;

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

INSERT INTO plans
    (id, objective, constraints, completion_criteria, state, created_at, updated_at)
SELECT id, intent, '[]', '[]', state, created_at, updated_at FROM plans_v5;

DROP TABLE plans_v5;

CREATE TRIGGER plans_intent_immutable BEFORE UPDATE ON plans
WHEN NEW.id IS NOT OLD.id OR NEW.objective IS NOT OLD.objective
    OR NEW.constraints IS NOT OLD.constraints
    OR NEW.completion_criteria IS NOT OLD.completion_criteria
BEGIN SELECT RAISE(ABORT, 'human intent is immutable'); END;

ALTER TABLE tasks RENAME TO tasks_v5;

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

INSERT INTO tasks (id, plan_id, key, objective, context, created_at)
SELECT id, plan_id, 'task-' || id, description, '', created_at FROM tasks_v5;

DROP TABLE tasks_v5;

CREATE TABLE task_scope (
    task_id INTEGER NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    path    TEXT    NOT NULL CHECK (path <> ''),
    PRIMARY KEY (task_id, path)
) STRICT, WITHOUT ROWID;

PRAGMA legacy_alter_table = OFF;
