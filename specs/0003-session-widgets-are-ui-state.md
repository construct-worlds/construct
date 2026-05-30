# 0003-session-widgets-are-ui-state

Status: accepted
Date: 2026-05-30
Area: ux
Scope: Applies to generative widgets, widget persistence, widget rendering, and action links.

## Decision

Session widgets are session-scoped UI state, not transcript history. Agents provide semantic Markdown widget content; clients own layout, scrolling, focus, visibility controls, and rendering details.

## Reason

Agents need a compact way to show progress, decisions, and steering actions without polluting the model-facing conversation. Widgets solve that by creating a durable UI surface that can be updated or removed as task state changes.

Keeping widgets outside the transcript also lets clients rehydrate the latest widget state after reconnect without replaying conversation history.

## Consequences

Agents should update, consolidate, or delete widgets as task state changes. Stale widgets are misleading and should not be treated as permanent logs.

Clients should render current widget state from the session, including after reconnect. A missed live widget update during a disconnect must not leave the user looking at stale content once the client reconnects.

Widget actions represent user intent. They do not bypass normal permission, safety, or approval requirements.

## Non-Goals

Widgets are not a replacement for transcripts, logs, or durable project documentation. They are for current task UI.

## Examples

A checklist showing "Implement", "Validate", "Open PR", and "Merge" is a widget. The final PR discussion belongs in the transcript or repository, not in a permanent widget.
