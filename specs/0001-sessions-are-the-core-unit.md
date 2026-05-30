# 0001-sessions-are-the-core-unit

Status: accepted
Date: 2026-05-30
Area: architecture
Scope: Applies to user-visible work, orchestration, persistence, and client rendering.

## Decision

agentd models active work as sessions. A session is the durable unit for identity, transcript, runtime status, UI state, cwd, harness ownership, grouping, and user interaction.

## Reason

Users operate on long-running agents, not isolated command invocations. Treating each agent run as a session lets clients reconnect, inspect history, resume work, and coordinate multiple agents without inventing separate abstractions for every surface.

This also gives all clients a shared mental model. The TUI, web UI, CLI, MCP tools, and harness adapters can describe the same object when they list, select, pin, rename, reorder, interrupt, or delete work.

## Consequences

Features that look like UI affordances should first be evaluated as session features. If the feature has transcript, status, input, persistence, or ownership, it probably belongs on a session rather than in a separate global UI channel.

Sessions need stable identity across reconnects and daemon restarts. Clients should preserve the user's selected session and avoid treating reconnect as a new run.

This makes sessions a broad abstraction. Future changes must be careful not to overload session state with unrelated global settings, and should keep cross-session concepts explicit.

## Non-Goals

This does not mean every small UI preference belongs in session state. Pure client-local layout choices can stay local when they do not need to survive across clients or daemon restarts.

## Examples

A terminal-backed worker, a chat-style agent, and the orchestrator command surface are all sessions, even though clients render them differently.
