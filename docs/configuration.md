# Configuration

agentctl reads two TOML files:

| File | Owner | Purpose |
| --- | --- | --- |
| `~/.config/agentctl/config.toml` | the machine operator | providers, role routing, runtime limits, security authority |
| `<repository>/.agentctl/project.toml` | the repository | invariants, architecture notes, canonical commands and verification, protected paths, tightening-only routing and security limits |

Both files reject unknown fields, missing required values, invalid content, and
unsupported versions, and report the file path in the error. Configuration is
never overwritten: `agentctl init` and `agentctl repo init` only create files that
are missing.

## Locations

| Scope | Default | Override |
| --- | --- | --- |
| Machine configuration | `~/.config/agentctl/config.toml` | absolute `XDG_CONFIG_HOME` |
| Canonical state (SQLite database, runtime artifacts, scratch) | `~/.local/share/agentctl/` (database: `state.sqlite3`) | absolute `XDG_DATA_HOME` |
| Cache | `~/.cache/agentctl/` | absolute `XDG_CACHE_HOME` |
| Project configuration | `<repository>/.agentctl/project.toml` | none |

Relative or empty `XDG_*` values are ignored. An absolute `HOME` is required for
any path without an absolute override. Keep state on a local filesystem that
supports SQLite locking. It must also be outside every workspace; worker launches
are refused otherwise. On Unix, new directories are created `0700` and new files
`0600`. Existing permissions are not silently changed.

Validate your configuration at any time:

```bash
agentctl doctor            # directories, machine config, database health
agentctl repo status       # project config validity for the current checkout
agentctl route executor    # resolved routing for one role, without launching anything
agentctl security doctor   # sandbox capabilities and a live self-test
```

## Machine configuration

`agentctl init` writes this default:

```toml
version = 1
busy_timeout_ms = 5000

[runtime]
timeout_ms = 600000
max_correction_rounds = 2

[runtime.providers]

[runtime.roles]

[runtime.concurrency]
max_agents = 4

[runtime.security.resources]
max_processes = 2048
max_open_files = 8192
strict = false

[runtime.security.experiment_events]
max_events_per_attempt = 1000000
max_event_bytes_per_attempt = 536870912
```

A representative configuration with two providers, a fallback, and a toolchain
read root:

```toml
version = 1
busy_timeout_ms = 5000

[runtime]
timeout_ms = 600000
max_correction_rounds = 2

[runtime.concurrency]
max_agents = 4

[runtime.providers.claude]
adapter = "claude"
executable = "/absolute/path/to/claude"

[runtime.providers.codex]
adapter = "codex"
executable = "/absolute/path/to/codex"

[runtime.roles.planner]
provider = "claude"

[runtime.roles.executor]
provider = "claude"

[runtime.roles.verifier]
provider = "codex"

[runtime.profiles.executor]
max_fallback_attempts = 1
fallbacks = [{ provider = "codex" }]

[runtime.security]
read_roots = ["/Users/you/.rustup", "/Users/you/.cargo"]
env = { RUSTUP_HOME = "/Users/you/.rustup", CARGO_HOME = "/Users/you/.cargo" }
```

Workers run with a private scratch `HOME`. Tools that find their installation
through `HOME` (rustup, pyenv, nvm, …) need both a read root and an explicit
variable that points at the real location.

### Top level

| Field | Default | Rules |
| --- | --- | --- |
| `version` | `1` | required; must be `1` |
| `busy_timeout_ms` | `5000` | required; 1–60000. SQLite busy timeout. |

### `[runtime]`

| Field | Default | Rules |
| --- | --- | --- |
| `timeout_ms` | `600000` (10 minutes) | 1–3600000. Upper bound for every role process; profiles can only lower it. |
| `max_correction_rounds` | `2` | 0–2. Maximum linked replacement plans before human escalation is required. |

### `[runtime.providers.<name>]`

`<name>` is your identifier for the provider, used by roles and routing. See
[providers.md](providers.md) for adapter behavior and authentication.

| Field | Rules |
| --- | --- |
| `adapter` | `"claude"` or `"codex"`; no other adapters exist |
| `executable` | absolute path to the installed CLI |
| `[...authentication] mode` | `"AUTO"` (default), `"NATIVE"`, or `"API_KEY"` |
| `[...authentication] api_key_env` | optional environment variable **name** (uppercase letters, digits, `_`); required for `API_KEY` |

### `[runtime.roles.<role>]`

Maps a role to a provider. The built-in roles are `planner`, `executor`,
`verifier`, `recon`, and `reviewer`. Only `planner`, `executor`, and `verifier`
can be launched by the runtime today. `recon`, `reviewer`, and custom roles can
be configured and inspected.

| Field | Rules |
| --- | --- |
| `provider` | required; must name a configured provider |
| `model` | optional opaque string passed to the CLI (≤128 bytes) |
| `effort` | optional opaque string passed to the CLI (≤128 bytes) |

