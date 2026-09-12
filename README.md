# agentctl

`agentctl` is an Integral Technologies project for a machine-level, provider-neutral
engineering control plane underneath coding agents. Engineering state should survive
switching providers, ending conversations, and restarting agent jobs. Canonical state
belongs to `agentctl`, never to a provider conversation or harness.

The intended workflow is a high-compute **planner** producing a dependency DAG of
compact `TaskPacket`s, bounded lower-compute **executors**, an independent fresh-context
**verifier for every packet**, and a final **integration verifier**. Executor success
does not unlock dependent work: only a `VERIFIED` prerequisite does. Individually
verified packets still need integration verification before the plan is complete.

Long term, the control plane will share persistent repository graph intelligence,
engineering memory, evidence, and durable task/job/resume state across providers.
It will support detached engineering and ML jobs, observable progress, and `agenttop`,
a btop-like TUI with a rolling token-usage graph. Roles are provider-neutral;
provider/model identities are opaque machine-configured adapter metadata.

## Stage 5

This repository currently provides one Rust 2024 crate with a library and a tiny CLI:

- Versioned JSON contracts for plans, tasks, results, verification, resume, evidence,
  jobs, events, probing, token usage, and experiments, plus memory trust/provenance.
- Task DAG validation, task/job lifecycle rules, and pure verification/completion guards.
- Generated JSON Schemas in `schemas/` and regression tests for the protocol invariants.
- Typed machine/project TOML config, XDG paths, Git checkout registration and source-state inspection.
- Local SQLite storage for immutable plans/tasks, job state, compact evidence metadata,
  and an append-only journal. State transitions and journal records commit atomically.
- Persistent, content-hashed code graphs for Rust, Python, TypeScript/TSX, and JavaScript/JSX.
- Incremental file-level extraction, hash-checked queries, deterministic code location,
  bounded graph context, and known structural impact. Linked worktrees have isolated
  source-specific graph state under their shared repository identity.
- Provider-neutral engineering memory with explicit trust/provenance, typed links,
  immutable promotion/supersession history, deterministic FTS5 search, live project-policy
  projections, and bounded code/TaskPacket memory context.
- Provider-neutral planning requests and frozen bounded planner input, strict ExecutionPlan
  import, one independent verification contract per task, final integration contracts,
  persistent plan lifecycle/history, and VERIFIED-only structural readiness.
- Local serialized execution through thin Claude Code/Codex CLI adapters, actual
  content-hashed diffs, canonical command evidence, fresh packet/integration verifiers,
  runtime-owned job authorization, drift blocking, and durable recovery checkpoints.

The accepted Stage 0 protocol and all 13 public schemas remain unchanged.

It does **not** implement LSP, automatic correction loops, concurrent writers, daemons,
an experiment runner, `agenttop`/TUI, token analytics, MCP services, web UI, remote
scheduling, embeddings, or a vector DB. Provider calls may use the network; canonical
verification commands may not. Native execution currently requires macOS `sandbox-exec`.

## Development

Requires stable Rust 1.88 or newer. No runtime services are needed.
Repository commands require Git on PATH. SQLite is bundled at build time; no SQLite
server or separate installation is needed. State is intended for a local filesystem.

