# Changelog

All notable changes to this project are documented in this file. The format is
loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
agentctl is alpha software; versions before `1.0.0` may include breaking changes
to storage, configuration, or the CLI.

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
