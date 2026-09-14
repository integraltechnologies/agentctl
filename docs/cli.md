# Command guide

This guide covers every command group in `agentctl` and `agenttop`. `agentctl
--help` prints the full syntax.

## Conventions

- Put `--json` **last** to get machine-readable output. Without it, output is
  human-readable, with untrusted text escaped.
- Errors go to stderr with a nonzero exit code. Diagnostic commands (`doctor`,
  `security doctor`, `repo status`, `route`, `route check`, `role show`) print
  their report and still exit nonzero when something is wrong.
- Repository-scoped commands act on the Git checkout that contains the current
  directory.
- Placeholders such as `<request-id>` and `<plan-id>` stand for IDs printed by
  earlier commands (for example `request:5dbf…`).
- Read-only commands open the database read-only. They never migrate or modify
  it.

## Setup and diagnostics

```bash
agentctl --version
agentctl init                 # create missing machine config, directories and database
agentctl doctor               # directory permissions, machine config, database health
agentctl security doctor      # sandbox capability report plus a live self-test
agentctl state status         # schema version and record counts
agentctl events list [--repo ID] [--task ID --job ID] [--limit N]   # recent journal entries (1–1000, default 20)
```

`init` never overwrites existing configuration. `--task` and `--job` require
`--repo`, because those IDs are repository-scoped.

## Repositories

```bash
agentctl repo init            # register this checkout; create .agentctl/project.toml if absent
agentctl repo status          # identities, registration health, project config, HEAD, dirty flag
agentctl repo list            # registered repositories and their workspaces
```

A **repository ID** is derived from the canonical Git common directory, so linked
worktrees share it. A **workspace ID** is derived from the per-worktree Git
directory. Independent clones are distinct repositories even when their commits
and remotes match. Remote URLs are metadata only, and no remote is required.
Moving a primary repository normally produces new IDs. The old registration stays
inspectable and is never silently rekeyed.

## Code intelligence

```bash
agentctl repo index           # incremental: only new, changed or version-stale files are reparsed
agentctl repo index --status  # last index, backend versions, counts, stale paths, failures

agentctl code symbol <name-or-id>      # exact name, qualified name, or graph ID
agentctl code search <text>            # name substring
agentctl code file <path>              # entities in a file
agentctl code locate <text>            # deterministic ranked lookup over names, paths, containers, signatures
agentctl code refs <symbol>            # resolved references
agentctl code callers <symbol>         # resolved callers
agentctl code tests <symbol>           # lexical test candidates
agentctl code neighbors <symbol> [--depth N] [--neighbors N]
agentctl code impact <symbol>          # known structural dependents
agentctl code context <text> [--limit N] [--depth N] [--neighbors N] [--tests N] \
    [--memory-canonical N] [--memory-facts N] [--memory-notes N] [--memory-bytes N]
```

- **Languages:** Rust, Python, TypeScript/TSX, and JavaScript/JSX, parsed with
  Tree-sitter. Other files are skipped.
- **Freshness:** every query rehashes current source first. If a path is new,
  changed, deleted, or stale, the query is refused with an instruction to run
  `repo index`. Partial indexes answer from successfully indexed files and are
  marked partial.
- **Precision:** results are syntactic, not compiler truth. Most imports and
  calls stay unresolved. Only unique same-module Rust `self::` paths are resolved.
  Test links are lexical candidates, not proven coverage. `impact` reports known
  dependents, not everything a change could break.
- **Discovery:** follows workspace `.gitignore`/`.ignore` rules. It skips
  symlinks, nested repositories, common build and dependency directories
  (`target`, `node_modules`, `.venv`, `vendor`, `dist`, `build`, `__pycache__`),
  and project `deny_read` paths.
- **Limits:** result limits are 1–100. `context` defaults to 5 primaries, depth 1,
  20 neighbors, 80 relations, and 8 tests; the maximums are 10, 3, 100, 400, and 20.

## Engineering memory

Memory belongs to a repository by default, so it is shared by linked worktrees.

