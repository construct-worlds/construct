# 0162-tunnel-re-registration-is-a-scoped-credential

Status: accepted
Date: 2026-07-30
Area: protocol
Scope: how a running daemon re-registers a first-party tunnel after its owner credential has expired, without user interaction.

## Decision

A successful first-party tunnel registration returns a **re-registration
credential** alongside the short-lived routing capability. It authorizes
exactly one operation — re-register this one reservation — and nothing else.

The daemon holds it in process memory for the life of the tunnel supervisor
and presents it, in place of the owner credential, on every subsequent
registration for that reservation. Each re-registration returns a fresh
credential and invalidates the one just used.

The credential's lifetime is long relative to the owner credential's. Owner
credentials stay short-lived and interactive; unattended recovery no longer
depends on one being current.

The credential is scoped to the tuple that identifies the reservation: owner
identity, chosen name, and installation. Presenting it to register a different
name, or from a different installation, is refused. It cannot mint an owner
credential, open or authorize a browser session, enumerate or read other
tunnels, or authorize anything outside re-registration.

Like the owner credential, it is never written to disk, displayed, copied,
logged, or configurable through the environment. When the daemon cannot
present a valid one, the existing interactive authorization path takes over
unchanged.

## Reason

Spec 0152 made the daemon re-register on its own when the gateway forgets a
route, but that recovery only works while the credential it already holds is
still valid. Owner credentials are deliberately short-lived, and the gateway
loses routes on every routine operator deploy. The two windows are independent,
so a deploy that lands after the owner credential lapses leaves a *running*
daemon with a dead tunnel, no way to re-register, and no path forward except
opening a browser on the host machine.

That is precisely the case remote access exists to serve. A user reaching
their machine from a phone has no way to complete a browser handoff on the
host, so the tunnel stays down until they are physically back at it — the
outage outlasts the very access that would have let them fix it.

The alternatives are worse. Extending the owner credential's lifetime widens
what a leaked credential can do, since it authorizes account-level operations.
Persisting the owner credential to disk was rejected in 0146 and for good
reason. Giving long life instead to a credential that can do exactly one
harmless thing — restore a route that this installation already owns — buys
unattended recovery at close to no additional blast radius: an attacker
holding one can only cause the reservation's own hostname to point where it
already points.

Keeping it memory-only is sufficient because the failure being fixed is a
*running* daemon outliving its credential. A daemon that restarts has already
lost its tunnel by a separate, deliberate decision (0146), and nothing here
changes that.

## Consequences

- Re-registration credentials are a distinct credential purpose. The operator
  must not accept one where an owner credential is required, nor the reverse.
- Rotation on use is mandatory, so a captured credential stops working as soon
  as the legitimate holder next recovers. Both sides must tolerate a
  re-registration whose response is lost: the daemon may retry, so the operator
  cannot invalidate the old credential until the new one has been issued.
- The credential's lifetime sets the ceiling on unattended recovery. Beyond it,
  the tunnel needs a human. That ceiling should be generous enough to cover a
  realistic trip away from the machine.
- Stopping a tunnel, or logging the owner identity out, must invalidate any
  outstanding re-registration credential for that reservation. Recovery
  survives operator restarts; it must not survive the user revoking access.
- Restarting the daemon still requires interactive authorization. 0146's
  memory-only rule is unchanged, and this spec must not be read as a step
  toward persisting credentials.
- The operator gains an authorization path that no browser drives, so its
  refusals must be legible to an unattended client: a rejected credential has
  to be distinguishable from a transport failure, or the daemon cannot tell
  "re-authorize" from "retry".

## Non-Goals

- Surviving a daemon restart, or any form of credential persistence.
- First-time authorization without a browser. The initial handoff stays
  interactive; only *re*-registration becomes unattended.
- Sharing, delegation, or multi-user access to a reservation.
- Replacing the health check that triggers re-registration. This spec supplies
  the credential that recovery uses; 0152 decides when recovery runs.

## Examples

- A operator deploy drops all routes a day after the user last signed in. The
  daemon has been running throughout. Its health check notices the route is
  gone, re-registers with the credential it holds, and the same public URL
  comes back. No browser opens, and a phone connected to that URL reconnects
  on its own.

- The same deploy, but the daemon was restarted at some point in between. It
  holds no credential, so remote control is simply off and the user reconnects
  explicitly the next time they are at the machine — the pre-existing behavior.

- A credential captured from a compromised host is replayed after the
  legitimate daemon has recovered once. It has already been rotated out and is
  refused. Even had it worked, it could only have re-pointed the reservation's
  own hostname at the installation that already owns it.
