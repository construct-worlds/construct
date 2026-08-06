# 0193-muse-native-session-tracking

Status: accepted
Date: 2026-08-05
Area: harness
Scope: Muse sessions bind native logs by process identity and persist stable UUIDs for reliable resume and transcript projection.

## Decision

Construct persists the active Muse UUID alongside its own session state and
resumes that UUID after adapter or daemon restart.

Interactive sessions bind their UUID by matching the actual spawned Muse
process ID against Muse's durable session metadata. They never select a session
by global modification order. Headless sessions mint a UUID before the first
turn and reuse it for every structured execution. Only root Muse sessions are
eligible; Muse-owned subagent logs never compete for the parent binding.

Same-harness forks use Construct's portable transcript seed. They do not resume
the parent's Muse UUID because Muse exposes resume but no native conversation
fork operation.

## Reason

Muse stores sessions by UUID and can resume them, but an interactive launch does
not accept a caller-selected session ID or a session-store override. Selecting
the newest global Muse session would race sibling agents working in the same
repository. Muse records its process ID in each root log, providing a stable
origin signal without isolating the user's plugins, skills, or other Muse data.

## Consequences

- Restarts continue the same Muse conversation when its UUID has been captured.
- Headless turns share one native conversation rather than starting isolated
  one-shot sessions.
- Interactive transcript projection tails only the process-matched root session
  log and ignores nested Muse subagent logs.
- Muse's own approval and sandbox defaults remain authoritative.
- A fork gets portable conversation context but not Muse-internal state that is
  absent from the transcript.

## Non-Goals

This decision does not mirror Muse-native subagents, inject Construct tools into
Muse, or translate Construct approval modes into Muse policy.
