# Architecture

This document describes agentctl's major components, the state each one owns,
and the authority boundaries between them. For commands, see [cli.md](cli.md).
For configuration, see [configuration.md](configuration.md). For sandboxing, see
[security.md](security.md).

## Principles

- **agentctl owns canonical engineering state.** Plans, tasks, jobs, evidence,
  verification decisions, the code graph, and engineering memory live in a local
  SQLite database. Provider conversations are disposable workers. They are never
  resumed, replayed, or treated as a source of truth.
- **Roles, not providers.** Work asks for a planner, executor, or verifier.
  Machine configuration decides which installed CLI and model serve a role.
  Provider and model names are opaque metadata.
- **Only verification unlocks work.** Executor success is an outcome report, not
  acceptance. Only an independent verifier's `PASS` makes a task `VERIFIED`, and
  only `VERIFIED` prerequisites satisfy dependencies. A plan completes only after
  a separate integration verification.
- **Model output is untrusted data.** It is parsed strictly and validated, and it
  is never executed as shell text. It cannot change routing, permissions, or
  limits.
- **Fail closed.** Unknown fields, versions, and enum values are rejected. Missing
  isolation refuses the launch. Uncertain state blocks instead of being assumed
  successful.

agentctl is not a chat-memory store, an autonomous coding agent, or a wrapper
around one provider.

## Components

```text
              ┌─────────────────────────── agentctl CLI / agenttop ────────────────────────────┐
              │                                                                                 │
   repository │  graph ──▶ memory ──▶ planning ──▶ runtime engine ──▶ adapters ──▶ security ──▶ OS │
   (Git)      │    │         │           │              │     │                       backend     │
              │    └─────────┴───────────┴──────┬───────┘     └── checks / experiments ──┘         │
              │                                 ▼                                                 │
              │               SQLite store + append-only journal + private artifacts             │
              │                                 ▲                                                 │
              │                     observe ──┬─┘── analytics                                     │
              │                               └── agenttop                                        │
              └─────────────────────────────────────────────────────────────────────────────────┘
```

| Component | Source | Responsibility |
| --- | --- | --- |
| Protocol | `src/protocol.rs`, `src/validation.rs`, `src/lifecycle.rs`, `src/schema.rs` | versioned wire contracts, DAG and lifecycle rules, JSON Schema generation |
| Paths & config | `src/local/paths.rs`, `src/local/config.rs` | XDG locations, safe file handling, machine and project configuration |
| Repository | `src/local/repository.rs` | Git discovery, repository/workspace identity, hardened Git subprocesses |
| Store | `src/local/store.rs`, `src/local/migrations.rs` | SQLite ownership, migrations, guarded transitions, journal |
| Code graph | `src/local/graph/` | Tree-sitter extraction, incremental indexing, bounded queries, ontology generation lifecycle and semantic deltas |
| Memory | `src/local/memory/` | trust-classified engineering memory with provenance |
| Planning | `src/local/planning/` | planning requests, planner input, plan import, lifecycle, completion gate |
| Runtime | `src/local/runtime/` | engine, routing, prompt compilation, provider adapters, source capture, experiments |
| Security | `src/local/security/` | OS-neutral policy, capability checks, platform backends, process-tree cleanup |
| Observe | `src/local/observe/`, `src/local/agenttop.rs` | read-only live projection and terminal UI |
| Analytics | `src/local/analytics/` | read-only historical metrics |

## Protocol

The public contracts are the documents that pass between agentctl and planners,
executors, and verifiers, plus the durable records built from them. There are 14,
each with a generated JSON Schema in `schemas/`: plans, tasks, results,
verification, resume, evidence, jobs, agent events, probes, token usage,
experiments, experiment events, memory provenance, and context requests.

- Every document carries `"version": "1"`. Enums are `SCREAMING_SNAKE_CASE`, and
  fields are `snake_case`.
- IDs are 1–128 ASCII characters matching `[A-Za-z0-9][A-Za-z0-9._:-]*`.
- Unknown fields, enum variants, and versions are rejected. Incompatible changes
  require a new wire version.
- JSON Schema validates structure. `Validate` adds semantic checks: acyclic
  dependencies, reference consistency, nonblank content, finite numbers, and so
  on. Consumers must run both.
- Rust types are canonical. Tests fail if `schemas/` drifts from the generated
  output.

A **PlanPacket** is a nonempty DAG of **TaskPackets**. Each task owns its
dependency list, so there is no separate edge list that could disagree. Each
TaskPacket carries the following:

- an objective;
- separate read and write scopes (normalized repository-relative paths, matched
  literally: `app/[slug]` is the directory named `[slug]`; traversal and the
  `*`/`?` wildcards are refused);
- graph references;
- critical invariant references;
- a definition of done;
- mandatory verification requirements.

Packets carry compact references, not source dumps or provider instructions.

### Task and job lifecycles

```text
Task:  PLANNED → READY → EXECUTING → AWAITING_VERIFICATION → VERIFYING → VERIFIED
                                                                       → REJECTED
       any nonterminal state → BLOCKED → PLANNED   (explicit reassessment)

Job:   QUEUED → RUNNING ⇄ WAITING
       QUEUED → CANCELLED
       RUNNING / WAITING → SUCCEEDED | FAILED | CANCELLED
```

`VERIFIED` and `REJECTED` are terminal. A rejection returns to planning as a new
correction plan with new task IDs. There is no automatic executor↔verifier retry
loop. A packet verifier's job must differ from the executor's. Plan completion
requires an `INTEGRATION` PASS covering the integration requirements and the union
of all critical task invariants.

Token counts carry mandatory provenance. `EXACT` means the provider reported the
counts, `ESTIMATED` means they were estimated, and `UNKNOWN` carries no counts,
never an invented zero.

## Local state

| Location | Contents |
| --- | --- |
| `~/.config/agentctl/config.toml` | machine configuration |
| `~/.local/share/agentctl/state.sqlite3` | canonical database |
| `~/.local/share/agentctl/runtime/` | `blobs/`: private content-addressed artifacts (source snapshots, diffs, bounded provider output, command logs), verified by hash and length on read; `scratch/`: per-job scratch directories; `locks/`: workspace leases |
| `~/.cache/agentctl/` | reconstructible cache; never the only copy of anything |

