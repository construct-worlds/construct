# 0096-remote-control-status-affordance

Status: accepted
Date: 2026-07-15
Area: tui
Scope: Make remote control continuously discoverable and communicate listener and client state from the minibuffer status bar.

## Decision

The minibuffer status bar always shows a clickable remote-control item in its
right-aligned persistent group, between theme and version.

The item distinguishes three states: remote control is off, its listener is
active with zero clients, and its listener is active with one or more clients.
Clicking any state opens the same remote-control dialog as `/remote-connect`.
Opening the dialog may start the local authenticated listener, but does not
publish a tunnel until the user explicitly chooses a provider.

The daemon reports both listener availability and connected-client count. A
newly subscribed client receives an immediate state snapshot rather than
waiting for the next remote connection event.

## Reason

A badge that appears only after a client connects communicates activity but
does not help users discover remote control or tell whether an idle listener is
already running. A persistent control provides both entry point and state in a
location already used for durable application controls.

## Consequences

Future status-bar changes must preserve stopped, active-zero, and active-count
states and keep the item clickable. Remote-control state notifications must
remain sufficient for a newly connected TUI to render the current state
without inferring it from client-count transitions.

## Non-Goals

The status item does not start a public tunnel by itself and does not replace
the keyboard-accessible `/remote-connect` command.
