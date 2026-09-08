# Architecture contract — Stage 0

## Ownership and roles

`agentctl` owns canonical engineering state at machine scope. Provider conversations
and harnesses are replaceable clients. This project must not become a Claude wrapper,
a Codex wrapper, another autonomous coding agent, a chat-memory store, or a giant
swarm framework.

| Role | Responsibility | Context contract |
| --- | --- | --- |
| Planner | High-compute understanding of the objective; creates the task DAG and later targeted corrections | User objective, canonical invariants, future repo graph/shared memory |
| Executor | Lower-compute implementation of one bounded packet | Packet plus the minimum necessary context; only assigned work |
| Verifier | Independent lower-compute assessment of a packet or integrated plan | Fresh context containing objective, invariants, diff/source state, and evidence; never executor reasoning |

No role selects a provider or model. Optional `ProviderMetadata` contains opaque
strings on jobs and observations only. Adapter-specific policies and tool syntax
belong outside the core. The planner should eventually choose the smallest coherent
set of specific packets that maximizes safe parallelism while minimizing frontier
tokens, critical-path time, repeated context, uncertainty, merge risk, and coordination
overhead. Stage 0 contains no optimizer or scheduler.

## Packets and verification

`PlanPacket.tasks` is nonempty. Each task owns its dependency list: there is no second
edge list that could disagree. IDs are unique within the plan; missing dependencies,
self-dependencies, repeated dependencies, and cycles are invalid. Array order does
not imply execution order.

`TaskPacket` carries an objective, explicit read/write scope, graph entity references,
critical invariant references, dependencies, definition of done, and mandatory
verification requirements. Scopes are normalized repository-relative files or directory
subtrees, without globs, absolute paths, or `.`/`..` components. An empty scope grants
no access; read and write permissions are separate. Paths are lexical contracts;
future execution must enforce canonical-path/symlink containment and protected-data
policy. Graph references do not grant extra access. Packets contain current deltas
and compact references, not repository prose, source dumps, or provider instructions.

The task state graph is:

```text
PLANNED -> READY -> EXECUTING -> AWAITING_VERIFICATION -> VERIFYING -> VERIFIED
                                                                  -> REJECTED
Any nonterminal work state -> BLOCKED -> PLANNED (explicit planner reassessment)
```

Only `VERIFIED` means complete and satisfies dependencies. Becoming `READY` and
starting execution both require verified prerequisites. Missing prerequisite state
never counts as completion. Executor `ResultStatus::Succeeded` reports an outcome;
it does not verify a packet. Results report changed paths/entities, evidence, concise
notes, and explicit failure/blocker details, with no reasoning field.

Every task receives an independent verifier. `VERIFYING -> VERIFIED` requires a valid
matching packet-level PASS covering the packet's required checks and critical invariants,
plus evidence when required. `VERIFYING -> REJECTED` requires a matching rejection with
structured findings. BLOCKED decisions explain why verification could not proceed.
PASS cannot contain ERROR or CRITICAL findings. Setting `evidence_required: false`
does not disable verification or required checks.

VERIFIED and REJECTED are terminal for that packet. Rejection returns to the planner
for a new correction/delta packet and a revised plan with explicit dependency changes;
there is no autonomous executor/verifier retry loop. Treat issued packet IDs as immutable
identities. A revised packet or plan gets a new ID; v1 has no automatic migration,
supersession, or dependency-rewrite machinery. Blocked work can be reassessed through
PLANNED, but has no direct path back to execution or completion.

`VerificationTarget` is a tagged union: PACKET contains a task and executor job ID;
INTEGRATION contains a plan and contributing executor job IDs. A verifier job must
differ from those executor jobs. Even after every packet is VERIFIED, plan completion
requires an INTEGRATION PASS for that plan, covering integration requirements and the
union of critical task invariants. Packet verification alone can never complete a plan.

Job state describes an invocation, independently of task acceptance:

```text
QUEUED -> RUNNING <-> WAITING
QUEUED -> CANCELLED
RUNNING or WAITING -> SUCCEEDED / FAILED / CANCELLED
```

Terminal job states cannot restart. Waiting can describe dependencies, external jobs,
or required input. A successful executor job is not a verified task. An executor job
must reference a task; planner and integration verifier jobs may reference only a plan.

The library implements pure validation, not enforcement infrastructure. Structural
transition predicates alone are insufficient: callers must use contextual plan guards.
The future runtime must authenticate job roles and decision authorship, require fresh
verifier context, verify the listed executor jobs against actual task results, resolve
evidence and check results against the exact source/diff, and atomically persist valid
state transitions. Caller-supplied VERIFIED states and verification references are
trusted inputs at Stage 0; JSON cannot prove their history or authenticity. A resume
document is a summary, never authority to bypass those checks.

## Durable facts and observations

