# Architecture contract — Stage 3, preserving protocol v1 and Stage 1/2 foundations

## Ownership and roles

`agentctl` owns canonical engineering state at machine scope. Provider conversations
and harnesses are replaceable clients. This project must not become a Claude wrapper,
a Codex wrapper, another autonomous coding agent, a chat-memory store, or a giant
swarm framework.

| Role | Responsibility | Context contract |
| --- | --- | --- |
| Planner | High-compute understanding of the objective; creates the task DAG and later targeted corrections | User objective, canonical invariants, future repo graph/shared memory |
| Executor | Lower-compute implementation of one bounded packet | Packet plus the minimum necessary context; only assigned work |
| Verifier | Independent lower-compute assessment of a packet or integrated plan | Fresh context containing objective, invariants, diff/source state, and evidence; never executor reasoning |

No role selects a provider or model. Optional `ProviderMetadata` contains opaque
strings on jobs and observations only. Adapter-specific policies and tool syntax
belong outside the core. The planner should eventually choose the smallest coherent
set of specific packets that maximizes safe parallelism while minimizing frontier
tokens, critical-path time, repeated context, uncertainty, merge risk, and coordination
overhead. Neither Stage 0 nor Stage 1 contains an optimizer or scheduler.

## Packets and verification

`PlanPacket.tasks` is nonempty. Each task owns its dependency list: there is no second
edge list that could disagree. IDs are unique within the plan; missing dependencies,
self-dependencies, repeated dependencies, and cycles are invalid. Array order does
not imply execution order.

`TaskPacket` carries an objective, explicit read/write scope, graph entity references,
critical invariant references, dependencies, definition of done, and mandatory
verification requirements. Scopes are normalized repository-relative files or directory
subtrees, without globs, absolute paths, or `.`/`..` components. An empty scope grants
no access; read and write permissions are separate. Paths are lexical contracts;
future execution must enforce canonical-path/symlink containment and protected-data
policy. Graph references do not grant extra access. Packets contain current deltas
and compact references, not repository prose, source dumps, or provider instructions.

The task state graph is:

```text
PLANNED -> READY -> EXECUTING -> AWAITING_VERIFICATION -> VERIFYING -> VERIFIED
                                                                  -> REJECTED
Any nonterminal work state -> BLOCKED -> PLANNED (explicit planner reassessment)
```

Only `VERIFIED` means complete and satisfies dependencies. Becoming `READY` and
starting execution both require verified prerequisites. Missing prerequisite state
never counts as completion. Executor `ResultStatus::Succeeded` reports an outcome;
it does not verify a packet. Results report changed paths/entities, evidence, concise
notes, and explicit failure/blocker details, with no reasoning field.

Every task receives an independent verifier. `VERIFYING -> VERIFIED` requires a valid
matching packet-level PASS covering the packet's required checks and critical invariants,
plus evidence when required. `VERIFYING -> REJECTED` requires a matching rejection with
structured findings. BLOCKED decisions explain why verification could not proceed.
PASS cannot contain ERROR or CRITICAL findings. Setting `evidence_required: false`
does not disable verification or required checks.

VERIFIED and REJECTED are terminal for that packet. Rejection returns to the planner
for a new correction/delta packet and a revised plan with explicit dependency changes;
there is no autonomous executor/verifier retry loop. Treat issued packet IDs as immutable
identities. A revised packet or plan gets a new ID; v1 has no automatic migration,
supersession, or dependency-rewrite machinery. Blocked work can be reassessed through
PLANNED, but has no direct path back to execution or completion.

`VerificationTarget` is a tagged union: PACKET contains a task and executor job ID;
INTEGRATION contains a plan and contributing executor job IDs. A verifier job must
differ from those executor jobs. Even after every packet is VERIFIED, plan completion
requires an INTEGRATION PASS for that plan, covering integration requirements and the
union of critical task invariants. Packet verification alone can never complete a plan.

Job state describes an invocation, independently of task acceptance:

```text
QUEUED -> RUNNING <-> WAITING
QUEUED -> CANCELLED
RUNNING or WAITING -> SUCCEEDED / FAILED / CANCELLED
```

Terminal job states cannot restart. Waiting can describe dependencies, external jobs,
or required input. A successful executor job is not a verified task. An executor job
must reference a task; planner and integration verifier jobs may reference only a plan.

The protocol library implements pure validation. Structural transition predicates
alone are insufficient: callers must use contextual plan guards. Stage 1 storage
loads state from SQLite, invokes those guards, and atomically records transitions
and their journal entries. A verification decision must reference distinct registered
executor/verifier jobs with the correct roles, task, and plan; both jobs must have
finished successfully, and referenced evidence must exist in the same repository.
The decision packet is preserved in the transition journal entry.

