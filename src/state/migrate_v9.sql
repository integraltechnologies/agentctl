-- Schema version 9 -> 10: an executor works in a disposable copy of the
-- repository outside the project, holding neither agentctl's state nor
-- Git's, and its capture observes that copy, which nobody else writes.
-- Another action in flight, or a path another generation owns, no longer
-- makes a change there unattributable, and Git's HEAD, which the copy does
-- not hold, is evidence only. A candidate is then installed into the
-- project's working tree, as `execution_installs` and
-- `execution_install_results` record. The definitions must match
-- schema.sql.
--
-- Captures already recorded are kept exactly: their changes were made in
-- the working tree itself, and none of them was installed.
DROP TRIGGER execution_captures_derived;
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
