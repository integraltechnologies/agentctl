# Configuration

agentctl `0.3.0-alpha` is configured by one file, `agentctl.toml`, at the
project root. This document describes exactly what the current parser
accepts.

## Location and purpose

- `agentctl.toml` marks the project root. Every command finds it by walking up
  from the current directory.
- It is portable and meant to be version-controlled. Its validity never
  depends on the machine: a provider whose command is missing is a warning at
  `init`, and a failure when an invocation is launched, never a configuration
  error.
- It is never engineering source. No task may change it and CodeGraph never
  indexes it, whatever the source roots. Final integration verification always
  binds its exact bytes as a repository input.
- It is read when a command starts. A change affects the next command only;
  for example `agents.max_concurrency` is read once when `run` starts.

`agentctl init` writes it interactively (see [cli.md](cli.md#agentctl-init)).
Local, machine-specific state lives in `.agentctl/`, which is never
configuration.

## Complete example

```toml
[project]
name = "demo"
version = "0.1.0"

[codegraph]
roots = ["src", "tests"]

[agents]
max_concurrency = 4
invocation_timeout_minutes = 90

[agents.planner]
provider = "claude"
model = "claude-opus-5-5"
reasoning_effort = "high"

[agents.executor]
provider = "codex"
model = "your-codex-model-id"
reasoning_effort = "medium"

[agents.verifier]
provider = "house-runtime"
model = "house-model-large"
reasoning_effort = "high"

[providers.house-runtime]
adapter = "generic"
command = "/opt/house/bin/house-runtime"
args = ["--quiet"]
env = ["HOUSE_RUNTIME_TOKEN"]
```

Model identifiers are opaque to agentctl and passed to the provider verbatim;
the values above are placeholders.

## Validation rules that apply everywhere

- **Unknown keys are errors**, in every table (a typo such as `modle` is
  rejected).
- **Text values** (names, versions, providers, models, commands, environment
  variable names) must be non-empty and have no leading or trailing
  whitespace.
- The whole file is rejected, with the reason, if any rule fails. Nothing is
  partially applied.

## `[project]` (required)

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `name` | text | yes | The project's name. |
| `version` | text | yes | The project's version. Free-form. |

## `[codegraph]` (required)

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `roots` | array of strings | yes | The directories whose files are accepted source and indexed by CodeGraph. |

`roots` bounds what agentctl treats as **source**: the baseline captured by
`init`, what CodeGraph indexes, and what task scopes may name. Every path a
planner assigns to a task must lie inside a root.

Each root:

- is relative to the project root and uses `/` as the separator
  (`\` is rejected; an absolute path or one containing `:` is rejected);
- must stay inside the project (`..` is rejected);
- is normalized: `./src/` becomes `src`, `lib//x` becomes `lib/x`, and `.` (or
  an empty path after normalization) means the whole project.

Rules for the list:

- at least one root is required;
- roots must not overlap (`src` and `src/core` together are rejected).

Within the roots, agentctl sees exactly what Git lists as repository content:
tracked files, and untracked files that are not ignored by Git's standard
exclude rules. Git-ignored files (for example a `target/` build tree) are never
source. `.agentctl/`, `agentctl.toml` and anything inside a `.git` entry are
never source. Only regular files carry source content.

Which files get a graph is decided by extension, not configuration: Rust
(`.rs`), Python (`.py`, `.pyi`), JavaScript (`.js`, `.mjs`, `.cjs`, `.jsx`),
TypeScript (`.ts`, `.mts`, `.cts`) and TSX (`.tsx`). Other files in the roots
are accepted source without a graph.

## `[agents]` (required)

| Key | Type | Required | Default | Meaning |
| --- | --- | --- | --- | --- |
| `max_concurrency` | integer ≥ 1 | yes | — (`init` offers 4) | The most pipelines (claims) held at once across every plan of the project. |
| `invocation_timeout_minutes` | integer ≥ 1 | no | 120 | How long one invocation of any role may run before agentctl ends it. |
| `planner` | table | yes | — | The planner role. |
| `executor` | table | yes | — | The executor role. |
| `verifier` | table | yes | — | The verifier role, used for task verification and final integration verification. |

`max_concurrency = 0` is rejected. The ceiling bounds scheduled pipelines
(each runs one executor, then one verifier). A planner or integration verifier
launched by `plan create`, `plan update` or `plan verify` does not take a
claim.

A timed-out invocation is ended exactly as a cancelled one: its lifecycle
domain is terminated through procd and the end recorded.

### Role tables: `[agents.planner]`, `[agents.executor]`, `[agents.verifier]`

All three are required; no other role name is accepted.

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `provider` | text | yes | A provider name: `claude`, `codex`, or a name declared under `[providers.<name>]`. |
| `model` | text | yes | A model identifier, passed to the provider verbatim. agentctl never interprets it. |
| `reasoning_effort` | string | yes | One of `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. |

A role naming a provider that is neither built in nor declared is rejected:

```text
the executor role uses provider `local`, which is neither claude, codex nor declared under [providers.local]
```

`reasoning_effort` is agentctl's canonical scale. Each adapter translates it or
refuses a level its provider cannot honor, at launch time: the `claude` adapter
refuses `minimal`; the `codex` adapter passes the level through as Codex's
`model_reasoning_effort`; a `generic` runtime receives it and must answer with
an error for a level it cannot honor. See [providers.md](providers.md).

## `[providers.<name>]` (optional)

Declares a provider: an opaque name that roles refer to, and how to run it.

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `adapter` | string | yes | The wire protocol: `claude`, `codex` or `generic`. |
| `command` | text | yes | The executable: an absolute path, or a name found on `PATH`. |
| `args` | array of strings | no (default empty) | Fixed arguments placed before the adapter's own. |
| `env` | array of text | no (default empty) | Exact names of extra environment variables the provider may see. A name containing `=` is rejected. |

`model` is **not** a provider key; it belongs to roles.

### Built-in providers

`claude` and `codex` exist without being declared. Undeclared, each is:

```toml
[providers.claude]
adapter = "claude"
command = "claude"

[providers.codex]
adapter = "codex"
command = "codex"
```

Declaring `[providers.claude]` or `[providers.codex]` replaces the implicit
definition, for example to run a wrapper or pass fixed arguments.

### Custom provider identities

The provider name is an identity only: it is what invocations record and what
`status`, `logs` and `agenttop` aggregate by. The adapter alone decides the
protocol. Several names may use the same adapter:

```toml
[agents.executor]
provider = "claude-work"
model = "claude-opus-5-5"
reasoning_effort = "medium"

[providers.claude-work]
adapter = "claude"
command = "/usr/local/bin/claude-work-wrapper"
env = ["CLAUDE_CONFIG_DIR"]
```

### Command resolution

At launch, `command` is used as a path if it names one, and otherwise looked up
on `PATH`. The executable is started directly, never through a shell. On
Windows a `.bat` or `.cmd` command is refused, since it would run through a
command interpreter. A missing executable fails the invocation
(`executable_missing`) before any provider process exists.

### Environment

Providers do not inherit agentctl's environment. Each provider process sees
only:

1. a fixed common set: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`,
   `TEMP`, `TMP`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `XDG_CONFIG_HOME`,
   `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`,
   `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`,
   `SSL_CERT_DIR`, `NODE_EXTRA_CA_CERTS`, and the Windows system variables
   (`SYSTEMROOT`, `SYSTEMDRIVE`, `WINDIR`, `COMSPEC`, `PATHEXT`,
   `USERPROFILE`, `USERNAME`, `USERDOMAIN`, `HOMEDRIVE`, `HOMEPATH`, `APPDATA`,
   `LOCALAPPDATA`, `PROGRAMDATA`, `PROGRAMFILES`, `PROGRAMFILES(X86)`,
   `COMMONPROGRAMFILES`, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE`,
   `OS`);
2. the adapter's own set:
   - `claude`: `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, and every
     variable starting with `ANTHROPIC_` or `CLAUDE_CODE_USE_`;
   - `codex`: `CODEX_HOME`, and every variable starting with `OPENAI_`;
   - `generic`: nothing;
3. the exact names listed in the provider's `env`.

Names match ignoring ASCII case. A listed variable that is not set is simply
absent.

## Defaults and built-in behavior (not configurable)

| Behavior | Value |
| --- | --- |
| Invocation timeout | 2 hours unless `invocation_timeout_minutes` is set |
| Cancellation request polling by a running invocation | every 250 ms |
| `plan cancel` wait for covered invocations | 60 seconds |
| Lifecycle requirement for every role | procd process-tree termination, enforced or best effort; never unsupported |
| State location | `.agentctl/` beside `agentctl.toml` |
| Workspaces | Created in the system temporary directory, outside the project |

## Validating a configuration

Any command that loads the project validates the file and reports the first
error, for example:

```bash
agentctl status
```

`agentctl status` changes nothing and creates no state, so it is a safe check.
