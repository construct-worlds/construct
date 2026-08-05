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
autoscales: the ceiling is the tallest column currently visible.

Magnitude is carried by the legend, which quantifies each series as a
**throughput**: tokens produced per second of *compute*, over a short
sliding window ending now.

Both halves of that are deliberate.

The denominator is the time sessions on that model actually spent working —
the span between a request going out and its response landing — not elapsed
wall-clock. Dividing by wall-clock answers "how much of the last minute did
this model spend generating", which drops as a fleet idles even though
nothing about the model changed. Dividing by compute answers "how fast is
this model when it runs", which is a property of the model and stays
comparable across a busy fleet and a quiet one.

That denominator is turn time, and turn time includes tool execution inside
the turn. It is not pure generation latency; no harness reports that per
call, and synthesizing it would be a guess.

The window is short and sliding, not the visible graph. A rate averaged over
everything on screen changes when the panel is resized and dilutes a burst
as history grows around it. A sliding window must also be finer-grained than
the columns: one that reset when a column did would flick every rate to zero
at each column boundary.

Three states must stay distinguishable, because collapsing any two of them
makes a false claim: a model that did not compute at all has no throughput
to report; one that computed and produced nothing is at zero; one that
produced less than a token per second is neither of those. A fleet where
nothing computed likewise has no rate.

The aggregate figure is the **sum** of the per-model rates, not the pooled
ratio of total tokens to total compute. Pooling yields a weighted average,
which can never exceed the fastest single model however many run at once —
blind to exactly the parallelism a fleet exists to provide, and arithmetically
inconsistent with the per-model figures displayed beside it. Any collapsed
legend row standing in for several models must sum them for the same reason:
whatever a client shows as a total has to be one.

The bars themselves carry shape and mix; they are comparable within a frame,
and the legend is what makes them comparable to anything else.

The ceiling falls back to a floor so a single small sample on an idle fleet
cannot paint a full-height column and read as saturation. After a burst
scrolls out of view the ceiling descends gradually rather than snapping, so
consecutive frames stay comparable — but it may never fall below a column
still on screen. That descent is tuned in wall-clock, so a change to the
column width has to carry it along or the ceiling falls faster for free.

No percentage is shown, and no fixed ceiling is assumed. A percentage here
would require inventing a denominator.

### Cached input is a subset, and it tones a band rather than growing it

A usage report's prompt side already contains whatever the provider served
from its prompt cache; the cached figure names part of that side, it does not
sit beside it. A sample's volume is therefore the prompt side plus the output
side, and adding the cached figure on top would count the cheapest tokens in
the report twice and inflate every total on the panel.

The cached figure is still worth showing, because most of a turn's prompt is
context the provider has already seen: a column that is mostly cache-read is a
much smaller amount of real work than the same column of fresh input. So each
model's band splits in two — the cache-served part at the base, the part
processed fresh directly above it — and the model stays one contiguous run.
Height keeps meaning billed volume, and the split answers how much of that
volume was new work.

A model owns exactly one **hue**, and both parts are solid tones of it: the
cache-served part is darker and the fresh part is full-strength. The cached
tone is authored in perceptual color space by lowering lightness while
retaining hue and most chroma. It is not a generic terminal `dim` attribute or
an alpha blend toward the panel background; either can wash a cyan into gray
teal or a peach into brown, making one model look like two.

The legend keeps its original single dot for each model. The dot uses the
darker cached tone while its model name and rate use the brighter fresh tone,
so both still read as one hue family without changing the legend's shape. On
reduced terminal palettes each pair is contrast-repaired as a pair, rather
than allowed to quantize onto one indistinguishable entry.

Ordering the cache-served part at the base remains a reading requirement: the
foundation of re-sent context sits under the work actually done on it. Since
the parts now have distinct solid tones, a boundary cell can draw the lower
tone as a partial foreground block over the upper tone as background without
changing either part's meaning.

A part with no volume draws no band at all, rather than a hairline that
rounds up to a visible slice. A harness that reports more cached than prompt
tokens is clamped to the subset contract instead of underflowing.