```sh
cargo build --locked
cargo run -- --version
cargo run -- schemas generate
cargo run -- schemas generate --output /tmp/agentctl-schemas
cargo run -- protocol validate task path/to/task.json
cargo run -- --help

cargo run -- init
cargo run -- doctor --json
cargo run -- repo init
cargo run -- repo status --json
cargo run -- repo list
cargo run -- repo index --json
cargo run -- repo index --status --json
cargo run -- code symbol resolve_candidate --json
cargo run -- code locate "vehicle confirmation" --limit 5 --json
cargo run -- code context "vehicle confirmation" --limit 3 --depth 1 --neighbors 12 --tests 4 --json
cargo run -- code impact 'qualified::symbol' --json
cargo run -- code callers 'qualified::symbol' --json
cargo run -- state status --json
cargo run -- events list --limit 20 --json

cargo test --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

`protocol validate` checks a single document's structure and semantic invariants.
The library's `PlanPacket::validate_task_transition`, `task_is_runnable`, and
`validate_completion` also check caller-supplied lifecycle state and verification
packets. The local store reuses these guards with durable state; it never schedules
or executes work. Creation/transition APIs are library operations in Stage 1.

`init` creates missing machine configuration and state. `repo init` runs inside a Git
checkout and creates `.agentctl/project.toml` only when absent. Existing configuration
is never overwritten. Status/list/doctor commands inspect existing state; put `--json`
last for machine-readable output. Errors use stderr and a nonzero exit code.

Defaults are `~/.config/agentctl/config.toml`,
`~/.local/share/agentctl/state.sqlite3`, and `~/.cache/agentctl/`, honoring absolute
XDG overrides. Machine config contains `version = 1` and `busy_timeout_ms = 5000`.
Project config has versioned declarations for invariants, architecture, commands,
protected data, and canonical verification. No provider settings are introduced.

A logical repository ID hashes its canonical Git **common directory**; all linked
worktrees share it. Each checkout has a distinct workspace ID hashing its canonical
per-worktree Git directory, with its own root, HEAD, and dirty-state observation.
`repo status` shows both identities; `repo list` groups registered workspaces under
one logical repository. Independent clones remain distinct, and no remote is required:
remote URLs are metadata only. Moving a primary repository normally creates new local
IDs; robust relocation is deferred and old registrations remain inspectable. Git status
and HEAD are observations, not an exact fingerprint of a dirty working tree.

## Code intelligence

Run `repo init`, then `repo index` in each workspace. The first pass extracts supported
files; subsequent passes hash content and only reparse new/changed/version-invalid files.
Deleted or newly ignored files lose their facts. A parse/read failure removes that file's
old facts, persists a diagnostic, and makes indexing exit nonzero while committing useful
results from other files. Database/journal failures roll back the entire update.

`repo index --status` reports the indexed source observation, parser/backend versions,
counts, stale paths, and failures. Every code query rechecks discovery and
content hashes; stale snapshots are refused with a refresh instruction. Partial indexes
can answer from successful files, prominently marked partial with failed-file counts.
These are sequential filesystem observations, not exact diff/evidence bindings.

`code symbol` performs exact name/qualified-name/ID lookup; `code search` searches name
substrings; `code file` lists file entities. `code locate` ranks names, normalized
snake_case/camelCase tokens, paths, containers, and compact signatures without an LLM.
`code refs`, `code callers`, `code tests`, `code neighbors`, `code context`, and
`code impact` expose bounded graph relationships. Ambiguous impact/relationship requests
require a qualified name or graph ID. Empty searches are valid empty results.

Tree-sitter provides syntax, not compiler or runtime truth. Imports and most calls remain
explicitly unresolved; only unique same-module Rust `self::name` call/type/trait paths
are resolved. Test entities recognize documented naming/attribute conventions, and test
links indicate lexical containment, not proven coverage. No macro expansion, dynamic
dispatch, cross-file resolution, C adapter, embeddings, or provider calls are implemented.
Indexing skips ignored files, symlinks, nested repositories, common build/dependency trees,
and project `deny_read` paths. See the architecture contract for limits and guarantees.

## Shared engineering memory

Memory belongs to a logical repository by default, so linked worktrees share durable
decisions without copying rows. `--workspace` scopes a temporary note/decision to the
current checkout. Source-derived facts and workspace evidence observations retain their
concrete workspace. Other workspaces' entries are hidden unless `--all-workspaces` is
explicit; another workspace's derived fact is never declared fresh in this one.

```sh
agentctl memory add --trust canonical --kind architecture-decision \
  --content 'Fuzzy vehicle identity matches require explicit confirmation.' \
  --key vehicle:confirmation --symbol resolve_candidate --invariant KD-VEHICLE-004
agentctl memory add --trust agent-note --kind finding --job job:executor-a \
  --workspace --content 'Possible resolve_candidate singleton bypass.'
agentctl memory derive confirm_vehicle --json
agentctl memory observe evidence:1 --json
agentctl memory search 'vehicle confirmation' --trust canonical --limit 10 --json
agentctl memory list --symbol resolve_candidate --json
agentctl memory list --task a --json
agentctl memory show memory:ID --json
agentctl memory links memory:ID --json
agentctl memory promote memory:ID --actor reviewer --json
agentctl memory supersede memory:OLD --with memory:NEW --json
agentctl memory reject memory:ID --actor reviewer
agentctl memory list --status superseded --include-stale --json
agentctl memory stale --json
agentctl memory policy --json
agentctl code context resolve_candidate --memory-canonical 3 --memory-facts 3 \
  --memory-notes 1 --memory-bytes 4096 --json
