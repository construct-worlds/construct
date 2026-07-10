#!/usr/bin/env bash
# Isolated wrapper around the worktree's debug `construct` binary.
#
# Same CLI as construct — all args are forwarded unchanged — but every
# CONSTRUCT_* path points at a per-worktree sandbox under /tmp so this never
# touches the system install's daemon, socket, config, or session data.
#
# Examples:
#   scripts/test_construct.sh daemon restart --sessions
#   scripts/test_construct.sh new shell ""
#   scripts/test_construct.sh list
#   scripts/test_construct.sh ping
#
# Override the sandbox root with CONSTRUCT_TEST_DIR if you need a stable path
# across shells. The worktree's target/debug is prepended to PATH so adapters
# resolve from the same build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$WT/target/debug"
CLIENT_BIN="$BIN_DIR/construct"

if [[ ! -x "$CLIENT_BIN" ]]; then
  echo "missing $CLIENT_BIN; run: cargo build" >&2
  exit 1
fi

if [[ -n "${CONSTRUCT_TEST_DIR:-}" ]]; then
  DEMO_DIR="$CONSTRUCT_TEST_DIR"
else
  WT_NAME="$(basename "$WT")"
  SAFE_WT_NAME="$(printf '%s' "$WT_NAME" | tr -c 'A-Za-z0-9_.-' '-')"
  DEMO_DIR="/tmp/construct-test-${SAFE_WT_NAME}"
fi

export CONSTRUCT_RUNTIME_DIR="$DEMO_DIR/run"
export CONSTRUCT_STATE_DIR="$DEMO_DIR/state"
export CONSTRUCT_DATA_DIR="$DEMO_DIR/data"
export CONSTRUCT_CONFIG_DIR="$DEMO_DIR/config"
export CONSTRUCT_SHELL_BIN="${CONSTRUCT_SHELL_BIN:-/bin/bash}"
export BASH_SILENCE_DEPRECATION_WARNING="${BASH_SILENCE_DEPRECATION_WARNING:-1}"
# Keep test remotes local; don't open a real tunnel from a sandbox daemon.
export CONSTRUCT_REMOTE_NO_TUNNEL=1
export PATH="$BIN_DIR:$PATH"

mkdir -p \
  "$CONSTRUCT_RUNTIME_DIR" \
  "$CONSTRUCT_STATE_DIR" \
  "$CONSTRUCT_DATA_DIR" \
  "$CONSTRUCT_CONFIG_DIR"

exec "$CLIENT_BIN" "$@"