The rates in the legend are unaffected: they quantify billed volume over
compute time, and netting cache reads out of them would report a throughput
no bill and no provider agrees with. Exact per-model cached figures belong in
the hover detail, which is where a darker band gets a number.

### The hover detail restates the column as bars

Hovering a column details what that span consumed: the span and the column's
total, then one row per model that contributed to it.

Those rows are **bars, not a sentence of numbers**. A column is a stack, and
the question asked of it — what was this mostly, and how much of that was
real work — is a question about proportion. Comma-separated figures leave the
division to be done by eye, and past two models a single line of them reads
as a run of text with no shape at all. Each row therefore draws that model's
share of the column as a bar, and keeps its exact figures beside it: the bar
answers the proportion, the figures answer the amount, and neither has to
stand in for the other.

The bars carry the column's own tones, laid along a row instead of up one: one
hue per model, the cache-served part darker at the bar's start and the fresh
part full-strength after it. A hover that recolored or reordered what it
details would be describing a different graph than the one under the pointer.

They do **not** inherit the column's sub-cell resolution. A column is a few
cells tall and buys precision by topping out on a partial block glyph; a bar
has room for twenty cells and does not need to. Ending on a glyph would cost
something a bar cannot afford instead: cells a band fills outright are painted
as background, and a foreground block set beside them is drawn only where the
font puts ink — a font that draws it short of the line box notches the bar's
corner, so the rectangle reads as damaged rather than as precise. A bar
therefore spends whole cells only, and its parts are apportioned by
largest-remainder so they still sum to exactly its length.

Rows follow series order — the order the column stacks them and the legend
names them — not size. Reordering by size would make the detail's rows and the
band they describe disagree about position, which is the same reading failure
that fixes the stack's order in the first place.

Shares are drawn against the hovered column's own total, so the bars are read
against each other and a model that did any work at all keeps a visible bar
however small its share; the figures beside it, not the bar, carry how the
column compares to its neighbours. A pane too narrow for a bar whose cells
can still separate one share from another shows the figures alone rather than
a stub bar that misstates every share.

The figures name the cache-served share in words. A glyph standing in for
"cached" has to be learned before the row can be read, and one borrowed from
another vocabulary — a refresh arrow, say — actively misleads; the box can
simply be as wide as the words need.

Any meter drawn from these buckets details itself this way, whether it is the
fleet-wide meter or one scoped to a subset of sessions (spec 0191). Two
graphs of the same data with two different hover vocabularies would make the
scoping look like a difference in kind.

### Buckets are arrival time

A sample lands in the bucket in which its report *arrived*, not spread over
the interval in which the tokens were actually produced. A long streaming
call reports once, at the end, so its entire payload lands in one bucket and
the graph is bursty by nature. Smoothing by smearing a sample backwards
would look better and would be a fabrication — the no-estimating rule of
spec 0103 applies here too.

### The stack must survive the cell grid

A terminal cell holds one glyph, so a band boundary that lands inside a
cell cannot be drawn as two glyphs — the second overwrites the first and the
lower band disappears from that column. A boundary inside a cell is drawn
as a partial block whose filled part is the lower band and whose
background is the upper one.

The partially-filled cell at the top of a column still cannot encode a second
boundary: its empty part must remain panel background, so there is nowhere to
put an upper tone. That cell goes to whichever band owns more of its filled
height. Every full cell can carry one boundary, including a model's own
cached/fresh split, because both parts are solid tones.

The legend must name every series it drew, wrapping onto further rows rather
than showing only what fits on one — a colored bar whose model is named
nowhere cannot be read. When the rows the legend is allowed to take still
can't hold them all, the remainder is counted, not silently dropped.

Those rows are a grid of equal-width cells, not a flow. Packing entries
end-to-end fits marginally more on a row, but every row then begins its names
at a different offset, and past a handful of models the dots and names
scatter into a block of text with no line to read down. A cell wide enough
for the widest entry puts each column's dot and name at the same offset on
every row, so the legend is read vertically — the way a list of models is
read — and a name is found by position rather than by scanning. The trailing
space this costs inside narrower cells is worth that; a cell never spills
into its neighbour, because one long name shifting the column below it undoes
the alignment the grid exists for.

