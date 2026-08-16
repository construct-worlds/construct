# 0152-tunnel-registrations-are-actively-health-checked

Status: accepted
Date: 2026-07-27
Area: architecture
Scope: the first-party tunnel keeps verifying its gateway registration after startup and re-registers on its own when the gateway loses it.

## Decision

While a first-party tunnel is up, the daemon periodically re-probes the
gateway's authenticated readiness endpoint. A sustained run of consecutive
failures is treated as "the gateway no longer routes this tunnel": the daemon
withdraws the published public URL and immediately re-enters its registration
cycle, reusing the owner credential it already holds in memory. Recovery must
not require user interaction while that credential is still valid, and must
not re-open a login browser as part of the health-triggered cycle itself.

## Reason

The gateway keeps active routes in memory only; a operator deploy or host
restart silently discards every registration. The tunnel transport reconnects
on its own and reports nothing, so without an active probe the daemon keeps
advertising a public URL that serves visitors an offline page until the
capability-expiry refresh, roughly a day later. A user-visible outage of that
length after every routine operator deploy is not acceptable for a feature
whose whole point is unattended remote access.

## Consequences

- The readiness endpoint is a load-bearing health contract, not just a
  startup gate: the gateway must keep answering it truthfully for the whole
  life of a registration (unknown or unroutable tunnel → failure status).
- Detection uses a failure streak, not a single miss, so brief gateway
  restarts and transient network errors do not flap the tunnel. The streak
  window (interval × threshold) should stay around a minute — comfortably
  longer than one failed request, much shorter than a capability refresh.
- Re-registration reuses the in-memory owner credential. If it has expired,
  the existing re-authentication path (which may open a browser) takes over;
  that is the pre-existing refresh behavior, not something the health check
  introduces. That fallback is unreachable for a user who is away from the
  host machine, which is the gap spec 0162 proposes to close by giving
  re-registration a credential of its own.

## Non-Goals

- Health-checking third-party tunnel providers, which supervise their own
  child processes and have their own reconnection story.
- Persisting owner credentials to survive daemon restarts (spec 0146 keeps
  them memory-only on purpose).
- Streaming tunnel health to clients as a dedicated status feed; the tunnel
  URL slot going empty remains the observable signal.
