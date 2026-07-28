# 0154-project-ipc-is-the-only-organizer-surface

Status: accepted
Date: 2026-07-28
Area: protocol
Scope: Session organizers are exposed on the wire only as `project.*` methods and `project/*` notifications.

## Decision

Clients create, list, rename, delete, collapse, and reorder organizers through `project.*` IPC methods. State changes are broadcast only as `project/state` and `project/deleted`. The historical `group.*` methods and `group/*` notifications are not part of the public protocol.

On-disk and in-memory fields may still use names such as `group_id` / `GroupSummary` until a separate storage migration renames them. Wire parameter aliases that accept a legacy `group_id` field on project-named payloads may remain while storage uses that name.

## Reason

The product name is project. A compatibility window kept dual method and notification names so older clients could migrate. After first-party clients (TUI, web UI, MCP, adapters) speak project terminology, dual surfaces only add ambiguity and maintenance cost.

## Consequences

- New clients must use `project.*` / `project/*`.
- Removing or renaming persisted `group_id` is out of scope for this decision and requires its own staged migration.
- Internal daemon APIs may keep `group` naming until storage renames catch up; that does not reintroduce group-named IPC.

## Non-Goals

- Renaming session summary fields, storage directories, or internal helper methods.
- Removing `session.set_group` / `session.set_project` dual membership methods in the same step unless callers have fully moved.

## Examples

- Creating an organizer: `project.create` with `{ "name": "…" }` → `{ "project_id": "…" }`.
- Collapse toggle: `project.set_collapsed` with `{ "project_id": "…", "collapsed": true }`.
- Broadcast after rename: only `project/state` with `{ "project": { … } }`.
