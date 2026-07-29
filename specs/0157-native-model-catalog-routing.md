# 0157-native-model-catalog-routing

Status: accepted
Date: 2026-07-29
Area: harness
Scope: How Construct routes models selected by a harness or its native subagents without replacing the harness's delegation machinery.

## Decision

Construct publishes available route/model pairs into a supported harness's
native model catalog by default. Running `construct new <harness>` is the
scope of this integration: Construct discovers usable provider logins and
declared endpoints, then injects a session-local catalog without requiring
per-harness model configuration. Routing and publication remain separately
configurable opt-outs.

Each published entry has a stable, human-readable, collision-safe model id
in Construct's namespace that reversibly identifies both the route and
target model. It contains exactly one slash for Codex metadata lookup and
percent-encodes separators inside either component. The harness carries
that id on every model request, including requests made by native
subagents. The proxy resolves it per request:

1. A valid Construct id selects its encoded route and model.
2. A native model id uses the session's manually pinned route, if one is
   armed.
3. A native model id with no pin goes to the exact origin named by the
   harness, retaining its native credential.
4. A malformed, unavailable, or stale id in Construct's namespace fails
   closed. It is never sent to the native provider.

Construct integrates through a session-scoped catalog override and never
edits the harness's persistent configuration or replaces its provider
endpoint. Native catalog entries remain present alongside Construct
entries. The first implementation supports Codex; other harnesses need
their own verified catalog adapter but use the same publication and
request-routing contract.

## Reason

The harness already owns model selection and delegation. Publishing routes
as native models lets its built-in picker, configuration, and subagent
scheduler choose different models without Construct inventing a parallel
delegation system or pinning an entire session to one target.

A request-carried alias is also more precise than mutable session state:
concurrent parent and subagent requests can select different routes without
racing over a shared pin.

Session-scoped generated catalogs preserve the user's native catalog and
avoid durable edits that could affect harnesses launched outside
Construct. Keeping endpoint selection in proxy transport preserves the
origin-safety rule in [0113](0113-model-routing-is-proxy-transported.md).

## Consequences

- Publication advertises only routes that are currently selectable for the
  harness. Credentials never appear in catalog entries or model ids.
- Installed harnesses are useful discovery signals, but a model is
  published only when Construct can verify a usable credential or a
  configured endpoint. A binary alone does not imply model access.
- Subscription credentials are discovered read-only from supported
  harness-owned stores. Custom API endpoints remain explicitly declared
  because they cannot be inferred safely.
- The published id, not display text or catalog order, is the routing
  authority.
- Request selection overrides a manual session pin for that request only.
  The pin remains unchanged for native model ids and future requests.
- Catalog-enabled sessions inspect only the harness's fixed model host.
  Other destinations remain blind tunnels.
- After inspecting a native request with no pin, Construct reconstructs it
  to the observed origin and preserves end-to-end credentials while
  removing proxy and hop-by-hop headers.
- Routed requests remove the harness's native credential and apply only the
  selected route's credential.
- Generated catalogs use conservative capabilities unless the shared model
  registry explicitly supplies richer metadata.
- Codex publication pins the session-local catalog to the v1 multi-agent
  surface. The v2 surface may encrypt a child task for the native ChatGPT
  backend, which makes the task unreadable to a routed provider.
- A bounded featured subset may be prioritized for native delegation
  schemas, while the complete available set remains selectable in the
  picker.

## Non-Goals

- Reimplementing a harness's subagent scheduler or model picker.
- Editing global harness configuration.
- Inferring provider capabilities from model names.
- Automatically choosing a route by price, latency, or availability.

## Examples

A Codex parent remains on its native model while a native subagent selects
`kimi-k2.5 · kimi` from the same model catalog. The subagent request carries
the readable Construct id `construct-kimi/kimi-k2.5`, so only that request
is translated and routed to Kimi. The parent's concurrent native request
still goes to the origin Codex selected.
