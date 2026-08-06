# Harnesses

A **harness** is an agent or shell runner inside construct. Harnesses let you run
smith, Claude, Codex, OpenCode, Prime Agent, Muse, Hermes, Antigravity, and local shells
side by side while construct gives them one UI, history, widgets, control
plane, and shared approval surface where supported.

A **fleet** is the set of sessions managed by one construct daemon. For example,
you can keep a shell running tests, ask Codex to implement a fix, ask Claude to
review it, and use smith as the built-in coordinator.

Contributing a new harness, or closing a feature gap in an existing one? The
developer-facing integration checklist lives in
[adding-a-harness.md](adding-a-harness.md).

## Which harness should I use?

| Harness | What it is | Use it when |
| --- | --- | --- |
| `smith` | construct's built-in agent | You want the deepest construct integration: native tools, approvals, skills, widgets, orchestration, and model-provider routing. |
| `shell` | Your local shell | You need long-running commands, logs, REPLs, servers, or manual debugging. |
| `claude` | The Claude CLI | You already use Claude Code and want it inside the same construct UI and session fleet. |
| `codex` | The Codex CLI | You already use Codex and want it inside the same construct UI and session fleet. |
| `opencode` | The OpenCode CLI | You already use OpenCode and want its native TUI inside the same construct session fleet. |
| `antigravity` | The Antigravity CLI | You want Antigravity sessions inside the same UI and daemon. |
| `grok` | The Grok CLI | You already use Grok and want it inside the same construct UI and session fleet. |
| `kimi` | The Kimi Code CLI | You already use Kimi Code and want its native TUI inside the same construct session fleet. |
| `hermes` | The Hermes Agent CLI | You use Hermes for coding and want its native UI, persisted sessions, and usage data in the same construct fleet. |
| `pi` | The pi coding agent CLI | You already use pi and want it inside the same construct UI and session fleet. |
| `prime-agent` | Prime Agent | You use Prime Agent and want its native TUI, structured headless mode, and sessions in the same construct fleet. |
| `muse` | Meta's Muse Code CLI | You use Muse and want its native TUI, headless mode, and resumable sessions in the same construct fleet. |

Create a session with:

```sh
construct new --prompt "review this repo" smith
construct new shell
construct new --prompt "implement the failing test" codex
construct new --prompt "implement the failing test" opencode
construct new --prompt "implement the failing test" kimi
construct new --prompt "implement the failing test" hermes
construct new --prompt "implement the failing test" pi
construct new --prompt "implement the failing test" prime-agent
construct new --prompt "implement the failing test" muse
```

Construct-owned options go before the harness name. Tokens after the harness name
are passed verbatim to that harness's CLI. That means `--model` before the
harness is construct metadata, while `--model` after the harness is passed to the
harness itself:

```sh
construct new --title "server logs" shell -lc 'tail -f server.log'
construct new --prompt "fix tests" claude --permission-mode acceptEdits
construct new --model opus claude      # construct model metadata
construct new claude --model opus      # raw claude CLI argv
```

By default, `construct new ...` creates an interactive session and opens the TUI
focused on it. Pass `--mode headless` when you want a script-friendly command
that creates a headless session, prints its id, and exits. Pass `--no-tui` when
you want to create an interactive session, print its id, and stay in the current
terminal.

CLI-backed harnesses require the matching CLI to be installed and discoverable on
`PATH`. Use the `*_BIN` or `*_CMD` environment variables below when you need to
point construct at a specific binary or command.

## What construct gives every harness

construct gives every harness the same shared session model, then lets each adapter
translate that model into the underlying agent or shell.