`agentctl route check` validates every known role, including `recon` and
`reviewer`, and exits nonzero if any of them is unrouted. To check only the roles
you use, run `agentctl route planner`, `agentctl route executor`, and
`agentctl route verifier`.

### `[runtime.profiles.<role>]`

Optional per-role behavior. Every field is optional. Only supplied fields
override lower layers, and lists replace rather than append.

| Field | Rules |
| --- | --- |
| `provider`, `model`, `effort` | override the role route |
| `fallbacks` | up to four `{ provider, model?, effort? }` alternatives, tried in order |
| `max_fallback_attempts` | 0–4; alternatives tried after the primary (default 4, bounded by the list) |
| `objective` | replaces the role's built-in objective (1–4096 bytes) |
| `instructions` | up to 8 short instruction strings (≤4096 bytes each) |
| `context_bytes` | 1–262144; compiled prompt budget (default 262144) |
| `timeout_ms` | 1–3600000; capped by `runtime.timeout_ms` |
| `advisory_tokens` | positive; recorded as an **advisory** target and not enforced by either CLI |
| `read_only` | executors only may be writable; other roles are always read-only |
| `network` | `false` removes network from the role's processes |

Fallback happens only on mechanical failures before a job starts working: missing
executable, unavailable authentication, unsupported capability, or an OS
not-found/permission error. Timeouts, bad output, nonzero exits, verifier REJECT,
and capacity exhaustion never trigger fallback. Each attempt is a new job with the
same permissions.

### `[runtime.concurrency]`

| Field | Default | Rules |
| --- | --- | --- |
| `max_agents` | `4` | 1–256. Hard ceiling on simultaneously active (queued or running) agent jobs across the whole machine database: planners, executors, verifiers, and integration verifiers. A launch beyond it fails with `AGENT_CAPACITY_EXCEEDED`. |

