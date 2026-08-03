# 0186-webui-lineage-and-token-meter

Status: accepted
Date: 2026-08-02
Area: webui
Scope: The web UI sidebar gains the selected-session lineage section and the ambient operator panel's fleet token meter, matching the TUI's sidebar composition.

## Decision

The web client's left sidebar stacks the same durable regions the TUI
does: session rows, then a collapsible **lineage** section, then a
collapsible **operator** ambient panel. Services remain ordinary list
rows (as they already are on the web).

### Lineage section

The lineage section is a master–detail panel for the *selected* session
(spec 0081):

- It appears when the selection has at least one recorded message, or
  when the selection has fork/subagent lineage beyond a lone node.
- It disappears for empty sessions with no lineage.
- The tree is rooted at the topmost fork/subagent ancestor reachable
  from the selection, then rendered downward with forks always shown
  and subagent children collapsed behind a per-parent toggle by default.
- Clicking a node selects that session; clicking the subagent toggle
  expands or collapses that parent's group.
- Collapse state and section height persist in the browser across
  reloads (global, not per session).

The web layout is an interactive tree rather than a cell-grid boxed-lane
or rails diagram. The data model, visibility rules, edge kinds (fork /
subagent / reset-snapshot), and subagent-group collapse match the TUI;
the presentation uses HTML tree rows so the section remains usable on
touch and narrow viewports.

### Operator ambient panel

The ambient panel sits at the bottom of the sidebar and hosts the same
named body modes as the TUI (spec 0019):

- **tokens** (default) — fleet-wide realtime token-usage history
  grouped by model (spec 0167).
- **rain** — a decorative matrix-rain body.

The panel opens expanded on first run. Collapse state, height, and mode
persist in the browser. Switching modes never collapses the panel.

### Token meter feed

The web token meter is built from the same sources as the TUI:

1. Seed on connect from `usage.token_history` so history that accrued
   while no browser was open is not a hole.
2. Live `session/event` Cost reports from every session (not only the
   focused one), attributed report-model → session model →
   `unattributed`.
3. Compute-time deltas from each model-backed session's `busy_ms` /
   `busy_running_since_ms` for legend rates.

Cached prompt tokens shade a model's band rather than growing column
height. Rates are tokens per second of compute, summed across models for
fleet throughput. An empty meter states that nothing has been reported.

## Reason

Users running the web client had no way to inspect fork/subagent
structure for the selected session, and no ambient view of fleet token
throughput — both of which the TUI surfaces continuously in the sidebar.
The daemon already exposes the required session fields and token history;
the gap was client presentation.

## Consequences

- Future web sidebar work must preserve the stack order: list → lineage
  → operator, and must not drop Cost events that arrive for unfocused
  sessions.
- The lineage section may later adopt the full boxed-lane diagram
  without changing the data rules or visibility gates defined here.
- Dollar cost and per-session breakdown remain non-goals for this panel
  (spec 0167).

## Non-Goals

- Porting the TUI's cell-grid boxed-lane / rails glyph layout to the web.
- Operator monolog typewriter overlay or widget-viewport indicators
  (spec 0019's transient widget overlay) on the web panel.
- Keyboard focus ownership of the lineage section (`C-x Tab`, j/k, m)
  on the web; mouse/touch selection is sufficient for this decision.
