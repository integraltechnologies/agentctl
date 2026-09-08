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
provider/model identities are optional opaque metadata for future adapters.

## Stage 2

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

The accepted Stage 0 protocol and all 13 public schemas remain unchanged.

It does **not** implement LSP, shared-memory retrieval/persistence, provider adapters or integrations, agent launching,
orchestration, autonomous loops, daemons, an experiment runner, token collection,
`agenttop`/TUI, MCP, web UI, remote services, networking, embeddings, or a vector DB.

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

Rust types are canonical. Regenerate and review schemas whenever contracts change;
tests fail if checked-in schemas drift. See [the architecture contract](docs/architecture.md)
for versioning, lifecycle semantics, trust boundaries, and directory conventions.
