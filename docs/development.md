# Development

## Prerequisites

- Rust 1.88 or newer (edition 2024). The lockfile pins every dependency.
- Git on `PATH`.
- No external services. SQLite is bundled and built from source by `rusqlite`.
- Running workers or the native sandbox tests requires macOS or Linux (see
  [security.md](security.md#platform-backends)). The rest of the test suite does
  not depend on a working host sandbox.

## Everyday commands

```bash
cargo build --locked
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Run the binaries from the build tree:

```bash
cargo run -- --help
cargo run -- --version
cargo run --bin agenttop -- --once --width 100 --height 30
```

To try commands without touching your real state, point the XDG variables at a
scratch directory. The values must be absolute paths.

```bash
export XDG_CONFIG_HOME=/tmp/agentctl-dev/config
export XDG_DATA_HOME=/tmp/agentctl-dev/data
export XDG_CACHE_HOME=/tmp/agentctl-dev/cache
cargo run -- init
```

There is no hosted CI configuration in the repository yet. Run the checks above
before submitting changes.

## Native sandbox tests

Tests that need real host enforcement are `#[ignore]`d:

```bash
cargo test --locked -- --ignored
```

Run them on a macOS or Linux host, as a non-root user, and not inside another
sandbox. agentctl refuses to launch workers or drive Git as root, and nested
Seatbelt fails on macOS. To exercise the Linux backend from another OS, run the
suite in a Linux container (Rust 1.88+, kernel with Landlock) as an unprivileged
user. These tests use fake executables and disposable repositories. They never
call a model.

## Tests

| File | Covers |
| --- | --- |
| `tests/protocol.rs` | wire contracts, DAG and lifecycle validation, checked-in schema drift |
| `tests/substrate.rs` | paths, configuration, repository identity, SQLite store, migrations (including the v1 fixture in `tests/fixtures/substrate-v1.sql`) |
| `tests/graph.rs` | code graph extraction, incremental indexing, freshness, queries |
| `tests/memory.rs` | engineering memory trust, provenance, search, and retrieval |
| `tests/planning.rs` | planning requests, plan import and validation, lifecycle, completion guards |
| `tests/runtime.rs` | end-to-end orchestration with fake adapters and checks |
| `tests/runtime_concurrency.rs` | the machine-wide concurrent-agent limit |
| `tests/routing.rs` | role policy, precedence, fallback, prompt compilation |
| `tests/observe.rs` | observation projections, liveness, usage series, agenttop rendering |
| `tests/analytics.rs` | historical analytics |
| `tests/experiment.rs`, `tests/experiment_decisions.rs` | experiment processes, event ingestion, decisions, planner wakeups |
| `tests/security.rs` | policy compilation, capability checks, environment rules |
| `src/local/security/native_tests.rs` | native sandbox enforcement (ignored by default) |

Tests must not make live or paid provider calls. Use the injectable
`ProviderAdapter` and `CheckLauncher` fakes that the existing tests use.

## Schemas

Rust types are the source of truth for the versioned JSON contracts. The files in
`schemas/` are generated, checked in, and compared byte-for-byte by the tests.
After changing a contract, regenerate them and review the diff:

```bash
cargo run -- schemas generate
```

Wire contracts are protocol version `1`. Incompatible changes need a new wire
version rather than an in-place edit.

## Database migrations

The SQLite schema version lives in `src/local/migrations.rs` (`SCHEMA_VERSION`).
Migrations are additive, run in a single transaction on writable opens, verify
their guards, and roll back entirely on conflict. Read-only opens never migrate.
Do not rewrite existing payloads or history in a migration. Add a regression test
that migrates an older database.

## Source layout

```text
src/
  main.rs              agentctl CLI entry point and help text
  bin/agenttop.rs      agenttop entry point
  protocol.rs          versioned wire contracts (PlanPacket, TaskPacket, …)
  validation.rs        semantic validation for contracts
  lifecycle.rs         task/job lifecycle rules
  schema.rs            JSON Schema generation and `protocol validate`
  local/
    cli.rs             command dispatch, doctor, security doctor
    config.rs          machine and project configuration
    paths.rs           XDG paths and safe file handling
    repository.rs      Git discovery and hardened Git subprocesses
    store.rs           SQLite store and journal
    migrations.rs      schema migrations
    graph/             code graph (Tree-sitter extraction, indexing, queries)
    memory/            engineering memory
    planning/          planning requests, plan import/validation, completion
    runtime/           orchestration engine, adapters, routing, prompts, experiments
    security/          sandbox policy and platform backends
    observe/           read-only observation projection and usage series
    analytics/         historical analytics
    agenttop.rs        terminal UI
    terminal.rs        safe rendering of untrusted text
```

## Expectations for changes

- Keep the documented authority boundaries. Project configuration can only
  tighten machine policy. Model output is never authority. Only `VERIFIED` tasks
  satisfy dependencies. Launches fail closed when isolation is missing.
- Keep `cargo fmt`, `clippy -D warnings`, and the full test suite clean.
- Update the relevant document in `docs/` when behavior, configuration, or
  commands change.
