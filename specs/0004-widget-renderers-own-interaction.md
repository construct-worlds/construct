# 0004-widget-renderers-own-interaction

Status: accepted
Date: 2026-05-30
Area: ux
Scope: Applies to widget display in TUI and web UI clients.

## Decision

Widget producers define semantic content, while each client renderer owns interaction mechanics such as hide/show, dropdown listing, collapsed state, scrolling, focus handling, and responsive layout.

## Reason

The same widget should be useful in multiple clients with different constraints. A terminal UI, browser UI, and future clients cannot share identical layout mechanics, but they can share semantic Markdown and action intent.

Separating content from interaction lets clients improve ergonomics without changing agent-produced widget files.

## Consequences

Clients should provide a way to hide visible widgets and bring hidden widgets back. Hiding is a user preference over current widget visibility, not a deletion of widget state.

Dropdowns, popovers, and widget windows should remain stable while the user toggles widget visibility. A checkbox-style action should not close the surrounding widget management UI unless closing is the explicit command.

Clients should avoid nested framed surfaces and should keep widget controls consistent with the rest of that client.

## Non-Goals

This does not require pixel parity across TUI and web UI. Equivalent semantics are more important than identical presentation.

## Examples

If a user unchecks a widget in a web dropdown, the dropdown should stay open so the user can adjust multiple widgets in one pass.
