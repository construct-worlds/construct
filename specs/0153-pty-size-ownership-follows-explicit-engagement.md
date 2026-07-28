# 0153-pty-size-ownership-follows-explicit-engagement

Status: accepted
Date: 2026-07-27
Area: protocol
Scope: Decide which of several clients viewing one session controls its single PTY geometry.

## Decision

PTY size ownership belongs to one client connection at a time, not to a client
transport category. Keyboard input, clicks, drags, wheel actions, and explicit
focus claims transfer ownership to that connection and apply its most recently
reported viewport.

A viewport change without explicit engagement is passive. It updates that
connection's remembered size and reaches the PTY only if the connection already
owns the geometry. It must not transfer ownership.

Clicking a visible terminal pane is an explicit claim even when that pane was
already locally focused and its measured dimensions did not change.

Pointer hover and motion are passive, including motion reports forwarded to a
mouse-tracking child application. Clicks, drags, wheel actions, and keyboard
input are explicit engagement and claim the geometry.

Clients must classify forwarded terminal mouse reports by their protocol
semantics, not solely by the lifetime of the originating UI event. A terminal
emulator may emit its input callback asynchronously after the pointer event has
finished. In SGR mouse mode, motion with the no-button code is hover; motion
with a held button remains an explicit drag.

Terminal-emulator protocol replies generated on behalf of the child are
passive. They must not be treated as keyboard input merely because the
terminal library exposes replies and user bytes through the same data stream.

PTY resize notifications identify, for each receiving connection, whether
that connection owns the resize. A client must immediately stop treating
itself as recently engaged and render the new owning grid when the resize
belongs to another connection. A local grace timer must not mask a newer
explicit claim from a TUI or another browser.

## Reason

A POSIX PTY has one row/column size even when several TUIs and browsers render
the session. Browser layout observers, delayed debounce timers, background-tab
settling, and other incidental measurements are not evidence that the user
changed attention. Treating every resize report as attention lets an idle
browser repeatedly undo a later TUI input claim. Grouping all TUIs or all web
clients together also loses the distinct viewport of each connection.

## Consequences

Each live connection keeps an independent remembered viewport per session.
Typing can restore the correct size immediately without first requiring another
window resize. Passive viewers follow the owner's reported PTY geometry and may
continue measuring their own viewport for a future claim.

Clients must distinguish explicit engagement from passive layout reports.
Compatibility clients that omit the distinction retain the historical
claiming behavior for both resize and input messages.

UI-event markers may supplement terminal-protocol classification, but cannot
be the only evidence that forwarded mouse input was passive.

When an owning connection disconnects, the session remains ownerless until the
next explicit engagement; the daemon does not guess among passive viewers.

An ownership handoff is announced to every attached client even when the
geometry does not change — for example when the new owner's remembered
viewport matches the current size, or when it has none yet. Clients are
entitled to track ownership from these announcements; a client that believes
it owns the geometry renders and reports its own fit, so a handoff it never
hears about would leave it authoritative over a grid it no longer controls.

## Non-Goals

This decision does not provide multiple simultaneous PTY grids, reflow a
full-screen terminal independently per viewer, or share keyboard focus between
clients.

## Examples

- A desktop browser is open at 90×30. A TUI receives a keystroke at 120×40.
  The PTY becomes 120×40; a later browser `ResizeObserver` report remains
  passive.
- Clicking an already-focused 90×30 browser split transfers ownership back to
  that browser and resizes the PTY to 90×30.
- Two TUI windows at different sizes retain separate remembered viewports;
  input in either window selects that exact connection's size.
- Moving a pointer over a browser or TUI pane does not transfer ownership, even
  when the TUI forwards that motion to a child using terminal mouse tracking.
- A browser answering a child's device-status or capability query does not
  transfer ownership; a browser keystroke still does.
- If a browser receives a TUI resize while its post-click grace timer is still
  live, it immediately mirrors the TUI grid; a subsequent mouse-tracking hover
  remains passive and is encoded against that owning grid.