```bash
agentctl memory add --trust canonical --kind architecture-decision \
  --content 'Fuzzy identity matches require explicit confirmation.' \
  --key identity:confirmation --symbol resolve_candidate --invariant stable-api
agentctl memory add --trust agent-note --kind finding --job <job-id> --workspace \
  --content 'Possible bypass in resolve_candidate.'
agentctl memory derive <symbol>                  # mechanical fact from an indexed symbol
agentctl memory observe <evidence-id>            # historical fact bound to recorded evidence
agentctl memory search 'identity confirmation' [--trust canonical] [--limit N]
agentctl memory list [--symbol ID] [--task ID] [--evidence ID] [--kind KIND] [--trust CLASS] \
  [--all | --status STATUS] [--include-stale] [--all-workspaces] [--recent] [--limit N]
agentctl memory show <memory-id>
agentctl memory links <memory-id>
agentctl memory promote <memory-id> --actor <name>
agentctl memory reject <memory-id> --actor <name>
agentctl memory supersede <old-id> --with <new-id>
agentctl memory stale                            # derived facts whose supporting source changed
agentctl memory policy                           # read-only projections of project.toml
```

The trust classes are:

| Trust class | How it is created | Meaning |
| --- | --- | --- |
| `CANONICAL` | explicitly with `--trust canonical`, or by `promote` | governed decision; promotion creates a new canonical entry and leaves the original unchanged |
| `DERIVED` | only by `memory derive` | signature and syntactic relations of one indexed symbol; becomes `STALE` when its supporting file changes |
| `OBSERVED` | only by `memory observe` | what a recorded evidence item reported, bound to that evidence; historical, not a claim about today's checkout |
| `AGENT_NOTE` | `--trust agent-note` with a registered author `--job` | fallible suggestion; never gains authority by being retrieved or reused |

Entries are immutable; rejection and supersession keep history. Keyed canonical
decisions are unique while active. To replace one atomically, use
`memory add ... --key KEY --supersedes <old-id>`. Project invariants, architecture,
commands, protected paths, and verification appear as read-only `PROJECT_CONFIG`
projections. To change them, edit `project.toml`. Memory content is data, never
instructions.

## Planning

```bash
agentctl plan prepare --objective TEXT [--query TEXT] [--bytes N] [--notes N]
agentctl plan prepare --objective-file PATH
agentctl plan prepare --request-file PATH       # a strict RequestDraft JSON document
agentctl plan context <request-id>              # the frozen planner input
agentctl plan import <execution-plan.json>      # externally produced plan → VALIDATED
agentctl plan validate <plan-id>
agentctl plan activate <plan-id>                # revalidate and make ACTIVE (one per workspace)
agentctl plan show <plan-id>
agentctl plan export <plan-id>
agentctl plan tasks <plan-id>
agentctl plan ready <plan-id>                   # structural readiness only
agentctl plan blocked <plan-id>
agentctl plan list [--all] [--limit N]
agentctl plan supersede <old-id> --with <new-id>
agentctl plan cancel <plan-id> --reason TEXT
```

`plan prepare` requires a complete, fresh code index. It freezes a bounded
**PlannerPacket**: the objective, project invariants and policy, graph context,
trusted memory, and exact source excerpts. It includes no conversation history.

The default budgets are 4 graph primaries, 8 neighbors, 4 tests, 8 files, 4
canonical memories, 3 observed/derived facts, **0 agent notes**, 768 bytes and 20
lines per excerpt, and 32 KiB total. You can adjust them with `--primary`,
`--neighbors`, `--tests`, `--files`, `--canonical`, `--facts`, `--notes`,
`--excerpt-bytes`, `--excerpt-lines`, and `--bytes` (4–128 KiB). Optional
material is truncated explicitly, never silently. If the objective, invariants,
or policy do not fit the budget, `plan prepare` fails.

Plans usually come from `agentctl run planner`. An external producer can instead
write an **ExecutionPlan** (a PlanPacket plus metadata with one independent
verification contract per task and a final integration contract) and import it.
Contract hashes are BLAKE3 over compact JSON in declared field order. Compute them
with:

```bash
agentctl run packet-hashes < plan-packet.json
```

`tests/planning.rs` contains a complete construction example.

Import bounds are 32 tasks, 256 KiB per plan, 16 KiB per TaskPacket, and 8 KiB per
contract. Import and activation recheck the source baseline, project policy,
invariants, graph references, and memory references. Replacement plans get new
plan and task IDs, and `VERIFIED` status is never transferred.

## Running plans

