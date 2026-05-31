# 0012-clients-render-shared-pty-at-its-true-width

Status: accepted
Date: 2026-05-30
Area: cross-client
Scope: How a client renders a PTY-backed session whose width is currently owned by a different client.

## Decision

A PTY-backed session has a single geometry (columns × rows) at any moment, owned by whichever client most recently took input ("active wins"). A client whose own viewport is narrower than the owning geometry renders the terminal at the session's true column count and lets the user scroll to reach the overflow — it does not reflow or wrap the output to its own width. When a client takes over input it claims geometry at its own size, as before.

Clients learn the live geometry from the daemon whenever it changes — a resize is announced to attached clients — not only at attach time.

## Reason

A single PTY emits content laid out for its current width. If a passive viewer re-wraps that content to a narrower width, column-aligned and full-screen output (tables, code, TUIs) becomes garbled, and two clients viewing the same session show materially different, wrong-looking screens. Preserving the true width keeps every viewer faithful to what the program actually drew; scrolling is a lossless way to fit a wide screen into a narrow viewport.

## Consequences

- The daemon must announce PTY geometry changes to attached clients, not just report size at attach. This is carried by a transient, non-persisted resize signal in the session event stream.
- A passive client tracks the session's current geometry and renders at the true width; it only resizes the shared PTY when it becomes the active input owner.
- Wide content is reached by scrolling, accepting a smaller effective viewport rather than reflowed text.
- Consumers that exhaustively handle session events must tolerate the new transient resize signal (ignoring it where geometry is irrelevant).

## Non-Goals

- This does not mandate a particular fit strategy. A viewer may scroll or scale; the rule is "do not wrap to a narrower width," not "scale to fit."
- It does not address the symmetric height case (a taller owning geometry). Rows may stay at the viewer's local fit; only wrapping (width) is in scope.
- It does not change the "active wins" geometry-ownership policy.

## Examples

A wide desktop terminal and a narrow phone web client view the same session. The desktop is active, so the PTY is wide. The phone shows the full-width terminal with a horizontal scrollbar instead of wrapping each line. When the phone user types, the phone claims geometry and the terminal reflows to the phone's width for both clients.
