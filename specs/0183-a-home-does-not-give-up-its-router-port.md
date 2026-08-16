# 0183-a-home-does-not-give-up-its-router-port

Status: accepted
Date: 2026-08-02
Area: architecture
Scope: What the router does when the port its live sessions are dialing is already taken.

## Decision

A home's router port is part of its identity, because harness processes
outlive the daemon and keep dialing whatever port they were given when they
were spawned. Losing it disconnects every one of them from their model
provider.

So a home does not give it up.

**It is waited for, not abandoned on first refusal.** The likeliest reason the
port is busy during a restart is the daemon that just left still holding it,
which clears in moments.

**A stand-in never becomes the home's recorded port.** Binding elsewhere so the
daemon can still boot is correct; recording that choice is not. It would turn
one busy moment into a permanent move and give up on every session still
dialing the original — the opposite of why the port is recorded at all. The one
exception is a home with no port yet: nothing is dialing anything, so whatever
it binds becomes its own. That is how a second home on one machine gets a
stable identity.

**A fallback is reported as the outage it is.** The daemon knows, at that
moment, that every session it spawned before now has lost its route and will
fail its next model call with a connection error explaining none of this. That
is not a warning about a port; it is an error about sessions.

**The port is taken back when it frees.** The router keeps trying, and serves
on the reclaimed port *in addition to* the stand-in, so neither the sessions
that predate the fallback nor those spawned during it are cut off. New sessions
are given the reclaimed port, so the home converges back to one.

A user-pinned port is exempt from all of this: it is intent, it never
falls back, and it is never rewritten.

## Reason

Route transport is injected into a session's environment once, at spawn, and
never revisited. There is no renegotiation, no discovery, and no way for a
running harness to be told the router moved. The port it was handed is the only
one it will ever try.

That makes an ephemeral fallback quietly destructive in a way its immediate
symptoms hide. Nothing fails when the fallback happens. The failures arrive
later, one per session, as a connection error naming a URL — attributable to
the network, the provider, or the harness, but not to the daemon that moved out
from under them. The distance between cause and symptom is what makes this
worth a spec rather than a fix.

Recording the fallback compounds it: the next start prefers the stand-in, so
the home never returns to the port its sessions know, and a transient collision
becomes permanent. Every session spawned before the collision is unroutable for
the rest of its life.

## Consequences

- A busy port costs a short delay at startup. That is deliberate — the
  alternative is spending it on every live session instead.
- A home that fell back is running on a port it does not consider its own until
  it reclaims it. The recorded port and the bound port may disagree, and the
  recorded one is authoritative for what the home wants.
- The router may serve more than one port at once. Attribution is by proxy
  credential, not by which port a connection arrived on, so this changes
  nothing about how a connection is routed.
- The reclaim attempt does not give up while the daemon lives. One bind attempt
  per interval is a smaller cost than sessions that stay unroutable.
- Sessions spawned during a fallback keep using the stand-in for their whole
  life. Convergence applies to new sessions, not existing ones.

## Non-Goals

- Telling a running harness that the router moved. It cannot be told.
- Guaranteeing a home always gets its port. Something else may hold it forever;
  the daemon still boots, and says clearly what that costs.
- Migrating existing sessions onto a reclaimed port.

## Examples

A daemon restarts and its predecessor has not released the port yet. The router
retries for a moment, gets it, and nothing downstream notices.

A second daemon is holding the port. The router binds a stand-in, logs an error
naming what has been lost, leaves the home's recorded port alone, and keeps
trying. When the other daemon exits, the port is taken back and served
alongside the stand-in; sessions spawned before the collision start working
again without being touched.

A user pins the port. A busy port is now a startup failure, not a
fallback, because the pin says the port matters more than booting.
