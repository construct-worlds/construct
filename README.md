# agentd

A terminal **agent fleet** — run and supervise multiple coding-agent sessions across heterogeneous harnesses (Claude Code, Codex, generic shell, ...) from one TUI.

Status: **milestone 1 — working but unstable. Wire protocols may break.**

```
┌─ sessions ────────────────┬─ session: s4f3...  shell  running ─────┐
│ ● s4f3a...  shell   echo… │  [12:04:11] status running              │
│ ◐ sa3944... shell   while │  [12:04:11]  agent: hello-from-shell    │
│ ✓ sc4d20... shell   echo… │  [12:04:11]  agent: and-another-line    │
│ ✗ s78b1... claude   migr… │  [12:04:11] ▢ done (exit 0)             │
│                           │                                          │
├───────────────────────────┴──────────────────────────────────────────┤
│ M-x send-input ▸ confirm yes_                                        │
├──────────────────────────────────────────────────────────────────────┤
│ agentd  [emacs]  sc4d20bd24  done  -    ? for help                   │
└──────────────────────────────────────────────────────────────────────┘
```

## Architecture

Five layers, each replaceable:

```
┌──────────────────────────────────────────────┐
│ TUI shell (rendering, layout, keymap)        │  emacs default; vim profile
├──────────────────────────────────────────────┤
│ Command + keybinding kernel                  │  every action is a command
├──────────────────────────────────────────────┤
│ Session manager (state, events, broadcast)   │  daemon-side
├──────────────────────────────────────────────┤
│ Agent Harness Protocol (AHP) — JSON-RPC      │  stable wire contract
├──────────────────────────────────────────────┤
│ Harness adapters (separate processes)        │  plugin boundary
│ shell   claude   codex   <your-harness>      │
└──────────────────────────────────────────────┘
```

- **Daemon** (`agentd`) owns sessions, spawns adapters, persists transcripts. Speaks JSON-RPC over a Unix socket to clients.
- **Client** (`agent`) is the TUI plus a set of one-shot subcommands. Multiple clients can attach concurrently.
- **Adapter** binaries are independent processes. They implement the AHP over stdio. Anyone can ship one in any language.

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `crates/protocol` | — (lib) | AHP + IPC types, transport, adapter SDK |
| `crates/daemon` | `agentd` | Session supervisor, IPC server |
| `crates/cli` | `agent` | TUI client + control subcommands |
| `crates/adapter-shell` | `agentd-adapter-shell` | Generic shell command runner |
| `crates/adapter-claude` | `agentd-adapter-claude` | Wraps the `claude` CLI |
| `crates/adapter-codex` | `agentd-adapter-codex` | Wraps the `codex` CLI |

## Quick start

```sh
cargo build --workspace --release

# Terminal 1: daemon (foreground)
./target/release/agentd run

# Terminal 2: control
./target/release/agent harnesses
./target/release/agent new shell "echo hello"
./target/release/agent list
./target/release/agent          # launches TUI
```

Smoke test:

```sh
cargo build --workspace
scripts/smoke.sh
```

## Paths

`agentd` reads/writes under XDG-style directories, with `AGENTD_*_DIR` overrides:

| Use | Default | Override |
|---|---|---|
| Config | `~/.config/agentd` | `AGENTD_CONFIG_DIR` |
| State (pid/log) | `~/.local/state/agentd` | `AGENTD_STATE_DIR` |
| Data (sessions) | `~/.local/share/agentd` | `AGENTD_DATA_DIR` |
| Socket | `$XDG_RUNTIME_DIR/agentd/agentd.sock` (falls back to state) | `AGENTD_RUNTIME_DIR` |

`agentd paths` prints the resolved layout.

## TUI keys (emacs default)

| Key | Action |
|---|---|
| `C-n` / `↓` | next session |
| `C-p` / `↑` | prev session |
| `C-c i` | send input to selected session |
| `C-c n` | new session (minibuffer wizard) |
| `C-c k` | kill selected session (confirms) |
| `C-c d` | show diff for selected session |
| `C-c C-c` | interrupt |
| `C-c r` | refresh |
| `M-x` | command palette |
| `Tab` | switch focus (list / transcript) |
| `?` | toggle help |
| `C-x C-c` / `q` | quit |

Set `AGENTD_KEYMAP=vim` for the vim profile.

## Adapter protocol (AHP)

The daemon spawns one adapter process per session and speaks JSON-RPC 2.0 over the adapter's stdin/stdout, one message per line.

Methods the adapter implements:

| Method | Payload |
|---|---|
| `initialize` | `{protocol_version, client_info}` → `InitializeResult` |
| `session.start` | `{session_id, cwd, prompt?, model?, env, args}` |
| `session.input` | `{session_id, text}` |
| `session.interrupt` | `{session_id}` |
| `session.stop` | `{session_id}` |
| `shutdown` | `{}` |

Notifications the adapter emits:

- `session/event` — one `SessionEvent` (see [`SessionEvent`](crates/protocol/src/lib.rs))
- `log` — free-form line for the daemon's log

Writing an adapter in Rust is roughly:

```rust
use agentd_protocol::adapter::run;
use agentd_protocol::{Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "demo".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities { supports_input: true, ..Default::default() },
    };
    run(metadata, |params, mut ctx| async move {
        ctx.emit.emit(SessionEvent::Status { state: SessionState::Running, detail: None });
        ctx.emit.emit(SessionEvent::Message {
            role: MessageRole::Assistant,
            text: format!("got prompt: {:?}", params.prompt),
        });
        ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
    }).await
}
```

Adapters in other languages just need to speak the same JSON shapes.

## Milestone 1 scope

Implemented:

- [x] Session lifecycle (create, list, get, send input, interrupt, stop, kill)
- [x] Multi-harness adapters: `shell`, `claude`, `codex`
- [x] **Multi-turn** for `claude` (via `--resume <session_id>`) and `codex`
      (per-turn re-spawn; opt-in resume via `AGENTD_CODEX_RESUME_FLAG`)
- [x] Live transcript view (streaming, structural rendering)
- [x] Session list with status glyphs
- [x] Send input to selected session; mid-turn inputs queue for the next turn
- [x] Diff panel (uses `git diff` against the session cwd / worktree)
- [x] Git worktree isolation (`--worktree`)
- [x] Command palette (`M-x`)
- [x] emacs + vim keymap profiles
- [x] Config file (`~/.config/agentd/config.toml`)
- [x] Daemon + client process split (Unix socket)

### Multi-turn semantics

Each adapter exposes the same surface to the daemon: a session stays alive
across many user turns until you `stop` or `kill` it. After the assistant
finishes a turn, the session enters `awaiting_input`; the next `agent send`
starts the next turn. Inputs sent while a turn is still in flight queue and
run in order.

- **`shell`** — true interactive: pipes input straight into the child's stdin.
- **`claude`** — per-turn `claude -p` process; subsequent turns pass
  `--resume <session_id>` (captured from the first turn's init event), so the
  Claude CLI threads the conversation server-side. Cost events come from the
  `result` payload.
- **`codex`** — per-turn `codex exec` process. By default each turn starts
  fresh (no context carry-over) — set `AGENTD_CODEX_RESUME_FLAG=--session-id`
  (or whichever resume flag your codex build supports) to pass a captured
  `session_id` field back in on each turn.

Deferred to later milestones:

- Custom user keymap file (today: choose `AGENTD_KEYMAP=emacs|vim`)
- Cost/token dashboards across sessions
- Notifications (desktop / Slack)
- Web UI on the same IPC

## License

MIT — see [LICENSE](LICENSE).
