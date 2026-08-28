# 0211-transcript-scan-checkpoint

Status: accepted
Date: 2026-08-28
Area: persistence
Scope: Derived per-session state is rebuilt from transcripts incrementally, resuming from a checkpoint, so daemon startup costs new history rather than all history.

## Decision

A session's transcript is the source of truth for several fields that are
*derived* rather than authored: the durable sequence counter, the chat
message count, the last-message snippet, the last-error snippet, the lifetime
token tally (spec 0103), the context gauge (spec 0104), and the fleet
token-history samples (spec 0167). The daemon rebuilds them at startup so a
record written before a field existed — or one that lagged a crash — heals
itself without a migration.

That rebuild is **incremental**. Each session persists the derived state
alongside its transcript, together with the transcript byte length that state
reflects. A later rebuild compares the recorded length against the file's
current length and reads only the bytes past it. Equal lengths read nothing.

This is sound because transcripts are **append-only**: a recorded length is
always a line boundary, and content already scanned never changes.

### The checkpoint is a cache, never an authority

A checkpoint may be absent, stale, truncated, or written by a build whose
fold differed. Each of those is answered the same way — discard it and walk
the transcript from the start. Deleting every checkpoint on disk must cost
one slow startup and nothing else.

It follows that a checkpoint is versioned and never migrated. When the fold
gains a field or changes what an existing one means, the version moves and
old checkpoints are rebuilt rather than upgraded.

A resumed fold must land on exactly the state a full walk of the same bytes
would produce. Any value carried *across* the resume point is therefore part
of the checkpoint, including scan-position state that is not itself a
reported field — notably the model in effect where the scan stopped, without
which usage recovered after the resume is attributed differently than a full
walk would attribute it.

### Startup binds the socket on new history, not all history

The rebuild runs before the IPC socket binds, so its cost is time during
which no client can reach the daemon at all. A full walk is proportional to
everything ever recorded and grows without bound; the checkpointed walk is
proportional to what was appended since the last start.

Startup must report what it read — how many sessions were current, resumed,
or rebuilt, and how much was actually read. A slow start is then a readable
fact rather than something to be inferred.

### Checkpoints are written where nothing is appending

Checkpointing requires sampling a transcript's length and its folded state as
one consistent pair. A length captured while the session is appending
describes state the fold does not have, in whichever direction the race
resolves: recording too much skips events forever, recording too little
counts them twice.

The rebuild therefore writes checkpoints during startup, before any adapter
is resumed and while nothing can append. A checkpoint written anywhere else
must hold whatever lock the append path holds.

## Reason

Sessions accumulate transcripts indefinitely, and a fleet's total transcript
volume is unbounded in a way its session *count* is not: a few hundred
sessions can hold gigabytes. Re-parsing all of it on every start put tens of
seconds between launching the daemon and its socket existing (#1313) — long
enough that the readiness wait gave up and reported a daemon that had failed,
while it was in fact still reading.

Persisting the derived state is what makes the cost proportional to change
rather than to history. Keeping it a discardable cache is what keeps that
optimization from becoming a second source of truth that can disagree with
the transcript — which, for self-healing fields, would defeat their purpose.

## Consequences

- Any new field folded out of transcripts belongs in the checkpoint, and
  adding one moves the checkpoint version.
- Scan-position state that influences later events is part of the fold's
  result, not scratch, and has to be persisted with it.
- Deleting a checkpoint must remain safe at any time, including by hand.
- A checkpoint is per session and lives with that session's data, so removing
  a session removes it with no separate bookkeeping.
- Anything that rewrites a transcript in place, rather than appending to it,
  breaks the length check and must invalidate the checkpoint explicitly.
  Truncation is already detected; silent same-length rewriting is not.
- Startup remains correct without any checkpoint, so this can be disabled or
  bypassed for diagnosis by removing the files.
- Data recovered only in a bounded time window (spec 0167's samples) is
  pruned to that window when written, so a checkpoint cannot grow without
  bound with history no consumer would accept back.

## Non-Goals

- Rotating, compacting, or truncating transcripts. This makes reading them
  cheap; how large they are allowed to grow is a separate decision.
- Continuously flushing checkpoints while sessions run. Startup-time
  checkpointing already bounds the read to one daemon lifetime's growth, and
  writing during operation requires coordinating with the append path.
- Replacing the summary record. The checkpoint holds derived state only; a
  session's authored fields are unaffected.

## Examples

- A daemon restarts a minute after the last one stopped: nearly every
  transcript is byte-identical to its checkpoint, nothing is read, and the
  socket binds immediately.
- A daemon runs for a week and restarts: only the week's appended bytes are
  read, however large the accumulated history behind them.
- A session's transcript is truncated by hand: its checkpoint claims more
  bytes than the file holds, so it is discarded and the session's tallies are
  rebuilt from what remains.
- A session switched models before the last checkpoint and reports usage
  after it: the recovered sample still names the model that was in effect,
  because the resumed fold started from it.
