# Providers

agentctl does not talk to model APIs. It drives installed coding-agent CLIs as
sandboxed worker processes through thin adapters. Every conversation is fresh and
disposable. Plans, results, verification, evidence, and memory are stored by
agentctl, not in provider sessions.

## Supported adapters

| `adapter` | CLI | Token usage |
| --- | --- | --- |
| `claude` | Claude Code (`claude`) | reported as `EXACT` when the CLI's JSON result includes usage counts; missing counts stay unknown |
| `codex` | Codex CLI (`codex`) | `UNKNOWN`: the non-interactive final-output interface supplies no trustworthy counts |

These are the only adapters. Any other `adapter` value is a configuration error.
Model and effort strings are passed through to the CLI unchanged. agentctl has no
model catalog.

The adapters rely on the flags listed below. If an installed CLI version lacks one
of them, the launch fails. agentctl never retries with weaker flags.

## Configuration

```toml
[runtime.providers.claude]
adapter = "claude"
executable = "/absolute/path/to/claude"   # e.g. the output of `command -v claude`

[runtime.providers.codex]
adapter = "codex"
executable = "/absolute/path/to/codex"

[runtime.roles.planner]
provider = "claude"

[runtime.roles.executor]
provider = "claude"
model = "your-model-name"   # optional, opaque

[runtime.roles.verifier]
provider = "codex"
```

A single provider can serve every role. The verifier's independence comes from
receiving a fresh conversation with only the task, invariants, diff, and captured
evidence, and never the executor's reasoning or transcript. It does not depend on
using a different vendor. See [configuration.md](configuration.md) for profiles,
fallbacks, and project restrictions.

## Authentication

agentctl never stores, copies, or parses provider credentials. Log in with each
CLI the way you normally would, before running agentctl:

```bash
claude auth login
codex login
```

| `mode` | Behavior |
| --- | --- |
| `AUTO` (default) | Use the provider's native login. If none is found and `api_key_env` is set, use that variable. |
| `NATIVE` | Native login only; no fallback. |
| `API_KEY` | Always use the variable named by `api_key_env`. |

```toml
[runtime.providers.codex.authentication]
mode = "AUTO"
api_key_env = "MY_CODEX_API_KEY"   # a variable NAME, never the value
```

With an API key, the named variable's value is given to that provider's worker
only, as `ANTHROPIC_API_KEY` (Claude) or `CODEX_API_KEY` (Codex). It is also added
to the list of values scrubbed from captured output. Ambient API-key variables are
never picked up implicitly.

### Native login inside the sandbox

Provider workers keep your real `HOME` and `USER`/`LOGNAME` so the CLI finds its
own login:

- **Codex:** `CODEX_HOME` (default `~/.codex`). Only `auth.json` is readable and
  writable in place (so the CLI can refresh it). The rest of `CODEX_HOME` is
  denied. Codex's SQLite state is redirected to per-job scratch.
- **Claude Code:** `CLAUDE_CONFIG_DIR` keeps its normal meaning, including when
  unset. `.credentials.json` and `.claude.json` are readable and writable in place,
  and `settings.json` is read-only. The rest of the provider home is denied. On
  macOS, the login Keychain is readable (read-only) because Claude Code uses it
  for native login.

Existing history, rules, plugins, and other jobs' scratch are never exposed.
Checks and experiments never receive provider authentication.

Unsupported mechanisms (enterprise credential helpers, custom endpoint routing)
fail with guidance. They are never silently worked around.

## How the adapters run

Prompts are delivered on stdin as structured argv. agentctl never builds a shell
command.

**Claude Code:**

```text
claude --print --safe-mode --restricted --no-session-persistence
       --session-id <fresh-uuid> --output-format json
       --setting-sources "" --strict-mcp-config --mcp-config '{"mcpServers":{}}'
       --disable-slash-commands --permission-mode dontAsk
       --tools <role tools> --allowedTools <role tools>
       [--model M] [--effort E]
```

The `<role tools>` are:

- executor: `Read,Edit,Write`;
- planner: `Read,Bash` (to run the read-only `agentctl run packet-hashes` helper,
  which only reads a PlanPacket from stdin and computes canonical hashes);
- verifiers: `Read`.

The result, or its structured output, must parse as the expected canonical
document.

**Codex:**

```text
codex exec --ephemeral --ignore-user-config --ignore-rules --color never
     --dangerously-bypass-approvals-and-sandbox
     -c project_doc_max_bytes=0 -c shell_environment_policy.inherit="none"
     -c cli_auth_credentials_store="auto" -c history.persistence="none"
     -c features.multi_agent=false -c features.memories=false
     -c sqlite_home=<scratch> [-c cli_auth_credentials_store="ephemeral"]
     [--model M] [-c model_reasoning_effort=E] -
```

Codex's own sandbox is bypassed because agentctl always runs it inside agentctl's
mandatory OS sandbox, and nested Seatbelt fails on macOS. There is no launch path
without the outer sandbox. `stdout` must be the canonical JSON document.

For both adapters:

- provider-internal sub-agent spawning is disabled;
- output is parsed strictly (duplicate JSON keys are rejected);
- no session is persisted or resumed.

## Inspecting providers

```bash
agentctl provider list                # configured providers, executable presence, sandbox availability
agentctl provider doctor              # also runs `<cli> --version` and the CLI's login-status command
agentctl route executor               # resolved route, fallbacks, and permissions for one role
agentctl route executor --override executor:codex   # preview an explicit override
```

`provider doctor` runs `codex login status` or `claude --safe-mode auth status`
with a bounded timeout, and reports only the authentication method and
availability. It never sends a model prompt. A successful status check does not
guarantee that credentials remain valid for the next call. Route inspection
launches nothing and reports authentication as `NOT_PROBED`.

## Limitations

- Only Claude Code and Codex CLI adapters exist.
- Codex token usage is unknown. Claude usage arrives when the job finishes, not
  as a stream.
- Provider frontends require network access. They share their sandbox with their
  own tool subprocesses, which can therefore read that frontend's login files.
- Provider-internal activity (tool calls, idle time) is not observed. agentctl
  records lifecycle, captured diffs, evidence, and final output.
