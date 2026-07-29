# 0113-model-routing-is-proxy-transported

Status: accepted
Date: 2026-07-25
Area: architecture
Scope: How Construct interposes on a coding harness's model API traffic so a session's model endpoint can be changed while the session is running.

## Decision

Construct may route a harness session's model API traffic through a
Construct-owned local proxy. The interposition mechanism is the harness
process's **proxy environment** (`HTTPS_PROXY`), never the harness's
base-URL/model configuration.

Three behaviors follow from one listener:

- **Pass-through (the default, and the state of every routed-capable
  session until a route is armed or native catalog publication is
  enabled).** The proxy accepts the client's
  `CONNECT`, dials the destination the client named, and splices the two
  sockets. It never terminates TLS, never parses a request or response
  body, and never inspects or substitutes a credential.
- **Routed (only after the user explicitly arms a route).** For the one
  destination host being routed, the proxy terminates TLS with a leaf
  certificate minted by a Construct-local CA, rewrites the request to the
  target endpoint, and streams the response back.
- **Native catalog selection (automatic for supported harnesses).** For the
  harness's fixed model host only, the proxy terminates TLS to read the
  request's model id. A Construct-published id selects its encoded route for
  that request. A native id with no armed route is reconstructed to the
  exact origin named by `CONNECT`, retaining the harness's native provider
  credential.

The origin — where the request would have gone without Construct — is
**always taken from the client's `CONNECT` line**, never derived from the
harness's configuration.

## Reason

A harness resolves its model endpoint from many channels: environment
variables, its own config files, per-project overrides, keychain/login
state, and defaults compiled into the binary. Any design that *displaces*
that resolution makes Construct responsible for reproducing a decision it
cannot fully observe. The failure mode is severe and silent: a user's
private gateway traffic, bearing their credential, sent to a vendor
default.

Proxy transport removes the class entirely. Construct does not
participate in endpoint resolution, so there is nothing to reproduce and
nothing to carry over. The client states its destination on every
connection.

It also makes the guarantee that matters structural rather than tested:
pass-through is a byte splice, so it is incapable of altering a request,
reordering a stream, changing SSE flush boundaries, re-encoding a body, or
mishandling an authorization header. Correctness there is a property of
the shape, not of test coverage.

## Consequences

- Pass-through must never parse. Inspection is available only for an armed
  route or a session whose native catalog was enabled. Catalog
  inspection is limited to the fixed model host and the model selector
  needed to choose between a published route and the native origin.
- Routing to a different endpoint — whether a same-dialect redirect or a
  translation — requires TLS interception, and therefore
  requires that the harness trust the Construct CA through a
  per-process channel (see
  [0115](0115-routing-injection-is-probe-verified.md)). A harness with no
  such channel is pass-through only; that is a supported, permanent state,
  not a degraded one.
- Certificate-trust channels come in two kinds and the difference is
  load-bearing: one *adds* a certificate authority to the platform's, the
  other *replaces* the platform's entirely. A replacing channel handed only
  the Construct CA leaves the session unable to reach anything except the
  routed endpoint. Such a channel must be given the platform trust store
  composed with the Construct CA, and a harness that needs that composition
  must be refused routing outright when the platform store cannot be read.
  Which kind a given variable is differs per harness and is established by
  probe, not by its name.
- Interception is per-destination, not per-connection. Every destination
  other than the routed model host stays a blind tunnel, including the
  harness's own auth-refresh and telemetry endpoints, and everything the
  harness spawns (MCP servers, subprocess network calls) that inherits the
  proxy environment.
- A pre-existing `HTTPS_PROXY` in the environment is a single standardized
  value: it is captured and chained to, so a user behind a corporate proxy
  keeps reaching it.
- Because the transport is cooperative, a harness may ignore it. That is
  benign by construction — the session behaves exactly as an unrouted one.
  It must be reported, never silently treated as routed.
- Privileged or globally-scoped interception (packet filter redirects,
  TUN devices, DNS overrides, system trust-store changes) is rejected: its
  failure escapes the session and can outlive the process that created it.
  Every accepted mechanism confines its worst case to a session the user
  deliberately launched through Construct.

## Non-Goals

- Routing traffic for processes Construct did not spawn.
- Intercepting anything on the blind pass-through path, for any purpose,
  including observability.
- The translation itself. What a route *is* — including when a target
  speaks a different dialect than the harness — is
  [0116](0116-cross-dialect-routing-is-a-translation.md).

## Examples

A session's harness is configured, by a file Construct never reads, to
reach a private gateway. Construct routes it:

- Unarmed: `CONNECT gateway.corp.internal:443` arrives; Construct dials
  that host and splices. The conversation is byte-identical to one with no
  Construct involvement, and the gateway credential never leaves the
  tunnel.
- Armed to a different endpoint: the same `CONNECT` arrives, Construct
  answers it with a minted certificate, and forwards the rewritten request
  to the endpoint the user chose. The origin recorded for the session is
  `gateway.corp.internal:443` — observed, not guessed.