Registration is not authentication. The future runtime must authenticate authorship,
require fresh verifier context, bind the executor job to actual results, and resolve
checks/evidence against the exact source/diff. A resume document is a summary, never
authority to bypass these checks. Stage 1 does not launch verifiers or implement plan
completion/scheduling; the accepted integration-completion guard remains available.

## Durable facts and observations

`ResumePacket` describes the active plan/task, lifecycle phase, verified completed work,
pending work, latest verification reference, next action, and optional source state.
It is current state, never transcript replay. Resume completion requires an integration
verification reference, whose target and decision must be resolved by future persistence.

`EvidenceRef` is a compact typed ID. `EvidenceRecord` can describe command/argv/cwd,
source revision and dirty-content hash, timestamps, exit status, stdout/stderr hashes,
full-log reference, and summary. Hash strings use an algorithm prefix, such as
`sha256:<digest>`. Log/source references are opaque, not automatically fetched or
executed. `CommandSpec` is argv, not an implicit shell string. Full logs remain outside
packets. Stage 1 stores compact metadata and references; it captures no command output.

Memory provenance distinguishes CANONICAL (explicitly governed authority), DERIVED
(computed from referenced sources), OBSERVED (recorded evidence), and AGENT_NOTE
(agent-authored suggestions). Agent notes require author attribution. AGENT_NOTE must
never silently acquire canonical authority through retrieval, summarization, or reuse;
promotion requires an explicit, auditable policy decision. Stage 0 only defines this
contract; Stage 3 supplies persistence and explicit promotion without changing the schema.

Agent events describe observable actions: starts/finishes, loaded packets, steps,
files/symbols, tools/commands, checks/findings, dependencies, experiments, and token
observations. Event IDs identify observations; tool/command invocation IDs pair starts
and finishes. `ProbeSnapshot` represents current role/task/packet/job, opaque provider,
phase/step/target/tool/command, last event, elapsed/idle time, blockers/dependencies,
and verification check. Neither events nor probes require hidden chain-of-thought.

All timestamps are Unix epoch milliseconds; durations explicitly end in `_ms`.
Token counts are optional unsigned per-observation deltas. Provenance is mandatory:
EXACT means reported exact counts, ESTIMATED means estimates, and UNKNOWN carries no
counts (including no invented zero). Missing counts are not zero. Cached/reasoning
subcounts may overlap input/output totals, so consumers must not blindly sum them.
Event IDs enable future deduplication; no collector or aggregation exists yet. In v1
task and packet identify the same work unit: if both context IDs appear, they must
agree. Nested usage observations must agree with their event envelope.

`ExperimentSpec` references inputs/source state, command, metrics/outputs, and named
decision boundaries: process exit, crash, no progress, NaN metric, epoch completion,
or a configured metric threshold. `ExperimentEvent` references the reached boundary
and carries observations/evidence. Thresholds and metric values must be finite JSON
numbers; NaN is an explicit boundary, not a JSON number. Boundary-to-spec resolution,
job detachment, decisions, retries, and execution are future responsibilities.

## Configuration hierarchy

| Scope | Default path | Contents |
| --- | --- | --- |
| Machine configuration | `~/.config/agentctl/` | Stable cross-repository workflow policy |
| Durable machine-local data | `~/.local/share/agentctl/` | Canonical repository/task/job/evidence/event/memory state; derived code graph; future resume state |
| Reconstructible cache | `~/.cache/agentctl/` | Disposable derived data; never the only copy of canonical state |
| Project configuration | `repo/.agentctl/project.toml` | Product invariants, architecture constraints, repo commands, protected-data rules, canonical verification definitions |
| Task packet | Managed protocol document | Current bounded work delta and references |

One `MachinePaths` resolver reads `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and
`XDG_CACHE_HOME`. Absolute XDG values win; relative/empty values use HOME defaults.
An absolute HOME is needed only for paths without absolute overrides. Traversal in
absolute paths is rejected. Resolution is pure and accepts injected `PathContext`;
tests never initialize the developer's actual home. The database is `state.sqlite3`
under the data root. Cache is never authority for durable engineering state.

Machine TOML requires `version = 1` and `busy_timeout_ms` (1–60000; default 5000).
It has no speculative provider, planner, data-directory override, or graph settings.
Project TOML requires `version = 1`; its optional display name and empty-by-default
collections specialize repository facts within machine policy:

```toml
version = 1
display_name = "Example"

