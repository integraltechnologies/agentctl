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

- **Codex:** the job gets its own `CODEX_HOME` in scratch, with the operator's
  `auth.json` linked in — the only file agentctl exposes, readable and writable
  in place so the CLI can refresh it. Codex bootstraps its config, caches,
  plugins and skills inside the job's scratch home and they are discarded with
  it, so the operator's real Codex home (history, attachments, installed
  plugins) is never visible to a worker. agentctl never opens the credential
  file itself.
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
       --append-system-prompt <role contract> --json-schema <output schema>
       [--model M] [--effort E]
```

The `<role tools>` are:

- executor: `Read,Edit,Write`;
- planner and verifiers: `Read`. A planner computes nothing: agentctl derives
  every hash from the decision it returns, so the planner needs no shell.

The reply arrives through Claude Code's native structured output
(`structured_output`, validated by the CLI against the schema). Without it, the
`result` text must itself be exactly one canonical JSON document; fenced or
prose-wrapped replies are refused, never extracted.

**Codex:**

```text
codex exec --ephemeral --ignore-user-config --ignore-rules --color never
     --dangerously-bypass-approvals-and-sandbox
     -c project_doc_max_bytes=0 -c shell_environment_policy.inherit="none"
     -c cli_auth_credentials_store="auto" -c history.persistence="none"
     -c features.multi_agent=false -c features.memories=false
     -c sqlite_home=<scratch> [-c cli_auth_credentials_store="ephemeral"]
     -c developer_instructions=<role contract>
     [--model M] [-c model_reasoning_effort=E] -
```

Codex's own sandbox is bypassed because agentctl always runs it inside agentctl's
mandatory OS sandbox, and nested Seatbelt fails on macOS. There is no launch path
without the outer sandbox. `stdout` must be the canonical JSON document.

Codex's `--output-schema` is not used: it requests strict structured output,
which needs every property to be required, while canonical schemas have
optional fields. The schema travels in the job input instead.

### Role contracts

What each role must do is one canonical contract per role (planner, executor,
task verifier, integration verifier) with stable rule IDs, rendered identically
for every provider. Every rule agentctl validates on provider output is in it,
and refusals cite the rule ID. Adapters only choose the channel: Claude receives
it as an appended system prompt, Codex as developer instructions; the job input
itself arrives as the user turn. Validation stays authoritative whatever a
provider was told. Each job records the contract it was bound to in its prompt
provenance (`agentctl-role-contract-1/<ROLE>/blake3:…`).

### Failures and retries

A provider exchange that yields no accepted output is classified:

| Class | Examples | agentctl does |
| --- | --- | --- |
| `RETRYABLE_PROVIDER_FAILURE` | timeout, transient nonzero exit, a reply that is not the canonical document | up to 2 fresh jobs on the same route; an executor only while the workspace is provably unchanged |
| `NONRETRYABLE_PROVIDER_FAILURE` | authentication refused, usage or rate limit (as the CLI reports it) | stops at once with the provider's reason |
| `SEMANTIC_REJECTION` | a valid executor result with status `BLOCKED`/`FAILED` | reports its code and summary as the task's blocking reason |
| `VALIDATION_FAILURE` | a parseable document that breaks a contract rule | verifier/executor: retried like a mechanical failure; planner: one correction attempt told the exact refusal |

When a provider failure leaves nothing uncertain (the workspace equals what the
run last recorded), the run stays resumable: `agentctl run resume` continues
once the provider is available again. Retries never consume the correction
rounds reserved for replacement plans.

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
