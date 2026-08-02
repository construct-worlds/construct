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

Effort support is a property of the **target and model together**, not of
the provider alone. A vendor may grade effort on one model and floor every
level to the same value on another; each model advertises only what it was
observed to honor. "Verified" means measured against the live API, not
inferred from vendor documentation — where the two disagree, the measurement
governs, and levels whose effect cannot be distinguished from run-to-run
variance are not offered.

The router's canonical request form carries the effort value so it survives
the rebuild/translation path, and the byte-forwarding path preserves it
implicitly. Anthropic targets map selected levels onto extended-thinking
budgets, with `minimal` as an explicit off position and default. Targets
without a verified mapping (including Gemini) drop the value rather than
guess.

Subscription targets may expose a provider-specific scale when their native
client publishes one. Grok accepts `low`, `medium`, and `high` verbatim and
defaults to `high`. Kimi K3 accepts `low`, `high`, and `max` through
`output_config.effort` while thinking remains enabled; Codex presents Kimi
`max` as its native `xhigh` level. Kimi models that publish no selectable
effort scale do not advertise one.

API-key targets may expose one on the same terms. DeepSeek accepts a seven
value effort enum, but only `low`, `high`, and `max` were measured to grade
the work monotonically, and only on its flash tier — its pro tier floors
every level to one default. So flash offers those three with `high` as its
default, pro offers none, and the levels the enum accepts but that showed no
separable effect are left out.

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
- Catalog generation must know, per route *and model*, whether the target
  accepts the effort knob verbatim, maps it onto a native control, or does
  not support it, and must default to the single-level advertisement for
  unsupported targets. Resolving a route must derive its effort scale from
  the model it actually resolved, or an armed route can carry a scale
  belonging to a sibling model.
- The canonical request form preserves effort end-to-end for accepting
  targets; adding a new dialect or target requires deciding whether it
  carries the knob verbatim, maps it, or drops it.
- The advertised level set stays conservative: a level the API accepts is
  not thereby offered. Adding one requires evidence it changes the work,
  which means a level set can shrink when a vendor changes a model's
  behavior, and a new model of an existing provider starts with no scale
  until measured.
- Anthropic `low`, `medium`, and `high` map to 4,096, 12,288, and 24,576
  thinking tokens. The router raises `max_tokens` above that budget and
  omits incompatible sampling controls. Forced-tool turns leave thinking
  off because Anthropic rejects that combination.
- Grok's scale is forwarded as `reasoning_effort`, with `high` as the
  catalog default.
- Kimi K3 requests carry `thinking: {type: enabled}` and map Codex
  `low/high/xhigh` onto Kimi `low/high/max`. K3 is always-thinking, so its
  picker does not offer an off position.
- DeepSeek's scale is forwarded as `reasoning_effort`, with `high` as the
  catalog default. Its enum also accepts an off position, which the picker
  does not currently expose because Construct's effort scales are graded
  rather than on/off; a future off-position concept could adopt it.

## Non-Goals

- Mapping effort onto Gemini or other unverified provider controls.
- Surfacing effort in Claude Code's gateway model list, which has no effort
  dimension.

## Related

- [0165](0165-pin-router-selects-reasoning-effort.md) — Construct's pin
  dialog may store a preferred effort on the durable session pin and inject
  it on pin-routed requests; native catalog picks still own effort via the
  request body as defined here.
