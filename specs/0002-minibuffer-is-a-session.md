# 0002-minibuffer-is-a-session

Status: accepted
Date: 2026-05-30
Area: architecture
Scope: Applies to the fleet command surface, minibuffer UI, approvals, and orchestration behavior.

## Decision

The minibuffer is a real session, not a special command bar. It should use the same persistence, transcript, input, resume, rendering, and approval mechanics as other sessions, while clients may present it in a specialized location.

## Reason

The minibuffer needs to reason about and act on the fleet over time. Making it a session avoids a parallel stack for history, queued input, tool calls, approval rendering, restart behavior, and harness-specific behavior.

This keeps orchestration extensible. New session capabilities automatically become available to the minibuffer unless there is a deliberate reason to exclude them.

## Consequences

Future minibuffer features should be implemented as session features by default. Special global UI paths should be reserved for display placement or selection policy, not for distinct behavior.

The minibuffer may be hidden from ordinary session lists while still retaining all session semantics. Clients should distinguish between "not shown in the normal list" and "not a session."

The user-facing label for this role should be operational and contextually consistent with agentd's Matrix-inspired aesthetic. The accepted label is "minibuffer."

## Non-Goals

This does not require every client to render the minibuffer in the same screen position. It only requires the underlying behavior to remain session-based.

## Examples

An approval requested by the minibuffer should appear in the minibuffer's own interaction stream rather than taking over an unrelated global prompt.