[invariants.compatibility]
description = "Preserve the public input format"
[architecture.local]
description = "No network services"
[commands.unit]
program = "cargo"
args = ["test"]
cwd = "."
[[protected]]
path = "data/private"
deny_read = true
deny_write = true
reason = "Private data"
[verification.unit]
description = "Run unit checks"
command_refs = ["unit"]
```

Declaration keys are compact IDs. Verification command references must resolve to
project commands; command working directories are repository-relative (`.` is allowed).
Protected paths denote a path/subtree. These are declarations, not enforcement or
execution in Stage 1; Stage 2 discovery excludes `deny_read` paths. Future task execution must resolve canonical checks and cannot
weaken protected-data or machine policy. Both config types reject unknown fields,
missing required values, invalid contents, and unsupported versions with file context.

Initialization publishes a fully written, synced default through an exclusive hard
link from a temporary file, then removes the temporary name. Existing files are read
and validated, never replaced. A crash can leave an orphan temporary name but cannot
publish a partially written config. New machine/project directories use mode 0700
and new config/database files 0600 on Unix. Existing permissions are not silently
changed. Owned directory/file symlinks, special files, and multiply-linked database
files are rejected, including SQLite sidecars. Selected ancestor aliases (such as
macOS `/var`) are resolved before SQLite's NOFOLLOW open. This is conservative local
filesystem handling, not a sandbox against another process able to replace ancestors.

## Repository identity and source observations

Stage 1 discovers non-bare Git checkout roots through local Git commands. It computes
`RepositoryId` (`repo-<sha256>`) from a fixed `agentctl-local-git-v1` domain prefix and
the canonical **common Git directory** path bytes (`git rev-parse --git-common-dir`).
`WorkspaceId` (`workspace-<sha256>`) uses a separate `agentctl-workspace-v1` domain and
the canonical per-worktree Git directory (`git rev-parse --absolute-git-dir`). Existing
Git discovery already obtains both paths; no Git library or extra discovery process is
introduced for the identity split. These are logical local repository identity and
concrete checkout identity respectively. Discovery does not create an ID file in `.git`, use
directory basenames, consult a network, or derive identity from upstream providers.
No remote is required; remote names/URLs are opaque, refreshable metadata.

- Repeated discovery of a workspace and remote changes preserve both IDs.
- Main and linked worktrees share one repository ID, but have different workspace IDs.
  Repository-level plan/task ownership and Stage 3 durable engineering memory are shared.
  Repository-level graph IDs reuse the same repository key, while concrete graph rows
  remain workspace-specific. Orchestration remains future work.
- Independent clones remain distinct logical repositories, even with identical commits
  and remote URLs. Matching directory basenames imply neither identity.
- Moving a primary repository normally moves its common and per-worktree Git directories,
  producing new local IDs. Robust relocation is intentionally deferred in Stage 1. The
  old registration remains available; there is no silent rekeying or move search.
- `git worktree move` normally preserves the common and administrative Git directories
  and therefore both IDs.
  Registration may update the root only when the previous root no longer exists;
  previous roots remain recorded. Ambiguous root reuse is rejected.
- Unix device/inode metadata detects replacement of the registered common or workspace Git directory
  at the same path. Such replacement is reported and rejected, not merged. This is
  diagnostic protection, not a distributed identity or proof against inode reuse.

One logical repository registration stores its common directory, replacement-detection
metadata, remotes, and first/last observation times. Each workspace registration stores
its repository ID, canonical filesystem root, per-worktree Git directory, diagnostic
metadata, prior roots, and its own source observation. `repo status` exposes both IDs
and the current workspace root. `repo list` groups workspaces under one logical repository
and reports unavailable, changed-identity, or changed-metadata workspaces without
mutating them. Repo status reports conflicts as failures. `repo init` refreshes safe
metadata; no automatic deletion, collision repair, or merging of old state is offered.
Stage 1 metadata requires UTF-8 filesystem paths and Git on PATH.

`RepositorySourceState` is a local envelope containing repository ID, **workspace ID**, optional HEAD
commit, dirty flag, observation timestamp, and `worktree_fingerprint = null`. HEAD is
absent for an unborn branch. Dirty includes changes reported by Git, including
untracked files and submodules; ignored artifacts are not covered. These are sequential
observations and can race external edits. Even clean status is not an exact filesystem
snapshot. No whole-tree hashing or automatic conversion of dirty state into an exact
evidence binding is performed. The accepted `SourceStateRef` schema is unchanged.

## SQLite ownership, migrations, and journal

Synchronous `rusqlite` with bundled SQLite owns local state. There is no daemon,
external database, async runtime, ORM, or network dependency. Writable connections
enable foreign keys, a bounded busy timeout, WAL, and `synchronous=FULL`. Writers use
`BEGIN IMMEDIATE`; readers may coexist with writers. Keep the database on a local
filesystem that supports SQLite locking. Backups must capture a consistent SQLite
snapshot, including committed WAL contents; copying only the main file while active
is not a backup procedure.

Database schema version 3 uses SQLite `application_id`, `user_version`, and a small
`schema_migrations` table. Migrations run in one transaction.
Concurrent/repeated opens recheck the version inside the transaction. Future versions,
foreign/unversioned nonempty databases, and missing migration metadata/required tables
or append-only guards fail clearly; startup never guesses or destructively repairs.
Normal writable opens apply known migrations. Status/doctor open existing state
read-only and do not initialize or migrate it. Doctor checks directory/config access,
permission bits, SQLite quick-check, foreign keys, schema version, and migration metadata.
It does not claim to authenticate stored facts or repair corruption.

The v1→v2 migration groups prior per-checkout repository registrations by their stored
common directory, creates workspace records, and rekeys repository-owned rows. Primary
repository IDs remain unchanged. Jobs, evidence, and historical events retain their
original checkout association via workspace IDs. Migration is offline: unavailable roots
do not prevent conversion. Common-directory device/inode information not captured by v1
linked-worktree records is populated on subsequent discovery. Protocol JSON and journal
payload bytes/sequences are preserved; historical event envelopes retain the old ID as
`legacy_repository_id`, so embedded v1 observations remain interpretable. Only indexed
ownership/location metadata is migrated. Conflicting repository-scoped IDs abort the
entire migration with an actionable error, preserving v1 state; automatic collision
resolution is deliberately not provided. Foreign keys are checked before commit and
append-only guards are restored within the transaction.

Tables contain logical repositories, their registered workspaces, immutable plans, task lifecycle rows, jobs, compact
evidence, and events. The small plan table is necessary to validate task dependencies
using the accepted `PlanPacket`, not a scheduling subsystem. A full plan is inserted
atomically with all its tasks PLANNED; a one-task plan creates an individual packet.
The immutable plan JSON is the sole stored TaskPacket definition. Task rows add only
ownership and lifecycle state. Jobs/evidence/AgentEvents store their accepted JSON
representations, validated on ingestion and decoding. No parallel packet model exists.
IDs are repository-scoped in storage; a task ID has one immutable owning plan, so a
conflicting plan registration fails rather than silently reusing or changing work.
Jobs, evidence, and local event envelopes have an optional workspace association outside
the unchanged Stage 0 wire schemas. Workspace-aware storage APIs validate that the
workspace belongs to the repository; job transitions and job-associated events retain
the job's location, and conflicting explicit event locations are rejected. Repository-only
APIs do not guess a checkout. Workspace association identifies where an observation or job
belongs; it does not turn a dirty flag into exact diff/evidence binding. Existing
`SourceStateRef` validation still applies. Workspace registration is identity/storage
groundwork, not orchestration, worktree creation, or job execution.

New jobs start QUEUED; their roles/targets/provider metadata remain fixed while state
and timestamps change through validated transitions. Task/job updates take an expected
current state, reread within the write transaction, and reject stale writes. Inserts
and state changes append corresponding journal records in that same transaction. If
validation, a constraint, or journal insertion fails, the entire operation rolls back.
Verification cannot commit without the durable decision-bearing transition record.

The events table is an append-only **local journal**, versioned with the database.
It wraps unchanged `AgentEvent`s and separately records storage facts such as task/job
state changes. No Stage 0 enum variants were repurposed or added. A global committed
AUTOINCREMENT sequence defines append order; timestamps may arrive out of order.
Provider event IDs are unique per repository, and duplicates are rejected. Repository,
plan, task/packet, and job associations are indexed and checked against durable owners;
event queries return the most recent matching entries in ascending sequence order.
UPDATE/DELETE triggers protect history even from accidental direct SQL changes.
This is not tamper-proof storage against a database owner who can drop triggers.

Agent observations do not themselves mutate lifecycle state; journal transition entries
are authoritative for those changes. Future adapters must preserve that distinction.
The journal is not replay-based event sourcing: SQLite snapshots and history are
transactionally coupled, with no streaming service or autonomous decision process.

Evidence metadata and ordinary incoming events/jobs are capped at 64 KiB; plan and
local journal documents at 4 MiB. Full-log locators are compact references, not data
URIs or embedded multiline output. Large logs/artifacts belong under
`data/artifacts/<repository-id>/<evidence-id>/`; repository cache namespaces live under
`cache/<repository-id>/`. These path helpers create no artifacts. There are no graph,
shared-memory, result-runner, or experiment tables in Stage 1.

## Wire compatibility and implementation boundary

Every public packet/event document carries `"version": "1"`; nested supporting values
inherit their enclosing version. Enums use SCREAMING_SNAKE_CASE and fields snake_case.
IDs are distinct Rust newtypes serialized as 1–128 ASCII characters matching
`[A-Za-z0-9][A-Za-z0-9._:-]*`. IDs are caller-issued; no UUID dependency is necessary.
Optional fields accept omission or null and serialize as null when absent.

Unknown fields, enum variants, and versions fail closed. Incompatible fields or
semantics require a new wire version and deliberate migration; the crate version is
separate. Do not rely on consumers ignoring new fields. Rust definitions generate
self-contained Draft 2020-12 schemas; the lockfile pins generator dependencies and
tests compare exact generated bytes with `schemas/`. JSON Schema validates structure;
`Validate` adds DAG, nonblank-content, reference consistency, and other semantic checks.
Consumers must run both deserialization and semantic validation before accepting data.

One crate contains the unchanged protocol foundation plus local paths/config, Git
discovery, SQLite migration/storage, and CLI dispatch. Stage 1 adds `init`, `doctor`,
repository init/status/list, state status, and event listing; status/list diagnostics
support trailing `--json`. Stage 2 adds the graph described below. No provider adapter, scheduler,
network stack, daemon, TUI, ML runner, or agent loop is present. Future stages must
preserve these ownership and verification boundaries.

## Stage 2 backend decision and extraction boundary

The bounded backend evaluation selected direct [Tree-sitter's Rust API](https://tree-sitter.github.io/tree-sitter/using-parsers/)
with pinned Rust 0.24.2, Python 0.25.0, TypeScript/TSX 0.23.2, and JavaScript 0.25.0
grammars, using Tree-sitter 0.25.10. Their package manifests declare MIT licenses;
the lockfile pins the build. This reuses mature parsers with no runtime service or
source execution. BLAKE3 supplies content hashing and `ignore` supplies Git-style walking.

[Serena](https://github.com/oraios/serena) offers richer LSP-backed resolution but adds
language-server processes/toolchains and a different runtime boundary. [Aider's repo map](https://aider.chat/docs/repomap.html)
is a useful compact-context precedent, but its application-oriented ranking/runtime is
not the storage/provenance API needed here. Unspecified CodeGraph/atlas-like products
did not provide a concrete reusable local Rust API in this evaluation. Rust-native
`syn` was available locally, but language-specific parser stacks would duplicate the
multi-language boundary. No external graph service, compiler integration, or speculative
parser-plugin system is introduced. The small internal extraction adapter turns parsed
syntax into language-neutral entities/relations; the core never uses Tree-sitter nodes.

Rust extraction covers inline/external module declarations, functions, methods, structs,
unions/type aliases, enums, traits, impl containers, constants/statics, `use` statements,
type references, calls outside opaque macros, and literal `#[test]` attributes. Python
covers modules, classes, nested functions/methods, imports, bases, and calls. Functions
named `test_*` in `test_*.py`/`*_test.py` files or `Test*` classes are test candidates.
Decorated definitions retain their underlying declarations; decorators are not executed.
TS/JS covers modules, functions, classes/methods, interfaces/type aliases/enums where
applicable, arrow/function-valued variables, imports/re-exports, calls, and TS implements
clauses. Literal `test`/`it` calls in `.test.`/`.spec.` files are test-convention candidates.
These conventions do not certify framework binding, collectability, or execution.
HTML, CSS, C, config/data files, and other unsupported extensions are omitted, not parsed
as another language. A new language adds one concrete adapter and a backend version.

