-- Schema version 10 -> 11: an installed candidate is verified by a fresh,
-- independent verifier, as `verifications` and `verification_results`
-- record. The definitions must match schema.sql.
--
-- Nothing already recorded was ever verified, so no verification is
-- recorded for it: absence stays absence.
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
