# 0188-attach-renders-a-screen-not-a-replay

Status: accepted
Date: 2026-08-03
Area: protocol
Scope: Clients that cannot hide a raw-history replay attach to a PTY session via a server-rendered screen snapshot instead.

## Decision

The daemon can render a PTY session's terminal server-side — by feeding a
bounded tail of the session's persisted PTY history through its own native
terminal emulator at the session's real geometry — and serve the result as
a compact escape-sequence stream: scrollback rows first, then a
visible-screen repaint, then cursor position, attributes, scroll region,
and input modes. A client writes that stream once into a freshly reset
terminal of the same size and is caught up.

Clients whose terminal renders progressively while consuming bytes (the
web UI's xterm.js) attach through this snapshot rather than replaying raw
history. Raw history stays available through the byte-range replay RPC:
paging into history older than the snapshot re-fetches the span the
snapshot rendered plus an older page and rebuilds from raw bytes.

The snapshot can be rendered with alternate-screen switch sequences
stripped from the history first, for clients that apply the same filter to
every byte they feed their terminal. The render must mirror the client's
filter so snapshot-then-live and replay-then-live converge on the same
screen.

A snapshot is only rendered at the session's last known PTY geometry.
When the daemon does not know the child's size, the RPC fails and the
client falls back to raw replay — a snapshot rendered at a guessed width
would wrap every line wrongly, which is worse than a visible replay.

## Reason

Raw-history attach cost scales with how much the session has ever printed;
snapshot attach cost scales with the screen and retained scrollback. The
native TUI hides its replay because it parses bytes into an in-memory grid
and paints one frame; xterm.js parses and paints incrementally, so the
same replay is visible to the user and was papered over with loading
overlays and offscreen hydration. Rendering server-side uses the same
trick the TUI uses — parse invisibly, paint once — and additionally stops
shipping megabytes of history to remote/mobile clients that only need the
current screen.

## Consequences

- The scrollback a snapshot carries is bounded. The result must say when
  rows were dropped so the client can still offer older history, and raw
  byte-range replay must remain a supported attach path (fallback for old
  daemons, unknown geometry, and history paging).
- A snapshot is a reconstruction, not the child's authoritative screen —
  the same trust level as a client-side replay of the same bytes. The
  existing post-attach force-redraw (the resize bump that makes the child
  repaint) remains the authority and must stay in place.
- The serialized stream must stay valid to feed a stock terminal emulator
  at the stated size with no client-side interpretation: plain escape
  sequences, scrollback flowed so the receiving terminal retains it,
  soft-wrapped rows left to the receiving terminal's autowrap so reflow
  and selection keep working.
- The server-side emulator's fidelity bounds snapshot fidelity: state the
  upstream dump omits (scroll region, origin mode) must be restored
  explicitly, and future terminal features consumed by harness TUIs may
  need the same treatment.

## Non-Goals

- Replacing the byte-oriented replay RPC. History paging, previews at
  arbitrary sizes, and clients with invisible replay (the native TUI) keep
  using raw bytes.
- Guaranteeing pixel-perfect equivalence with a full replay. The snapshot
  trades unbounded-history fidelity for bounded attach cost; the child's
  own repaint is the corrector.

## Examples

- Opening a long-running session in the web UI writes one small stream
  into xterm: the screen appears fully formed, with recent scrollback
  above it, and no loading progression is visible.
- Scrolling to the top and requesting older history converts the terminal
  to raw-replay mode: the client re-fetches the byte span the snapshot
  covered plus one older page and rebuilds, after which further paging
  extends normally.
- A session whose child never reported a size attaches the old way: raw
  replay, loading overlay, offscreen hydration.
