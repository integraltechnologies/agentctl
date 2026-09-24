# Changelog

All notable changes to this project are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
agentctl is alpha software; versions before `1.0.0` may include breaking changes
to storage, configuration, or the CLI.

## [Unreleased]

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
