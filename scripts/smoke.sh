#!/usr/bin/env bash
# Quick end-to-end smoke test against a freshly built workspace.
#
# Spins up the daemon under an isolated $AGENTD_*_DIR sandbox, exercises the
# IPC surface (ping / harnesses / create / list / show / send / stop), and
# tears down. Run from the workspace root:
#
#     cargo build --workspace && scripts/smoke.sh

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SANDBOX=${AGENTD_SMOKE_DIR:-/tmp/agentd-smoke}
rm -rf "$SANDBOX"
mkdir -p "$SANDBOX"/{state,data,config,runtime}

export AGENTD_STATE_DIR="$SANDBOX/state"
export AGENTD_DATA_DIR="$SANDBOX/data"
export AGENTD_CONFIG_DIR="$SANDBOX/config"
export AGENTD_RUNTIME_DIR="$SANDBOX/runtime"

AGENTD="$ROOT/target/debug/agentd"
AGENT="$ROOT/target/debug/agent"
[ -x "$AGENTD" ] || { echo "build first: cargo build --workspace" >&2; exit 1; }
[ -x "$AGENT" ]  || { echo "build first: cargo build --workspace" >&2; exit 1; }

"$AGENTD" run >"$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
sleep 0.4

echo "==> ping"
"$AGENT" ping

echo "==> harnesses"
"$AGENT" harnesses

echo "==> shell session"
SID=$("$AGENT" new shell "echo hello-from-shell; echo and-another-line" --cwd "$SANDBOX")
echo "  session: $SID"
sleep 0.6

echo "==> list"
"$AGENT" list

echo "==> show"
"$AGENT" show "$SID"

echo "==> stop (idempotent on done sessions)"
"$AGENT" stop "$SID" 2>/dev/null || true

echo "OK"
