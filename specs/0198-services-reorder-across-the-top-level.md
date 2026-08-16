# 0198-services-reorder-across-the-top-level

Status: accepted
Date: 2026-08-11
Area: ux
Scope: Service rows reorder through the whole top-level session list, not only among themselves.

## Decision

A service row can be moved anywhere in the top-level list flow: among other
services, between ungrouped sessions, and between project blocks. Services
stay top-level rows — moving one past a project hops the entire project block;
a service never becomes a project member.

A service that has never been moved out of the leading service block stays
there, and new services still append to that block, so fleets that never
reorder keep the legacy layout.

The persisted form of an interleaved position is a pin, not an anchor: the
service records which region it sits in (the ungrouped-session run or the
projects run) plus the position *value* of the row it was dropped after. At
equal position values the session/project row sorts first, then services in
their own persisted order. Because the pin is a value rather than a row id,
the service keeps its slot when the neighbor it was dropped after is archived,
deleted, or moved elsewhere.

Each reorder step is computed by the daemon against the same top-level flow
every list client renders, and each successful step re-derives all services'
persisted order from the result. Every first-party list client must render the
daemon's persisted interleave with the same ordering rule.

## Reason

Services, sessions, and projects are peer rows in one unified list, but the
reorder command previously stopped working at the service-block boundary: the
same keybinding that walks a session across regions silently pinned a service
inside its block. Minibuffers organizing a fleet expect any top-level row to be
placeable relative to the others.

## Consequences

- The daemon's service reorder needs the fleet's sessions and projects, not
  just the service definitions, to compute a step.
- All list clients must share one ordering rule (region, pinned value, row
  before service on ties, then service order) or the same fleet renders in
  different orders per client.
- Rows outside the flow are skipped in one step: a service crossing a project
  header hops the whole block, and hidden rows (routed service sessions,
  archived sessions, nested subagents/forks) are never step targets.
- Re-deriving persisted order on every step self-heals pins whose row values
  have since vanished.
- Sessions moving past an interleaved service row do not push it around:
  session reorder still swaps session positions only, so a session may
  visually hop a service row in one step.

## Non-Goals

- Nesting services inside projects.
- Reordering the sessions routed beneath a service row through the service
  reorder action.

## Examples

- With rows `[service A, service B, session 1, project P]`, moving B down
  twice yields `[service A, session 1, service B, project P]`, then
  `[service A, session 1, project P, service B]`.
- Archiving session 1 afterwards leaves B between A and P.
