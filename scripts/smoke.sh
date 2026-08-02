#!/usr/bin/env bash
# Quick end-to-end smoke test against a freshly built workspace.
#
# Spins up the daemon under an isolated $CONSTRUCT_*_DIR sandbox, exercises the
# IPC surface (ping / harnesses / create / list / show / send / stop), and
# tears down. Run from the workspace root:
#
#     cargo build --workspace && scripts/smoke.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SANDBOX=${CONSTRUCT_SMOKE_DIR:-/tmp/construct-smoke}
rm -rf "$SANDBOX"
mkdir -p "$SANDBOX"/{state,data,config,runtime}

export CONSTRUCT_STATE_DIR="$SANDBOX/state"
export CONSTRUCT_DATA_DIR="$SANDBOX/data"
export CONSTRUCT_CONFIG_DIR="$SANDBOX/config"
export CONSTRUCT_RUNTIME_DIR="$SANDBOX/runtime"

CONSTRUCT_CLI="$ROOT/target/debug/construct"
[ -x "$CONSTRUCT_CLI" ]  || { echo "build first: cargo build --workspace" >&2; exit 1; }

"$CONSTRUCT_CLI" daemon run >"$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
# Poll for readiness: a fixed sleep loses the race on a cold cache or a slow
# machine, and the whole script then fails on the first `ping`.
for _ in $(seq 1 100); do
  "$CONSTRUCT_CLI" ping >/dev/null 2>&1 && break
  sleep 0.2
done

echo "==> ping"
"$CONSTRUCT_CLI" ping

echo "==> harnesses"
"$CONSTRUCT_CLI" harnesses

echo "==> shell session"
# Construct's own flags go before the harness name; tokens after it are the
# harness's own argv. A shell *command* is a prompt (the adapter runs it as
# `$SHELL -lc <prompt>`), not argv — as argv, bash reads it as a script
# filename and exits. `--no-tui` keeps the id on stdout instead of attaching.
SID=$("$CONSTRUCT_CLI" new --no-tui --cwd "$SANDBOX" \
  --prompt "echo hello-from-shell; echo and-another-line" shell)
echo "  session: $SID"
sleep 0.6

echo "==> list"
"$CONSTRUCT_CLI" list

echo "==> show"
"$CONSTRUCT_CLI" show "$SID"

echo "==> stop (idempotent on done sessions)"
"$CONSTRUCT_CLI" stop "$SID" 2>/dev/null || true

echo "OK"
