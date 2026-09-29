# Changelog

All notable changes to this project are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
agentctl is alpha software; versions before `1.0.0` may include breaking changes
to storage, configuration, or the CLI.

## [Unreleased]

## [0.3.0-alpha]

A ground-up rebuild. agentctl is now a local, provider-agnostic engineering
control plane in which a human provides intent and interchangeable AI agents
plan, execute and independently verify the work as one persistent, concurrent,
observable and recoverable system. Models supply intelligence; the planner
supplies engineering judgment; agentctl owns execution truth, authority,
coordination and continuity; CodeGraph owns structural knowledge of accepted
source; verifiers supply independent acceptance evidence. No provider owns any
canonical state.

This release is **not compatible** with 0.2.0-alpha.1 or 0.1.x: configuration,
local state and the command line are all new. Re-initialize projects with `agentctl init`. See
[README.md](README.md) and [docs/](docs/architecture.md).

### Added

- **Project-local configuration and state.** A version-controlled
  `agentctl.toml` (project, CodeGraph source roots, roles, providers,
  concurrency, invocation timeout) and a Git-ignored `.agentctl/` holding one
  canonical SQLite store and content-addressed recovery objects. Every state
  change is one transaction together with its event.
- **Plans from human intent.** `agentctl plan create` records an objective,
  constraints and completion criteria, fixed for the plan's life, and runs
  initial planning.
- **Planner command protocol.** Planners propose typed commands
  (`add_task`, `update_task`, `remove_task`, `set_dependencies`, `finalize`;
  when replanning `cancel_task`, `retry_task`, `raise_attention`,
  `propose_completion`). agentctl validates the whole proposal against the
  plan and applies it atomically or not at all.
- **Provider-neutral agent runtime.** One contract for every invocation:
  input on standard input, one structured result validated by agentctl
  against a JSON Schema, agentctl's own failure classification, usage with
  provenance (provider-reported, local estimate, unavailable), and no
  provider-controlled text in durable records. Built-in `claude` and `codex`
  adapters, and a `generic` adapter implementing agentctl's external-agent
  protocol, version 1, usable under any configured provider name.
- **Structured action journal.** Engineering-control actions are journaled
  INTEND → ACT → RECONCILE, attempted before any acting process exists and
  reconciled from agentctl's own observations.
- **Concurrent DAG scheduling.** `agentctl run` claims eligible tasks within a
  project-wide `max_concurrency` ceiling and runs each generation's pipeline
  (executor, candidate install, independent verifier, acceptance)
  concurrently. Only an accepted task satisfies its dependents.
- **Mutation ownership.** Exclusive, all-or-nothing ownership of exact literal
  paths per generation, across all plans, granted only where the working tree
  holds accepted state.
- **Disposable executors and candidates.** Each generation's executor works in
  a disposable copy of the repository outside the project. agentctl derives
  the candidate from the workspace itself, rejects out-of-scope changes, and
  installs the candidate into the working tree only where the baseline still
  stands, with both old and new bytes preserved as recovery objects.
- **Independent task verification.** A fresh verifier judges each installed
  candidate over accepted source, never over other pipelines' unaccepted work,
  given nothing the executor said.
- **Phased acceptance.** A verified candidate is published as accepted source,
  CodeGraph is synchronized, then the generation is accepted, its task
  completed and its ownership released, each phase durable and resumable.
- **CodeGraph.** Persistent, syntax-only structural facts (entities and direct
  relations) derived from accepted source by Tree-sitter frontends for Rust,
  Python, JavaScript, TypeScript and TSX, bound to accepted content hashes and
  refreshed on acceptance.
- **Replanning.** `agentctl plan update` gives a fresh planner canonical
  feedback on how the work went. Proposals are bound to the plan state they saw
  and refused as stale if it changed. Retries require an explicit, single-use
  authorization of a task's current definition; abandoning an attempt restores
  its candidate to accepted state.
- **Needs-attention and human decisions.** Planners can stop a plan with a
  concern; only a human decides it (`agentctl plan attention`,
  `agentctl plan decide ... accept|instruct|stop`), once.
- **Final integration verification.** A plan completes only when a fresh
  verifier passes its assembled accepted result against the intent, bound to
  the exact accepted source and repository inputs it judged
  (`agentctl plan verify`, or automatically when a replan proposes
  completion).
- **Pause, resume and cancel.** `agentctl plan pause`, `plan resume` and
  `plan cancel`, with durable cancellation requests honored by whichever
  agentctl process runs the work.
