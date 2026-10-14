# 0217-tui-animations-off-by-default

Status: accepted
Date: 2026-10-14
Area: tui
Scope: Looping visual effects in the TUI and how every frame reaches the terminal.

## Decision

Looping TUI animations (busy spinners, the Matrix rain body, scrolling session
names, the focus-border sweep, the playbook run shimmer) are off unless the user
opts in with `animations = true` in `theme.toml`. With animations off, each
effect holds a static frame, so a repaint with no real state change writes no
cells. Every frame is wrapped in synchronized output (DEC mode 2026) whether or
not animations are on.

## Reason

Constant 40–120 ms repaints read as flicker on terminals such as foot, which
can display a frame before it is fully written. Synchronized output stops
half-drawn frames; a static default removes the churn entirely.

## Consequences

New looping effects must check the animations setting and fall back to a
static frame. Short one-shot transitions (layout slides, reveal fades) are not
covered and still play. The rain panel mode shows the token meter while
animations are off, since the rain has no static form.

## Non-Goals

Not a reduced-motion mode for one-shot transitions, and not a frame-rate cap.