## Graph ownership, identity, and provenance

Existing Repository/Workspace registrations are the graph roots; their ownership keys
are reused rather than duplicated as invented graph IDs. File, synthetic file-module,
and symbol entities carry `GraphEntityId`, kind, name, lexical qualified name, parent,
byte range (half-open), 1-based lines, optional Rust visibility, and a deterministic
signature capped at 240 characters. No source blobs or generated prose are stored.
Kinds distinguish file, module, function, method, type, enum, trait, constant, test, other.

Entity IDs are domain-separated BLAKE3 hashes of logical repository ID, normalized path,
language, kind, lexical qualified name, and duplicate-name occurrence ordinal. They do
not depend on lines/content hashes, so ordinary body edits and line insertions preserve
identity. Names are compacted to 160 characters; impl-header changes, renames/moves,
kind/container changes, and reordering otherwise identical duplicate declarations can
change identity. These are syntactic identities, not compiler USRs or relocation-proof IDs.
Qualified names use `::` for lexical containers in all languages and include file context;
they are not claims about language import paths. Edge IDs include source range/occurrence
and are derivation-specific, not promised stable across edits.

Every file-derived entity and edge includes repository ID, workspace ID, relative path,
`blake3:<content digest>`, language, grammar/parser version, and extraction/index version.
`indexed_files` stores the same supporting hash/backend (or no hash on a read failure).
Edges are supported by their source file. Resolved targets are currently within that
same file, so file-level invalidation cannot strand cross-file dependencies. A future
cross-file resolver must track and invalidate target-dependent derivations too.

