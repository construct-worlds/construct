# 0108-pi-native-session-tracking

Status: accepted
Date: 2026-07-24
Area: harness
Scope: How construct tracks, resumes, resets, and forks pi coding agent conversations.

## Decision

The pi harness gives every construct session a **private pi session store**:
the adapter points pi's session-directory option at a folder inside the
construct session's own data directory instead of pi's global per-cwd store.
All native-session bookkeeping follows from that:

- **Native id.** pi persists one JSONL file per conversation, named with a
  sortable timestamp prefix and the conversation's UUID, and the file's
  header record states that UUID. The UUID is the native id persisted for
  the construct session. The newest file in the private store is the live
  conversation by construction — no cwd matching, originator tagging, or
  sibling-exclusion scans.
- **Resume.** On daemon respawn the adapter relaunches pi pointed at the
  captured conversation's file (resolved by UUID suffix within the private
  store) and seeds its transcript cursor past the file's existing records so
  history is never re-emitted. If the id or its file is missing, start
  fresh and say so rather than guessing.
- **Reset detection.** A new file appearing in the private store (pi's
  new-session flow), or the bound file's header re-minting its UUID, is a
  native reset: emit the native-id change so the daemon synthesizes the
  archived reset-snapshot fork (specs 0079/0085), rebind, and restart the
  cursor.
- **Native fork.** Same-harness forks pass the parent's UUID through the
  fork env var; the adapter resolves the UUID to a session file — searching
  its own store first, then every sibling construct session's store under
  the same sessions root — and launches pi with its fork flag so the child
  gets real model memory (specs 0031/0078). The sibling walk is what makes
  forking from a reset snapshot work: the snapshot's data dir holds the
  retired UUID while the file still lives in the original session's store.
  A forked file's copied history deliberately backfills into the child
  construct session's transcript from the top.
- **Chat fidelity.** Both modes translate pi's message objects (identical
  in the session file and in pi's headless JSON event stream) into
  structured events: assistant thinking/text/tool-call blocks, tool-result
  messages, live model and thinking-level changes, and per-call usage.
  pi's per-call usage states a full token split where the `input` figure
  EXCLUDES cache reads and writes (verified live: total = input + cacheRead
  + output), so the reported prompt side is input + cacheRead + cacheWrite
  with cacheRead as the cached subset (spec 0103), plus pi's own exact USD
  cost per call. pi states no context window, so the context gauge carries
  bare usage with no denominator (spec 0104). User messages are mirrored
  from the session file only in interactive mode; headless prompts arrive
  via session input, which the daemon already records.

## Reason

pi's global store keys sessions by working directory, so two construct
sessions in the same cwd would be indistinguishable there — the same
ambiguity codex solves with originator tags and kimi with sibling scans.
pi, uniquely among wrapped harnesses, lets the client choose the session
directory per invocation, which dissolves the problem instead of managing
it. Keeping conversations inside the construct session's data dir also
means archiving or deleting a construct session naturally owns its pi
history.

## Consequences

- The private store is authoritative: adapter changes must keep pointing
  pi at it in every mode (interactive, headless, resume, fork) — one spawn
  falling back to the global store strands that conversation where no
  construct session can find it again.
- Fork resolution depends on session data dirs of *other* construct
  sessions remaining readable under one shared sessions root; a layout
  change there must revisit the sibling walk.
- Deleting a construct session deletes its pi conversations with it.
  Reset-snapshot children whose files live in the original session's store
  lose fork-ability if the original session's data dir is deleted.
- pi conversations created this way do not appear in `pi --resume` pickers
  run manually in the project directory (they are not in the global
  per-cwd store). That is accepted; the construct session is the handle.

## Non-Goals

- Injecting construct's unified tool layer (MCP) into pi. pi has a JS
  extension system that could host it (the opencode pattern); that is
  future work, not part of this decision.
- Translating construct approval policy into pi (pi exposes no approval
  controls to translate to).
- Guessing context-window sizes from pi's model catalog: it is a
  model-name table, not a per-session report, and spec 0104 forbids it.

## Examples

- `construct new --prompt "fix the failing test" pi` runs pi's TUI in the PTY
  with the prompt as a launch argument; the transcript shows the user message,
  reasoning, tool calls/results, and per-call cost mirrored from the
  session file as pi writes it.
- Restarting the daemon relaunches pi on the same conversation file; the
  transcript continues without duplicated history.
- `/new` inside pi's TUI produces an archived pre-reset snapshot session in
  construct's lineage view, and the live session continues under the new
  conversation UUID.
- Forking the construct session yields a pi conversation whose file was
  forked by pi itself into the child's private store — asking the child
  about earlier turns works with full fidelity.
