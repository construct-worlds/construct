# 0160-reasoning-effort-is-routable-request-state

Status: accepted
Date: 2026-07-29
Area: protocol
Scope: how a harness's reasoning-effort choice travels through routed requests and when the native picker may offer one.

## Decision

Reasoning effort is per-request state carried in the request body, not part
of the published model id. The routed model catalog advertises a real set of
selectable effort levels for a published model only when the route's target
accepts the effort knob verbatim (today: targets speaking OpenAI Responses).
For every other target the catalog advertises a single provider-default
level, so the picker never offers a choice the route cannot honor.

The router's canonical request form carries the effort value so it survives
the rebuild/translation path, and the byte-forwarding path preserves it
implicitly. Dialects whose reasoning controls have a different shape
(Anthropic thinking budgets, Gemini) drop the value rather than guess a
mapping; mapping effort onto unlike knobs is a separate, future decision.

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
  effort knob verbatim, and must default to the single-level advertisement
  when it does not.
- The canonical request form preserves effort end-to-end for accepting
  targets; adding a new dialect requires deciding whether it carries the
  knob verbatim, drops it, or (future) maps it.
- The advertised level set is a conservative intersection until routes carry
  per-model capability metadata.

## Non-Goals

- Mapping effort onto Anthropic thinking budgets or other unlike controls.
- Surfacing effort in Claude Code's gateway model list, which has no effort
  dimension.
