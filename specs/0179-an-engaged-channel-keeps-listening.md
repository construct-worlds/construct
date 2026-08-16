# 0179-an-engaged-channel-keeps-listening

Status: accepted
Date: 2026-08-02
Area: ux
Scope: When a service channel treats a message as addressed to it, and what surrounding conversation it may read.

## Decision

A bot that must be re-addressed for every turn cannot hold a conversation. Once
a channel has been addressed, it may keep answering in the conversation it was
addressed in, and it may read that conversation's earlier messages so it
understands what it was pulled into.

Both are bounded, and the user sets the bounds per channel.

**Engagement is derived, never stored.** A channel is engaged in a conversation
exactly when it already routes a session for that conversation. There is no
second record of "the bot is participating here" that could disagree with the
routing table, drift after a restart, or need expiry.

**How far engagement reaches is configurable, because the right answer differs
per channel.** A dedicated channel wants the bot to answer everything; a busy
shared channel wants it to stay inside the thread it was called into and
nowhere else. Not answering untagged messages at all remains available and is
what an unconfigured channel does when the transport cannot deliver them.

**One message is one turn, regardless of how many subscriptions carried it.**
A transport may deliver the same message more than once under different event
identities. Deduplication is therefore keyed on the message, not on the
delivery.

**Conversation the bot did not solicit is untrusted input.** History is read
only when joining a conversation already in progress, and it reaches the
session inside a boundary that marks it as material to read rather than
instructions to follow. It is never fetched again once the session has been
present for the conversation itself.

## Reason

The unit of conversation is the thread, and a participant who has been brought
into a thread is expected to keep participating in it. Requiring a mention per
message makes the bot conspicuously less capable than any human in the channel.

Reading history is what makes joining useful at all. "What do you think?" is
unanswerable from the mention alone; the question is about everything said
before it.

But widening who can put text in front of an agent that holds tools is a real
change in exposure. Before this, only the person who addressed the bot supplied
input. After it, everyone in the conversation does — so the boundary has to be
explicit, the scope has to be narrow, and the user has to be able to turn it
off. Marking untrusted text is not a guarantee against injection; it is the
minimum owed, and the narrow scope is what limits the damage.

## Consequences

- Engagement cannot be granted or revoked on its own. Ending a conversation
  means the session for it stops existing.
- A channel whose transport lacks the subscription for untagged messages
  silently behaves as if follow-up were off. That is correct, not a failure.
- Reading history needs a permission a deployment may not have granted.
  Refusal costs context, never the answer.
- Widening what a channel reads means widening what an untrusted author can put
  in front of the agent. Any future widening carries that cost and must be
  weighed as such, not treated as more of the same.

## Non-Goals

- Following a conversation the bot was never addressed in.
- Treating history as authoritative over the user's own instruction.
- Expiring engagement on a timer; the conversation's own lifetime bounds it.

## Examples

Someone mentions the bot in a thread that already has ten messages. The bot
reads those ten as marked-untrusted background, answers, and answers subsequent
replies in that thread without being mentioned again. A message in a different
thread of the same channel is ignored unless the user widened the channel's
follow-up, and a message in a channel the bot has never been addressed in is
always ignored.
