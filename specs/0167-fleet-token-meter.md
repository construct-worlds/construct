# 0167-fleet-token-meter

Status: accepted
Date: 2026-07-31
Area: ux
Scope: A realtime fleet-wide token-throughput history, grouped by model, rendered as a selectable body of the ambient panel.

## Decision

Clients may render a **fleet token meter**: a scrolling history of token
consumption across every session, binned into fixed-width time buckets and
stacked by model. It is built entirely from the per-call usage reports
sessions already broadcast — the same reports that feed the lifetime tally
(spec 0103) and the context gauge (spec 0104). No new capture path, no
polling, and specifically not the model router, whose default transport is a
blind byte splice that must never be parsed for observability (spec 0113).

### Attribution

Usage reports carry the model that consumed them, spelled exactly as that
adapter spells the same model in its model-change reports. A client
attributes a sample in this order:

1. the model named on the report;
2. the reporting session's currently-tracked model;
3. an explicit `unattributed` series.

A sample is never dropped for want of a label. A graph that silently omits
volume it measured misstates the total, which is worse than naming an
unattributed slice.

The two spellings — usage report and model-change report — must not diverge
within an adapter. A label that disagrees splits one model into two series
and two colors, which reads as two models running.

### It is a throughput history, not a utilization gauge

Token throughput has no capacity to divide by. The meter therefore
autoscales: the ceiling is the tallest column currently visible, and the
rendered scale value is stated alongside the graph. It falls back to a floor
so a single small sample on an idle fleet cannot paint a full-height column
and read as saturation. After a burst scrolls out of view the ceiling
descends gradually rather than snapping, so consecutive frames stay
comparable — but it may never fall below a column still on screen.

No percentage is shown, and no fixed ceiling is assumed. A percentage here
would require inventing a denominator.

### Buckets are arrival time

A sample lands in the bucket in which its report *arrived*, not spread over
the interval in which the tokens were actually produced. A long streaming
call reports once, at the end, so its entire payload lands in one bucket and
the graph is bursty by nature. Smoothing by smearing a sample backwards
would look better and would be a fabrication — the no-estimating rule of
spec 0103 applies here too.

### Series identity is stable

A model's color is assigned when it is first seen and never reassigned.
Rank order changes constantly on a live fleet; a color that followed rank
would repaint history under the user, so a column drawn a minute ago would
no longer mean what it meant when drawn. Models beyond the palette share one
"other" color and are reported as a single collapsed legend row rather than
reusing a color that already identifies another model.

### Coverage is whatever harnesses report

Harnesses that report no usage contribute nothing and are not estimated
(spec 0103's non-goal). The meter must not imply otherwise: an empty meter
states that nothing has been reported rather than rendering an empty grid.

## Reason

Users running many sessions have no way to see where model spend is
actually going in the moment — the lifetime tally answers "how much did this
session use", and the context gauge answers "how full is this conversation",
but neither answers "what is the fleet burning right now, and on which
model". That question is asked while work is in flight, which makes it an
ambient display rather than a report.

Building it on the existing usage broadcast rather than on the router is
what makes it cheap and complete at once: the reports already arrive for
every session at every client, whereas the router observes only the fraction
of sessions with an armed route and is contractually forbidden from
inspecting the rest.

## Consequences

- Adapters that report usage should name the model on the report whenever
  the harness states one, and must use the same spelling as their
  model-change reports.
- The model on a usage report is optional and additive: records written
  before it existed must keep loading, and a report that omits it must
  serialize the same way it always did.
- Any client rendering the meter must state the scale it drew against.
  Bars without a stated ceiling are unreadable once the ceiling moves.
- Attribution is a client concern. A session's own lifetime tally stays
  model-agnostic; per-model accounting is derived at the display, not
  accumulated per session.
- The meter is fed continuously whether or not it is on screen, so
  selecting it shows real history rather than starting blank. This bounds
  how much history can be retained — it is a fixed-size ring, not a log,
  and it does not survive a client restart.
- A sub-quantum share within one column may be omitted from that column's
  stack; the column's height and the legend totals still account for it.
  Promoting it to a visible slice would take that space from a series that
  earned it.

## Non-Goals

- Cost in dollars. The usage reports carry a dollar figure, but this
  display is about volume; a currency readout is a separate decision.
- Per-session breakdown. The meter is a fleet aggregate grouped by model;
  which sessions contributed is answered by the session list.
- Persisting history across client restarts, or reconciling it against
  provider-side billing. This is an ambient live view, not an accounting
  record.
- Deriving throughput from intercepted model traffic. See spec 0113.

## Examples

- Three sessions on two models run for a minute: the meter shows one column
  per second, each stacked in two colors, with a legend naming both models
  and their totals over the visible window, and a stated peak.
- A session's harness reports usage without naming a model, and the session
  has reported a model change earlier: the sample is attributed to that
  model, not to `unattributed`.
- A shell session runs continuously: it contributes no columns at all, and
  the meter does not estimate any for it.
- The fleet goes quiet after a large burst: the ceiling descends over the
  following seconds rather than dropping to the floor the instant the burst
  scrolls off, so the columns in between stay comparable.
