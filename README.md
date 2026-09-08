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

## Stage 0

This repository currently provides one Rust 2024 crate with a library and a tiny CLI:

- Versioned JSON contracts for plans, tasks, results, verification, resume, evidence,
  jobs, events, probing, token usage, and experiments, plus memory trust/provenance.
- Task DAG validation, task/job lifecycle rules, and pure verification/completion guards.
- Generated JSON Schemas in `schemas/` and regression tests for the protocol invariants.

It does **not** implement repository indexing, graph parsing, Tree-sitter, LSP,
memory persistence, SQLite, provider adapters or integrations, agent launching,
orchestration, autonomous loops, daemons, an experiment runner, token collection,
`agenttop`/TUI, MCP, web UI, remote services, networking, embeddings, or a vector DB.

## Development

Requires stable Rust 1.88 or newer. No runtime services are needed.

```sh
cargo build --locked
cargo run -- --version
cargo run -- schemas generate
cargo run -- schemas generate --output /tmp/agentctl-schemas
cargo run -- protocol validate task path/to/task.json
cargo run -- --help

cargo test --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

`protocol validate` checks a single document's structure and semantic invariants.
The library's `PlanPacket::validate_task_transition`, `task_is_runnable`, and
`validate_completion` also check caller-supplied lifecycle state and verification
packets. They do not execute work or persist state.

Rust types are canonical. Regenerate and review schemas whenever contracts change;
tests fail if checked-in schemas drift. See [the architecture contract](docs/architecture.md)
for versioning, lifecycle semantics, trust boundaries, and directory conventions.
