# Controller workflows

agentd workflows are a convention for dynamic planning with the primitives that
already exist: sessions, subagents, widgets, memory, and the shared tool layer.

A workflow has one **controller** session or subagent. The controller plans,
spawns workers, reads results, updates progress widgets, aggregates approvals,
and reports the final answer. Workers stay focused on one task and remain
visible as normal agentd sessions.

```text
parent session
  workflow controller
    worker: implementation scout
    worker: mobile reviewer
    worker: verifier
```

This keeps the parent session user-facing while the controller owns the messy
orchestration loop.

## When to use a workflow

Use a controller workflow when the task is complex enough that one linear agent
turn is likely to lose track of state, evidence, or competing risks. Good
signals include:

- the plan cannot be known up front and should change after investigation
- the work has several independent parts that can run in parallel
- the task needs both implementation and independent review
- the result depends on comparing or reconciling multiple findings
- progress, approvals, or budget should stay visible while work continues
- the controller may need to stop early, retry a weak path, or ask for more
  scope based on intermediate results

Do not use a workflow for a simple localized edit, a single test failure, or a
question that one focused agent can answer directly.

## Controller loop

The controller repeats this loop until it can finish:

1. Read the goal, constraints, budget, and current state.
2. Update the workflow widget.
3. Decide the next actions based on available evidence.
4. Spawn or continue workers.
5. Read worker outputs.
6. Summarize facts, disagreements, risks, and approvals.
7. Ask the parent/user when blocked or crossing scope.
8. Finish with a compact final report.

The controller should plan dynamically. It can stop early when confidence is
high, spawn a verifier when workers disagree, retry weak work, or ask for more
budget when the current budget is not enough.

## Pause and resume

Pause and resume are controller responsibilities. A paused workflow stops
orchestrating: the controller must not spawn workers, enqueue follow-up work, or
advance the plan until the user resumes it. Existing worker sessions remain
normal agentd sessions; they may finish their current turn unless the user
explicitly asks to interrupt them.

Use these states in the workflow widget:

| State | Meaning |
|---|---|
| `running` | controller may plan, spawn workers, and synthesize results |
| `paused` | controller preserves state but does not advance orchestration |
| `resuming` | controller is reloading worker state and revising the plan |
| `completed` | controller produced the final answer |
| `cancelled` | user stopped the workflow intentionally |
| `failed` | controller cannot continue without user or system intervention |

When pausing, the controller should:

1. Update the workflow widget to `Status: paused`.
2. Record the current plan revision, active workers, pending approvals, and next
   decision point in the widget.
3. Stop launching or continuing workers.
4. Leave explicit actions for resume, cancel, and optional worker interruption.

When resuming, the controller should:

1. Update the workflow widget to `Status: resuming`.
2. Inspect current worker session state and outputs.
3. Summarize what changed while paused.
4. Create a new plan revision before continuing.
5. Update the widget back to `Status: running`.

Do not resume by replaying an old static plan. Resume is a fresh planning
iteration using the latest durable state.

## Controller prompt template

Use this prompt shape when creating a workflow controller:

```text
You are the workflow controller for: <goal>

Operate as a planner/controller, not as a worker. Use existing agentd tools to
create focused worker sessions/subagents, read their results, update workflow
widgets, and synthesize the final answer.

Rules:
- Keep the parent session user-facing; do not flood it with worker detail.
- Maintain a workflow status widget with phase, worker state, findings, and
  pending approvals.
- Treat pause/resume as controller state. If the user pauses the workflow, stop
  orchestration until an explicit resume action arrives. On resume, inspect
  worker state and create a new plan revision before continuing.
- Spawn visible workers with narrow roles and clear output formats.
- Workers should not spawn more workers unless explicitly asked.
- Read and summarize worker results before deciding the next step.
- If evidence conflicts, spawn a targeted verifier or ask the user.
- Before risky actions, aggregate the request in an approval widget and wait
  for user intent.
- Preserve normal tool approval and safety rules. Widget actions are intent,
  not permission bypasses.
- Respect the budget: max workers <N>, max concurrent workers <N>, max
  planning iterations <N>.

Finish only when you can provide:
- final answer or implementation recommendation
- worker evidence summary
- unresolved risks or follow-up tasks
```