IDs may coincide across linked workspaces when path/symbol identity agrees, but graph rows
are keyed by **workspace plus entity ID**. Hashes, ranges, edges, HEAD observations, and
freshness never leak between workspaces. Every public query requires a registered
concrete workspace. There is no repository-wide merged source snapshot. Identical files
are reused within a workspace, not yet deduplicated across workspaces; this is a deliberate
cache-efficiency limitation, not a loss of source isolation. Clone/move identity semantics
remain those of Stage 1.

Relations are `CONTAINS`, `IMPORTS`, `CALLS`, `REFERENCES`, `IMPLEMENTS`,
`TEST_RELATED_TO`, and `DEPENDS_ON` (external Rust modules/JS re-export syntax).
Containment and test-to-lexical-container links have known endpoints. Only explicit
Rust `self::name` paths to one compatible declaration in the same lexical module are
resolved for calls/type references/trait implementations. All other extracted relations
have `target = null` plus a compact syntactic target name. Bare names, imports, method
dispatch, and Python/JS calls are never joined globally by name. Macro expansion, cfg
evaluation, type checking, name rebinding, cross-file imports/calls, and semantic execution
are absent. Related tests are lexical candidates, not proof they verify a symbol.

## Incremental storage and freshness

The additive v2→v3 migration creates four tables: `graph_indexes`, `indexed_files`,
`graph_entities`, `graph_edges`. It changes no Stage 0 packets or existing stored payloads.
Composite foreign keys constrain workspace ownership and cascade file-fact deletion;
name, qualified-name, file, outgoing-edge, and incoming-edge indexes support lookup.
Migration conflicts roll back without advancing the version. Read-only inspection still
does not migrate. Graph updates use the existing IMMEDIATE transaction and append-only
journal function; one `INDEX_COMPLETED` local event carries measurable aggregate counts,
including failures, and workspace association. Detailed failures live in file status.
No per-file success spam or speculative percentage events are emitted. Failed database
transactions publish neither graph updates nor completion events.

