# Security model

agentctl launches untrusted work: provider CLIs driven by model output, the
programs those CLIs run, canonical verification checks, and experiment processes.
This document describes what agentctl enforces around those processes, how it
decides whether a launch is allowed, and where the guarantees stop.

It describes the current alpha implementation. Nothing here is a claim of perfect
isolation.

## Threat model

**Untrusted:** worker output and every process a worker causes to run. That
includes provider frontends (Claude Code, Codex), their tool subprocesses,
verification checks, and experiment programs.

**Trusted:** the agentctl binary, the installed provider CLIs you configure, the
programs named in your project's `[commands]`, the operating system, and the
operator's user account.

**Out of scope:** same-user host compromise. Another process running as your
account can edit the workspace, the agentctl database, or its configuration
directly. agentctl's SQLite guards and journal triggers protect against accidental
or out-of-band lifecycle changes through ordinary connections, not against a
machine owner who can alter the schema or replace the database.

The goals are:

- a worker cannot modify Git metadata, agentctl state/configuration, or files
  outside the paths its role is granted;
- a worker cannot read agentctl state, well-known credential stores, or files
  outside its granted read roots;
- a worker cannot silently widen the context it was issued: additional context
  comes only from a typed, bounded, auditable request that agentctl resolves
  inside the planner's envelope (see
  [issued-context visibility](#issued-context-visibility));
- a worker does not inherit the controller's ambient environment;
- checks and experiments cannot reach provider credentials, and are offline unless
  explicitly allowed;
- a worker's processes are terminated when the job ends, and a cleanup that cannot
  be proven is reported rather than assumed;
- when the host cannot enforce what a job requires, the job is **refused**. There is
  no unsandboxed fallback.

## How a launch is checked

Every worker launch goes through a single path:

```text
ProcessSpec ──compile──▶ SecurityPolicy ──check(capabilities)──▶ platform backend spawn
                                           │
                                           └─ a required capability is not met
                                              → refused before launch
```

1. Orchestration code (runtime, checks, experiments) describes the launch as an
   OS-neutral `ProcessSpec`: workspace, scratch directory, whether the workspace is
   writable, whether network is allowed, the worker class, protected paths, and
   resource limits.
2. `security::compile` turns it into a `SecurityPolicy`: exact read/write roots,
   always-denied paths, the complete worker environment, and resource ceilings.
3. The active platform backend reports which capabilities it enforces. The policy
   lists which capabilities it needs, and at what minimum level. If any requirement
   is unmet, or agentctl is running as root/administrator, the launch fails with
   `SECURITY_CAPABILITY_UNSUPPORTED`.
4. Only then does the backend spawn the process.

No planning, routing, verification, analytics, or experiment code calls a platform
sandbox API directly.

## Worker classes

| Class | Used for | Network | Provider authentication |
| --- | --- | --- | --- |
| Provider frontend | planner, executor, verifier and integration-verifier jobs (the Claude/Codex CLI process) | allowed by default; removed by role or project policy | native login files or an explicitly configured API-key variable |
| Tool | verification checks and experiments | checks: always denied; experiments: denied unless `--network` is given and the project does not set `deny_network` | never |

Write access by role:

| Process | Workspace | Private scratch |
| --- | --- | --- |
| Executor | writable, unless its profile is `read_only` | writable |
| Planner, verifier, integration verifier | read-only | writable |
| Verification check | read-only | writable (build output goes here; `CARGO_TARGET_DIR` points into scratch) |
| Experiment | writable | writable |

## Capabilities

Each backend reports every capability as `ENFORCED`, `BEST_EFFORT`, or
`UNSUPPORTED`. `agentctl security doctor` prints the report for the current host.

| Capability | Meaning |
| --- | --- |
| `FILESYSTEM_READ` | file contents outside granted roots are unreadable |
| `FILESYSTEM_WRITE` | writes outside granted roots are denied |
| `FILESYSTEM_METADATA_WRITE` | chmod/chown/utimes/xattr changes are confined |
| `NETWORK_DENY` | network access can be removed, including localhost |
| `PROCESS_TREE` | all of a job's processes are found and terminated |
| `ENVIRONMENT_ISOLATION` | the worker receives only an allowlisted environment |
| `CREDENTIAL_ISOLATION` | tool workers cannot reach provider or host credentials |
| `MEMORY_LIMIT`, `CPU_LIMIT`, `PROCESS_LIMIT`, `OPEN_FILE_LIMIT`, `FILE_SIZE_LIMIT` | resource ceilings |
| `WALL_CLOCK` | timeouts (enforced by the controller on every platform) |
| `OUTPUT_CAPTURE` | stdout/stderr are each bounded to 4 MiB; overflow fails the job |

What a launch requires:

| Requirement | Minimum |
| --- | --- |
| Filesystem read, filesystem write, environment isolation, wall clock, output capture | `ENFORCED` for every launch |
| Process tree | `BEST_EFFORT` for every launch |
| Network deny | `ENFORCED` when the job has no network |
| Credential isolation | `ENFORCED` for tool workers |
| Each configured resource limit | `BEST_EFFORT`, only when `[runtime.security.resources] strict = true` |

Resource limits that the host cannot enforce are otherwise applied where possible
and reported, but they do not block launches.

## Platform backends

| Platform | Backend | Worker execution |
| --- | --- | --- |
| macOS | `macos-seatbelt`: a generated Seatbelt profile run through `/usr/bin/sandbox-exec` | supported |
| Linux | `linux-landlock-seccomp`: Landlock + seccomp-BPF + `no_new_privs` + rlimits, with no helper binary or user namespaces | supported when Landlock (ABI ≥ 1, kernel 5.13+, enabled in the LSM list) and seccomp are available for the CPU architecture |
| Windows | `windows-job-objects` | **refused**: filesystem and network confinement are not implemented |
| Other | `unsupported` | refused |

### macOS

- File contents and directory listings are deny-by-default. Only the OS and
  toolchain roots (`/usr`, `/bin`, `/sbin`, `/System`, `/Library`, `/opt`, `/nix`,
  Xcode, the platform temp directory, and a few system files), the workspace,
  scratch, the repository's Git directories, the program's own install
  directory, and operator-granted `read_roots` are readable. The platform temp
  directory is read-only and a platform root like `/usr`: provider CLIs use it
  directly regardless of `TMPDIR`, and the Claude Code CLI will not start
  without it. Nothing in agentctl state, the workspace under issued visibility,
  Git metadata or a credential store becomes reachable through it — those
  denials are compiled after every grant and win.
- `stat` metadata stays readable so path resolution works. This can reveal whether
  a file exists and how large it is, but not what it contains.
- Writes are deny-by-default. Network denial includes localhost and Unix-domain
  sockets.
- Tool workers also lose access to Keychain mach services. Provider frontends that
  use native login can read `~/Library/Keychains` read-only, because the macOS
  security daemon checks the client's sandbox.
- `MEMORY_LIMIT` is `UNSUPPORTED`: XNU does not enforce `RLIMIT_AS`/`RLIMIT_DATA`.
  CPU, process, open-file, and file-size limits are per-process or per-user
  rlimits (`BEST_EFFORT`).
- Nested Seatbelt does not work. Worker launches fail if agentctl itself is
  running inside another Seatbelt sandbox.

### Linux

- Landlock confines reads and writes to explicit allow rules. Landlock can only
  allow, so a denied path inside a granted root (such as `.git` inside a writable
  workspace) is carved out by granting the root's other entries individually.
  **Consequence:** an executor cannot create or remove entries directly in the
  workspace root. Everything below ordinary subdirectories is unaffected.
- seccomp denies `socket(2)` for every address family when network is denied
  (`socketpair` remains available for local pipes). It also denies io_uring and
  kernel keyring access, and denies `truncate(2)` on Landlock ABIs that lack
  truncate control.
- `FILESYSTEM_METADATA_WRITE` is `UNSUPPORTED`: Landlock does not mediate
  chmod/chown/utimes/xattr. File contents stay protected.
- Memory, CPU, process, open-file, and file-size limits are rlimits (`BEST_EFFORT`,
  per process or per user, not aggregated across a job).
- There is no cgroup or PID namespace containment (see [process trees](#process-trees)).

### Windows

The Windows backend implements Job Objects (the whole process tree is owned by
the job and killed with it, and processes start suspended), job memory and
active-process limits, the allowlisted environment with isolated
`TEMP`/`TMP`/`USERPROFILE`, and refusal to run elevated.

Job Objects provide no filesystem or network confinement, and AppContainer is not
implemented. `FILESYSTEM_READ`, `FILESYSTEM_WRITE`, `NETWORK_DENY`, and
`CREDENTIAL_ISOLATION` are therefore reported as `UNSUPPORTED`. Because
filesystem confinement is required for every worker, **every worker launch is
refused on Windows**. This is intentional fail-closed behavior.

## Filesystem policy

Always readable: the platform roots listed above, the workspace, the private
per-job scratch directory, the repository's Git directories, the executable's
install directory (plus one level of `#!` interpreter), and any machine
`read_roots`. Nothing under your home directory is readable by default.

Always writable: the private scratch directory and a few device files (such as
`/dev/null`, and `/dev/shm` on Linux). The workspace is writable only as shown in
[Worker classes](#worker-classes).

Always denied, even inside a granted root:

| Path | Denial |
| --- | --- |
| agentctl data, configuration, and cache directories | read, `stat`, and write (except the job's own scratch) |
| `.git` (and the resolved Git directories), `.agentctl`, `.codex`, `.claude` in the workspace | write |
| `~/.codex`, `~/.claude`, and the configured provider home | read and write, except the specific native authentication/configuration files a provider frontend needs |
| `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.azure`, `~/.kube`, `~/.docker`, `~/.config/gcloud`, `~/.config/gh`, Git credential files, `~/.netrc`, `~/.npmrc`, `~/.pypirc`, Cargo credentials, `~/.password-store`, `~/.local/share/keyrings`, `~/Library/Keychains` | read and write (the Keychain exception above applies only to native-login provider frontends on macOS) |
| project `[[protected]]` paths | write, and read when `deny_read = true` |

agentctl's machine state must live outside the workspace. A launch is refused if
the workspace or scratch directory falls inside a denied path.

`PATH` for workers is the controller's `PATH` with relative and empty entries
removed, and with any entry under the workspace or agentctl state removed, so a
repository cannot shadow `cargo` or `git`.

## Issued-context visibility

A task's `read_scope` is an **authorization envelope**: the paths the task may
request context from. The **issued context** is what a job was actually given,
recorded in its context manifest. The two are deliberately distinct, and a
worker cannot widen its own issued context: it asks, and agentctl resolves the
request deterministically inside the envelope or blocks for a planner decision
(see [architecture.md](architecture.md#context-relay)).

`[runtime.context] visibility` chooses how strongly the *filesystem* enforces
that distinction:

| Mode | Repository reads of an executor/verifier job |
| --- | --- |
| `workspace` (default) | the whole workspace, as before |
| `issued` | only the repository files issued to that job in full, plus (for a writable executor) its write scope |

Under `issued`, the workspace tree and the repository's Git directories are not
read roots, so `.git` cannot be used to recover unissued source, and the
executor's write roots are its planner-authored write scope rather than the
whole workspace. The workspace is additionally *denied*, excepting only the
issued files, the write scope and the exact directories on the path to them, so
a broader read root — an operator `read_roots` entry, or the platform temp
directory when the checkout lives under it — cannot silently restore
workspace-wide access. Those directories stay listable because a process must
be able to resolve its own working directory; their contents do not become
readable. Checks and experiments are unaffected: they keep the read-only
workspace access they need. agentctl state, credentials and the control plane
stay denied exactly as before.

**The default is `workspace`, and `issued` is opt-in.** Before the default can
flip, the following must be settled:

- **Real-provider validation.** Both Claude Code and Codex must be shown to work
  through the relay under confinement — including whatever they read at startup
  in the working directory and in `.git`. This has not been measured with real
  provider processes yet, and a provider that needs an unissued path would fail
  closed rather than degrade.
- **Planner jobs.** `issued` applies to executor and verifier jobs. Planner
  jobs still run with workspace visibility, because the planner is the authority
  that decides what to issue and its packet carries excerpts rather than whole
  files.
- **Write scopes are readable.** The backends grant read access to write roots,
  so a Directory write scope is readable under `issued`. Strict read
  confinement needs File write scopes.
- **Linux write targets.** Landlock rules need an existing path, so a write
  target that does not exist yet cannot be granted on Linux; such a task must
  create files under a granted directory instead.
- **Atomic replacement.** Editors replace a file by writing `<file>.tmp.*`
  beside it and renaming it into place. On macOS, an authorized File write
  target also admits exactly its `<file>.<suffix>` siblings (create, write,
  rename onto the target, remove); every other sibling stays unwritable.
  Landlock cannot match names, so on Linux the grant stays exact-file and such
  an edit is refused.

## Environment isolation

Workers never inherit the controller's environment. agentctl builds the complete
environment itself:

- `PATH` (sanitized), `HOME` (a scratch home), `TMPDIR`/`TMP`/`TEMP`,
  `XDG_*_HOME`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR`, all pointing into scratch;
- `CARGO_TARGET_DIR` in scratch, `PYTHONDONTWRITEBYTECODE=1`,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` set to the null device,
  `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, and `LANG`;
- a per-launch `AGENTCTL_JOB_MARKER`, used for process-tree tracking;
- `AGENTCTL_EVENT_FILE` for experiments;
- machine-configured `[runtime.security] env` values and `inherit_env` names.

Machine `inherit_env`/`env` entries are validated. Loader-injection variables
(`LD_*`, `DYLD_*`), names agentctl reserves (the list above and `AGENTCTL_*`), and
credential-looking names (containing `TOKEN`, `SECRET`, `PASSWORD`, `API_KEY`,
`AUTH`, `SESSION`, and so on, or prefixed with `AWS_`, `GITHUB_`, `OPENAI_`,
`ANTHROPIC_`, `SSH_`, and similar) are rejected. Ambient worker environment
cannot carry credentials.

An experiment may pass named variables with `--env NAME`. This is an explicit,
per-experiment operator authorization, limited to 32 names, and it still refuses
loader-injection and reserved names. Passed values are redacted from captured
output.

Provider frontends additionally receive only what their native login needs: the
real `HOME`, `USER`/`LOGNAME`, and the provider home location. If API-key
authentication is configured, they receive the named key under the provider's
expected variable. See [providers.md](providers.md).

## Credentials

- agentctl never opens, parses, copies, or stores provider credential files.
  Provider frontends read their own native login files in place, inside the
  sandbox.
- Tool workers never receive native authentication or API keys.
- Configured API-key values and recognizable provider-token strings are scrubbed
  from captured output before it is persisted. This is defense in depth, not a
  general detector for deliberately encoded secrets.
- Provider frontends and their own tool subprocesses share one sandbox, so they
  can read that frontend's authentication files. Workers are instructed never to
  inspect credentials, but the installed provider binary remains trusted.

## Process trees

Each job's processes are identified three ways:

1. the job's own process group, killed as a group;
2. the random per-launch `AGENTCTL_JOB_MARKER` environment variable, which
   `setsid`/double-fork daemons normally keep;
3. an inherited sentinel pipe, which reaches end-of-file only when every process
   holding it has exited.

After the direct child exits, is cancelled, or times out, agentctl kills the
group, searches for marker or sentinel holders outside it (Linux: `/proc`; macOS:
libproc descriptor scans), kills them, and checks the sentinel. If a holder cannot
be identified, the outcome is reported as unproven. A child's exit is never taken
as proof that its tree is gone.

**Residual risk (macOS and Linux):** a daemon that closes every inherited
descriptor (and, on Linux, also clears its environment) leaves no trace that
agentctl can detect without cgroups or PID namespaces. On Windows the Job Object
owns the entire tree.

Other containment:

- the controller enforces role, check, and experiment timeouts and terminates the
  whole tree;
- stdout and stderr are each capped at 4 MiB;
- core dumps are disabled (`RLIMIT_CORE = 0`) because they could contain
  credentials;
- a workspace lock excludes other controllers from the same workspace and is
  inherited by the job's processes;
- default ceilings are 2048 additional processes and 8192 open files. Memory, CPU
  time, and file size are unlimited unless the operator sets them.

## Git hardening

agentctl's own Git subprocesses (repository discovery and source capture) drop
every inherited `GIT_*` variable. They disable hooks, fsmonitor, and global
attributes, and override every clean/smudge/process filter and diff/merge driver
from any configuration scope to a no-op. A repository therefore cannot make
`git status` run a program. agentctl refuses to run Git operations as root against
a repository owned by another user.

## Machine policy versus project policy

The machine configuration (`~/.config/agentctl/config.toml`) is the operator's
authority. The project configuration (`.agentctl/project.toml`) is repository
content and can be written by anyone who can change the repository, so it can
only **tighten** machine authority:

| Setting | Machine | Project |
| --- | --- | --- |
| Read roots, worker environment | defines | no field exists |
| Resource ceilings, experiment event volume | defines | may only lower (`[security]`) |
| Concurrent agent limit | defines `max_agents` | may only lower (`[routing] max_agents`) |
| Network for roles/experiments | per-role profile | may only remove (`deny_network`, profile `network = false`) |
| Workspace writes | executor profile | may only remove (`read_only`, profile `read_only = true`) |
| Context budget, role timeout | per-role profile | may only lower |
| Providers | defines available providers | may only restrict (`allowed_providers`); cannot add providers |
| Protected paths | none | adds denials |

Non-executor roles can never gain write access. Provider output and planner output
can never change routing, permissions, or limits. Explicit `--override` flags can
select a different configured provider or model, but they cannot bypass a project
allowlist or raise the concurrency ceiling. See
[configuration.md](configuration.md).

## Other untrusted-input handling

- Provider output and experiment event frames are parsed with a strict JSON parser
  that rejects duplicate object keys and limits nesting depth to 64.
- Human-readable CLI output neutralizes terminal control and bidirectional
  characters found in untrusted text (paths, Git refs, provider text, metric
  names). JSON output keeps the exact value.
- Plans, packets, and memory content are data. agentctl never executes embedded
  shell text. Checks run only as project-declared `program`/`args`/`cwd` argv,
  never through a shell.
- Configuration files are read without following symlinks and are limited to
  1 MiB. On Unix, new state directories are created `0700` and new files `0600`.

## Checking a host: `agentctl security doctor`

```bash
agentctl security doctor
agentctl security doctor --json
```

The command reports the backend, the per-capability status, the effective machine
security policy, and whether the data directory and database are owner-only. It
then runs a live self-test: it plants a secret the sandbox must not read and
checks that writes outside the workspace and into `.git` are denied. It makes no
model call and no network request, and prints no secret values.

It exits nonzero if baseline isolation is not enforced, if any hard requirement is
unsupported, if the self-test fails, or if agentctl is running as root. In those
cases every worker launch would be refused.

## Known limitations

- Windows cannot run workers. Other non-macOS, non-Linux platforms are refused.
- The macOS backend depends on `sandbox-exec`, which Apple marks as deprecated but
  still ships. Nested sandboxes fail.
- Readable paths are limited to granted roots. The workspace, toolchains, and the
  operator's extra `read_roots` are fully readable by every worker in that
  workspace. This is a confinement boundary for the rest of the host, not
  secrecy for the workspace itself.
- `stat` metadata of unreadable paths remains visible on macOS and Linux.
- On Linux, metadata writes are not mediated, and executors cannot create or
  remove entries directly in the workspace root.
- Process-tree cleanup is best effort on macOS and Linux (see above). There are no
  aggregate cgroup limits. Most resource limits are per process.
- Provider frontends need network access and share their sandbox with their own
  tool subprocesses.
- Source snapshots are careful sequential observations, not atomic filesystem
  snapshots. External edits between observations cannot be proven absent.
- Source snapshots do not look inside directories that the repository ignores
  as a whole, such as `target/` or `node_modules/` (see
  [cli.md](cli.md#running-plans)). An executor's writes inside such a directory
  are therefore neither scope violations nor part of the verified diff.
  Individually ignored files and every ignore-rule file are observed, so an
  executor cannot create a new ignored location unnoticed. `.git/info/exclude`,
  which lives outside the worktree, is recorded by content hash, so changing it
  fails closed as `SOURCE_DRIFT`. Ignored files are
  observed by metadata only: a change is detected by size, mode, identity, and
  change times rather than by content hash, and their content is never captured
  or placed in agent context. If checks consume an ignored directory, such as
  `node_modules/`, deny writes to it with `[[protected]]`.
- Local authorization is machine-local orchestration authority. It is not user
  authentication and does not protect against the machine owner.

## Native sandbox tests

The regular test suite does not require a working host sandbox. The native tests
are marked `#[ignore]` and must run on a capable macOS or Linux host, not as root
and not inside another sandbox:

```bash
cargo test --locked -- --ignored
```

They use fake executables and disposable repositories. They exercise secret and
state confinement, write confinement, environment isolation, network denial
(including localhost), resource limits, cancellation, and daemonized-descendant
detection. They make no model calls.
