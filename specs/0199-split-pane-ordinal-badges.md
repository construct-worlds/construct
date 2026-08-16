# 0199-split-pane-ordinal-badges

Status: accepted
Date: 2026-08-16
Area: tui
Scope: With multiple split panes open, each pane and its session-list row wear the same number so the operator can map one to the other at a glance.

## Decision

When the main view holds two or more split panes, every pane has a 1-based
ordinal determined by layout order (the order the eye reads the splits in).
The ordinal is rendered as a single-digit badge — the digit over a highlight
background — in two places that must always agree:

- **The pane title**, immediately before the session's status glyph.
- **The session list**, in the first column of the row of every session
  currently visible in a pane.

Badges replace an existing cell rather than adding one: no column in the
list (marker, glyph, title, harness) and no offset in the pane title bar
moves when a badge appears or disappears. On a depth-0 list row the badge
takes the reserved marker-gutter cell; on a nested row it takes the first
rail/indent cell and the marker survives.

The badge's background brightness tracks focus: the pane that last held
focus (and its session's list badge) wears the selection colors; every other
pane wears the inactive highlight.

With a single pane there are no badges anywhere — a lone pane needs no
locating aid.

## Reason

Locating a split pane's session in the list (and the reverse) required
reading titles and comparing text, which gets slow with several panes and
long, similar session names. A shared number is the cheapest stable link
between the two surfaces: glanceable, unambiguous, and independent of name
collisions.

## Consequences

- Ordinals are positional: panes are numbered by layout order, and
  non-session panes (services, projects, empty panes) still occupy a slot,
  so a badge always states the pane's on-screen position. Removing a pane
  renumbers those after it; the number is a locator, not an identity.
- A session shown in more than one pane wears the focused pane's number
  when it is one of them, else the first pane's in layout order.
- List geometry invariants from the one-reserved-marker-cell rule still
  hold: a badge may repaint the content of the first cell but must never
  change any row's width or shift a column.
- While a splittable session with children is on screen, its disclosure
  glyph is covered by the badge; expand/collapse remains operable from the
  row. Any future change that makes the disclosure cell the only way to
  expand must give the badge a different home.
- Ordinals past 9 are not rendered (the row falls back to its plain
  lead-in) so the badge can never widen beyond one cell.

## Non-Goals

- No badge on service pane titles yet; service list rows are likewise
  unbadged. The numbering already reserves their slots, so adding their
  badges later is additive.
- This does not renumber or alter the existing pane-cycling / focus-by-index
  keybindings; the badge is a visual locator only.
