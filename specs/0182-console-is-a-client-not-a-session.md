# 0182-console-is-a-client-not-a-session

Status: proposed
Date: 2026-08-02
Area: architecture
Scope: The web UI's console mode — an instance of our own TUI, spawned by the daemon and rendered in a browser terminal.

## Decision

A client may ask the daemon to run the daemon's own TUI in a PTY and stream
it back, so that every TUI surface is reachable from a browser without the
web UI having reimplemented it.

That PTY is a **console**, and a console is a client, not a session:

- It is never listed, persisted, resumed after a daemon restart, or counted
  in fleet tallies, token meters, or lineage. Nothing in the fleet can address
  it, and it has no transcript.
- It belongs to exactly one client connection. It is created on request,
  reaped when that connection drops, and never shared between connections.
- Because it has exactly one viewer, its geometry is simply that viewer's
  fit. The ownership arbitration a session's shared PTY needs
  ([`0153`](0153-pty-size-ownership-follows-explicit-engagement.md)) does not
  apply and must not be introduced.

While a console is open, the client's own keymap stands down. Every key,
chord included, belongs to the child.

A console is offered on desktop viewports only. It is an addition to the web
UI, never its default surface — the modern web client remains the baseline
([`0144`](0144-webui-modern-default-matrix-optin.md)).

## Reason

Two clients render this fleet, and they have never been at parity. Each TUI
capability has had to be ported into the web UI a second time — view modes,
playbook, split panes, session controls, activity, the chord keymap — and the
queue does not empty. A console makes anything not yet ported reachable from
a browser the day it lands in the TUI, which changes parity from a blocker
into a preference about which surface is nicer for a given task.

Modeling it as a session would have been less code: the attach, input, resize
and reconnect paths already exist. It would also have been wrong. A session is
a unit of work the fleet is tracking
([`0001`](0001-sessions-are-the-core-unit.md)); a console is a viewport onto
the fleet. As a session it would appear in its own list, count itself in its
own token meter, persist a transcript of nothing, and survive a restart it has
no reason to survive.

The keymap rule is not a detail. The web UI deliberately speaks the TUI's
`C-x` chord vocabulary ([`0150`](0150-web-ui-shares-the-tui-chord-keymap.md)).
Pointed at a console, that becomes two consumers of one vocabulary: `C-x C-f`
opens a web dialog stacked over a terminal that never saw the prefix. Whichever
surface holds the keyboard must be the only one interpreting it, and the child
is the one drawing the screen.

Desktop-only is a judgment about honesty. A full-screen TUI on a phone has no
modifier keys and no room for the grid it wants; offering it there would
advertise a capability that does not survive contact with the device.

## Consequences

- The daemon spawns its own executable as a client. That is a real capability
  boundary: anything reachable in the TUI is now reachable through whatever
  gate the client connection came in by, including remote control. This adds
  no authority a web client did not already have — it can already create a
  shell session and type into it — so the exposure decision stays where it is
  ([`0143`](0143-remote-control-exposure-is-chosen.md)) rather than growing a
  second, console-specific gate.
- Console lifetime is bounded by its connection, so a closed tab must not
  strand a process on the daemon host. Reaping on connection drop is
  load-bearing, not cleanup hygiene.
- A console cannot be reattached. A dropped socket ends it; reconnecting
  yields a new connection, and therefore a new console, not the old one.
- Terminal-emulator affordances the TUI expects from a real terminal must be
  supplied by the rendering client or deliberately given up: background
  reporting, clipboard (OSC 52), and any chord the browser reserves for
  itself.

## Non-Goals

This does not make the console the way to use construct from a browser, and it
is not a reason to stop bringing TUI capabilities into the web UI natively. The
native surface is the one that can be good on a phone; the console is the
escape hatch for everything else.

This does not introduce a general "run a program on the daemon host" facility.
The console runs one program — this daemon's own client — and takes no command
from the caller.

## Examples

Opening the console from a laptop, pressing `C-x C-f`, and choosing a harness
creates a session that appears in every other client's list. The console itself
appears in none of them, including its own.

Closing the browser tab ends the console. Reopening the page shows the web UI
again, with the session that was created still there.
