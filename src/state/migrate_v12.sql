-- Schema version 12 -> 13: schedulers claim tasks of running plans, each
-- claim starting a generation that owns its task's whole scope and holding
-- a unit of the project's concurrency ceiling until released, as
-- `scheduler_claims` and `scheduler_releases` record, and only a completed
-- acceptance completes a task for its dependents (`completed_tasks`). The
-- definitions must match schema.sql.
--
-- No scheduler ran before, so nothing is claimed: generations that exist,
-- active or not, stay exactly as they were, never scheduled, and a task
-- one of them ever served is never claimed.
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
-- that starts the generation and acquires its ownership, and only for a
-- task of a running plan that no generation ever served and whose every
-- dependency is completed, while fewer than `capacity` claims are held:
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
            AND NOT EXISTS (SELECT 1 FROM generations o
                WHERE o.task_id = t.id AND o.id <> g.id)
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
