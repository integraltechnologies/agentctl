# Architecture

This document describes the architecture of agentctl `0.3.0-alpha` as it is
implemented. It explains the components, the durable state they share, and the
invariants the implementation enforces. It is not a function reference.

- [Division of responsibility](#division-of-responsibility)
- [Project-local state](#project-local-state)
- [Plans and human intent](#plans-and-human-intent)
- [Agents, invocations and the provider-neutral runtime](#agents-invocations-and-the-provider-neutral-runtime)
- [The structured action journal](#the-structured-action-journal)
- [Planning](#planning)
- [Scheduling and concurrency](#scheduling-and-concurrency)
- [Mutation ownership](#mutation-ownership)
- [Executors, generations and candidates](#executors-generations-and-candidates)
- [Independent task verification](#independent-task-verification)
- [Acceptance](#acceptance)
- [Accepted source and working-tree drift](#accepted-source-and-working-tree-drift)
- [CodeGraph](#codegraph)
- [Replanning](#replanning)
- [NEEDS_ATTENTION and human decisions](#needs_attention-and-human-decisions)
- [Final integration verification and completion](#final-integration-verification-and-completion)
- [Process lifecycle](#process-lifecycle)
- [Cancellation, pause and timeout](#cancellation-pause-and-timeout)
- [Crash and restart recovery](#crash-and-restart-recovery)
- [Events and observation](#events-and-observation)
- [Concurrency between plans and processes](#concurrency-between-plans-and-processes)
- [Current architectural limits](#current-architectural-limits)

## Division of responsibility

| Part | Owns | Never owns |
| --- | --- | --- |
| Models (providers) | Engineering intelligence for one invocation | Any canonical state |
| Planner (a role) | Engineering judgment: task decomposition, scopes, ordering, replanning, when to raise a concern, when to propose completion | Applying anything; human intent; accepted source; runtime truth |
| agentctl (coordinator, runtime, state store) | Execution truth, authority, coordination, continuity: what ran, what it changed, who may change what, what is accepted | Engineering judgment |
| CodeGraph | Structural facts about accepted source | Working-tree bytes, unaccepted candidates |
| Verifiers (a role) | Independent evidence about one candidate, or about a plan's accepted result as a whole | Acceptance or completion |
| Durable state and content storage (`.agentctl/`) | Continuity across invocations, processes and restarts | — |

Every agent invocation starts fresh from canonical state. A provider session is
recorded as metadata only; it is never resumed and never identifies anything.

## Project-local state

A project is the directory holding `agentctl.toml`, found from the current
directory by walking up to the nearest ancestor that contains one. `agentctl init`
puts a new project at the root of the enclosing Git repository if there is one.

| Path | Purpose | Version-controlled |
| --- | --- | --- |
| `agentctl.toml` | Portable project configuration ([configuration.md](configuration.md)) | Yes |
| `.agentctl/state.db` | The canonical SQLite store (with its `-wal`/`-shm` files) | No |
| `.agentctl/objects/` | Content-addressed recovery objects: files named by the SHA-256 of their bytes | No |
| `.agentctl/` lock files | One per agentctl session, and a recovery lock | No |

`.agentctl/` is local and disposable as a whole: agentctl adds `/.agentctl/` to
the project's `.gitignore` when it first creates the directory. Deleting it
discards all plans and history; `agentctl init` then starts fresh local state
from the unchanged configuration.

The store has exactly one writer type (`Store`). Every write is a single
`IMMEDIATE` SQLite transaction that also appends the event describing it, so
concurrent agentctl processes serialize on the database and state and history
never diverge. The schema itself enforces structure and many lifecycle facts
with constraints and triggers. It is one canonical schema (`user_version` 1).
A store with any other schema is refused and left untouched; there are no
migrations from pre-release development stores.

A recovery object is published under its hash name only once complete, and is
never replaced. Canonical state refers to an object only once it is durable.

## Plans and human intent

A plan is created from human intent:

- an **objective**;
- **constraints** (zero or more);
- **completion criteria** (zero or more).

Intent is fixed when the plan is created. No planner command can change it; a
changed intent is a new plan.

Plan states:

| State | Meaning | Entered by |
| --- | --- | --- |
| `planning` | Its planner is decomposing the intent | `plan create` |
| `ready` | Planning was finalized with an executable DAG | the planner's `finalize` command only |
| `running` | Eligible tasks may be claimed | `run` (from `ready`), `plan resume` |
| `paused` | Nothing more of it is claimed | `plan pause`, `plan cancel` |
| `needs_attention` | A concern awaits a human decision, or a human instruction awaits the planner | a replan's `raise_attention`; left only through decisions and replans |
| `completed` | A final integration verification passed its accepted result | that pass only; never left |

A plan is a DAG of **tasks**. Each task has a key unique in its plan, an
objective, context for its worker, a **scope** (the exact literal project paths
it may mutate), and dependencies on other tasks of the same plan.

## Agents, invocations and the provider-neutral runtime

A **logical agent** is created for one purpose: the planner of a plan, the
executor of one generation, the verifier of one verification, or the verifier
of one integration verification. An **invocation** is one provider process run
on behalf of an agent. An agent's invocation is never resumed; later work gets
a fresh agent.

The runtime runs every invocation through one provider-neutral contract:

- The whole input is delivered on standard input, which is then closed.
- The provider must answer with exactly one structured result that satisfies
  the launch's JSON Schema. agentctl validates it itself; prose is never a
  result.
- The runtime classifies how the invocation ended (`succeeded`, `failed` with a
  failure kind, `cancelled`, `interrupted`) and records it, with token usage and
  its provenance (provider-reported, local estimate, or unavailable).
- Provider-controlled text (output, standard error, error messages) is never
  recorded durably; standard error is returned to the caller for diagnosis only.

Adapters are the only place provider command lines and output formats live.
agentctl ships three: `claude`, `codex` and `generic` (agentctl's external-agent
protocol, version 1). See [providers.md](providers.md).

Each role runs in a distinct working directory and editing mode:

| Role | Working directory | Workspace mode |
| --- | --- | --- |
| Planner | The project root | read-only |
| Executor | A disposable copy of the repository outside the project | editable |
| Task verifier | A disposable view: accepted source plus the candidate | disposable (may also run commands) |
| Integration verifier | A disposable copy of the accepted repository | disposable |

The workspace mode is passed to the provider, which enforces it in its own way.
agentctl adds no sandbox and relies on none; it protects itself by giving
editable and disposable modes only directories it can afford to lose, and by
observing what changed there itself.

## The structured action journal

Every engineering-control action an agent performs is journaled as
**INTEND → ACT → RECONCILE**:

1. **Intended**: the action is recorded with a structured intent (an
   agentctl-defined kind such as `executor.run`, with literal parameters) and
   the authority and baseline it relies on. Nothing has happened yet.
2. **Attempted**: recorded before any process that could act exists, bound to
   the invocation that performs it. From here its outcome is unknown until
   reconciled; nothing, including how the invocation ended, stands in for
   reconciliation.
3. **Reconciled**: agentctl observes what actually happened and records the
   outcome (`completed_as_intended`, `completed_with_deviation`, `failed`) with
   structured evidence, in the same transaction that records the consequences.

Journaled actions are `executor.run`, `executor.install`, `verifier.run`,
`verifier.integrate` and `planner.replan`. An action left intended by a process
that ended is **withdrawn** by recovery; an attempted one whose process ended is
settled by recovery from what can be proven, never guessed.

## Planning

The planner receives JSON holding the human intent, the plan's tasks, the
configured source roots and a map of accepted source from CodeGraph (per-file
entity lists, bounded). It answers with a list of commands:

| Command | Initial planning | Replanning |
| --- | --- | --- |
| `add_task` | yes | yes |
| `update_task` | yes | yes (tasks the input marks as changeable) |
| `remove_task` | yes | no |
| `set_dependencies` | yes | yes |
| `finalize` | yes (last) | no |
| `cancel_task` | no | yes |
| `retry_task` | no | yes |
| `raise_attention` | no | yes |
| `propose_completion` | no | yes, alone |

agentctl treats a planner's response as untrusted input. It validates every
command against the state its predecessors leave and the result as a whole
(keys, bounds, dependencies within the plan, no cycles, scopes within the
configured source roots, never `agentctl.toml`, `.agentctl/` or Git state, every
path canonical and literal), and applies all of it in one transaction or none
of it. A refused or failed planning invocation leaves the plan exactly as it
was. The planner's prose explanation is shown once and never recorded.

Paths are literal throughout. `src/[slug].ts`, `src/(group)/page.tsx`,
`src/a+b.rs`, names with spaces or `@`, and Unicode names each name one file.
On Unix even `*` and `?` are ordinary characters; on Windows, names the
filesystem cannot address literally are refused.

## Scheduling and concurrency

`agentctl run <plan>` starts a `ready` plan (it becomes `running`) and runs the
scheduler. The scheduler owns execution truth, never engineering judgment: it
never creates, changes or reorders tasks, never retries a pipeline that stopped
short, and never completes a plan.

A task is **eligible** only while its plan is running with a valid DAG, it is
not cancelled, no generation ever served it (unless a replan explicitly
authorized a retry of its current definition), every dependency is completed,
and no other generation owns any path of its scope.

Claiming is one transaction: the task is found eligible, fewer claims than the
configured ceiling (`agents.max_concurrency`) are held across **every plan** of
the project, the working tree holds exactly the accepted state of every path of
the task's scope, and then a new **generation** is started, bound to the task's
current definition, owning its whole scope, and claimed. All of it or nothing.

Each claimed generation runs its pipeline on its own thread with its own store
connection:

```text
executor → install candidate → independent verifier → (on pass) acceptance
```

Providers of different pipelines run concurrently. Within one agentctl process,
observations and writes of the working tree are serialized so one pipeline
never observes another's half-written install. The scheduler is event driven:
it claims what it can, sleeps until a pipeline ends, then looks again, always
in planner order (ascending task id). `run` returns when nothing more can be
launched and every launched pipeline has ended.

A claim holds capacity until released, which the store allows only once
nothing the pipeline started can still be live and its outcome is established
(`accepted`, `not_executed`, `execution_failed`, `install_failed`,
`verification_failed`, `verification_inconclusive`, `acceptance_declined`).
Only `accepted` completes the task. Every other outcome leaves the task
**stopped** for the planner.

## Mutation ownership

Ownership records which generation holds exclusive authority to mutate each
project path.

- Paths are exact literals: owning `src/a` says nothing about `src/a/b` or any
  path it would match as a pattern.
- A generation acquires a set of paths within its task's scope atomically: all
  of them, or none if any is owned by another generation.
- Ownership of a path is granted only while the working tree holds exactly its
  accepted state (its accepted bytes, or nothing where it is accepted absent or
  has no accepted state). Bytes that differ are someone else's work.
- Ownership ends only by explicit release: a completed acceptance, or a
  replan that abandons a stopped generation. A generation ending, a scope
  change or a restart does not release it. Stale ownership is safe.

Reading is never restricted by ownership.

## Executors, generations and candidates

A **generation** is one attempt at one task. It gets exactly one **executor**:
a logical agent embodied by one fresh invocation.

1. With ownership of the whole scope held, agentctl observes the repository
   (every path Git lists as repository content: tracked, or untracked and not
   ignored; never ignored files, `.agentctl/` or Git's own state). It re-checks
   that the working tree holds accepted state at every scope path.
2. It stages a **workspace**: a disposable copy of that observation outside the
   project, holding ordinary files only, with no `.git` and no `.agentctl`.
3. It journals the execution as intended, with that baseline, and as attempted
   before the executor process exists.
4. The executor edits the workspace in its provider's editable mode. Its input
   holds the task, the intent it serves, its exact authority
   (`mutable_paths`) and a CodeGraph map of accepted source.
5. When the invocation ends, agentctl observes the workspace itself and derives
   what changed, whether every change stayed within the generation's authority,
   and the outcome: `candidate`, `reported_failed`, `malformed_result`,
   `invocation_failed`, `scope_violated`, `unattributable` or `interrupted`.
   What the executor reports (status, summary, claimed paths) is kept as a
   claim, never proof.

A **candidate** is an authorized workspace delta against the observed
baseline: structurally valid work awaiting verification. It is not accepted
source and does not complete a task.

A candidate is then **installed** into the project's working tree as a
separately journaled action, only while every changed path still holds what the
baseline observed there. Installing is prepared first: both the candidate's
bytes and the bytes they replace are published as recovery objects, so an
interrupted install can always be completed or undone. Install outcomes:
`installed`, `drifted` (a path changed; nothing written), `refused`, `failed`
(everything written was restored).

Nothing of an execution that is not a candidate reaches the project. The
workspace is discarded. The generation keeps its ownership whatever the outcome.

## Independent task verification

A **verifier** is a fresh logical agent created for one verification of one
generation's installed candidate. It is never the executor, never continues the
executor's session, and judges once.

- Its input is built from canonical facts alone: the task, the intent, the
  executor's exact authority, the candidate as agentctl captured it (each
  changed path with its exact bytes) and CodeGraph knowledge of accepted source,
  marked as describing the state before the candidate. Nothing the executor
  said is given.
- It works in a disposable **view** outside the project: accepted content read
  from recovery objects, the candidate at its changed paths, and working-tree
  bytes only at paths no candidate ever touched. Another pipeline's unaccepted
  candidate in the shared working tree therefore never affects what a verifier
  judges.
- It may read, build, test and lint. Changing repository source in its view is
  a boundary violation.
- The outcome is derived from agentctl's own observations first (did the
  working tree keep holding the candidate? did the verifier change source?) and
  from the verifier's report only then: `passed`, `failed`,
  `candidate_drifted`, `candidate_changed`, `boundary_violated`,
  `invocation_failed`, `malformed_result`, `interrupted`.

A pass requires a `pass` verdict with at least one passed check. A pass is
evidence for acceptance, never acceptance: verifying changes no accepted source,
no CodeGraph fact and no ownership.

## Acceptance

Acceptance turns the exact candidate that an independent verification passed
into accepted repository state. It proceeds in durable phases, each its own
transaction:

```text
verifier PASS
  ↓
1. re-establish acceptability from canonical state alone
   (generation active and owning its scope; capture, install and latest
    verification reconciled as candidate / installed / passed; every
    content to publish available as a recovery object whose bytes hash
    to its name)
  ↓
2. PUBLISHED: in one transaction, observe that the working tree holds the
   candidate at every changed path, then record the acceptance and make the
   candidate's captured identities accepted source
  ↓
3. SYNCHRONIZED: CodeGraph refreshed for every changed path from the
   published content, read from recovery objects
  ↓
4. COMPLETED: generation accepted → task completed → whole ownership released
  ↓
the scheduler releases the pipeline's claim (capacity)
```

The accepted-source transition (publication) therefore happens before the
CodeGraph refresh, and ownership is released last. Published identities come
from the execution's capture, never from bytes read at acceptance; observing
the working tree only decides whether publication may go ahead.

Failing before publication records nothing: accepted source, CodeGraph,
ownership and the generation stay as they were. Failing after publication
leaves the acceptance durably incomplete at the phase it reached; its source
stays accepted, its generation stays active and owning its paths, and the
remaining phases are finished (by the pipeline or by recovery) exactly as
recorded, never publishing anything else.

Only a completed acceptance completes a task, and only a completed task
satisfies a dependency. Dependent tasks therefore always start from accepted
source that already includes their dependencies' accepted work, with CodeGraph
synchronized.

## Accepted source and working-tree drift

**Source** is what Git considers repository content (tracked, or untracked and
not ignored) within the configured source roots, excluding `.agentctl/`,
`agentctl.toml` and anything inside a `.git` entry. Only regular files carry
source content. The store records, for each tracked source path, its accepted
content hash (or accepted absence) and the generation whose acceptance produced
it. The bytes live in recovery objects.

`agentctl init` captures the **baseline**: every eligible source file with no
accepted state yet. A path's accepted state changes after that only through
acceptance of a verified candidate.

The working-tree invariant is:

```text
accepted bytes  =  executor starting bytes  =  restoration target
```

It is enforced at each step:

- **Claim.** A task is claimed only in the same transaction that finds the
  working tree holding exactly the accepted state of every path of its scope.
  Otherwise nothing is acquired, and `run` reports the drifted paths.
- **Executor start.** Before staging, the executor re-checks that its observed
  baseline equals accepted state at every scope path, and refuses to start
  otherwise.
- **Install.** A candidate is written only while every changed path still holds
  the baseline bytes.
- **Restoration.** When a replan abandons a stopped generation, each path of
  its installed candidate is restored to the last accepted state from recovery
  objects, and only while the path still holds exactly what the candidate
  installed (or already holds accepted state). If a path holds anything else,
  nothing is written and the replan is refused.

Consequences:

- Human work in progress inside a task's scope is never overwritten, never
  taken as an executor's starting point, and never accepted under a task's
  name. The task simply waits until a human reconciles the difference (for
  example by committing or stashing it elsewhere and restoring the file).
- Writes that did not come through a verified candidate (for example a
  provider writing into the project by absolute path) cannot be laundered into
  accepted source through a later task: they show up as drift.
- agentctl never commits, pushes, checks out or resets. Accepted source lives
  in agentctl's store; Git history is the human's.

## CodeGraph

CodeGraph is agentctl's persistent, language-neutral store of structural facts
about **accepted** source, never working-tree bytes.

- A language frontend derives, from one file's accepted content, the entities
  it defines (kind, symbol, byte span) and the direct relations it asserts.
  `contains` relations are *proven*; relations naming things as written
  (`imports`, `calls`, `constructs`, `references`, `extends`, `implements`,
  `self_type`) are *inferred* and point at unresolved external symbols.
- Frontends are syntax-only (Tree-sitter): no name resolution, type inference,
  macro expansion or cross-file reading.

| Language | Extensions |
| --- | --- |
| Rust | `.rs` |
| Python | `.py`, `.pyi` |
| JavaScript | `.js`, `.mjs`, `.cjs`, `.jsx` |
| TypeScript | `.ts`, `.mts`, `.cts` |
| TSX | `.tsx` |

Other files are accepted source without a graph. A file its frontend cannot
parse keeps no graph.

**Freshness.** Facts are bound to the content hash they were derived from. A
source's graph is `current` while that content is accepted, and `stale` once
other content (or absence) is accepted; queries return facts only as current and
say so when they cannot. Acceptance synchronizes the graph of every changed
path in its `synchronized` phase, and a plan cannot be proposed complete while
its accepted source is out of sync with CodeGraph.

CodeGraph feeds agent inputs: the planner, executors, verifiers and integration
verifiers receive bounded per-file entity maps of accepted source.

## Replanning

`agentctl plan update <plan>` on a finalized plan invokes a fresh planner with
canonical feedback: every task's status and recent generation history
(executions, installs, verifications with verifier blockers marked as claims),
earlier replans, concerns with their decisions, recent integration
verifications, and repository context drawn from accepted source.

- The proposal is bound to the **basis** (the plan state the feedback showed).
  If that state changed while the planner worked, nothing is applied (stale).
- A replan may change only the frontier nothing is running for: tasks never
  served, tasks whose latest generation conclusively stopped, and tasks already
  authorized to retry. Completed, live, unresolved and cancelled tasks are
  beyond it.
- Task definitions are versioned. A claim binds each generation to the
  revision it executes.
- `retry_task` authorizes exactly one fresh generation of the task's current
  definition. Revising the task afterwards makes the authorization unusable.
  Nothing else ever runs a task again.
- `retry_task` and `cancel_task` **abandon** the stopped generation: its
  ownership is released, it ends as `rejected` (a verifier failed it) or
  `failed`, and its installed candidate is restored to the last accepted
  state, inside the replan's transaction and before it commits.

Replanning is itself journaled (`planner.replan`), reconciled in the
transaction that applies it.

## NEEDS_ATTENTION and human decisions

Only a planner raises a **concern** (`raise_attention`, during a replan), and
only a human decides one. A concern has a key its plan raises at most once, a
reason, evidence and affected tasks. Raising it moves the plan to
`needs_attention`: nothing more of it is claimed; a provider already running
runs to its end and is recorded, and its pipeline stops at the next step that
needs the plan ready, running or paused (starting an executor, accepting);
nothing accepted, recorded or owned is rewritten; other plans are unaffected.

`agentctl plan decide` records a human decision, exactly once per concern:

| Decision | Effect |
| --- | --- |
| `accept` | The plan continues unchanged despite the concern. When no concern blocks it and no instruction awaits the planner, it is `running` again in the same transaction. |
| `instruct` | The instruction is given to the next planner as authoritative input. The plan stays `needs_attention` until a replan acts on it (`plan update`). |
| `stop` | The plan never continues autonomously. A changed intent is a new plan. |

Decisions are canonical and reach every later planner and the integration
verifier.

## Final integration verification and completion

Every task completing is not evidence that the intent is met. A plan completes
only through **final integration verification**:

1. The plan is **settled**: every task completed or cancelled, nothing live or
   unresolved, no blocking concern, no unfinished acceptance, and its accepted
   source synchronized with CodeGraph. The scheduler reports this as
   `AllCompleted`; the plan stays `running`.
2. A replan's planner judges the intent met and issues `propose_completion`,
   alone. That only makes verification possible.
3. A fresh integration verifier judges the plan's accepted result as a whole,
   in a disposable copy outside the project holding accepted source (from
   recovery objects) plus **repository inputs**: every other path Git lists as
   repository content (manifests, lock files, build scripts, fixtures,
   configuration) and always `agentctl.toml`. Ignored and generated files are
   never inputs. Its input is the intent, the tasks and where each ended, the
   paths their accepted work changed, the human's decisions and a CodeGraph map.
   Nothing any executor, verifier or planner said is given.
4. Its outcome is derived first from whether its **basis** still holds when it
   ends (the proposal still latest, the plan still settled, accepted source and
   repository inputs unchanged): otherwise `basis_changed`. Then from whether it
   changed repository source (`boundary_violated`), then from its report.

**Completion guard.** Only a `passed` integration verification completes a plan,
in the same transaction that records it. A `failed` one's blockers become
canonical feedback for the next planner; nothing is retried. `plan update`
runs the verification at once when its planner proposes completion;
`plan verify` runs it again for an existing proposal.

## Process lifecycle

agentctl does not implement process containment itself. Every invocation's
processes run in a **lifecycle domain** created by
[procd](https://github.com/integraltechnologies/procd), an external lifecycle
authority. agentctl links procd statically and consumes it only as installed:
the build takes `procd.h` and procd's static library from where the target's
C toolchain finds them, or from `PROCD_INCLUDE_DIR` and `PROCD_LIB_DIR` set
together (see the [README](../README.md#building-from-source)), and never builds or
fetches procd. agentctl's bindings (`src/procd.rs`) are checked against that
same header: the signatures of the functions it calls when it compiles, and
the structure layouts and constants by its tests.

- The domain is created, at the strength the launch requires, before anything
  is recorded or run, and its durable identity is recorded with the invocation
  before its first process exists.
- procd starts `agentctl-shim` as the domain's first process. The shim connects
  back to agentctl over loopback (authenticated with a token from a private
  directory), then starts the provider inside the domain with exactly the
  standard streams, working directory and environment agentctl constructs, and
  relays them. It classifies nothing.
- The agentctl process that launched an invocation holds its domain, and the
  only authority over its processes, until the invocation ended. When the
  invocation ends, however it ends, agentctl terminates the domain through
  procd.

agentctl's roles launch where procd supports process-tree termination at
least best effort (`ROLE_LIFECYCLE = AllowBestEffort`); where procd supports
none, nothing is launched. How an invocation ended is recorded only once
terminating its domain established the end of its processes, and the record
carries the strength, as the invocation's **termination** column and in the
`invocation.ended` event:

| Termination | Meaning |
| --- | --- |
| `enforced` | procd proved the domain empty: admission closed, authority-directed, emptiness proven at an enforced level, final state empty. |
| `best_effort` | procd's backend tracks the domain only best effort (macOS): it closed admission, killed every process it tracked, and a final scan found none. A process it never tracked is not excluded. |

Anything less leaves the invocation unresolved: it keeps no end, and only
recovery can settle it. The meaning of ENFORCED, BEST_EFFORT and UNSUPPORTED
per platform is detailed in [security.md](security.md).

## Cancellation, pause and timeout

- **Pause** (`plan pause`): the plan stops being claimed. Pipelines already
  running end as they would: execution, verification and acceptance of work
  already claimed may still complete while the plan is paused.
- **Cancel** (`plan cancel`): a durable cancellation request is recorded, which
  pauses a running plan in the same transaction. Each agentctl process running
  a covered invocation polls for requests (every 250 ms), terminates the domain
  through procd and records the end. A cancelled pipeline starts no further
  invocation. Nothing is marked cancelled until procd established the end, and
  nothing owned or accepted is released: the planner decides what follows.
- **Timeout**: every invocation has a deadline (`agents.invocation_timeout_minutes`,
  default two hours). Past it, it is ended exactly as a cancelled one.
- **Interrupt**: `run`, `plan create`, `plan update` and `plan verify` catch
  SIGINT, SIGTERM and SIGHUP (Ctrl-C, Ctrl-Break and console close on Windows).
  The process launches nothing more, terminates every live domain it holds,
  records how each ended, then exits with an error.

## Crash and restart recovery

A **session** is one agentctl process's identity in the store, together with a
lock file it holds exclusively for as long as it runs. The operating system
releases the lock when the process ends however it ends, so taking it proves
the session ended. Process ids are never used for this.

Whatever may still be live or has no established outcome (invocations without
an end, attempted actions, claims, unfinished acceptances) is **unresolved**,
listed with the session that recorded it. While that session runs, it is its
work. Once it ended, nothing new starts on that plan (no claim, executor,
verifier, planner, integration verification or acceptance) until
`agentctl recover` settles it. Other plans run on.

`agentctl recover`, for each ended session, in order:

1. **Invocations** without an end: settled only by procd's proof that the
   recorded domain is gone (reacquire and terminate with proof of emptiness,
   or proof of destruction), then recorded `interrupted`. Anything less leaves
   it unresolved, and everything waiting on it.
2. **Attempted actions**: executions, verifications and integration
   verifications whose process ended before recording a result become
   `interrupted` (judging and accepting nothing); an unapplied replan is failed;
   an interrupted install is completed where every path holds the candidate,
   otherwise restored to what each path held before, never over anything else.
3. **Intended actions** never attempted: withdrawn.
4. **Acceptances** that published and did not complete: finished exactly as
   recorded.
5. **Claims**: released once their pipeline's outcome is established.

Recovery never retries, judges, accepts or completes anything. It is
idempotent: an interrupted recovery is finished by the next. What cannot be
proven is reported `blocked` or `unsupported` and left exactly as found, and
the command exits with an error.

## Events and observation

Every state change appends an **event** (sequence number, time, kind, optional
plan/task/agent, detail) in the same transaction. Events are chronology only;
workflow status is always computed from canonical state.

All observation surfaces share one read-only model:

- `agentctl status`: plans, task counts by status, what awaits a human,
  unresolved records, claims held against the ceiling, and token usage.
- `agentctl logs`: the event log, filterable, optionally followed.
- `agenttop`: a live terminal monitor of invocations, token usage and recent
  events, aggregated by provider, plan, task, role or agent.

Observation writes nothing and never takes a session or recovery lock, so a
poller cannot make a dead session look alive or hold back recovery. Unknown
stays unknown: an invocation with no recorded end is shown as such (never as
running or succeeded), its usage is *pending*, an invocation that ended without
usage is *unavailable*, and neither is ever counted as zero tokens.
Provider-reported and locally estimated counts are never merged. Every string
that can originate outside agentctl is escaped before it reaches a terminal.

## Concurrency between plans and processes

- Several plans may exist and run at once, from one or several `agentctl run`
  processes. Every claim of every plan counts against the same
  `max_concurrency` ceiling, enforced by the schema.
- Ownership is exclusive across plans: two plans can never hold the same path.
  A task whose scope another generation owns waits (`waiting` for ownership).
- The recovery barrier is per plan: interrupted work of one plan never blocks
  another.
- Within one process, working-tree observations and writes are serialized.
  Across processes they are not; an observation that another process's install
  disturbs fails and is recorded truthfully (for example a verification ending
  without a judgment), never retried.

## Current architectural limits

- The baseline is captured once. There is no command to adopt a human's later
  edit of an already-accepted path as accepted source; the human reconciles
  the working tree instead (see [Accepted source and working-tree drift](#accepted-source-and-working-tree-drift)).
- After `run` reports `AllCompleted`, a human runs `plan update` to have the
  planner propose completion. Replanning after stopped tasks is likewise
  human-initiated; `run` never replans.
- CodeGraph frontends are syntax-only. Relations are stored and traversable in
  the library, but agent inputs currently carry entity maps, not relation
  traversals.
- Recovery after an agentctl process is killed outright depends on procd
  proving the orphaned domain's fate, which is not possible on every host (see
  [security.md](security.md)).
