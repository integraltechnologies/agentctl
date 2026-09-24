-- Schema version 1 -> 2: `accepted_sources` now records only accepted state
-- whose recovery object was published before the row was committed, and may
-- accept a path as not existing. Version 1 predates recovery objects, so none
-- of its accepted hashes is known to be recoverable, and it could not record
-- absence: its rows are dropped, not carried forward, leaving every path
-- without accepted state until the source layer establishes it from actual
-- bytes. The table definition must match schema.sql.
DROP TABLE accepted_sources;

CREATE TABLE accepted_sources (
    path          TEXT    PRIMARY KEY,
    hash          TEXT    CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    generation_id INTEGER REFERENCES generations (id)
) STRICT, WITHOUT ROWID;