```

Replace illustrative IDs with registered records. Stage 0 requires an author job for
AGENT_NOTE; register plans/jobs through the existing Store APIs first. There is no job
launcher or invented CLI job creation. `observe` requires existing persisted evidence;
it records an observation, never executes its command. `derive` mechanically summarizes
one unambiguous indexed symbol's signature and bounded syntactic outgoing relations—no
LLM prose or claims of compiler resolution. Free-form DERIVED/OBSERVED creation is refused.

CANONICAL creation requires explicit `--trust canonical`. Promotion is also explicit:
it creates a new canonical descendant, preserves original provenance and ownership scope,
and leaves the original unchanged. Local journal events audit both actions; there is no
authentication or automatic authority inference. Keyed canonical decisions are unique
while active. To replace one atomically, use `memory add --trust canonical --key KEY
--supersedes memory:OLD ...`; old content remains historical. `--all` includes inactive
history; `--include-stale` separately includes stale derived facts. Normal reads exclude both.

DERIVED freshness checks only supporting paths/hashes and graph/parser/derivation
versions, honoring the graph's read exclusions. Source changes do not invalidate durable
decisions or notes. OBSERVED remains historical, explicitly bound to the recorded evidence
and source state, not a claim that today's checkout passes. Notes remain fallible even
when their validity label is DURABLE (meaning not file-hash-bound).

`.agentctl/project.toml` stays the sole source of truth for its policies. Invariants,
architecture, commands, protected paths, and verification definitions appear as read-only
CANONICAL/PROJECT_CONFIG projections with config fingerprints, never mutable database
copies. Edit that file to change them; `config:` keys are reserved. Projections reflect
the current worktree's config and may differ between branches. Search/list JSON separates
`entries` and `policy`; item limits/truncation are explicit. Memory text is data, not
instructions, and never changes config or launches commands.

Search normalizes camelCase/snake_case and punctuation, requires all lexical tokens,
and supports typed link, exact tag, kind, trust, status, and recency filters. Matching
canonical entries rank ahead of observed/derived facts, then notes. Results and source
checks are bounded; narrow filters if truncated. Code context adds only compact summaries
with separate trust quotas and a compact-JSON byte budget; full provenance remains available
through `memory show`. `Store::memory_for_task` provides the same bounded retrieval for
future task/resume consumers, without implementing a runtime.

SQLite migration 4 adds memory entries, typed links, and FTS5 to the existing machine
database without rewriting tasks/jobs/events/graph data. No new dependency or service is
required. Existing databases migrate on `init` or a writable open; read-only commands do
not migrate. Independent clones and moved primary repositories retain Stage 1's distinct
local identities; memory is not synchronized or relocated automatically.

## Planning without a model runtime

Stage 4 controls plans; an external planner decides their decomposition. Nothing in
`agentctl` calls a model, generates a fake plan, launches an executor/verifier, or schedules
work. Configure canonical checks in `.agentctl/project.toml` under `[verification.KEY]`
with references to `[commands.KEY]` before importing executable plans.

```sh
agentctl plan prepare --objective 'Add deterministic cache invalidation to repository indexing' \
  --query 'cache invalidation' --bytes 32768 --json
