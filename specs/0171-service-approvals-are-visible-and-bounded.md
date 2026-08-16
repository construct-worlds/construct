# 0171-service-approvals-are-visible-and-bounded

Status: accepted
Date: 2026-08-01
Area: architecture
Scope: What a service caller is told when a turn stops at a tool approval, and how long that turn may stay stopped.

## Decision

When a service session's turn stops at a tool approval, the service result says
so: that the turn is waiting on the user, which tool it is waiting on, a
summary of what that tool would do, and how long it has been waiting.

A service may bound that wait. When the bound elapses, the pending call is
denied on the caller's behalf, the turn resumes, and the result reports that
the action was refused rather than continuing to look unfinished. The bound is
per service and defaults to waiting indefinitely, so a user who has not
asked for a deadline remains the only one who can decide.

Approval state is derived from the session's own transcript rather than
tracked separately. A request is pending exactly while it is the last thing of
consequence in that transcript; once answered, the turn appends past it.

## Reason

A turn stopped at an approval is indistinguishable from a slow turn when
viewed from outside: both report only that no reply is ready. The caller has
no way to learn that progress now depends on a person, and no way to reach
that person. It polls a session that will not move.

The approval itself is right — a service session is prompted by a third party,
so a human deciding on consequential actions is the point. What was wrong was
telling the caller nothing about it, and letting an unanswered prompt hold a
turn open forever.

Denying on timeout is preferable to abandoning the turn, because a denial is
an answer: the harness resumes, explains that it could not act, and the caller
receives a reply. Defaulting the bound to "never" keeps that from silently
refusing work a user would have approved, at the cost of requiring them
to opt into the deadline.

Deriving state from the transcript avoids a second source of truth that could
disagree with what the session actually did, and it survives daemon restart
for free, because the transcript does.

## Consequences

- A caller can distinguish "still working" from "blocked on a human", and can
  surface the difference to whoever is waiting on the other end.
- The reported summary is written for the user, not the caller; it may
  describe internals, and it reaches whoever can reach the channel.
- Timeouts are evaluated when a result is requested, so a caller that stops
  polling leaves the turn parked. The user's own surfaces remain the way
  to notice and answer it.
- Denial is the only automatic outcome. Automatic approval is not offered
  here, because the party who would benefit from it is the party who cannot be
  trusted to ask for it.
- Any future change that records approval resolutions in the transcript must
  keep the positional rule true, or replace it deliberately.

## Non-Goals

- Letting the caller approve, deny, or influence the decision.
- Notifying the user through the channel the request arrived on.
- Per-tool or per-risk timeouts; the bound is per service.

## Examples

- A request triggers a file write. The caller polls and sees that the turn is
  waiting on the user, which tool it is, and that it has been waiting nine
  seconds. The user approves; the next poll returns the finished reply.
- The same service is configured with a bound. Nobody answers within it, so
  the call is denied, the turn resumes, and the caller receives a reply
  explaining that the action was refused. The action did not happen.
- A service with no bound configured waits as long as it takes, and reports
  the wait accurately the whole time.
