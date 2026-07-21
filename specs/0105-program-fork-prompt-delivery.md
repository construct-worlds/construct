# 0105-program-fork-prompt-delivery

Status: accepted
Date: 2026-07-21
Area: architecture
Scope: How the first prompt reaches a freshly forked Program-execution session.

## Decision

When a Program Run or verb executes in a newly created fork, the daemon
delivers the fork's first prompt asynchronously, after the fork's harness
is observably ready — not inline in the execute request:

1. **Wait for startup.** Before sending anything, wait for the fork's PTY
   to go quiet after its harness's startup draw (bounded by a hard
   timeout, after which delivery proceeds anyway).
2. **Gate the submit on consumption.** For external agent TUIs the prompt
   is a bracketed paste followed by a submit Enter; the Enter is sent only
   after the PTY shows output following the paste (bounded, with the old
   fixed-delay behavior as the timeout fallback). The gate watches for any
   post-paste output rather than the prompt text itself, because these
   TUIs collapse long pastes into a placeholder that never echoes the
   body.
3. **Deliver in the background.** The execute response returns as soon as
   the fork exists and its run bookkeeping is seeded. A successful execute
   response therefore means "the fork exists and delivery is underway",
   not "the prompt has been delivered". Delivery failure is logged; verb
   forks additionally drop their pending merge registration and archive
   the fork so nothing waits forever on a session that never got its
   instructions.

Prompts to *already-running* sessions (owner-targeted Run/verb execution)
keep their synchronous, delivery-acknowledged path.

## Reason

A fork cold-starts its harness process, often through a native
fork-resume. A harness still busy with its own startup work does not drain
stdin: a prompt pasted during that window either vanishes into the
pre-draw screen, or accumulates in the PTY buffer together with the submit
Enter so both arrive in one read — the Enter then parses as part of the
paste burst instead of a submit keypress, and the prompt sits visibly
typed but never submitted. Either way the fork idles forever. This is the
same race the harness usage probe hit and solved (see the usage-probe
spec); Program forks need the same treatment.

The wait can last as long as the harness takes to boot (seconds), and
execute requests run on an IPC dispatch loop that serves a connection's
requests serially — waiting inline would freeze the requesting client's
entire connection for the duration.

## Consequences

- Clients must not assume the fork has received (or started acting on) its
  prompt when the execute call returns; the fork's own state/events are
  the signal that work began.
- The startup wait trades a few seconds of latency before the fork's first
  turn for reliable delivery; UIs should expect a booting fork to be
  briefly idle before its prompt appears.
- Any future path that sends a first prompt into a session it just created
  over the PTY must use the same settle-then-gate delivery, not a fixed
  delay.

## Non-Goals

- Changing how prompts reach already-running sessions.
- A daemon-side guarantee that a delivered prompt was *accepted* by the
  harness's input model; the gate proves consumption of bytes, not
  semantic acceptance.
