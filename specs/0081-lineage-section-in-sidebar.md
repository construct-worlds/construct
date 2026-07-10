# 0081-lineage-section-in-sidebar

Status: accepted
Date: 2026-07-10
Area: tui
Scope: Fork/subagent lineage renders as a collapsible section of the left sidebar — between the session rows and the operator panel — showing the selected session's tree; there is no floating per-pane lineage widget.

## Decision

The sidebar stacks three regions top to bottom: the session rows, the
lineage section, and the operator panel. The lineage section renders the
SELECTED session's fork/subagent tree — the same tree data, diagram modes
(boxed-lane and compact rails, toggled from the section header, compact
being the default for the narrow column), turn-info stats, and row
interaction vocabulary the earlier per-pane preview had (spec 0080,
superseded). It is a master–detail pattern: the section follows the list
selection like a detail panel, with the selected session's node highlighted
in the diagram.

Visibility is automatic, not user-armed: the section appears whenever the
selected session has lineage to show (more than its own lone node) and
disappears entirely otherwise — no hover trigger, no pin, no per-session
open state. A click on its one-row header collapses it to just that header;
the collapse state and view mode persist across launches (collapse is
global, not per session). The section never squeezes the session rows below
their minimum height and never takes more than half the rows region — a
deep tree scrolls (vertically and horizontally) inside the section instead
of crowding the list out.

Keyboard focus:

- Bare `Tab`, while the list pane holds focus, switches keyboard focus
  between the session rows and the lineage section (in both directions).
  It is an intercept scoped to list-pane focus, NOT a global keymap
  binding — view-focused PTY sessions keep receiving Tab (terminal
  completion is untouched), and a list with no lineage section leaves Tab
  meaning nothing.
- `C-x Tab` toggles the section's focus from anywhere, expanding a
  collapsed section on entry.
- While focused, the section owns the row vocabulary carried over from the
  preview: `j`/`k`/arrows/`C-n`/`C-p` move the node selection, `Enter`
  jumps into the selected session (a merged fork jumps to its parent, spec
  0078), `m`/`d` merge or discard via the same path as `C-x m`, `Esc`
  backs out to the session rows. Any other key clears focus and re-enters
  ordinary routing with the same keystroke.
- Focusing the section counts as sidebar (list-pane) focus; jumping into a
  session moves focus to the view pane.

Mouse: clicking the header collapses/expands, clicking the header's mode
toggle switches diagram modes, clicking a session's box jumps to it,
clicking anywhere else in the body focuses the section, the wheel scrolls
it (shift for horizontal), and clicking outside blurs it. There is no
drag-resize: the section sizes itself to content within its caps.

Relatedly, the session list itself shows lineage structure ambiently: a
fork renders as an indented child row under its parent (like subagent
rows) — recursively, so a fork of a fork still appears — never as its own
top-level row, and a parent row badges its live fork count. The section provides the temporal detail (fork/merge order,
per-window message counts and compute time) that list rows cannot.

## Reason

The preview's hover/pin trigger on the harness label was hard to discover
and carried heavy machinery (hover grace timers, per-session pin state,
floating-box anchoring, drag-resize) for what is conceptually a detail view
of the selected session. A sidebar section is always discoverable, needs no
arming gesture, and gives lineage a stable home that doesn't overlap pane
content — eliminating the click-through/forwarding conflicts a floating
overlay had with mouse-grabbing PTY children.

Folding lineage into the session list itself was considered and rejected:
the list is a user-ordered, live surface (manual positions, groups,
pinning, archived hidden) while a lineage tree is time-ordered history
(merged/discarded forks, turn-info rows that aren't sessions). Mixing them
in one scroll region forces conflicting ordering/filtering rules and
complicates row hit-testing and reordering. Keeping two stacked surfaces
with separate rules — plus lightweight fork indentation in the rows —
captures the benefits of both.

## Consequences

- Lineage is per-selection, not per-session-pane: there is exactly one
  lineage surface, showing the selected session's tree. Features that need
  lineage for a non-selected session must select it first.
- The section's visibility is derived state; nothing persists per session.
  Only the global collapse flag and view mode persist.
- Bare Tab in the list pane is now taken. Future bindings must not claim
  it, and the sessions⇄lineage switch must stay scoped so PTY sessions
  keep receiving Tab.
- The sidebar's vertical space is shared three ways; the section's caps
  (half the rows region, list minimum preserved) must survive future
  layout changes.
- The diagram renderer, tree construction, and merge/discard actions
  remain shared single implementations (`crate::lineage`,
  `apply_fork_merge`) — the section did not fork them.

## Non-Goals

- No lineage rows interleaved into the session list beyond simple fork
  indentation.
- No per-session pin/open state and no floating, pane-anchored lineage
  surface.

## Examples

- Selecting a session with two forks shows the section under the rows;
  selecting a session with none removes it entirely.
- Pressing Tab with the list focused moves focus into the section; Enter
  on a fork's node jumps to that fork and focuses the view pane.
- Collapsing the section leaves a single `▸ ⑂ lineage` header row; the
  collapsed state survives a TUI restart.