| Capability | Why it matters | Support and details |
| --- | --- | --- |
| Session identity and lifecycle | Every harness has the same id, title, state, cwd, mode, transcript, and lifecycle. | All harnesses. |
| Transcript and scrollback | You can inspect session history from the TUI, Web UI, and remote APIs, even after restart. | All harnesses; fidelity depends on what the harness emits. |
| Shared UI | Different CLIs appear in one session list instead of separate terminals. | All harnesses. |
| Approval flow | Risky actions can use construct's approval UI instead of each session inventing its own workflow. | Native in `smith` only; CLI-backed harnesses keep their own upstream gate. Defaults differ per harness — see [Write access and approvals](#write-access-and-approvals). |
| Widgets | Agents can publish Markdown status/action panels once and every client can render them. | All harnesses can write widgets via `CONSTRUCT_SESSION_WIDGETS_DIR`; see [Generative widgets](generative-widgets.md). |
| Session context | Sessions receive shared cwd, environment, data dirs, widget dirs, memory pointers, and resume flags. | All harnesses receive the context; each upstream CLI decides what to do with it. |
| Context gauge and breakdown | The status bar shows how full the model's context window is; hovering it details what fills the window (system prompt, tools, messages, free space) — estimated components are marked with `~`. | Gauge where the harness reports usage; breakdown where the adapter can derive components from the harness's own data (exact sections in `smith`; message/system-prompt estimates for CLI-backed harnesses with readable transcripts). |
| Skills | Reusable instructions can be defined once for the built-in agent. | Native in `smith`; CLI-backed harnesses use their own upstream skill/plugin systems today. |
| Unified tools | Agents can inspect and coordinate the fleet without shelling out to `construct` commands. | Native in `smith`; injected through MCP where supported; see [Unified tool layer](unified-tool-layer.md). |
| Resume | Restarts do not wipe out what you were looking at, and some upstream CLIs can continue the same conversation. | `smith` resumes from construct state; `shell` restarts in the same cwd; CLI-backed harnesses resume when their CLI exposes a reliable mechanism. |

The adapter is the translation layer between these fleet-wide capabilities and a
specific harness. Some capabilities are native in smith, some are injected into
CLI-backed harnesses, and some depend on what the upstream CLI exposes.

OpenCode receives Construct's unified tools through a process-local MCP entry.
It also uses OpenCode's native session fork for same-harness forks and reports
`/new` as reset lineage, matching the native context behavior of Claude and
Codex. Set `CONSTRUCT_INJECT_MCP=0` to disable MCP injection for any supported
wrapper harness.

## Built-in vs CLI-backed harnesses

There are two kinds of harnesses:

### Built-in harness

`smith` is native to construct. Use it when you want access to the most construct
features: tools, approvals, skills, widgets, background tasks, and structured
status updates.

See [smith built-in agent](smith.md) for details.

### CLI-backed harnesses

`claude`, `codex`, `opencode`, `antigravity`, `grok`, `kimi`, `hermes`, `pi`,
`prime-agent`, and `muse` wrap existing CLIs. Use them when you want those tools exactly as
installed on your machine, but inside the same construct fleet.

Because these depend on binaries and logins construct does not own, `construct
doctor` reports which ones it can find, which logins have expired, and whether
the running daemon sees a different `PATH` than your shell does.

CLI-backed harnesses keep their native behavior. If an upstream CLI does not
expose a setting — for example, path-scoped tool auto-approval — construct cannot
always force that behavior from outside the process. In those cases the session
still gets the shared UI, transcript, lifecycle, and environment, but the
upstream CLI keeps control of its own internals.

Claude Code, Codex, Antigravity, and Grok subagents created through their native
delegation tools appear beneath the owning session as `(native)` child rows.
Their live state and structured transcript are inspectable like any other
session, including nested children. These rows are read-only mirrors: use the
parent CLI's native subagent commands to message, interrupt, resume, or remove
them. Removing a Claude child archives its mirror while retaining the
transcript. A native child from any harness is archived automatically when it
reaches a terminal state, preserving both its transcript and terminal outcome.

## Interactive and headless sessions

Most harnesses can run in two modes:

- **Interactive**: the harness owns a PTY, so its normal terminal UI appears in
  the construct pane. This is the default when you create sessions from the TUI.
- **Headless**: the harness emits structured events instead of a terminal UI.
  This is useful for automation and non-PTY clients.

**How the mode is chosen.** An explicit `--mode` always wins. Otherwise the mode
is *interactive* when the creating client supplied a PTY size or used the
`construct new` CLI, and *headless* when it did not. The TUI always supplies one,
so TUI sessions default to interactive.

