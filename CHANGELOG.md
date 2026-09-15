# Changelog

All notable changes to this project are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
agentctl is alpha software; versions before `1.0.0` may include breaking changes
to storage, configuration, or the CLI.

## [Unreleased]

### Fixed

- Runtime source capture no longer reads, counts, or walks Git-ignored
  content. A large ignored build tree such as `target/` no longer exhausts the
  64 MiB workspace capture budget and blocks `agentctl run planner` (issue #3).
  Snapshots now observe the checkout the way Git walks it:
  - tracked files (even ones an ignore rule matches) and untracked, non-ignored
    files are captured by content, as before;
  - individually ignored files are observed by metadata only, so they cost no
    capture budget and never become agent context, but creating or changing
    one is still drift and a scope violation;
  - directories an ignore rule matches as a whole are never walked.

  Ignore rules come from the repository's `.gitignore` files and
  `.git/info/exclude`; `core.excludesFile` is not applied. The 64 MiB /
  20,000-file aggregate bounds are unchanged. `.git/info/exclude` lives outside
  the worktree, so the snapshot records a hash of it (the file Git resolves,
  shared by linked worktrees). Changing it fails closed as `SOURCE_DRIFT`
  rather than silently hiding a change.

### Changed

- Ignored symlinks, hardlinks, nested repositories, and read-denied paths no
  longer make a run refuse. They are still refused among captured source files.
- Executor writes inside ignored directories are no longer observed.
- At most 20,000 individually ignored files are observed per capture; beyond
  that, the run refuses.
- A run whose stored baseline was captured by 0.1.0-alpha.2 and included
  ignored files reports `SOURCE_DRIFT` and needs an explicit replan.

## [0.1.0-alpha.2]

### Fixed

- Fixed planner startup failures (`runtime file/artifact exceeds size limit`)
  caused by workspace files larger than an obsolete 2 MiB per-file capture
  ceiling. Workspace capture now relies solely on the existing bounded
  aggregate budget (64 MiB total, 20,000 files) rather than an unrelated
  per-file cliff.
- Runtime size-limit errors now identify the responsible subsystem (workspace
  capture, file count, git index, or artifact readback) along with the
  observed and limit values, instead of a generic message.

### Added

- Boundary and end-to-end regression coverage for planner execution and
  workspace capture size limits.

## [0.1.0-alpha.1]

First alpha release. Local, provider-agnostic engineering control plane for
coordinating coding agents, usable for small, well-scoped repositories.

### Added

- Planner → executor → verifier orchestration over a durable, resumable task DAG,
  with verified-only progression and a separate integration verification pass.
- Provider-independent local state: plans, tasks, jobs, diffs, evidence, and
  decisions stored and journaled in a local SQLite database.
- Repository intelligence: an incremental, content-hashed code graph for Rust,
  Python, TypeScript, and JavaScript (symbol lookup, ranked location, callers,
  tests, impact, bounded context packets).
- Structured engineering memory with explicit trust classes and provenance
  (`CANONICAL`, `DERIVED`, `OBSERVED`, `AGENT_NOTE`).
- Provider routing and fallback across configured roles, with project-level
  tightening of machine policy.
- A machine-wide concurrency ceiling (`max_agents`, default 4) across all
  simultaneously active agent jobs.
- Experiment supervision for long-running programs (training runs, benchmarks),
  with structured metric/checkpoint ingestion and deterministic decision
  boundaries that can open new planning requests.
- OS-level sandboxing and capability enforcement: Seatbelt on macOS, Landlock +
  seccomp on Linux, with fail-closed launch refusal when a host cannot enforce
  the required policy. The Windows backend is intentionally unsupported and
  fails closed.
- Observability via `agenttop` (terminal UI) and `agentctl observe`, and
  historical `agentctl analytics`.
- Claude Code and Codex CLI provider adapters.

### Known alpha limitations

- Only Claude Code and Codex CLI adapters exist; Codex token usage is not
  reported.
- Tasks within a workspace run one at a time; there are no parallel worktrees.
- Repositories are limited to 20,000 files and 64 MiB, with no symlinks,
  hardlinks, or submodules in the checkout. Verifier diffs are limited to
  128 KiB.
- The code graph is syntactic and single-file, resolving only a narrow set of
  references.
- Process-tree cleanup on macOS and Linux is best effort; resource limits are
  mostly per process.
- Windows cannot run workers; the backend fails closed.
- agentctl never commits or pushes; you review and commit results yourself.
- No artifact garbage collection yet.

See [README.md](README.md) and [docs/security.md](docs/security.md) for the full
capability list, security model, and platform support matrix.
