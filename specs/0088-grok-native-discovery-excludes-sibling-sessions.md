# 0088-grok-native-discovery-excludes-sibling-sessions

Status: accepted
Date: 2026-07-12
Area: harness
Scope: Grok's harness-native reset detection must not mistake a sibling construct session's activity for its own conversation being cleared.

## Decision

Grok stores its own native sessions per-cwd (`~/.grok/sessions/<cwd>/<uuid>/`), not per construct-session, and has no originator-tag construct can stamp and match on ([0138](0138-harness-native-session-id-tracks-clear.md)). Its native-reset discovery falls back to "the non-fork session directory with the newest mtime under this cwd."

When multiple construct sessions run Grok against the same cwd, that heuristic must exclude any directory that another live, same-cwd, same-harness construct session currently reports as its own native id (each session's own tracked native id is readable from its own on-disk record). A sibling's routine turn-taking touches its own directory's mtime; without exclusion that alone can look identical to this session's own conversation having been cleared.

When resuming a persisted Grok session, discovery must also exclude every other native session directory that already existed before the resumed adapter spawned its Grok process. Resume is already bound to its persisted native id, while a genuine future `/clear` creates a new UUID. This startup baseline prevents a bulk daemon session restart from converging every construct session onto whichever Grok directory resume happened to touch last, even if live sibling metadata is incomplete or stale.

## Reason

Grok's CLI rewrites each native session's own metadata file on every turn via an atomic replace, which updates that directory's mtime as a side effect. Two or more construct sessions sharing a cwd (the common case for any session not given its own worktree) therefore constantly perturb "which directory is newest" in that shared folder, independent of whether either session's own conversation was ever cleared.

Before this decision, a sibling simply taking a turn could make an idle session's watcher believe its own native conversation had been reset: it would fork-and-archive a full copy of the session's transcript-so-far, then rebind the live session's tracked native id onto the unrelated sibling's conversation — corrupting resume/fork for the live session and leaving behind a duplicate-transcript archived session per false positive. Left running, this compounds without bound (transcript duplication grows with each false fork) and was observed producing over a thousand archived forks across a handful of long-lived, same-cwd Grok sessions, the large majority of stored session data.

## Consequences

- Grok's discovery excludes, in addition to native-subagent ids and (for forks) the fork's own parent, the current native id of every other live construct session that shares this session's cwd and harness.
- A resumed Grok adapter snapshots the cwd's native session ids before spawning Grok and permanently excludes every preexisting id except its own. Native directories created afterward remain eligible for `/clear` rebinding.
- That exclusion set is read from each sibling's own on-disk native-id record and cached between poll ticks, since a full metadata scan every 500ms is needless I/O. The cache is only a fast path: before adopting any different native id, the watcher synchronously refreshes sibling ownership and rescans. A fresh construct session prewrites its native id before spawning Grok, so its directory cannot be mistaken for another session's reset even when it appears between periodic refreshes.
- A sibling's already-reported native reset is rejected by the same pre-adoption refresh. Grok does not expose an originator tag for the instant between creating a reset directory and its owning watcher recording that id, so that narrower in-process reset race remains best-effort.

## Non-Goals

- Identifying arbitrary Grok CLI processes launched outside construct. They have no construct metadata or originator tag, so a post-start external native directory remains indistinguishable from this session's own reset.
- Guaranteeing ownership during the sub-second interval after a same-cwd Grok process creates an in-process reset directory but before its construct watcher records the new id. Fresh construct sessions are protected because they prewrite their id before spawn; Grok-native `/clear` does not provide the same signal.
- Changing discovery for harnesses that already match on an originator tag (Claude, Codex, Antigravity) — those are not exposed to this failure mode.
- Retroactively cleaning up archived forks created before this decision; that is an operational cleanup, not a behavior this decision governs.

## Examples

1. Two construct sessions, both running Grok, both cwd `/repo`. Session A is idle (awaiting input); session B takes a turn, which rewrites B's own native session's metadata file and bumps its directory's mtime. A's watcher polls, sees B's directory is now newest, but finds it in A's exclusion set (B reported that id as its own) and does not rebind — A's conversation is untouched.
2. Same setup, but the exclusion cache predates B's existence. A's fast scan sees B's new directory, synchronously rereads sibling ownership before rebinding, finds the id B wrote before spawn, and remains attached to A. B's cost events appear once in the fleet token meter rather than once per same-cwd watcher.
