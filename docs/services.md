# Services and channels

A service turns authenticated external deliveries into ordinary headless
Construct sessions. Run `/serve <name>` in the TUI to create one, then attach
or create an HTTP channel in its service view.

Each HTTP channel owns a loopback port and a bearer credential. The credential
is shown only when the channel is created or explicitly rotated. Submit work
with:

```sh
curl -X POST http://127.0.0.1:8787/svc/alerts \
  -H 'Authorization: Bearer cst_…' \
  -H 'Content-Type: application/json' \
  -d '{"message":"Investigate this alert","session_key":"incident-42"}'
```

Accepted requests return `202` and a session id. Poll the scoped result route
with the same credential:

```sh
curl http://127.0.0.1:8787/svc/alerts/sessions/<session-id> \
  -H 'Authorization: Bearer cst_…'
```

HTTP channels start loopback-only. Enabling or attaching one never exposes it
to the network.

## Publishing a channel

Select an attached channel and press `p`, or use its **publish** button in the
web client. The equivalent command is:

```text
/service publish <service> [channel]
```

Construct opens the first-party owner-authorization flow and reports the
public endpoint only after its reverse route is ready. The public HTTPS URL
includes the same service request path as the loopback endpoint. Callers keep
using the channel's bearer credential; tunnel authorization is for the owner
publishing the route and never replaces channel authentication.

Press `p` again, use **unpublish**, or run:

```text
/service unpublish <service> [channel]
```

to withdraw only that channel's public route. Remote control is independent.
Pausing, disabling, detaching, or rebinding a channel also withdraws it. A
later resume or reattachment stays loopback-only until explicitly published
again. Publication is runtime-only and is not restored after daemon restart.

The UI distinguishes `authorizing`, `connecting`, `ready`, and `error` states.
For development against another compatible publication control plane, set
`CONSTRUCT_CHANNEL_PUBLICATION_API_URL`; its default is
`https://tunnel.zarvis.ai/api/v1/channel-publications`.

## Protocol boundary

Publication is not specific to HTTP. A channel adapter supplies a typed local
ingress endpoint: transport, loopback address, and the small amount of protocol
metadata needed to form a public edge. The tunnel transports bytes and owns
public routing; the channel continues to parse requests, authenticate callers,
and route sessions.

HTTP and WebSocket channels receive public URLs. A future opaque TCP channel
can receive a public host and port through the same boundary. UDP needs a
provider that explicitly supports it, while outbound broker or polling
channels need no inbound tunnel at all.
