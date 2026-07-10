# 0080-lineage-preview-on-harness-label

Status: accepted
Date: 2026-07-09
Area: tui
Scope: The pane title bar's harness label doubles as a hover/click trigger for a small, session-attached preview of that session's fork/subagent lineage.

## Decision

A session that has fork/subagent lineage to show (it was itself forked from a
parent, or it has at least one live fork/subagent descendant) gets an
additional behavior on its pane title bar's existing harness label (the
right-aligned harness name in `apply_pane_title_right_cluster`): hovering it
reveals a small, read-only preview box anchored to that session's own pane,
rendering the same tree data the `C-x q` / `q` lineage popup shows. Clicking
the label toggles a persistent pin, keeping the preview open regardless of
hover. Ordinary sessions with no lineage get no hit-rect on the label at all
— it renders exactly as it always has, with no hover/click behavior and no
visual change.

This preview is intentionally a second, lighter-weight presentation of the
same data the modal shows, not a replacement:

- **The modal (`C-x q` / `q`) is unchanged.** It stays the full-screen,
  fully interactive dialog — keyboard navigation, merge/discard, jump-in.
- **The preview is session-attached, not a global dialog.** It renders next
  to the specific session's own pane the same way a sticky widget's
  hover/pinned body attaches to its owning pane — never a large, centered,
  screen-wide overlay independent of which pane it belongs to.
- **The preview is read-only.** No keyboard navigation, no merge/discard, no
  row selection in v1. `C-x q` / `q` remains the only way to act on the
  graph.
- **Tree construction and row formatting are reused, not duplicated.** The
  preview calls the same `crate::lineage::build_tree`/`flatten` and the same
  per-row rendering the modal uses, so the two surfaces can never drift into
  showing different shapes for the same data.

Visibility, hover-grace, and pin state mirror the shape of the (separately
deprecated, spec 0003) session-widget hover/pin system —
`{session_id, until}` hover state with a short grace period so the pointer
can travel from the trigger down onto the preview body without it
vanishing, and pinned-OR-unexpired-hover as the combined visibility rule —
but as independent state, not a dependency on that system.

## Reason

The full lineage graph (spec 0079) answers "what's this session's fork/
subagent shape" in detail, but opening it requires a deliberate keystroke and
takes over the whole screen. Most of the time a user just wants a quick
glance — "does this session have any lineage, and what does it look like" —
without leaving whatever they're doing. Putting that glance on the harness
label (an element already sitting in the title bar, already inert on
ordinary sessions) gives ambient discoverability for free: the label simply
starts reacting to the mouse exactly where a user's eye already rests, only
on the sessions where there's something to show.

## Consequences

- The title bar's right cluster keeps its existing width and elements
  (widgets, harness label, close button) — this feature adds no new column
  to the cluster, only new behavior on the harness label's existing span.
- `LayoutSnapshot` gained `harness_label_hits` (populated only for sessions
  with lineage) and `lineage_preview_area` (the last-rendered preview box),
  following this codebase's per-frame hit-rect convention.
- `App` gained independent `lineage_preview_hover` /
  `lineage_preview_pinned` state, mirroring but not reusing the session-
  widget hover/pin fields.
- A future removal of the session-widget system (spec 0003) does not need to
  touch this feature — it was built to mirror that system's shape, not
  depend on its code.

## Non-Goals

- Does not change what counts as fork or subagent lineage, or how the graph
  is built/capped (spec 0078, spec 0079 still govern that).
- Does not add keyboard navigation, merge/discard, or row-click-to-jump to
  the preview — those remain modal-only in this iteration.
- Does not change the modal's own behavior, keybindings, or rendering in any
  way.
