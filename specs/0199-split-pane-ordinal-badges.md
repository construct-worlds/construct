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

- **The pane frame**, painted over the border's top-left corner — the
  pane's geometric anchor, ahead of its title.
- **The session list**, in the first column of the row of every session
  currently visible in a pane.

Badges replace an existing cell rather than adding one: no column in the
list (marker, glyph, title, harness) and nothing in the pane title bar
moves when a badge appears or disappears. On a depth-0 list row the badge
takes the reserved marker-gutter cell; on a nested row it takes the first
rail/indent cell and the marker survives. On the pane it takes the corner
border cell.

The corner badge is painted after the pane renders, independent of what the
pane shows: session, service, project, empty pane, and transient overlays
all wear their number the same way.

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

## Selecting a pane by its badge

The badge is also an address. One rule everywhere: **digit N goes to the
pane wearing badge N; digit 0 goes to the session list.**

- `Alt+digit` is the accelerator in both keymap profiles — it matches the
  emacs window-numbering convention and, unlike `Ctrl+digit`, arrives in
  every terminal (legacy encodings deliver it ESC-prefixed).
- `Ctrl+digit` is the same jump for terminals with keyboard-enhancement
  disambiguation (see the terminal-keyboard-disambiguation spec). This
  REPLACES the earlier scheme where `Ctrl+1` meant the session list and
  `Ctrl+2` the first pane: once badges were painted, that off-by-one read
  as a bug. The list jump moved to digit 0.
- The vim profile additionally binds the digits behind its window-command
  prefix chord, same numbering.
- Clicking a badge — on a pane's corner or in a list row's first cell —
  focuses that pane. In the list this outranks the disclosure toggle the
  badge may be covering.

Pane digits 1..9 act only while badges are on screen (two or more panes);
with a single pane they fall through, so a focused child terminal
application keeps its own Alt/Ctrl+digit bindings (emacs prefix arguments
chief among them). Digit 0 is the always-available list jump.

## Non-Goals

- Service and project rows in the session list are unbadged (their panes
  are, via the corner badge). The numbering already reserves their slots,
  so badging their rows later is additive.
- The web UI does not mirror the digit accelerators (browsers reserve
  modifier-digit combinations for tab switching); its pane-focus story is
  its own.