All three roots honor absolute `XDG_*` overrides. See
[configuration.md](configuration.md#locations).

**SQLite.** The database uses synchronous `rusqlite` with bundled SQLite. There is
no daemon, server, ORM, or async runtime. Writable connections enable foreign keys,
WAL, `synchronous=FULL`, and a bounded busy timeout. Writers use
`BEGIN IMMEDIATE`, and readers can coexist with writers. Back up with a consistent
SQLite snapshot (including WAL contents), not by copying the main file while it is
in use.

**Migrations.** The current schema version is 13. Migrations are additive and
transactional, and they run on writable opens. They verify their required guards,
refuse databases from newer versions or with foreign or unversioned content, and
roll back entirely on conflict. Migrations never repair or synthesize history.
Read-only commands never migrate. `agentctl doctor` checks database health without
modifying it.

**Journal.** State transitions and their journal entries commit in the same
transaction. The journal is append-only. `UPDATE`/`DELETE` triggers protect
history from accidental direct SQL, and a global sequence defines order. Lifecycle
changes to runtime-owned plans, tasks, and jobs require a connection-local
capability that only the runtime's validated operations grant. Plan completion
additionally requires a payload-bound capability together with its matching audit
event. These guards protect against accidental or out-of-band changes through
ordinary connections. They do not protect against a machine owner who can alter
the schema.

## Repositories and workspaces

A **repository ID** hashes the canonical Git common directory, so a main checkout
and its linked worktrees share one repository. A **workspace ID** hashes the
per-worktree Git directory. Durable decisions and memory are shared at repository
level. Graph rows, jobs, evidence, active plans, and runtime authority are bound
to a concrete workspace.

Independent clones are distinct repositories. Moving a primary repository normally
changes its IDs. The old registration stays inspectable, and nothing is rekeyed or
merged automatically. Device and inode metadata detect a Git directory being
replaced at the same path.

Source observations (HEAD, dirty flag, and later full manifests) are sequential
observations. They are not atomic filesystem snapshots.

## Code graph

The graph uses Tree-sitter with pinned grammars for Rust, Python, TypeScript/TSX,
and JavaScript/JSX. A small internal adapter converts syntax trees into
language-neutral entities (files, modules, functions, methods, types, enums,
traits, constants, tests) and relations (`CONTAINS`, `IMPORTS`, `CALLS`,
`REFERENCES`, `IMPLEMENTS`, `TEST_RELATED_TO`, `DEPENDS_ON`). Sources are never
executed. No language server, build script, package manager, or network is
involved.

- **Identity.** Entity IDs are domain-separated BLAKE3 hashes of the repository,
  path, language, kind, lexical qualified name, and duplicate ordinal. Body edits
  and line shifts preserve an entity's identity; renames and moves change it.
- **Provenance.** Every fact records its workspace, path, content hash, and
  grammar/parser and index versions. Rows are keyed by workspace, so hashes,
  ranges, and freshness never leak between worktrees.
- **Incremental indexing.** Each pass hashes files. Unchanged files are reused;
  new or changed files are reparsed; deleted, ignored, and excluded files are
  removed. A per-file failure removes that file's old facts, records a diagnostic,
  and makes the command exit nonzero while keeping the other results.
  Database-level failures roll back the whole pass.
- **Freshness.** Queries rehash the discovered source in a read snapshot and
  refuse stale results.
- **Literal paths.** Discovered repository paths are literal names. Characters
  that routing conventions or pattern APIs give meaning (`[slug]`, `[...slug]`,
  `[[...slug]]`, `(group)`, `@slot`, `{}`, `%`, `_`, and on Unix `*`/`?`) are
  ordinary filename characters throughout discovery, indexing, queries, planner
  context, runtime capture, and reported changes. No interface gives a
  repository path pattern semantics: SQL `LIKE` input is escaped, Git receives
  no pathspecs, and sandbox profiles quote paths as literals. Traversal, absolute
  paths, empty segments, backslashes, `:` and control characters stay rejected,
  and authored scopes additionally refuse the `*`/`?` wildcards.
- **Resolution.** Every rule is syntactic and requires exactly one compatible
  candidate; anything ambiguous stays unresolved, and each resolved relation
  names its rule. `LEXICAL_SCOPE`: bare Rust and Python calls to the unique
  declaration visible in the file's scope chain (never through a parameter,
  `let` or assignment that shadows the name; Rust `use super::*` reaches the
  enclosing module), plus `self::`/`super::` paths. `ENCLOSING_TYPE`:
  `self.m()`, `this.m()`, `Self::m` and `Type::m` to a method of a type in the
  same file. `QUALIFIED_PATH`: Rust `module::f`, `Type::f` and
  `crate::`/`super::`/`self::` paths into other files, matched by unique suffix
  of a normalized module path (`mod.rs`/`lib.rs` and `impl` headers
  normalized). A qualified path whose leading segment names no module or crate
  root agentctl can identify from repository structure — an external crate, an
  unresolved re-export, or a typo — stays unresolved; a unique match on the
  remaining suffix elsewhere in the workspace is not treated as evidence of
  identity, since agentctl parses no Cargo.toml or workspace metadata to prove
  what that segment names. Cross-file resolutions live in a derived table
  rebuilt in the index transaction, so re-deriving one file never cascades
  away another file's relations. Imports and re-exports are not followed,
  macros are not expanded, and there is no type inference.
