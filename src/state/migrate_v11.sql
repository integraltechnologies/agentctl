-- Schema version 11 -> 12: a verified candidate is accepted in phases,
-- as `acceptances`, `acceptance_sources` and `acceptance_phases` record,
-- and generations that executed are accepted only that way. The
-- definitions must match schema.sql.
--
-- Nothing already recorded was accepted this way, so no acceptance is
-- recorded for it: accepted source, CodeGraph, ownership and generations
-- stay exactly as they were.
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
