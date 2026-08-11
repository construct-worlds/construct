# 0196-service-view-token-meter

Status: accepted
Date: 2026-08-10
Area: tui
Scope: A service view shows a live token-usage graph scoped to its routed sessions.

## Decision

The service view shows the same token-usage history graph as the project view,
scoped to sessions routed by that service and their descendants. Cost and
compute-time observations feed the service meter even while its view is
closed, so opening the service reveals activity the TUI has already observed.

The graph keeps the shared meter semantics: time buckets, stacked model bands,
cache-served shading, compact totals and rates, and per-column hover detail.
Cached input remains a subset of input rather than an additional token count.
When no usage has been observed, the meter region states that quietly. A short
pane omits the graph to preserve usable room for service fields and rows.

## Reason

Services can create and reuse many sessions without an operator opening each
one. A scoped history graph answers whether a service is actively consuming
tokens, how that activity changes over time, and which models contribute,
without mixing in unrelated fleet or project work.

## Consequences

- Every Cost event from a routed session or descendant contributes to its
  service's meter.
- Compute time from routed model-backed sessions and descendants contributes
  to the scoped throughput rates.
- Sessions from other services never contribute to the displayed history.
- Split panes may show independent service meters and hover details at once.
- Service meters begin with observations made by the current TUI process;
  daemon token-history samples cannot seed them because those samples do not
  carry session identity.

## Non-Goals

- This does not add an aggregate lifetime token label to the service header.
- This does not change how sessions are associated with a service.
