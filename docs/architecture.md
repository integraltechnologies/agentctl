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
| Code graph | `src/local/graph/` | Tree-sitter extraction, incremental indexing, bounded queries |
| Memory | `src/local/memory/` | trust-classified engineering memory with provenance |
| Planning | `src/local/planning/` | planning requests, planner input, plan import, lifecycle, completion gate |
| Runtime | `src/local/runtime/` | engine, routing, prompt compilation, provider adapters, source capture, experiments |
| Security | `src/local/security/` | OS-neutral policy, capability checks, platform backends, process-tree cleanup |
| Observe | `src/local/observe/`, `src/local/agenttop.rs` | read-only live projection and terminal UI |
| Analytics | `src/local/analytics/` | read-only historical metrics |

## Protocol

The public contracts are the documents that pass between agentctl and planners,
executors, and verifiers, plus the durable records built from them. There are 13,
each with a generated JSON Schema in `schemas/`: plans, tasks, results,
verification, resume, evidence, jobs, agent events, probes, token usage,
experiments, experiment events, and memory provenance.

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

**Migrations.** The current schema version is 11. Migrations are additive and
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
  source bound to a generation the workspace has not reached.
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
   and contract, invariants, constraints, and bounded scope-filtered graph,
   memory, and source context. It does not receive other tasks or any
   conversation.
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
   the unchanged source. Only `PASS` makes the task `VERIFIED`, and accepted
   changes then refresh the graph before downstream context is built.
6. **Integration.** When all tasks are `VERIFIED`, the runtime computes the
   combined baseline-to-final diff, runs the integration checks, and launches a
   separate fresh integration verifier. Completion goes through the same guarded
   completion gate used by imported plans.

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
| Expanded verifier diff | 128 KiB |
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
- exact byte accounting of the compiled provider input. Instructions, one
  category per context field, and JSON framing sum exactly to the prompt's
  bytes.

Provider-side system prompts, tools, and tokenization are marked
`NOT_OBSERVED` and never estimated. `context_deltas` is reserved and empty.
Manifests are deterministic, and building one fails closed if its accounting
does not reproduce the compiled prompt.

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
- Runtime artifacts are retained indefinitely; there is no garbage collection.
- Repository relocation is not tracked.
