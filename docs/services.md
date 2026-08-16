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

### Creating the Slack app

A channel needs an app installed in your workspace, which gives you the two
tokens the channel is configured with: an **app-level token** (`xapp-…`) that
opens the Socket Mode connection, and a **bot token** (`xoxb-…`) that posts as
the bot. They are different tokens with different lifetimes — the app-level one
is not an install artifact and does not change when you reinstall.

The fastest path is a manifest, which sets the scopes and event subscriptions
in one step. At [api.slack.com/apps](https://api.slack.com/apps) choose **Create
New App → From an app manifest**, pick the workspace, and paste:

```yaml
display_information:
  name: Construct
features:
  bot_user:
    display_name: Construct
    always_online: true
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - im:history
      - channels:history
      - groups:history
      - mpim:history
      - reactions:write
settings:
  event_subscriptions:
    bot_events:
      - app_mention
      - message.im
      - message.channels
      - message.groups
      - message.mpim
  socket_mode_enabled: true
  interactivity:
    is_enabled: false
  token_rotation_enabled: false
```

Keep `token_rotation_enabled: false`: Construct holds a static bot token and has
nowhere to put a refreshed one. With the app created:

1. **Basic Information → App-Level Tokens → Generate Token and Scopes.** Add the
   `connections:write` scope and generate. The `xapp-…` value it shows is
   `app_token`, and it is shown once — copy it now.
2. **OAuth & Permissions → Install to Workspace.** Approve the scopes. The
   **Bot User OAuth Token** (`xoxb-…`) on that page is `bot_token`; you can come
   back for it later.
3. **Invite the bot to the channels it should work in** (`/invite @Construct`).
   Slack sends no events for a channel the app is not a member of. DMs need no
   invite.

Adding a scope later takes effect only after reinstalling the app to the
workspace — Slack keeps issuing the token you already have with the scopes it
was granted. Construct logs a refusal rather than failing the turn when a scope
is missing, so a silent capability gap usually means a pending reinstall.

Both tokens are credentials for your workspace. Configure them where the rest of
the channel is configured (the service view's channel editor, or the config file
below), not in a shared repository.

### What each permission buys

Nothing here is required except the first three; every other scope enables a
behavior described further down, and an app without it simply cannot do that
thing.

| bot scope | needed for |
| --- | --- |
| `connections:write` *(app-level token)* | Opening the Socket Mode connection at all. Without it the channel never connects. |
| `app_mentions:read` | Receiving `@bot` mentions, with the `app_mention` event. |
| `chat:write` | Posting answers, and the `placeholder` progress message. |
| `im:history` | DMs, with the `message.im` event. |
| `channels:history` | Untagged follow-ups (`follow_up`) and `thread_context` in public channels, with `message.channels`. |
| `groups:history` | The same in private channels, with `message.groups`. |
| `mpim:history` | The same in group DMs, with `message.mpim`. Group DMs behave like channels, not like DMs: the bot must be mentioned before it engages. |
| `reactions:write` | `progress = "reaction"` or `"both"`. |

A scope and its event travel together — the scope grants access, the event
subscription is what makes Slack deliver anything. Granting `channels:history`
without subscribing to `message.channels` still leaves `follow_up` behaving like
`off`.

### Configuring the channel

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
tool — that turn will not resume until a user acts in the TUI, and the
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
