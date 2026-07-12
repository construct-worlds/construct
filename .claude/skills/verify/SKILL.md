---
name: verify
description: Drive a construct daemon end-to-end from a worktree build — isolated daemon, headless sessions, raw IPC — to verify daemon/adapter/session changes at their real surface.
---

# Verifying daemon changes end-to-end

## Isolated daemon from a worktree build

```bash
DEMO=/tmp/<short-name>   # keep it SHORT: long runtime dirs overflow SUN_LEN on macOS
rm -rf $DEMO; mkdir -p $DEMO/{run,state,data,config}
export CONSTRUCT_RUNTIME_DIR=$DEMO/run CONSTRUCT_STATE_DIR=$DEMO/state \
       CONSTRUCT_DATA_DIR=$DEMO/data CONSTRUCT_CONFIG_DIR=$DEMO/config \
       CONSTRUCT_SHELL_BIN=/bin/bash
BIN=<worktree>/target/debug/construct
$BIN daemon run > $DEMO/daemon.log 2>&1 &
for i in $(seq 1 50); do $BIN ping >/dev/null 2>&1 && break; sleep 0.2; done
```

Kill the daemon and `rm -rf $DEMO` when done.

## Sessions without a TUI

`construct new shell "" --no-tui` prints the session id and exits.
Plain `construct new` tries to attach the TUI and dies with
"enable raw mode: Device not configured" in a non-tty shell.

## Raw IPC (the daemon's real surface)

Newline-delimited JSON-RPC over `$CONSTRUCT_RUNTIME_DIR/construct.sock`.
From python: connect a `AF_UNIX` stream socket, send
`{"jsonrpc":"2.0","id":1,"method":"session.pty_input","params":{...}}\n`,
read one line back. Useful methods: `session.pty_input`
(`{session_id, data:<base64>}`), `session.pty_replay` (returns base64
`data` of the pty log), `usage.query`, `ping`.

Send a shell command as PTY input with a trailing `\r`; give the
harness ~1s, then `session.pty_replay` shows the echo + output.

## Simulating a slow / starved adapter

Each session's adapter is a child process of the daemon; find it by
matching its open adapter socket to the session id:

```bash
for pid in $(pgrep -P <daemon-pid>); do
  lsof -U -a -p $pid 2>/dev/null | grep -q <session-id> && echo $pid
done
```

`kill -STOP <adapter-pid>` freezes its AHP ACKs (the real
"CPU-starved adapter" failure mode); `kill -CONT` resumes and any
queued input flows through. Good for exercising input queueing,
timeouts, and dispatch-loop behavior.
