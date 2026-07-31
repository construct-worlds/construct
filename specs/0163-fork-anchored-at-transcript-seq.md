# 0163-fork-anchored-at-transcript-seq

Status: accepted
Date: 2026-07-31
Area: architecture
Scope: A fork may branch from any transcript position, not only the conversation tail, and its lineage stamp describes the branch point.

## Decision

A fork operation accepts an optional anchor: the last transcript sequence
number (inclusive) carried into the fork. Without an anchor — or with an
anchor at or past the source's current tail — the fork branches from the
present, exactly as before anchors existed.

An anchored fork:

- seeds the new session from only the transcript events up to and including
  the anchor; everything after the anchor is what the fork is branching away
  from and must not leak into it;
- always uses the portable transcript seed, even for a same-harness fork of
  a harness with a native fork primitive — native forks branch at the
  conversation head only;
- stamps its fork-lineage record (`ForkedFrom`) with the ANCHOR's counters:
  the anchor seq itself, and message/token/busy tallies recomputed from the
  events at or before the anchor. The stamp describes the branch point, not
  the fork's creation moment. The fork's creation wall-clock time is still
  recorded separately (`at_ms`).

Lineage rendering must tolerate out-of-order anchors: a fork created later
may carry an earlier branch point than a checkpoint the parent's lane has
already passed (from a prior fork or merge). Lane checkpoints are
advance-only — a backdated stamp never regresses them, so a parent's events
are never counted into two windows. A backdated branch emits no new segment
row; instead its branch edge is labeled with how far back it reaches (in
messages when tracked, transcript events otherwise).

## Reason

Users need to retry a conversation from an earlier turn — the useful branch
point is rarely the tail. The transcript seq is the only stable, dense,
persisted per-session ordinal shared by every client and every stamp in the
lineage model (`ForkedFrom.transcript_seq`, `ForkMerge.merged_seq`), so it is
the anchor unit. Stamping anchor-accurate counters keeps lineage's
subtraction-based window math truthful: stamping "now" onto a backdated
branch would misattribute post-anchor work to the wrong window.

## Consequences

- `ForkedFrom.transcript_seq` now carries real information (it was
  previously always the tail at fork time); consumers must not assume fork
  stamps are monotonic in fork-creation order.
- Anchors are only meaningful within the current reset epoch: a transcript
  reset truncates history and restarts the counter, so pre-reset branch
  points are reachable only through reset snapshots (spec 0085), not
  anchors.
- The busy tally of an anchored stamp is an estimate replayed from
  persisted state-transition events; it is clamped so it never exceeds the
  parent's true accumulated busy time.
- Same-harness anchored forks lose native-fork fidelity (thinking blocks,
  native tool state) by design; the portable seed is the only faithful
  carrier for a mid-conversation branch.

## Non-Goals

- Anchoring native forks mid-conversation. No wrapped harness exposes a
  stable mid-conversation branch handle; if one appears, it can lift this
  limitation for that harness without changing the anchor contract.
- Turn-level semantics. The anchor is a raw transcript seq; "turns" are a
  client-side presentation built on top of it (see the turn-marker spec).

## Examples

- A session has 30 events. Forking with anchor 12 seeds events 1–12 and
  stamps `transcript_seq: 12` with the message/token counts as of event 12.
  The lineage lane shows the branch arrow at the seq-12 window; the parent's
  remaining 18 events land in its later windows.
- The same session already has a fork stamped at seq 20. A new fork
  anchored at seq 10 is created afterwards: the parent's checkpoint stays at
  20, no segment row is emitted for the new branch, and its edge is labeled
  "N msgs back".