## Worker prompt template

Workers should receive narrow tasks:

```text
You are a workflow worker for: <workflow goal>
Role: <role>
Controller: <controller title/session>

Do only this task:
<task>

Output:
- findings
- evidence
- recommendation
- confidence
- risks or follow-up checks

Do not spawn additional workers. Do not make broad unrelated edits. If the task
needs more scope or approval, stop and explain the request.
```

## Widget conventions

The controller should create a sticky workflow status widget:

```md
# Workflow: <short goal>

Status: running
Controller: <controller title>
Plan revision: 3

:::timeline
- [x] Scope
- [~] Investigate
  - [x] scroll reviewer
  - [~] mobile reviewer
  - [ ] verifier
- [ ] Synthesize
- [ ] Finish
:::

| Worker | Status | Result |
|---|---|---|
| scroll reviewer | done | no blocker |
| mobile reviewer | running | |
| verifier | pending | |

Next: wait for mobile reviewer, then choose fix plan.

[Pause](agentd:action/workflow-pause?key=p)
[Cancel](agentd:action/workflow-cancel)
```

When paused, keep the same widget visible and preserve the last known state:

```md
# Workflow: <short goal>

Status: paused
Controller: <controller title>
Plan revision: 3

Paused at: waiting for verifier output

:::timeline
- [x] Scope
- [~] Investigate
  - [x] scroll reviewer
  - [x] mobile reviewer
  - [~] verifier
- [ ] Synthesize
- [ ] Finish
:::

| Worker | Status | Result |
|---|---|---|
| scroll reviewer | done | no blocker |
| mobile reviewer | done | one mobile risk |
| verifier | running | allowed to finish current turn |

[Resume](agentd:action/workflow-resume?key=r)
[Interrupt workers](agentd:action/workflow-interrupt-workers)
[Cancel](agentd:action/workflow-cancel)
```

Use a separate approval widget when approvals are pending:

```md
# Workflow Approvals

:::timeline
- [!] 2 approvals pending
  - implementation worker: edit web UI file
  - verifier: run focused test
:::

| Worker | Action | Risk | Reason |
|---|---|---|---|
| implementation | edit file | medium | apply selected fix |
| verifier | run test | low | validate behavior |

[Review next](agentd:action/review-next)
[Approve low-risk](agentd:action/approve-low-risk)
[Deny all](agentd:action/deny-all)
```

Action links express user intent to the controller. The controller must still
honor normal permission and safety policy before instructing workers to act.

## Approval policy

Workers can request risky actions, but the controller presents them coherently.
Use these approval categories:

| Category | Meaning |
|---|---|
| `tool` | worker wants a tool or command |
| `edit` | worker wants to modify files |
| `spawn` | controller wants more workers |
| `budget` | controller wants more time, tokens, or concurrency |
| `scope` | controller wants to expand touched files or behavior |
| `finalize` | controller wants to commit, open PR, merge, or clean up |

High-risk or destructive actions should never be batch-approved. Denials should
be sent back to the requesting worker with enough context to choose a safer
path.

## Naming and visibility

Use predictable titles:

- controller: `workflow: <short goal>`
- worker: `workflow/<role>: <short task>`

Workers should be visible, inspectable sessions. Avoid hidden worker pools; if a
workflow result matters, the user should be able to inspect the sessions that
produced it.

## Current limitations

This convention does not yet provide daemon-native workflow metadata, a global
workflow list, or direct aggregation of pending tool approvals. Pause/resume is
implemented as controller behavior driven by widget actions and durable session
state, not as a daemon-level scheduler control. Native controls can be added
later once the controller pattern is proven through real tasks.