agentctl plan context request:ID --json
# An external producer writes execution-plan.json using this frozen planner input.
agentctl plan import execution-plan.json --json
agentctl plan validate plan:ID --json
agentctl plan show plan:ID --json
agentctl plan export plan:ID --json
agentctl plan tasks plan:ID --json
agentctl plan ready plan:ID --json
agentctl plan blocked plan:ID --json
agentctl plan activate plan:ID --json
agentctl plan list --json
agentctl plan supersede plan:OLD --with plan:NEW --json
agentctl plan cancel plan:ID --reason 'Objective withdrawn' --json
```

Use returned request/plan IDs, not the illustrative placeholders. `prepare` accepts
`--objective-file PATH` instead of inline text, or `--request-file PATH` containing a
strict `RequestDraft`: objective, optional query, scope, constraints, definition_of_done,
optional verification, invariant_refs, and provenance (actor, source_refs, optional opaque
provider metadata). No transcript/reasoning field exists. All project invariants become
critical request invariants. Explicit additional invariant keys can reference active
repository-wide CANONICAL/INVARIANT memory.

The planner input is a `PlannerPacket` with `artifact = FROZEN_PLANNING_INPUT`, request,
context, and exact compact-JSON `serialized_bytes`. It combines the accepted graph/memory
APIs with a frozen policy snapshot and bounded exact source excerpts. Defaults: four graph
primaries, eight neighbors, four tests, eight files; four canonical memories, three
observed/derived facts, **zero notes**; 768 bytes/20 lines per excerpt; 32 KiB total.
`--notes N` explicitly opts into fallible notes. Other knobs are `--primary`, `--neighbors`,
`--tests`, `--files`, `--canonical`, `--facts`, `--excerpt-bytes`, `--excerpt-lines`, and
`--bytes`. Truncation is reported; required intent/invariant/policy text is never silently
discarded to fit. `plan context` returns the same persisted historical input, not fresh
claims about today's checkout. It does not regenerate context on every read.

External output is an `ExecutionPlan` envelope with exactly two fields: `packet` is the
unchanged Stage 0 `PlanPacket`; `metadata` contains version, request_id, the request's
unchanged source binding, creation time, provenance, per-task contracts, an integration
contract, and optional replan history. Each task contract names its TaskId and packet hash,
requires an independent verifier with `PACKET_DIFF_AND_EVIDENCE` input, and adds memory_refs,
exclusions, and non_goals. Its objective, scope, invariants, done criteria and check refs
come from the immutable TaskPacket, not a second competing task definition. Integration
binds the complete PlanPacket, requires all task verifications and final diff/evidence,
and carries overall expectations including user-specified done criteria.

Use `agentctl::local::planning::hash` on the typed `TaskPacket` and `PlanPacket` for contract
hashes: BLAKE3 of compact serde JSON in declared field order, **not** pretty-printed JSON
or an arbitrary map's key order. The executable fixture in [tests/planning.rs](tests/planning.rs)
shows construction and cross-process import. Import deserializes strict Rust types and
applies Stage 0 validation plus Stage 4 reference/scope/source/contract checks. It never
executes embedded text or policy commands. Bounds are 32 tasks/256 KiB per plan, 16 KiB
per TaskPacket, and 8 KiB per task contract. `plan tasks` reports individual byte sizes
and carries source assumptions, constraints, and resolved critical invariant text.

Import publishes `VALIDATED`, not ACTIVE. Activation rechecks graph/source/policy/memory
assumptions and permits only one active plan per workspace. `ready` is structural readiness,
not permission to execute: PLANNED/READY candidates need every prerequisite VERIFIED.
Executor success or verification rejection cannot unlock dependents. The Stage 4 `plan`
commands never run tasks; the Stage 5 `run` commands below do.
Stage 4 packet verification requires the executor, verifier and every evidence record to
be bound to the plan's workspace: sibling worktrees and unbound records cannot unlock tasks.
The library's `complete_execution_plan` accepts externally recorded integration proof only
after all packets are VERIFIED; it checks registered successful jobs, the actual contributing
executor set, and evidence bound to the submitted final source/workspace. Authentication,
fresh verifier sessions, actual command execution, and exact diff/evidence capture
are supplied by Stage 5 for runtime-owned plans, not retroactively for legacy jobs.

Replacement plans use new PlanIds and new TaskIds, with an explicit prior-plan reference
and reason. Historical VERIFIED/replaced task references are supported, but never copy
acceptance into new tasks. `supersede` retains the old plan and does not activate the new
one automatically. Cancellation/supersession refuse unfinished jobs. Inspection reconstructs
objective, DAG, task states, contracts and readiness without replaying conversations.

Migration 5 adds only planning_requests and execution_plans; existing plans/tasks remain
the single task store. Imports, lifecycle changes and audit events commit atomically.
Migration 6 adds completion guards: SQL updates cannot set COMPLETE without a connection-local,
payload-bound capability granted by the validated completion operation and its matching audit
event. INSERT/REPLACE cannot start a plan completed. Existing Stage 4 databases migrate on open.
Before upgrading v5, migration validates every existing COMPLETE plan against durable task
verification history, workspace-owned jobs/evidence, integration proof, final source and its
matching completion audit. Invalid or unverifiable history aborts the entire migration at v5
with a plan-specific error; no completion state or audit is repaired or synthesized.
Pending/terminal Stage 4 plans cannot transition task rows; legacy Stage 1 plans retain
their accepted behavior. Existing packet schemas, graph IDs, memory semantics and dependency
versions are unchanged (rusqlite's existing `functions` feature is enabled for the guard).
Read-only opens do not migrate. Index STARTED/FAILED attempt-level
observability remains deferred to the later observability stage.

Rust types are canonical. Regenerate and review schemas whenever contracts change;
tests fail if checked-in schemas drift. See [the architecture contract](docs/architecture.md)
for versioning, lifecycle semantics, trust boundaries, and directory conventions.

## Running agents and verification

Configure installed executable paths and role mappings in machine `config.toml`.
Models/effort are optional opaque provider values; choose them explicitly for your budget.
For example (replace the executable paths with your installations):

```toml
[runtime]
timeout_ms = 600000
max_correction_rounds = 2

