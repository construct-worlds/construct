# 0087-pty-input-ack-on-enqueue

Status: accepted
Date: 2026-07-12
Area: protocol
Scope: The client-facing PTY-input request acknowledges acceptance into an ordered per-session queue, not adapter delivery.

## Decision

When a client sends interactive PTY input for a session, the daemon
responds as soon as the bytes are accepted into that session's ordered
input-delivery queue. The adapter round-trip (actually writing the bytes
into the harness's PTY and receiving the adapter's acknowledgement)
happens afterwards, on a dedicated per-session writer, off the
connection's request-dispatch path.

Three invariants hold:

1. **Per-session order is absolute.** Every producer of PTY input —
   interactive typing from any client, daemon-internal prompt
   submission, synthesized control responses — funnels through the same
   per-session queue, so bytes reach the harness in acceptance order
   regardless of which mix of paths is active.
2. **Daemon-internal senders may still await delivery.** Internal flows
   that sequence real-time behavior against the harness having received
   the bytes (e.g. paste-then-settle-then-Enter submission) use a
   delivery-acknowledged variant. Their jobs travel through the same
   queue as everything else; only their completion signal differs.
3. **The queue is bounded and fails loudly.** If an adapter stops
   consuming input long enough to fill the queue, further input is
   rejected with a visible error rather than buffered without limit.
   Typing into a session with no live adapter still fails synchronously
   at accept time.

## Reason

The daemon serves each client connection's requests serially, and
clients pump keystrokes one request at a time, awaiting each response.
When the input response meant "the adapter acknowledged delivery", a
single slow or CPU-starved adapter transitively froze typing into
*every* session (the client's input pump is fleet-wide) and delayed all
other requests queued on that connection — observed live as "the TUI
still repaints and clicks work, but typing hangs for seconds". Typing
responsiveness must not depend on the health of any adapter process.

## Consequences

- A successful input response no longer proves the harness received the
  bytes. Delivery failures after acceptance are logged (and the input is
  dropped), not returned to the sender. Clients must not build
  read-after-write logic on the input response; anything that needs
  delivery confirmation belongs in a daemon-internal flow using the
  delivery-acknowledged variant.
- Input queued while an adapter is being respawned is delivered to the
  new adapter: the writer resolves the session's current adapter per
  batch, at delivery time.
- The per-session writer must remain the *only* path that performs the
  adapter input round-trip; a new code path writing input directly to
  the adapter would silently break invariant 1.
- Backpressure is coarse (a fixed batch-count bound per session).
  Sustained input into a wedged adapter surfaces as rejected input, not
  as slowed-down clients.

## Non-Goals

- PTY resize and non-PTY structured input keep their existing
  synchronous request semantics; this decision covers the PTY byte
  stream only.
- No cross-session fairness or prioritization between sessions' queues;
  each session's writer is independent.

## Examples

- A user types into session A while session A's harness is starved by a
  CPU spike: the client's input requests ACK immediately; the typed
  bytes render when the harness catches up. Typing into session B in
  the meantime is unaffected.
- A program run submits a bracketed paste and then an Enter keypress
  with a settle delay between them: the paste job's delivery is awaited
  before the delay starts, so the Enter cannot outrun the paste even
  though interactive typing is being queued concurrently — and all of
  it reaches the harness in order.
