# 0173-tui-split-panes-replace-pinned-session-view

Status: accepted
Date: 2026-08-01
Area: tui
Scope: The terminal TUI's persistent session-view affordance.

## Decision

The TUI does not provide a pinned-session view or pin strip. The normal
session layout consists of the session list, one or more split session panes,
and the existing operator/minibuffer surfaces. Session panes are the way to
keep multiple worker sessions visible and interactive at the same time.

The TUI therefore does not render a pin marker in session-list rows, bind
keyboard or MIDI actions for pinning, reserve layout space for pinned tiles,
hydrate sessions solely because they are marked pinned, or treat a pinned
session as visible for redraw decisions. Mouse hit-testing and resize handles
must likewise contain no pin-strip behavior.

The shared session `pinned` field and `session.set_pinned` protocol remain
available for clients that use them, including the web UI. This decision is
limited to the terminal TUI's session-view presentation; it does not remove
Playbook clip cards, dynamic-widget pinning, fleet-panel pinning, or other
client-local pinned surfaces.

## Reason

Split session view provides the same persistent multi-session visibility while
preserving each session's own pane geometry and input focus. A second display
surface for the same sessions added layout complexity, duplicate mouse and
keyboard affordances, extra hydration work, and shared-parser resize pressure.
Once splits are available, the pin strip no longer provides enough distinct
value to justify those costs.

## Consequences

Session visibility in the TUI is derived from the main window tree and the
operator panel, not from the daemon's session pin flag. Background PTY output
from sessions outside those surfaces may skip an immediate full-frame redraw,
while sessions in split panes are rendered and resized according to their own
pane geometry.

The old parser-sharing rule for pin tiles is historical and superseded. The
TUI must not reintroduce a pin strip merely to preserve that implementation;
future multi-session presentation work should extend split panes or define a
separate surface with its own explicit ownership and hydration rules.

## Non-Goals

- Removing the shared session pin field or its daemon/client API.
- Removing web UI session pinning.
- Removing Playbook clip pinning or dynamic-widget/fleet-panel pinning.
- Preventing non-TUI clients from showing the same session in multiple places.

## Examples

A user who wants two worker sessions visible keeps them in two split panes.
Selecting a session already shown in another pane follows the unique-session
swap behavior in spec 0039; it does not create or update a pinned tile below
the panes.