```bash
agentctl provider list
agentctl provider doctor                  # CLI --version and token-free login status
agentctl roles
agentctl role show <role>
agentctl route <role> [--override role:provider[:model]]
agentctl route check                      # every known role, including recon and reviewer

agentctl run planner <request-id>         # planner provider produces a plan; imported as VALIDATED
agentctl run plan <plan-id> --dry-run     # workspace, strategy, configured roles, ready tasks
agentctl run plan <plan-id> [--override role:provider[:model]]
agentctl run status <plan-id>
agentctl run resume <plan-id>
agentctl run cancel <plan-id>             # request cancellation from another terminal
agentctl run replace <old-plan-id> <validated-replacement-id>
```

`run plan` is a foreground controller. For an **ACTIVE** plan, it does the
following:

1. Requires the plan's prepared baseline and the current checkout to be clean and
   committed, with a fresh index.
2. Runs ready tasks one at a time in the workspace. Each task gets a fresh
   executor, then the project checks, then a fresh verifier. Only a verifier
   `PASS` makes a task `VERIFIED` and unlocks its dependents.
3. When every task is `VERIFIED`, runs the integration checks and a fresh
   integration verifier over the combined diff before the plan can become
   `COMPLETE`.
4. Stops and blocks on rejection, failure, or unexpected source or policy drift
   (`SOURCE_DRIFT`).

agentctl never commits, pushes, resets, or cleans your checkout. Review and commit
the resulting changes yourself. Commit before preparing the next plan, because
each plan needs a clean baseline.

To correct rejected or blocked work, prepare a replacement plan and link it with
`run replace`. The replacement stays `VALIDATED` until you activate it. The
number of replacement rounds is bounded by `max_correction_rounds`. `run resume`
continues from durable checkpoints without replaying conversations: `VERIFIED`
tasks never run again, and jobs whose controller was lost are marked interrupted
and require review.

`--override` applies to the current command only. It must name a configured
provider and cannot bypass project `allowed_providers`. If the machine-wide
`max_agents` limit is reached, the launch fails with `AGENT_CAPACITY_EXCEEDED`.
That failure is not treated as a provider failure and does not trigger fallback.

The runtime supports repositories of up to 20,000 files and 64 MiB, with at most
2 MiB per file. Verifier diffs are limited to 128 KiB and provider input to
256 KiB. Symlinks, hardlinks, nested repositories or submodules, and read-denied
files in the checkout cause the run to refuse. Ignored files are included in
source snapshots.

## Experiments

Experiments run ordinary long-running programs (training runs, benchmarks,
simulations) under the same sandbox as checks. No model is involved.

```bash
agentctl experiment run --program PATH [--arg V]... [--cwd PATH] [--network] [--env NAME]... \
    [--timeout-ms N] [--boundary SPEC]... [--max-wakeups N]
agentctl experiment run --command KEY [--network] [--env NAME]... [--timeout-ms N] [--boundary SPEC]...
agentctl experiment status <experiment-id>
agentctl experiment list
agentctl experiment cancel <experiment-id>
agentctl experiment restart <experiment-id>
agentctl experiment metrics <experiment-id> [--attempt N] [--name NAME] [--limit N]
agentctl experiment checkpoints <experiment-id> [--attempt N] [--limit N]
agentctl experiment events <experiment-id> [--attempt N] [--limit N]
agentctl experiment boundaries <experiment-id>
agentctl experiment decisions <experiment-id>
agentctl experiment wakeups <experiment-id>
```

- `experiment run` stays in the foreground and supervises the process. The
  default timeout is 24 hours; the maximum is 30 days.
- `--command KEY` runs the project's `[commands.KEY]` as declared.
- Network is off unless you pass `--network`, and project `deny_network` wins.
  `--env NAME` explicitly passes a variable from your environment (up to 32 names;
  values are redacted from captured output).
- `cancel` from another terminal only takes effect while a live controller is
  polling. `restart` starts a new attempt.

### Structured events

Each attempt receives an `AGENTCTL_EVENT_FILE` path. Instrumented programs append
one JSON object per line. Ordinary stdout and stderr are never parsed.

```json
{"type":"metric","sequence":1,"timestamp_ms":1700000000000,"source":"trainer","name":"loss","value":0.183,"step":1200}
```

Rules for event frames:

- Every frame needs `type` (`metric`, `checkpoint`, `health`, or `status`),
  `sequence`, `timestamp_ms`, and `source`.
