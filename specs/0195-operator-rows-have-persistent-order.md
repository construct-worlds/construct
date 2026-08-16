# 0195-operator-rows-have-persistent-order

Status: superseded by 0198-operators-reorder-across-the-top-level
Date: 2026-08-05
Area: ux
Scope: Ordering and reordering of operator rows in the unified session list.

> Superseded 2026-08-11: operators still have a persistent user-controlled
> order, but they are no longer confined to a distinct region — a operator row
> now reorders across the whole top level (see 0198).

## Decision

Operators remain a distinct top-level region above ordinary sessions and
projects, but operator rows have a user-controlled persistent order within that
region. Every first-party session-list client must expose the same reorder
action it exposes for project rows.

Definitions without an explicit operator position use operator name as a stable
tie-breaker. The first reorder materializes explicit positions so the chosen
order survives client and daemon restarts.

## Reason

Operators are ordinary selectable rows in the unified list. Fixing them in
alphabetical order while adjacent session and project rows can be organized
makes the same reorder command silently fail based only on row type.

## Consequences

- Reordering a operator never moves it into the session or project regions.
- Operator edits preserve its existing position.
- New operators append to the existing operator region.
- Terminal and web clients must render the daemon's persisted operator order.
- Legacy operator definitions remain valid and deterministic before any
  reorder occurs.

## Non-Goals

- Interleaving operators with sessions or projects.
- Reordering sessions routed beneath an expanded operator row through the
  operator reorder action.
