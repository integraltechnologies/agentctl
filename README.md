# agentctl

agentctl is a local, provider-agnostic engineering control plane for coordinating
coding agents. It keeps planning, execution state, verification, repository
intelligence, and engineering memory outside any individual model or provider
session.

It drives installed coding-agent CLIs (currently Claude Code and Codex) as
sandboxed, disposable workers. The durable record of the work lives in a local
database that you own.

> **Status: alpha.** Interfaces, storage, and configuration may still change. See
> [Current status](#current-status).

## What agentctl does

- **Planner → executor → verifier orchestration.**
  - A planner turns an objective into a DAG of small TaskPackets.
  - Each task is implemented by a fresh executor, then judged by a fresh,
    independent verifier.
  - The verifier sees only the task, its invariants, the actual diff, and the
    captured check evidence. It never sees the executor's reasoning.
- **Verified-only progression.** A task's dependents run only after a verifier
  `PASS`. Executor success alone never unlocks work.
- **Integration verification.** When every task is verified, the runtime runs the
  project's integration checks and a separate verifier over the combined diff. Only
  then can the plan complete.
- **Provider-independent state.**
  - Plans, tasks, jobs, diffs, evidence, and decisions are stored locally and
    journaled.
  - Provider conversations are never resumed or replayed.
  - You can switch providers, restart, or resume without losing engineering state.
- **Repository intelligence.** An incremental, content-hashed code graph for Rust,
  Python, TypeScript, and JavaScript supports symbol lookup, ranked location,
  callers, tests, impact, and bounded context packets. It runs locally, without a
  language server or a model.
- **Structured engineering memory.** Durable decisions, derived facts, observed
  evidence, and agent notes carry explicit trust and provenance. Low-trust notes
  never silently become authority.
- **Resumable execution.**
  - `run resume` continues from durable checkpoints.
  - Verified tasks never run again.
  - Uncertain work blocks rather than being assumed successful.
- **Provider routing and fallback.** Roles map to configured providers and models.
  An ordered fallback chain covers mechanical launch failures. Projects can
  restrict providers but cannot add them.
- **Global concurrency limit.** A machine-wide ceiling (`max_agents`, default 4)
  applies to simultaneously active agent jobs across every workspace.
- **Experiments.** agentctl can supervise long-running programs such as training
  runs and benchmarks in the same sandbox. It ingests their structured metric and
  checkpoint events, evaluates deterministic threshold boundaries, and can open a
  new planning request when a boundary fires.
- **Sandboxing and capability enforcement.**
  - Every worker runs under an OS sandbox (Seatbelt on macOS, Landlock + seccomp on
    Linux).
  - Workers get confined filesystem access, an allowlisted environment, optional
    network denial, resource limits, and process-tree cleanup.
  - A launch is refused when the host cannot enforce what it requires.
- **Observability.** The `agenttop` terminal UI and `agentctl observe` provide
  read-only views of sessions, agent trees, task DAGs, recent events, and token
  usage.
- **Analytics.** `agentctl analytics` reports historical token usage, verifier
  decisions, fallback and correction behavior, and latency. Unknowns are kept
  explicit.

## Why agentctl exists

Coding-agent harnesses are powerful, but the most important engineering state
often ends up inside provider-specific sessions: the plan, which tasks are done,
what was verified and against what evidence, and what was decided. That state is
hard to inspect, hard to resume, and tied to one vendor's conversation.

agentctl puts a durable control plane underneath the harnesses:

- **Planning is explicit and bounded.** Planner input is a frozen, inspectable
  packet.
- **Execution is observable and resumable.** Canonical state lives in SQLite, not
  in chat history.
- **Acceptance is independent.** Fresh verifiers judge actual diffs and captured
  evidence.
- **Providers are interchangeable workers**, not the system of record.

## Current status

agentctl is **alpha** software (version 0.1.0). It is usable for small,
well-scoped repositories, but expect rough edges and breaking changes.

| Platform | Worker execution | Notes |
| --- | --- | --- |
| macOS | supported | Seatbelt via `/usr/bin/sandbox-exec`. Memory limits are not enforceable. |
| Linux | supported when Landlock (kernel 5.13+, enabled) and seccomp are available | Executors cannot create or remove entries directly in the workspace root. Metadata writes are not mediated. |
| Windows | **not supported** | The backend fails closed: filesystem and network confinement are not implemented, so every worker launch is refused. |

Run `agentctl security doctor` to see exactly what your host enforces.

Alpha limitations to know about:

- Only Claude Code and Codex CLI adapters exist. Codex token usage is not
  reported.
- Tasks within a workspace run one at a time. There are no parallel worktrees.
- The runtime supports modest repositories: up to 20,000 files and 64 MiB, with no
  symlinks, hardlinks, or submodules in the checkout. Verifier diffs are limited to
  128 KiB.
- The code graph is syntactic and single-file. It resolves only a narrow set of
  references.
- Process-tree cleanup on macOS and Linux is best effort, and resource limits are
  mostly per process. See [docs/security.md](docs/security.md#known-limitations).
- agentctl never commits or pushes. You review and commit results yourself.
- There is no artifact garbage collection yet.

## Installation

Requirements:

- Rust 1.88 or newer (install with [rustup](https://rustup.rs));
- Git on `PATH`;
- macOS, or Linux with Landlock and seccomp, to run workers;
- the Claude Code and/or Codex CLI installed and logged in, for the providers you
  plan to use.

agentctl is not published to crates.io or any package manager. Install it from
source:

```bash
git clone https://github.com/integraltechnologies/agentctl.git
cd agentctl
cargo install --locked --path .
```

This installs two binaries into `~/.cargo/bin`: `agentctl` and `agenttop`. SQLite
is bundled, so no database server is needed.

## Quick start

This walkthrough takes one small change through planning, execution, verification,
and integration. It uses one provider (Claude Code) for every role. See
[docs/providers.md](docs/providers.md) to use Codex or mix providers.

### 1. Initialize agentctl and check the sandbox

```bash
agentctl init              # creates ~/.config/agentctl/config.toml and the local database
agentctl security doctor   # must end with "Hard requirements: all enforced"
```

### 2. Configure a provider

agentctl reuses the provider CLI's own login and never stores credentials. Log in
first with `claude auth login` if you have not already. Then map the roles to the
CLI:

```bash
cat >> ~/.config/agentctl/config.toml <<EOF

[runtime.providers.claude]
adapter = "claude"
executable = "$(command -v claude)"

[runtime.roles.planner]
provider = "claude"

[runtime.roles.executor]
provider = "claude"

[runtime.roles.verifier]
provider = "claude"
EOF
```

If you set `XDG_CONFIG_HOME`, the file is at
`$XDG_CONFIG_HOME/agentctl/config.toml` instead.

### 3. Verify the configuration

```bash
agentctl doctor
agentctl provider doctor      # CLI version and token-free login status; sends no prompt
agentctl route executor       # resolved route and permissions; launches nothing
```

### 4. Register your repository

Start from a Git checkout with a clean, committed working tree:

```bash
cd /path/to/your/repo
agentctl repo init            # creates .agentctl/project.toml
```

Declare at least one canonical check. The verifier requires evidence from checks
declared here, and planners cannot invent their own. Replace the command with your
project's offline test command:

```bash
cat >> .agentctl/project.toml <<'EOF'

[commands.test]
program = "cargo"
args = ["test", "--locked", "--offline"]
cwd = "."

[verification.test]
description = "Unit tests pass"
command_refs = ["test"]
EOF

git add .agentctl/project.toml
git commit -m "Add agentctl project policy"
agentctl repo index
```

Checks run in the sandbox with a read-only checkout, no network, and a private
scratch `HOME`. If your toolchain lives under your home directory, grant it with
`[runtime.security] read_roots` and `env` (for example `RUSTUP_HOME`). See
[docs/configuration.md](docs/configuration.md).

### 5. Describe the work

```bash
agentctl plan prepare --objective 'Add a --verbose flag that prints each processed file'
```

This prints `Prepared request:<id>`. The request freezes the objective, your
project invariants, and bounded code and memory context.

### 6. Plan

```bash
agentctl run planner request:<id>     # the planner produces a task DAG; imported as VALIDATED
agentctl plan list                    # find the plan ID
agentctl plan tasks <plan-id>         # review tasks, scopes and checks before running anything
agentctl plan activate <plan-id>
```

### 7. Run

```bash
agentctl run plan <plan-id> --dry-run
agentctl run plan <plan-id>
```

`run plan` stays in the foreground. For each ready task, it runs an executor, the
declared checks, and an independent verifier. It then runs integration checks and
an integration verifier.

### 8. Watch progress

From another terminal:

```bash
agenttop
agentctl run status <plan-id>
```

### 9. Inspect the verified result

```bash
agentctl plan show <plan-id>          # COMPLETE only after integration verification passes
agentctl observe tasks
git status && git diff                # agentctl never commits; review and commit yourself
agentctl analytics summary
```

If a task is rejected or blocked, the run stops. Prepare a replacement plan and
link it with `agentctl run replace <old-plan-id> <new-plan-id>`, or resume an
interrupted run with `agentctl run resume <plan-id>`. See [docs/cli.md](docs/cli.md).

## How it works

```text
user intent
   │  plan prepare: freeze objective, invariants, bounded graph/memory context
   ▼
planning ── planner worker ──▶ ExecutionPlan (task DAG + verification contracts)
   │  import → VALIDATED → activate → ACTIVE
   ▼
for each task whose prerequisites are VERIFIED:
   executor worker ──▶ actual diff captured and scope-checked
   project checks  ──▶ evidence (sandboxed, offline)
   verifier worker ──▶ PASS → VERIFIED (unlocks dependents) │ REJECT → blocked, replan
   ▼
integration: combined diff + integration checks + fresh integration verifier
   ▼
COMPLETE
```

agentctl owns the canonical state at every step. Provider workers receive bounded,
explicit inputs, return strict JSON, and are discarded. Their claims are checked
against what actually happened on disk and in the checks. Every transition is
recorded in an append-only journal and guarded in the database. An interrupted run
therefore resumes from durable facts, not from a conversation.

See [docs/architecture.md](docs/architecture.md) for components and authority
boundaries.

## Security model

- **Fail-closed capability checks.**
  - Every launch is compiled into an OS-neutral policy and checked against what the
    host backend reports it can enforce.
  - A missing capability refuses the launch; there is no unsandboxed fallback.
  - agentctl refuses to launch workers as root.
- **Filesystem confinement.**
  - Workers can read only OS and toolchain roots, the workspace, a private scratch
    directory, and roots you grant.
  - Only executors, experiments, and scratch can be written.
  - Git metadata, agentctl state, provider homes, and common credential stores are
    always denied.
- **Network.** Checks are always offline. Experiments are offline unless you allow
  network. Provider frontends have network access, which roles and projects can
  remove.
- **Environment isolation.**
  - Workers receive an allowlisted environment, never the ambient one.
  - Credential-looking and loader-injection variables are rejected.
  - Checks and experiments never receive provider credentials.
- **Process and resource containment.**
  - Timeouts, bounded output capture, and resource limits apply to every worker.
  - Process-group and marker/sentinel sweeps find and kill escaped descendants.
    Cleanup that cannot be proven is reported, not assumed.
- **Machine policy versus project policy.** The machine configuration is the
  authority. `.agentctl/project.toml` can only **tighten** it: fewer providers,
  lower limits, read-only roles, no network, extra protected paths. It can never
  grant paths, environment, network, or a higher concurrency ceiling.

`agentctl security doctor` prints the host's capability report and runs a live
self-test.

This is not perfect isolation. Worker output is treated as untrusted, but same-user
host compromise is out of scope. Readable workspace content is visible to every
worker, and cleanup of deliberately daemonizing processes is best effort. Read
[docs/security.md](docs/security.md) for the threat model, platform details, and
known limitations.

## Configuration

The machine configuration lives in `~/.config/agentctl/config.toml`. The optional
keys below override the defaults shown.

```toml
version = 1
busy_timeout_ms = 5000

[runtime]
timeout_ms = 600000          # per worker process
max_correction_rounds = 2    # explicit replacement plans before human escalation

[runtime.concurrency]
max_agents = 4               # machine-wide active agent jobs

[runtime.providers.claude]
adapter = "claude"
executable = "/absolute/path/to/claude"

[runtime.roles.planner]
provider = "claude"
[runtime.roles.executor]
provider = "claude"
[runtime.roles.verifier]
provider = "claude"
```

The project configuration lives in `.agentctl/project.toml`, committed with your
code. It holds invariants, architecture notes, canonical commands and
verification, protected paths, and tightening-only `[routing]` and `[security]`
limits. See [docs/configuration.md](docs/configuration.md) for every field,
default, and trust boundary.

## Observability

- `agenttop` is a read-only terminal UI. It shows a rolling tokens-per-minute graph,
  the agent tree, the task DAG with `N/M VERIFIED` progress, a per-worker probe, and
  recent events. `agenttop --once` renders a single frame as text.
- `agentctl observe snapshot|sessions|agents|tasks|events|experiments|usage` returns
  the same data, as JSON with `--json`.
- `agentctl run status <plan-id>` shows run and job state for one plan.
- `agentctl analytics summary|usage|roles|routes|corrections` produces historical,
  descriptive metrics. Token provenance (`EXACT`/`ESTIMATED`/`UNKNOWN`) is kept, and
  missing data is never counted as zero.

Liveness is reported honestly. A job is `LIVE` only when the controller that owns
the process has just confirmed it. Other observers show `UNKNOWN`.

## Documentation

| Document | Contents |
| --- | --- |
| [docs/cli.md](docs/cli.md) | every command, with behavior and limits |
| [docs/configuration.md](docs/configuration.md) | machine and project configuration reference |
| [docs/providers.md](docs/providers.md) | Claude Code and Codex adapters, authentication |
| [docs/security.md](docs/security.md) | threat model, capabilities, platform backends, limitations |
| [docs/architecture.md](docs/architecture.md) | components, state, lifecycles, authority boundaries |
| [docs/development.md](docs/development.md) | building, testing, and contributing |

## Roadmap

These directions follow from current design limits. They are not commitments or
dates.

- **Stronger cross-platform sandboxing.** Filesystem and network confinement on
  Windows, and kernel-level process-tree and aggregate resource containment on
  Linux and macOS.
- **Parallel execution.** Isolated, managed worktrees so independent verified-ready
  tasks can run concurrently.
- **Richer repository intelligence.** Cross-file resolution with correct
  invalidation, and more languages.
- **Provider integrations.** Broader authentication and platform support, and more
  complete token telemetry.
- **Orchestration and experiments.** More decision-boundary types beyond metric
  thresholds.
- **Operations.** Artifact retention and garbage collection, and repository
  relocation handling.
- **Observability.** Finer-grained indexing and runtime activity events.

## Contributing

Contributions are welcome. Before opening a change, run:

```bash
cargo build --locked
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Tests are offline and never call a model; they use fake provider adapters. Native
sandbox tests are opt-in: `cargo test --locked -- --ignored` on a macOS or Linux
host, as a non-root user. Regenerate `schemas/` with `cargo run -- schemas
generate` when you change a contract.

When you change behavior, configuration, or commands, keep the documented authority
boundaries and update the relevant document in `docs/`. See
[docs/development.md](docs/development.md).

## License

agentctl is licensed under the Mozilla Public License 2.0 (`MPL-2.0`). See
[LICENSE](LICENSE) for the full text.
