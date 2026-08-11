# 0191-project-dashboard-pane

Status: accepted
Date: 2026-08-04
Area: tui
Scope: Selecting a project header in the session list shows a single-column live dashboard of member cards in the main view pane.

## Decision

When the TUI selection is a **project** (organizer header), the main view pane
renders a **project dashboard** scoped to that project's members. The dashboard
is an observation and hand-off surface, not a second session workspace.

Its organizing principle: the sidebar list already answers *which* sessions
exist and *that* something needs attention (glyph level). The wide pane's job
is **content** — for each member, the one line the operator would otherwise
open the session to learn. There is no separate event feed and no preview
strip: every member card carries its own content.

### Layout

Top to bottom, dropping lower-priority regions when the pane is short:

1. **Header** — member count, project-scoped tally (working / wants you /
   errored, same ranking and glyphs as the fleet tally), lifetime token total
   when non-zero, and a dominant cwd line. Project identity is not repeated
   here: the main pane title bar already shows `project: {name}`.
2. **Token meter** — project-scoped throughput history fed from the same
   `Cost` / busy-time path as the fleet meter, filtered to members of this
   project. Idle projects show a quiet empty line rather than a blank grid.
   Its colored legend follows the operator meter's layout and rate formatting;
   hovering a column details it exactly as the fleet meter's does (spec 0167).
3. **Member cards** — a single full-width column, one card per member.

### Members

- Only active, Construct-owned, top-level user sessions in the project
  (same exclusions as before: archived rows, native harness mirrors, and
  parented subagents stay out of the roster).
- Sort order: errored → needs attention → running → most recent activity.
- Each card is two rows (one on cramped panes; a breathing row is added when
  everything fits airily):
  - **Title row** — status glyph, attention mark, title, and right-aligned
    meta (tokens, context gauge, model/identity, harness, busy/age — least
    important facts dropped first as width shrinks). Same vocabulary as
    full-mode list cards.
  - **Content row** — what the session is doing / asking / blocked on,
    chosen by urgency:
    1. `approve? {tool · args}` — a tool call is waiting on the user
       (from the live pending-approval set; multiple prompts show a count).
    2. `error: {message}` — the session is errored; quotes the daemon's
       durable last-error snippet.
    3. `asks: {text}` — the session stopped while unwatched (the sticky
       needs-attention marker) and its last words are assistant text: the
       question or result waiting on the user.
    4. `now:` / `on:` — running: the latest streamed assistant text, or the
       user prompt being worked when the agent hasn't spoken this turn.
    5. `you:` / `last:` — idle: the most recent message, labeled by who
       spoke it. Idle sessions the operator already saw stay at `last:` —
       "asks" is reserved for unwatched stops.
    6. A soft placeholder when there is nothing to quote.

### Content comes from durable daemon snippets

Member cards quote `SessionSummary`'s last-message / last-error snippets,
which the daemon maintains as events persist and restores from the transcript
at load. The pane is therefore fully populated immediately after a TUI or
daemon restart — a client-local message cache (which starts empty exactly
when the operator wants to catch up) is not an acceptable source for card
content. Streaming assistant deltas accumulate into one snippet; the snippet
is capped daemon-side so summary broadcasts stay small.

### Interaction

- With the **list** focused, the dashboard is watch-only: list keys keep
  navigating the sidebar.
- With the **view** focused, Up/Down (and page scroll) move the member cursor;
  Enter opens the highlighted member. Mouse click on a card always opens that
  session. Mouse hover highlights a card without selecting; Enter prefers the
  hovered card over the keyboard cursor.

### Non-goals of this decision

- Project-level bulk actions beyond what already exists on the project
  selection (rename, delete, create-inherits-project).
- A chronological event/activity feed. One was tried and removed: a
  transition log duplicates the roster's sort/glyph signal without content,
  and a client-local one is empty at exactly the moment the operator opens
  the TUI to catch up. "What just happened" is answered per-member by the
  content line, not by a timeline.
- Answering approval prompts or chatting from the dashboard itself; cards
  hand off to the session.
- Replacing the session list hierarchy; the list remains the primary
  navigator.

## Reason

Operators who run several agents under one project need a ten-second answer
to "what needs me, what just happened, and is this project still burning
tokens?" without arrowing through every session. Glyphs alone can't complete
a triage decision — "wants you" without the question, "errored" without the
error, still forces the operator to open the session. Quoting the actual
question, error, or current work on each card lets most decisions finish in
the pane. A two-column layout (roster | feed) was tried first and starved
both columns of width while the feed re-encoded information the roster
already carried.

## Consequences

- Clients that render a project selection must keep the dashboard's signals
  aligned with list/tally glyphs (status, attention, working/wants-you/errored).
- The daemon must maintain and restore last-message / last-error snippets on
  session summaries; clients must not reintroduce a client-local message
  cache as the card source.
- Clients that track pending tool approvals must retain the tool name and
  args summary (not just call ids) so cards can say what wants approval.
- Cost and busy events must feed project-scoped meters even when the project
  pane is not visible, so switching to a project shows real history.
- Focus routing (list vs view) is load-bearing: view-focused navigation must
  not steal list keys while the list holds focus.

## Examples

- Three members: one errored, one with the attention marker, one running.
  The cards order them in that sequence; the header shows `●1 ·1 ✗1`; the
  errored card quotes the error, the attention card quotes the question it
  asked, the running card quotes what it is doing right now.
- Enter on a project header in the list focuses the dashboard; Enter again
  opens the highlighted member and leaves the project selection.
- A project of only shell sessions shows an empty meter line and still lists
  member cards with cwd identity in place of a model label.