- **Process lifecycle through procd.** Every invocation's processes run in a
  procd lifecycle domain created before anything runs. agentctl ends the whole
  domain on completion, cancellation, timeout (`invocation_timeout_minutes`,
  default two hours) and SIGINT/SIGTERM/SIGHUP, and records each end with its
  termination strength (`enforced` or `best_effort`) as durable provenance.
- **Crash and restart recovery.** `agentctl recover` settles work left by
  agentctl processes that ended, using session locks for liveness and procd's
  proof for provider processes, and fails closed where nothing can be proven.
- **Observation.** `agentctl status`, `agentctl logs` and the `agenttop` live
  monitor, over one read-only model of canonical state that never probes
  liveness and never shows unknown usage as zero.
- **Environment authorization.** Providers receive a cleared environment plus
  a fixed common set, their adapter's variables and the exact names their
  configuration lists.

### Changed

- Tasks run concurrently in disposable workspaces outside the project,
  replacing 0.2.0-alpha.1's managed linked worktrees and serial fallback.
- Process lifecycle is delegated to procd instead of agentctl's own
  sandboxing. On macOS the lifecycle guarantee is best effort and recorded as
  such; on Linux with a delegated cgroup v2 subtree (or as root) it is
  enforced.
- Windows builds and runs its test suites; it is not yet qualified for use.
- The code graph is rebuilt from scratch as CodeGraph over accepted source.

### Removed

- The 0.1/0.2 command set, configuration format and storage, including the
  repository indexing, planner and plan-running commands, the `ontology` and
  `run context` commands, `agentctl observe` and `agentctl analytics`.
- 0.2.0-alpha.1's ontology generations, semantic deltas, impact and
  structural-footprint analysis, planner-mediated context relay, context
  manifests and optional semantic enrichment (`repo enrich`).
- Provider routing and fallback, machine-level policy, structured engineering
  memory with trust classes, and experiment supervision.
- agentctl's own OS sandboxing (Seatbelt on macOS, Landlock and seccomp on
  Linux). Providers are confined only by their own editing modes or sandboxes;
  see [docs/security.md](docs/security.md).
- The 64 MiB / 20,000-file workspace capture budget.

### Fixed

These issues were first fixed in 0.2.0-alpha.1; the rebuilt system fixes them
again by construction and was requalified against them.

