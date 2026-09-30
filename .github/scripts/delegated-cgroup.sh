#!/usr/bin/env bash
# Runs a command as the CURRENT (unprivileged) user inside a systemd
# transient service that owns a delegated cgroup v2 subtree.
#
# This is procd's documented Linux prerequisite: the caller's own cgroup and
# its cgroup.procs must be writable by the caller ("root, or an ordinary user
# that owns a delegated cgroup v2 subtree, as provided by a systemd unit with
# delegation"). A GitHub-hosted runner's job process is not in such a cgroup,
# so procd reports PREREQUISITE_MISSING there. sudo is used only to ask systemd
# for the unit; the workload itself never runs as root, so procd's root-only
# record store (and recovery) stays off exactly as for an ordinary user.
#
# Nothing is faked: the command sees the real kernel cgroup, and procd
# decides its own capability level there.
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

# systemd-run resolves a bare command name itself, against the PATH of the
# sudo-reset environment it is started in (no ~/.cargo/bin), and fails with
# "Failed to find executable". Resolve it here, in the caller's own PATH, so
# the already-installed toolchain is what runs. The service then gets the
# caller's PATH (below), so the rustup proxy finds rustc, linkers and helpers
# exactly as an ordinary step would.
command_path="$(command -v -- "$1" || true)"
if [ -z "$command_path" ]; then
  echo "delegated-cgroup: '$1' not found in PATH=$PATH" >&2
  exit 127
fi
shift

# Environment the workload needs; forwarded by value, if set.
forward=(PATH HOME CARGO_HOME RUSTUP_HOME CARGO_TERM_COLOR CARGO_INCREMENTAL
  RUST_BACKTRACE RUSTFLAGS RUSTUP_TOOLCHAIN PROCD_INCLUDE_DIR PROCD_LIB_DIR RUNNER_TEMP CI TMPDIR
  AGENTCTL_REQUIRE_PRODUCTION)
setenv=()
for name in "${forward[@]}"; do
  if [ -n "${!name+x}" ]; then
    setenv+=("--setenv=$name=${!name}")
  fi
done

exec sudo systemd-run \
  --quiet --pipe --wait --collect \
  --uid="$(id -u)" --gid="$(id -g)" \
  --property=Delegate=yes \
  --working-directory="$PWD" \
  "${setenv[@]}" \
  -- "$command_path" "$@"
