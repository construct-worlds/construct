# 0177-operator-deliveries-reach-sessions-as-native-input

Status: accepted
Date: 2026-08-02
Area: harness
Scope: How an inbound operator channel delivery reaches the session that must act on it.

## Decision

A channel delivery must actually start a turn in the session it is routed to.
Construct is responsible for the framing that makes that true; a operator
channel never reasons about terminals, keystrokes, or harness startup.

Two rules follow from that, and they are different because the session is in a
different condition in each case.

**The first delivery of a conversation is the session's seed prompt.** It
travels through session creation, as structured data, and the adapter starts
its first turn from it natively. The daemon does not create an idle session and
then write the opening message into it.

**Every later delivery is text spoken to a live session**, and is framed the
way that session's harness accepts a submitted turn: a bracketed paste followed
by a gated Enter for an agent TUI, a CR-terminated write for a harness running
its own line editor, and a structured adapter input for a headless harness. A
delivery is not complete when the bytes are written — only when the harness has
taken them as a turn.

An inbound delivery is recorded in the session transcript as a user turn
regardless of which framing carried it, so the channel's request is visible in
the session's own history and not only in channel state.

## Reason

An interactive session is a real agent TUI attached to a real PTY, and a TUI
imposes two conditions that plain "write the text into the session" ignores.

It is not listening yet when it is young. A cold-started TUI does not attach
its input handler until well into its startup draw, and the terminal discards
whatever was written before the harness switches it into raw mode. An opening
message written straight after spawn is therefore silently lost — the session
exists, the channel believes it delivered, and nothing ever runs.

It does not treat LF as Enter once it is listening. A terminal's Enter key
sends CR; in raw mode LF is a different keypress entirely, and agent TUIs bind
it to "insert a newline." Text terminated with LF is faithfully typed into the
composer and left sitting there, so the session looks like it received the
message and simply chose not to answer.

Both failures are silent and both look identical from the channel: a delivery
that was accepted and produced no reply. Putting the framing decision in
Construct — keyed off the harness, next to the readiness signal the daemon
already tracks — is what keeps a channel from having to know any of this.

## Consequences

- Adding an interactive harness means declaring how a submitted turn is framed
  for it. A harness with no declaration must not silently fall back to a
  framing that types without submitting.
- The seed-prompt path stays free of terminator concerns: it is structured
  data, and adding a CR or LF to it is always wrong (see 0046).
- Who records the inbound delivery depends on the harness: Construct speaks for
  a harness that does not mirror its own user turns, and stays quiet for one
  that does. Adding a harness means saying which it is; getting it wrong shows
  the caller's message twice or not at all.
- Delivery-side failures stay attributable: a delivery that cannot be framed or
  cannot reach its session is an error to the channel, not a silent drop.

## Non-Goals

- Inferring that a turn started by watching terminal output.
- Making channels harness-aware, or letting a channel choose framing.
- Changing how a session replies to a delivery — that is 0176.

## Examples

A Slack thread's first message creates an interactive Codex session; Codex
boots with the message already as its prompt and begins work without any
follow-up nudge. The next message in that thread is pasted into the running
Codex composer and submitted, and appears in the session transcript as a user
turn. A headless operator session receives both as ordinary structured input.