Each cell carries a margin past its widest entry, so adjacent columns are
separated by blank space rather than by a single column. A rate ends one cell
and a dot opens the next; with nothing between them the row reads as one run
of text and the grid stops looking like columns at all.

Within a cell the name starts it and the rate ends it, against the cell's
right edge. A rate that trails its name begins wherever that name happens to
stop, so the figures sit at a different offset on every row and comparing
them means finding each one first. Anchored to the edge they form a column of
their own, which is what a column of numbers is for. The slack lands between
name and rate, where it separates two things that are read differently rather
than misaligning either.

A rate that is a reading carries the series' full weight; `idle` is the
absence of one and is dimmed, so a glance finds the models actually working.
The dimming changes weight only — the whole legend row, `idle` included,
stays on the model's own color, because that color is what ties the row to
its band in the graph. A rate word shifted toward a neutral gray would stop
pointing at the band the row exists to name.

### Series identity and stacking order are stable

A model's color is assigned when it is first seen and never reassigned, and
every column stacks its series in that same order. Rank order changes
constantly on a live fleet; a color or a layer position that followed rank
would repaint history under the user, so a column drawn a minute ago would
no longer mean what it meant when drawn. A stacked graph is read by
following a band across columns, which is impossible if the bands swap
places whenever the leader changes. Models beyond the palette share one
"other" color and are reported as a single collapsed legend row rather than
reusing a color that already identifies another model.

### History outlives any one client

A client that remembered only the samples it saw itself would come back from
a restart showing a hole exactly where the fleet kept working — the sessions
keep burning tokens while nobody is attached. The history therefore belongs
to the daemon, which observes every session's usage report whether or not a
client is connected, and a starting client seeds its view from that window.

Nothing new is persisted for it. The samples are recovered at startup from
the same usage reports already written to each session's transcript, walked
in the same pass that self-heals the token tallies (spec 0103), so the
history survives a daemon restart at no additional storage cost and with no
second copy to keep consistent.

Recovered samples are attributed to the model that was in effect **at that
point in the transcript**, not to the session's current model. A session
that switched models must not have its earlier work credited to its later
model.

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
- Every series a client draws must be named somewhere on screen, with the
  throughput it represents. A colored band with no name is unreadable, so a
  legend that cannot fit on one row wraps rather than showing only what fits;
  what still cannot fit is counted, not silently dropped. Wrapping is into
  aligned columns, so the cost of naming many models is trailing space rather
  than a wall of ragged text.
- Naming series takes priority over graph height, down to a floor that still
  reads as a graph. A legend capped well above that floor strands series
  behind a "+N" while rows sit unused — the cap belongs at the floor, not at
  some fraction of the panel.
- The number of distinguishable series colors is the real limit on how many
  models a legend can name, and it is the only reason to collapse any of
  them. Listing more names than there are colors produces rows that cannot be
  matched to a band, which is worse than an honest collapsed row.
- Cached/fresh tones are an authored pair, not two independently chosen
  colors. Their perceptual hue must stay aligned, the legend dot must use the
  darker member while its text uses the brighter member, and reduced-color
  clients must keep the pair distinguishable after quantization.
- The span a column covers is a tuning choice, not a fixed part of the
  design. Anything stating an absolute count for one column must name that
  span, so the count is never mistaken for a rate.
- Because the bars carry no stated ceiling, a client must not invite
  cross-time comparison of bar heights alone; the legend's rates are the
  figure that survives the scale moving.
- The hover detail's bars and the column's bands share tones and order: a
  change to how a band is colored or stacked has to move both, or the detail
  starts describing a graph the user isn't looking at. Resolution is where
  they legitimately differ — the column needs eighths of a cell, the bar needs
  a square edge — and neither should be pushed onto the other.
