# 0007-session-list-actions-target-visible-user-sessions

Status: accepted
Date: 2026-05-30
Area: ux
Scope: Applies to session list ordering, navigation, and list actions.

## Decision

Session list actions should operate on visible user sessions, not hidden system sessions or collapsed descendants that the user cannot currently see.

## Reason

List actions should match the user's visible model. If a reorder or navigation command affects hidden rows, the result feels unpredictable and can appear broken.

Hidden orchestrator sessions and collapsed project contents are implementation or organization details from the user's current point of view.

## Consequences

Move-up, move-down, and similar list operations should skip hidden or currently collapsed items. Selection and ordering semantics should be based on the rendered user list unless a command explicitly says it is operating globally.

This may require keeping internal ordering separate from visible ordering, or translating user actions through the current visible projection.

## Non-Goals

This does not forbid commands that operate on all sessions. Such commands must be explicit about their wider scope.

## Examples

If a project is collapsed, moving a visible session down should jump over that collapsed block rather than targeting one of its hidden sessions.