- Frames are at most 64 KiB, and metric values must be finite.
- Replaying a frame exactly is idempotent. Conflicting reuse of a sequence number
  is recorded as an ingestion-health fact. A partial trailing frame is rejected
  when the process exits.
- Checkpoints are workspace-relative file references. agentctl records their
  observed size and mtime, plus a BLAKE3 hash for files up to 64 MiB. Traversal,
  absolute, Git-administrative, protected, and symlink-escaping paths are
  rejected.
- Per-attempt event volume is capped by `[runtime.security.experiment_events]`.
- Queries return 1,000 events by default (maximum 10,000).

### Decision boundaries

```text
--boundary ID:METRIC:OP:VALUE:record[:TAG=VAL,...]
--boundary ID:METRIC:OP:VALUE:planner:VERIFICATION_REF[:TAG=VAL,...]
```

`OP` is one of `<`, `<=`, `>`, `>=`, `==`. Boundaries are deterministic scalar
comparisons over recorded metric events; no model evaluates them. Omitting the tag
selector matches only an **untagged** metric of that name. It is not a wildcard.

- `record` only records the decision.
- `planner` records the decision and creates a **planner wakeup**: a new planning
  request carrying the decision's context, bound one-to-one to the decision. It
  needs a verification profile already declared in project policy.

Creating a wakeup never calls a provider. Run `agentctl run planner <request-id>`
to plan against it; the normal plan activation and verification rules still
apply. Each experiment allows 3 distinct wakeups by default (`--max-wakeups`, up
to 20) and up to 32 boundaries. Boundaries never kill, restart, or launch
anything.

## Observability

```bash
agentctl observe snapshot
agentctl observe sessions | agents | tasks | events | experiments | usage
agentctl observe session <id> | agent <id> | job <id> | task <id> | experiment <id>
agentctl observe usage provider <name> | task <id> | role <role>
agenttop
agenttop --once --width 100 --height 30      # render once as text, no interactive terminal needed
```

`observe` and `agenttop` share one bounded, read-only projection of canonical
state. They never start work, contact providers, or change state. Views cover
recent history (bounded, with explicit truncation warnings), not full history.

`agenttop` shows a rolling tokens-per-minute graph over the last ten minutes, then
the selected session's agent tree, TaskPacket DAG, a probe for the selected worker
or task, and recent events. Keys: **q**/Esc quit, **↑/↓** select, **Tab** switch
between agents and tasks, **Enter** expand the probe, **s** cycle sessions,
**g** cycle graph scope (aggregate, provider, task, role), **r** refresh, **?**
help. It polls once per second.

How to read the output:

- Progress is shown as `N/M VERIFIED`, never as a guessed percentage.
- Job lifecycle (for example `RUNNING`) is separate from liveness. `LIVE` requires
  the controller that owns the child process to have just confirmed it is still
  running. A separate `observe`/`agenttop` process therefore shows `UNKNOWN`
  liveness.
- Token usage keeps its provenance (`EXACT`, `ESTIMATED`, `UNKNOWN`,
  `MIXED`, `PARTIAL`). Missing usage is unknown, not zero. Current adapters
  report usage when a job finishes, so the graph shows bursts.

## Analytics

```bash
agentctl analytics summary | usage | roles | routes | corrections [filters]
agentctl analytics session <id> | task <id> | job <id> [filters]
```

The available filters are `--repository ID`, `--workspace ID`, `--session ID`,
`--role ROLE`, `--provider NAME`, `--model NAME`, `--task ID`, `--job ID`,
`--lifecycle STATE`, `--from-ms N`, `--to-ms N`, and `--limit N` (default 10,000,
maximum 20,000).

Analytics are read-only descriptive views over durable state. The default scope
is the current workspace, covering jobs created in the last seven days.
`--repository` combines a repository's workspaces. The views report token usage
with separate exact and estimated buckets, verifier decisions, reject and fallback
rates with explicit denominators, route provenance, policy skips, correction
lineage, context sizes, and latency distributions.

No pricing is assumed, so monetary cost is `UNKNOWN`. These are descriptive
measurements; they do not rank models or change routing. `--json` returns the
complete bounded snapshot.

## Protocol tools

```bash
agentctl schemas generate [--output DIR]   # write JSON Schemas (default: schemas/)
agentctl protocol validate <type> <file>   # structural and semantic validation of one document
```

`agentctl --help` lists the protocol document types.
