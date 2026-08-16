# 0173-operator-definitions-apply-without-restart

Status: accepted
Date: 2026-08-01
Area: architecture
Scope: When an edit to a operator definition reaches the running daemon, and how that schedule is stated to the user.

## Decision

A operator definition is applied to the running daemon when it is saved. No
restart is involved, whether the edit arrived over IPC or was made by hand in
the configuration directory.

Each part of a definition has one propagation class, and the class is a
property of where the daemon reads that field:

| Change | Applies |
|---|---|
| Channel attachment, port, enabled state, credential, behavior options | immediately |
| Routing rule, paused, approval timeout | on the next request |
| Instruction, harness, model, working directory, sandbox | on new sessions |

The classes are published as data that both the daemon and its clients read,
so the promise shown beside a field is the promise the daemon keeps. Adding a
field to a definition means giving it a class.

**Nothing is re-applied to a running conversation.** A session keeps the
instruction, harness, model, and confinement it was created with, for its whole
life. This is a limit of the harnesses, not a choice about safety: a harness
assembles its system prompt when it starts, and resuming one deliberately skips
its seed prompt so the model is not charged for it twice. There is therefore no
point at which a changed instruction could be delivered to a live session
except as a new message, which is a different thing from re-instructing it.

**Reload is all or nothing.** If any definition fails to parse, the running
configuration is left exactly as it was. Partial application would leave the
minibuffer unable to say what is running, and it is what makes watching the
configuration directory safe: a file caught mid-write simply fails to parse and
is picked up once it is complete.

**Only a port change moves a socket.** Every other field is read where it is
used, so a rotated credential, a changed routing rule, or a paused operator take
effect without disturbing the listener. When sockets must move, every stop
completes before any start, so two channels can exchange ports.

An outbound channel is the mirror image: it holds its configuration in the
connection rather than reading it per request, so any change to that
configuration replaces the connection. That is why a channel's behavior options
sit in the immediate class — not because they touch a port, but because saving
one is what makes the running connection adopt it.

**Requests in flight are never interrupted.** Stopping a listener stops it
accepting; a request already being served runs to completion, because a operator
request has an agent turn behind it. Connections drain, sockets rebind.

An accepted edit reports what it did — started, stopped, and rebound channels,
and any that could not be bound. A channel that fails to bind does not fail the
edit: the definition is saved, and the report says which part is not live.

## Reason

Definitions describe a running operator, so an edit that requires a restart is
an edit that has not been made. Worse, the previous behavior was silently
partial: `paused` was consulted only when the daemon started, so pausing a
operator left it serving, and rotating a credential left the old one working.
A user reading the configuration could not tell what the daemon was doing.

Publishing the propagation classes as shared data, rather than as prose in each
client, is what keeps the answer honest as the code changes. Prose drifts from
behavior silently; a shared table drifts loudly.

The all-or-nothing rule and the stop-before-start ordering both exist because
the failure modes are quiet. A half-applied reload and a port that was released
after its replacement tried to bind are each states a user would discover
much later, through a operator that simply is not answering.

## Consequences

- A operator that has been paused in configuration stops serving as soon as the
  daemon notices. Installations that paused a operator while relying on it
  continuing to answer will see it stop.
- Adding a field to a operator definition means assigning it a propagation
  class; a field with no class is an unanswered question at the point of edit.
- Per-operator runtime state — which session serves which key, and which
  deliveries have already been handled — must survive a reload. Rebuilding it
  would silently strand live conversations and admit duplicate deliveries.
- Two channels configured on one port cannot both bind. The conflict is
  reported and the already-claimed port is left with its current owner, rather
  than letting the later definition take it.
- Watching the configuration directory means edits are noticed within a short
  interval rather than instantly; the schedule above is about what applies, not
  about the latency of noticing a hand edit.
- A future harness that can be re-instructed or re-modelled in place would let
  those fields move to a shorter class. Until then, claiming they apply sooner
  would be false.

## Non-Goals

- Re-instructing, re-modelling, or re-confining a conversation that is already
  running.
- Budgets. No budget field exists; the guarantee here is structural — a scalar
  added to a definition is read where it is used and therefore applies on the
  next request without further work.
- Draining or migrating in-flight requests across a rebind.

## Examples

- A user edits a definition file in a text editor and saves. Within a
  couple of seconds the new routing rule governs the next request; no restart,
  no signal, no command.
- A credential is rotated. The next request with the old secret is rejected and
  the next with the new one is accepted, and the listener never moved.
- A operator is paused. Its port is released and callers are refused. Resuming
  it binds the port again.
- Two channels are given the same port by hand. The one already serving keeps
  it; the other is reported as unbound, and the rest of the reload still
  applies.
- A definition is saved with a syntax error. Every operator keeps running
  exactly as before, and the corrected file is picked up when it is saved.
