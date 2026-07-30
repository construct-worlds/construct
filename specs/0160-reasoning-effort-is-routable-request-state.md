# 0160-reasoning-effort-is-routable-request-state

Status: accepted
Date: 2026-07-29
Area: protocol
Scope: how a harness's reasoning-effort choice travels through routed requests and when the native picker may offer one.

## Decision

Reasoning effort is per-request state carried in the request body, not part
of the published model id. The routed model catalog advertises a real set of
selectable effort levels for a published model only when the route's target
has a verified way to honor them. A target may accept the effort knob
verbatim or map it onto a semantically similar native control. For every
unsupported target the catalog advertises a single provider-default level,
so the picker never offers a choice the route cannot honor.

The router's canonical request form carries the effort value so it survives
the rebuild/translation path, and the byte-forwarding path preserves it
implicitly. Anthropic targets map selected levels onto extended-thinking
budgets, with `minimal` as an explicit off position and default. Targets
without a verified mapping (including Gemini) drop the value rather than
guess.

## Reason

Encoding effort into model ids would multiply picker entries, complicate the
id codec shared with clients (specs 0157/0158), and desynchronize featured
model selectors. Harnesses like Codex already have a native per-model effort
selector driven by catalog metadata and already send the chosen effort in
the request body — the router only needs to declare honest capability
metadata and not lose the value in transit. Advertising levels that a target
silently ignores would misrepresent what the user selected.

## Consequences

- The published-id codec stays `(route, model)`; effort never enters it.
- Catalog generation must know, per route, whether the target accepts the
  effort knob verbatim, maps it onto a native control, or does not support
  it, and must default to the single-level advertisement for unsupported
  targets.
- The canonical request form preserves effort end-to-end for accepting
  targets; adding a new dialect or target requires deciding whether it
  carries the knob verbatim, maps it, or drops it.
- The advertised level set is a conservative intersection until routes carry
  per-model capability metadata.
- Anthropic `low`, `medium`, and `high` map to 4,096, 12,288, and 24,576
  thinking tokens. The router raises `max_tokens` above that budget and
  omits incompatible sampling controls. Forced-tool turns leave thinking
  off because Anthropic rejects that combination.

## Non-Goals

- Mapping effort onto Gemini or other unverified provider controls.
- Surfacing effort in Claude Code's gateway model list, which has no effort
  dimension.
