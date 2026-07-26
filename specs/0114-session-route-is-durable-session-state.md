# 0114-session-route-is-durable-session-state

Status: accepted
Date: 2026-07-25
Area: persistence
Scope: How a session's active model route is recorded, surfaced, and restored.

## Decision

A session's route is durable per-session state owned by the daemon, in the
same class as its approval mode and its active model
([0032](0032-smith-active-model-persists-across-resume.md)).

- A session is either unrouted (the default) or routed to exactly one
  named route.
- Arming, changing, or clearing a route takes effect on the running
  session without restarting it, and without restarting the harness
  process.
- The choice survives session restart and daemon restart. A resumed
  session comes back on the route it was last running.
- The session record carries both the model the harness itself reports and
  the model the route substitutes, so a client can show the substitution
  rather than only its result.

Clients read the live route from the session record. It is never a
transcript row: a route change is configuration, not conversation.

## Reason

The value of runtime routing is that a session in flight can be moved to a
different endpoint. That is only true if the change applies to the live
process — hence a routing table the proxy consults per request, rather
than anything baked into the harness at spawn.

Durability is what makes the choice trustworthy across the restarts
Construct performs routinely. Without it, a daemon restart would silently
return a session to an endpoint the user had deliberately moved it off,
which is the same class of surprise
[0032](0032-smith-active-model-persists-across-resume.md) exists to
prevent.

Keeping both models on the record — reported and substituted — is what
lets the user see that a substitution is in effect at all. A single field
showing only the effective model makes routing invisible, and invisible
routing is indistinguishable from a harness that changed its own model.

## Consequences

- The route is stored by name, and that name is a model-profile name
  shared with the harness-agnostic profile registry — an endpoint is
  declared once and reachable from every consumer of it. A name that no
  longer resolves after a config change must surface as an actionable
  error on the session, not silently fall back to pass-through and not
  silently pin a stale endpoint.
- Because the harness is not restarted, the harness continues to believe
  it is talking to its own model. Anything derived from the harness's
  self-report — its own context-window accounting, its displayed model —
  reflects the harness's belief, not the route. Construct's own display
  must show the substitution.
- A route change applies at the next request the harness makes. It must
  not apply to a request already in flight.
- "Next request" has to account for connection reuse. A harness holding a
  live connection would otherwise keep using the disposition that
  connection was opened with, so a change that appears to have been made
  silently does nothing. A connection made stale by a route change must
  therefore be closed so the harness reconnects and is classified afresh —
  but only once nothing is outstanding on it, since closing a connection
  that is awaiting a response would abort the very turn this rule protects.
  A connection whose last traffic flowed toward the harness and has since
  gone quiet is between turns; one whose last traffic flowed away from it
  is waiting.
- Clearing a route returns the session to pass-through, which is always
  available. Clearing can never fail.
- A session that was never route-capable (no proxy transport injected at
  spawn) cannot be armed. It reports why, and offers no route options that
  would silently do nothing.

## Non-Goals

- Per-turn or per-request routing, and any notion of route history. Only
  the current route is retained.
- Automatic or policy-driven route selection (cost, availability,
  failover). A route changes because a user changed it.
- Preserving a route across a fork into a new session, unless the fork
  semantics for other session state say otherwise.

## Examples

A session reports its model as `claude-opus-5`. The user arms the route
named `kimi`, whose target model is `kimi-k2.5`. The session's record now
carries both, and Construct displays the substitution — the reported model
and the routed model together — rather than replacing one with the other.
After a daemon restart, the session resumes still routed to `kimi`.