**The initial prompt does not pick the mode.** `construct new --prompt "<prompt>" <harness>`
and `construct new <harness>` both create interactive sessions unless you pass
`--mode headless`; the prompt only decides what the session does once it starts:

- A non-empty prompt is recorded as the first user turn and run immediately. For
  headless clients this is the structured-output path (for example, `shell`
  runs `$SHELL -lc "<prompt>"` once and exits).
- An empty prompt launches the harness's interactive playbook (for example,
  `shell` runs `$SHELL -il`), which you can attach to and type into.

Pass `--mode` to choose explicitly (optionally alongside a seed prompt):

```sh
construct new claude
construct new --no-tui claude
construct new --mode headless --prompt "summarize the last run" smith
```

`smith`, `claude`, `codex`, `antigravity`, `grok`, `hermes`, `pi`, `prime-agent`, and `muse`
support both modes. `opencode` and `kimi` are interactive-only and always run
their native TUIs. `shell` always
owns a PTY (there is no structured "headless" shell), so it presents a terminal
regardless of the mode label.

## Write access and approvals

Harnesses do **not** share one write/approval policy. Construct owns the
approval gate for `smith` only; every CLI-backed harness keeps whatever gate its
upstream CLI implements. That means the same prompt — "edit this file" — can
prompt you, write silently, or be refused outright, depending on which harness
the session runs.

This matters most when orchestrating a fleet, where one prompt fans out to
several harnesses at once. Check this table before assuming a session will stop
and ask you.

| Harness | Modes | Who gates a risky write | Honors construct's approval mode | Auto-approve paths | Unified tools |
| --- | --- | --- | --- | --- | --- |
| `smith` | interactive, headless | **construct** — its own gate, in both modes | Yes | Checked natively | Native |
| `shell` | always a PTY | Nobody — it is your shell, running your commands | n/a | n/a | n/a |
| `claude` | interactive, headless | Claude Code's own permission system | No | `--allowed-tools` | Injected |
| `codex` | interactive, headless | Codex's own sandbox and approval policy | No | Not translated | Injected |
| `opencode` | interactive only | OpenCode's native TUI | No | Not translated | Injected |
| `antigravity` | interactive, headless | Interactive: the `agy` TUI. **Headless: nothing** | No | Not translated | Not injected |
| `grok` | interactive, headless | The Grok CLI's own permission system | No | `--allow` | Not injected |
| `kimi` | interactive only | Kimi Code's native TUI | No | Not translated | Not injected |
| `hermes` | interactive, headless | Hermes' own defaults | No | Not translated | Not injected |
| `pi` | interactive, headless | pi's own defaults | No | Not translated | Not injected |
| `prime-agent` | interactive, headless | Prime Agent's own defaults | No | Not translated | Not injected |
| `muse` | interactive, headless | Muse's approval and sandbox policy | No | Not translated | Not injected |

### Reading the table

**"Honors construct's approval mode"** means the session reacts to construct
changing its approval mode at runtime (`manual` / `auto-review` /
`always-approve`, see [Approval modes](../specs/0015-approval-modes.md)). Only
`smith` does. Other adapters receive the message and drop it: construct does not
sit inside their tool-call loop, so it has nothing to gate. Where clients show
an approval mode, it is meaningful for tool-gating sessions; for the rest, the
authority is the upstream CLI's own setting.

**"Auto-approve paths"** is the `CONSTRUCT_AUTO_APPROVE_PATHS` allow-list. The
daemon sets it to the session's widget directory so agents can publish widgets
without prompting; it is not a general-purpose permission knob. Adapters
translate it only where the upstream CLI has a matching flag. Where the column
says *Not translated*, the upstream CLI exposes no path-scoped allow-list, so
the policy is passed in the environment but has no effect.

**"Unified tools"** is MCP injection (see
[Unified tool layer](unified-tool-layer.md)). It is independent of the approval
columns: sharing a fleet-control tool surface says nothing about who gates that
harness's file writes.

### Defaults worth knowing

- **`smith` prompts by default.** Read-only tools run silently; anything that
  mutates the filesystem or other sessions pauses for approval. Set
  `CONSTRUCT_SMITH_AUTOMODE=1` to start unattended runs in always-approve
  instead. See [smith](smith.md).
