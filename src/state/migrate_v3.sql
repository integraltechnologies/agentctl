-- Schema version 3 -> 4: invocations record their lifecycle, how they
-- ended and the usage observed. Version 3 recorded only whether an
-- invocation had ended, not how, so an ended one becomes 'interrupted'
-- (agentctl does not know how it ended) with usage unavailable, and a live
-- one stays 'running', which is exactly what version 3 knew. Nothing
-- references invocations. The definitions must match schema.sql.
DROP INDEX invocations_live;
ALTER TABLE invocations RENAME TO invocations_v3;

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

INSERT INTO invocations
    (id, agent_id, provider, model, state, diagnostic, usage, started_at, ended_at)
SELECT id, agent_id, provider, model,
       iif(ended_at IS NULL, 'running', 'interrupted'),
       iif(ended_at IS NULL, NULL, 'ended before agentctl recorded how invocations end'),
       iif(ended_at IS NULL, NULL, 'unavailable'),
       started_at, ended_at
FROM invocations_v3;

DROP TABLE invocations_v3;

-- A logical agent is embodied by at most one live invocation.
CREATE UNIQUE INDEX invocations_live ON invocations (agent_id)
    WHERE ended_at IS NULL;
