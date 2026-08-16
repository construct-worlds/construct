# 0174-channel-publication-is-transport-not-protocol

Status: accepted
Date: 2026-08-01
Area: architecture
Scope: The boundary between channel implementations and public tunnel providers.

## Decision

Public exposure is a capability layered over a channel-owned local ingress
endpoint. It is not an HTTP-channel feature and it is not part of remote
control.

A channel adapter may expose a typed local ingress endpoint consisting of a
network transport, local address, and application-protocol profile. Profiles
carry only edge-routing metadata, such as the canonical request path of an
HTTP listener; channel credentials and payload schemas remain private. A separate
publication supervisor owns explicit publish and withdraw intent, provider
authentication, reverse-transport lifetime, readiness, public-address state,
and retry. Provider backends consume only that endpoint description. They do
not parse channel configuration, credentials, or payloads.

Public addresses are typed. HTTP and WebSocket edges return URLs; raw stream
edges return host-and-port sockets. Clients call all of these public endpoints
rather than assuming every future channel has a URL.

Publishing is an explicit runtime action on one attached, enabled channel.
Enabling or attaching a channel never publishes it. Pausing its operator,
detaching it, disabling it, losing its local listener, or replacing its local
endpoint withdraws the route. A later resume or reattachment does not silently
republish it. Daemon restart also withdraws publication and requires another
explicit action, preserving the first-party tunnel's memory-only owner
authorization boundary.

The first-party provider registers channel publications separately from
remote-control tunnels. Its registration identifies the installation and
globally exclusive channel id, declares the local transport and protocol
profile, and requests channel-owned authentication. The gateway allocates a
scoped reverse endpoint and returns a typed public endpoint, relay capability,
readiness probe, and optional re-registration credential. HTTP publication
terminates public TLS but passes the caller's authorization header and body
through unchanged. The channel listener remains the final authority and
revalidates its own credential. A returned HTTP URL includes the channel's
canonical request path, so callers reach the same route locally and publicly.

The current first-party provider supports HTTP over loopback TCP. WebSocket and
opaque TCP protocols fit the boundary without changing publication lifecycle,
but a backend must advertise and implement each combination before it can be
selected. UDP likewise requires a provider and reverse transport that
explicitly support UDP. Outbound channels such as broker subscriptions expose
no local ingress and therefore need no publication.

## Reason

Tying tunnels to HTTP would force future protocols through HTTP-shaped
configuration and URL-shaped status. Tying them to remote control would mix
owner login and Basic authentication with machine-to-machine channel
credentials, and stopping remote control could unexpectedly take an automation
endpoint down.

The local endpoint is the narrow common boundary. A channel knows how to bind,
authenticate, and route its protocol; a provider knows how to make bytes reach
that listener. Keeping those responsibilities separate lets either side grow
without teaching it the other's implementation details.

## Consequences

- Remote control and channel publications have independent routes and
  lifetimes even when they use the same first-party account and relay operator.
- Channel ids identify publication reservations because they are globally
  exclusive transport resources; mutable operator names do not define public
  identity.
- The first-party gateway must implement the channel-publication registration
  contract and must not replace a channel's Authorization header with remote
  control's upstream Basic credentials.
- A provider advertises support before authorization. Unsupported transport or
  protocol combinations fail locally and allocate no public route.
- Publication state is pushed to connected clients. A URL is shown only after
  the gateway proves the reverse endpoint ready.
- A selected publishable channel exposes an explicit Publish or Withdraw
  action. Authorization and public URLs expose Open and Copy actions; typed
  socket endpoints expose Copy without pretending to be browser-openable.
- A channel-secret rotation does not require a new public address because
  authentication remains channel-owned and end to end.
- Adding a provider means implementing the publication backend boundary and
  registering it with the supervisor; adding a channel means supplying an
  ingress endpoint from its adapter. Neither requires branching on the other.

## Non-Goals

- Persisting owner credentials or automatically restoring publication after a
  daemon restart.
- Giving every channel public ingress. Outbound and local-only channels remain
  valid.
- Moving channel authentication into the tunnel gateway. Edge rejection may
  be added as defense in depth, but the local channel remains authoritative.

## Examples

- An HTTP channel publishes as an HTTPS URL. A caller sends its existing
  bearer token and JSON body; the loopback HTTP listener authenticates and
  parses both exactly as it does for a local caller.
- A future database-protocol channel exposes a loopback TCP socket and receives
  a public host and port. The UI offers Copy, but not Open, for that endpoint.
  The tunnel carries opaque bytes; the database channel performs its own
  handshake and authentication.
- A operator is paused while its channel is public. The publication route is
  withdrawn. Resuming restores only the loopback listener until the user
  explicitly publishes again.