- **`claude` follows Claude Code's own defaults.** Construct does not force a
  `--permission-mode`; pass one yourself after the harness name if you want a
  specific behavior, e.g. `construct new claude --permission-mode acceptEdits`.
- **`codex` follows Codex's own sandbox.** In headless mode each turn is a
  non-interactive `codex exec`, so there is no prompt to answer: a write that
  Codex's sandbox refuses fails the turn instead of asking. If a codex session
  reports blocked writes, adjust Codex's own sandbox configuration.
- **`antigravity` headless approves everything.** The adapter passes
  `--dangerously-skip-permissions` because a headless turn has no TUI to approve
  in. Interactive antigravity sessions still approve normally in the `agy` TUI.
- **Interactive CLI-backed sessions approve in their own TUI.** The prompt
  appears inside the session pane, not in construct's minibuffer. Answer it
  there.
- **`muse` keeps its safety defaults.** Construct does not pass bypass flags;
  Muse starts with approvals and its sandbox enabled unless you explicitly pass
  different Muse arguments after the harness name.

Aligning these defaults would require either upstream CLI support or construct
intercepting each harness's tool calls; today the difference is intentional and
each harness keeps its native behavior.

## Resume after restart

When construct restarts, it restores sessions from saved start parameters:

- PTY scrollback and transcripts remain readable.
- `shell` starts a fresh shell in the original cwd.
- `smith` reloads its persisted conversation state.
- CLI-backed harnesses resume when their upstream CLI provides a reliable session
  id or resume command.

If a harness cannot be restarted — for example, its binary is missing — construct
marks the session errored while keeping the transcript available.

## Common knobs

You normally do not need these, but they are useful for scripting and debugging:

| Setting | Purpose |
| --- | --- |
| `--mode interactive\|headless` | Choose the session mode at creation time. |
| `CONSTRUCT_SMITH_MODE`, `CONSTRUCT_CLAUDE_MODE`, `CONSTRUCT_CODEX_MODE`, `CONSTRUCT_ANTIGRAVITY_MODE`, `CONSTRUCT_HERMES_MODE`, `CONSTRUCT_MUSE_MODE` | Default mode per harness. |
| `CONSTRUCT_CLAUDE_CMD`, `CONSTRUCT_CODEX_CMD`, `CONSTRUCT_OPENCODE_CMD`, `CONSTRUCT_ANTIGRAVITY_CMD`, `CONSTRUCT_KIMI_CMD`, `CONSTRUCT_HERMES_CMD`, `CONSTRUCT_MUSE_CMD`, `CONSTRUCT_SHELL_CMD` | Override the full command used for a CLI-backed harness or shell. |
| `CONSTRUCT_CLAUDE_BIN`, `CONSTRUCT_CODEX_BIN`, `CONSTRUCT_OPENCODE_BIN`, `CONSTRUCT_ANTIGRAVITY_BIN`, `CONSTRUCT_KIMI_BIN`, `CONSTRUCT_HERMES_BIN`, `CONSTRUCT_MUSE_BIN`, `CONSTRUCT_SHELL_BIN` | Override just the binary path when no full command override is set. |
| `CONSTRUCT_HERMES_HOME` | Override the Hermes home whose `state.db` the adapter follows. |
| `CONSTRUCT_SMITH_MODEL` | Default model for the built-in smith harness. |
| `CONSTRUCT_AUTO_APPROVE_PATHS` | Path allow-list injected into adapters that can translate it. Set by the daemon to the session's widget directory; see [Write access and approvals](#write-access-and-approvals). |
| `CONSTRUCT_SESSION_WIDGETS_DIR` | Directory where a session writes Markdown widgets. |
| `CONSTRUCT_INJECT_MCP=0` | Disable automatic MCP tool injection for MCP-capable harnesses. |

Set these in the daemon environment, or in whatever process starts `construct`. See
[Configuration](configuration.md) for general configuration patterns.

Prefer the normal `construct new ...` flow unless you are integrating construct into a
larger script or testing a custom harness setup.
