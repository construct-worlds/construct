# 0106-session-list-view-modes

Status: accepted
Date: 2026-08-02
Area: tui
Scope: The sidebar session list offers a compact one-line view and a vertically spaced full card view per session, toggled from the pane's border and persisted per user.

## Decision

The session list renders in one of two user-selectable view modes:

- **Compact** (the default): one line per session — lineage/pin markers, the
  status glyph, the session name, an attention marker, and the right-aligned
  harness label.
- **Full**: the compact line plus a muted second detail line aligned under the
  name, showing (in display order) the model and reasoning effort, a small
  context-window gauge with percentage, current activity (live busy time while
  running; otherwise a coarse age since the last chat message, so status
  rows, tool blocks, and daemon-restart resume events do not reset it —
  sessions with no messages fall back to the last recorded event), and
  lifetime token
  volume, followed by one blank separator row. Cost is deliberately excluded.
  The gauge's fill rounds to the nearest step so the bar tracks the percentage
  (just over half reads as half, not three quarters).

Rules both modes must preserve:

- The toggle is a small labeled control on the list pane's border,
  right-aligned immediately before the pane's collapse control — the same
  placement and label shape as the lineage section's full/compact toggle. The
  choice persists across launches; legacy state restores to compact.
- The detail line is COLUMNIZED: every row lays its fields into one shared
  set of columns — the context gauge leftmost (the highest-priority scan
  signal), then activity and tokens, with the identity cell (model·effort)
  right-anchored at the row edge so it stacks directly under the title
  row's right-aligned harness label and the two "what's running this"
  facts read as one group. The same field starts at the same position on
  every row and the list scans vertically; the most variable-width field
  (identity) sits where its ragged left edge cannot disturb alignment.
  Column widths are computed across the whole list, not the viewport, so
  nothing shifts while scrolling; one overlong identity is capped and
  ellipsized rather than claiming the row.
- A cell whose session lacks that datum renders a dim placeholder, holding
  the column open — position must always mean the same field. A session
  reporting no model (e.g. a plain shell) fills the identity column with
  where it lives (its worktree or working directory) instead.
- Values read one step brighter than unit suffixes and placeholders, and
  live busy time renders in the running color, giving the line an internal
  hierarchy instead of one undifferentiated style.
- Labels stay bounded: busy time switches to the coarse single-unit age
  scale past an hour, and the gauge clamps at full/100% even when a harness
  reports usage above its inferred window.
- On a narrow sidebar the detail line drops whole columns rather than
  wrapping, least important first: tokens, then activity, then identity,
  keeping the context gauge longest — and identically on every row. Full
  mode never forces the sidebar wider and never horizontally scrolls.
- A row that heads a subtree — a project header, a service — carries no
  detail line, but in full mode it gains the same closing rail/breathing row
  a session card ends with, drawing the stem down into its first member. Its
  members hang off rails; without that row those rails begin in mid-air, and
  the header alone would sit flush against its first member while every card
  around it floats. When the header is collapsed or has no members the row is
  blank, still separating it from what follows. Archived-disclosure rows head
  nothing and stay one line in both modes; no header gains a detail line, in
  either mode.
- In full mode a project header also opens on a fixed margin of blank rows,
  wider than the single row separating two sibling cards: at one row a project
  reads as another item in the stream, and what it has to say is "a new
  section starts here". The margin is NORMALIZED, not additive — it counts
  whatever blank row the item above already leaves behind (a card and a header
  leave one; an archived-disclosure row leaves none) and supplies only the
  difference, so every project in the list opens on the same amount of air
  regardless of its neighbor. A project at the top of the list has nothing to
  be set apart from and carries no margin. Compact mode packs its rows with
  nothing between them and gains no margin, and services — which head a
  subtree but not a section — do not take one.
- That margin belongs to the project it sets apart: it counts toward the
  header's measured height, so clicking the gap selects the project and
  scrolling accounts for it, but it renders as its own row rather than as
  part of the header, so selecting a project never highlights the air above
  it, and the header's first line — where its gutter affordances live —
  remains its title row.
- Session nesting uses the same two-cell indentation step in both modes. Each
  generation gets its own depth, including fork-of-fork, subagent-of-subagent,
  and mixed trees; the hierarchy never changes the user's depth-first session
  order. Parent titles render one emphasis step above leaf titles.
- Only full mode draws branch and continuation rails into that indentation.
  Its cards are broken apart by detail and spacing rows, so a child needs a
  continuous rail to still read as attached to its parent. Compact mode packs
  its rows with nothing between them, so depth alone carries the hierarchy and
  the rails would only add ink to the view whose whole purpose is density.
  Both modes indent identically, so switching modes moves no column.
- One marker cell sits between the rails and the row's content, and every row
  hanging off the tree spends exactly that one cell: a session's children
  disclosure, a fork's lineage mark, an archived-children row's own disclosure,
  or a space when a session has none of them. The cell is reserved whether or
  not it is used, so sibling rows share one left edge, a session gaining its
  first child never shifts its own title, and a child's status glyph lands
  under its parent's title. Where a session could claim the cell twice the
  disclosure wins, because it is the affordance the operator can act on. Any
  future row marker joins that column rather than reserving another.
- The web UI's session list shows the same detail line with the same
  content and omission/fallback rules, but always on — it has no
  compact/full mode pair. Its gauge may render at finer resolution than
  the TUI's cell bar (a continuous fill), since the constraint being
  mirrored is the information and its semantics, not the glyphs.
- Selection, keyboard navigation, and scrolling operate on items, not display
  rows; a click anywhere within a card selects it, while gutter affordances
  (disclosure triangle, pin target) live on the card's first line only.
  Hit-testing must consult the rendered row-to-item mapping, never assume one
  display row per item.

## Reason

The compact list answers "which session" but not "how is it doing" — model,
context pressure, activity, and spend previously required selecting each
session and reading the modeline. Scanning a fleet benefits from a denser
per-session summary, but permanently taller rows materially reduce the visible
session count, so the density is a user choice, mirroring the precedent the
lineage section set for a full/compact pair.

## Consequences

- Display rows and list items are no longer 1:1; any future list interaction
  (hover zones, drag targets, new gutter affordances) must go through the
  row-to-item mapping and declare which line of a card it lives on.
- Scroll limits and scrollbar geometry are measured in display rows, so
  mixed-height items (one-line disclosure rows and headers whose height
  depends on the item above them, among three-row cards) stay correct.
- An item's height can depend on its neighbor, so nothing may assume a kind
  of item has a fixed height, and any new spacing rule has to be expressed in
  the same measurement the hit map and scroll geometry read — spacing painted
  only at render time would desynchronize them.
- Adding new per-session data to the detail line means placing it in the
  existing drop-priority order, not appending unconditionally.

## Non-Goals

- No third, denser-still or taller-still mode; two modes keep the toggle a
  binary.
- A web-UI mode switch: the web list always shows the detail line.
- The detail line is a summary, not a control surface — it hosts no buttons.
