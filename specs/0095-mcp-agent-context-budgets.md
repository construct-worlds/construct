# 0095-mcp-agent-context-budgets

Status: accepted
Date: 2026-07-14
Area: protocol
Scope: Model-facing MCP tools return bounded, compact projections instead of UI-oriented daemon payloads.

## Decision

Construct's MCP boundary is an agent-context API, not a transparent serialization of daemon IPC responses.

- Every text result uses compact JSON serialization.
- Potentially unbounded reads have conservative defaults, explicit caller-controlled limits, truncation metadata, and an intentional full/detail mode where full fidelity is useful.
- Session and subagent listings return the fields needed to identify, route, and monitor work; UI layout, persistence, and diagnostic fields remain available through daemon IPC but are omitted from the default MCP projection.
- Transcript reads default to a recent bounded tail and compact any single oversized event. Full event detail is opt-in and still bounded by the requested event count.
- PTY reads default to a bounded byte tail and expose offsets so callers can page backward deliberately.
- Program reads return current Markdown once, compact block addressing/status, and no revision bodies by default. Program write/execute responses acknowledge the mutation and return only identities the caller could not already know.
- Program verb/template listings expose selection metadata by default; internal prompts and template bodies are returned only when explicitly requested.
- Agent context omits long reference policies and the Markdown registry by default, and supports content etags so unchanged memory or Program-run payloads need not be repeated.

The daemon's rich IPC structs remain authoritative for interactive clients. MCP projections may intentionally have a different and breaking shape.

## Reason

UI clients need full scrollback, rich session metadata, revision history, cursor state, and complete Program projections. Passing those same payloads through an MCP tool can consume megabytes of model context, repeat the same document many times, and trigger premature compaction. Most agent decisions need a recent tail, stable identifiers, concise state, and an explicit way to request more.

## Consequences

- Truncation must be visible; a bounded response must never silently pretend to be complete.
- Pagination coordinates are part of bounded read responses.
- Defaults optimize the common agent decision, while explicit detail flags preserve debugging and recovery workflows.
- Destructive session actions remain separate tools so approval and safety policy can distinguish them; schema reduction must not hide risk behind a generic action tool.

## Non-Goals

- This does not reduce daemon-side retention or change TUI/web payloads.
- This does not summarize model content with another model; compact projections and truncation are deterministic.