[runtime.providers.codex]
adapter = "codex"
executable = "/absolute/path/to/codex"

[runtime.providers.claude]
adapter = "claude"
executable = "/absolute/path/to/claude"

[runtime.roles.planner]
provider = "codex"
[runtime.roles.executor]
provider = "claude"
[runtime.roles.verifier]
provider = "codex"
```

Authentication defaults to `AUTO`: reuse the provider's native login first. Codex keeps
the original `CODEX_HOME` for authentication; Claude preserves normal HOME/config-location
and Keychain identity, using `--safe-mode` rather than `--bare`. Authentication is persistent,
but each worker conversation is fresh and non-resumable. History/customizations remain
disabled and provider history files are sandbox-denied; no credentials are copied into
agentctl state. `provider doctor` and runtime preflight use token-free native login-status
commands and report only authentication method/availability, not account or credential data.
They do not prove live model access or refresh expired credentials on behalf of the provider.

Optional API-key use must be intentional; configure a variable **name**, never its value:

```toml
[runtime.providers.codex.authentication]
mode = "AUTO" # NATIVE forbids fallback; API_KEY bypasses native login
api_key_env = "MY_CODEX_API_KEY" # optional AUTO fallback
```

The named value is passed only to the selected provider's key variable. Ambient API keys
are not silently selected. Unsupported CLIs/auth mechanisms fail with login/configuration
guidance, not an unsandboxed fallback.

```sh
agentctl provider list --json
agentctl provider doctor --json
# Prepare bounded intent using the existing plan prepare command, then:
agentctl run planner request:... --json  # imports VALIDATED; never auto-activates
agentctl plan activate plan:...
agentctl run plan plan:... --dry-run --json
agentctl run plan plan:... --json
agentctl run status plan:... --json
agentctl run resume plan:... --json
agentctl run cancel plan:...           # request cancellation from another process
agentctl run replace plan:old plan:new # explicit VALIDATED correction; then activate
```

Project verification profiles must reference nonempty canonical `commands` (program,
argv, cwd), not planner shell prose. Checks run read-only against the workspace with
network disabled and scratch-only build output; configure tools accordingly. Each task
executes once, its actual changes are scope-checked, checks produce evidence, and a
fresh verifier must PASS before dependents run. All packets then require fresh integration
verification through the unchanged Stage 4 completion guard. No automatic commits,
pushes, resets, worktree creation or cleanup of user source occur.

Start with a clean committed checkout and a newly prepared/activated plan. Unexpected
source, HEAD, index or policy changes block the run; rejected/uncertain work requires an
explicit replacement plan. The default permits at most two replacement rounds (configurable
downward to zero), never automatic executor↔verifier retries. Human reconciliation of the
checkout is required before preparing a clean correction baseline.

Migration 7 adds runtime runs/jobs and local authorization guards without rewriting old
records. Source snapshots, diffs, bounded provider output and command logs live in private
content-addressed storage outside the checkout. Resume uses durable artifacts, not chat
replay: verified tasks are not re-executed, pending verification/integration checkpoints
continue, and uncertain interrupted jobs block rather than being treated as successful.

An EngineeringSession owns the undertaking; roles are reusable configuration, while each
AgentInstance is session-native and ephemeral. The accepted planner DAG requests workers;
agentctl checks readiness, routing, workspace and budget before creating each executor,
then its independent verifier, and finally the integration verifier. Parent/session ownership
is durable provenance, never conversation reuse. Implicit children inherit their parent's
engineering session; persistent/cross-session workers are not implemented and would require
explicit user intent. Provider-internal agent spawning is disabled. Temporary job context is
removed after normal termination; durable packets/evidence/events remain, with no automatic
conversation-to-memory promotion.

Ownership fits existing v7 JSON metadata; no schema bump or historical backfill is needed.
Older unaccepted v7 runtime rows remain inspectable but cannot resume without ownership;
they require explicit reconciliation/replanning. Accepted Stage 0–4 data is unchanged.

Stage 5 intentionally supports small text checkouts: at most 20,000 files/64 MiB total,
2 MiB per file, 128 KiB expanded verifier diff and 256 KiB provider input. Ignored files
are included; symlinks, hardlinks, nested repositories and read-denied protected files
are rejected. Captures are double-checked sequential observations, not atomic filesystem
snapshots. No live provider model calls are needed for tests. Native sandbox tests are
separately runnable with `cargo test --locked -- --ignored` on a capable macOS host;
they use fake executables/disposable repositories and opt-in installed native login-status
checks, not paid model calls. Native login-status tests require locally logged-in CLIs.
