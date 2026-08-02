# 0176-playbook-run-tracks-its-execution-session

Status: accepted
Date: 2026-08-02
Area: architecture
Scope: Which session lifecycle signals a Playbook run's shimmer may start and stop on.

## Decision

A Playbook run is one turn in one session. It carries two facts about that turn:

- **Its execution session** — the session the run's prompt was sent to. For an
  owner-targeted Run that is the Playbook's own session; for a fork Run or a
  verb it is the fork. For a dispatch that fans out to several sessions, no
  single session executes the run.
- **Its dispatch instant** — when the prompt actually reached that session.

Session state transitions may advance or settle a run only when they come from
its execution session *and* arrive after its dispatch instant. In particular:

- A run is armed before its prompt goes out, so that the transitions its turn
  produces are never dropped. Everything reported in that window belongs to the
  session's boot or to the turn before, and must be ignored.
- If the execution session was not observably idle at the dispatch instant, the
  `Running` already in effect is not this run's turn. The run's turn starts at
  the next `Running` after an intervening idle.
- The idle-without-a-turn backstop, which stops a dispatch that went nowhere, is
  measured from the dispatch instant, not from when the run was armed.
- A terminal state is the one exception to the execution-session rule: the
  Playbook's own session dying clears its run whoever was executing it, because
  nothing is left to render or settle those blocks.

## Reason

The daemon's view of a session's state is advanced by the same in-order event
drain that reports these transitions. That makes "the session was idle when we
dispatched" a statement about the event stream, not just about the session: every
event the adapter emitted before that idle has already been consumed, so a
`Running` seen afterwards can only be a new turn. This is the property that lets
a run trust a lifecycle signal at all.

Without it, a run inherits transitions it has no relationship to. A PTY harness
announces `Running` when it spawns and idles again at its first prompt; a Run
armed while that pair is still in flight saw a turn start and end within
milliseconds and settled its own shimmer immediately. A fork Run was worse: the
run is keyed by the Playbook's session, so the *owner* — a bystander that does no
work for this run — could settle the fork's blocks just by finishing something of
its own, or by finishing booting.

Both failures are invisible in the state that remains: the shimmer is simply gone,
and no record says why. The two Playbook e2e regressions that caught it
(#1145) failed about half the time on loaded CI runners and every time on a fast
local machine, purely on which of two microsecond-apart orderings won.

## Consequences

- Anything that delivers a Playbook run's prompt must report the dispatch — the
  session it went to and that session's state at the moment it went out.
  Delivery paths that skip this leave the run armed but never started: it will
  shimmer until it is settled by a declaration, by the executing session dying,
  or by the inactivity backstop. That is the safe direction to fail; a run that
  reads the wrong session's lifecycle is not.
- The state must be sampled *before* the prompt goes out. Sampling after it can
  observe the turn the prompt itself started and mistake it for a turn already
  in flight.
- A dispatch that fans out (one subagent per selected item) binds no execution
  session; such runs are settled by the agent's own declarations, by the
  dispatched sessions closing, or by the backstop.
- Progress signals other than state — first output, for instance — follow the
  same rule: they count for the run only from its execution session, and only
  after dispatch.

## Non-Goals

This does not make a run's stop signal ambiguity-free when a human types into the
execution session mid-run. Two turns in one session are indistinguishable from
the outside; the run stops with whichever one ends first.

## Examples

- Run on a session that is at its prompt. Prompt delivered → session goes
  `Running` → run is working → session idles → run settles.
- Run on a session that is mid-turn. Prompt is queued behind it. The in-flight
  turn's `Running` is not this run's; the idle that ends it is not this run's
  stop signal. The next `Running` starts the run.
- Fork Run. The Playbook's session may boot, run turns of its own, and idle
  repeatedly — the blocks keep shimmering. The fork finishing, closing, or dying
  settles them.
