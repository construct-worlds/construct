# 0182-an-accepted-delivery-outlives-the-daemon

Status: accepted
Date: 2026-08-02
Area: persistence
Scope: What happens to a service delivery whose turn is still running when the daemon restarts.

## Decision

Accepting a delivery is a promise to say something about it. That promise
outlives the daemon process that made it.

A delivery is recorded the moment it is accepted and stays recorded until
something has been said in the channel about how it ended. The record carries
what a later daemon needs to pick the wait back up — which session, where in
its transcript the turn began, when it was accepted — plus a channel-owned
blob describing whatever the channel already put in front of the person
waiting.

On starting a channel, every delivery it left outstanding is finished before
any new one is taken:

**Resume by default.** A harness outlives the daemon and is reattached, so a
turn interrupted by a restart is usually still running. The wait is picked back
up rather than declared lost.

**On what is left of the original allowance.** Surviving a restart must not
extend a turn's life. A delivery whose allowance is already spent is reported,
not resumed.

**Report only what cannot be resumed.** A delivery whose session no longer
exists has nothing left to wait on, and says so.

The channel context is opaque to the ingress. The ingress routes and waits; it
does not render, and a second kind of channel must not require a schema change
in state shared by all of them.

## Reason

The task waiting on a turn lives exactly as long as the daemon process. A
restart takes it with it — silently, because nothing is watching the waiter.
Whatever the channel had already placed on behalf of that delivery is then
permanent: a progress placeholder that will never be replaced, a reaction that
will never be settled. No timeout fires, because the thing that would have
timed out is gone.

This is not a rare edge. Restarts happen on upgrade, on configuration change,
and on operator command, and a service channel accepts deliveries the whole
time. Every restart that lands mid-turn strands one.

Resuming rather than reporting is what makes the recovery worth having. The
harness did not restart, its work was not lost, and in most cases the answer is
still coming. Declaring the turn dead would throw away a completed turn's
output to report an interruption the person waiting never needed to know about.

Records are left in place while a delivery is being resolved rather than
consumed on read, so a daemon interrupted *again* mid-recovery finds them a
second time. Resolving one twice edits the message already there; losing one
strands it forever. The asymmetry decides it.

## Consequences

- The window between accepting a delivery and resolving it must be fully
  covered by the record. Anything a channel places during that window — a
  placeholder, a reaction — has to be recorded as it is placed, or a restart
  strands exactly that.
- Reconciliation runs before the channel takes new work, so a reconnect cannot
  race a resumed turn for the same conversation.
- A resumed delivery inherits its original deadline. Restart loops cannot
  extend a turn indefinitely.
- Shared service state is now written on delivery acceptance, not only on
  routing changes.
- Records are keyed per channel. Two channels of one service using the same
  request id must not collide, and neither may reconcile the other's.
- Recovery is best-effort at the edges: a record whose channel context cannot
  be understood is dropped rather than acted on blindly.

## Non-Goals

- Surviving anything other than the daemon going away and coming back. A
  harness that dies with its session is a different failure.
- Replaying or re-submitting a turn. The delivery is waited on again, never
  sent again.
- Guaranteeing exactly-once channel output across repeated restarts mid-
  recovery. Editing the same message twice is accepted; stranding it is not.

## Examples

A question arrives in a channel and its turn is a long one. Four minutes in,
the daemon is restarted to pick up a new build. The channel comes back, finds
the delivery outstanding, reattaches to the still-running session, and — when
the turn finishes two minutes later — replaces the same placeholder with the
answer. Nobody in the channel learns a restart happened.

The same, except the session was deleted while the daemon was down. The
placeholder is replaced with a statement that the turn never finished, and the
delivery stops being tracked.
