# 0178-a-channel-shows-that-a-turn-is-still-running

Status: accepted
Date: 2026-08-02
Area: ux
Scope: What a operator channel tells the person waiting while a turn it accepted has not answered yet.

## Decision

A channel that accepts a delivery and then goes silent is indistinguishable
from one that dropped it. A channel may therefore show that a turn is still
running, and the user chooses how visible that is per channel — including
turning it off.

Four rules constrain it.

**Silence is correct for a turn that answers promptly.** The affordance exists
for a wait that has already become long enough to look like a failure, so it
appears only after such a wait. A quick turn leaves no trace of one.

**It must keep looking alive.** An affordance whose words have not changed in
twenty minutes reads as abandoned, which is the impression it exists to
prevent. Past the point where the wait is worth remarking on, it carries
something that visibly advances — how long this has been going.

**It must not claim to be working when it is not.** A turn stopped at a tool
approval is not making progress and will not resume until a human acts at
another surface entirely. Whatever the channel is showing has to change to say
so, naming what is being approved.

**It must resolve.** Whatever the affordance put in the channel is replaced by
the outcome — the answer, or a statement that the turn ended without one. A
turn that fails must not leave "working on it" standing forever, which means a
failure is now reported to the channel rather than only to the daemon's log.

The affordance is presentation, so it belongs to the channel. The channel does
not inspect sessions to build it: the ingress publishes what the turn is doing,
and publishing is advisory — nothing about a turn's progress or outcome depends
on anyone rendering it.

## Reason

The person waiting is not the user. They cannot see the session, the
harness, or the daemon log, so from their side a slow turn and a lost one look
the same, and the reasonable response to both is to send the message again —
which starts a second turn in the same thread and makes things worse.

The approval case is the sharpest version. The turn is stopped, and only a
human at the TUI can unstick it. Reporting that plainly turns an unexplained
silence into an action someone can take.

Making it configurable acknowledges that channels differ: a busy shared channel
may want nothing, a quiet one may want the acknowledgement, and some
affordances cost permissions the user's workspace may not have granted.

## Consequences

- A new affordance must degrade to silence, not to an error. Anything cosmetic
  that the workspace refuses is logged and the answer still gets delivered.
- The channel is the only place that renders progress. New progress states are
  added to what the ingress publishes; a channel that does not know a state
  keeps working.
- Turn failures are now visible to the channel's users, not only in the log.
- The delay before showing anything is a product decision about what counts as
  a long wait, not a tuning knob for load.

## Non-Goals

- Streaming a turn's partial output, tool calls, or reasoning to the channel.
- Making progress delivery reliable or ordered; it is a best-effort hint.
- Giving the channel a way to answer an approval. Approvals stay with the
  minibuffer.

## Examples

A Slack thread asks a question that takes two minutes. After a few seconds the
channel says it is working; the turn then stops at an approval and the same
message changes to name the tool awaiting sign-off; when the user approves
and the turn finishes, that message becomes the answer. A question answered in
three seconds produces just the answer, with nothing before it.
