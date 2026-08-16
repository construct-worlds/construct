# 0201-split-pane-materialization-transitions

Status: accepted
Date: 2026-08-16
Area: tui
Scope: Split-pane creation and removal use a brief Construct-inspired visual transition without delaying the layout change.

## Decision

When a split pane is created, its final geometry and contents become live
immediately beneath a short code-veil materialization effect. When a pane is
removed, the surviving layout expands immediately and a collapsing digital
residue briefly occupies the removed pane's former bounds.

The transition is purely composited presentation state. Focus, keyboard and
mouse input, shared-layout publication, hit targets, and PTY sizing all use the
new topology from the first frame; none waits for the effect to finish.

Topology changes received from another client use the same effect when the TUI
can identify added or removed pane identities. The animation uses the active
theme's background and Matrix palette and completes in well under half a
second.

## Reason

Splitting and closing panes are spatial changes that otherwise snap between two
dense terminal layouts. A brief materialization cue makes the new or departing
region legible and gives Construct a distinctive Matrix-inspired motion
language. Keeping the topology authoritative throughout avoids dropped input,
stale hit testing, PTY resize churn, and delayed cross-client synchronization.

## Consequences

- A new pane may be visually obscured for a few frames, but it is already the
  focused, interactive pane.
- A removed pane's residue is drawn over the surviving layout; no terminal
  contents or frame snapshot need to be retained after deletion.
- Animation state is local and ephemeral. It is never persisted or included in
  the shared layout document.
- If the old pane had no measured on-screen bounds, removal still happens
  normally and simply has no residue to draw.

## Non-Goals

Focus changes, divider resizing, session switches within an existing pane, and
zoom transitions do not use this pane-topology animation.
