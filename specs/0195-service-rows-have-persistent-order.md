# 0195-service-rows-have-persistent-order

Status: accepted
Date: 2026-08-05
Area: ux
Scope: Ordering and reordering of service rows in the unified session list.

## Decision

Services remain a distinct top-level region above ordinary sessions and
projects, but service rows have a user-controlled persistent order within that
region. Every first-party session-list client must expose the same reorder
action it exposes for project rows.

Definitions without an explicit service position use service name as a stable
tie-breaker. The first reorder materializes explicit positions so the chosen
order survives client and daemon restarts.

## Reason

Services are ordinary selectable rows in the unified list. Fixing them in
alphabetical order while adjacent session and project rows can be organized
makes the same reorder command silently fail based only on row type.

## Consequences

- Reordering a service never moves it into the session or project regions.
- Service edits preserve its existing position.
- New services append to the existing service region.
- Terminal and web clients must render the daemon's persisted service order.
- Legacy service definitions remain valid and deterministic before any
  reorder occurs.

## Non-Goals

- Interleaving services with sessions or projects.
- Reordering sessions routed beneath an expanded service row through the
  service reorder action.
