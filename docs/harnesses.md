# Harnesses

A **harness** is the engine that runs a session. agentd gives each harness the
same outer shape — sessions, transcripts, approvals, widgets, restart behavior,
and remote control — while the harness decides how the actual assistant or shell
runs.

You can mix harnesses in one fleet. For example, keep a shell open, run Codex in
one pane, ask Claude in another, and use Zarvis as the built-in agent that can
coordinate them.

## Available harnesses

| Harness | What it is | Best for |
| --- | --- | --- |
| `zarvis` | agentd's built-in agent | Native tool use, session orchestration, widgets, and model-provider routing without a separate CLI. |
| `shell` | Your local shell | Long-running commands, logs, REPLs, and manual debugging. |
| `claude` | The Claude CLI | Using Claude Code exactly as installed on your machine, inside the agentd fleet. |
| `codex` | The Codex CLI | Using Codex exactly as installed on your machine, inside the agentd fleet. |
| `antigravity` | The Antigravity CLI | Using Antigravity sessions inside the same UI and daemon. |

Create a session with:

```sh
agent new zarvis "review this repo"
agent new shell ""
agent new codex "implement the failing test"
```

## Interactive and headless sessions

Most harnesses can run in two modes:

- **Interactive**: the harness owns a PTY, so its normal terminal UI appears in
  the agentd pane. This is the default when you create sessions from the TUI.
- **Headless**: the harness emits structured events instead of a terminal UI.
  This is useful for automation and non-PTY clients.

Choose explicitly when needed:

```sh
agent new claude --mode interactive ""
agent new zarvis --mode headless "summarize the last run"
```

`zarvis`, `claude`, `codex`, and `antigravity` support both modes. `shell` is
always interactive because it is a terminal program.

## What agentd provides to every harness

agentd is not just a process launcher. It provides a shared **session contract**
for every harness:

- **One session model**: every harness appears as a session with an id, title,
  state, transcript, scrollback, cwd, and lifecycle.
- **One UI surface**: sessions show in the same TUI, Web UI, and remote-control
  APIs, even when their internal CLIs are different.
- **One approval flow**: risky actions surface through agentd's approval UI when
  the harness exposes enough control for agentd to mediate them.
- **One widget system**: harnesses can publish compact Markdown status panels and
  action links through the session widgets directory.
- **One persistence story**: transcripts and PTY logs stay readable after a
  restart; adapters resume their upstream harness when that harness supports it.
- **One configuration layer**: agentd passes common environment and policy into
  harnesses so user-facing features can be defined once and reused.

A useful term for this is **capability injection**: define a capability once at
the agentd layer, and agentd injects it into the harness in the form that harness
understands.

Examples:

- Define skills once, and agentd can include the relevant skill instructions in
  Zarvis prompts. Wrapper harnesses still run their own upstream CLIs, but they
  receive the same session context and environment where applicable.
- Define a session widget once, and every client renders it the same way.
- Define an auto-approval policy once, and adapters translate it into their
  harness's native allow-list when the upstream CLI supports that.
- Define session metadata once — cwd, title, mode, worktree, environment — and
  every harness starts with the same agentd-managed context.

The adapter is the translation layer between this shared contract and a specific
harness.

## Built-in vs wrapper harnesses

There are two kinds of harnesses:

### Built-in harness

`zarvis` is native to agentd. Because it runs inside the agentd adapter, it can
use agentd features directly: tools, approvals, skills, widgets, background
tasks, and structured status updates.

See [Zarvis built-in agent](zarvis.md) for details.

### Wrapper harnesses

`claude`, `codex`, and `antigravity` wrap existing CLIs. agentd starts the CLI,
connects it to a session, records its output, and injects the common session
context it can provide.

Wrapper harnesses keep their native behavior. If an upstream CLI does not expose
a setting — for example, path-scoped tool auto-approval — agentd cannot always
force that behavior from outside the process. In those cases the session still
gets the shared UI, transcript, lifecycle, and environment, but the upstream CLI
keeps control of its own internals.

## Resume after restart

When agentd restarts, it restores sessions from saved start parameters:

- PTY scrollback and transcripts remain readable.
- `shell` starts a fresh shell in the original cwd.
- `zarvis` reloads its persisted conversation state.
- Wrapper harnesses resume when their upstream CLI provides a reliable session id
  or resume command.

If a harness cannot be restarted — for example, its binary is missing — agentd
marks the session errored while keeping the transcript available.

## Common knobs

You normally do not need these, but they are useful for scripting and debugging:

| Setting | Purpose |
| --- | --- |
| `--mode interactive\|headless` | Choose the session mode at creation time. |
| `AGENTD_ZARVIS_MODE`, `AGENTD_CLAUDE_MODE`, `AGENTD_CODEX_MODE`, `AGENTD_ANTIGRAVITY_MODE` | Default mode per harness. |
| `AGENTD_*_CMD` | Override the full command used for a built-in wrapper harness. |
| `AGENTD_*_BIN` | Override just the binary path when no full command override is set. |
| `AGENTD_AUTO_APPROVE_PATHS` | Path allow-list injected into adapters that can translate it. |
| `AGENTD_SESSION_WIDGETS_DIR` | Directory where a session writes Markdown widgets. |

Prefer the normal `agent new ...` flow unless you are integrating agentd into a
larger script or testing a custom harness setup.
