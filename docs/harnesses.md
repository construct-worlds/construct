# Harnesses

A **harness** is an agent or shell runner inside agentd. Harnesses let you run
Zarvis, Claude, Codex, Antigravity, and local shells side by side while agentd
gives them one UI, history, widgets, control plane, and shared approval surface
where supported.

A **fleet** is the set of sessions managed by one agentd daemon. For example,
you can keep a shell running tests, ask Codex to implement a fix, ask Claude to
review it, and use Zarvis as the built-in coordinator.

## Which harness should I use?

| Harness | What it is | Use it when |
| --- | --- | --- |
| `zarvis` | agentd's built-in agent | You want the deepest agentd integration: native tools, approvals, skills, widgets, orchestration, and model-provider routing. |
| `shell` | Your local shell | You need long-running commands, logs, REPLs, servers, or manual debugging. |
| `claude` | The Claude CLI | You already use Claude Code and want it inside the same agentd UI and session fleet. |
| `codex` | The Codex CLI | You already use Codex and want it inside the same agentd UI and session fleet. |
| `antigravity` | The Antigravity CLI | You want Antigravity sessions inside the same UI and daemon. |

Create a session with:

```sh
agent new zarvis "review this repo"
agent new shell ""
agent new codex "implement the failing test"
```

CLI-backed harnesses require the matching CLI to be installed and discoverable on
`PATH`. Use the `*_BIN` or `*_CMD` environment variables below when you need to
point agentd at a specific binary or command.

## What agentd gives every harness

agentd gives every harness the same shared session model, then lets each adapter
translate that model into the underlying agent or shell.

| Capability | Why it matters | Support and details |
| --- | --- | --- |
| Session identity and lifecycle | Every harness has the same id, title, state, cwd, mode, transcript, and lifecycle. | All harnesses. |
| Transcript and scrollback | You can inspect session history from the TUI, Web UI, and remote APIs, even after restart. | All harnesses; fidelity depends on what the harness emits. |
| Shared UI | Different CLIs appear in one session list instead of separate terminals. | All harnesses. |
| Approval flow | Risky actions can use agentd's approval UI instead of each session inventing its own workflow. | Native in `zarvis`; translated where CLI-backed harnesses expose enough control. |
| Widgets | Agents can publish Markdown status/action panels once and every client can render them. | All harnesses can write widgets via `AGENTD_SESSION_WIDGETS_DIR`; see [Generative widgets](generative-widgets.md). |
| Session context | Sessions receive shared cwd, environment, data dirs, widget dirs, memory pointers, and resume flags. | All harnesses receive the context; each upstream CLI decides what to do with it. |
| Skills | Reusable instructions can be defined once for the built-in agent. | Native in `zarvis`; CLI-backed harnesses use their own upstream skill/plugin systems today. |
| Unified tools | Agents can inspect and coordinate the fleet without shelling out to `agent` commands. | Native in `zarvis`; injected through MCP where supported; see [Unified tool layer](unified-tool-layer.md). |
| Resume | Restarts do not wipe out what you were looking at, and some upstream CLIs can continue the same conversation. | `zarvis` resumes from agentd state; `shell` restarts in the same cwd; CLI-backed harnesses resume when their CLI exposes a reliable mechanism. |

The adapter is the translation layer between these fleet-wide capabilities and a
specific harness. Some capabilities are native in Zarvis, some are injected into
CLI-backed harnesses, and some depend on what the upstream CLI exposes.

## Built-in vs CLI-backed harnesses

There are two kinds of harnesses:

### Built-in harness

`zarvis` is native to agentd. Use it when you want access to the most agentd
features: tools, approvals, skills, widgets, background tasks, and structured
status updates.

See [Zarvis built-in agent](zarvis.md) for details.

### CLI-backed harnesses

`claude`, `codex`, and `antigravity` wrap existing CLIs. Use them when you want
those tools exactly as installed on your machine, but inside the same agentd
fleet.

CLI-backed harnesses keep their native behavior. If an upstream CLI does not
expose a setting — for example, path-scoped tool auto-approval — agentd cannot
always force that behavior from outside the process. In those cases the session
still gets the shared UI, transcript, lifecycle, and environment, but the
upstream CLI keeps control of its own internals.

## Interactive and headless sessions

Most harnesses can run in two modes:

- **Interactive**: the harness owns a PTY, so its normal terminal UI appears in
  the agentd pane. This is the default when you create sessions from the TUI.
- **Headless**: the harness emits structured events instead of a terminal UI.
  This is useful for automation and non-PTY clients.

Sessions created from the TUI default to interactive mode. CLI/API-created
sessions may run headless unless you pass `--mode interactive`. Choose explicitly
when the mode matters:

```sh
agent new claude --mode interactive ""
agent new zarvis --mode headless "summarize the last run"
```

`zarvis`, `claude`, `codex`, and `antigravity` support both modes. `shell` is
always interactive because it is a terminal program.

## Resume after restart

When agentd restarts, it restores sessions from saved start parameters:

- PTY scrollback and transcripts remain readable.
- `shell` starts a fresh shell in the original cwd.
- `zarvis` reloads its persisted conversation state.
- CLI-backed harnesses resume when their upstream CLI provides a reliable session
  id or resume command.

If a harness cannot be restarted — for example, its binary is missing — agentd
marks the session errored while keeping the transcript available.

## Common knobs

You normally do not need these, but they are useful for scripting and debugging:

| Setting | Purpose |
| --- | --- |
| `--mode interactive\|headless` | Choose the session mode at creation time. |
| `AGENTD_ZARVIS_MODE`, `AGENTD_CLAUDE_MODE`, `AGENTD_CODEX_MODE`, `AGENTD_ANTIGRAVITY_MODE` | Default mode per harness. |
| `AGENTD_CLAUDE_CMD`, `AGENTD_CODEX_CMD`, `AGENTD_ANTIGRAVITY_CMD`, `AGENTD_SHELL_CMD` | Override the full command used for a CLI-backed harness or shell. |
| `AGENTD_CLAUDE_BIN`, `AGENTD_CODEX_BIN`, `AGENTD_ANTIGRAVITY_BIN`, `AGENTD_SHELL_BIN` | Override just the binary path when no full command override is set. |
| `AGENTD_ZARVIS_MODEL` | Default model for the built-in Zarvis harness. |
| `AGENTD_AUTO_APPROVE_PATHS` | Path allow-list injected into adapters that can translate it. |
| `AGENTD_SESSION_WIDGETS_DIR` | Directory where a session writes Markdown widgets. |
| `AGENTD_INJECT_MCP=0` | Disable automatic MCP tool injection for MCP-capable harnesses. |

Set these in the daemon environment, or in whatever process starts `agentd`. See
[Configuration](configuration.md) for general configuration patterns.

Prefer the normal `agent new ...` flow unless you are integrating agentd into a
larger script or testing a custom harness setup.