- **Generations.** Each index pass records a generation: a fingerprint over the
  index version and every indexed file's path, content hash, backend, and
  diagnostic, plus a per-workspace sequence that advances only when the
  fingerprint changes. Graph context, PlannerPackets, job manifests, and
  `INDEX_COMPLETED` events carry it. Import and activation reject a planning
  source bound to a generation the workspace has not reached. Which generation
  is *trusted* is decided separately; see [Ontology lifecycle](#ontology-lifecycle).
- **Ranking.** Queries drop English stopwords and apply a small symmetric
  stemmer. Terms are weighted by integer IDF over the workspace's entities,
  fields by name > container > path > signature, and precise names over long
  ones. Distinct matched terms are coordinated, and entities are scaled by how
  much of the query's IDF mass their file's declarations cover. The strongest
  implementation matches credit what they resolve to. Implementation and test
  entities rank in separate lanes: primary context is implementation first,
  with one slot kept for another file whenever one qualifies, and tests follow
  with an association basis (`CALLS`, `CALLS_VIA_HELPER`, `CONTAINER`,
  `LEXICAL`). Neighbors are resolved relations of the primary entities, spread
  across files. Relations in context are resolved only; unresolved call sites
  are summarized per entity as bounded name lists. Context packets write
  provenance once per file in a source table. A scoped request or task selects
  within its scope. Ranking scans the workspace's entities twice per query
  (bounded memory; O(entities) time).
- **Bounds.** Up to 20,000 supported files, 100,000 visited entries, depth 64,
  2 MiB per file, 200,000 syntax nodes per file, and a 2-second parse budget. Limits
  are explicit failures.

Tree-sitter was chosen over language-server-backed tools (richer resolution, but
extra processes, toolchains, and runtime surface) and application-level repo maps
(useful compact context, but not a storage and provenance API). The core never
handles Tree-sitter nodes directly, so adding a language means adding one adapter.

## Ontology lifecycle

The live graph tables materialize the most recent index pass. That is an
*observation* of the worktree, not canonical truth. Canonical truth is the
workspace's single **accepted generation**: a durable pointer to an immutable
snapshot of the facts. `src/local/graph/lifecycle.rs` owns the pointer and the
lifecycle; `src/local/graph/delta.rs` owns snapshots and semantic deltas.

```text
index pass ─▶ CANDIDATE ─┬─ accept ─────────────────▶ ACCEPTED ── later acceptance ─▶ RETIRED
                         ├─ reject ─────────────────▶ REJECTED
                         └─ superseded / plan closed ▶ ABANDONED
```

- **Records.** Every observation that changes what is recorded becomes a row in
  `ontology_generations`: generation ID, a per-workspace **ordinal** (lifecycle
  order, never repeated), the graph generation (sequence and fingerprint), the
  snapshot, file/entity/relation counts, file failures, the HEAD/dirty
  observation, the origin (`EXTERNAL`, or `RUNTIME` with plan and task), the
  accepted generation it was compared with (`base`), the delta, and the
  acceptance and closure decisions with reason, time, superseding generation,
  plan, task, verification hash and final-source hash. Transitions append an
  `ONTOLOGY_GENERATION_CHANGED` journal entry in the same transaction.
- **Guards.** SQL triggers allow only `CANDIDATE → ACCEPTED | REJECTED |
  ABANDONED` and `ACCEPTED → RETIRED`, forbid changing identity columns, and
  forbid deletion. Partial unique indexes allow at most one `ACCEPTED` and one
  `CANDIDATE` row per workspace, so the canonical pointer is always a single
  complete row. Nothing is ever deleted; closed records stay inspectable.
- **Observation.** An index pass that leaves the recorded generation unchanged
  records nothing. Otherwise it snapshots the live facts, supersedes any open
  candidate (`ABANDONED`, reason `SUPERSEDED`), and records a new row:
  - no accepted generation yet, a complete pass (no file failures), external
    origin: **accepted** as `BOOTSTRAP`;
  - facts identical to the accepted generation (same fingerprint and an empty
    delta), complete: **accepted** as `IDENTICAL_TO_ACCEPTED`, retiring the
    previous row. A revert is therefore a new position with the old
    fingerprint and a later sequence and ordinal, never a return to the old
    row;
  - anything else: a **candidate** with a delta against the accepted
    generation.

  An explicit (external) index of an unchanged generation opens a new
  candidate only when nobody could otherwise decide it: its latest record is
  closed, or it is a runtime candidate. The runtime never reopens anything.
- **Who may accept.** `agentctl ontology accept` accepts an external candidate
  that is still the indexed generation, whose worktree still hashes to it, with
  no file failures, and whose `base` is still the accepted generation.
  Runtime candidates are accepted only at their plan's acceptance boundary.
  Acceptance retires the previous generation in the same transaction and is
  idempotent. Planning (`plan prepare`) and runtime adoption require the
  indexed generation to *be* the accepted generation, and a plan's bound
  generation to equal it.

### Snapshots and semantic deltas

A snapshot is a manifest of per-file fact chunks in `ontology_blobs`, a
content-addressed, immutable table verified by hash and length on every read.
Unchanged files share chunks across generations, so a generation costs roughly
the size of the files whose facts changed. A chunk holds, for one file, its
content hash, backend and diagnostic; each entity's kind, name, qualified name,
key, path, line range, signature, visibility and **text hash**; and the file's
distinct resolved relations (source, kind, target, target path, rules).
`CONTAINS`/`TEST_RELATED_TO` links (implied by entity facts) and unresolved
call sites are not relation facts. Snapshots live in SQLite, not the runtime
CAS, so a generation row and its snapshot commit atomically and the graph layer
does not depend on the runtime.

The **text hash** is BLAKE3 over a declaration's own source: the declaration
plus the comment, attribute and decorator lines directly above it, with nested
declarations (and their attached lines) cut out together with the whitespace
around them. An edit is attributed to the innermost declaration containing it,
and adding or removing a member does not by itself modify its container.
Whitespace inside a declaration is significant. Text hashes are computed when a
file is parsed and stored with its entity rows.

A **semantic delta** (`SemanticDelta`, version `1`) is a pure function of two
snapshots of one workspace indexed by the same graph version, stored as an
artifact when a candidate is recorded and derivable on demand between any two
records. It contains:

- `entities`: `ADDED` and `REMOVED` with the full facts, and `MODIFIED` with
  before/after facts and the changed fields (`SIGNATURE`, `VISIBILITY`, `TEXT`,
  `KEY`). A range-only move is not a change. The `FILE` entity's text is not
  compared; file content changes are reported per file;
- `relations`: distinct resolved relations `ADDED` or `REMOVED`. Call-site
  multiplicity and positions are not facts. A relation can change in a file
  whose bytes did not (for example when another file makes a qualified path
  ambiguous and graph resolution abstains);
- `files`: every file whose content or facts differ, with its content change
  (`ADDED`, `REMOVED`, `MODIFIED`, `UNCHANGED`) and counts of entity and
  relation changes, so files with semantic changes are directly queryable;
- `summary` counts, and both endpoints (generation ID, generation, snapshot).

Output is sorted, so identical inputs give identical bytes. Files whose chunks
are identical are skipped without being read. Across different graph versions
no delta is derived (`UNAVAILABLE`); a delta over 64 MiB is recorded as
unavailable as well.

### Cross-generation identity

Two entities in different generations are the same entity only when both hold:

1. they have the same graph entity ID, which already binds repository, path,
   language, kind, lexical qualified name and duplicate ordinal; and
2. their `(path, kind, qualified name)` group holds exactly one declaration in
   **both** generations (`identity: UNIQUE`).

Consequences:

- a body edit, a signature or visibility edit, a doc-comment or attribute
  edit, and a line shift keep identity (`MODIFIED`, or no change for a pure
  shift);
- a rename, a file move, and a move into another container change the ID, so
  they are reported as `REMOVED` plus `ADDED`. agentctl makes no rename or move
  claim, because no structural evidence proves one;
- methods of different types have different qualified names and never match;
- a group with several same-named declarations is ordinal-numbered in file
  order, so its IDs prove nothing about which declaration they name. Unless the
  group's facts are unchanged as a multiset, every member is reported as
  `REMOVED` and `ADDED` with `identity: DUPLICATE_ORDINAL`, and so is every
  relation touching one of them, even if the relation's IDs are unchanged.

No similarity measure, heuristic, or model is used.

### The acceptance boundary

For runtime work, the boundary is **plan completion**:

1. After a task's verifier `PASS`, the runtime re-indexes. The result is the
   plan's candidate (origin `RUNTIME`, plan and task); the previous one is
   superseded. It is the working ontology for the plan's downstream tasks, but
   it is not accepted: those tasks' context comes from it, and the next task's
   verification still lies ahead.
2. When every task is verified, integration checks and the integration verifier
   run. Only when that proof is `PASS` does
   `complete_execution_plan_accepting` complete the plan **and** promote the
   candidate in one transaction. It requires the indexed generation to be an
   open candidate that this plan's runtime itself recorded (same sequence and
   fingerprint, whatever the latest record's label), the index to rehash clean
   against the worktree, and the worktree to equal the verified final state.
   The decision records the plan, the verification hash and the final
   source-state hash.
3. A plan whose tasks left the accepted facts unchanged promotes nothing.

Per-task acceptance was rejected: the accepted ontology would then describe
intermediate states that no integration verification covered.

### Rejection, abandonment, and external changes

| Event | Effect on the ontology |
| --- | --- |
| task verifier `REJECT` | the plan's open candidate becomes `REJECTED` (`VERIFICATION_REJECTED`); the rejected edit is never indexed by the runtime |
| integration verifier `REJECT` | the candidate becomes `REJECTED` (`INTEGRATION_REJECTED`) |
| verifier `BLOCKED`, executor or verifier crash, interrupted jobs, drift | nothing: the candidate stays open and unaccepted; the run blocks |
| plan cancelled or superseded | an open candidate of that plan becomes `ABANDONED` |
| a later observation | the open candidate becomes `ABANDONED` (`SUPERSEDED`, naming its successor) |
| `ontology reject` | an external candidate becomes `REJECTED`; the live index still holds its facts, so planning refuses until the source is restored or a new observation is accepted |

When source changes without an agentctl transition, `repo index` records a
candidate and planning refuses until someone runs `ontology accept` or restores
the source. If a run stops for good, a human `repo index` turns its state into
an external candidate that can be inspected and accepted; the delta then
includes whatever unverified edits the worktree holds.

### Crash consistency

- Snapshot, delta, lifecycle row, supersession and journal entry are written
  inside the index transaction, so a crash leaves either the previous state or
  the complete observation. Blobs are only ever inserted; a rolled-back pass
  leaves none behind.
- A crash after a task's `TASK_VERIFIED` checkpoint but before its re-index is
  repaired on resume by the existing refresh (runtime origin).
- A crash before integration verification leaves the candidate open;
  resuming re-runs integration and accepts once. A crash after integration
  verification is recovered from the recorded verifier output and goes through
  the same completion transaction.
- Plan completion and promotion commit together. A failure inside it rolls
  both back (the plan stays `ACTIVE`, the accepted pointer is unchanged). A
  crash after it leaves a completed plan with its candidate accepted; resuming
  only finishes the run record.
- The accepted pointer can therefore never name an incomplete, failed or
  mismatched observation: the row is complete when inserted, promotion checks
  the live generation, file failures and base, and the unique index forbids two
  accepted rows.

### Relationship to the context relay

Context bases and ContextDeltas stay bound to the graph generation (sequence and
fingerprint) they were derived from, and every round revalidates it. The
lifecycle adds:

- a base is issued only when the indexed generation is the accepted one or an
  open candidate that this plan's runtime recorded; an unexplained external
  observation blocks issuance (`SOURCE_DRIFT`);
- every observation that changes the indexed facts moves the indexed
  generation away from the one a ledger is bound to, and acceptance or
  rejection never moves it back. An old base or delta can therefore never be
  reused against a different accepted generation, even when a revert restores
  the old fingerprint (its sequence differs). Old artifacts remain inspectable
  through `run context`;
- verifier independence, issued visibility and manifest accounting are
  unchanged. Semantic deltas are not issued to workers.

## Semantic impact

A delta says what changed. Impact analysis answers the next question — *what
existing code or behavior could that plausibly affect, and what proves it?* —
over the same Stage-1 ontology, with no second graph, no embeddings and no
model. The invariant is that **no impact claim exists without a machine
inspectable evidence path through already-observed facts**, and that anything
the ontology cannot prove is represented rather than guessed.

### Evidence model

Every reported item carries an ordered chain of hops from a seed. A hop is one
of three observed facts, and nothing else can create one:

- a **resolved relation** (`CALLS`, `REFERENCES`, `IMPLEMENTS`, `IMPORTS`,
  `DEPENDS_ON`) with the resolution rules that produced it. Unresolved
  syntactic relations never form a hop;
- **containment**, used only for the container of a declaration that was added
  or removed, whose composition therefore changed;
- a **Stage-1 test association**, carrying its `AssociationBasis` so a
  container guess is never read as proven coverage.

Items are typed by what the evidence shows:

| Class | What it claims |
| --- | --- |
| `DIRECT_DEPENDENCY` | One resolved relation away from a changed entity. |
| `CONTRACT_EXPOSURE` | Reached through a node the ontology proves re-exposes the change. |
| `VERIFICATION_RELEVANCE` | A test associated with a changed or affected entity: a candidate check, not proven coverage. |
| `CONTAINMENT_OWNERSHIP` | A container whose composition changed. |

Uncertainty is deliberately *not* a class. It is a separate `boundaries` list,
so an open question can never be mistaken for a claim: `UNPROVEN_IDENTITY`
(a duplicate-ordinal seed, which is never traversed), `UNRESOLVED_REFERENCES`
(unresolved relations elsewhere that merely *name* the symbol),
`UNDETERMINED_PROPAGATION` (the node has dependents but propagation through it
is unproven), `DEPTH_LIMIT`, `FANOUT_LIMIT` and `ENTITY_ABSENT`.

### Seeds and change sensitivity

Seeds come from a Stage-3 `SemanticDelta`, or from named entities analyzed as a
prospective change. The delta's rule table is:

- an **entity change** always seeds that entity. A `MODIFIED` entity whose only
  changed field is `TEXT` is a *body* change; anything else (`SIGNATURE`,
  `VISIBILITY`, `KEY`, `ADDED`, `REMOVED`) is a *contract* change;
- a **removed** entity no longer exists in the analyzed generation, so the
  delta's removed relations targeting it are its traversal evidence — the
  dependents it used to have, reported with `removed: true` on the hop;
- a **relation change explained by a changed endpoint** creates no extra seed:
  seeding the caller of a deleted function would hide it inside the seed set
  instead of reporting it as impact;
- a relation change whose endpoints are both unchanged seeds its source, since
  the ontology says a fact about that source changed and nothing else explains
  it;
- a seed whose identity is `DUPLICATE_ORDINAL` is recorded but never traversed.

### Traversal and bounding

Traversal is a deterministic breadth-first walk of *incoming* resolved
relations, with a visited set (so cycles terminate and no entity is reported
twice) and explicit bounds on depth, seeds, items, tests, fan-out and
boundaries. Graph distance alone is never relevance. A hop past the first is
taken only when the ontology proves the previous node re-exposes the change:

- the node `IMPLEMENTS` the changed declaration; or
- the seed change was a contract change **and** the node's own declared
  visibility is provably exported.

Everything else stops and records `UNDETERMINED_PROPAGATION`. Unknown
visibility is never read as exported, so a body edit reaches its direct callers
and no further, while a signature change can travel through an exported relay.
Analysis is bound to one generation: a delta is analyzable only while the
generation it describes is the one the graph tables materialize, and a stale
request fails closed.

### Planning and review integration

`plan prepare` attaches a deliberately small `ImpactOutlook` for the primary
entities it selected, read against the request's own scope, so a planner can
see that a locally-correct edit has consequences elsewhere. It is advisory:
it never widens read or write scope, it carries no source text, it adds no
file to the request's provenance-bound support set, and it is the *first*
record shed under the byte budget, so a packet with impact never displaces
context a packet without it would have carried. Impact discovery is not
authorization; out-of-envelope source still goes through the Stage-2 relay.

`ontology impact --plan <id>` reads an observed delta against a plan's declared
write scope, which answers what a reviewer should check because of what
actually changed. It decides nothing, writes nothing, and leaves verifier
independence untouched.

## Engineering memory

Memory entries are immutable records with a trust class, kind, bounded content
(8 KiB), actor, provenance, and typed links. The links can point to graph
entities, files, tasks, plans, jobs, evidence, invariants, commits, other memory,
and tags.

| Trust | Source | Validity |
| --- | --- | --- |
| `CANONICAL` | explicit creation or promotion (a new entry; the original is unchanged) | durable decision; not hash-bound |
| `DERIVED` | only mechanical derivation from one indexed symbol | `STALE` when supporting files or versions change |
| `OBSERVED` | only from registered evidence | historical: what that evidence reported |
| `AGENT_NOTE` | attributed to a registered author job | fallible; excluded from planner input by default |

Trust never changes implicitly through retrieval, summarization, or reuse.
Supersession requires matching trust, kind, scope, and key. Active canonical keys
are unique, and rejected or superseded entries keep their payloads.

Project policy in `project.toml` is exposed as read-only `PROJECT_CONFIG`
projections with a configuration fingerprint. It is never copied into the
database.

Search uses SQLite FTS5 with deterministic normalization (camelCase, snake_case,
punctuation) and ranks by trust, exact phrase, then recency. Every retrieval is
bounded, both in candidates checked and in the compact-JSON byte budget for
context. Memory content is data, never instructions.

## Planning

```text
objective ─▶ plan prepare ─▶ PlannerPacket (frozen) ─▶ planner ─▶ ExecutionPlan
                                                                     │ import
                                                                     ▼
                                            VALIDATED ──activate──▶ ACTIVE ──▶ COMPLETE
                                                 │                    │
                                                 └── cancel / supersede ┘
```

- **PlanningRequest.** Records the objective, optional scope, constraints,
  done criteria, requested verification, invariant references, provenance, and the
  source baseline (HEAD, dirty flag, policy hash, graph version, hashes of the
  selected files).
- **PlannerPacket.** The persisted, frozen planner input. It combines the request,
  bounded graph context, trusted memory, the project policy snapshot, and exact
  source excerpts, and its serialized size is part of the packet. Every project
  invariant is attached as critical. Rereading the packet returns identical bytes.
  Excerpts are bound to the entities they show: primary implementation first,
  then up to two implementation neighbors, then up to two tests. Over budget,
  the least valuable material goes first: unresolved summaries, relations, test
  excerpts, neighbors, extra tests, memory, secondary excerpts, the last test,
  extra primaries, the last excerpt, and finally the last primary. Nothing is
  kept once what it refers to is gone. `plan context <id> --manifest` prints
  the packet's context manifest.
- **ExecutionPlan.** A PlanPacket plus metadata. The metadata holds one
  **verification contract** per task and a final **integration contract**. A
  verification contract is bound to the task's packet hash, requires an
  independent verifier, and lists memory references, exclusions, and non-goals.
  The integration contract is bound to the whole PlanPacket, requires all task
  verifications and the final diff and evidence, and states overall expectations.
  Hashes are BLAKE3 over compact JSON in declared field order.
- **Import.** Performs strict deserialization and protocol validation, then checks
  scope (within the request scope, clear of protected paths, `.git`, and symlink
  ancestors), references, source baseline, and contracts. Import produces
  `VALIDATED`, never `ACTIVE`.
- **Activation.** Revalidates the fresh index, source support, HEAD/dirty state,
  policy, invariants, and memory references. At most one plan per workspace can be
  `ACTIVE`.
- **Readiness.** Structural readiness means a task's prerequisites are all
  `VERIFIED`. It is not permission to run.
- **Replans.** Replacement plans get new plan and task IDs and explicit lineage.
  They can reference earlier `VERIFIED` work but never inherit its status.

## Runtime execution

### Sessions and agents

A planning request identifies an **EngineeringSession**, scoped to a repository
workspace, and replacement plans stay in the same session. Every job is a new
**agent instance** with a fresh provider conversation and recorded parentage:

- planners belong to the session;
- executors and integration verifiers belong to the planner, or to the runtime
  supervisor if there is no planner;
- packet verifiers are fresh children of their completed executor, with none of
  its conversation.

Only agentctl creates agents. Provider-internal agent spawning is disabled, and
there are no persistent or cross-session workers.

### Flow for an ACTIVE plan

1. **Adoption.** The runtime rejects plans with pre-existing manual jobs. It
   requires the prepared baseline and the current checkout to be clean and
   committed, with a fresh graph. It then captures a full source snapshot: a
   manifest of every tracked and non-ignored untracked file with content
   hashes and modes, the metadata (never the content) of individually ignored
   files, and HEAD and the Git index hash. Directories that the repository
   ignores as a whole, such as build output and dependency caches, are not
   walked. The snapshot is taken twice and the two captures must match. It is
   an integrity observation for drift and scope checks, not agent context:
   agents receive only bounded, scope-filtered context.
2. **Execution.** A workspace lock serializes every task and check within the
   workspace, so there are no concurrent writers and no automatic worktrees. For
   each ready task, the runtime checks readiness, routing, scope, drift, and
   concurrency, then launches an executor. The executor receives its TaskPacket
   and contract, invariants, constraints, and the **planner-authored context**
   described in [Context relay](#context-relay) — nothing the runtime chose on
   its own. It does not receive other tasks or any conversation. An executor
   that lacks context requests it through the relay instead of exploring.
3. **Capture.** After the executor exits, agentctl computes the actual diff
   (additions, deletions, content and mode changes) against the snapshot and
   checks it against the write scope. The executor's self-reported changed paths
   must match the diff. Out-of-scope changes are recorded as evidence, never
   accepted.
4. **Checks.** The project's canonical commands for the task's verification
   requirements run as sandboxed tool workers: read-only, offline, with no
   credentials. Each run produces evidence with timestamps, exit status, and
   external log hashes. A failing check blocks the task before any model can
   claim success.
5. **Verification.** A fresh verifier receives only the task, its contract,
   invariants, the actual diff, and captured evidence. It never sees the executor's
   response or transcript. Its decision is validated against the issued target and
   the unchanged source. Only `PASS` makes the task `VERIFIED`, and verified
   changes then refresh the index as the plan's ontology candidate before
   downstream context is built. The candidate is not yet accepted truth.
6. **Integration.** When all tasks are `VERIFIED`, the runtime computes the
   combined baseline-to-final diff, runs the integration checks, and launches a
   separate fresh integration verifier. Completion goes through the same guarded
   completion gate used by imported plans, and promotes the plan's ontology
   candidate in the same transaction (see
   [The acceptance boundary](#the-acceptance-boundary)).

### Context relay

The planner is the authority over what a worker initially sees. Two ideas are
kept apart:

- **`read_scope` is an authorization envelope**: the paths a task may *request*
  context from. It is not content, and a Directory scope injects nothing.
- **Issued context is actual visibility**: what a job was really given, byte for
  byte, recorded in its context manifest.

An executor's base context is materialized only by dereferencing planner
references, in the planner's own order:

| Planner reference | Issued |
| --- | --- |
| `graph_entities` | identity and structural facts, the entity's bounded definition excerpt, and its resolved relations one hop away *inside the envelope* as identity-only stubs (at most 8 per direction; the rest are counted, not named) |
| `read_scope` File entries | bounded file content |
| `write_scope` File entries inside the read envelope | bounded file content (a write target the executor must edit) |
| contract `memory_refs` | the memory entry, revalidated (active, visible, not stale) |
| `verification.requirement_refs` | the task's canonical checks and their argv |

No objective-derived graph search, no lexical memory search, and no
directory file-fill runs. Excerpts and files share one bounded budget, and
truncation is explicit.

A worker that cannot finish safely returns a typed **ContextRequest** (protocol
document `context-request`) instead of exploring: `status = BLOCKED`,
`failure.code = CONTEXT_REQUIRED`, no reported changes, and 1–16 items drawn
from a deliberately narrow vocabulary — `SYMBOL_DEFINITION`, `SYMBOL_BY_NAME`,
`SYMBOL_RELATIONS` (callers/callees), `RELATED_TESTS`, `NEIGHBORHOOD` (depth
≤ 2), `FILE_RANGE` (≤ 400 lines) and `MEMORY`. Failure text alone never means
this, and a request that reports edits is invalid. There is no repository
search language.

`runtime::context` resolves a request deterministically against the ontology
snapshot and the captured source — no model is involved in retrieving a
definition, callers, related tests, a neighborhood, a file range, or a memory
fact. Each item is answered, then judged:

- every path an answer touches is inside the envelope → **granted**;
- any path outside it → **escalated** to the planner (never granted silently);
- unresolvable (absent, stale, ambiguous name, not source, binary, range past
  end of file), or over a budget → **denied**, and the task blocks.

Issued source stays hash-bound, which matters while a captured change is not
yet indexed (only accepted work refreshes the ontology). Source text — a
definition excerpt or a file range — is checked against the captured snapshot
before it is issued, so asking for the definition of a symbol in the file the
executor just changed fails closed as `STALE` rather than returning text that
was never in the diff. Identity facts (relation, neighborhood and test stubs)
carry the content hash they were derived from and no source.

A grant becomes a **ContextDelta**: a hash-bound artifact carrying typed facts
and exact source only (no prose, no reasons), bound to the request hash, the
ontology generation, the source state, the requesting job and the round. The
run record keeps a per-subject **ledger** (`executor:<task>`, `verifier:<task>`,
`integration`) of every round: the request, its resolution, the delta, and any
planner decision.

Expansion never resumes a conversation. Each granted round launches a **fresh
provider job** whose input is the original base context plus the accumulated
deltas, with explicit lineage. Between rounds the ontology generation and the
captured source are revalidated, so a delta is never issued on stale
assumptions. An executor requesting context must leave the workspace unchanged:
a request with a non-empty diff fails closed, because this runtime has no safe
rollback primitive.

Everything is bounded by machine-owned maxima (see
[configuration.md](configuration.md#runtimecontext)): rounds per task, bytes per
round, cumulative bytes per subject, and escalations per task. A worker cannot
raise them by asking.

**Planner escalation.** An out-of-envelope request blocks the run with
`NEEDS_PLANNER_CONTEXT_APPROVAL`. `agentctl run context <plan-id>` prints the
request, the resolution, the paths outside the envelope and a decision
template; `agentctl run context decide` consumes an explicit decision document.
An approval names 1–8 read-scope additions, which are revalidated (policy hash,
source and generation, the planning request's own scope, protected paths,
symlinks, contract exclusions) and then re-resolved before any delta exists; the
task returns to `PLANNED` so `run resume` re-issues it. A denial leaves the task
blocked for a replan. The executor has no path to this decision: its own output
can never widen its scope.

**Verifier relay.** A verifier can also lack context, and its relay is
independent: its requests are derived from verifier-visible material only, it
never inherits the executor's requests, reasons or transcript, it has its own
round budget, and it cannot escalate to the planner (out-of-envelope requests
are denied). A verifier context request is neither PASS nor REJECT and never
becomes a task transition.

### Concurrency

`[runtime.concurrency] max_agents` (default 4, project may only lower it) is a
machine-wide hard ceiling on queued or running agent jobs. It is checked inside
the same write transaction that registers each job, so simultaneous controllers
in different workspaces cannot exceed it. Exhaustion fails the launch with
`AGENT_CAPACITY_EXCEEDED`. It is not a provider failure and never triggers
fallback. Finished jobs free capacity. Experiments are not agents.

### Drift, correction, and recovery

- Unexpected HEAD, index, content, or policy changes block with a `SOURCE_DRIFT`
  event before launch, acceptance, or completion. Project policy is hash-checked
  again immediately before each adapter launch and process spawn. Nothing is
  silently rebased, rolled back, or rewritten.
- Rejection or failure blocks the plan and returns control to planning. `run
  replace` links an explicit replacement plan (default at most two rounds,
  configurable down to zero). There is no automatic retry and no source-discard
  operation.
- `run resume` continues from durable artifacts. Pending checks and verification
  resume, `VERIFIED` tasks are never re-executed, and recorded verifier outputs are
  recovered without a new call. Queued or running jobs whose controller was lost
  become interrupted and block. Stored PIDs are diagnostic only; they are never
  reattached or killed. Crash gaps before a checkpoint can require manual review.
- agentctl never commits, pushes, resets, or cleans the checkout.

### Runtime limits

| Limit | Value |
| --- | --- |
| Files in a captured checkout | 20,000 captured source files (64 MiB of content in total); 20,000 individually ignored files (metadata only); depth 64 |
| Expanded verifier diff | 128 KiB; it carries hunks with 3 lines of context, so it scales with the change, not with file size |
| Issued base context | 64 KiB of source text in total; 6 KiB per definition excerpt; 16 KiB per issued file |
| Context request | 1–16 items, 1 KiB reason, ≤ 32 KiB requested |
| Context rounds and bytes | machine-owned: `[runtime.context]`, hard maxima 4 executor rounds, 2 verifier rounds, 32 KiB per round, 96 KiB per subject, 2 escalations |
| Compiled provider input | 256 KiB |
| Role process timeout | `runtime.timeout_ms` (default 10 minutes, maximum 1 hour) |
| Captured stdout/stderr | 4 MiB each |

Symlinks, hardlinks, nested repositories or submodules, special files, and
read-denied files among the captured source files make capture fail closed.
Capture never reads individually ignored files (it records their metadata only)
and never walks directories ignored as a whole, so none of these conditions
applies there. The snapshot also records a hash of the repository-local exclude
rules (`info/exclude`, as Git resolves it), so changing them fails closed as
`SOURCE_DRIFT`.

## Structural economy

`local::graph::footprint` is a pure, bounded projection of a `SemanticDelta`;
the CLI can compare any two recorded generations when the second is still the
indexed generation. Plan-linked and runtime footprints are narrower: they must
be the live plan-owned candidate's own accepted-base delta. The projection adds
no table, artifact, graph, score, or policy gate. File, entity, visibility,
identity, and resolved-relation records are the machine-inspectable evidence.
Production/test separation reuses the Stage-1
test-kind and path conventions and exposes that basis. Public surface is only
claimed where the extractor records visibility (currently Rust). Review signals
form a closed set and embed the exact facts that triggered them.

The report must name the exact two ontology generation points and the compared-
to generation must still be the indexed generation. Stale data fails closed.
Plan review adds declared write scope and plan-level verification references
only after the Stage-3 runtime ownership rule attributes the exact candidate to
that plan; neither changes the plan or proves that a particular entity was exercised. The
integration verifier receives a compact report before the Stage-3 acceptance
boundary. Planning receives none: before implementation there is no structural
delta to report, and speculative structure would be weaker than the existing
bounded context and impact view.

The analyzer abstains from configuration/persistence classification, semantic
duplication, unresolved import claims, non-Rust export claims, and per-entity
test coverage. A later policy may interpret its signals; Stage 5 itself never
accepts or rejects a generation and never grants context or filesystem access.
Analysis currently materializes the complete recorded delta-derived fact and
signal inputs before applying presentation limits. The 64 MiB `SemanticDelta`
artifact ceiling remains the outer bound; Stage 5 does not add a separate
streaming or analysis-budget mechanism.

## Routing and prompt compilation

`runtime::routing` is a pure policy layer: it reads no repository, database, or
network. It resolves a role through layered patches (built-in semantics, machine
`runtime.roles`, machine `runtime.profiles`, project `routing.profiles`, then
explicit `--override`) and applies hard project limits. Repository layers can only
tighten security-relevant fields. See [configuration.md](configuration.md#precedence-and-trust).

The result includes an ordered fallback chain of at most four alternatives. The
chain advances only on mechanical pre-launch failures, and each attempt is a new
job with identical permissions.

The routing decision is recorded on each job, so history never consults newer
configuration. That record contains the resolved route, fallback history, field
sources, and the profile, policy, prompt, and context hashes and byte counts.

Each job also records a **context manifest**. It describes what agentctl
intentionally supplied without copying any of it:

- role and job, plan, task, and request identity;
- graph version and generation;
- the repository paths and ranges supplied, each bound by content hash and
  labeled as an excerpt, file, diff, or graph facts only;
- graph entity, memory, and invariant identifiers;
- every issued repository item traced to the authority that selected it
  (`PLANNER_GRAPH_ENTITY`, `PLANNER_READ_FILE`, `PLANNER_WRITE_TARGET`,
  `PLANNER_MEMORY_REF`, `PLANNER_VERIFICATION_REF`, `CONTEXT_DELTA`) with its
  exact serialized bytes, so supplied bytes trace back to planner intent;
- the context round, and every ContextDelta issued to the job by ID, artifact
  hash, bytes, round, requesting job and whether a planner approved it;
- the repository read visibility the job ran with;
- exact byte accounting of the compiled provider input. Instructions, one
  category per context field, and JSON framing sum exactly to the prompt's
  bytes.

Provider-side system prompts, tools, and tokenization are marked
`NOT_OBSERVED` and never estimated. Manifests are deterministic, and building
one fails closed if its accounting does not reproduce the compiled prompt.

`runtime::prompt` combines a short role instruction delta, the configured
instruction fragments, and the bounded canonical input with the expected output
contract. Identical inputs produce identical bytes. If the complete prompt exceeds
its budget, compilation fails rather than dropping invariants. Token budgets are
advisory only.

## Provider adapters

A `ProviderAdapter` declares capabilities, launches a sandboxed process, collects
strict output, and optionally reports usage. The two adapters, `claude` and
`codex`, are thin argv builders around the installed CLIs, with native-first
authentication and fresh, non-persistent sessions. See [providers.md](providers.md).

Tests use injectable fake adapters and check launchers. The full
planner-to-integration flow therefore runs offline.

## Security

Every process launch (provider frontends, checks, experiments) is compiled into an
OS-neutral `SecurityPolicy` and checked against the host backend's capability
report. A launch is refused if any required capability is missing. The backends
are Seatbelt on macOS and Landlock + seccomp on Linux; the Windows backend refuses
worker launches. See [security.md](security.md).

## Experiments

An experiment is a plain OS process (training, benchmarking, simulation) that runs
under the same sandbox as checks. It has no provider and no engineering-session
ownership. Its layers are:

1. **Process runtime.** Durable experiment records and attempts, a foreground
   polling controller, timeouts (default 24 hours, maximum 30 days), cancellation,
   and restart. Persisted `RUNNING` is historical; only the polling controller can
   report a process as live.
2. **Structured events.** Programs append JSON lines to `AGENTCTL_EVENT_FILE`
   (`metric`, `checkpoint`, `health`, `status`). Frames are validated, capped, and
   idempotent under exact replay. Checkpoints are hashed file references. Events
   are append-only. See [cli.md](cli.md#structured-events).
3. **Decisions.** Frozen, hash-bound `METRIC_THRESHOLD` boundaries are evaluated
   deterministically over persisted metric events, using a durable per-attempt
   cursor. Evaluation has no process-control path and makes no model call.
4. **Planner wakeups.** A decision whose boundary action is
   `REQUIRE_PLANNER_REVIEW` mints exactly one planning request, bound one-to-one to
   the decision and limited by a per-experiment wakeup budget. Creating a wakeup
   never launches a provider. Planning, activation, and verification then follow
   the normal rules.

## Observability

`Store::observe` builds one bounded read-transaction projection that `agentctl
observe` and `agenttop` share. It contains the following:

- sessions and current plans;
- agent ownership trees;
- task DAGs with blockers;
- phases, compact events, and experiments;
- token usage series.

It never grants capabilities, opens provider artifacts, runs Git, indexes source,
mutates lifecycle, or calls a provider. Bounds are 64 plans, 512 recent jobs, 4,096
tasks, and 2,048 recent journal entries, with explicit truncation.

- **Liveness** is separate from lifecycle. `LIVE` requires an in-memory registry
  entry for the current controller, confirmed by its owned child handle. Any other
  observer reports `UNKNOWN`. No heartbeat is persisted.
- **Usage** is built from canonical token-delta events. Exact and estimated counts
  stay distinct, and cached or reasoning subcounts are never added again. The
  rolling graph samples a trailing 60-second sum every 10 seconds over 10 minutes,
  and empty buckets are unknown, not zero.
- **agenttop** is a Ratatui terminal UI over the same snapshot. `--once` renders
  deterministically to text for scripts and tests.

## Analytics

`local::analytics` reads the database read-only in a single snapshot and produces
a versioned, bounded descriptive snapshot. It covers token usage by dimension,
verifier decisions, reject and fallback rates with explicit denominators, route
provenance, policy skips, correction lineage, context sizes, and latency
distributions.

- **Attribution.** Usage is attributed through explicit ownership (workspace,
  session, agent, job, plan, task), never by timestamp proximity.
- **Unknowns.** Unknown values stay unknown. No pricing is assumed.
- **Privacy.** Raw prompts, artifacts, credentials, and transcripts are excluded.

## Known limitations

- Execution within a workspace is serialized. There are no managed parallel
  worktrees.
- Context expansion is deliberately narrow: typed items over ontology entities,
  literal paths and memory IDs. There is no semantic repository search, and a
  request for anything outside the task's read scope needs a planner decision
  rather than being resolved automatically.
- Verifiers cannot escalate to the planner; an out-of-envelope verifier request
  is denied and blocks the task.
- Planner escalation is consumed through an explicit decision document
  (`agentctl run context decide`). agentctl does not itself launch a planner job
  to answer an escalation.
- Hard issued-context visibility is opt-in; the default keeps the workspace
  readable (see [security.md](security.md#issued-context-visibility)).
- The code graph is syntactic. Cross-file resolution covers Rust qualified
  paths only (no import or re-export following, macros, or type inference).
  Python and TypeScript relations resolve only within a file, and method calls
  on variables stay unresolved.
- Ranking scans a workspace's entities twice per query. A changing index pass
  rebuilds workspace resolution in time proportional to path-hinted relations.
- Only Claude Code and Codex CLI adapters exist. Codex usage is unknown, and
  usage arrives at job end.
- Worker execution requires macOS or Linux with the required sandbox primitives.
- Source observations are sequential, not atomic. Local authority is not user
  authentication.
- Runtime artifacts and ontology snapshots are retained indefinitely; there is
  no garbage collection.
- Semantic deltas are syntactic. They say what changed in the extracted facts,
  not what a change could affect, and they make no rename or move claims.
  Imports, unresolved call sites and macro-generated items are not delta facts.
- Snapshotting reads every entity row on each index pass that records a new
  generation (O(entities)).
- The first complete index of a workspace is accepted without review, and so is
  a re-observation whose facts equal the accepted generation.
- Repository relocation is not tracked.
