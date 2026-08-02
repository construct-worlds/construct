# 0178-list-row-click-selects-before-toggling

Status: accepted
Date: 2026-08-02
Area: tui
Scope: A click on a whole session-list row that also has a toggle (project headers, archived disclosure rows) selects it first; only a click on the already-selected row toggles it.

## Decision

Session-list rows whose entire width acts as a toggle — project/group headers
and "N archived" disclosure rows — require the row to already be selected
before a click toggles it:

- Click on a row that is **not** the current selection: move the selection to
  that row. Nothing expands, collapses, or reveals.
- Click on a row that **is** the current selection: perform the row's toggle
  (collapse/expand the project, reveal/hide the archive).

This applies only to full-row toggles. Dedicated hit targets that occupy a
specific column — a session's disclosure marker for its nested children, a
service's children marker — keep toggling on the first click, because aiming
at them is already an unambiguous statement of intent.

Keyboard toggles are unaffected: arrow/collapse-expand actions operate on the
current selection, which by definition is already selected.

## Reason

Selecting a project and folding a project are different intents, and the row
was serving both from one gesture. Reaching for a project header with the
mouse — to select it, act on it, or just look at it — collapsed it as a side
effect and shifted every row below out from under the cursor, so the next
click landed on something the user never aimed at. Requiring the row to be
selected first makes the destructive-looking half of the gesture deliberate
while costing one extra click only the first time.

## Consequences

- Toggling an unselected group is a two-click gesture; this is accepted.
- Selection and toggling must stay distinguishable per row: any new full-row
  toggle added to the list follows the same select-then-toggle rule rather
  than firing on first click.
- A row's toggle must be cheap to reach once selected — the second click is
  on the same coordinates, so the toggle must not move the row it lives on.

## Non-Goals

- Does not change what a click does on ordinary session or service rows,
  which only ever select.
- Does not introduce double-click semantics: the two clicks are independent
  and unconstrained by timing.
