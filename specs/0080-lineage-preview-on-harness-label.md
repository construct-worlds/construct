# 0080-lineage-preview-on-harness-label

Status: accepted
Date: 2026-07-09
Area: tui
Scope: The pane title bar's harness label doubles as a hover/click trigger for a small, session-attached preview of that session's fork/subagent lineage, and that preview can itself be given keyboard focus to navigate, merge, discard, and jump — there is no separate full-screen lineage dialog.

## Decision

A session that has fork/subagent lineage to show (it was itself forked from a
parent, or it has at least one live fork/subagent descendant) gets an
additional behavior on its pane title bar's existing harness label (the
right-aligned harness name in `apply_pane_title_right_cluster`): hovering it
reveals a small preview box anchored to that session's own pane, rendering
the same tree data a fork/subagent lineage graph shows (edge glyphs, status,
and activity stats — see "Activity stats are per-segment, not per-node"
below). Clicking the label toggles a persistent pin, keeping the preview
open regardless of hover. Ordinary sessions with no lineage get no hit-rect
on the label at all — it renders exactly as it always has, with no
hover/click behavior and no visual change.

This preview is the ONLY lineage UI. An earlier iteration of this feature
(spec 0079) had a second, architecturally distinct surface — a full-screen
`C-x q` / `q` modal with its own global `App` slot — presented as the
"real" interactive view while this preview stayed read-only. That modal has
been deleted. Its interaction vocabulary was ported onto the preview itself
rather than kept as a second surface, because a session's lineage is
inherently a per-session concern; a global dialog for it was an unnecessary
architectural split, not a distinct need.

### Two visibility states, two label intensities

- **Hover-only** (the pointer is over the label, or has been recently — a
  short grace period lets it travel down onto the preview body without the
  preview vanishing mid-move): the "light" tier. The label bolds in a
  hover-flash color.
- **Pinned** (toggled on by a click, persists until unpinned): the "strong"
  tier. The label bolds in the accent color AND gains `Modifier::REVERSED`
  (inverts fg/bg) — this codebase's established convention for an emphatic,
  persistently-active interactive-text cue (the same modifier the
  action-link/URL hover styling uses, applied here to the *persistent*
  state rather than a hover flash). The two states must read as visibly
  different intensities, not just different hues, since the pinned state
  means "this stays here" while hover means "this is a transient glance."

Visibility itself (pinned-OR-unexpired-hover) and the hover-grace timing
mirror the shape of the (separately deprecated, spec 0003) session-widget
hover/pin system, kept as independent state rather than a dependency on it.

### Keyboard focus

A visible preview can be given keyboard focus two ways:

- Clicking inside the preview's body (its rows/content area — not the
  harness label itself, which only ever toggles the pin).