Each index pass discovers files in deterministic order and hashes their bytes. Matching
hash plus parser/index version reuses existing rows without parsing. New/changed files
replace their complete derivation; deleted, ignored, or newly excluded files cascade out.
Parser/index version changes invalidate affected files (global index version changes
invalidate all). A read, encoding, limit, or parse error deletes previous file facts and
persists a diagnostic instead; failed files are retried on the next pass. Successful files
still commit in a partial index, and the CLI returns nonzero. Discovery/policy failures
or SQLite/journal failures roll back the whole pass. A final discovery/hash check rejects
known mid-index changes rather than publishing a knowingly mixed observation.

`repo index --status` reports last index time/source, expected index version, stored backend
version counts, stale paths, failures, and graph sizes. File provenance contains exact backend
versions. Queries re-discover and rehash current source in a single-use SQLite read
snapshot; a new/changed/deleted/version-stale path refuses the entire query with refresh
instructions. Partial but otherwise current indexes return only successful-file facts,
with `fresh = false`, full failure count, and at most ten sampled diagnostics in query
context. Status retains the full diagnostic lists. An empty, successfully indexed scope
is valid. Filesystem checks are sequential observations, not atomic snapshots; external
edits after checking are possible. HEAD/dirty are context, not the hash trust basis, and
none of this establishes exact Stage 0 evidence/diff binding.

Unchanged passes still pay discovery/content-read costs; parsing and row replacement
scale with changed files. No mtime-only trust, persisted syntax trees, cross-workspace
content cache, filesystem watcher, or background refresh is implemented.

## Bounded discovery and context queries

Discovery honors workspace `.gitignore`/`.ignore` rules, but not ancestor/global Git
excludes, to avoid hidden machine-dependent scope. It excludes `.git`, `.agentctl`,
symlinks, nested repositories, project `deny_read` paths, and `target`, `node_modules`,
`.venv`, `vendor`, `dist`, `build`, `__pycache__` directories. Ignore-policy errors fail
closed. Unsupported extensions are skipped; binary/NUL/non-UTF-8 supported sources are
recorded as failures. Paths use Stage 0 normalized relative-path validation. Bounds are
20,000 supported files, 100,000 visited entries, directory depth 64, 2 MiB per source,
200,000 syntax nodes, extraction depth 128, 10,000 entities/20,000 edges per file, and a
2-second parser cancellation budget. Limits are explicit failures, never silent truncation
of file derivations. Per-file processing bounds memory instead of loading all sources.
Sources are never executed; no package manager, build script, language server, network,
LLM, provider runtime, or repository instruction is invoked. Symlink metadata checks
and Unix NOFOLLOW/nonblocking source opens are conservative protections, not a sandbox
against a concurrent process replacing ancestor directories.

