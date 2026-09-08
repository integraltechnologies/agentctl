# Architecture contract — Stage 1, preserving protocol v1

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
promotion requires a future explicit, auditable policy decision. There is no persistence
or promotion operation in this stage.

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
| Durable machine-local data | `~/.local/share/agentctl/` | Canonical repository/task/job/evidence/event state; future graph/memory/resume state |
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
execution in Stage 1. Future task execution must resolve canonical checks and cannot
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
  Repository-level plan/task ownership is shared; future project identity, engineering
  memory, architecture decisions, and repository-level graph identity can use this same
  repository key. No such Stage 2 systems are implemented here.
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

Database schema version 2 uses SQLite `application_id`, `user_version`, and a small
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
support trailing `--json`. No graph implementation, provider adapter, scheduler,
network stack, daemon, TUI, ML runner, or agent loop is present. Future stages must
preserve these ownership and verification boundaries.
