# 0082-remote-tui-hydration-forces-redraw

Status: accepted
Date: 2026-07-26
Area: ux
Scope: Initial PTY hydration for a session a remote client is showing — an SSH-connected TUI or the web terminal.

## Decision

After hydrating a visible PTY session from durable history, a remote client
forces one child redraw by sending a one-column size bump followed by the
actual pane size. This applies to normal-screen and alternate-screen terminal
applications, and to both remote clients: an SSH-connected TUI and the web
terminal. Local clients and remote sessions that are only hydrated for a
background preview do not gain this extra redraw.

## Reason

PTY history records terminal deltas, not a geometry-independent screen
snapshot. A remote client can attach at the same dimensions already cached by
the daemon yet still reconstruct an imperfect screen from prior
cursor-addressed output. The daemon correctly deduplicates that nominally
unchanged resize, so the child never repaints; physically resizing the user's
window fixes the screen only because it finally produces a real SIGWINCH.

The web terminal has a second, sharper reason. It replays history at the
geometry the daemon last cached and then immediately fits to the browser
viewport. That fit reflows bytes which were painted for a different grid, so
even a faithful replay can end up one row off. Once that happens, a child that
repaints its footer with relative cursor moves — a spinner line, a mode line —
overwrites the wrong rows, and the same line appears twice.

The explicit one-column bump makes the automatic attach path equivalent to the
successful manual workaround and guarantees the final repaint uses the visible
client's current geometry.

## Consequences

- A visible remote terminal application receives two resize notifications on
  first hydration and must finish at the exact requested geometry.
- The redraw is scoped to remote clients because network attachment and
  differing client geometry make replay drift common there; local attachment
  keeps the cheaper same-size dedup path.
- Background pinned or preview-only sessions must not be resized merely to
  warm their local render cache.
- The bump and the settle must be ordered, not concurrent. The daemon
  deduplicates by last-applied size, so overlapping them can collapse both
  into a single no-op.

## Non-Goals

- This does not make historical PTY bytes reflow across every past resize.
- This does not replace the daemon's same-size resize deduplication for normal
  steady-state layout events.
- This does not make a replayed grid correct on its own. It concedes the
  opposite: only the child can render its own screen authoritatively, so the
  client's job is to ask for that repaint rather than to reconstruct it.
