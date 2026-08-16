# 0198-operators-reorder-across-the-top-level

Status: accepted
Date: 2026-08-11
Area: ux
Scope: Operator rows reorder through the whole top-level session list, not only among themselves.

## Decision

A operator row can be moved anywhere in the top-level list flow: among other
operators, between ungrouped sessions, and between project blocks. Operators
stay top-level rows — moving one past a project hops the entire project block;
a operator never becomes a project member.

A operator that has never been moved out of the leading operator block stays
there, and new operators still append to that block, so fleets that never
reorder keep the legacy layout.

The persisted form of an interleaved position is a pin, not an anchor: the
operator records which region it sits in (the ungrouped-session run or the
projects run) plus the position *value* of the row it was dropped after. At
equal position values the session/project row sorts first, then operators in
their own persisted order. Because the pin is a value rather than a row id,
the operator keeps its slot when the neighbor it was dropped after is archived,
deleted, or moved elsewhere.

Each reorder step is computed by the daemon against the same top-level flow
every list client renders, and each successful step re-derives all operators'
persisted order from the result. Every first-party list client must render the
daemon's persisted interleave with the same ordering rule.

## Reason

Operators, sessions, and projects are peer rows in one unified list, but the
reorder command previously stopped working at the operator-block boundary: the
same keybinding that walks a session across regions silently pinned a operator
inside its block. Minibuffers organizing a fleet expect any top-level row to be
placeable relative to the others.

## Consequences

- The daemon's operator reorder needs the fleet's sessions and projects, not
  just the operator definitions, to compute a step.
- All list clients must share one ordering rule (region, pinned value, row
  before operator on ties, then operator order) or the same fleet renders in
  different orders per client.
- Rows outside the flow are skipped in one step: a operator crossing a project
  header hops the whole block, and hidden rows (routed operator sessions,
  archived sessions, nested subagents/forks) are never step targets.
- Re-deriving persisted order on every step self-heals pins whose row values
  have since vanished.
- Sessions moving past an interleaved operator row do not push it around:
  session reorder still swaps session positions only, so a session may
  visually hop a operator row in one step.

## Non-Goals

- Nesting operators inside projects.
- Reordering the sessions routed beneath a operator row through the operator
  reorder action.

## Examples

- With rows `[operator A, operator B, session 1, project P]`, moving B down
  twice yields `[operator A, session 1, operator B, project P]`, then
  `[operator A, session 1, project P, operator B]`.
- Archiving session 1 afterwards leaves B between A and P.