Projects can only lower this value (see [Project routing](#routing)). Overrides,
routing profiles, and provider output cannot raise it. Experiments are not agents
and do not count against it.

### `[runtime.security]`

Machine security authority. See [security.md](security.md) for how it is enforced.

| Field | Default | Rules |
| --- | --- | --- |
| `read_roots` | `[]` | up to 64 absolute paths readable (and executable) by every worker; cannot be `/`. Use for toolchains under your home directory (rustup, pyenv, nvm, …). |
| `inherit_env` | `[]` | up to 64 variable **names** copied from the controller environment into workers |
| `env` | `{}` | up to 64 explicit non-secret `NAME = "value"` pairs (values ≤4096 bytes) |

Names in `inherit_env` and `env` must be valid identifiers. They must not be
loader-injection variables (`LD_*`, `DYLD_*`), names agentctl reserves (`PATH`,
`HOME`, `TMPDIR`, `XDG_*_HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
`CARGO_TARGET_DIR`, `LANG`, `GIT_*` settings, `AGENTCTL_*`, …), or
credential-looking names. Credentials never travel through ambient worker
environment.

#### `[runtime.security.resources]`

| Field | Default | Range |
| --- | --- | --- |
| `max_memory_bytes` | unset | 64 MiB – 16 TiB |
| `max_cpu_seconds` | unset | 1 – 2592000 (30 days) |
| `max_processes` | `2048` | 16 – 1000000 (additional processes above the user's current count on Unix) |
| `max_open_files` | `8192` | 64 – 1048576 |
| `max_file_size_bytes` | unset | 1 MiB – 16 TiB |
| `strict` | `false` | when `true`, every configured limit becomes a hard requirement; an `UNSUPPORTED` one refuses launches |

With `strict = true` and `max_memory_bytes` set, every launch on macOS is refused,
because macOS cannot enforce memory limits.

### `[runtime.context]`

Machine-owned budgets of the planner-mediated context relay (see
[architecture.md](architecture.md#context-relay)). Hard maxima are compiled in;
configuration can only choose values inside them, and neither provider output,
planner output nor project policy can raise them.

| Field | Default | Rules |
| --- | --- | --- |
| `max_rounds` | `2` | 0–4. Context rounds granted per executor task. Each round is a new provider job; `0` disables executor expansion. |
| `verifier_max_rounds` | `1` | 0–2. Context rounds granted per verification (packet or integration), on a budget independent of the executor's. |
| `max_round_bytes` | `16384` | 1024–32768. Bytes one round's ContextDelta may carry. A worker's own `max_bytes` can only ask for less. |
| `max_task_bytes` | `49152` | 1024–98304, and at least `max_round_bytes`. Cumulative delta bytes per task or verification. |
| `max_escalations` | `1` | 0–2. Planner escalations per executor task. Verifiers never escalate. |
| `visibility` | `"workspace"` | `"workspace"` or `"issued"`. See [security.md](security.md#issued-context-visibility); `issued` is opt-in. |

```toml
[runtime.context]
max_rounds = 2
verifier_max_rounds = 1
max_round_bytes = 16384
max_task_bytes = 49152
max_escalations = 1
visibility = "workspace"
```

Exhausting a budget does not degrade quietly: the request is denied, the task
blocks with the persisted reason, and the decision returns to a planner.

#### `[runtime.security.experiment_events]`

| Field | Default | Range |
| --- | --- | --- |
| `max_events_per_attempt` | `1000000` | 1 – 100000000 |
| `max_event_bytes_per_attempt` | `536870912` (512 MiB) | 64 KiB – 64 GiB |

## Project configuration

`agentctl repo init` creates a minimal `.agentctl/project.toml`. Commit it: the
runtime only adopts plans prepared from a clean, committed checkout.

```toml
version = 1
display_name = "example"

[invariants.stable-api]
description = "Public function signatures in src/lib.rs must not change"

[architecture.local-only]
description = "No network services"

[commands.test]
program = "cargo"
args = ["test", "--locked", "--offline"]
cwd = "."

[verification.test]
description = "Unit tests pass"
command_refs = ["test"]

[[protected]]
path = "data/private"
deny_read = true
deny_write = true
reason = "Customer data"

[routing]
allowed_providers = ["claude", "codex"]
max_agents = 2

[routing.profiles.executor]
instructions = ["Preserve the public cache API."]

[security]
max_processes = 512
max_cpu_seconds = 3600
```

Declaration keys (the `KEY` in `[commands.KEY]` and similar) are 1–128 ASCII
characters matching `[A-Za-z0-9][A-Za-z0-9._:-]*`.

The file generated by `repo init` contains `protected = []`. To add protected
paths, replace that line with `[[protected]]` tables. TOML does not allow both in
the same file.

| Section | Fields | Notes |
| --- | --- | --- |
| top level | `version` (required, `1`), `display_name` (optional, nonblank) | |
| `[invariants.KEY]` | `description` | Critical constraints. Every project invariant is attached to every planning request and must be covered by verification. |
| `[architecture.KEY]` | `description` | Architecture constraints shown to planners. |
| `[commands.KEY]` | `program`, `args`, `cwd` | Structured argv, never a shell string. `cwd` is repository-relative (`.` allowed). |
| `[verification.KEY]` | `description`, `command_refs` | Canonical checks. Every reference must name a `[commands]` key. Plans can only require checks declared here. |
| `[[protected]]` | `path`, `deny_read`, `deny_write`, `reason` | Normalized repository-relative path or subtree. At least one of `deny_read`/`deny_write` must be true. Read-denied paths are also excluded from indexing and from plan scope. |
| `[routing]` | see below | Tightening only. |
| `[security]` | see below | Tightening only. |

Checks run inside the sandbox with a read-only checkout, no network, no provider
credentials, and scratch-only build output (`CARGO_TARGET_DIR` is set). Configure
them to work offline, for example `cargo test --offline`, and grant toolchains
under your home directory through machine `read_roots`.

### Routing

| Field | Default | Effect |
| --- | --- | --- |
| `allowed_providers` | unset (all) | Hard allowlist for primaries, fallbacks, and explicit overrides. Forbidden configured candidates are skipped in order and recorded as policy skips. A forbidden explicit override fails. |
| `read_only` | `false` | Makes every role read-only, including executors. |
| `deny_network` | `false` | Removes network from all role processes and experiments in this repository. Provider CLIs normally need network to reach their service. |
| `max_context_bytes` | unset | 1–262144; lowers every role's prompt budget. |
| `max_agents` | unset | 1–256; lowers the machine `max_agents` (effective value is the minimum). |
| `profiles.<role>` | none | Same fields as machine profiles. Provider/model/effort/fallbacks/instructions/objective override lower layers. `context_bytes` and `timeout_ms` can only lower, `read_only` can only turn on, and `network` can only turn off. |

### Security

`[security]` in `project.toml` can only lower machine ceilings. Every field is an
optional positive integer: `max_memory_bytes`, `max_cpu_seconds`,
`max_processes`, `max_open_files`, `max_file_size_bytes`,
`max_experiment_events`, and `max_experiment_event_bytes`. The effective value is
the minimum of the machine and project values. There is deliberately no project
field that can grant paths, environment, or network.

## Precedence and trust

Role resolution is layered:

```text
built-in role semantics
  < [runtime.roles.<role>]         (machine)
  < [runtime.profiles.<role>]      (machine)
  < [routing.profiles.<role>]      (project; tightening-only for security-relevant fields)
  < --override role:provider[:model]  (explicit command-line choice)
then hard project limits apply: allowed_providers, read_only, deny_network,
max_context_bytes, max_agents
```

Planner output, provider output, and memory content can never change routing,
permissions, or limits.

## When changes take effect

- **Machine configuration** is read once per controller invocation (`run plan`,
  `run resume`, `run planner`). Jobs in that invocation use it; the next
  invocation uses whatever is on disk then.
- **Project configuration** is part of the plan's source assumptions. Plans record
  a hash of the normalized project policy. A change after activation is detected
  before each launch and each check (`SOURCE_DRIFT`) and requires a new plan. It
  is never reloaded mid-run.
