# 0169-session-list-fleet-tally

Status: accepted
Date: 2026-07-31
Area: ux
Scope: The session list titles itself with a tally of the fleet, counting the attention marker rather than the at-a-prompt run state as "waiting on you".

## Decision

**The session list carries a tally of its own rows in its title**, one entry
per reading worth acting on:

- **working** — sessions actively running
- **wants you** — sessions carrying the sticky attention marker (spec 0054)
- **errored** — sessions that failed

Each entry is a count plus a glyph. Glyphs are distinct from one another, so
the tally survives a terminal that drops color; color reinforces, and the
"wants you" entry takes the same hue as the per-row attention marker so the
count and the dots it counts read as one signal. Hovering an entry names it
in words — the glyphs are compact by design, and the tooltip is where the
vocabulary is actually spelled out.

The tally obeys four rules:

- **It counts what the list shows.** Rows the list folds away or never
  renders are excluded, so the summary can never disagree with the rows
  beneath it.
- **Buckets are disjoint and ranked.** Every counted session lands in
  exactly one bucket, most specific reading first: errored, then wants-you,
  then working. A crash the operator hasn't seen is counted once, as a
  crash.
- **Zero is silent, and the tally yields.** Empty buckets render nothing,
  and the whole tally is dropped rather than collide with the controls
  sharing its title bar.
- **It is a scan list, not a census.** It will not sum to the fleet size,
  by design.

**"Waiting on you" is the attention marker, not a run state.** The state
that means "sitting at a prompt" is the fleet's resting state, and clients
must not dress it up as blocked: it shares the working state's glyph and
color, and it is not tallied. Only the attention marker means a session
wants the operator.

## Reason

The operator's most frequent question about a fleet is "which of these is
waiting on me?" Per-row markers only answer it for rows currently on screen;
a fleet longer than the pane, or a scrolled list, has no summary at all —
and the count is exactly what decides whether the operator needs to look.

The subtler half of this decision is *what* counts as waiting. An earlier
attempt gave the at-a-prompt state its own "waiting" glyph and color, on the
theory that it was the operator-blocked state. It is not. It is where
sessions rest: a healthy fleet is mostly at prompts, so that rendering
flagged everything, and a signal that fires on the resting state carries no
information. The marker in spec 0054 already encodes the real predicate —
*stopped, after activity the operator hasn't seen* — and it is sticky, so it
survives the transition that produced it and clears when the operator
actually looks.

Tallying idle sessions was rejected for the same reason: it would put the
largest and least actionable number in the title.

Glyph-first because color is the least reliable channel available: terminal
themes remap it, monochrome and low-contrast terminals drop it, and a
meaningful share of operators cannot separate two adjacent hues. Two counts
distinguished only by hue, rendered side by side, would be unreadable.

## Consequences

- Whatever detects "this session wants the operator" now feeds two surfaces
  — the per-row marker and the tally. Weakening that detection weakens the
  summary the operator scans, not just one row.
- Adding a tally bucket means minting a glyph distinct from the existing
  ones, and adding hover text for it.
- The tally's placement is not fixed by this spec, but wherever it renders
  it must be able to yield: the title bar is shared with controls that own
  their geometry, and a summary is never worth overlapping a control.
- Hover text is the only place the tally's vocabulary is written out, so it
  must read as a sentence at every count, singular included.
- Because buckets are ranked rather than overlapping, a session that is both
  crashed and unseen appears once. An operator counting "how many things
  want me" must add the wants-you and errored entries.

## Non-Goals

- Not a notification or escalation mechanism. This spec governs what is
  legible at rest; it says nothing about alerting, sounds, or navigating to
  a flagged session.
- Does not replace the per-session attention marker (spec 0054). The marker
  says *which* row; the tally says *how many*. Both render.
- Does not standardize which specific glyph means which bucket. That is a
  client-visible detail free to evolve, as long as distinctness holds.

## Examples

- A fleet of thirty: four agents mid-turn, twenty-three idle at prompts the
  operator has already seen, three that stopped while the operator was away,
  and no failures. The title reports four working and three wanting the
  operator; the twenty-three idle sessions are not tallied and no errored
  entry renders.
- The operator focuses one of the three flagged sessions. Its marker clears,
  and the tally drops to two — without any run state having changed.
- The same fleet in a narrow sidebar, where the title bar has only enough
  room for the view-mode toggle: the tally renders nothing and the toggle
  keeps its position.
