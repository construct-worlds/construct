# 0113-split-layout-is-shared-daemon-state

Status: accepted
Date: 2026-07-26
Area: architecture
Scope: The split layout — which panes exist, how they divide the area, and which session each shows — is daemon-owned state shared by every client, while pane focus and zoom remain per-client.

## Decision

The window tree is **shared state owned by the daemon**. A split made in one
client appears in every other client that is rendering the layout. The tree is
a recursive binary structure: a pane is a leaf showing at most one session, and
a split node divides its area between two children by percentage.

What is shared:

- The tree's shape, each split's direction, and each split's ratio.
- Which session each pane shows. A pane may legitimately be empty.
- Nothing else. The daemon stores presentation state here, not a rendering.

What is deliberately **not** shared, and must stay per-client:

- **Focus.** Which pane a client has focused decides where *that user's*
  keystrokes go. Sharing it would let one client redirect another's typing into
  a different session mid-sentence. Focus is per connection, and a client whose
  focused pane disappears falls back to the first pane in layout order — never
  to whatever the writer had focused.
- **Zoom**, scrollback position, selection, and per-pane view mode. View mode
  in particular is resolved locally by each client (see
  [0062-webui-view-mode-is-per-session](0062-webui-view-mode-is-per-session.md)
  and [0065-tui-view-mode-is-per-split-window](0065-tui-view-mode-is-per-split-window.md));
  the clients do not agree on a single set of view modes because they do not
  offer the same surfaces.

Ratios are percentages, not absolute sizes. This is what makes one tree
meaningful in both a character grid and a pixel viewport.

### Narrow clients view the layout; they never write it

A client whose viewport is too small to render the tree usefully — a phone, or
a small terminal — shows a **single session** and **must not write the layout
back**. It may read the shared layout to pick a session to open on, and it
tracks its current session locally, exactly as it did before the layout was
shared.

Crossing the threshold is therefore lossless in both directions: narrowing
hides the panes without writing anything, so widening restores the tree exactly
as the other clients still have it.

### Concurrency

Writes are whole-tree replacements carrying the version they were composed
against. A write composed against a stale version is rejected; the rejected
client re-reads and may retry. There is no merge semantics for a split tree,
and inventing one would add failure modes without adding capability.

A client must serialize its own writes. Two writes issued from one client
against the same base version would conflict with each other and silently lose
an edit — which a held-down arrow key is enough to produce.

On a rejected write a client adopts the broadcast state rather than re-pushing
its own. Re-pushing turns a conflict into a write war between two clients.

### Pruning

The daemon prunes the tree: when a session is deleted, every pane showing it is
**emptied**, and the change is broadcast. Panes are emptied rather than removed
because the *shape* of the layout is the user's — a session going away must not
collapse a split the user arranged.

Pruning also runs when the daemon loads a layout, against the sessions that
actually came back.

## Reason

Users run one fleet from several surfaces at once — a terminal on the desktop,
a browser on a second monitor, a phone. Before this, each client kept its own
window tree, so "the layout" meant something different in each one and arranging
panes had to be repeated per client.

Making the layout daemon state costs a protocol surface and a concurrency story,
and the two carve-outs above are what keep that cost bounded. Focus is excluded
because sharing it is actively harmful, not merely unnecessary. Narrow clients
are excluded because a layout that is right for a wide screen is unusable on a
phone, and a phone that reconciled by writing its own collapsed view back would
destroy the layout everywhere else — the failure mode that would make the whole
feature not worth having.

## Consequences

- Any client rendering the layout must implement the full recursive tree. A
  fixed two-pane layout is not sufficient, because it must faithfully render
  whatever tree another client writes.
- Splits are not a progressive enhancement that a client can defer: a split
  created on a wide client arrives at every client, so the narrow-viewport
  clamp must exist from the first release that renders panes at all.
- Per-pane local state (scrollback, view mode, sizes) is keyed by pane id, so
  pane ids must survive a round trip through the daemon unchanged, and must be
  unique within a tree.
- A client that was disconnected has a stale tree by definition and must re-read
  the layout on reconnect, not just subscribe to changes.
- Clients may still disagree about what a pane *shows* — one may render a
  session as a terminal and another as chat. Only the pane-to-session mapping
  is shared.

## Non-Goals

- Sharing focus, zoom, scroll position, or view mode.
- Making every client render the same surfaces. The web client can show chat in
  several panes at once because it holds several transcripts in memory; the TUI
  does not, and that difference is allowed to stand.
- A general layout-sync protocol for other UI state. Session list widths, panel
  sizes, and similar chrome remain client-local.
- Real-time streaming of an in-progress divider drag. Only the settled ratio is
  published.

## Examples

- A user splits a terminal pane in the TUI; the browser on the second monitor
  shows the same two panes, at the same ratio, without a reload.
- A pane in the TUI is focused while the same layout is open in a browser. The
  browser user moves focus to the other pane; the TUI's focus does not move,
  and neither user's typing is redirected.
- A session shown in two clients' panes is deleted. Both panes empty; neither
  split collapses.
- A user opens the web UI on a phone, browses several sessions, and closes it.
  The desktop's panes are exactly as they were.
- Two clients drag the same divider at once. One write lands; the other is
  rejected, adopts the winner's ratio, and does not re-push.
