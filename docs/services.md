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

## Slack channels

A Slack channel connects over Socket Mode and needs no inbound port. It routes
each thread to its own session, so a conversation in Slack is a conversation in
Construct.

```toml
[channels.my-bot]
kind = "slack"
enabled = true
app_token = "xapp-…"       # Socket Mode
bot_token = "xoxb-…"       # posting
progress = "placeholder"   # off | placeholder | reaction | both
```

### Showing that a turn is still running

A turn that takes a while would otherwise leave the thread silent, which is
indistinguishable from a delivery that was dropped. `progress` chooses what the
channel shows while it works. Nothing appears for a turn that answers promptly
— the affordance is only for a wait long enough to look like a failure.

| value | behavior |
| --- | --- |
| `placeholder` (default) | Posts a message in the thread that later becomes the answer itself. |
| `reaction` | Reacts 👀 to the message that triggered the turn, then ✅ when it answers (⚠️ if it fails). |
| `both` | Both of the above. |
| `off` | Says nothing until the answer is ready. |

If the turn stops at a tool approval, the affordance says so and names the
tool — that turn will not resume until an operator acts in the TUI, and the
person waiting in Slack cannot see that prompt. A turn that ends without an
answer now reports that in the thread instead of only in the daemon log.

`reaction` and `both` call `reactions.add`, which needs the **`reactions:write`**
scope. If the app was installed without it, Construct logs the refusal and still
delivers the answer — reinstall the app to your workspace to enable it.

## Publishing a channel

Select an attached ingress channel. Its TUI action bar shows **Publish**; press
`p` or click the button. The web client also exposes **publish**. The equivalent
command is:

```text
/service publish <service> [channel]
```

Construct opens the first-party owner-authorization flow and reports the
public endpoint only after its reverse route is ready. The public HTTPS URL
includes the same service request path as the loopback endpoint. Callers keep
using the channel's bearer credential; tunnel authorization is for the owner
publishing the route and never replaces channel authentication.

While publication is active, the TUI action changes to **Withdraw**. Press `p`,
click it, use **unpublish** in the web client, or run:

```text
/service unpublish <service> [channel]
```

to withdraw only that channel's public route. Remote control is independent.
Pausing, disabling, detaching, or rebinding a channel also withdraws it. A
later resume or reattachment stays loopback-only until explicitly published
again. Publication is runtime-only and is not restored after daemon restart.

The UI distinguishes `authorizing`, `connecting`, `ready`, and `error` states.
When an authorization or public URL is available, its action bar also shows
**Open** (`o`) and **Copy** (`y`). Typed host-and-port endpoints show **Copy**
without **Open**, so future non-HTTP channels are not presented as browser
URLs.

The first-party `tunnel.zarvis.ai` provider currently publishes HTTP over
loopback TCP. It rejects other combinations locally before opening owner
authorization.
For development against another compatible publication control plane, set
`CONSTRUCT_CHANNEL_PUBLICATION_API_URL`; its default is
`https://tunnel.zarvis.ai/api/v1/channel-publications`.

## Protocol boundary

Publication is not specific to HTTP. A channel adapter supplies a typed local
ingress endpoint: transport, loopback address, and the small amount of protocol
metadata needed to form a public edge. The tunnel transports bytes and owns
public routing; the channel continues to parse requests, authenticate callers,
and route sessions.

Providers that support HTTP and WebSocket channels return public URLs. A future
provider for opaque TCP can return a public host and port through the same
boundary. UDP needs a provider that explicitly supports it, while outbound
broker or polling channels need no inbound tunnel at all.
