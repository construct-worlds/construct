# 0116-cross-dialect-routing-is-a-translation

Status: accepted
Date: 2026-07-29
Area: protocol
Scope: What a route does when the endpoint it targets speaks a different wire dialect than the harness being routed.

## Decision

Translation goes through a **canonical intermediate form**, never pairwise.
Each dialect contributes a parser and an emitter; a request is parsed from
the harness's dialect into the canonical form and emitted in the target's,
and a response stream is decoded from the target's event vocabulary into
canonical events and re-encoded into the harness's. Adding a dialect costs
two pieces, not one per existing dialect.

A route may target an endpoint whose wire dialect differs from the one the
harness speaks. When it does, Construct **translates**: it rebuilds the
request in the target's dialect and re-encodes the response stream back
into the harness's. The harness is never told, and never has to be — it
sends and receives exactly the protocol it already speaks.

Route targets are the shared **model profiles**, not a routing-specific
list. An endpoint is declared once and is reachable both by the built-in
agent and as a route target.

Support is per-dialect-pair and explicit:

- Same dialect on both sides → no translation. The request is forwarded
  with its destination, credential, and model substituted.
- A pair with a translator → translated.
- A pair without one → the profile is **listed and not selectable**, with
  the missing translator named as its reason. It is never silently
  attempted.

The maintained canonical adapters cover Anthropic Messages, Google Gemini
GenerateContent, OpenAI Chat Completions, and OpenAI Responses. Azure OpenAI
uses the Responses adapter with Azure's `api-key` authentication rather than
being treated as a separate JSON dialect.

Translation is lossy in one direction only, and only where the target has
no equivalent concept. Losses must drop information, never invent it: a
construct with no counterpart is omitted, not approximated into something
the model would read as content.

## Reason

Restricting routes to same-dialect targets made the feature almost
useless: the endpoints a user already has configured are mostly not in the
dialect their harness speaks, so the picker filled with entries that could
be seen but not chosen. The dialect is an implementation detail of the
endpoint, not a property the user is choosing between.

Sharing the profile registry rather than defining routing-specific targets
removes a second place to declare the same endpoint, and with it the
chance of the two drifting apart — two declarations of one endpoint
eventually disagree about its URL, key, or model.

Refusing unsupported pairs outright, rather than attempting a best-effort
mapping, is the same principle the routing design rests on elsewhere: a
wrong translation does not fail cleanly, it corrupts a turn in ways that
look like the model behaving badly. A missing translator is a
configuration message; a guessed one is a bug report about the wrong
component.

## Consequences

- Every supported dialect is a maintained parser and emitter, and each is
  a standing cost: the request shape, the streaming event vocabulary, the
  tool-call encoding, and the stop/finish taxonomy all evolve
  independently. Adding a dialect means committing to that, not just
  adding a case.
- Provider-specific wire constraints are enforced at the final emission
  boundary. In particular, Gemini tool schemas and function names are
  constrained to its documented subset, with request-scoped reversible name
  mappings so the harness still receives the exact tool name it registered.
- A dialect's event vocabulary must be established from a captured real
  exchange, not from memory or documentation. Streaming formats carry
  structure that is easy to get subtly wrong and whose failure mode is a
  stream that parses but displays nothing.
- Streaming translation must preserve **framing**, not only content. A
  dialect that brackets content blocks with explicit start/stop events
  cannot be fed a flat delta stream: an unbracketed or misnumbered stream
  renders as nothing, which is worse than an error.
- A translated stream must always terminate as a complete turn, including
  when the upstream produces nothing or fails midway. A truncated stream
  leaves the harness waiting.
- Endpoints that the target dialect does not implement at all (auxiliary
  ones such as token counting) must be answered locally rather than
  refused, where refusing would break the harness's own bookkeeping. Such
  answers are approximations and must be understood as such — they are
  never presented as authoritative.
- Because the harness keeps speaking its own dialect, anything it derives
  from its own protocol — context accounting, model identity, cache
  behavior — reflects its beliefs, not the target's reality. Displaying
  the substitution is what keeps that honest
  ([0114](0114-session-route-is-durable-session-state.md)).
- Prompt caching, provider-specific request fields, and reasoning content
  generally do not survive translation. A routed turn can therefore cost
  or perform differently than the same turn unrouted.
- A harness's dialect is not always a property of the harness. Some
  harnesses are provider-agnostic: the dialect they emit, and the host
  they emit it to, follow whatever provider the user configured. For those,
  a per-harness declaration is wrong by construction, and the dialect must
  be recognized from the intercepted request itself. A declaration table is
  an optimization for harnesses that speak exactly one dialect, not the
  general mechanism.

## Non-Goals

- Translating between arbitrary dialect pairs on demand. Only pairs with a
  written, tested translator are offered.
- Preserving provider-specific features across a translation.
- Making a routed session indistinguishable from an unrouted one. The
  substitution is deliberately visible.

## Examples

A profile declaring an OpenAI-compatible endpoint is selected for a
harness that speaks Anthropic Messages. The system prompt moves into the
message list, tool definitions become function definitions, an
assistant tool call plus its result become an assistant `tool_calls`
message followed by a tool-role message, and the target's flat streaming
deltas are re-emitted as explicitly bracketed, indexed content blocks.
Reasoning blocks, which the target has no equivalent for, are dropped
rather than replayed as assistant text.

A profile declaring an endpoint whose dialect has no translator appears in
the picker greyed out, labelled with the provider that lacks one.