Exact/qualified/ID lookup, prefix/substring search, file lookup, incoming/outgoing typed
relations, related tests, and bounded neighborhoods are reusable library APIs. `locate`
streams stored entities and ranks exact symbol/ID matches (1000), normalized name tokens
(100 each), container tokens (40), path tokens (25), signature tokens (5), with a one-point
symbol-over-container preference; each token takes its strongest signal. CamelCase,
acronyms, snake_case and punctuation are normalized deterministically. Ties use path,
qualified name, then ID. Substring/lexical location is a streaming scan; no claim of
sublinear full-text search is made. Exact and adjacency queries use SQLite indexes.

`context` returns ranked primaries, bounded containers/neighbors/relations, lexical test
candidates, ranges/signatures and provenance—not source concatenation. Defaults are five
primaries, depth one, twenty neighbors/eighty relations and eight tests; hard limits are
ten primaries, depth three, one hundred neighbors/four hundred relations, twenty tests.
Ordinary result limits are 1–100, queries 1–512 bytes (location: at most 32 tokens).
Count truncation is indicated. Graph/ranking data is deterministic for identical inputs;
freshness observation timestamps and measured indexing durations naturally vary.
`impact` requires one unambiguous symbol and traverses known incoming structural edges,
excluding ownership containment. It reports known dependents, not everything a change
will break. `refs`/`callers` only report resolved endpoints; absent edges are not proof of
independence. Memory respects these precision, coverage, freshness, and scope limits.

## Shared engineering memory: authority and history

Stage 3 adds local `MemoryEntry`/`MemoryView` types, not new Stage 0 wire contracts.
An entry has an opaque random `memory:<128-bit hex>` ID, RepositoryId, optional WorkspaceId,
creation workspace/time, kind, bounded text, actor, unchanged Stage 0 MemoryProvenance,
origin, optional opaque provider metadata, typed links, and optional derivation/observed
source. The view adds ACTIVE/SUPERSEDED/REJECTED status, update time, replacement ID,
validity explanation, and unresolved graph/file links. Kinds are invariant, architecture
decision, constraint, finding, observation, limitation, task/experiment conclusion, note,
or other. Kind does not confer trust.
Content is limited to 8 KiB, actor labels to 128 bytes, links to 64, and complete stored
metadata to 64 KiB. Oversized input/observations fail explicitly instead of being silently
truncated or interpreted.

- CANONICAL requires explicit creation or promotion. Authority is a local action, not
  authenticated identity or proof of correctness. No inference or query changes trust.
- AGENT_NOTE requires a registered author job, preserving Stage 0 attribution rules.
  Its job/plan/task references and opaque provider/model metadata are copied as provenance.
  It is fallible, even when DURABLE means its validity does not depend on file layout.
- DERIVED can only be created mechanically from an unambiguous indexed symbol, signature,
  and the first twelve syntactic outgoing relations. Target spellings are not semantic
  dependency assertions. The entry records supporting graph IDs, file/hash/backend
  provenance, workspace, and `graph-memory-1` derivation version.
- OBSERVED can only be constructed from registered EvidenceRecord metadata. It preserves
  the evidence ID and exact optional SourceStateRef, exit state, timestamp, and recorded
  summary. HISTORICAL means what that evidence reported, not current validity or a verified
  general engineering claim. Commands and logs are never executed or fetched.

Promotion creates a new CANONICAL entry linked to the original memory. It preserves
origin/actor/author/provider/evidence/derivation provenance and ownership, adding the
explicit promotion actor to source references and the audit event. The original remains
unchanged. Inactive or stale-derived items cannot be promoted. A promoted source-bound
fact becomes an explicitly accepted durable decision; provenance still shows its basis.
Rejection and supersession retain payloads. Supersession requires distinct active entries
with identical trust, kind, scope, and canonical key; cycles and implicit trust changes
are impossible through these transitions. Creation with `--supersedes` atomically retires
the old entry and inserts its replacement. Direct supersession records the replacement
on the historical view; entries themselves are immutable.

Active repository-scoped canonical keys are unique when supplied. `config:` is reserved.
Exact whitespace-normalized content with the same repository/scope/trust/kind and
derivation/observation basis is also unique while active. Collisions fail explicitly;
there is no fuzzy merge or contradiction detector. Multiple distinct relevant decisions
remain visible, rather than silently picking a winner.

## Ownership, links, and validity

Durable decisions default to RepositoryId, shared by all linked worktrees. Temporary notes
may opt into WorkspaceId; derived facts always do, and observations inherit evidence
location. Repository entries plus current-workspace entries are the default query scope.
Explicit all-workspace history may expose other workspaces, but cannot call their derived
facts fresh or silently promote/mutate them from this checkout. Independent clones stay
separate; moved primary repositories may get new local IDs and receive no automatic memory
relocation. No remote is required. Those accepted Stage 1 rules remain unchanged.

