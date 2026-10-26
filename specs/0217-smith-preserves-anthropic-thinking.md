# 0217-smith-preserves-anthropic-thinking

Status: accepted
Date: 2026-10-25
Area: harness
Scope: Smith replays signed Anthropic thinking with its original assistant turn.

## Decision

When an Anthropic Messages response includes thinking or redacted thinking,
Smith persists the complete assistant content blocks in their original order,
including signatures and tool calls. On later Anthropic requests it replays
those blocks as the assistant turn, without rebuilding or duplicating that
turn. Headless and interactive Smith sessions follow the same rule.

Smith also keeps its normalized assistant text and tool calls so a different
provider can read the conversation. Providers other than Anthropic ignore the
Anthropic-specific blocks and use the normalized turn.

## Reason

Current Claude models can think on every request. Anthropic requires signed
thinking blocks to be returned complete and unchanged when tool results are
sent back. Rebuilding the assistant turn from text and tool calls drops those
blocks and can make the next request fail.

## Consequences

- Stream handling must retain block order, empty thinking text, signatures,
  redacted blocks, and tool-use inputs.
- The replayable blocks survive session resume alongside normal Smith history.
- A provider switch may use the normalized turn but must not send Anthropic
  signatures to a different provider.

## Non-Goals

Thinking blocks are provider state, not a portable reasoning transcript or a
replacement for Smith's user-visible reasoning stream.
