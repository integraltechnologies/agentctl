# Command-line reference

agentctl `0.3.0-alpha` installs two user-facing commands, `agentctl` and
`agenttop`, plus `agentctl-shim`, which agentctl starts itself and which must
sit in the same directory as `agentctl`.

```text
agentctl init
agentctl plan create <OBJECTIVE> [--constraint <TEXT>]... [--criterion <TEXT>]...
agentctl plan update <PLAN>
agentctl plan verify <PLAN>
agentctl plan pause <PLAN>
agentctl plan resume <PLAN>
agentctl plan cancel <PLAN>
agentctl plan attention <PLAN>
agentctl plan decide <PLAN> <CONCERN> <accept|instruct|stop> [--instruction <TEXT>]
agentctl run <PLAN>
agentctl recover
agentctl status
agentctl logs [--plan <PLAN>] [--task <TASK>] [--agent <AGENT>] [--kind <KIND>]
              [--after <SEQ>] [-n, --limit <N>] [-f, --follow]
agenttop [--interval <MS>]
```

`<PLAN>`, `<TASK>`, `<AGENT>` and `<CONCERN>` are the numeric ids agentctl
prints (`plan 1`, `task 3`, `concern 1`). Every command except `init` works on
the project that owns the current directory: the nearest ancestor containing
`agentctl.toml`. It fails with `no agentctl project here` otherwise.

Terminology (plan, task, generation, invocation, candidate, accepted source,
ownership) is defined in [architecture.md](architecture.md).

## Contents

