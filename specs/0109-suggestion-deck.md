# 0109-suggestion-deck

Status: accepted
Date: 2026-07-24
Area: ux
Scope: On-demand, generated next-prompt suggestions ("the suggestion deck") for any harness session.

## Decision

Construct can generate a hand of suggested next prompts for a session
that is awaiting input: one top pick plus a few generated "verbs"
(directions), each holding concrete prompt cards. The contract:

- **On demand only.** Generation runs when the user asks for it (the
  orb / an explicit request), never automatically on turn end. Idle
  sessions cost nothing.
- **Daemon-generated from the normalized transcript, never
  adapter-generated.** The daemon owns generation so every harness —
  current and future — gets suggestions without per-adapter work.
- **Same-harness generation.** Suggestions are produced by the same
  harness the session runs, so they draw on the same credentials or
  subscription the user's session already uses. Harnesses that can fork
  their native conversation generate through a hidden fork of the
  target session — reusing the provider's prompt cache over the full
  history and predicting from complete context. Harnesses that cannot
  fork get a hidden one-shot fed a rendered transcript tail.
- **Ephemeral.** A hand is broadcast to live clients and never
  persisted. It describes exactly one turn boundary: the session
  running again, a user message, or a terminal state invalidates it.
- **All generated, fixed shape.** Every string in the hand (top pick,
  verb labels, cards) is model-generated per turn; only the structure
  (one top, few verbs, few cards, clamped lengths) is fixed, enforced by
  a single shared parser so every generator obeys the same contract.
  Generators additionally receive a bounded block of the user's recent
  prompts from the global prompt history (spec 0155) as voice/workflow
  context; clients may present that history alongside the hand as a
  clearly-separate verbatim-recall selection, which is the one
  non-generated surface the deck offers.
- **Accepting a suggestion sends through the ordinary session-input
  path.** The deck has no send machinery of its own.
- **Typing always wins.** The suggestion UI may only consume input
  while explicitly open, and any printable key must close it and take
  its normal route. Suggestions are an offer, never a gate.

## Reason

Most next prompts are highly predictable from the transcript, but a
single completion string cannot cover the space of user intents. A
small generated hierarchy (top pick → verbs → cards) covers the common
cases with one or two activations while staying cheap. Same-harness
generation avoids a second credential requirement and, via native
forks, makes the marginal cost of a suggestion request close to one
appended message. On-demand generation keeps a fleet of idle sessions
from burning tokens.

## Consequences

- Clients render the hand however fits their surface (TUI corner orb →
  fan → stack; web can differ) but must preserve the invalidation and
  typing-always-wins rules.
- Hidden generation sessions must be torn down after use and must never
  appear in user-facing session lists.
- A generation result must be discarded if a newer turn started while
  it was in flight.
- Adding a harness automatically adds suggestion support at the
  rendered-tail tier; adding native-fork support upgrades it.

## Non-Goals

- Automatic (turn-end) generation, auto-send, or any form of
  suggestion that acts without explicit user acceptance.
- Fill-in-the-blank template cards and acceptance-feedback learning are
  future extensions, not part of this contract.
