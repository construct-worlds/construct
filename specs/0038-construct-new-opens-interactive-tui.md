# 0038-construct-new-opens-interactive-tui

Status: accepted
Date: 2026-06-24
Area: cli
Scope: The user-facing `construct new` command that creates top-level sessions.

## Decision

`construct new` is an interactive entry point by default. When no explicit mode
is provided, it creates the session in interactive mode, starts the daemon if
needed, and opens the TUI focused on the new session.

The command grammar is `construct new [construct-options] <harness>
[harness-args...]`: construct-owned options appear before the harness name, and
every token after the harness name is passed verbatim to the harness adapter as
CLI argv. The initial prompt is a construct-owned option (`--prompt`/`-p`), not a
positional argument after the harness.

Scripts and integrations that need create-and-exit behavior must request a
non-attaching flow explicitly. `--no-tui` creates an interactive session, prints
its id, and exits. `--mode headless` creates a headless session, prints its id,
and exits.

## Reason

Creating a new session is usually the first step in operating it. Opening the
TUI makes the command behave like the user's next expected action, avoids a
separate attach step, and keeps the default experience aligned with how sessions
are created inside the TUI.

## Consequences

Future CLI changes should keep the default `construct new` flow attached to the
interactive UI. One-shot creation behavior should remain explicit so it remains
safe for scripts to choose it deliberately.

Future construct-owned session-creation options should preserve the boundary:
options parsed by construct belong before `<harness>`, while post-harness argv is
reserved for the harness and must not be parsed by construct.

Removing or hiding individual one-shot commands does not remove the underlying
client or protocol operation when other clients, tools, or the TUI still need
that operation.

## Non-Goals

This does not require every IPC client to default to interactive mode. Non-TUI
protocol clients may keep their own explicit mode and attach behavior.
