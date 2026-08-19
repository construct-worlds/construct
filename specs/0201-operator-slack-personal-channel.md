# 0201-operator-slack-personal-channel

Status: accepted
Date: 2026-08-18
Area: protocol
Scope: A second Slack operator channel kind that acts through the user's own Slack account via an MCP backend, with no Slack app or bot token.

## Decision

`slack-personal` is a distinct channel kind from the Socket Mode `slack` kind,
not a mode on it. It connects to Slack as the user's own account through an
MCP server that exposes Slack tools. The daemon embeds a classic MCP client
and drives the channel with plain logic; no agent, harness, or model turn is
involved in ingress or delivery.

The channel is defined against a tool contract, not a provider. The contract
is: sweep messages newer than a timestamp across the account's conversations,
read a thread, send a message into a thread, and (optionally) create a draft
and add a reaction. Any MCP server satisfying the contract is a valid
backend; the channel configuration selects the backend, and a per-backend
adapter maps contract operations to that server's tool names and schemas.
Known backends at time of writing: the claude.ai Slack MCP proxy
(authenticated by the user's claude.ai account) and self-hosted open-source
Slack MCP servers spawned over stdio (authenticated by a Slack user token or
browser session tokens). Requiring any specific vendor account is not
acceptable; at least one backend must work without one.

Ingress simulates an event subscription by polling: the channel keeps its own
high-water timestamp, sweeps for newer messages, deduplicates by message
timestamp, and forwards matches into the shared operator router using the
same route key as the `slack` kind — workspace ID, channel ID, thread
timestamp. Routing, thread context, behavior options, allowlists, and the
explicit reply binding are shared with the `slack` kind; the two kinds are
two transports over one Slack-conversation model. "Unread" means newer than
the channel's own cursor; Slack's human read-markers are never consulted.

Polling is adaptive within explicit bounds. The configured poll interval is
the idle ceiling, and five seconds is the active floor. Startup and backend
reconnection begin at the idle ceiling. A sweep that accepts at least one
message resets the next interval to the active floor; each sweep with no
accepted messages doubles the following interval until the idle ceiling is
reached. Activity outside the configured scope does not reset the cadence.
With the default 20-second ceiling, activity produces the sequence 5, 10, 20
seconds across subsequent idle sweeps; further accepted activity resets it to
5 seconds again.

Because the operator and the user share one identity, the channel records the
timestamp of every message it posts and excludes those from ingress. Messages
authored by the user themself are legitimate triggers and are never filtered
by author.

Backend credentials follow the same write-only rule as `slack` tokens:
clients may learn that a credential exists, never its value. Slack callers
cannot submit tool approval decisions; approvals remain in Construct's
minibuffer surfaces.

## Reason

The Socket Mode channel requires creating and installing a Slack app and
minting two tokens — a real onboarding cost, and impossible where the user
lacks workspace permission to install apps. Acting through the user's own
account removes that setup entirely for personal-assistant use.

A separate kind rather than a mode: credential lifecycle (OAuth or spawned
server vs pasted tokens), runtime shape (poll scheduler vs WebSocket), the
trigger model (no bot exists to mention), and the speaking identity all
differ. Switching them together mid-life would silently change who the
operator is in the workspace; that is a new-channel decision.

A tool contract rather than a provider: Construct is multi-harness and must
not require an Anthropic account for a Slack feature. The contract also makes
the embedded MCP client a reusable primitive for future MCP-backed channels.

## Consequences

- Latency is the current adaptive interval plus backend indexing lag: up to
  the configured ceiling while idle and the five-second floor after accepted
  activity. The kind must not promise Socket Mode's push latency, and
  turn-running indication (0178) applies from the moment ingress forwards a
  message.
- Sweeps that read search indexes may return conversations of every type,
  including direct messages; the configured allowlists gate what ingress may
  forward, and an unconfigured scope is not forwarded.
- Adding a backend means adding an adapter mapping, not a new channel kind.
- Backends built on browser session credentials are outside Slack's terms
  and break without notice; they may be offered only with that stated
  plainly, and never as the default.
- Replies posted by this channel appear as the user. The autonomy decision
  (0202) governs when posting is allowed at all.

## Non-Goals

- Replacing the Socket Mode `slack` kind, which remains the right choice for
  team-facing operators needing push latency, a mentionable bot identity,
  and daemon-held credentials.
- Streaming partial responses, file exchange, or interactive components.
- A general MCP-client surface for sessions or harnesses; the embedded
  client exists to serve channels.

## Examples

A teammate DMs the user. The next sweep returns that message; its workspace,
channel, and thread timestamp form the route key, the operator router starts
a conversation, and the configured response mode (0202) decides whether the
reply is posted, drafted, or only observed.

The operator posts "Done" into a thread and records the returned timestamp.
The following sweep returns that same message; the recorded timestamp
excludes it, and no ingress event is produced.
