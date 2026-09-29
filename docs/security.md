# Security model

This document describes what agentctl `0.3.0-alpha` guarantees, what it only
mitigates, and what it does not attempt. It is a **bootstrap** security model
for a local, single-user tool in alpha. It is written to be precise rather than
reassuring: where a guarantee depends on a platform or on another program, that
dependency is stated.

- [Scope and threat model](#scope-and-threat-model)
- [Trust boundaries](#trust-boundaries)
- [Canonical state](#canonical-state)
- [Authority of each role](#authority-of-each-role)
- [Mutation ownership and accepted-source integrity](#mutation-ownership-and-accepted-source-integrity)
- [Working-tree drift protection](#working-tree-drift-protection)
- [Candidate isolation and scope checking](#candidate-isolation-and-scope-checking)
- [Environment authorization](#environment-authorization)
- [Process lifecycle](#process-lifecycle)
- [Cancellation, timeout and signals](#cancellation-timeout-and-signals)
- [Recovery](#recovery)
- [Platform behavior](#platform-behavior)
- [Capabilities agentctl does not provide](#capabilities-agentctl-does-not-provide)
- [Current limitations](#current-limitations)

## Scope and threat model

agentctl coordinates AI agents working on a repository its user controls, on
the user's own machine, as the user. It is designed so that:

- agent output, however wrong, cannot become accepted source without passing
  agentctl's own observation, an independent verifier and the acceptance
  transaction;
- agents cannot change human intent, history, ownership or accepted source by
  what they *say*;
- a human's uncommitted work is never overwritten or claimed by a task;
- agentctl's records never overstate what it knows, including how strongly a
  provider's processes were ended.

It is **not** designed to contain a hostile agent or a hostile repository.
Providers run as the user, with the user's filesystem and network access,
confined only by their own editing modes or sandboxes. Verifiers run the
repository's builds and tests. Do not point agentctl at a repository or a
provider you would not run yourself.

## Trust boundaries

| Party | Trusted for | Not trusted for |
| --- | --- | --- |
| The human (CLI user) | Intent, constraints, completion criteria, decisions on concerns, reconciling the working tree | — |
| agentctl and its store (`.agentctl/`) | Canonical state, authority, execution truth | — |
| procd | Lifecycle domains and termination evidence | Anything else |
| Providers and models | Nothing. Every result is untrusted input | Results, reports, claims, prose, exit behavior |
| Repository content | Being the subject of work | Its code is executed by verifiers; nothing in it is interpreted as instructions to agentctl |
| Git | Listing repository content and ignore rules | Accepted state (agentctl keeps its own) |

Everything a provider returns is validated before use: the result must satisfy
the JSON Schema of its role (checked by agentctl itself, whatever the provider
claims to enforce), then the role's own protocol bounds, then the canonical
state it would change. Provider-controlled text is never written to the store,
and every string that can originate outside agentctl is escaped before it is
printed to a terminal.

## Canonical state

Canonical state lives only in `.agentctl/state.db` and the content-addressed
recovery objects in `.agentctl/objects/`.

- Every write is one SQLite `IMMEDIATE` transaction together with the event
  describing it. The schema enforces structure and lifecycle rules with
  constraints and triggers beneath the Rust code: for example, only a passed
  integration verification can complete a plan, a human decision cannot
  change, capacity cannot be exceeded, and a generation that executed can be
  accepted only through the acceptance phases.
- Recovery objects are named by the SHA-256 of their bytes, published only
  once complete, never replaced, and verified against their name when read.
  State refers to an object only once it is durable.
- A store whose schema is not exactly the current one is refused and left
  untouched.

**Limit:** `.agentctl/` is ordinary files owned by the user. It is outside
every agent's working directory, but a provider process running as the user
could still write to it by absolute path; only the provider's own confinement
prevents that. agentctl does not detect tampering with its own store.

## Authority of each role

### Planner

- Runs in the project root in **read-only** workspace mode (enforced by the
  provider, not by agentctl).
- Can only *propose* commands. agentctl validates the whole proposal and
  applies it atomically or not at all.
- Can never change human intent, accepted source, recorded history, ownership,
  runtime records or decisions.
- Task scopes must be canonical literal paths inside the configured source
  roots, never `agentctl.toml`, `.agentctl/` or Git state.
- Can only authorize a retry of a task's current definition, once; cannot run
  anything itself.
- Can raise a concern; cannot decide one. Can propose completion; cannot
  complete.

### Executor

- One fresh invocation per generation, in **editable** mode, in a disposable
  copy of the repository outside the project, with no `.git` and no
  `.agentctl`.
- Authorized to change exactly its task's scope (`mutable_paths`), which its
  generation owns exclusively.
- Its report (status, summary, claimed paths) is kept as a claim. agentctl
  decides what it changed by observing the workspace itself.

### Verifiers

- A task verifier is a fresh agent for one verification; it is never the
  executor, never resumes its session, and is given nothing the executor said.
- It judges a view built from accepted content (recovery objects) plus the
  candidate's exact bytes, so another pipeline's unaccepted work in the shared
  working tree cannot influence it.
- It runs in **disposable** mode and may build and test. Changing repository
  source in its view is a `boundary_violated` outcome, never a pass.
- A pass needs a `pass` verdict with at least one passed check **and**
  agentctl's own observations (the candidate still installed, source
  untouched). A pass is evidence; only acceptance accepts.
- The integration verifier is likewise fresh and independent, judging the
  plan's accepted result assembled from recovery objects plus the exact
  repository inputs recorded as its basis. Only its pass completes a plan.

**Limit:** agentctl does not observe which commands a verifier ran or how
they exited; the verifier's checks are its claims, kept apart from what
agentctl observed.

## Mutation ownership and accepted-source integrity

- Each project path is owned by at most one generation, across all plans,
  enforced by the schema. Acquisition is all-or-nothing and only within the
  task's authorized scope.
- Ownership is granted only while the working tree holds that path's accepted
  state.
- Accepted source changes only through the acceptance transaction, and only to
  the identities the execution captured, never to bytes read at acceptance.
  Publication happens only if, within that same transaction, the working tree
  is observed to hold the candidate at every changed path.
- Ownership is released only by a completed acceptance or by a replan that
  abandons a stopped generation (after restoring its candidate).

## Working-tree drift protection

agentctl maintains:

```text
accepted bytes  =  executor starting bytes  =  restoration target
```

- A task is claimed only if every path of its scope holds exactly its accepted
  state (checked in the claim transaction).
- An executor refuses to start unless its observed baseline equals accepted
  state at every scope path.
- A candidate is installed only while every changed path still holds the
  baseline; otherwise nothing is written (`drifted`).
- Abandoning a stopped attempt restores its candidate to the last accepted
  state only where each path still holds exactly the candidate; if anything
  else is there, nothing is written and the replan is refused.
- Recovery of an interrupted install never overwrites a path that holds
  anything other than the candidate or its prior content.

Effects: human work in progress in a task's scope is never overwritten, never
used as a starting point and never accepted under a task's name; the task waits
and `run` names the paths. A write that did not arrive through a verified
candidate, such as a provider writing into the project by absolute path, is
not laundered into accepted source through a later task; it shows up as drift
for a human to resolve.

**Limit:** observation and publication are serialized with other agentctl
writers, but not with arbitrary processes. A process that changes a file
between agentctl's observation and its commit can still race it; what is
published is then still the verified candidate, and the working tree has
drifted from accepted source, which later claims detect.

## Candidate isolation and scope checking

- The executor's workspace is a copy of what Git lists as repository content
  (tracked, or untracked and not ignored). It holds no `.git`, no `.agentctl`,
  nothing ignored, and no symlink whose target leaves the copy. Nothing links it
  to the project.
- After the executor ends, agentctl observes the workspace (with the project's
  own ignore rules, not rules the executor could rewrite in the copy; paths in
  `.agentctl` or `.git` inside the copy are always observed) and derives the
  changes against the baseline. Any change outside the generation's authority
  makes the outcome `scope_violated`; a workspace that keeps changing after the
  invocation ended is `unattributable`. Neither is ever installed.
- Only a candidate reaches the project, and only through a journaled install
  whose original and new bytes are both preserved as recovery objects first.
- Paths are literal everywhere. Names such as `[slug]`, `[...slug]`,
  `(group)`, `@slot`, `a+b`, names with spaces, and Unicode names are
  ordinary names; Git is invoked with literal pathspecs. On Windows, names the
  filesystem would reinterpret (device names, trailing dots, 8.3 aliases,
  wildcard or stream characters) are refused.
- Symlinks are never followed when reading or writing source.

**Limit:** agentctl observes the workspace, not who wrote to it. Ignored paths
in the workspace are not observed. A candidate is "structurally valid work
within authority", not proof of provenance.

## Environment authorization

Provider processes do not inherit agentctl's environment. The shim clears the
environment and passes only:

- a fixed common set (paths, home, user, locale, time zone, XDG directories,
  proxies, certificate locations, and Windows system variables);
- the adapter's own set (`claude`: `CLAUDE_CONFIG_DIR`,
  `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_*`, `CLAUDE_CODE_USE_*`; `codex`:
  `CODEX_HOME`, `OPENAI_*`; `generic`: none);
- the exact names listed in the provider's `env` configuration.

The full list is in [configuration.md](configuration.md#environment). Provider
executables are started directly, never through a shell; on Windows `.bat` and
`.cmd` are refused. Git is run with `GIT_DIR`, `GIT_WORK_TREE`,
`GIT_INDEX_FILE` and pathspec-mode variables removed.

## Process lifecycle

agentctl does not implement process containment. It delegates it to
[procd](https://github.com/integraltechnologies/procd), which creates a
**lifecycle domain** for every invocation, places the first process (the
`agentctl-shim`) in it before it runs, tracks descendants, terminates the domain
by its own authority, and reports evidence.

procd reports each capability at one of three levels. agentctl uses them
exactly as reported, never raising them:

| Level | Meaning |
| --- | --- |
| **ENFORCED** | Strong backend enforcement. For process-tree termination: procd can prove, by authority, that no process of the domain remains. |
| **BEST_EFFORT** | A usable, weaker guarantee. For process-tree termination: procd tracks the domain's processes and kills every one it tracked, and a final scan finds none. A process that escaped its tracking is not excluded. It is **not** proof of emptiness, and it is **not** unsupported. |
| **UNSUPPORTED** | The operation cannot safely be attempted. |

agentctl's roles require process-tree termination at least BEST_EFFORT. Where
procd reports it UNSUPPORTED, or procd's capabilities cannot be established,
agentctl launches nothing: `this host cannot run agents`. Each domain is also
checked when created; a domain procd could establish only UNSUPPORTED
termination for is refused before anything is recorded or run.

While the agentctl process that launched an invocation is alive, that process
holds the domain. However the invocation ends (normal completion, failure,
explicit cancellation, timeout, or SIGINT/SIGTERM/SIGHUP to agentctl), agentctl
terminates the whole domain through procd, then records the end **only if**
termination established it, and records how strongly:

| Recorded `termination` | Requires |
| --- | --- |
| `enforced` | procd proved the domain empty: admission closed, authority-directed termination, emptiness proven, at an enforced level, final state empty. |
| `best_effort` | The domain was created at the BEST_EFFORT level, and procd's termination succeeded there: admission closed, every tracked process killed, none found after. |

The strength is durable provenance: it is stored on the invocation record
(`invocations.termination`) and appears in the `invocation.ended` event
(`agentctl logs --kind invocation.ended`). A `best_effort` end is never
recorded as `enforced`. If termination establishes less than its level allows
(for example a termination that fails or times out), nothing is recorded and
the invocation stays **unresolved**, as do the actions waiting on it.

Unresolved invocations block the conflicting work: nothing new starts on their
plan, and the paths their generation owns stay owned.

## Cancellation, timeout and signals

| Trigger | What happens |
| --- | --- |
| `agentctl plan cancel <plan>` | A durable request is recorded (the plan is paused). The agentctl process holding each covered invocation sees it within about 250 ms and terminates the domain. `plan cancel` waits up to 60 s and exits non-zero unless every covered invocation is proven ended. |
| Invocation timeout | Past `agents.invocation_timeout_minutes` (default 120), the domain is terminated as for a cancellation. |
| SIGINT, SIGTERM, SIGHUP to `run`, `plan create`, `plan update`, `plan verify` (Ctrl-C, Ctrl-Break, console close on Windows) | agentctl launches nothing more, terminates every live domain it holds, records each end, and exits with an error. |
| SIGKILL, crash, power loss | agentctl cannot act. Its invocations are left without an end, for [recovery](#recovery). |

A cancelled invocation is recorded `cancelled`, with a diagnostic stating why
(cancelled, at the human's request, interrupted, or timed out) and the
termination strength. Cancelling never releases ownership, restores files, or
marks work failed beyond what was established; the planner decides what
follows.

## Recovery

`agentctl recover` settles records left by agentctl processes that ended.
Session liveness is established by exclusive lock files the operating system
releases when a process dies, never by process ids.

An invocation left without an end is settled only by procd's authoritative
proof that its recorded domain is gone: procd either proves the exact domain
was destroyed, or reacquires it, terminates it and proves it empty. That
requires **ENFORCED** process-tree termination on the host and procd's ability
to recover the domain (**SafeRecovery**). Anything less (a backend that tracks
only best effort, a recovery procd refuses or cannot resolve, a missing
identity) leaves the invocation unresolved, reported `unsupported`, and
`recover` exits non-zero. Nothing else, not process ids, ancestry, elapsed time
or a provider's own state, is ever taken as proof.

SafeRecovery availability depends on the host and on the authority agentctl
runs with:

- **Linux, root**: procd can reacquire an orphaned domain, terminate it and
  prove it empty. Recovery settles the invocation and the plan continues.
- **Linux, ordinary user** (in a delegated cgroup): procd's recovery is
  root-only, so it refuses. The invocation stays unresolved.
- **macOS**: process-tree termination is BEST_EFFORT, so nothing procd reports
  after authority was lost is proof. The invocation stays unresolved.
- **Windows**: procd's current recovery does not resolve an orphaned domain.
  The invocation stays unresolved.

When recovery cannot settle an invocation, the plan fails closed:

```text
agentctl process killed outright (SIGKILL, crash)
  ↓
recovery cannot establish the old domain authoritatively
  ↓
the invocation stays unresolved; `recover` reports it and exits non-zero
  ↓
no new work starts on that plan; its generation keeps its paths,
so no other plan can take conflicting mutation authority either
```

On a host where recovery cannot settle it, the orphaned provider processes may
still be running. agentctl no longer has authority over them, and v0.3 has no
command that settles such an invocation; end them yourself. This is a current
bootstrap limitation. Other plans are unaffected unless they need the paths
the stuck generation owns.

Everything else recovery does is decided from canonical state and the working
tree alone: attempted actions whose process died are recorded `interrupted`
(judging and accepting nothing), never-attempted actions are withdrawn,
published acceptances are finished as recorded, interrupted installs are
completed or undone without overwriting anything unexpected, and claims are
released once their outcome is established. Recovery never retries, judges,
accepts or completes.

## Platform behavior

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| procd mechanism | cgroup v2 | tracking of the domain's processes | Job Objects |
| Process-tree termination for agentctl's roles | ENFORCED when procd's prerequisite holds: root, or an ordinary user that owns a delegated cgroup v2 subtree (for example a systemd unit with `Delegate=yes`). Otherwise agentctl runs only at whatever level procd reports, and not at all if UNSUPPORTED. | BEST_EFFORT | As procd reports |
| Recorded `termination` for normal completion, cancel, timeout, SIGINT/SIGTERM/SIGHUP | `enforced` | `best_effort` | as established |
| After agentctl is killed outright | Root: recovered. Ordinary user: unresolved, fails closed. | Unresolved, fails closed | Unresolved, fails closed |
| Whole-system qualification for 0.3.0-alpha | Yes (ordinary user in a delegated cgroup) | Yes | No: builds and runs its test suites in CI; the command-line qualification suite is Unix-only |

Linux note: in a terminal, a Ctrl-C reaches the shim and provider directly as
well as agentctl, since they share agentctl's process group there. agentctl
still terminates the domain and records the end at `enforced` strength, but the
invocation may be recorded `interrupted` (its end was not reported by the shim)
rather than `cancelled`.

## Capabilities agentctl does not provide

agentctl reports these as UNSUPPORTED (not implemented) on every platform:

- **Filesystem isolation**: providers can read and write whatever the user can.
  Workspace modes are enforced, if at all, by the provider's own permission
  mode or sandbox (Claude Code's permission modes, Codex's sandbox, a generic
  runtime's own implementation).
- **Network isolation**: providers and the builds and tests verifiers run have
  the user's network access.
- **Resource limits**: no CPU, memory or process-count bounds.
- **Confined path resolution** is only BEST_EFFORT: ancestors are checked, then
  used by path, so a concurrent process swapping a directory for a symlink
  between the two is not excluded.

## Current limitations

- Hardening against a malicious repository or a malicious provider is out of
  scope for this release (see [Scope and threat model](#scope-and-threat-model)).
- On macOS, lifecycle guarantees are BEST_EFFORT: a descendant that escaped
  procd's tracking is not excluded from a `best_effort` end.
- If agentctl is killed outright while an invocation is live, on any host
  without SafeRecovery (macOS, Windows, non-root Linux) the plan stays blocked
  and orphaned provider processes may survive; there is no supported command
  to clear it in v0.3.
- agentctl's own store is protected from providers only by provider
  confinement, not by agentctl.
- Verifier checks are the verifier's claims; agentctl does not observe the
  commands it ran.
- agentctl never commits or pushes. Review accepted work and commit it
  yourself.
