# 0121-client-grid-matches-pty-geometry

Status: accepted
Date: 2026-07-27
Area: webui
Scope: Agreement between a client's local terminal grid and the geometry the child process believes it has.

## Decision

A client must never render a PTY at a geometry the child has not been told
about. Whenever a client changes its own terminal grid, it either propagates
that geometry to the child or adopts the child's geometry instead. The two must
not be allowed to drift apart and stay apart.

Concretely, a client has three legitimate options when its viewport changes:

1. **Own the geometry** — resize the local grid and send the resize onward.
   Ownership transfers only on explicit engagement under spec `0153`; a
   passive measurement cannot take it.
2. **Defer** — resize the local grid and send the resize after a settling
   delay, coalescing rapid changes into one notification.
3. **Follow** — decline to own the geometry and render at the size the owning
   client established, rather than at the local fit.

Silently resizing the local grid without ever choosing one of these is not an
option. When following is impossible because the child's screen is larger than
the client can display, the client clamps and accepts a visible mismatch rather
than clipping the newest output.

Two rules qualify these options:

- **A follower still reports.** Choosing to follow does not exempt a client
  from telling the daemon its measured viewport as a passive, non-claiming
  report. The daemon remembers it per connection, so this client's next
  explicit engagement applies the correct size immediately — and if the
  connection in fact still owns the geometry (nobody else claimed since its
  last engagement), the passive report reaches the child and the follower is
  told, through the ordinary resize event, to render its own fit. An owner
  has no one to follow; silently clamping to its own stale size corrupts
  rendering with only one client attached.
- **A claim needs a real measurement.** A client whose local grid cannot be
  measured yet (a host still hidden while history hydrates, a pane
  mid-layout) must not claim geometry with the garbage fit it would read.
  It defers the claim until the grid has real dimensions, then resumes it.

Every passive rendering surface of a session follows the same rule as the
focused view. A mirrored or split pane showing a session it does not own
renders the owner's grid, not its own pane fit.

## Reason

Terminal applications position output relative to a grid they believe they
have. A footer repainted with relative cursor moves, a scroll region, or any
absolute cursor address is computed against the child's row and column count.
A client rendering the same byte stream into a differently-sized grid scrolls
at a different line than the child expects, so those repaints land on the wrong
rows: status lines duplicate instead of overwriting, and output appears below a
prompt that should be the last thing on screen.

This is easy to introduce accidentally, because fitting the local grid and
notifying the child are separate steps. Any code path that performs the first
and then returns early — to avoid churning the child on incidental layout
changes such as a composer text area auto-growing, or to avoid stealing
geometry from another attached client — creates a permanent disagreement. The
cost of that disagreement is corrupted rendering that persists until something
else happens to produce a real resize, which makes it look intermittent and
unrelated to its cause.

Suppressing a notification is therefore the wrong tool for avoiding churn.
Deferring achieves the same goal — one notification instead of one per
keystroke — without ever leaving the two grids out of sync.

## Consequences

- Incidental local layout changes cost one coalesced resize after they settle.
  Children that repaint fully on resize will repaint once; that is accepted as
  the price of correct rendering.
- A passive viewer renders at the owning client's size, so it may show blank
  space beside or below the child's screen rather than filling its viewport.
  Filling the viewport is not worth a corrupt grid.
- A viewer smaller than the child's screen remains a known-imperfect case. It
  must degrade by clamping, never by clipping the newest output.
- Any new early return between fitting a grid and notifying the child is a
  regression in this decision and must instead defer or follow.

## Non-Goals

- This does not require every client to have identical geometry. Only that a
  client's rendered grid and the geometry it has told the child about agree.
- Which client connection wins is governed by explicit engagement under spec
  `0153`.
