# 0002-orchestrator-is-a-session

Status: accepted
Date: 2026-05-30
Area: architecture
Scope: Applies to the fleet command surface, operator UI, approvals, and orchestration behavior.

## Decision

The orchestrator is a real session, not a special command bar. It should use the same persistence, transcript, input, resume, rendering, and approval mechanics as other sessions, while clients may present it in a specialized location.

## Reason

The orchestrator needs to reason about and act on the fleet over time. Making it a session avoids a parallel stack for history, queued input, tool calls, approval rendering, restart behavior, and harness-specific behavior.

This keeps orchestration extensible. New session capabilities automatically become available to the orchestrator unless there is a deliberate reason to exclude them.

## Consequences

Future orchestrator features should be implemented as session features by default. Special global UI paths should be reserved for display placement or selection policy, not for distinct behavior.

The orchestrator may be hidden from ordinary session lists while still retaining all session semantics. Clients should distinguish between "not shown in the normal list" and "not a session."

The user-facing label for this role should be operational and contextually consistent with agentd's Matrix-inspired aesthetic. The accepted label is "operator."

## Non-Goals

This does not require every client to render the orchestrator in the same screen position. It only requires the underlying behavior to remain session-based.

## Examples

An approval requested by the orchestrator should appear in the orchestrator's own interaction stream rather than taking over an unrelated global prompt.
