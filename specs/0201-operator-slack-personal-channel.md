# 0201-operator-slack-personal-channel

Status: accepted
Date: 2026-08-18
Area: protocol
Scope: A second Slack operator channel kind that acts through the user's own Slack account via Slack's hosted MCP service, without user-supplied commands or tokens.

## Decision

`slack-personal` is a distinct channel kind from the Socket Mode `slack` kind,
not a mode on it. It connects to Slack as the user's own account through
Slack's hosted MCP endpoint. The daemon owns the fixed endpoint, pinned local
proxy invocation, OAuth callback, and credential-storage location. Channel
configuration never contains an MCP command or Slack token. Saving and
enabling the channel starts browser OAuth on first use.

The daemon embeds a classic MCP client and drives the channel with plain
logic; no agent, harness, or model turn is involved in ingress or delivery.
Its adapter maps the hosted server's native contract exactly:

- `slack_search_public_and_private` sweeps messages and detects the account
  owner's replies;
- `slack_read_thread` reads context using `channel_id` and `message_ts`;
- `slack_send_message` posts automatic replies; and
- `slack_send_message_draft` creates the safe-default draft.

Read results may be structured content or JSON encoded in a text content
block. The hosted service's detailed search and thread schemas place their
rendered messages in string fields, so those formats are explicitly adapted
rather than treated as synthetic message objects. Send results may identify
the new message with a Slack permalink instead of a timestamp; the adapter
derives the timestamp from that link for echo suppression.

Ingress simulates an event subscription by polling: the channel keeps its own
high-water timestamp, sweeps for newer messages, deduplicates by message
timestamp, and forwards matches into the shared operator router using the
same route shape as the `slack` kind — workspace identity, channel ID, thread
timestamp. Hosted detailed results expose the workspace as the hostname in a
Slack permalink, so a personal-channel workspace allowlist uses hosts such as
`acme.slack.com`, not Socket Mode team IDs. Routing, thread context, behavior
options, channel allowlists, and the explicit reply binding are otherwise
shared with the `slack` kind. "Unread" means newer than the channel's own
cursor; Slack's human read-markers are never consulted.

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
timestamp of every message it posts and excludes those from ingress. It also
cross-references normal search with `from:me`. The current detailed hosted
result cannot safely distinguish the user's self-DM from their own side of an
ordinary DM, so own messages are conservatively excluded unless a future
structured result explicitly identifies the self-DM.

OAuth credentials follow the same write-only rule as `slack` tokens: clients
may learn that a non-empty persisted OAuth token record exists, never its
value. That summary is a storage fact, not a claim that refresh, connectivity,
or workspace approval currently succeeds. Slack callers cannot submit tool
approval decisions; approvals remain in Construct's minibuffer surfaces.

## Reason

The Socket Mode channel requires creating and installing a Slack app and
minting two tokens — a real onboarding cost. Acting through the user's own
account removes command and token entry for personal-assistant use, while
leaving Slack's OAuth and workspace-approval policies visible.

A separate kind rather than a mode: credential lifecycle (browser OAuth vs
pasted tokens), runtime shape (poll scheduler vs WebSocket), the
trigger model (no bot exists to mention), and the speaking identity all
differ. Switching them together mid-life would silently change who the
operator is in the workspace; that is a new-channel decision.

Using the hosted native contract avoids asking users to select, install, and
maintain an executable backend whose tool names and result schemas may not
match the channel's assumptions.

## Consequences

- Latency is the current adaptive interval plus backend indexing lag: up to
  the configured ceiling while idle and the five-second floor after accepted
  activity. The kind must not promise Socket Mode's push latency, and
  turn-running indication (0178) applies from the moment ingress forwards a
  message.
- Sweeps that read search indexes may return conversations of every type,
  including direct messages; the configured allowlists gate what ingress may
  forward, and an unconfigured scope is not forwarded.
- First use requires Node/npm to run the pinned MCP proxy and network access
  to Slack. Browser OAuth can still require workspace-admin or app approval;
  the absence of token fields does not bypass Slack policy.
- First-run status and logs must tell the user that browser authorization is
  starting. Credential summaries must say only whether credentials were
  persisted, never that the hosted service is connected or ready.
- A different MCP provider or tool schema requires an explicit adapter and a
  new design decision; arbitrary executable configuration is not restored.
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
