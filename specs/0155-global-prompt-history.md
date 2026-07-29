# 0155-global-prompt-history

Status: accepted
Date: 2026-07-28
Area: persistence
Scope: A daemon-owned, capped FIFO of the user's prompts across all sessions, feeding suggestion generation and client-side prompt recall.

## Decision

The daemon keeps one global prompt history: the prompts the user has
sent to any user-kind session, newest first, persisted across daemon
restarts. The contract:

- **User voice only.** An entry is recorded for every user message on a
  user-kind session, wherever it entered (composer input, a creation
  prompt, a prompt typed directly into the harness's terminal).
  Machine-written prompts — orchestrator observations, subagent briefs,
  hidden probe prompts — are never recorded. Slash commands are skipped:
  they are UI commands, not reusable prompts.
- **FIFO with shell-history dedupe.** The history is capped at a fixed
  size; old entries fall off the tail. Re-sending a prompt that is
  already in the history moves it to the front (refreshing its
  timestamp) instead of storing a duplicate. Oversized pastes are not
  recorded.
- **Best-effort persistence.** A missing or corrupt history file
  degrades to an empty history; it is never a startup dependency and
  recording failures are silent.
- **Readable by any client.** An IPC method returns the retained
  history, newest first, with an optional limit. Clients may offer the
  entries for direct re-send (the web deck's "history" stack); a
  selected entry goes through the ordinary session-input path,
  verbatim.
- **Feeds suggestion generation.** A bounded, labeled block of recent
  history entries is appended to the suggestion-generation context
  (spec 0109) so the generator can mirror the user's real phrasing and
  recurring workflows — with instructions that suggestions must stay
  grounded in the target session's transcript, never blind replays of
  old prompts.

## Reason

Users repeat themselves across sessions ("run the tests", "open a PR",
a favorite review incantation), and a generator that has only one
session's tail writes suggestions in a generic voice. A small global
history makes recall a two-tap selection on mobile (where retyping is
most expensive) and gives every suggestion generator the user's actual
diction for free. Daemon ownership keeps the history consistent across
web, TUI, and CLI clients instead of per-client localStorage silos.

## Consequences

- Recording sits on the same path that observes every user message, so
  new input surfaces inherit history recording automatically.
- The history is global, not per-project: an entry may name files or
  branches from another workspace. Consumers (generators, users
  re-sending) must treat entries as voice/recall material, not as
  guaranteed-valid commands for the current session.
- Dedupe is exact-text: near-duplicates accumulate as separate entries
  until they age off the FIFO. Accepted for simplicity.
- The injected generation block and the client-facing list are bounded
  regardless of the retained cap, so growing the cap must not bloat
  generation prompts.

## Non-Goals

- No cross-session semantic search, ranking, or frequency weighting —
  recency order only.
- No per-session or per-project scoping of the history surface.
- No recording of harness-internal or agent-generated prompts.
