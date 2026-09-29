# agentctl

agentctl is a local, provider-agnostic engineering control plane. You provide
engineering intent; interchangeable AI agents plan, implement and
independently verify the work as one persistent, concurrent, observable and
recoverable system running in your repository.

- **Models** supply engineering intelligence, one fresh invocation at a time.
- **The planner** supplies engineering judgment: tasks, scopes and ordering.
- **agentctl** owns execution truth, authority, coordination and continuity.
- **CodeGraph** owns structural knowledge of accepted source.
- **Verifiers** supply independent acceptance evidence.
- **Durable local state** (`.agentctl/`) owns continuity.

No provider owns any canonical state. Claude Code, Codex, or any runtime that
speaks agentctl's generic protocol can fill any role.

## Status: 0.3.0-alpha

`0.3.0-alpha` is a ground-up rebuild and the first version intended for
dogfooding. It has been exercised end to end through the real CLI (planning,
concurrent and dependent tasks, verification, acceptance, CodeGraph refresh,
integration verification, completion, pause/resume/cancel, timeouts, signals,
crash recovery, working-tree drift) on macOS and on Linux. It is alpha
software: storage, configuration and the CLI may change without migration.
It is not compatible with 0.1.x. See [CHANGELOG.md](CHANGELOG.md).

## How it works

```text
human intent (objective, constraints, completion criteria)
  ↓
planner ─ proposes commands; agentctl validates and applies them atomically
  ↓
validated task DAG (each task: objective, exact file scope, dependencies)
  ↓
executor ─ fresh agent, disposable copy of the repo, owns only its task's files
  ↓
candidate ─ the change agentctl itself observed, installed only where nothing drifted
  ↓
independent verifier ─ fresh agent, judges the candidate over accepted source
  ↓
accepted source ─ published by agentctl's acceptance transaction
  ↓
CodeGraph refresh
  ↓
dependent tasks become eligible (independent ones run concurrently)
  ↓
final integration verification of the assembled result against your intent
  ↓
COMPLETED
```

Details: [docs/architecture.md](docs/architecture.md).

## Requirements

- macOS or Linux. (Windows builds and runs its test suites in CI, but is not
  yet qualified for use.)
- Git, and a Git repository to work in.
- At least one provider CLI on `PATH`: Claude Code (`claude`), Codex
  (`codex`), or your own runtime implementing the
  [generic protocol](docs/providers.md#generic-provider-adapter-adapter--generic).
- Process lifecycle support from [procd](https://github.com/integraltechnologies/procd),
  which agentctl links statically (see below). On Linux, agents run with
  enforced lifecycle only as root or as a user that owns a delegated cgroup v2
  subtree (for example inside a systemd unit with `Delegate=yes`); on macOS
  the lifecycle guarantee is best effort. See [docs/security.md](docs/security.md).

## Building from source

procd is a separate project, pinned as the `third_party/procd` Git submodule
(currently procd v0.1.0). The build compiles that pinned source with procd's
own CMake into Cargo's build directory and links it statically, so procd is
never installed separately. You need a stable Rust toolchain (edition 2024),
CMake 3.16 or later, and a C11 compiler.

```bash
git clone --recurse-submodules https://github.com/integraltechnologies/agentctl.git
cd agentctl
cargo install --locked --path .
```

In an existing clone, run `git submodule update --init` first. Alternatively,
`cargo install --locked --git https://github.com/integraltechnologies/agentctl`
fetches the submodule itself and has the same build prerequisites.

Either way this installs three binaries into the same directory: `agentctl`,
`agenttop` and `agentctl-shim`. agentctl starts providers through
`agentctl-shim` and looks for it beside its own executable, so keep the three
together if you move them.

## Quick start

```bash
cd your-repo
agentctl init
```

`init` asks for the project name, CodeGraph source roots (default `src`), and
the provider, model and reasoning effort of the planner, executor and verifier
roles, then writes `agentctl.toml`, creates `.agentctl/` (added to
`.gitignore`), and builds the initial index of accepted source. Commit
`agentctl.toml`; `.agentctl/` is local.

Create and plan work:

```bash
agentctl plan create "Add input validation to the config loader" \
  --constraint "Keep the public API unchanged" \
  --criterion "Invalid files are rejected with a clear error"
```

The planner decomposes the intent into tasks, each with an exact file scope.
Nothing runs yet. Then:

```bash
agentctl run 1          # execute, verify and accept every task it can
agentctl plan update 1  # replan from how it went; when every task is done,
                        # the planner proposes completion and the
                        # integration verifier runs
agentctl status
```

A plan is `completed` only when the final integration verifier passes its
accepted result as a whole. Review the accepted changes in your working tree
and commit them yourself: agentctl never commits or pushes.

## Everyday commands

| Command | Purpose |
| --- | --- |
| `agentctl plan create <objective>` | Record intent and plan it |
| `agentctl run <plan>` | Run eligible tasks (Ctrl-C ends its agents cleanly) |
| `agentctl plan update <plan>` | Replan from feedback; propose completion |
| `agentctl plan verify <plan>` | Rerun final integration verification |
| `agentctl plan pause` / `resume` / `cancel <plan>` | Control a plan's work |
| `agentctl plan attention <plan>` / `decide ...` | Answer concerns the planner raised |
| `agentctl status` | Where every plan stands |
| `agentctl logs [-f]` | The event log |
| `agenttop` | Live monitor of agents and token usage |
| `agentctl recover` | Settle work left by an agentctl process that died |

Full reference: [docs/cli.md](docs/cli.md).

## Your working tree is safe

agentctl claims a task only while every file in its scope holds exactly the
accepted source, so your uncommitted edits are never overwritten, never used
as an agent's starting point, and never accepted under a task's name; `run`
tells you which files block which task. Paths are literal everywhere
(`src/[slug].tsx` is a file name, not a pattern), and Git-ignored content such
as `target/` is never read or given to agents.

## Supported languages

CodeGraph indexes Rust (`.rs`), Python (`.py`, `.pyi`), JavaScript (`.js`,
`.mjs`, `.cjs`, `.jsx`), TypeScript (`.ts`, `.mts`, `.cts`) and TSX (`.tsx`)
syntactically with Tree-sitter. Files in other languages can still be worked
on; they just have no graph.

## Recovery

If `run` is interrupted normally (Ctrl-C, SIGTERM, closing the terminal),
agentctl ends its agents and records how they ended before exiting. If an
agentctl process is killed outright, `agentctl recover` settles what it left:
nothing new starts on that plan until it does. Settling a provider invocation
requires procd's proof that its processes are gone, which is available on Linux
as root; on macOS, and on Linux as an ordinary user, such an invocation stays
unresolved and its plan stays blocked (fails closed).

## Important alpha limitations

- agentctl does not sandbox providers. They run as you, confined only by their
  own editing modes (Claude Code permission modes, Codex's sandbox). Verifiers
  run your repository's builds and tests. Use agentctl only with repositories
  and providers you trust.
- macOS lifecycle termination is best effort, and recorded as such.
- Killing agentctl outright can leave a plan permanently blocked on hosts
  without procd recovery, with orphaned provider processes you must end
  yourself.
- A human edit to an already-accepted file cannot be adopted as accepted
  source; restore the file before tasks that touch it can run.
- Replanning and proposing completion are started by you (`plan update`).

## Documentation

- [Architecture](docs/architecture.md)
- [Command-line reference](docs/cli.md)
- [Configuration](docs/configuration.md)
- [Providers](docs/providers.md)
- [Security model](docs/security.md)
- [Changelog](CHANGELOG.md)
