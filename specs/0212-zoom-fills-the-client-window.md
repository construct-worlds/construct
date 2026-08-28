# 0212-zoom-fills-the-client-window

Status: accepted
Date: 2026-08-28
Area: ux
Scope: What zoom means in a client, and what it is allowed to hide.

## Decision

Zoom means: **the focused session view fills this client's window.** Every
other pane gets out of the way, and so does the session list.

- Zoom is available whether or not a split layout exists. With one pane it
  still has the list to cover, so it is never a no-op the user has to
  discover by pressing it.
- Zoom is per-client presentation state (see
  [0118-split-layout-is-shared-daemon-state](0118-split-layout-is-shared-daemon-state.md)).
  It hides panes rather than rewriting the shared tree, publishes no layout
  edit, and unzooming restores the layout exactly.
- Zoom must not consume the user's own list show/hide preference. It hides
  the list for the duration; unzooming restores whatever the user had, not
  whatever zoom left behind.
- Asking for the session list while zoomed is a request to leave zoom. A
  client must not leave a control that asks for the list doing nothing.
- Whatever surface offers zoom must also offer the way back, labelled as
  such, unless the zoomed layout genuinely has no room for it — a client
  whose zoomed view keeps its title bar has room.

## Reason

"Zoom" that only hid sibling panes left the session list occupying a
column, so the same gesture produced a full-screen view in one client and a
partial one in another, and did nothing at all in the common single-pane
case. One name should mean one thing across clients.

Zoom is a viewing gesture, not a layout edit: the user is looking closer,
not rearranging what everyone else sees. That is why it stays client-local
and why it must be exactly reversible — including the preferences it
temporarily overrides.

## Consequences

- New chrome that competes for space (list, sidebars, panels) must decide
  whether zoom hides it; the default answer is yes, since zoom's promise is
  the window.
- Clients must keep zoom out of persisted preferences, or they will leak a
  temporary view state into the user's saved layout.
- A client whose zoomed layout drops its title bar has no menu to offer the
  way out from; its zoom must stay reachable by key or command.

## Non-Goals

- Zoom is not a layout edit and never becomes one; a user who wants other
  clients to see one pane closes the others instead.
- Hiding the composer, header, or other input affordances is not implied —
  zoom covers what competes with the view, not what drives it.