- Any surface that fills cells outright as background must not put a partial
  block glyph beside them where an edge shows. The two are only
  interchangeable while the glyph's neighbours are empty; against filled cells
  the font's leading becomes a visible notch (#1183).
- The hover detail is the meter's exact-figures surface: it may add shape but
  may not drop the per-model volume or the cached subset in favor of it. A
  proportion is not an amount.
- Compute time must be attributed only to model-backed sessions. A session
  running a shell command spends most of its life busy with no model behind
  it; counting that would divide real token output by unrelated seconds and
  understate every rate on the fleet.
- Compute time should be derived by differencing an authoritative
  accumulator rather than by sampling a state flag, so a span that begins
  and ends between two samples still counts.
- Attribution is a client concern. A session's own lifetime tally stays
  model-agnostic; per-model accounting is derived at the display, not
  accumulated per session.
- The meter is fed continuously whether or not it is on screen, so
  selecting it shows real history rather than starting blank. It is a
  fixed-size ring, not a log.
- A sub-quantum share within one column may be omitted from that column's
  stack; the column's height and the legend totals still account for it.
  Promoting it to a visible slice would take that space from a series that
  earned it.
- A sample's cached figure is a subset of its prompt side and must never be
  added to a total. Any surface that carries it — the wire record, the
  daemon's window, a client's buckets — has to carry it alongside the volume
  rather than folded into it, or the split can only be recovered by guessing.
  It is additive on the wire: records written before it existed load as zero
  cached, which is indistinguishable from a provider that cached nothing.

## Non-Goals

- Cost in dollars. The usage reports carry a dollar figure, but this
  display is about volume; a currency readout is a separate decision.
- Per-session breakdown. The meter is a fleet aggregate grouped by model;
  which sessions contributed is answered by the session list.
- Reconciling the history against provider-side billing. This is an ambient
  live view, not an accounting record.
- Deriving throughput from intercepted model traffic. See spec 0113.

## Examples

- Three sessions on two models run for a minute: the meter shows a column
  per interval, each stacked in two model hues, with a legend naming both models
  and the throughput each achieved while computing.
- A model produced 60k tokens during 20 seconds of work and then sat idle
  for the rest of the minute: it reads as 3k/s, not 1k/s. The bar shows the
  60k; the rate shows how fast they arrived.
- A model has bars on screen but did nothing in the last minute: it is
  listed as idle, not as 0/s.
- Two models run concurrently at 2k/s and 1k/s: the fleet reads 3k/s — what
  it is actually producing — not the 1.5k/s that pooling their tokens and
  compute would give.
- A session's harness reports usage without naming a model, and the session
  has reported a model change earlier: the sample is attributed to that
  model, not to `unattributed`.
- A shell session runs continuously: it contributes no columns at all, and
  the meter does not estimate any for it.
- The fleet goes quiet after a large burst: the ceiling descends over the
  following columns rather than dropping to the floor the instant the burst
  scrolls off, so the columns in between stay comparable.
- A user quits their client, work continues for twenty minutes, and they
  reopen it: the graph shows those twenty minutes, not a gap followed by a
  fresh start.
- A session ran on one model this morning and another this afternoon: after
  a daemon restart the morning's columns still credit the morning's model.
- A turn re-sends a 90k-token context and adds 2k of new input: the column
  grows by the whole prompt side, but nine tenths of that model's band is
  drawn in its darker cached tone, so a glance separates a long conversation
  being replayed from the same volume of genuinely new work.
- A harness reports no cache figure at all: its bands are drawn solid, entirely
  as new work, which is what "nothing was cached" looks like — the meter does
  not infer a cached share from the size of the prompt.
- A column mixing three models is hovered: the detail names the span and the
  column's total, then draws three bars in the same order and hues the column
  stacked them, each with its own token figure and, where the provider cached
  part of the prompt, that subset marked as such. Which model dominated the
  column is answered by the bars' lengths; how much it actually was, by the
  figures beside them.
- One model in that column was almost entirely cache-served: its bar is nearly
  all darker tone, so a long conversation being replayed is distinguishable
  from the same volume of new work at a glance rather than by comparing two
  numbers.