- [End-to-end example](#end-to-end-example)
- [init](#agentctl-init)
- [plan create](#agentctl-plan-create)
- [run](#agentctl-run)
- [plan update](#agentctl-plan-update)
- [plan verify](#agentctl-plan-verify)
- [plan pause](#agentctl-plan-pause) / [plan resume](#agentctl-plan-resume)
- [plan cancel](#agentctl-plan-cancel)
- [plan attention](#agentctl-plan-attention) / [plan decide](#agentctl-plan-decide)
- [recover](#agentctl-recover)
- [status](#agentctl-status)
- [logs](#agentctl-logs)
- [agenttop](#agenttop)
- [Interrupts and exit status](#interrupts-and-exit-status)

## End-to-end example

```bash
cd my-repo                      # a Git repository
agentctl init                   # answer the prompts; build the initial index

agentctl plan create "Add input validation to the config loader" \
  --constraint "Keep the public API unchanged" \
  --criterion "Invalid files are rejected with a clear error" \
  --criterion "The test suite passes"
# plan 1 created
# task 1 (validate-loader): ["src/config.rs"]
# task 2 (tests): ["tests/config.rs"] after 1
# plan 1: ready; `agentctl run 1` runs it

agentctl run 1                  # executor → verifier → acceptance per task
# ...
# plan 1: running (AllCompleted)

agentctl plan update 1          # the planner reviews the result and, if the
                                # intent is met, proposes completion; the
                                # integration verifier then runs at once
# plan 1: its planner proposed completion
# integration verification 1 of replan 1: passed
# plan 1: completed

agentctl status
```

The output shown is illustrative; task keys, scopes and ordering are the
planner's. If `run` leaves tasks stopped (a verifier failed a candidate, an
executor failed), `agentctl plan update 1` gives the planner that feedback; it
may revise and authorize retries, after which `agentctl run 1` runs them.

## `agentctl init`

Creates a project, or prepares local state for an existing one.

```bash
agentctl init
```

**New project** (no `agentctl.toml` in the current directory or any parent):
the project root is the nearest ancestor containing `.git`, else the current
directory. `init` asks, each with a default accepted by pressing Enter:

| Prompt | Default |
| --- | --- |
| Project name | the root directory's name |
| Project version | `0.1.0` |
| CodeGraph source roots (comma-separated) | `src` |
| planner provider (`claude`\|`codex`) | `claude` |
| planner model | `claude-opus-5-5` |
| planner reasoning effort | `high` |
| executor provider / model | the planner's |
| executor reasoning effort | `medium` |
| verifier provider / model | the planner's |
| verifier reasoning effort | `high` |
| Maximum concurrent agents | `4` |

An invalid answer is asked again. A source root that does not exist yet only
warns. If a chosen provider's command is not on `PATH`, `init` warns and asks
whether to continue (default no; declining writes nothing). It then writes
`agentctl.toml` (never overwriting one), creates `.agentctl/`, and appends
`/.agentctl/` to `.gitignore`.

Interactive setup offers only the built-in `claude` and `codex` providers. A
custom provider is declared by editing `agentctl.toml`
([configuration.md](configuration.md), [providers.md](providers.md)).

**Existing project**: the configuration is loaded and validated (an invalid
file fails and nothing is created), `.agentctl/` is created if missing, and
unavailable providers are reported as warnings.

Finally, in both cases:

```text
Build the initial repository index now? [Y/n]
```

Yes captures the **baseline accepted source** (every file Git lists as
repository content within the source roots that has no accepted state yet) and
indexes it into CodeGraph, printing `Indexed N source files.` It is idempotent:
paths that already have accepted state are left alone. Plans can only schedule
work on accepted source, so answer yes before planning.

`init` requires `git` on `PATH` to build the index.

## `agentctl plan create`

Creates a plan from human intent and runs its initial planning.

```bash
agentctl plan create <OBJECTIVE> [--constraint <TEXT>]... [--criterion <TEXT>]...
```

| Argument | Meaning |
| --- | --- |
| `<OBJECTIVE>` | What the plan is to achieve, given verbatim to the planner. |
| `--constraint <TEXT>` | A constraint or invariant the work must respect. Repeatable. |
| `--criterion <TEXT>` | A completion criterion. Repeatable. |

The intent is recorded first (`plan N created`), then a fresh planner
invocation proposes tasks. agentctl validates the whole proposal and applies it
atomically, or not at all. On success it prints each task with its key, scope
and dependencies, and either:

- `plan N: ready; agentctl run N runs it` (the planner finalized), or
- `plan N: still planning ...` (it did not finalize; run `plan update N`).

If the proposal is refused, or the planner invocation ends without a result,
the command fails and the plan stays `planning`, unchanged; `plan update N`
plans it again. Nothing is executed by `plan create`.

Fails before launching anything if the host cannot run agents (procd supports
no process-tree termination; see [security.md](security.md)).

## `agentctl run`

Runs a plan's eligible tasks as far as they can go now.

```bash
agentctl run <PLAN>
```

Requires the plan to be `ready` (it becomes `running`) or `running`. Paused,
planning, needs-attention and completed plans are refused. Also refused while
interrupted work of this plan awaits `agentctl recover`.

For each eligible task, within `agents.max_concurrency` (counted across all
plans), it claims a new generation and runs its pipeline: executor, install of
the candidate, independent verifier, and acceptance on a pass. Independent
tasks run concurrently; a task becomes eligible once every dependency is
completed. `run` returns when nothing more can be launched and every pipeline
it launched has ended. It prints:

- each finished pipeline: `task T generation G: <release outcome>`;
- each task not claimed because the working tree does not hold accepted source
  in its scope (human edits, or anything agentctl did not accept), with the
  paths. Such a task waits until you reconcile those files;
- every task's status and the plan's state and condition, for example
  `plan 1: running (AllCompleted)`.

`run` never replans, never retries a stopped task and never completes a plan.
`AllCompleted` means every task is completed or cancelled; the plan stays
`running` until final integration verification passes (see
[plan update](#agentctl-plan-update)).

Exits non-zero if scheduling stopped early (for example interrupted, or a
store error). Ctrl-C ends the agents it runs, records how they ended, and
launches nothing more.

## `agentctl plan update`

Plans or replans a plan.

```bash
agentctl plan update <PLAN>
```

- On a `planning` plan: runs initial planning again, as `plan create` does.
- On a `ready`, `running` or `paused` plan, or a `needs_attention` plan whose
  human instruction awaits its planner: runs a **replan**. The planner receives
  canonical feedback on how the work went and may add, revise or cancel tasks
  that are not running or completed, authorize one fresh attempt at a stopped
  task (`retry_task`), raise a concern for a human (`raise_attention`), or,
  when every task is completed or cancelled, propose completion.

On success it prints each task's standing and `plan N: replan R applied;
<state>`. If the plan now needs attention, its concerns are shown. If the
planner proposed completion, **final integration verification runs
immediately**, as [`plan verify`](#agentctl-plan-verify) does, and its result
is printed.

Fails, applying nothing, when:

- the plan changed while the planner worked (`update it again`);
- the proposal was refused (the reason is printed);
- the planner invocation ended without a result;
- the plan has unresolved interrupted work (run `agentctl recover`), or a
  path the replan would restore holds something other than the abandoned
  candidate or accepted source (reconcile it first).

## `agentctl plan verify`

Runs final integration verification of a plan whose planner proposed
completion.

```bash
agentctl plan verify <PLAN>
```

Requires the plan to be settled (every task completed or cancelled, nothing
live or unresolved, no blocking concern, accepted source synchronized with
CodeGraph) with its latest replan proposing completion. A fresh verifier judges
the accepted result as a whole. Prints
`integration verification V of replan R: <outcome>`, any blockers, and the
plan's state.

| Outcome | Next |
| --- | --- |
| `passed` | The plan is `completed`. |
| `failed` | Blockers go to the planner: `agentctl plan update N`. |
| anything else (`basis_changed`, `boundary_violated`, `invocation_failed`, `malformed_result`) | Nothing was judged: `agentctl plan verify N` verifies again. |

## `agentctl plan pause`

```bash
agentctl plan pause <PLAN>
```

Requires a `running` plan. Nothing more of it is claimed. Pipelines already
running continue to their end (their executor, verifier and acceptance may
still complete); `plan cancel` ends them instead.

## `agentctl plan resume`

```bash
agentctl plan resume <PLAN>
```

Requires a `paused` plan; it becomes `running`. Nothing is launched: run
`agentctl run N`.

## `agentctl plan cancel`

Ends a plan's live agent work.

```bash
agentctl plan cancel <PLAN>
```

Records a durable cancellation request, pausing the plan if it is running.
Each agentctl process running one of its invocations (in any terminal) sees the
request within about a quarter second, terminates the invocation's lifecycle
domain through procd and records its end; the cancelled pipelines start no
further invocation. `plan cancel` waits up to 60 seconds and prints each
covered invocation:

- its terminal state (for example `cancelled`);
- `still running: its agentctl process has not ended it yet`; or
- `left by an agentctl process that no longer runs ...`, which only
  `agentctl recover` can settle, where procd can establish its fate.

It exits non-zero if any covered invocation is not proven ended. What the
stopped attempts own stays owned and what was accepted stays accepted: the
planner decides what follows (`plan update`). The plan stays paused; resume it
with `plan resume`.

## `agentctl plan attention`

```bash
agentctl plan attention <PLAN>
```

Shows every concern the plan's planner raised (number, key, the replan that
raised it, the decision so far, reason, evidence, affected tasks) and the
plan's state, with what continues it.

## `agentctl plan decide`

Records a human decision on a concern, once.

```bash
agentctl plan decide <PLAN> <CONCERN> accept
agentctl plan decide <PLAN> <CONCERN> instruct --instruction "<TEXT>"
agentctl plan decide <PLAN> <CONCERN> stop
```

| Decision | Effect |
| --- | --- |
| `accept` | Continue unchanged despite the concern. Once nothing else blocks the plan, it is `running` again. |
| `instruct` | `--instruction` is required. The instruction reaches the next planner as authoritative input; run `agentctl plan update N`. |
| `stop` | The plan never continues autonomously. |

Requires the plan to be `needs_attention` and no planner of it to be acting. A
concern is decided once; repeating the same decision reports
`already decided so; nothing changed`, and a different decision is refused.
`--instruction` is accepted only with `instruct`.

## `agentctl recover`

Settles work left unresolved by agentctl processes that are no longer running.

```bash
agentctl recover
```

For each ended session it settles, in order: provider invocations with no
recorded end (only on procd's proof that their processes are gone), attempted
actions, never-attempted actions, published-but-unfinished acceptances, and
claims. Work of an agentctl process that is still running is left alone. It
prints each record with `recovered`, `in use by a running agentctl process`,
`blocked` or `unsupported`, then a summary, or `nothing to recover`.

Nothing is retried, judged or accepted anew. Exits non-zero if anything could
not be settled (`recovery is blocked: ...`); what it could not establish stays
exactly as found, and new work on that plan stays blocked. Safe to run at any
time and repeatedly. Whether a killed process's invocations can be settled
depends on the host; see [security.md](security.md#recovery).

## `agentctl status`

```bash
agentctl status
```

Shows, from recorded state alone: claims held against the ceiling; each plan's
state, condition, objective, task counts (`completed`, `scheduled`, `eligible`,
`waiting`, `stopped`, `cancelled`, `unscheduled`), what awaits a human,
unresolved records with their sessions, and token usage; then project-wide
usage. It changes nothing, creates nothing, and never claims unfinished work is
running or done. In a project with no local state yet it says so.

## `agentctl logs`

```bash
agentctl logs [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--plan <PLAN>` | Only this plan's events. |
| `--task <TASK>` | Only this task's events. |
| `--agent <AGENT>` | Only this agent's events. |
| `--kind <KIND>` | Only this event kind, or a group when it ends in `.` (for example `invocation.`). |
| `--after <SEQ>` | The first events after this sequence number, instead of the newest. |
| `-n, --limit <N>` | How many events (default 50). |
| `-f, --follow` | Keep printing new events until interrupted. |

Each line is `seq time kind [plan P] [task T] [agent A] detail`, oldest first.
Events are chronology only; use `status` for where things stand. Event kinds
are dotted names such as `plan.created`, `scheduler.claimed`,
`invocation.ended` (whose detail includes the recorded `termination`),
`verification.finished`, `acceptance.completed`, `attention.raised` and
`integration.finished`.

## `agenttop`

```bash
agenttop [--interval <MS>]
```

A live, read-only terminal monitor of the project's agent work and token
usage. It needs an interactive terminal. `--interval` sets milliseconds
between reads of state (default 1000, minimum 100).

| Key | Action |
| --- | --- |
| `1`–`6` | View: aggregate, provider, plan, task, role, agent |
| `Tab` / `Shift-Tab` | Next / previous view |
| `q`, `Esc`, `Ctrl-C` | Quit |

Invocations with no recorded end are shown as such, with usage pending; local
estimates are marked `~` and drawn separately from provider-reported counts.

## Interrupts and exit status

`run`, `plan create`, `plan update` and `plan verify` launch providers. On
Ctrl-C, SIGTERM or SIGHUP (Ctrl-C, Ctrl-Break or console close on Windows)
they launch nothing more, end every live invocation through procd, record how
each ended, and exit non-zero with `interrupted: ...`.

Every command exits `0` on success and non-zero with a message on stderr on
failure.