`ResumePacket` describes the active plan/task, lifecycle phase, verified completed work,
pending work, latest verification reference, next action, and optional source state.
It is current state, never transcript replay. Resume completion requires an integration
verification reference, whose target and decision must be resolved by future persistence.

`EvidenceRef` is a compact typed ID. `EvidenceRecord` can describe command/argv/cwd,
source revision and dirty-content hash, timestamps, exit status, stdout/stderr hashes,
full-log reference, and summary. Hash strings use an algorithm prefix, such as
`sha256:<digest>`. Log/source references are opaque, not automatically fetched or
executed. `CommandSpec` is argv, not an implicit shell string. Full logs remain outside
packets. Stage 0 records no evidence and implements no source-state resolver.

Memory provenance distinguishes CANONICAL (explicitly governed authority), DERIVED
(computed from referenced sources), OBSERVED (recorded evidence), and AGENT_NOTE
(agent-authored suggestions). Agent notes require author attribution. AGENT_NOTE must
never silently acquire canonical authority through retrieval, summarization, or reuse;
promotion requires a future explicit, auditable policy decision. There is no persistence
or promotion operation in this stage.

Agent events describe observable actions: starts/finishes, loaded packets, steps,
files/symbols, tools/commands, checks/findings, dependencies, experiments, and token
observations. Event IDs identify observations; tool/command invocation IDs pair starts
and finishes. `ProbeSnapshot` represents current role/task/packet/job, opaque provider,
phase/step/target/tool/command, last event, elapsed/idle time, blockers/dependencies,
and verification check. Neither events nor probes require hidden chain-of-thought.

All timestamps are Unix epoch milliseconds; durations explicitly end in `_ms`.
Token counts are optional unsigned per-observation deltas. Provenance is mandatory:
EXACT means reported exact counts, ESTIMATED means estimates, and UNKNOWN carries no
counts (including no invented zero). Missing counts are not zero. Cached/reasoning
subcounts may overlap input/output totals, so consumers must not blindly sum them.
Event IDs enable future deduplication; no collector or aggregation exists yet. In v1
task and packet identify the same work unit: if both context IDs appear, they must
agree. Nested usage observations must agree with their event envelope.

`ExperimentSpec` references inputs/source state, command, metrics/outputs, and named
decision boundaries: process exit, crash, no progress, NaN metric, epoch completion,
or a configured metric threshold. `ExperimentEvent` references the reached boundary
and carries observations/evidence. Thresholds and metric values must be finite JSON
numbers; NaN is an explicit boundary, not a JSON number. Boundary-to-spec resolution,
job detachment, decisions, retries, and execution are future responsibilities.

## Configuration hierarchy

| Scope | Default path | Contents |
| --- | --- | --- |
| Machine configuration | `~/.config/agentctl/` | Stable cross-repository workflow policy |
| Durable machine-local data | `~/.local/share/agentctl/` | Future canonical graph/memory/task/job/evidence/resume state |
| Reconstructible cache | `~/.cache/agentctl/` | Disposable derived data; never the only copy of canonical state |
| Project configuration | `repo/.agentctl/project.toml` | Product invariants, architecture constraints, repo commands, protected-data rules, canonical verification definitions |
| Task packet | Managed protocol document | Current bounded work delta and references |

The machine paths follow `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME` when
set to absolute paths, using the defaults above otherwise. Project policy specializes
repository facts within machine policy; a task cannot weaken either policy. Exact
config syntax, merge behavior, initialization, and migration are deferred. Stage 0
creates none of these runtime directories and has no config engine.

## Wire compatibility and implementation boundary

Every public packet/event document carries `"version": "1"`; nested supporting values
inherit their enclosing version. Enums use SCREAMING_SNAKE_CASE and fields snake_case.
IDs are distinct Rust newtypes serialized as 1–128 ASCII characters matching
`[A-Za-z0-9][A-Za-z0-9._:-]*`. IDs are caller-issued; no UUID dependency is necessary.
Optional fields accept omission or null and serialize as null when absent.

Unknown fields, enum variants, and versions fail closed. Incompatible fields or
semantics require a new wire version and deliberate migration; the crate version is
separate. Do not rely on consumers ignoring new fields. Rust definitions generate
self-contained Draft 2020-12 schemas; the lockfile pins generator dependencies and
tests compare exact generated bytes with `schemas/`. JSON Schema validates structure;
`Validate` adds DAG, nonblank-content, reference consistency, and other semantic checks.
Consumers must run both deserialization and semantic validation before accepting data.

One crate contains protocol definitions, validation, pure lifecycle guards, schema
generation, and CLI dispatch. There is no runtime, database, graph implementation,
provider adapter, network stack, daemon, TUI, ML runner, or agent loop. Future stages
must preserve these ownership and verification boundaries as those systems are added.
