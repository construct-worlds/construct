# 0149-fork-prompt-waits-for-harness-readiness

Status: accepted
Date: 2026-07-27
Area: harness
Scope: Machine-delivered input to a cold-started session waits for the harness's own readiness signal, never for an inferred one.

## Decision

When the daemon delivers input to a session whose harness it just cold-started
— a Playbook Run or verb fork above all — it must wait for the harness to
report that it is ready for input. The session state machine already carries
that signal; delivery consumes it rather than re-deriving readiness from the
shape of the harness's output.

A quiet-output fallback may exist so a harness that never reports readiness
still gets its input before the hard timeout, but it is a fallback, not the
primary signal, and its window must be at least as long as the daemon's own
definition of an idle interactive PTY. A session that has produced no output
at all since it was created is never ready, whatever its state field says.

This applies to input the daemon originates. A human typing into a session is
unaffected: they can see the harness and choose when to type.

## Reason

Cold-started harnesses do not draw continuously. They emit, pause while doing
non-drawing startup work, emit again, and only attach their input handler once
that work is done — often seconds after the last byte of the startup draw.

Any readiness rule based on output shape reads that mid-startup pause as "the
harness is finished". The failure it causes is silent and total: bytes written
into a PTY before the harness attaches its handler are discarded when the
harness puts the terminal into raw mode, the write itself succeeds, no error
is raised anywhere, and the session sits idle forever looking perfectly
healthy. Users see a fork that was created, holds the right context, and never
does the work — with nothing in any log to explain it.

Confirmed live: a fork's startup draw paused ~750ms partway through, a 500ms
quiet gate fired inside that pause, and three consecutive Runs dispatched
nothing. Input sent after the same fork reported ready landed every time.

## Consequences

- Delivery latency is bounded by how fast the harness reports readiness, not
  by how fast its output goes quiet. Machine-delivered input arrives later
  than it used to, and that is the point.
- Adapters own readiness. An adapter that never reports it hands its sessions
  the fallback path, which is slower and less precise — so reporting it
  accurately is part of what an adapter owes the daemon.
- Echo-based gates around a paste (proving the harness consumed it) stay
  useful but can never substitute for a readiness check: unrelated output
  satisfies them, so they pass silently in exactly the case that matters.
- Tests covering machine-delivered input must exercise a harness that pauses
  mid-startup. A stand-in that drains input immediately, like a plain shell,
  cannot reproduce the failure and will pass against a broken daemon.

## Non-Goals

Does not change what is delivered, where a fork gets its Playbook context, or
how a fork settles the blocks it was dispatched for. Does not require that
every harness report readiness — only that the daemon prefers that signal
whenever it exists.
