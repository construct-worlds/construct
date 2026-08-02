# 0180-a-restart-does-not-invent-work

Status: accepted
Date: 2026-08-02
Area: architecture
Scope: What lifecycle state a session comes back with when a daemon restart reattaches to an adapter that outlived the daemon.

## Decision

A daemon restart may not report a session as working unless it is.

When the daemon reattaches to an adapter process that survived the restart, the session keeps the lifecycle state it was persisted with. A session parked at its harness's prompt comes back idle. Only a session the daemon has to bring back — one whose adapter is gone and whose replacement child is booting — may be marked as running on the strength of the resume alone, and only because the resume genuinely started something.

## Reason

A surviving adapter did not change across the restart. It is blocked in the same place it was blocked before, and an adapter waiting for input has no reason to announce anything: its next status arrives when a turn ends, which is to say never, until someone starts one.

That makes an optimistic "running" unfalsifiable. The daemon's only correction for harnesses that do not self-report is the idle sweep over PTY silence, and silence is only measurable against output. A session whose child emits nothing until its next turn — every headless one — therefore stays wrongly running until a client happens to open it and the attach repaints its child. Until then the fleet paints a working indicator over an idle session, the operator cannot tell which of their sessions are actually busy, and the session banks compute time it never spent.

The cost of the honest default is bounded in the other direction: if a reattached session really was mid-turn, its state says so, because that is what was persisted.

## Consequences

- Resume must treat "the adapter is alive" and "the session is working" as different facts. Any future resume path that writes a state has to justify it from what the harness actually reported, not from the fact that a socket answered.
- A non-terminal placeholder on the boot path stays legitimate: a session that was never started, or that errored and is being retried, must stop looking dead the moment its adapter is back, and something really is starting.
- Compute accounting stays truthful across restarts, since busy spans are opened only by real turns. Restart no longer inflates a session's recorded compute time by its idle hours.
- Clients may keep treating a headless session's running state as "working right now" — that reading is only sound while the daemon refuses to assert it speculatively.

## Non-Goals

Does not change which sessions are resumed at startup, and does not add a status query to the harness protocol. The rule is about not overwriting known state, not about interrogating adapters for fresh state.
