# 0215-smith-provider-token-counts-are-authoritative

Status: accepted
Date: 2026-09-09
Area: harness
Scope: Smith uses provider input-token usage as the source of truth for context pressure.

## Decision

After a successful model call, Smith treats the provider-reported total input
token count as authoritative for context-window state, compaction, pruning, and
limit-probe decisions. Before the next call, content added or removed since the
reported request is represented by a character-based delta estimate anchored
to that report.

When a provider omits input usage, or a resumed session has not yet completed a
new model call, Smith may estimate the full pending request by characters. That
fallback is preflight-only and must not be presented as provider usage. A later
provider report replaces the fallback immediately.

Providers whose APIs split prompt usage into fresh, cache-write, and cache-read
fields must report their sum as total input occupancy. A cached-token metric may
remain the cache-read subset, but it must not be subtracted from the context
total.

## Reason

Provider tokenizers account for message framing, tool schemas, cache prefixes,
and model-specific tokenization that a character ratio cannot reproduce. Using
the heuristic after a real count is available can compact too early or overrun
the actual window. New input still needs a preflight estimate because no
provider can report a request it has not received yet.

## Consequences

- Context mutations between calls are estimated relative to the latest real
  count, rather than re-estimating the entire prompt.
- Missing usage remains an explicit state; zero must not silently mean unknown.
- Switching providers or resetting a conversation invalidates the prior anchor.
- Overflow errors and learned limits remain safety mechanisms, not substitutes
  for successful-call usage.

## Non-Goals

This does not add a local provider tokenizer or persist an inferred token anchor
across process restarts.
