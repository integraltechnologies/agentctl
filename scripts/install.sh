#!/usr/bin/env bash
# Installs agentctl, first installing its external prerequisite procd
# system-wide if it is not already installed.
#
#   scripts/install.sh [--prefix DIR] [--deps-only]
#
# procd is found where agentctl's build finds it: through the C toolchain's
# own search paths, or PROCD_INCLUDE_DIR and PROCD_LIB_DIR. When it is not
# there, procd's published release is fetched into a temporary directory,
# built with procd's own CMake, and its header, static library and `procd`
# command are installed under the prefix (default /usr/local; on Windows,
# %ProgramFiles%/procd). The temporary directory is removed afterwards;
# nothing of procd is placed in this repository.
#
# --deps-only installs the prerequisite and stops (CI uses this). Otherwise
# agentctl is then installed with `cargo install --locked --path`.
#
# Needs git, CMake 3.16 or later and a C11 compiler to install procd, and a
# Rust toolchain to install agentctl. Uses sudo when the prefix is not
# writable.

set -euo pipefail

PROCD_REPO=https://github.com/integraltechnologies/procd
PROCD_TAG=v0.1.0
PROCD_COMMIT=2e3bb6e86156a8863dde52940f0020eb6d7d18e7

repo="$(cd "$(dirname "$0")/.." && pwd)"
prefix=""
deps_only=false
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --deps-only) deps_only=true; shift ;;
    *) echo "usage: $0 [--prefix DIR] [--deps-only]" >&2; exit 2 ;;
  esac
done

windows=false
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) windows=true ;; esac
if [ -z "$prefix" ]; then
  if $windows; then prefix="$(cygpath -m "$PROGRAMFILES")/procd"; else prefix=/usr/local; fi
fi
if $windows; then library=procd.lib; else library=libprocd.a; fi

# Whether agentctl's build will find an installed procd.
installed() {
  if [ -n "${PROCD_INCLUDE_DIR:-}${PROCD_LIB_DIR:-}" ]; then
    [ -f "${PROCD_INCLUDE_DIR:-}/procd.h" ] && [ -f "${PROCD_LIB_DIR:-}/$library" ]
  elif $windows; then
    # MSVC has no system-wide library path: the install is at the prefix,
    # named to the build through PROCD_INCLUDE_DIR and PROCD_LIB_DIR.
    [ -f "$prefix/include/procd.h" ] && [ -f "$prefix/lib/$library" ]
  else
    local probe status
    probe="$(mktemp -d)"
    printf '#include <procd.h>\nint main(void) { return procd_status_name(PROCD_OK) == 0; }\n' \
      > "$probe/probe.c"
    status=0
    "${CC:-cc}" "$probe/probe.c" -o "$probe/probe" -lprocd -pthread >/dev/null 2>&1 || status=$?
    rm -rf "$probe"
    return "$status"
  fi
}

# The temporary directory install_procd builds procd in, removed however the
# script ends: success, an error under set -e, SIGINT or SIGTERM. It is
# script-wide so the traps still see it once install_procd has returned.
procd_work=""
cleanup() {
  if [ -n "$procd_work" ]; then rm -rf "$procd_work"; fi
}
trap cleanup EXIT
trap 'cleanup; trap - INT; kill -INT $$' INT
trap 'cleanup; trap - TERM; kill -TERM $$' TERM

install_procd() {
  local work build lib cli sudo=""
  procd_work="$(mktemp -d)"
  work="$procd_work"
  git clone --quiet --depth 1 --branch "$PROCD_TAG" "$PROCD_REPO" "$work/procd"
  if [ "$(git -C "$work/procd" rev-parse HEAD)" != "$PROCD_COMMIT" ]; then
    echo "procd $PROCD_TAG is not the expected commit $PROCD_COMMIT" >&2
    exit 1
  fi
  build="$work/build"
  cmake -S "$work/procd" -B "$build" -DCMAKE_BUILD_TYPE=Release
  cmake --build "$build" --config Release --target procd procd-cli
  # procd v0.1.0 has no install rules: its header, library and command are
  # copied. Multi-configuration generators (Visual Studio) build into Release/.
  if $windows; then
    lib="$build/Release/procd.lib"
    cli="$build/Release/procd.exe"
  else
    lib="$build/libprocd.a"
    cli="$build/procd"
  fi
  if ! mkdir -p "$prefix/include" "$prefix/lib" "$prefix/bin" 2>/dev/null ||
    [ ! -w "$prefix/include" ] || [ ! -w "$prefix/lib" ] || [ ! -w "$prefix/bin" ]; then
    sudo=sudo
    sudo mkdir -p "$prefix/include" "$prefix/lib" "$prefix/bin"
  fi
  $sudo cp "$work/procd/include/procd.h" "$prefix/include/procd.h"
  $sudo cp "$lib" "$prefix/lib/$library"
  $sudo cp "$cli" "$prefix/bin/"
  rm -rf "$work"
  procd_work=""
}

if installed; then
  echo "procd is already installed; not reinstalling it"
elif [ -n "${PROCD_INCLUDE_DIR:-}${PROCD_LIB_DIR:-}" ]; then
  echo "PROCD_INCLUDE_DIR and PROCD_LIB_DIR must both be set, to directories holding" \
    "procd.h and $library; unset them to install procd under $prefix" >&2
  exit 1
else
  echo "procd is not installed: installing procd $PROCD_TAG under $prefix"
  install_procd
  if $windows; then
    export PROCD_INCLUDE_DIR="$prefix/include" PROCD_LIB_DIR="$prefix/lib"
    echo "procd installed; builds find it with PROCD_INCLUDE_DIR=$PROCD_INCLUDE_DIR" \
      "and PROCD_LIB_DIR=$PROCD_LIB_DIR, and its command is in $prefix/bin"
  elif ! installed; then
    echo "procd is installed under $prefix, outside the C toolchain's search paths:" \
      "set PROCD_INCLUDE_DIR=$prefix/include and PROCD_LIB_DIR=$prefix/lib" >&2
    $deps_only || exit 1
  fi
fi

if ! $deps_only; then
  cargo install --locked --path "$repo"
fi
