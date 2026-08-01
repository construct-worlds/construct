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

Construct integrates through a session-scoped native mechanism and never
edits the harness's persistent configuration. Codex receives a generated
catalog override. Claude receives a loopback Anthropic gateway URL whose
`/v1/models` response is consumed by Claude Code's native gateway discovery.
The Claude adapter enables loopback discovery only for that child process
and does not displace a user-configured `ANTHROPIC_BASE_URL`. Native catalog
entries remain present alongside Construct entries. Claude's gateway source
subtitle is harness-owned and may be generic, so each published display name
also identifies Construct explicitly. Construct primes Claude's native
gateway-model cache and uses a non-auth session capability for loopback
requests; it does not install an API key or auth token, so a claude.ai login
remains authoritative and its organization connectors remain available.

Service definitions that select a published route/model pair persist the
ordinary `construct-<route>/<model>` id as their harness-neutral routing
authority. When a service creates a session, Construct materializes that id
for the selected harness, including Claude's required `claude-construct-`
prefix. A client must never reduce a route-aware service selection to the
target model name alone.

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
- A service may change between supported native-catalog harnesses without
  losing its selected route; the durable id is adapted when each new session
  starts.
- Request selection overrides a manual session pin for that request only.
  The pin remains unchanged for native model ids and future requests.
- Catalog-enabled sessions inspect only the harness's fixed model host.
  Other destinations remain blind tunnels.
- Request compression is decoded before model-id inspection and translation,
  then forwarded with framing that matches the decoded body.
- Codex catalog sessions use a session-local OpenAI-compatible provider that
  keeps Codex's active native authentication and corresponding HTTPS origin
  while disabling Responses-over-WebSocket. Construct's proxy supports the
  HTTPS/SSE transport; it must not let a routed picker selection first escape
  to the harness's fixed WebSocket endpoint and fail before fallback.
- After inspecting a native request with no pin, Construct normally
  reconstructs it to the observed origin and preserves end-to-end credentials
  while removing proxy and hop-by-hop headers. Claude subscription sessions
  use the session-token exchange described below.
- Claude's gateway discovery requires an API-shaped credential. When Claude
  Code already has an API credential, Construct preserves it. For a
  subscription session, the adapter presents the session capability token
  to the loopback gateway and the router exchanges native Claude selections
  for the detected Claude OAuth route. The capability is valid only for its
  owning session.
- Claude's loopback gateway is excluded from that child's proxy settings so
  discovery reaches the listener directly instead of recursively proxying
  through the same listener.
- A user-configured Claude gateway remains authoritative; Construct does not
  add its own rows to that gateway's picker.
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

A Claude session opens `/model` and sees
`gpt-5.6-sol · codex-oauth · Construct` as a gateway entry next to Claude's
built-in rows. Selecting it carries
`claude-construct-codex-oauth/gpt-5.6-sol` on the request, allowing the same
Claude session or one of its native subagents to select the Codex route
without changing the parent's model.
