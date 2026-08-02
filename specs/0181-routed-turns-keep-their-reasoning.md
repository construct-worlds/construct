# 0181-routed-turns-keep-their-reasoning

Status: accepted
Date: 2026-08-02
Area: protocol
Scope: A routed turn must hand a thinking target back the reasoning that target produced, without the harness having to carry it.

## Decision

When a target refuses an assistant turn that does not carry back the
reasoning it produced for that turn, the router supplies it:

- The router **captures** the reasoning a target streams, keyed by the
  tool-call ids of the turn it belongs to, and keeps it for the session.
- On a later request it **restores** that reasoning onto each replayed
  assistant turn that called a tool, in the field the target's dialect
  defines for it.
- A turn whose reasoning the harness carried itself is left alone: the
  harness's own account of its turn wins over the router's memory.
- A turn whose reasoning is no longer remembered is sent with an **empty**
  reasoning. The router never writes reasoning the model did not produce.
- Reasoning is remembered for the session but never re-encoded into the
  response the harness reads. It is the target's own record, not new
  assistant content, and materializing it as text would put words in the
  model's mouth.

Which targets need this is **measured, not assumed** — the same rule as
effort support (spec 0160). A target is marked as requiring the echo only
after its refusal has been observed.

An arm that must carry reasoning always rebuilds the request body, even
when the harness and the target speak the same dialect: byte-forwarding
cannot add a field.

## Reason

Reasoning models are increasingly stateful about their own thinking: the
provider will not re-derive reasoning it already produced, and refuses a
replayed tool-calling turn that arrives without it. DeepSeek's thinking
mode is the observed case — a turn carrying `tool_calls` is rejected
unless `reasoning_content` accompanies it, while the same turn with an
empty reasoning is accepted.

The harness cannot solve this. Most harnesses speak a dialect with no
field for another vendor's reasoning, so whatever the target reasoned is
gone by the time the harness replays the conversation. A harness that
does have such a field is not the one that produced the reasoning either.
The router is the only participant that sees both the target's response
and the request that replays it, so it is the only one that can keep them
consistent.

Depending on the target to recover its own reasoning is not a
substitute. A provider may resolve reasoning from a recently issued
tool-call id, but that recovery is a cache: it holds early in a
conversation and lapses later, which surfaces as a session that works for
several turns and then fails permanently mid-task.

## Consequences

- Sessions carry bounded per-session state that a stateless proxy
  otherwise would not. It is capped, and the oldest turns are forgotten
  first; a forgotten turn degrades to an empty echo, never to an error.
- A daemon restart loses the memory. Resumed conversations continue with
  empty reasoning on their older turns rather than failing.
- Reasoning is provider-private text. It stays inside the router: it is
  not persisted to the transcript, not shown as assistant output, and not
  forwarded to a different target than the one that produced it.
- Adding a dialect means deciding how it carries reasoning in both
  directions, not just how it carries text and tool calls.

## Non-Goals

- Rendering a routed model's reasoning to the user. Whether reasoning is
  surfaced in a client is a separate decision from keeping the turn valid.
- Turning reasoning into portable content moved between providers.
- Reconstructing reasoning for a turn the router never saw.

## Examples

- A harness calls three tools over three turns against a thinking target.
  Each replay carries that turn's own reasoning back; the conversation
  continues instead of failing on the second or third round.
- The same conversation after a daemon restart: every prior turn carries
  an empty reasoning, which the target accepts, and new turns start
  accumulating reasoning again.
- A target with no such requirement sees byte-identical requests to
  before; nothing is captured and nothing is added.
