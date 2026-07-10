# 0082-remote-tui-hydration-forces-redraw

Status: accepted
Date: 2026-07-10
Area: tui
Scope: Initial PTY hydration for a session visible in a TUI connected through SSH.

## Decision

After hydrating a visible PTY session from durable history, an SSH-connected
TUI forces one child redraw by sending a one-column size bump followed by the
actual pane size. This applies to normal-screen and alternate-screen terminal
applications. Local clients and remote sessions that are only hydrated for a
background preview do not gain this extra redraw.

## Reason

PTY history records terminal deltas, not a geometry-independent screen
snapshot. A remote TUI can attach at the same dimensions already cached by the
daemon yet still reconstruct an imperfect screen from prior cursor-addressed
output. The daemon correctly deduplicates that nominally unchanged resize, so
the child never repaints; physically resizing the user's terminal fixes the
screen only because it finally produces a real SIGWINCH.

The explicit one-column bump makes the automatic attach path equivalent to
that successful manual workaround and guarantees the final repaint uses the
visible SSH pane's current geometry.

## Consequences

- A visible remote terminal application receives two resize notifications on
  first hydration and must finish at the exact requested pane size.
- The redraw is scoped to SSH because network attachment and differing client
  geometry make replay drift common there; local attachment keeps the cheaper
  same-size dedup path.
- Background pinned or preview-only sessions must not be resized merely to
  warm their local render cache.

## Non-Goals

- This does not make historical PTY bytes reflow across every past resize.
- This does not replace the daemon's same-size resize deduplication for normal
  steady-state layout events.
