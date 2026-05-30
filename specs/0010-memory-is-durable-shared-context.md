# 0010-memory-is-durable-shared-context

Status: accepted
Date: 2026-05-30
Area: persistence
Scope: Applies to global memory, project memory, agent context, and multi-harness continuity.

## Decision

Memory is durable, human-editable Markdown context shared across agent sessions and harnesses. It is separate from transcripts and should store reusable facts, preferences, workflows, decisions, commands, glossary, and pitfalls.

## Reason

Transcripts record what happened, but they are too noisy and session-specific to serve as durable operating context. Agents need a small, auditable surface for information that should influence future work.

Markdown keeps memory easy for users and agents to inspect, edit, prune, and correct.

## Consequences

Agents should load available memory before starting substantial work and update it when they learn durable information that will likely help future sessions.

Memory should use the narrowest useful scope. Cross-project preferences belong globally; repository-specific workflows or architecture belong to project memory.

Memory maintenance should be conservative. Agents should avoid storing secrets, transient task status, large command output, speculation, or facts that will quickly go stale.

## Non-Goals

Memory is not a complete knowledge base, task tracker, transcript summary, or replacement for repository documentation.

## Examples

A recurring project merge workflow belongs in project memory. A one-time CI run URL does not.
