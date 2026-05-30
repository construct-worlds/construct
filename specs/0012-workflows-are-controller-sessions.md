# 0012-workflows-are-controller-sessions

Status: accepted
Date: 2026-05-30
Area: architecture
Scope: Applies to dynamic workflow planning, worker delegation, workflow progress, and approval aggregation.

## Decision

agentd workflows should be modeled as a controller session or subagent that dynamically plans work and creates visible worker sessions. The controller owns the planning loop, worker coordination, progress widget, approval queue, and final synthesis; worker sessions perform focused tasks and report compact results.

Pause and resume are controller orchestration states. Pausing a workflow stops the controller from launching workers, enqueueing follow-up work, or advancing the plan. Resuming a workflow asks the controller to inspect current worker state, summarize anything that changed while paused, and create a new plan revision before continuing.

## Reason

Dynamic workflows need an active planner that can inspect results, branch, retry, stop early, ask for approval, or create new workers as evidence changes. A static phase list is not enough.

Using a controller session preserves agentd's core session model. Workflows reuse existing subagents, widgets, transcripts, tools, and approvals instead of introducing a separate invisible execution system. The parent session stays user-facing while the controller absorbs orchestration detail.

## Consequences

Workflow workers should remain visible and inspectable. A user should be able to see, interrupt, or read the worker sessions that contributed to a workflow result.

Workflow progress should be published through session widgets so TUI and web clients can show phase status, worker state, findings, and pending approvals without special workflow UI.

Workflow widgets should expose pause/resume actions when useful. These actions are user intent delivered to the controller; they do not require every worker harness to implement native suspension. Existing workers may finish their current turn unless the user explicitly interrupts them.

Approvals should be summarized by the controller but remain tied to the underlying worker intent and normal safety rules. A controller may group low-risk requests for presentation, but it must not bypass tool approvals or hide high-risk actions.

The first implementation can be convention-based: controller prompts, worker title conventions, and widgets. Native workflow metadata and daemon-level controls can be added later after the behavior proves useful.

## Non-Goals

This does not require a daemon-owned script runtime, hidden worker pool, or separate workflow object before the controller pattern has been validated.

## Examples

A broad web UI overhaul can create a workflow controller that spawns a scroll reviewer, mobile UX reviewer, implementation worker, and adversarial verifier. The controller reads their outputs, updates a workflow timeline widget, asks for approval before risky actions, and reports the final synthesis to the parent session.