Typed links cover graph entity, normalized file path, task, plan, job, evidence, invariant,
full Git commit ID, memory, and exact tag. Durable database targets must exist in the same
repository when created. Graph targets must exist in the current workspace index. File
paths can intentionally refer to planned or historical files; invariant references are
opaque keys, and commit links validate full object-ID syntax without resolving Git objects.
Graph/file references are historical links, not cascading foreign keys: a rename or graph
deletion cannot delete a canonical decision. Missing links are exposed as unresolved on
inspection. A link alone does not turn a decision or hypothesis into source-bound truth.

Derived validity checks only its supporting paths: current indexed hash/backend with no
diagnostic, current file hash, parser/index/derivation version, supporting entity presence,
and originating workspace must match. Targeted discovery reuses Stage 2 ignore/protected/
symlink/size policy while pruning unrelated subtrees; hashes are cached within a candidate
batch. Unrelated file edits do not invalidate the fact. Changed/deleted/excluded/unreadable
support or old versions yield STALE even before reindexing. Stale items stay inspectable
but are excluded by default. Filesystem checks are sequential observations, not an atomic
snapshot or exact evidence binding. Human decisions and notes are not hash-invalidated;
evidence observations remain historical after source changes. Future derivations that
claim cross-file resolution must record all target-dependent support, not just a caller.

## Persistence, policy, and bounded retrieval

Transactional migration 4 adds `memory_entries`, `memory_links`, and bundled SQLite FTS5
`memory_fts` (plus its internal tables), without altering earlier payloads or graph IDs.
Repository/workspace foreign keys and retrieval/link/active-key indexes constrain ownership
and support selective queries. Immutable-history triggers reject payload edits/deletes and
link edits/deletes. Status transitions, new rows, FTS tokens, links, and aggregate local
MEMORY_CREATED/PROMOTED/SUPERSEDED/REJECTED journal events commit in one IMMEDIATE transaction.
Audit failure rolls everything back. The accepted AgentEvent schema is untouched. Reads
emit no events; validity is computed, not a read-triggered persistent mutation. Read-only
opens require the current schema; migration conflict/future-version rejection remain intact.

Project TOML remains the sole authority for its invariants, architecture, commands,
protected paths, and verification policy. Memory exposes read-only CANONICAL/PROJECT_CONFIG
projections with source path, workspace, and a BLAKE3 fingerprint of validated normalized
config—not stored copies. Editing project.toml immediately changes the projection. Different
worktrees can expose different branch policies; they do not overwrite shared repository
decisions. Reserved projection keys cannot be promoted, rejected, or superseded as stored
MemoryIds. There is no import/export/synchronization framework.

FTS indexes normalized content and tags: camelCase/acronyms, snake_case, punctuation, and
case are normalized; all requested lexical tokens must match. Typed link filters match
any supplied link and combine with text/trust/kind/status/scope filters. Ranking is trust
(CANONICAL, then OBSERVED/DERIVED, then AGENT_NOTE), exact phrase, newest creation time,
then ID. `recent` skips phrase preference, not trust priority. Queries read a SQLite
snapshot, fetch bounded candidates, and validate only those—not the whole history in Rust
or the entire source tree. Limits: 1–100 entries, at most 64 link filters, 512 search bytes/
32 tokens, and at most `min(limit*10,1000)` checked candidates. Truncation reports both
result overflow and candidate exhaustion; narrow filters when stale candidates dominate.
Policy is separate, bounded to ten matching projections (or the smaller result limit),
2048 characters each, with explicit item/content truncation. There is no pagination yet.

`code context` wraps the accepted ContextPacket with a memory section; graph nodes and
Stage 0 schemas do not change. `memory_for_code` uses bounded primary/neighbor/test entity
and file links plus lexical query matches. `memory_for_task` uses task ID, graph IDs,
invariant refs, and bounded objective tokens; evidence-bearing entries linked to these
remain discoverable. This supports future resume knowledge, not resume orchestration.
Context unions linked/text matches, favors trust then recency/ID, and also offers live
project policy. Relevant stored canonical decisions precede unfiltered policy projections.
Defaults are three canonical items, three observed/derived facts, one note, and 4096 bytes;
hard bounds are 10/10/5 items and 256–16384 bytes of compact serialized memory JSON.
Summaries are capped at 384 characters, with truncation flags and IDs for full inspection.
Inactive/stale facts never silently enter default context. Memory cannot overwhelm the
graph packet or become authority just by being recent.

Content is untrusted data, never commands, config patches, or instructions to this program.
There is no authentication, autonomous extraction, semantic contradiction resolution,
embeddings, network, provider adapter, planner/executor/verifier runtime, scheduler, or
Stage 4 functionality. Registered evidence is reported faithfully, not independently
verified by this layer. Mechanical extraction is intentionally narrow; other knowledge
uses explicit canonical decisions or attributed lower-trust notes.
