# 0196-service-view-lifetime-token-usage

Status: accepted
Date: 2026-08-10
Area: tui
Scope: A service view reports the aggregate lifetime token usage of its routed sessions.

## Decision

The service view's Sessions heading shows the lifetime token total across all
sessions routed by that service whenever the total is non-zero. The total uses
the same token accounting and compact formatting as the project view: input
plus output tokens, with cached input treated as a subset of input rather than
added again.

## Reason

Services can create and reuse many sessions without an operator opening each
one. A service-level total makes their aggregate model usage visible at the
same glance as their routed-session count and keeps project and service
organizer views consistent.

## Consequences

- Every routed session known to the service view contributes to the total.
- Sessions that have not reported usage contribute zero.
- A zero aggregate is omitted instead of rendering a misleading usage value.
- Future token-accounting changes must keep project and service lifetime
  totals aligned.

## Non-Goals

- This does not add a service-scoped realtime throughput meter.
- This does not change how sessions are associated with a service.
