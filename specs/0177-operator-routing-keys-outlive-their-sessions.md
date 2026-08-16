# 0177-operator-routing-keys-outlive-their-sessions

Status: accepted
Date: 2026-08-01
Area: persistence
Scope: A operator routing key that maps to a deleted session must open a new session rather than fail.

## Decision

A operator's routing table is a cache of which session currently serves which
routing key. It is not a guarantee that the session exists.

Any routed delivery that resolves to a session which no longer exists must drop
the stale entry and route as if the key had never been seen: a new session is
created, adopted by the operator, and recorded under that key. The delivery
succeeds. This holds for every routing mode that keys sessions, including the
fixed key used by single-session routing.

Deciding that a session is gone must rest on the session's absence from the
daemon's session registry, not on the failure of a larger read. A transcript or
storage error is not evidence of deletion, and must not abandon a live
conversation.

## Reason

Sessions are deletable from every client, and a operator is not consulted or
notified when one goes. There is no path that prunes the routing table on
deletion, and adding one would still leave the table stale after any deletion
that happened while the daemon was down.

So the routing table will contain dangling entries in normal operation, and the
delivery path is the only place that can observe it. Treating a dangling entry
as an error strands the key permanently: the session it names can never come
back, so every future delivery on that key fails the same way, and the only
repair is hand-editing the operator's state file. The channel is left silently
broken for one caller while the operator still looks healthy.

Recreating is also what the routing modes promise. Keyed routing says equal keys
continue the same conversation — it does not promise a conversation that was
deleted is recoverable, only that the key keeps working.

## Consequences

- Deleting a routed session is a supported way to reset that conversation. The
  next delivery starts a fresh one with no history.
- Routing state is self-healing. Stale entries are repaired when the key is next
  used, so neither daemon startup nor session deletion needs to reconcile it.
- The routing table can hold entries for sessions that no longer exist for an
  unbounded time. Anything reading it must tolerate that rather than assume
  every id resolves.
- Pruning and the replacement insert must stay atomic against concurrent
  deliveries on the same key, or two deliveries can each create a session and
  orphan one.
- Ownership records are pruned with the routing entry, so a deleted session's id
  cannot later be reused to expose an unrelated session through the channel.

## Non-Goals

This does not make operators responsible for preserving a deleted conversation's
history, and it does not add deletion notifications from sessions to operators.
The recovery is deliberately confined to the delivery path.

## Examples

- A caller has been talking to a operator under a stable key. A user deletes
  that session from the TUI. The caller's next message opens a new session and
  is answered; the reply has no memory of the earlier exchange.
- A operator routes every delivery to one shared session. That session is
  deleted. The next delivery from any caller creates the new shared session, and
  subsequent deliveries join it.
- A routed session's transcript cannot be read because of a filesystem error.
  The delivery fails and the routing entry is kept, because the conversation may
  still be intact.