- Pressing `C-x Tab` on the selected session (a no-op if it has no lineage
  to show, same gate the harness label's own hover/click affordance uses).
  A second press on the same session's preview closes it — un-pins and
  clears focus, a single-keystroke open/close toggle.

Either entry path also pins the preview open if it wasn't already —
focusing something implies wanting to keep interacting with it, so a
preview about to auto-hide from a hover timeout shouldn't vanish out from
under active keyboard interaction.

While a preview holds focus, it owns the keyboard for its own vocabulary —
the same vocabulary the deleted modal used to own exclusively:

- `j`/`k`/arrows/`C-n`/`C-p`: move the row selection.
- `Enter`: jump into the selected session (a *merged* fork jumps to its
  parent instead, since the merge point in the graph and the transcript
  message it injected are the same event — spec 0078). Jumping in also
  clears both focus and the pin for this preview: leaving to go work in
  that session means the preview has served its purpose.
- `m` / `d`: merge or discard the selected row, via the exact same
  merge/discard path the `C-x m` minibuffer menu uses — a direct-key
  shortcut for it, not a second implementation.
- `Esc`: clears focus ONLY. It does not un-pin. A preview the user
  explicitly pinned stays visible after they're done navigating it — Esc
  backs out one level (stop owning the keyboard), it doesn't dismiss
  unrelated state, matching what Esc means everywhere else in this UI.

Any other key clears focus and is reported as unhandled, so the caller
re-dispatches the SAME keystroke through ordinary routing — the same
"a closing overlay never eats a live keystroke" rule other dismissable
overlays in this UI follow (e.g. `/configure`), so `C-x C-c` still quits
and `C-x b` still switches sessions while a preview is focused.

Tree construction, row formatting, and the merge/discard action are all
reused, never duplicated: the preview calls the same
`crate::lineage::build_tree`/`flatten`, the same per-row rendering, and the
same `App::apply_fork_merge` any other merge/discard path in this UI uses.

### Border reacts to focus

The preview's own border uses the same focus-reactive style any other pane
border does (`theme.border_focused` vs. the dimmer `theme.border`) —
`theme.border_focused` while this preview holds keyboard focus, `theme.border`
otherwise (whether it's showing via hover, pin, or both). This reuses the
existing theme colors rather than inventing a third lineage-specific hue,
and gives a focused preview the same "this pane owns your keystrokes" visual
language every other focused pane already uses.

### Activity stats are per-segment, not per-node

Activity stats (message/turn count, elapsed time) are rendered as separate,
non-selectable annotation rows on the rail — positioned BETWEEN the markers
that bound them — rather than attached to each node's own line. The markers
on a node's own timeline are: its own creation, each fork child's fork-out
point, each fork child's merge-back point (only when it actually merged —
a discard doesn't inject anything into the parent's transcript, so it isn't
a boundary), and "now" (or the node's own terminal point, if it has one).
Each gap between consecutive markers becomes one segment row describing
what happened in exactly that window, e.g.:

```
◆ ● claude — auth-refactor
│   12 msgs · 8m12s
│ ⑂ ● claude — fork idea A  ↩ merged
│ │   2 msgs · 1m05s
│   5 msgs · 3m40s
│ ⑂ ● claude — fork idea B
  │   1 msg · 30s
```

A node's own line no longer carries stats at all — it's rail + edge glyph +
status glyph + harness [+ title] [+ merged/discarded marker], nothing more.

The rail itself (the `│` columns on the left) is a pure vertical-line
connector, not a `git log --graph`-style tree with `├─`/`└─` branch
corners — it mirrors the `:::timeline` markdown extension's rendering
convention instead (same one-column-per-nesting-level `│` used elsewhere
in this UI for checklists), on the reasoning that a node's own edge glyph
(`◆`/`⑂`/`▸`) already marks "a branch starts here", so a second, redundant
corner glyph on the rail added visual noise without adding information.
Whether a column keeps drawing `│` below a given row, or goes blank, is
carried by that row's ancestors' `is_last` — a sibling that isn't the last
one (e.g. "fork idea A" above, with "fork idea B" still to come) reads
identically to one that is; the difference only shows up one level down, in
whether ITS children's rail carries a `│` in that column or not.
A childless node still gets exactly one segment (its whole life, start to
"now" or to its own terminal point), so every node's activity ends up
visible somewhere, not just nodes with forks. A window with zero messages
in it is skipped entirely rather than rendered as a "0 msgs" line.

This is possible without any extra fetch because `SessionSummary::event_count`,
`ForkedFrom::transcript_seq`, and `ForkMerge::merged_seq` are all the same
counter (the transcript's own sequence number) — a child's
`forked_from.transcript_seq` is a precise, already-in-memory snapshot of the
parent's position at fork time, and `ForkMerge::merged_seq` (stamped by the
daemon from the parent's own `event_count` at the moment of merge) is the
same for the merge-back point. Segment math is therefore plain arithmetic
over data already on `SessionSummary`, computed fresh on every render from
live session state — never a stored/cached total.

Subagent children (spec 0014) don't stamp a parent-timeline position the
way forks do, so they don't act as boundary markers; they're simply
recursed into in place without splitting the parent's timeline.

## Reason

A session's fork/subagent lineage is a per-session fact, not a
fleet-wide or otherwise global one — asking "what's this session's lineage
shape, and can I act on it" never has a reason to open a screen-centered
dialog disconnected from the session's own pane. The original two-surface
design (read-only preview + a separate fully-interactive modal) existed
because the preview shipped first as a lighter-weight glance and the modal
predated it as the only interactive surface; once the preview existed,
maintaining two places that both render the same tree — one visually
anchored to the session, one not — was duplication with no upside. Folding
the modal's interaction vocabulary into the preview keeps exactly one
lineage UI: ambient and glanceable by default (hover), pinnable for a longer
look, and keyboard-interactive on demand without ever leaving the session's
own pane.

## Consequences

- The title bar's right cluster keeps its existing width and elements
  (widgets, harness label, close button) — this feature adds no new column
  to the cluster, only new behavior on the harness label's existing span.
- `LayoutSnapshot` gained `harness_label_hits` (populated only for sessions
  with lineage), `lineage_preview_area` (the last-rendered preview box, used
  to swallow stray clicks so they don't fall through to the pane
  underneath), and `lineage_preview_body_hit` (the rows/content area alone,
  tagged with its owning session — clicking here enters focus).
- `App` gained `lineage_preview_hover` / `lineage_preview_pinned` (mirroring
  but not reusing the session-widget hover/pin fields) plus
  `lineage_preview_focused` and its selection/scroll state for the
  keyboard-focused mode.
- There is exactly one `KeyAction` for keyboard entry into lineage
  (`C-x Tab`, both keymap profiles) instead of a dedicated fork-log-popup
  action — a single compound chord toggles the whole interactive
  experience on and off.
- A future removal of the session-widget system (spec 0003) does not need
  to touch this feature — it was built to mirror that system's shape, not
  depend on its code.
- `ForkMerge` (protocol) gained `merged_seq: u64`, stamped by the daemon
  from the parent's `event_count` at merge time — the parent-timeline
  counterpart to `ForkedFrom::transcript_seq`, and the one piece of data
  segment rendering needed that wasn't already on `SessionSummary`.
- The lineage row model gained a non-selectable `Segment` row kind,
  interleaved into the flattened row list at the correct points alongside
  node rows and the existing "+N more" collapse marker.

## Non-Goals

- Does not change what counts as fork or subagent lineage, or how the graph
  is built/capped (spec 0078 still governs that; spec 0079's tree-
  construction rules are unchanged, only its "second global dialog"
  delivery mechanism was removed).
- Does not add a docked/always-visible panel — the preview is still
  hover/pin/focus-triggered, never rendered unconditionally.
- Does not change what merge/discard or jump-in DO (spec 0078 governs
  those); it only changes where the keys that trigger them live.
- Does not attribute cost (`SessionSummary::cost_usd`) to individual
  segments — it's a single cumulative total with no per-checkpoint snapshot
  the way `event_count` has, so it was dropped from the lineage view
  entirely rather than approximated or misattributed to one window.
