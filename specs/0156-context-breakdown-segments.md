# 0156-context-breakdown-segments

Status: accepted
Date: 2026-07-28
Area: protocol
Scope: Adapters may report a labeled per-component breakdown of the context window, and clients render it in the context-gauge hover detail.

## Decision

A harness adapter may report, alongside the context-usage gauge (spec 0104),
a **breakdown** of what is occupying the context window: an ordered list of
labeled segments, each carrying a token count and an `estimated` flag. The
daemon stores only the latest report per session (gauge semantics: each
report replaces the previous; a context reset clears it), persists it with
the transcript so it survives daemon restart, and exposes it on the session
summary. Clients render the breakdown as the detail view behind the context
gauge — in the TUI, the hover tooltip on the modeline gauge; in the web UI,
the popover behind the runtime chip.

Rules:

- **Segments are additive components of the used context**, ordered
  fixed-prefix first (system prompt, guides, skills, tools, then
  conversation). Clients derive two synthetic rows: *free space*
  (`window − used`, only when the harness reports a window) and
  *unaccounted* (`used − Σ segments`, only when positive) — adapters never
  emit those themselves.
- **Estimates are allowed but must be marked.** A segment whose count comes
  from a char-length heuristic (rather than a harness- or provider-reported
  number) sets `estimated`, and every client renders estimated counts
  visually distinct (a `~` prefix). This is a deliberate, narrow carve-out
  from the anti-estimation rules of specs 0103/0104: the *gauge* numbers
  (used / window / lifetime tallies) remain harness-reported only; segment
  detail may be approximate because it answers "what is filling the window",
  not "how many tokens am I being billed".
- **Only report what the harness's own data surface makes derivable.** An
  adapter that can see the conversation content (its native transcript,
  rollout, wire log) may estimate a `messages` segment; one that can see the
  actual system prompt on disk may report a `system prompt` segment; one
  that can see nothing reports nothing, and the client keeps showing the
  plain used/window tooltip. No segment is ever invented from a model-name
  table or a hardcoded guess.
- **Report on change, not per poll**, at the same cadence as the
  context-usage gauge.
- **Differential segments are a sanctioned technique.** An adapter may
  report a `fixed overhead` segment measured as the difference between the
  harness-reported gauge and the sum of its estimated segments, *pinned at
  the first gauge report of a context epoch* — the moment the conversation
  (and therefore the char-heuristic error) is smallest. The fixed prefix
  does not change within an epoch, so the epoch-first residual stays valid
  as the conversation grows; re-deriving it later would re-absorb exactly
  the estimate drift the pin avoids. This is how a harness whose fixed
  prefix (system prompt, tool/MCP schemas, skills listings) never reaches
  any adapter-readable surface still gets a labeled row instead of a
  meaningless *unaccounted* remainder. Rules: the minuend must be the
  harness-reported gauge (never itself an estimate); the segment is
  real-minus-estimate and therefore stays `estimated`; the pin resets
  whenever the epoch changes (compaction, clear, session rebind); an
  adapter that cannot see a conversation estimate at all must not pin — a
  residual measured against nothing is just the gauge restated. Adapters
  with an on-disk usage history (transcripts, rollouts, wire logs) derive
  the pin statelessly from the epoch's first usage record so restarts and
  re-scans agree; adapters with snapshot-only gauges hold the pin in
  memory and accept that a restart mid-conversation re-measures at the
  current turn.

## Reason

The gauge says *how full* the window is; users debugging a bloated or
soon-to-compact session need *what* is filling it — fixed prefix vs
conversation — to know whether compaction, fewer tools, or a shorter guide
file would help. Harnesses expose no structured breakdown API, but every
adapter already tails a data surface that makes a useful approximation
derivable. Pushing the breakdown as a session event (rather than adding a
query into running adapters) reuses the exact plumbing the gauge uses:
adapters emit at the moment they already parse usage, the daemon's
persistence/recount/reset machinery applies unchanged, and every client
gets the data through the session summary it already receives. Spec 0086
explicitly reserved structured usage data for "a separate, explicitly
designed data path" — this event is that path.

## Consequences

- The daemon persists breakdown reports in the transcript and restores the
  latest one at load, exactly like the gauge; a context reset must clear
  segments together with used/window.
- Clients must treat the segment list as optional. Sessions of harnesses
  that report nothing render exactly as before this spec.
- Estimated segments must never be presented with the same authority as
  reported numbers — the `~` marking is part of the contract, not styling.
- The fixed carve-out means future adapters must not "upgrade" estimates
  into the gauge fields; the boundary between spec 0104's real numbers and
  this spec's approximations stays.
- Segment labels are display strings chosen by the adapter (lowercase,
  short); clients render them verbatim, so renaming a label is a
  user-visible change.

## Non-Goals

- No harness-side probe or RPC: the breakdown is computed from data the
  adapter already has, never by driving the harness UI (that surface stays
  spec 0086's, and its verbatim-panel cache stays unstructured).
- No billing semantics: segments explain window occupancy; token tallies
  and costs remain spec 0103's domain.
- No obligation of completeness: an adapter reporting only a `messages`
  estimate is compliant; the client's *unaccounted* row absorbs the rest.

## Examples

- Smith reports `system prompt`, `project guide`, `skills`, `tools`, and
  `messages`, all estimated (char heuristic), and the tooltip shows each
  with a `~` plus computed free space.
- The Claude adapter reports an estimated `messages` segment derived from
  its native transcript, plus a differential `fixed overhead` segment (the
  system prompt + tools it cannot see, measured at the epoch's first
  usage record); the tooltip shows both with a `~`, plus free space.
- Grok reports `system prompt` (from the prompt file its CLI writes to
  disk) and `messages`, both estimated.
- The shell harness reports nothing; hovering its gauge shows nothing new.
