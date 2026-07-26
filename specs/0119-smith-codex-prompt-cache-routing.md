# 0119-smith-codex-prompt-cache-routing

Status: accepted
Date: 2026-07-25
Area: harness
Scope: Smith sessions using Codex OAuth preserve stable prompt-cache routing across turns and daemon restarts.

## Decision

Every adapter process receives its owning Construct session id as
`CONSTRUCT_SESSION_ID`, on both initial spawn and resume.

Smith's Codex OAuth provider sends that stable identity as the Responses API
prompt-cache key. Its default transport is one reused Responses WebSocket per
session. An operator may explicitly opt out of WebSocket transport by setting
`CONSTRUCT_SMITH_CODEX_WS=0`; connection or pre-stream failures fall back to
HTTP for the rest of that adapter process.

## Reason

Smith resends a growing, mostly unchanged conversation prefix for each model
step. Without a stable cache-routing key, otherwise cacheable requests can land
on different cache nodes and receive erratic cache hits. Stateless HTTP also
produces substantially worse cache locality than a reused Responses WebSocket.

The session id is daemon-owned, stable for the lifetime of a session, and
already identifies the same boundary used for transcript persistence and
resume, making it the appropriate cache-routing identity.

## Consequences

- Adapter launch code must treat `CONSTRUCT_SESSION_ID` as daemon metadata and
  overwrite conflicting caller-provided values.
- Resumed sessions retain the same prompt-cache key.
- Smith Codex OAuth requests use WebSocket unless explicitly opted out or the
  connection cannot be established before streaming begins.
- HTTP remains the compatibility fallback, but it carries the same stable
  prompt-cache key.

## Non-Goals

- This does not promise a particular provider cache-hit percentage or cache
  retention duration.
- This does not change Smith's conversation pruning or compaction policy.
