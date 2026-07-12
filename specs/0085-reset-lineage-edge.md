# 0085-reset-lineage-edge

Status: accepted
Date: 2026-07-11
Area: harness, protocol, tui
Scope: A harness-native context reset (`/clear`, `/branch`, `/new`, and equivalents) synthesizes a real, archived fork of the pre-reset conversation, so it stays selectable, readable, and forkable through construct's existing fork/archive machinery.

## Decision

The live session never changes identity across a reset: its own native conversation id is rewritten in place (`x → y → ...`) exactly as spec 0079 already implements — that part is unchanged. What's new is the *other* half: on each detected native-id change, construct synthesizes a real, ordinary child session holding a frozen copy of the pre-reset conversation — "reset is fork-and-archive, plus switching the live session's own resume id," not a new kind of node.

Concretely, when an adapter emits `SessionEvent::NativeIdChanged { prior_native_id, new_native_id }` (still the same event spec 0079's detection point produces — only what the daemon does with it changes), the daemon:

1. Snapshots the live session's current transcript (`event_count`/`busy_ms`/`message_count`) and reads its full transcript up to that point.
2. Mints a new session id and builds a `SessionSummary` for it: `forked_from: Some(ForkedFrom { session_id: <live session's id>, transcript_seq, at_ms, parent_busy_ms, parent_message_count, is_reset_snapshot: true })` — an ordinary fork record, using the same struct every user-initiated fork already uses, just with one new flag set.
3. Born already `archived: true`, `state: Done`, titled `"(cleared) <title-or-harness>"` (mirroring the existing `"(fork) <title>"` convention).
4. Writes the new session's *own* native-id file (`claude_session_id.txt` and equivalents) to `prior_native_id` — the id that's about to be retired on the live session.
5. Copies the live session's transcript events verbatim into the new session's own transcript.
6. Persists and broadcasts it, the same "daemon builds a `SessionSummary`/`SessionEntry` directly, no adapter, save + insert + broadcast" shape `SessionEvent::NativeSubagent` projection already uses.

Because the archived snapshot is a real session with its own real, never-again-overwritten native-id file, every downstream concern is already-existing, unmodified machinery:

- **Selecting it** in the lineage view is an ordinary session jump — no synthetic node, no popup, no windowing math. Archived sessions were already fully visible/walkable in the lineage tree and session list (`session.list()` has no `archived` filter) before this feature existed.
- **Reading it** is an ordinary transcript view of an ordinary (archived) session.
- **Forking from it** is the existing, unmodified same-harness-fork-resume path (`lifecycle.rs`'s native-fork spawn logic reads *the fork source's own* current native-id file — which, for an archived snapshot, is permanently `prior_native_id`, since nothing ever overwrites it again).

The one visible difference from a user-initiated fork: `ForkedFrom.is_reset_snapshot` drives a distinct lineage edge glyph (`↺` instead of `⑂`), checked before the ordinary `LineageEdge::Fork`/`Subagent`/`Root` match rather than added as a new edge variant — it *is* an ordinary fork edge, just one a user didn't create on purpose.

## Reason

`session.list()`/the lineage tree walk already have zero archived-session filtering, and `SessionEvent::NativeSubagent` handling already establishes the "daemon synthesizes a `SessionSummary`/`SessionEntry` directly, no adapter, persist + insert + broadcast" pattern. Reusing fork + archive as the reset primitive, rather than inventing a synthetic third node type with its own windowed-read and popup machinery, means almost every consumer (selection, reading, forking, merge-eligibility, session-list rendering) needs zero new code — it already knows how to handle an ordinary archived, forked session. The only genuinely new logic is the synthesis step itself (transcript copy + native-id-file write), which has no existing precedent to reuse but is a small, self-contained daemon-side operation.

## Consequences

- All four adapters spec 0079 covers (Claude, Codex, Antigravity, Grok) emit `SessionEvent::NativeIdChanged` at their existing native-id-change detection point — unchanged from the harness-integration side; only the daemon's handling of that event changed.
- `ForkedFrom` gains one field, `is_reset_snapshot: bool` (`#[serde(default)]`) — no new struct, no new protocol surface beyond that.
- `crate::lineage` needs no new `LineageEdge` variant, no new node field, no new tree-construction pass: the ordinary `Fork`-edge derivation already reads `forked_from.{transcript_seq,at_ms,parent_busy_ms,parent_message_count}` to place the branch arrow and size the live session's pre-branch window, and `SessionState::Done` already renders a completed lane's final turn-info line correctly. The only lineage.rs change is the glyph check.
- The merge-eligibility guard (`m` in the lineage section) excludes archived forks (`!s.archived`), not just merged/discarded ones — a reset snapshot is `merge: None` but will never be mergeable, and this closes the same gap for any other archived-but-unmerged fork.
- Antigravity has no native fork primitive (per spec 0079/0078), so a reset snapshot for it is still created and fully readable, just not natively-forkable — the same limitation an ordinary Antigravity fork already has.

## Non-Goals

- Does not change what a fork or subagent edge fundamentally is (specs 0078, 0014 unchanged) — a reset snapshot is a fork, distinguished only by a flag.
- Does not retroactively backfill snapshots for clears that happened before this lands; only clears observed after adapters start emitting `NativeIdChanged` are recorded.
- Does not distinguish *which* slash command triggered a reset (`/clear` vs `/branch` vs `/new`) — adapters other than Claude can't reliably tell the cause of a native-id change apart, so the event stays cause-agnostic.
- Does not expose any new MCP/CLI parameter for this — synthesis is entirely daemon-internal, triggered only by the adapter event; forking from an archived snapshot uses the plain, existing fork call with no special parameter needed.

## Examples

A user runs Claude, works for a while, types `/clear`, keeps working, then `/clear`s again. The live session's lineage node now shows two `↺`-glyph, archived fork siblings hanging off it — each an ordinary session, titled `"(cleared) <title>"`, each holding a real copy of the transcript up to its own clear point. Selecting either jumps to it exactly like selecting any archived session; forking from either resumes that snapshot's own frozen native id, through the same fork action used everywhere else, landing as an ordinary new fork of *that* snapshot.