- Repository paths are literal everywhere (issue #1). Names such as `[slug]`,
  `[id]`, `[...slug]`, `(group)`, `@slot`, names with spaces or `+`, and
  Unicode names are ordinary filenames in source discovery, planner scopes,
  ownership, workspaces, installs, verification views and restoration; Git is
  invoked with literal pathspecs. On Unix even `*` and `?` are literal. On
  Windows, names the filesystem would reinterpret are refused rather than
  reinterpreted.
- Git-ignored content is never ingested (issue #3). Source discovery,
  repository observation, executor workspaces and verification views include
  only what Git lists as repository content (tracked files, and untracked files
  not ignored by Git's standard exclude rules), so a large ignored build tree
  such as `target/` is never read, copied, indexed or given to an agent. A
  tracked file stays repository content even if an ignore rule matches it.
  Generated and ignored files are never inputs to integration verification.

### Known alpha limitations

- On macOS, termination is best effort: a process that escaped procd's
  tracking is not excluded.
- If agentctl is killed outright while an agent runs, its invocation can be
  settled only where procd can recover the domain (Linux as root). Elsewhere
  the plan stays blocked, fails closed, and orphaned provider processes may
  keep running.
- No filesystem, network or resource isolation of providers by agentctl.
- A human edit to an already-accepted file cannot be adopted as accepted
  source; the working tree must be reconciled before tasks touching it run.
- After `run`, replanning and proposing completion are human-initiated
  (`agentctl plan update`).
- CodeGraph frontends are syntax-only; agent inputs carry entity maps, not
  relation traversals.
- agentctl never commits or pushes.

## [0.2.0-alpha.1]

Published 2026-09-24 as the prerelease "agentctl v0.2.0-alpha.1 — Prerelease
Control Plane" (tag `v0.2.0-alpha.1`). The tagged tree still carried
`version = "0.1.0-alpha.2"` in `Cargo.toml`, and its changelog listed these
changes under `[Unreleased]`; they are recorded here as the release they
shipped in. This release was the fixed baseline for the v0.3 rebuild; the mechanisms
below belong to the architecture 0.3.0-alpha replaced.

### Added

- Ontology generation lifecycle. Indexing is now an observation: every index
  pass that changes the indexed facts is recorded as a generation with an
  immutable snapshot, and a workspace has exactly one *accepted* generation.
  Changes become accepted only deliberately (`agentctl ontology accept`) or,
  for runtime work, in the same transaction that completes a plan after its
  integration verification passes. Task-verified work is the plan's
  candidate until then. The first complete index of a workspace, and a
  re-observation identical to the accepted facts (for example a revert), are
  accepted automatically. Candidates are superseded by later observations,
  rejected by verification rejections or `agentctl ontology reject`, and
  abandoned with their plan; all history stays inspectable.
- Semantic deltas (`SemanticDelta`, version 1): the deterministic difference
  between two generations — entities added, removed, or modified (with the
  changed signature, visibility, text or key facts), distinct resolved
  relations added or removed, and per-file content and change counts.
  Identity across generations requires the same entity ID and a unique
  declaration of that path, kind and name in both generations; renames and
  moves are reported as removal plus addition, and same-named duplicates as
  unproven (`DUPLICATE_ORDINAL`). Doc comments and attributes count as part of
  the declaration below them, and nested declarations are excluded from their
  container's text.
- `agentctl ontology status|list|show|delta|accept|reject`, with `--json`
  output and `delta` filters (`--change`, `--path`, `--limit`,
  `--from`/`--to`).
- Semantic impact analysis (`ImpactReport`, version 1): given an observed
  `SemanticDelta` or a proposed edit, the bounded set of existing code that
  could be affected, with a machine-inspectable evidence chain for every
  claim. Evidence is only resolved relations, containment of an added or
  removed declaration, and graph test associations (basis carried); nothing
  is derived from lexical similarity, name coincidence or graph proximity.
  Items are classed `DIRECT_DEPENDENCY`, `CONTRACT_EXPOSURE`,
  `VERIFICATION_RELEVANCE` or `CONTAINMENT_OWNERSHIP`; open questions are kept
  apart as boundaries (`UNPROVEN_IDENTITY`, `UNRESOLVED_REFERENCES`,
  `UNDETERMINED_PROPAGATION`, `DEPTH_LIMIT`, `FANOUT_LIMIT`, `ENTITY_ABSENT`).
  Traversal is deterministic, cycle-safe and bounded, and goes past the first
  hop only where the ontology proves the change is re-exposed, so a body edit
  reaches its direct callers while a signature change can travel through an
  exported relay. Analysis is bound to the generation it names and fails
  closed when that generation is not the indexed one.
- `agentctl ontology impact [<generation-id> | --from <id> --to <id> |
  --symbol NAME] [--plan <plan-id>] [--depth N] [--limit N] [--tests N]`,
  with `--json` output. Read-only: it changes no lifecycle state.
- `plan prepare` now attaches a bounded impact outlook for the entities the
  planner selected, read against the request's own scope, so a planner sees
  consequences outside its intended neighborhood. It is advisory: it carries
  no source text, adds no file to the request's support set, never widens read
  or write scope, and is the first record shed under the byte budget.

- Planner-mediated context relay. A task's `read_scope` is an authorization
  envelope, not content: an executor's context is materialized only from
  planner-authored references (graph entities with bounded definitions and
  in-envelope relation stubs, explicit File read scopes, File write targets,
  contract memory references, and the task's checks). A worker that cannot
  finish returns a typed `ContextRequest` (new `context-request` protocol
  document) instead of exploring; agentctl resolves it deterministically
  against the ontology snapshot and captured source, inside the envelope and
  within machine-owned budgets, and issues a hash-bound `ContextDelta` to a
  *fresh* provider job carrying the original base context plus the accumulated
  deltas. Requests needing paths outside the envelope are never granted
  automatically: the run blocks with `NEEDS_PLANNER_CONTEXT_APPROVAL` for an
  explicit decision (`agentctl run context`, `agentctl run context decide`).
  Budgets, rounds and escalations are configured under `[runtime.context]`
  with compiled-in hard maxima.
- Independent verifier context relay: a verifier may request context from
  verifier-visible material only, on its own round budget, never inheriting the
  executor's requests, reasons or transcript. Such a request is neither PASS nor
  REJECT and never becomes a task transition.
- Opt-in issued-context visibility (`[runtime.context] visibility = "issued"`):
  executor and verifier jobs may then read only the repository files issued to
  them (plus an executor's write scope), with the workspace tree and Git
  directories removed from their read roots. The default remains `workspace`
  pending validation with real provider processes; see docs/security.md.
- Context manifests. Every planner, executor, and verifier job records what
  agentctl supplied: identities, the graph generation, the repository paths and
  ranges (hash-bound), graph entity, memory, and invariant IDs, and exact byte
  accounting by category that sums to the compiled prompt. Manifests copy no
  repository content, and provider-hidden context is marked unobserved rather
  than estimated. `agentctl plan context <id> --manifest` prints one for a
  prepared PlannerPacket.
- Graph generations: a content-derived fingerprint plus a monotonic
  per-workspace sequence. It is reported by `repo index`, recorded in index
  events, and bound into graph context, PlannerPackets, and job manifests.
  Import and activation reject a planning source bound to a generation the
  workspace never reached.
- Conservative relation resolution. In-file resolution covers lexical scope
  (shadowing-aware) and enclosing-type methods. Rust qualified paths also
  resolve across files. Each resolved relation names its rule, and ambiguous
  names stay unresolved.

- Concurrent task execution. READY tasks with proven-disjoint scopes and
  accepted-ontology impact run concurrently in managed linked worktrees, each a
  sandboxed worker of its own. Eligibility is typed (`COMPATIBLE`, `CONFLICT`,
  `DEPENDENCY_BLOCKED`, `SOURCE_INCOMPATIBLE`, `UNKNOWN`) and shown by
  `run plan <id> --dry-run`; unknown or conflicting work stays serial, and
  reconciliation, verification, ontology refresh, integration and acceptance
  remain serialized. The machine-wide `max_agents` ceiling is unchanged.
- Optional semantic enrichment: `agentctl repo enrich` attaches relations
  that an installed semantic provider (`rust-analyzer`, or `scip-python` for
  Python) proves, beyond the syntactic graph.
- Structural footprint analysis: `agentctl ontology footprint` projects
  semantic deltas into bounded, evidence-backed reports of structural change
  (production versus test structure, public-surface and abstraction growth).
  It is advisory, read-only and generation-bound.

### Changed

- Graph context and PlannerPacket composition:
  - IDF ranking without stopwords, with separate implementation and test
    lanes;
  - primary selection that spans files, and neighbors spread across files;
  - tests offered with their association basis;
  - resolved relations only, plus bounded summaries of unresolved call sites;
  - provenance written once per file;
  - excerpts bound to entities;
  - value-ordered truncation that sheds graph noise before implementation
    excerpts;
  - scoped requests and tasks select within their scope.

  On the Issue #3 request at the default 32 KiB budget, the packet now reaches
  the source-capture implementation with excerpts. It carries 0 unresolved
  relation records (previously 52% of the packet) and about 54% fewer
  provenance bytes.
- The graph index version is now `agentctl-graph-2`, so the first
  `repo index` after upgrading re-derives every file. Requests prepared by
  earlier versions stay readable but cannot seed new plans; prepare again.
- Database schema 12 adds workspace-level relation resolution. The migration
  is additive and lossless.
- Database schema 13 adds the ontology lifecycle (`ontology_generations`,
  content-addressed `ontology_blobs`, and a per-entity text hash). The
  migration is additive and lossless and synthesizes no history; the first
  `repo index` afterwards re-derives each file once (facts and generation are
  unchanged) and records the accepted baseline.
- `plan prepare` and `run plan` now require the indexed generation to be the
  accepted one, and a plan to be bound to it. After editing files yourself,
  run `repo index` and `ontology accept` before preparing a plan.
- Context relay bases are issued only from the accepted generation or the
  running plan's own candidate; an unexplained observation during a run blocks
  with `SOURCE_DRIFT`.

### Fixed

- `repo index`, runtime capture, and every other consumer of repository paths
  now treat paths literally (issue #1). Next.js-style names such as `[slug]`,
  `[...slug]`, `[[...slug]]`, `(group)` and `@slot`, and other characters that
  pattern APIs give meaning, are ordinary filename characters. Previously a
  single such path made `repo index` fail, and would have blocked runtime
  capture. Traversal, absolute paths, empty segments, backslashes, `:` and
  control characters are still rejected. Authored scopes still refuse the
  `*`/`?` wildcards.
- Runtime source capture no longer reads, counts, or walks Git-ignored
  content. A large ignored build tree such as `target/` no longer exhausts the
  64 MiB workspace capture budget and blocks `agentctl run planner` (issue #3).
  Snapshots now observe the checkout the way Git walks it:
  - tracked files (even ones an ignore rule matches) and untracked, non-ignored
    files are captured by content, as before;
  - individually ignored files are observed by metadata only, so they cost no
    capture budget and never become agent context, but creating or changing
    one is still drift and a scope violation;
  - directories an ignore rule matches as a whole are never walked.

  Ignore rules come from the repository's `.gitignore` files and
  `.git/info/exclude`; `core.excludesFile` is not applied. The 64 MiB /
  20,000-file aggregate bounds are unchanged. `.git/info/exclude` lives outside
  the worktree, so the snapshot records a hash of it (the file Git resolves,
  shared by linked worktrees). Changing it fails closed as `SOURCE_DRIFT`
  rather than silently hiding a change.
- `QUALIFIED_PATH` resolution no longer drops the leading segment of an
  unmatched Rust qualified path (e.g. `ext::helpers::run`) to retry the match
  against the remaining suffix. Without Cargo/workspace metadata, agentctl
  cannot tell an external crate, an unresolved re-export, or a typo apart from
  a real local crate name, so a unique match on the shortened suffix was not
  evidence that the target was correct; it could bind a call to an unrelated
  declaration elsewhere in the workspace that merely shared that suffix. Such
  paths now stay unresolved.

### Changed

- Ignored symlinks, hardlinks, nested repositories, and read-denied paths no
  longer make a run refuse. They are still refused among captured source files.
- Executor writes inside ignored directories are no longer observed.
- At most 20,000 individually ignored files are observed per capture; beyond
  that, the run refuses.
- A run whose stored baseline was captured by 0.1.0-alpha.2 and included
  ignored files reports `SOURCE_DRIFT` and needs an explicit replan.

## [0.1.0-alpha.2]

### Fixed

- Fixed planner startup failures (`runtime file/artifact exceeds size limit`)
  caused by workspace files larger than an obsolete 2 MiB per-file capture
  ceiling. Workspace capture now relies solely on the existing bounded
  aggregate budget (64 MiB total, 20,000 files) rather than an unrelated
  per-file cliff.
- Runtime size-limit errors now identify the responsible subsystem (workspace
  capture, file count, git index, or artifact readback) along with the
  observed and limit values, instead of a generic message.

### Added

- Boundary and end-to-end regression coverage for planner execution and
  workspace capture size limits.

## [0.1.0-alpha.1]

First alpha release. Local, provider-agnostic engineering control plane for
coordinating coding agents, usable for small, well-scoped repositories.

### Added

- Planner → executor → verifier orchestration over a durable, resumable task DAG,
  with verified-only progression and a separate integration verification pass.
- Provider-independent local state: plans, tasks, jobs, diffs, evidence, and
  decisions stored and journaled in a local SQLite database.
- Repository intelligence: an incremental, content-hashed code graph for Rust,
  Python, TypeScript, and JavaScript (symbol lookup, ranked location, callers,
  tests, impact, bounded context packets).
- Structured engineering memory with explicit trust classes and provenance
  (`CANONICAL`, `DERIVED`, `OBSERVED`, `AGENT_NOTE`).
- Provider routing and fallback across configured roles, with project-level
  tightening of machine policy.
- A machine-wide concurrency ceiling (`max_agents`, default 4) across all
  simultaneously active agent jobs.
- Experiment supervision for long-running programs (training runs, benchmarks),
  with structured metric/checkpoint ingestion and deterministic decision
  boundaries that can open new planning requests.
- OS-level sandboxing and capability enforcement: Seatbelt on macOS, Landlock +
  seccomp on Linux, with fail-closed launch refusal when a host cannot enforce
  the required policy. The Windows backend is intentionally unsupported and
  fails closed.
- Observability via `agenttop` (terminal UI) and `agentctl observe`, and
  historical `agentctl analytics`.
- Claude Code and Codex CLI provider adapters.

### Known alpha limitations

- Only Claude Code and Codex CLI adapters exist; Codex token usage is not
  reported.
- Tasks within a workspace run one at a time; there are no parallel worktrees.
- Repositories are limited to 20,000 files and 64 MiB, with no symlinks,
  hardlinks, or submodules in the checkout. Verifier diffs are limited to
  128 KiB.
- The code graph is syntactic and single-file, resolving only a narrow set of
  references.
- Process-tree cleanup on macOS and Linux is best effort; resource limits are
  mostly per process.
- Windows cannot run workers; the backend fails closed.
- agentctl never commits or pushes; you review and commit results yourself.
- No artifact garbage collection yet.

See [README.md](README.md) and [docs/security.md](docs/security.md) for the full
capability list, security model, and platform support matrix.
