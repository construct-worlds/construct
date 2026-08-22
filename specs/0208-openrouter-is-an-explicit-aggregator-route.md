# 0208-openrouter-is-an-explicit-aggregator-route

Status: accepted
Date: 2026-08-22
Area: harness
Scope: OpenRouter is a first-class smith provider and built-in route target, selected only by explicit prefix, with cost taken from the wire.

## Decision

OpenRouter is supported as a thin variant of the OpenAI chat-completions
provider: its own provider prefix, its own key env var, and a built-in
route target (spec 0179) at its single public endpoint. Three rules hold:

1. **Explicit selection only.** There is no bare-name sniff for OpenRouter
   model ids. A `vendor/model` slash id typed without a prefix must not
   route to OpenRouter.
2. **Cost comes from the wire, not a price table.** Requests to OpenRouter
   ask for its usage accounting, and the per-generation USD cost reported
   in the response is what Construct records. No per-model price list is
   maintained for routed models.
3. **The default model is the aggregator's own meta-router id**
   (`openrouter/auto`) — the only id every OpenRouter account can serve.
   Specific routed models are always a user choice.

## Reason

OpenRouter serves new and stealth models before they have official SDKs or
dedicated adapters, so an aggregator route makes any such model reachable
the day it appears, with no per-model code. But an aggregator is a distinct
billing path, and its slash-shaped ids collide with other namespaces
(local runtimes also use `host/user/model` names) — so selecting it stays
an explicit act, consistent with the rule that switching the endpoint or
billing path is never inferred (spec 0028). The models behind it are
heterogeneous and change without notice, which is why cost must be read
from each response rather than curated, and why no specific routed model
can be a default.

## Consequences

- Reasoning-effort support for OpenRouter targets is advertised only after
  the underlying model is measured to grade it (spec 0160's model-aware
  rule); until then no effort scale is offered.
- Context-window budgeting uses a conservative floor plus the runtime
  overflow-learning path, since no single window is true across routed
  models.
- Curated model lists may include the stable meta-router id and, as
  examples, currently-live routed ids; routed ids (stealth ones
  especially) rotate, and staleness there must degrade to "typed ids still
  work", never to a broken route.

## Non-Goals

- Per-model metadata sync from OpenRouter's catalog (context windows,
  pricing) into Construct's own tables. The catalog may later inform the
  provider's runtime context-window hook, but is not a source for curated
  lists.
- OpenRouter-specific routing features (provider ordering, fallback
  chains, nitro/free variants) beyond passing the user's model id through
  verbatim.

## Examples

- A user with only an OpenRouter key gets a working session on the
  meta-router by default, and can pin any routed id explicitly.
- A stealth model id typed with the OpenRouter prefix works the day the
  model appears, and its exact cost shows up in the session's usage.
- The same id typed bare falls through to the local-runtime fallback, not
  to OpenRouter.
