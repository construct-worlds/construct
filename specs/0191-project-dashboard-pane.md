# 0191-project-dashboard-pane

Status: accepted
Date: 2026-08-04
Area: tui
Scope: Selecting a project header in the session list shows a live project dashboard in the main view pane instead of a passive flat member list.

## Decision

When the TUI selection is a **project** (organizer header), the main view pane
renders a **project dashboard** scoped to that project's members. The dashboard
is an observation and hand-off surface, not a second session workspace.

### Layout

Top to bottom, dropping lower-priority regions when the pane is short:

1. **Header** — member count, project-scoped tally (working / wants you /
   errored, same ranking and glyphs as the fleet tally), lifetime token total
   when non-zero, and a dominant cwd line. Project identity is not repeated
   here: the main pane title bar already shows `project: {name}`.
2. **Token meter** — project-scoped throughput history fed from the same
   `Cost` / busy-time path as the fleet meter, filtered to members of this
   project. Idle projects show a quiet empty line rather than a blank grid.
3. **Members | activity** — two columns when wide enough; members alone when
   narrow.
4. **Preview** — chrome for the target session (name, state, model/identity,
   context gauge, activity) plus up to a few recent chat messages when the
   client has cached them.

### Members

- Only active, Construct-owned, top-level user sessions in the project.
  Archived rows, native harness mirrors, and parented subagents are excluded
  from the default roster (same exclusions as the fleet tally where they apply).
- Sort order: errored → needs attention → running → most recent activity.
- Each row shows status glyph, optional attention mark, title, harness, and
  (when space allows) a detail line with context gauge, activity, tokens, and
  model/identity — the same facts as full-mode list cards, not a new vocabulary.

### Activity feed

- Client-local ring buffer of **state transitions** (created, running, wants
  you, done, errored). Not a transcript and not durable across TUI restart.
- Clicking a feed line selects that session (hand-off).

### Preview

- Target order: hovered member → keyboard cursor (when the view is focused) →
  hottest member by the sort above.
- Chat body uses messages observed live on the client; if none are cached,
  show a soft empty state ("open session to load history") rather than
  attaching a PTY or blocking on a transcript fetch.

### Interaction

- With the **list** focused, the dashboard is watch-only: list keys keep
  navigating the sidebar.
- With the **view** focused, Up/Down (and page scroll) move the member cursor;
  Enter opens the highlighted member. Mouse click on a member or feed row
  always opens that session. Mouse hover retargets the preview without
  selecting.

### Non-goals of this decision

- Project-level bulk actions beyond what already exists on the project
  selection (rename, delete, create-inherits-project).
- Project memory editor, durable activity history, or seeding the project
  meter from fleet token history (samples are not session-attributed on the
  wire today).
- Replacing the session list hierarchy; the list remains the primary
  navigator.

## Reason

Selecting a project previously painted a flat status+name+harness list that
duplicated the sidebar with less information and no hand-off. Operators who
run several agents under one project need a ten-second answer to "what needs
me, what just happened, and is this project still burning tokens?" without
arrowing through every session.

## Consequences

- Clients that render a project selection must keep the dashboard's signals
  aligned with list/tally glyphs (status, attention, working/wants-you/errored).
- Cost and message events must feed project-scoped caches even when the
  project pane is not visible, so switching to a project shows real history.
- Focus routing (list vs view) is load-bearing: view-focused navigation must
  not steal list keys while the list holds focus.

## Examples

- Three members: one errored, one with the attention marker, one running.
  The roster orders them in that sequence; the header shows `●1 ·1 ✗1`; the
  preview defaults to the errored session until the operator moves the cursor
  or hovers another row.
- Enter on a project header in the list focuses the dashboard; Enter again
  opens the highlighted member and leaves the project selection.
- A project of only shell sessions shows an empty meter line and still lists
  members with cwd identity in place of a model label.
