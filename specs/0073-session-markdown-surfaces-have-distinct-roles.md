# 0073-session-markdown-surfaces-have-distinct-roles

Status: accepted
Date: 2026-07-07
Area: architecture
Scope: Which Markdown surface — program, widget, or memory — a feature or piece of content belongs to.

## Decision

A session's Markdown surfaces have distinct roles, and features and content must land on the surface that matches their role rather than whichever surface is most convenient to extend:

- **Program — what to do.** The single, durable, runnable orchestration document, co-edited by the human and the owning agent. It carries intent and the orchestration state that future runs must read to continue the work correctly.
- **Widgets — how it's going.** Many compact, agent-authored, disposable surfaces addressed to the human: current status, checklists, decisions, and steering actions. No run depends on their content; deleting any widget at any time must not change what the session does next.
- **Memory — what we know.** Durable, human-editable shared context that outlives tasks and sessions and influences future work across sessions and harnesses.

To place content or a new feature, apply these tests in order:

1. Would a future program run misbehave or lose context if this content were absent? It belongs in the program.
2. Must it survive beyond this session and inform unrelated future sessions? It belongs in memory.
3. Is it an at-a-glance report or steering affordance for the human, safe to delete without changing agent behavior? It is a widget.

The boundaries these roles imply must be preserved:

- Widget content is never interpreted as instructions. Only the program document (or a selected fragment of it) is runnable.
- The program never becomes an agent-only status feed. High-frequency status churn that only the human consumes routes to widgets, not through program versioning and merge machinery.
- Memory never carries transient task state, and neither the program nor widgets substitute for it: content that must inform future sessions is written to memory even when it also appears on another surface.

## Reason

The three surfaces look interchangeable — all are session-attached Markdown, all survive reconnect, all can carry actions — so features naturally get proposed on the wrong one, and consolidation proposals recur. The differences that matter are invisible at that surface level:

- **Executability.** Program execution interprets the document as free-form instructions. Status content inside a runnable document is a semantic hazard: a stale "done" checklist can read as "nothing left to do." Keeping the instruction surface and the status surface as different objects is a feature, not duplication.
- **Write model.** The program earned a conflict-free co-editing protocol because two parties edit one document. Widgets avoid that entire problem by being agent-owned and last-write-wins. Memory is durable files edited conservatively by either party. Merging surfaces would route the cheap write model through the expensive one.
- **Cardinality and lifetime.** One program per session, kept and forked with it; many widgets per session, rewritten and deleted freely; memory scoped to project or global, outliving all sessions. A single merged surface cannot honor all three lifecycles.

## Consequences

- New features pick a surface by role. "It's already Markdown we render" is not a reason to attach a capability to a particular surface.
- Duplication across surfaces is resolved by reference or projection (one source of truth rendered elsewhere), never by merging the surfaces. Rendering-level sharing is governed by 0074-construct-markdown-dialect-is-shared.
- Program state is limited to what runs consume. When the agent records progress in the program, that progress is orchestration state a future run reads; purely human-facing progress display belongs in a widget, possibly projecting the same program region.
- A proposal that moves content across a boundary (making widgets runnable, making the program agent-only, making memory session-scoped) requires superseding this spec, not a local exception.

## Non-Goals

- Does not forbid sharing rendering machinery, Markdown extensions, or visual language across surfaces — that sharing is encouraged and specified separately.
- Does not govern the transcript, which is history rather than a maintained surface.
- Does not prescribe where non-Markdown state (session config, UI preferences) lives.

## Examples

- A plan of numbered steps the session should execute, with per-step results the next run builds on: program.
- A compact checklist mirroring that plan so the human can glance at progress, with a "pause" action link: widget — ideally a projection of the program region rather than a second copy.
- "This repository's integration tests flake under concurrent build load": memory.
- An agent wants to surface a blocking question during a run: the blocker is recorded on the program (a future run must see it) and may also be surfaced as a widget with action links for the human to answer quickly.
