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
follow_up = "thread"       # off | thread | channel
thread_context = 50        # earlier thread messages to read on joining; 0 = none
```

The last three are also editable where the channel is. Select a Slack channel
in the service view and press `e`: the editor lists **Progress**, **Follow-up**,
and **Thread context** below the allowlists, with what each one needs from your
Slack app in the help column. Space or `→` steps an option forward and `←` back;
thread context is typed. The web client offers the same fields. Saving any of
them reconnects that channel's Socket Mode connection, the same as changing an
allowlist does.

### Answering without being mentioned

A bot you must `@`-mention for every message cannot hold a conversation. DMs
have always worked untagged; `follow_up` extends that to channels.

| value | behavior |
| --- | --- |
| `thread` (default) | After being mentioned in a thread, answers later messages in that thread. |
| `channel` | After being mentioned anywhere in a channel, answers everything posted there. |
| `off` | Only direct mentions and DMs. |

"Already engaged" means Construct already routes a session for that thread —
there is no separate participation state to get out of sync. Each thread stays
its own session in every mode, so unrelated topics never share context.

This needs the **`message.channels`** event subscription (plus
**`message.groups`** for private channels). Without it Slack never sends
untagged messages and every mode behaves like `off`. Note that subscribing to
both `app_mention` and `message.channels` makes Slack deliver a message that
mentions the bot twice; Construct deduplicates on the message itself, so this
is safe.

### Reading the thread it was pulled into

`thread_context` is how many earlier messages of a thread the bot reads when it
is first mentioned in one, so "@bot what do you think?" can be answered from
the conversation rather than from those five words. It reads only on joining —
after that the session has been present for the thread itself. Set `0` to
disable.

Needs **`channels:history`** (`groups:history` for private channels). Without
the scope Construct logs the refusal and answers from the message alone.

> **Trust boundary.** Thread history is written by other people and the session
> has tools. Construct fences fetched history in a block marked as material to
> read, never instructions to follow. That is a mitigation, not a guarantee —
> if a channel's participants are not people you would let instruct the agent
> directly, keep `thread_context = 0`.

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
