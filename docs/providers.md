# Providers

agentctl `0.3.0-alpha` is provider-neutral. Providers supply engineering
intelligence for one invocation at a time; agentctl owns everything else about
the invocation, and every piece of canonical engineering state. No provider
session, transcript or memory carries anything from one invocation to the next.

## The model

| Concept | What it is | Where it is set |
| --- | --- | --- |
| **Provider name** | An opaque configured identity. It is what each invocation records and what observation aggregates by. It implies nothing about the provider. | `[agents.<role>].provider`, `[providers.<name>]` |
| **Adapter** | The wire protocol implementation: how to build the command line, deliver input, and decode output. One of `claude`, `codex`, `generic`. | `[providers.<name>].adapter` |
| **Command** | The executable that is run, with optional fixed arguments. | `[providers.<name>].command`, `args` |
| **Model** | An opaque, provider-owned identifier, passed through verbatim. | `[agents.<role>].model` |

`claude` and `codex` are implicit provider names using the adapter and command
of the same name. Any other name must be declared. Configuration details are in
[configuration.md](configuration.md#providersname-optional).

## What agentctl owns for every invocation

Whatever the adapter, agentctl owns an invocation's identity, lifecycle,
process, input, structured result, usage accounting, liveness, cancellation,
timeout and failure classification:

1. The launch is validated (absolute working directory, a valid JSON Schema for
   the result) before anything is recorded.
2. A procd lifecycle domain is created and its identity recorded with the
   invocation before any process exists ([security.md](security.md)).
3. The provider is started directly (never through a shell) by
   `agentctl-shim` inside the domain, with the working directory and scrubbed
   environment agentctl constructs.
4. The whole input is written to the provider's standard input, which is then
   closed. No adapter accepts input mid-run.
5. Standard output is read as JSON Lines. Each non-empty line must be a JSON
   object with a string `type`; anything else makes the output malformed.
6. The invocation ends when the provider exits, or when agentctl ends it
   (cancellation, a human's cancel request, timeout, or agentctl being
   interrupted). Either way agentctl terminates the lifecycle domain and records
   the end only once procd established it.
7. The result is accepted only if the provider exited with status 0, reported
   no error, produced well-formed output, and produced one result that
   satisfies the launch's JSON Schema, validated by agentctl itself. Prose is
   never a result.

Recorded end states and failure kinds:

| State | Meaning |
| --- | --- |
| `succeeded` | Exit status 0 with one schema-valid result. |
| `failed` | With a failure kind: `executable_missing`, `spawn_failed`, `input_failed`, `provider_error`, `malformed_output`, `no_result` (exited 0 without a result), `exit_status` (exited non-zero). |
| `cancelled` | agentctl ended it: cancellation, a human's request, timeout, or agentctl being interrupted. The diagnostic says which. |
| `interrupted` | agentctl lost authoritative knowledge of how it ended (for example the shim ended without reporting), or recovery settled it after its agentctl process died. |

Every recorded end also carries its lifecycle **termination** strength,
`enforced` or `best_effort` (see [security.md](security.md)).

What agentctl records is its own classification and a short diagnostic in its
own words. Provider-controlled text (the provider's output, error messages and
standard error) may contain secrets and is never stored; standard error is
kept in memory for diagnosis only. The provider's session id is recorded as
metadata; it is never resumed and never used as identity.

### Usage and provenance

Token usage is recorded with its provenance and never fabricated:

| Provenance | Meaning |
| --- | --- |
| provider-reported | Counts the provider reported. |
| local estimate | Counts a generic runtime marked `estimated`. |
| unavailable | The invocation ended without usage. |
| pending (observation only) | No end recorded yet. |

Counts are `input` (every input token processed, cached or not) and `output`,
with optional `cached_input`, `cache_write` and `reasoning` (a subset of
output). Observation never merges reported and estimated counts and never
shows unavailable or pending usage as zero.

### Role instructions and output schemas

agentctl supplies each role's instructions (appended to or set as the
provider's system/developer instructions) and the JSON Schema of its
structured result. Inputs are JSON built from canonical state.

| Role | Workspace mode | Result |
| --- | --- | --- |
| Planner | `read_only`, at the project root | `{commands: [...], explanation}` |
| Executor | `editable`, in a disposable copy | `{status: succeeded\|failed, summary, modified_paths}` |
| Task verifier | `disposable`, in a disposable view | `{verdict: pass\|fail, checked, blockers, non_blocking}` |
| Integration verifier | `disposable`, in a disposable copy | same as task verifier |

Every result is untrusted: a planner's commands are validated and applied
atomically or not at all; an executor's report is kept as a claim while
agentctl observes the workspace itself; a verifier's verdict counts only
alongside agentctl's own observations. See [architecture.md](architecture.md).

## Built-in Claude support (`adapter = "claude"`)

Runs Claude Code non-interactively:

```text
<command> [args...] --print --output-format=stream-json --verbose
  --no-session-persistence --permission-mode=<mode> --permission-prompts=none
  [--allowedTools=Bash,PowerShell] --model=<model> [--effort=<effort>]
  [--append-system-prompt=<instructions>] --json-schema=<schema>
```

- Input arrives on standard input.
- Workspace modes map to Claude's permission modes: `read_only` → `default`;
  `editable` and `disposable` → `acceptEdits` (file edits in the working
  directory are accepted without asking). `disposable` also allows the command
  tools `Bash` and `PowerShell`. Nobody answers permission prompts, so any
  other tool use that would ask is denied.
- agentctl's instructions are appended to Claude Code's own system prompt.
- The CLI is asked to enforce the output schema; agentctl takes the result
  event's `structured_output` and validates it again. Its prose `result` is
  never the result. More than one result event is malformed.
- Nothing is persisted for resumption.
- Usage comes from the result event (provider-reported). Claude's cost figure
  is kept as in-memory metadata only.
- `reasoning_effort = "minimal"` is refused.
- Environment passed through (beyond the common set): `CLAUDE_CONFIG_DIR`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_*`, `CLAUDE_CODE_USE_*`.

## Built-in Codex support (`adapter = "codex"`)

Runs Codex non-interactively:

```text
<command> [args...] exec --json --ephemeral --skip-git-repo-check
  --sandbox=<read-only|workspace-write> [workspace-write tmp exclusions]
  --cd=<working directory> --model=<model>
  [--config=model_reasoning_effort=<effort>]
  [--config=developer_instructions=<instructions>]
  --output-schema=<temporary schema file> -
```

- Input arrives on standard input (`-`).
- Workspace modes map to Codex's own sandbox: `read_only` → `read-only`;
  `editable` → `workspace-write` whose only writable root is the working
  directory (the temporary directories Codex would otherwise add are
  excluded); `disposable` → `workspace-write` keeping those temporary
  directories, for build and test artifacts. That confinement is Codex's, not
  agentctl's.
- agentctl's instructions become Codex's developer instructions.
- The output schema is handed over in a temporary file, removed when the
  invocation ends. The turn's final agent message must be JSON, which agentctl
  validates against the schema itself; earlier messages are progress.
- A failed turn is fatal and final. An `error` event counts only if the turn
  never completes (Codex also reports errors it recovers from).
- Sessions are ephemeral. Usage comes from turn completion (provider-reported).
- The reasoning effort is passed as `model_reasoning_effort`; Codex decides
  which levels it accepts.
- Environment passed through (beyond the common set): `CODEX_HOME`, `OPENAI_*`.

## Generic provider adapter (`adapter = "generic"`)

The generic adapter speaks agentctl's **external-agent protocol, version 1**.
Any executable that implements it can be an engineering-agent runtime under
any provider name:

```toml
[agents.executor]
provider = "house-runtime"
model = "house-model-large"
reasoning_effort = "medium"

[providers.house-runtime]
adapter = "generic"
command = "/opt/house/bin/house-runtime"
args = ["serve-once"]
env = ["HOUSE_RUNTIME_TOKEN"]
```

The command is run directly with exactly the configured `args` (the adapter
adds none), in the role's working directory, with only the common environment
plus the names in `env`.

### Request

One JSON object on standard input, followed by end of input:

```json
{
  "protocol": 1,
  "model": "house-model-large",
  "reasoning_effort": "medium",
  "instructions": "You are an executor of agentctl, ...",
  "input": "{ ...JSON text of the role's input... }",
  "output_schema": { "type": "object", "...": "..." },
  "workspace": "editable"
}
```

| Field | Meaning |
| --- | --- |
| `protocol` | Always `1`. |
| `model` | The role's configured model, verbatim. |
| `reasoning_effort` | The role's effort, or `null`. |
| `instructions` | agentctl's role instructions. |
| `input` | The role's input, a string (itself JSON text). |
| `output_schema` | The JSON Schema the result value must satisfy. |
| `workspace` | `read_only`, `editable` or `disposable`. |

The runtime must honor the workspace mode within its working directory, or
answer with an `error`. It must likewise answer with an `error` for any
protocol version, model or reasoning effort it cannot honor. agentctl adds no
sandbox of its own and knows nothing of what the runtime is.

### Events

The runtime answers on standard output in JSON Lines: one JSON object per
line, each with a string `type`.

| Event | Meaning |
| --- | --- |
| `{"type": "session", "id": "..."}` | Metadata only. |
| `{"type": "usage", "input": N, "output": N, "cached_input": N, "cache_write": N, "reasoning": N, "estimated": false}` | Cumulative usage; the last one stands. `cached_input`, `cache_write` and `reasoning` are optional. `estimated` (default `false`) marks the counts as the runtime's own estimate rather than a provider report. |
| `{"type": "result", "value": ...}` | The one successful terminal event. `value` must satisfy `output_schema`. |
| `{"type": "error", "message": "..."}` | The terminal failure. The message is kept in memory for diagnosis, never recorded. |

Other event types are ignored. A line that is not a JSON object with a string
`type`, a malformed `usage` event, a `result` without `value`, or a second
terminal event makes the invocation fail as `malformed_output`.

The runtime must then exit. Success requires exit status 0 after a `result`.
An `error`, a non-zero exit, or exit 0 without a result fails the invocation.
Output a process leaves open after the runtime exits is waited on only briefly
and noted in the diagnostic.

### Minimal example

A runtime answering an executor request that it cannot do the work:

```text
<- stdin:  {"protocol":1,"model":"m","reasoning_effort":"medium", ... ,"workspace":"editable"}
-> stdout: {"type":"session","id":"s-1"}
-> stdout: {"type":"usage","input":1200,"output":80}
-> stdout: {"type":"result","value":{"status":"failed","summary":"cannot build the parser without network access","modified_paths":[]}}
   exit 0
```

## Lifecycle and cancellation

Providers do not manage their own lifecycle within agentctl. Every provider
process, and every process it starts, runs inside a procd lifecycle domain
held by the agentctl process that launched it. Cancellation, a human's
`plan cancel`, the invocation timeout, and Ctrl-C/SIGTERM/SIGHUP to agentctl
all end the invocation the same way: agentctl terminates the whole domain
through procd and records the end with its termination strength. A provider
cannot prolong its invocation by ignoring signals or detaching children, to
the extent procd's backend tracks them (enforced on Linux, best effort on
macOS). See [security.md](security.md).

## Availability checks

`agentctl init` warns when a role's provider command is not on `PATH`. The
configuration stays valid; the check is advisory. At launch, a missing command
fails the invocation as `executable_missing` before any process exists, and
the planner, execution or verification it served is recorded accordingly.
