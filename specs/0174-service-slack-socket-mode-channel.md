# 0174-service-slack-socket-mode-channel

Status: accepted
Date: 2026-08-01
Area: protocol
Scope: Slack is an outbound service channel whose threads map to Construct service conversations.

## Decision

A Slack service channel connects through Slack Socket Mode using a user-supplied `xapp-` app token and posts replies through the Web API using a user-supplied `xoxb-` bot token. It opens no inbound listener.

The channel accepts app mentions and direct messages. Each top-level event starts or continues a route keyed by workspace ID, channel ID, and thread timestamp; replies in that Slack thread reuse the same route. Slack's event ID is the ingress request ID, so replayed deliveries are deduplicated by the shared service runtime.

Every Socket Mode envelope is acknowledged before session work begins. Bot messages, message subtypes, unsupported events, and events excluded by configured workspace or channel allowlists are ignored. A completed assistant turn is posted once to the originating Slack thread; partial output is not streamed.

Socket Mode disconnects reconnect with bounded exponential backoff. Pausing, disabling, detaching, or materially editing a Slack channel closes its current connection immediately; enabling or attaching starts one without a daemon restart.

Slack credentials are write-only configuration. API summaries, logs, errors, and UI state may report whether each credential exists, but must never return or print either token. Slack callers cannot submit tool approval decisions; approvals remain controlled by Construct's minibuffer surfaces and existing service timeout policy.

## Reason

Socket Mode gives locally running Construct daemons Slack ingress without a public webhook, while the shared service router preserves the same routing, least-privilege sessions, deduplication, and approval semantics as HTTP channels. Thread-based keys make Slack's visible conversation boundary match Construct's persisted conversation boundary.

## Consequences

Slack app configuration must enable Socket Mode, create an app-level token with `connections:write`, subscribe to `app_mention` and `message.im`, and grant the bot `app_mentions:read`, `im:history`, and `chat:write`. Minibuffers install the app and enter both tokens manually; OAuth installation is not part of Construct.

Channel tasks are not assumed to own TCP ports. Supervisors and clients must treat HTTP listeners and outbound Slack connections as peer channel lifecycles. Runtime state belongs to the service ingress layer and survives channel reconnects and configuration reloads.

## Non-Goals

This decision does not define OAuth installation, file exchange, interactive components, slash commands, or partial-response streaming.

## Examples

An app mention in workspace `T1`, channel `C1`, at timestamp `100.25` uses route key `T1:C1:100.25`. A reply event whose `thread_ts` is `100.25` uses the same key. The final assistant response is posted with `channel=C1` and `thread_ts=100.25`.
