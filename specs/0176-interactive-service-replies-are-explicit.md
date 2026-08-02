# 0176-interactive-service-replies-are-explicit

Status: accepted
Date: 2026-08-01
Area: protocol
Scope: How a native interactive service session returns a response to the channel delivery that invoked it.

## Decision

A service may create either a structured headless session or a native
interactive PTY session. Headless remains the default. Interactive service
sessions are initially supported for Codex and Claude.

An interactive session never derives a channel response by scraping terminal
output or guessing when the native UI has finished a turn. Every accepted
channel delivery receives an opaque, one-shot delivery id. Construct registers
that id against the service-owned session before submitting the prompt and
injects a session-local `construct_service_reply` tool. The tool accepts only
the delivery id and caller-facing text.

The daemon validates that the delivery id is pending for the Construct session
that owns the tool process. It then records the explicit reply in the session
transcript and returns it to the channel adapter that already owns the native
destination. The model cannot supply or override a workspace, channel,
recipient, thread, endpoint, or credential.

When the service does not grant general MCP access, the injected Construct MCP
server advertises only the reply tool and no plugin MCP servers. Explicitly
granting general MCP access may add the normal Construct and plugin tools, but
does not weaken delivery binding.

## Reason

Native terminal output is presentation state, not a reply protocol. It can
redraw, stream status, include tool output, and return to an input prompt
without a stable semantic answer boundary. Scraping it would make channel
delivery harness- and version-dependent.

A narrow capability preserves the native interactive experience while keeping
channel routing, credentials, allowlists, deduplication, and thread ownership
inside Construct. It also keeps the reply contract transport-neutral instead
of making service sessions depend on a Slack-specific agent integration.

## Consequences

- Changing session mode applies only to newly created service sessions.
- Existing routed sessions keep the mode and tool surface they started with.
- One delivery id can produce at most one channel response.
- A reply with an unknown, expired, already-used, or differently owned
  delivery id fails closed.
- If an interactive agent never calls the reply tool, the channel turn times
  out; terminal prose is never treated as an implicit response.
- The official Slack MCP integration may be granted for additional agent
  actions, but it is not the service reply transport and cannot replace the
  bound delivery capability.

## Non-Goals

- Parsing or scraping Codex, Claude, or other native terminal UIs.
- Letting a model choose arbitrary channel destinations for a service reply.
- Converting an already-running headless session into an interactive PTY, or
  the reverse.

## Examples

A Slack thread starts an interactive Codex service session. Construct includes
delivery id `abc123` in the submitted prompt. Codex calls
`construct_service_reply` with `{ "delivery_id": "abc123", "text": "Done" }`.
Construct validates the id against that Codex session and posts `Done` through
the configured Slack channel adapter to the original thread.
