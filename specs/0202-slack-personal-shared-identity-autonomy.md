# 0202-slack-personal-shared-identity-autonomy

Status: accepted
Date: 2026-08-18
Area: ux
Scope: How a slack-personal operator's freedom to act is configured, given that the operator and the human share one Slack identity.

## Decision

Autonomy is not a single knob. It is a response mode assigned per scope: a
default policy plus per-conversation overrides, layered on the channel's
allowlists. The modes form a ladder; each rung adds visible action in Slack:

1. **Observe** — forward matching messages to the minibuffer as
   observations; never touch Slack.
2. **Acknowledge** — add a reaction so the sender knows the message was
   seen; post nothing.
3. **Draft** — compose the reply as a Slack draft in the user's account.
   The human reviews and sends it as themself.
4. **Ask to send** — the outbound message surfaces as a tool approval in
   the minibuffer; posting happens only on approval.
5. **Auto-send** — post directly, subject to the disclosure and grace
   options below.

Orthogonal to the ladder, a trigger policy defines what enters it. There is
no bot to mention, so each scope declares what counts as addressed to the
operator: every message in the conversation, messages mentioning the user,
messages matching a keyword, or direct messages only. Trigger and response
mode together are the whole autonomy configuration; a scope with neither
configured is not acted on.

Auto-send carries two options:

- **Disclosure**, default on: an automatic reply carries a marker (a
  signature line or reaction) telling recipients an agent wrote it.
  Turning disclosure off is an explicit, per-scope choice.
- **Grace period**: auto-send waits a configured time and yields silently
  if the human has already replied in the thread themself.

Approvals never move into Slack: rung 4 uses Construct's existing approval
surfaces, and no Slack message can approve anything.

## Reason

Every action this channel takes is indistinguishable from the human acting,
so the cost of a wrong action varies by conversation, not globally: a DM
thread with oneself can safely run at full autonomy while a customer channel
should never go past drafting. Per-scope modes match the risk to the place.

Drafting is the rung the shared account makes uniquely honest: the agent
does the work, the human's deliberate send makes the words theirs.

Disclosure defaults on because undisclosed impersonation of the user to
their colleagues is the failure mode this design must prevent; convenience
may not silently override it. The grace period makes the operator a backstop
for an absent human rather than a competitor to a present one.

## Consequences

- Adding a channel scope requires choosing (or inheriting) a trigger and a
  response mode; there is no implicit "reply to everything" state.
- The ingress sweep must keep observing threads the operator answered, so
  grace-period yielding and human-reply detection stay possible.
- Rungs 3 and 2 are only available on backends whose tool contract exposes
  drafts and reactions; a scope configured for an unavailable rung degrades
  to the next lower rung and reports that in channel status.
- Escalation (an observed scope suggesting it could have answered) is
  allowed only as minibuffer output, never as a Slack post.

## Non-Goals

- Modeling Slack-side presence or read state; deference derives from the
  thread's own messages, not from whether the human appears online.
- Autonomy configuration for the Socket Mode `slack` kind, whose bot
  identity already discloses itself; this decision exists because identity
  is shared.

## Examples

A scope covering the user's own DM-to-self is set to trigger on every
message with auto-send and disclosure off: it behaves as a private command
line to the operator.

A scope covering a team channel triggers on mentions of the user with mode
draft: a teammate's question produces a ready reply in the user's drafts,
and nothing in the channel changes until the human sends it.

A scope set to auto-send with a ten-minute grace period sees the human
answer after four minutes; the prepared reply is discarded and nothing is
posted.
