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

# Environment the workload needs; forwarded by value, if set.
forward=(PATH HOME CARGO_HOME RUSTUP_HOME CARGO_TERM_COLOR CARGO_INCREMENTAL
  RUST_BACKTRACE RUSTFLAGS PROCD_PREFIX PROCD_CLI RUNNER_TEMP CI TMPDIR)
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
  -- "$@"
