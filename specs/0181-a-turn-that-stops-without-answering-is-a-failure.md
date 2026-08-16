# 0181-a-turn-that-stops-without-answering-is-a-failure

Status: accepted
Date: 2026-08-02
Area: architecture
Scope: How a service delivery learns that the turn behind it has ended without producing an answer, and what it reports when that happens.

## Decision

A turn that has stopped is finished, whether or not it answered. A delivery
waiting on one must reach that conclusion by **observing the session**, not by
waiting for the harness to declare a failure.

A session is treated as having stopped without answering when, continuously
and for long enough to be sure, it is idle, has no input queued, is not parked
at a tool approval, and is appending nothing to its transcript. Reaching that
state without the delivery's answer ends the wait as a failure.

Two constraints on the observation:

**Only progress counts as activity.** A harness with a live terminal repaints
constantly, including while idle. Redraws must not read as work, or a session
that draws its own cursor will never look stopped.

**Not-yet-started is not stopped.** A session is briefly idle between receiving
input and picking it up. A turn never observed running gets a longer grace than
one that ran and came back.

When a turn does fail, what the channel reports is **the harness's own words
where it left any** — including text it only drew on its terminal. Failing
that, it says plainly that the session stopped without replying. It never
reports a timeout for a turn that did not time out.

## Reason

The daemon does not run the model. It supervises a harness, and a harness that
loses its upstream mid-turn is under no obligation to say so in a way the
daemon can read: an interactive one prints the failure into its viewport and
returns to its composer. From outside, that is a session sitting at "awaiting
input" — the same shape as a turn that finished normally, differing only in
having produced nothing.

Every failure signal that waits to be *told* therefore misses this case, and
what is left is the delivery's own expiry. That is a wait measured in tens of
minutes, ended by a message that names the timeout rather than the cause, while
the actual explanation sat legible on a screen the whole time. Observing the
session instead collapses that to seconds and lets the report say what broke.

Quoting the harness is what makes the report worth reading. "The turn ended
without an answer" tells the person waiting only that they are still stuck;
"stream disconnected before completion" tells them it was the network and that
sending the message again is a reasonable thing to do.

## Consequences

- Idle, quiet, unqueued, and unparked must **all** hold, continuously, before a
  turn is called over. Any one of them alone will misfire on a normal turn.
- Approval parking is load-bearing here: it is the one stop that is supposed to
  last, and it must keep reading as waiting rather than as failure.
- Recovering error text from a terminal is best-effort and is only ever
  consulted for a turn already known to have failed. A wrong guess costs a
  slightly-off sentence in a failure notice; it can never cost an answer or
  turn a success into a failure.
- The delivery expiry stops being the mechanism that ends a failed wait and
  goes back to being a backstop.
- Failure reports carry harness text, which is written by the harness and may
  be long or oddly formatted. It is bounded before it is quoted.

## Non-Goals

- Distinguishing *kinds* of failure, or retrying one. The delivery reports what
  happened and stops.
- Parsing harness terminals for anything other than a failure notice.
- Making a harness that fails silently start reporting properly. This observes
  from outside precisely because that cannot be relied upon.

## Examples

An interactive session loses its connection to the model provider ten minutes
into a turn, prints a two-line error, and returns to its prompt. Within seconds
the waiting channel stops saying it is working and posts the harness's error
instead, naming the endpoint that failed.

A turn runs for six minutes across a dozen tool calls with pauses between them.
None of those pauses ends the wait, because each one is interrupted by
something new appearing in the transcript.

A turn stops at an approval and stays there for an hour. It is never reported
as failed; the channel keeps saying a user has to act.
