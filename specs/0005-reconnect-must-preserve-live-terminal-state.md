# 0005-reconnect-must-preserve-live-terminal-state

Status: accepted
Date: 2026-05-30
Area: webui
Scope: Applies to web client reconnect, terminal sessions, composer resize, and mobile viewport changes.

## Decision

Reconnect and viewport changes must preserve the live terminal surface. A client must not replay transcript or PTY history into an already-live terminal just because the websocket reconnects, the composer changes size, or a mobile keyboard appears.

## Reason

Terminal surfaces are stateful. Replaying old output into an existing terminal buffer makes the user see history duplicate or jump back to the beginning, which is especially disruptive during mobile keyboard show/hide and long-running sessions.

The user's scroll position and active terminal buffer are part of their current working context.

## Consequences

For terminal sessions, reconnect should refresh side-channel UI state that can become stale, such as widgets, while avoiding terminal transcript or PTY replay into the current buffer.

Composer resize and input insertion must not force scroll-to-top, scroll-to-middle, or full terminal repaint behavior.

This creates a split between terminal and non-terminal hydration paths. Future client code should preserve that distinction.

## Non-Goals

This does not prevent a fresh client with no existing terminal buffer from hydrating initial terminal content. It only constrains reconnect and resize behavior for an already-rendered live terminal surface.

## Examples

If a phone keyboard opens and the browser reconnects, the terminal should stay where the user left it while the widget panel can still update to the latest server state.
