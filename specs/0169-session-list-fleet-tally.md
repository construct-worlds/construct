# 0169-session-list-fleet-tally

Status: accepted
Date: 2026-08-01
Area: ux
Scope: The session list titles itself with a tally of the fleet, counting the attention marker rather than the at-a-prompt run state as "waiting on you", and each entry opens into the rows it counted.

## Decision

**The session list carries a tally of its own rows in its title**, one entry
per reading worth acting on:

- **working** — sessions actively running
- **wants you** — sessions carrying the sticky attention marker (spec 0054)
- **errored** — sessions that failed

Each entry is a count plus a glyph. **The "wants you" entry wears the
per-row attention marker itself** — the same glyph and the same hue — so the
count and the markers it counts are visibly one signal rather than two
vocabularies the operator has to learn separately. Failure keeps a glyph of
its own, so the one reading that must survive a terminal with no color at
all still does.

This accepts a real cost: "working" and "wants you" are then separated only
by hue. That is the deliberate tradeoff — the marker's recognizability is
worth more than glyph-level distinctness on the two buckets that are both
"alive and fine." Opening an entry names it in words, and that naming is
what carries the distinction when color cannot.

**An entry opens into the rows it counted.** A count that names a problem
the operator then has to hunt for is half a signal, so each entry is a way
in: it lists its own sessions, and picking one selects that session and
brings it into view, scrolling the list if the row sat below the fold. The
list is the destination — the panel closes and hands off rather than
becoming a second place to work.

That panel is reachable two ways, and both are required. It opens on hover
and survives the trip to it — it lingers briefly after the pointer leaves
and holds open while the pointer is over it, so a listed row is actually
clickable. But hover cannot be the only door: terminals that never report
mouse motion exist, and there the panel would be permanently invisible.
Clicking the entry therefore pins it open until dismissed. Any surface
rendering this tally owes the operator a motion-free way in.

While the list is unfocused the tally dims **by attribute, not by
desaturation**. Shifting it toward gray would collapse the only channel
separating two of its three buckets.

The tally obeys four rules:

- **It counts what the list shows.** The unit is the rendered row, not the
  session: rows the list never renders are excluded, and a row standing in
  for hidden descendants — a collapsed subtree or group surfacing one
  rolled-up marker — counts exactly once, as itself. The tally can then
  never disagree with the marks beneath it, and every entry it counts is a
  row the operator can be taken to. Archived rows are the one deliberate
  exclusion: they are counted out even while the archive drawer is open, so
  the tally does not flicker on a disclosure toggle.
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

On glyphs: color is the least reliable channel available — terminal themes
remap it, monochrome and low-contrast terminals drop it, and a meaningful
share of operators cannot separate two adjacent hues. The first draft
therefore minted a distinct glyph per bucket. That was wrong for the
wants-you entry specifically: the operator already knows the blue dot from
the rows, and a summary that counts those dots while wearing a different
mark reads as a fourth thing to learn rather than as their total. Matching
the marker is worth more than the distinctness it costs, because the cost
falls between "working" and "wants you" — two buckets that are both fine —
while the reading that actually needs to survive a colorless terminal is
failure, which keeps its own glyph.

## Consequences

- Whatever detects "this session wants the operator" now feeds two surfaces
  — the per-row marker and the tally. Weakening that detection weakens the
  summary the operator scans, not just one row.
- The wants-you tally and the per-row marker must change together. They are
  one signal rendered at two scales, and a change to either that is not
  mirrored in the other breaks the equivalence this decision rests on.
- Adding a tally bucket means minting a glyph — distinct from the existing
  ones unless it, too, mirrors a marker the operator already reads — and
  giving it a name its panel can render.
- Because two buckets share a glyph, the panel's wording is load-bearing
  rather than a convenience. Any surface that renders the tally without it
  owes the operator another way to tell those two apart.
- The tally is now an interactive control on a title bar it shares. Its hit
  zones must stay disjoint from the controls beside it, and must not outlive
  the frame that drew them — a tally that is not rendered has no hit zone,
  or a collapsed pane inherits a phantom button.
- Selecting from the panel scrolls the list, which is otherwise moved only
  by the operator. That is accepted because the operator asked for that row;
  nothing else in this decision licenses moving the viewport on its own.
- The tally's placement is not fixed by this spec, but wherever it renders
  it must be able to yield: the title bar is shared with controls that own
  their geometry, and a summary is never worth overlapping a control.
- Hover text is the only place the tally's vocabulary is written out, so it
  must read as a sentence at every count, singular included.
- Because buckets are ranked rather than overlapping, a session that is both
  crashed and unseen appears once. An operator counting "how many things
  want me" must add the wants-you and errored entries. Such a row still
  paints both marks individually, so the visible attention markers can
  outnumber the wants-you count — the equivalence between that count and
  those marks holds for every row except this one.

## Non-Goals

- Not a notification or escalation mechanism. This spec governs what is
  legible at rest and what the operator can reach by asking; it says nothing
  about alerting, sounds, or pulling the operator toward a flagged session
  unprompted.
- Not a second session list. The panel is an index into the list, capped
  rather than scrollable, and closes on use. A bucket too long to show
  entirely says so and stops; the list itself remains the place to work.
- Does not replace the per-session attention marker (spec 0054). The marker
  says *which* row; the tally says *how many*. Both render.
- Does not standardize which specific glyph means which bucket, beyond the
  two constraints above: wants-you mirrors the attention marker, and failure
  stays distinct from both other buckets. Within that, glyph choice is a
  client-visible detail free to evolve.

## Examples

- A fleet of thirty: four agents mid-turn, twenty-three idle at prompts the
  operator has already seen, three that stopped while the operator was away,
  and no failures. The title reports four working and three wanting the
  operator; the twenty-three idle sessions are not tallied and no errored
  entry renders.
  The two counts render with the same mark, told apart by hue and by the
  words their panels carry; three of the rows below carry that same mark
  individually, and the title's count of them is the total.
- Only one of those three flagged rows is on screen; the pane is too short
  for the rest. The operator opens the wants-you entry, reads the three
  names, and picks the second. The list scrolls until that row is visible
  and selects it — the operator never scrolled by hand looking for a dot the
  title had promised.
- The operator focuses one of the three flagged sessions. Its marker clears,
  and the tally drops to two — without any run state having changed.
- A group is collapsed with two flagged sessions inside it. The group's row
  carries one rolled-up marker, the tally counts one, and its panel lists
  the group. Picking it lands on the group, where expanding reveals which
  two.
- Focus moves to the transcript. The tally fades but keeps its colors, so
  "which of these is waiting on me" is still answerable from the corner of
  the eye without clicking back into the list.
- The same fleet in a narrow sidebar, where the title bar has only enough
  room for the view-mode toggle: the tally renders nothing and the toggle
  keeps its position.
