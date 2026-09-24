-- Schema version 2 -> 3 adds CodeGraph. Version 2 held no graph facts, so
-- every source starts without any: nothing is carried into the stronger
-- invariant that graph facts are bound to accepted content. The definitions
-- must match schema.sql.

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
