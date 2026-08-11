//! Fleet-wide realtime token meter (spec 0167).
//!
//! Every `SessionEvent::Cost` the daemon broadcasts — from every session,
//! not just the selected one — lands here as one sample, binned into
//! fixed-width time buckets and grouped by the model that consumed it. The
//! matrix-rain panel renders the buckets as a scrolling history graph.
//!
//! Two properties are load-bearing and easy to break later:
//!
//! - **Buckets are arrival time, not generation time.** A streaming call
//!   that ran for 40s reports its usage once, at the end, so its whole
//!   payload lands in a single bucket. The graph is therefore bursty by
//!   nature. Smearing a sample backwards over the gap since the previous
//!   one would look smoother and would be a fabrication — spec 0103's
//!   no-estimating rule applies here too.
//! - **Colors are assigned in first-seen order and never reassigned.** Rank
//!   order changes constantly on a live fleet; if color followed rank, a
//!   column painted 30 seconds ago would change color under the user and
//!   history would misreport itself.
//! - **Cached tokens are a subset, never an addend.** A report's prompt side
//!   already contains what the provider served from its cache, so the cached
//!   figure splits a model's band in two rather than growing it. Adding it
//!   would inflate every total on the panel.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::style::Color;

/// Seconds of wall-clock one column covers. One bar per minute: a turn's
/// worth of activity lands in a single column, and a full-width panel holds
/// well over an hour of history, so the graph reads as a session-long trend
/// rather than a live oscilloscope.
pub const BUCKET_SECS: u64 = 60;

/// Width of one history bucket.
pub const BUCKET: Duration = Duration::from_secs(BUCKET_SECS);

/// Hard cap on retained history, independent of panel width — a very wide
/// terminal still can't grow this unboundedly.
const MAX_BUCKETS: usize = 600;

/// Per-bucket decay applied to the autoscale ceiling — a half-life of about
/// three columns. It only governs the ceiling after a burst leaves the
/// visible window, so it is tuned in columns rather than wall-clock: the
/// scale settles over the next few bars instead of snapping the moment the
/// burst scrolls off.
const PEAK_DECAY: f64 = 0.8;

/// Floor for the autoscale ceiling, expressed as a rate so it means the same
/// thing at any column width. Without it, a single 40-token sample on an
/// otherwise idle fleet would paint a full-height column and read as
/// saturation.
const MIN_SCALE_PER_SEC: u64 = 1_000;
const MIN_SCALE: u64 = MIN_SCALE_PER_SEC * BUCKET_SECS;

/// Distinct midtone series colors, assigned in first-seen order. Same values
/// as the context-breakdown palette so a model's color reads as "a series
/// color" in the same visual language; kept as its own const because the two
/// lists are free to diverge.
const SERIES_PALETTE: [Color; 10] = [
    Color::Rgb(250, 179, 135), // orange
    Color::Rgb(137, 180, 250), // blue
    Color::Rgb(249, 226, 175), // yellow
    Color::Rgb(203, 166, 247), // mauve
    Color::Rgb(148, 226, 213), // teal
    Color::Rgb(245, 194, 231), // pink
    Color::Rgb(166, 227, 161), // green
    Color::Rgb(243, 139, 168), // red
    Color::Rgb(137, 220, 235), // sky
    Color::Rgb(180, 190, 254), // lavender
];

/// Darker companions for [`SERIES_PALETTE`], used for cache-served input.
///
/// These are not alpha blends toward the panel background: that would wash
/// chroma out and make a cached band look like a different, grayer model.
/// Each tone was derived in OKLCH by lowering perceptual lightness by 0.15
/// and retaining 90% of chroma, which keeps the hue within one degree of its
/// full-strength partner. Keeping the pairs explicit also makes changes to
/// the authored palette reviewable instead of hiding color work in every
/// rendered cell.
const CACHED_SERIES_PALETTE: [Color; 10] = [
    Color::Rgb(195, 134, 96),  // orange
    Color::Rgb(97, 133, 193),  // blue
    Color::Rgb(198, 178, 134), // yellow
    Color::Rgb(154, 122, 191), // mauve
    Color::Rgb(108, 176, 165), // teal
    Color::Rgb(193, 149, 181), // pink
    Color::Rgb(124, 177, 120), // green
    Color::Rgb(186, 98, 123),  // red
    Color::Rgb(98, 170, 183),  // sky
    Color::Rgb(135, 144, 198), // lavender
];

/// Every model past the palette's size shares this gray and is reported as
/// one collapsed "other" row, rather than reusing a color already spoken
/// for. The palette is the real limit on how many series a legend can name:
/// listing more names than there are distinguishable colors would produce
/// rows nobody could match to a band.
const OTHER_COLOR: Color = Color::Rgb(127, 132, 156);
const CACHED_OTHER_COLOR: Color = Color::Rgb(85, 89, 109);

/// Resolution of the sliding window the legend's rates are measured over.
/// One-second slots keep the rate a true sliding minute instead of resetting
/// at each column boundary, which would flick every rate to zero at the top
/// of every minute.
const RECENT_SLOT_SECS: u64 = 1;

/// Slots retained for the rate window — one minute of them.
const RECENT_SLOTS: usize = 60;

/// Label for a sample whose model could be established neither from the
/// report nor from the session's tracked model. Named rather than hidden so
/// the graph never silently drops volume it did measure.
pub const UNATTRIBUTED: &str = "unattributed";

/// Which part of a model's band a segment carries. A turn's prompt side
/// mostly re-sends context the provider already has; separating the two says
/// how much of a column was actually new work without hiding either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Part {
    /// Tokens the provider processed fresh: output, plus the prompt side it
    /// had not already cached.
    New,
    /// Prompt tokens served from the provider's cache.
    Cached,
}

/// One drawable band of a column: whose it is, and which part of that model's
/// usage it represents.
pub type Band = (u16, Part);

/// One model's share of one bucket. `cached` is a subset of `tokens`, mirroring
/// the usage report it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slice {
    model: u16,
    tokens: u64,
    cached: u64,
}

/// One time bucket: a sparse per-model slice. Most buckets hold zero or one
/// entry, so a dense per-model vector would be mostly padding.
#[derive(Debug, Default, Clone)]
pub struct Bucket {
    entries: Vec<Slice>,
}

/// One slot of the rate window: tokens produced, and how long any session on
/// that model was actually computing. The ratio of the two is the rate — a
/// model that produced 60k tokens during 20 seconds of work ran at 3k/s,
/// regardless of how much of the minute it spent idle.
#[derive(Debug, Default, Clone)]
struct RecentSlot {
    tokens: Vec<(u16, u64)>,
    busy_ms: Vec<(u16, u64)>,
}

fn add_sparse(entries: &mut Vec<(u16, u64)>, key: u16, amount: u64) {
    match entries.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = slot.1.saturating_add(amount),
        None => entries.push((key, amount)),
    }
}

impl Bucket {
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|s| s.tokens).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Per-model totals — `(model, tokens, cached)` — in series order.
    pub fn by_model(&self) -> Vec<(u16, u64, u64)> {
        let mut out: Vec<Slice> = self.entries.clone();
        out.sort_by_key(|s| s.model);
        out.iter()
            .map(|s| (s.model, s.tokens, s.cached.min(s.tokens)))
            .collect()
    }

    /// The column's bands, bottom-up, in series order — first-seen, which is
    /// also palette order — so every column stacks the same way.
    ///
    /// Ordering by size instead would reshuffle the bands whenever the
    /// leader changed, and a stacked graph whose layers swap places
    /// column-to-column can't be read as layers at all: the eye tracks a
    /// band's continuity, not its rank.
    ///
    /// Each model contributes up to two adjacent bands, so a model is still
    /// one contiguous run of its color. A part that is zero produces no band
    /// at all rather than a hairline that rounds up to a visible slice.
    ///
    /// Cache-served volume sits at the base of a model's run and new work on
    /// top of it — the foundation of re-sent context under the work actually
    /// done on it. That order is also what the renderer needs: the two parts
    /// share a color, so their boundary can only be carried by fill, and the
    /// one cell that cannot carry it is the partial cell topping the run.
    /// Putting new work there makes a solid block the correct answer.
    pub fn stacked(&self) -> Vec<(Band, u64)> {
        let mut slices: Vec<Slice> = self.entries.clone();
        slices.sort_by_key(|s| s.model);
        let mut out = Vec::with_capacity(slices.len() * 2);
        for slice in slices {
            let cached = slice.cached.min(slice.tokens);
            let fresh = slice.tokens - cached;
            if cached > 0 {
                out.push(((slice.model, Part::Cached), cached));
            }
            if fresh > 0 {
                out.push(((slice.model, Part::New), fresh));
            }
        }
        out
    }

    fn add(&mut self, model: u16, tokens: u64, cached: u64) {
        match self.entries.iter_mut().find(|s| s.model == model) {
            Some(slot) => {
                slot.tokens = slot.tokens.saturating_add(tokens);
                slot.cached = slot.cached.saturating_add(cached);
            }
            None => self.entries.push(Slice {
                model,
                tokens,
                cached,
            }),
        }
    }
}

/// One legend row: a model, its color, and what it consumed over the
/// currently visible history.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub label: String,
    pub tokens: u64,
    /// Full-strength tone used for fresh work, labels, and rates.
    pub color: Color,
    /// Darker companion tone used for the legend dot and cache reads.
    pub dot_color: Color,
    /// Tokens per second of compute over the last minute, or `None` when
    /// this model did no work in that window. Fractional so a formatter can
    /// tell "computed and produced nothing" from "produced less than a token
    /// a second" — integer division collapses those into the same 0.
    pub rate: Option<f64>,
}

#[derive(Debug)]
pub struct TokenMeter {
    /// Interned model labels in first-seen order; the index is the series id
    /// stored in buckets and the color slot.
    models: Vec<String>,
    /// Oldest first; the last element is the bucket currently filling.
    buckets: VecDeque<Bucket>,
    /// Start instant of the bucket currently filling.
    current_start: Instant,
    /// Decaying autoscale ceiling, in tokens per bucket.
    peak: f64,
    /// Sliding one-minute window behind the legend's rates, at a finer
    /// resolution than the columns so the figure doesn't reset when a column
    /// does.
    recent: VecDeque<RecentSlot>,
    /// Start instant of the recent slot currently filling.
    recent_start: Instant,
}

impl TokenMeter {
    pub fn new(now: Instant) -> Self {
        let mut buckets = VecDeque::with_capacity(MAX_BUCKETS);
        buckets.push_back(Bucket::default());
        Self {
            models: Vec::new(),
            buckets,
            current_start: now,
            peak: 0.0,
            recent: VecDeque::from([RecentSlot::default()]),
            recent_start: now,
        }
    }

    /// Seed the meter with samples the daemon retained (spec 0167), each
    /// given as its age in milliseconds at the moment the window was taken.
    ///
    /// A client that only remembered its own samples would come back from a
    /// restart showing a hole exactly where the fleet kept working, so the
    /// history is rebuilt rather than carried. Samples older than the meter
    /// retains are dropped; the rest land in the bucket their age selects,
    /// which is why they are passed as ages rather than wall-clock instants —
    /// the meter's own clock is monotonic and cannot be compared to one.
    pub fn seed(&mut self, samples: impl IntoIterator<Item = (i64, Option<String>, u64, u64)>) {
        for (age_ms, model, tokens, cached) in samples {
            if tokens == 0 || age_ms < 0 {
                continue;
            }
            let back = (age_ms as u128 / BUCKET.as_millis()) as usize;
            let len = self.buckets.len();
            // `back == 0` is the bucket currently filling; anything older
            // than the ring reaches is simply out of history.
            let Some(index) = len.checked_sub(back + 1) else {
                continue;
            };
            let idx = self.intern(model.as_deref().unwrap_or(UNATTRIBUTED));
            if let Some(bucket) = self.buckets.get_mut(index) {
                bucket.add(idx, tokens, cached);
                let total = bucket.total() as f64;
                if total > self.peak {
                    self.peak = total;
                }
            }
        }
    }

    /// Fill the ring with empty buckets covering `secs` of past time, so
    /// [`Self::seed`] has somewhere to place historical samples. A freshly
    /// constructed meter holds only the bucket it is filling.
    pub fn reserve_history(&mut self, secs: u64) {
        let want = ((secs / BUCKET_SECS) as usize + 1).min(MAX_BUCKETS);
        while self.buckets.len() < want {
            self.buckets.push_front(Bucket::default());
        }
    }

    /// Roll the history forward to `now`, appending empty buckets for the time
    /// that passed. Called by timer maintenance as well as rendering so
    /// idle time scrolls as a gap without requiring continuous paints.
    pub fn advance_to(&mut self, now: Instant) -> bool {
        self.advance_recent(now);
        let elapsed = now.saturating_duration_since(self.current_start);
        let steps = (elapsed.as_millis() / BUCKET.as_millis()) as usize;
        if steps == 0 {
            return false;
        }
        for _ in 0..steps.min(MAX_BUCKETS) {
            self.buckets.push_back(Bucket::default());
        }
        while self.buckets.len() > MAX_BUCKETS {
            self.buckets.pop_front();
        }
        self.current_start += BUCKET * steps as u32;
        // Decay once per elapsed bucket. This only governs how the ceiling
        // *falls* — `scale` re-floors against what is actually on screen, so
        // decay can never shrink the graph below a column it must draw.
        self.peak *= PEAK_DECAY.powi(steps.min(1_000) as i32);
        true
    }

    fn advance_recent(&mut self, now: Instant) {
        let slot = Duration::from_secs(RECENT_SLOT_SECS);
        let elapsed = now.saturating_duration_since(self.recent_start);
        let steps = (elapsed.as_millis() / slot.as_millis()) as usize;
        if steps == 0 {
            return;
        }
        for _ in 0..steps.min(RECENT_SLOTS) {
            self.recent.push_back(RecentSlot::default());
        }
        while self.recent.len() > RECENT_SLOTS {
            self.recent.pop_front();
        }
        self.recent_start += slot * steps as u32;
    }

    /// Record compute time: `delta_ms` of wall-clock during which a session
    /// on `model` was running a turn.
    ///
    /// This is the rate's denominator, and it is turn time — the span
    /// between a request going out and its response landing, including any
    /// tool work inside that turn. It is not pure generation latency; no
    /// harness reports that per call, and inventing it would be a guess.
    pub fn observe_busy(&mut self, model: Option<&str>, delta_ms: u64, now: Instant) {
        if delta_ms == 0 {
            return;
        }
        self.advance_recent(now);
        let idx = self.intern(model.unwrap_or(UNATTRIBUTED));
        if let Some(slot) = self.recent.back_mut() {
            add_sparse(&mut slot.busy_ms, idx, delta_ms);
        }
    }

    /// Tokens and compute-milliseconds for `model` over the sliding rate
    /// window.
    fn recent_totals(&self, model: u16) -> (u64, u64) {
        let mut tokens = 0u64;
        let mut busy = 0u64;
        for slot in &self.recent {
            for (m, t) in &slot.tokens {
                if *m == model {
                    tokens = tokens.saturating_add(*t);
                }
            }
            for (m, ms) in &slot.busy_ms {
                if *m == model {
                    busy = busy.saturating_add(*ms);
                }
            }
        }
        (tokens, busy)
    }

    /// Throughput for `model` over the last minute: tokens produced per
    /// second of compute, not per second of wall-clock.
    ///
    /// `None` when nothing on that model computed in the window — a model
    /// that did no work has no throughput, and reporting `0/s` for it would
    /// claim it was working slowly rather than not at all. A model that
    /// computed and produced nothing does report `0`.
    pub fn recent_rate(&self, model: u16) -> Option<f64> {
        let (tokens, busy_ms) = self.recent_totals(model);
        (busy_ms > 0).then(|| tokens as f64 * 1_000.0 / busy_ms as f64)
    }

    /// Fleet throughput over the rate window: the sum of the per-model
    /// rates.
    ///
    /// Deliberately a sum and not a pooled `total tokens / total compute`.
    /// Pooling produces a weighted *average* of the per-model rates, so it
    /// can never exceed the fastest single model however many run at once —
    /// which makes it blind to the parallelism that is the whole point of a
    /// fleet, and leaves a figure labeled `Σ` that the rates above it visibly
    /// don't add up to.
    ///
    /// The sum reads as "what the fleet produces per second with everything
    /// running at once". Models that did no work contribute nothing, and a
    /// fleet where none did has no rate at all rather than zero.
    pub fn recent_fleet_rate(&self) -> Option<f64> {
        let mut total = 0.0f64;
        let mut any = false;
        for idx in 0..self.models.len() {
            if let Some(rate) = self.recent_rate(idx as u16) {
                total += rate;
                any = true;
            }
        }
        any.then_some(total)
    }

    /// Whether the one-minute throughput window still contains data whose
    /// displayed rate can change as one-second slots age out.
    pub fn has_recent_activity(&self) -> bool {
        self.recent
            .iter()
            .any(|slot| !slot.tokens.is_empty() || !slot.busy_ms.is_empty())
    }

    /// Record one usage sample. `model` is the label to group under —
    /// callers resolve it from the report first, then the session's tracked
    /// model, and pass `None` only when neither is known. `cached` is the
    /// part of `tokens` the provider served from its prompt cache, and is a
    /// subset of it: the rate and the column height both stay the volume the
    /// call actually billed.
    pub fn observe(&mut self, model: Option<&str>, tokens: u64, cached: u64, now: Instant) {
        if tokens == 0 {
            return;
        }
        self.advance_to(now);
        let idx = self.intern(model.unwrap_or(UNATTRIBUTED));
        let bucket = self
            .buckets
            .back_mut()
            .expect("meter always holds a current bucket");
        bucket.add(idx, tokens, cached);
        let total = bucket.total() as f64;
        if total > self.peak {
            self.peak = total;
        }
        if let Some(slot) = self.recent.back_mut() {
            add_sparse(&mut slot.tokens, idx, tokens);
        }
    }

    fn intern(&mut self, model: &str) -> u16 {
        if let Some(idx) = self.models.iter().position(|m| m == model) {
            return idx as u16;
        }
        // u16::MAX distinct models in one session is not a real scenario;
        // clamping beats panicking if one ever shows up.
        if self.models.len() >= u16::MAX as usize {
            return (self.models.len() - 1) as u16;
        }
        self.models.push(model.to_string());
        (self.models.len() - 1) as u16
    }

    /// The most recent `width` buckets, oldest first. Shorter than `width`
    /// only before enough history has accumulated; callers right-align.
    pub fn window(&self, width: usize) -> impl Iterator<Item = &Bucket> {
        let skip = self.buckets.len().saturating_sub(width);
        self.buckets.iter().skip(skip)
    }

    /// The bucket under a column of a `width`-wide right-aligned render, if
    /// history reaches back that far.
    pub fn bucket_at(&self, column: usize, width: usize) -> Option<&Bucket> {
        let history = self.buckets.len().min(width);
        // A partially-filled meter draws its history flush to the right, so
        // the leading columns are empty rather than showing the oldest data
        // in the wrong place.
        let lead = width.saturating_sub(history);
        let index = column.checked_sub(lead)?;
        self.window(width).nth(index)
    }

    fn max_in_window(&self, width: usize) -> u64 {
        self.window(width).map(Bucket::total).max().unwrap_or(0)
    }

    /// Tokens-per-*bucket* value the top of the graph represents, for a
    /// `width`-wide render. Always at least the tallest visible column — the
    /// decaying peak only slows the ceiling's *descent* after a burst
    /// scrolls off, so the graph settles instead of snapping between scales.
    ///
    /// This is the number bar heights are computed against. For display use
    /// [`Self::scale_per_second`]: a bucket wider than a second makes the raw
    /// figure a per-bucket total, which reads as a rate and isn't one.
    pub fn scale(&self, width: usize) -> u64 {
        self.peak
            .max(self.max_in_window(width) as f64)
            .max(MIN_SCALE as f64)
            .ceil() as u64
    }

    /// Human label for the span one column covers, so a per-column count is
    /// never read as a rate.
    pub fn bucket_span_label() -> String {
        if BUCKET_SECS % 60 == 0 {
            format!("{}m", BUCKET_SECS / 60)
        } else {
            format!("{BUCKET_SECS}s")
        }
    }

    pub fn model_label(&self, idx: u16) -> &str {
        self.models
            .get(idx as usize)
            .map(String::as_str)
            .unwrap_or(UNATTRIBUTED)
    }

    /// Color for a series index. Everything past the palette shares the
    /// "other" gray.
    pub fn color(&self, idx: u16) -> Color {
        SERIES_PALETTE
            .get(idx as usize)
            .copied()
            .unwrap_or(OTHER_COLOR)
    }

    /// Cache-served companion tone for a series index. It keeps the series'
    /// hue while lowering perceptual lightness, so cached and fresh remain a
    /// recognizable pair without relying on a texture glyph.
    pub fn cached_color(&self, idx: u16) -> Color {
        CACHED_SERIES_PALETTE
            .get(idx as usize)
            .copied()
            .unwrap_or(CACHED_OTHER_COLOR)
    }

    /// Resolve one model/part band to the solid tone used to paint it.
    pub fn band_color(&self, band: Band) -> Color {
        match band.1 {
            Part::New => self.color(band.0),
            Part::Cached => self.cached_color(band.0),
        }
    }

    /// Legend rows for the visible window, busiest first. Models sharing the
    /// "other" color collapse into one row so the legend can't claim a color
    /// distinguishes them.
    pub fn legend(&self, width: usize) -> Vec<LegendEntry> {
        let mut totals: Vec<u64> = vec![0; self.models.len()];
        for bucket in self.window(width) {
            for slice in &bucket.entries {
                if let Some(slot) = totals.get_mut(slice.model as usize) {
                    *slot = slot.saturating_add(slice.tokens);
                }
            }
        }
        let mut named: Vec<LegendEntry> = totals
            .iter()
            .enumerate()
            .take(SERIES_PALETTE.len())
            .filter(|(_, tokens)| **tokens > 0)
            .map(|(idx, tokens)| LegendEntry {
                label: self.models[idx].clone(),
                tokens: *tokens,
                color: SERIES_PALETTE[idx],
                dot_color: CACHED_SERIES_PALETTE[idx],
                rate: self.recent_rate(idx as u16),
            })
            .collect();
        named.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.label.cmp(&b.label)));

        let other: u64 = totals
            .iter()
            .skip(SERIES_PALETTE.len())
            .fold(0u64, |acc, t| acc.saturating_add(*t));
        if other > 0 {
            let count = totals
                .iter()
                .skip(SERIES_PALETTE.len())
                .filter(|t| **t > 0)
                .count();
            // Summed, not pooled, for the same reason as the fleet figure:
            // this row stands in for several models, and the column has to
            // add up.
            let mut other_rate = 0.0f64;
            let mut other_any = false;
            for idx in SERIES_PALETTE.len()..totals.len() {
                if let Some(rate) = self.recent_rate(idx as u16) {
                    other_rate += rate;
                    other_any = true;
                }
            }
            named.push(LegendEntry {
                label: format!("other ({count})"),
                tokens: other,
                color: OTHER_COLOR,
                dot_color: CACHED_OTHER_COLOR,
                rate: other_any.then_some(other_rate),
            });
        }
        named
    }

    /// Total tokens across a visible window, used by accounting tests.
    #[cfg(test)]
    pub fn window_total(&self, width: usize) -> u64 {
        self.window(width)
            .fold(0u64, |acc, b| acc.saturating_add(b.total()))
    }

    /// True when nothing has ever been recorded, so the panel can say why
    /// it's empty instead of drawing a blank grid.
    pub fn is_idle(&self) -> bool {
        self.models.is_empty()
    }
}

/// Tone pairs that are drawn directly against each other and must survive
/// terminal palette quantization. The backend repairs a collapsed pair at
/// 256- and 16-color depths while preserving hue whenever that palette has a
/// second lightness available.
pub fn contrast_pairs() -> impl Iterator<Item = (Color, Color)> {
    CACHED_SERIES_PALETTE
        .into_iter()
        .zip(SERIES_PALETTE)
        .chain(std::iter::once((CACHED_OTHER_COLOR, OTHER_COLOR)))
}

// ── Column paint (shared by fleet meter + project dashboard meter) ─────────
//
// Cells filled outright use background color rather than a `█` glyph so the
// bar is solid on fonts whose FULL BLOCK is shorter than the terminal line
// box (see #1183). Partial cells keep eighth-block glyphs.

/// Partial-block glyphs for a cell filled 0..7 eighths. Index 0 is empty
/// (caller should not paint that cell); full cells use background fill.
pub const METER_PARTIALS: [&str; 8] = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];

/// How a band identifies its solid paint. Cached and fresh parts of one model
/// use distinct lightnesses of the same hue, so their boundary can use the
/// terminal's ordinary foreground/background split without a texture glyph.
pub trait BandPaint: Copy + PartialEq {
    /// Stable key for the exact tone that fills this band.
    fn paint(self) -> u32;
}

impl BandPaint for Band {
    fn paint(self) -> u32 {
        u32::from(self.0) * 2
            + match self.1 {
                Part::Cached => 0,
                Part::New => 1,
            }
    }
}

/// Plain series indices, for callers that draw one band per model.
impl BandPaint for u16 {
    fn paint(self) -> u32 {
        u32::from(self)
    }
}

/// One painted cell of a stacked column: which glyph, in whose color, over
/// whose color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnCell<K> {
    /// Rows up from the bottom of the graph.
    pub row: u16,
    pub glyph: &'static str,
    /// Band drawn as the glyph's filled part.
    pub fg: K,
    /// Band filling the rest of the cell, when a boundary lands inside it.
    pub bg: Option<K>,
}

/// Lay a column's stacked segments onto terminal cells.
///
/// Cells are 8 eighths tall. A cell wholly owned by one band is painted as
/// that band's background (not a full-block glyph); the column's topmost cell
/// is a partial block; a cell where one band ends and another begins is a
/// partial block of the lower tone drawn *over* the upper tone as background,
/// which is the only way to get two colors into one cell.
///
/// Only the partially-filled cell topping a column cannot encode a second
/// boundary: its empty part has to remain panel background. Whichever band
/// owns more of that cell takes it.
pub fn column_cells<K: BandPaint>(
    segments: &[(K, usize)],
    filled: usize,
    cells: usize,
) -> Vec<ColumnCell<K>> {
    let mut out = Vec::new();
    if segments.is_empty() || filled == 0 {
        return out;
    }
    // Which band owns each eighth, bottom-up.
    let mut owner: Vec<K> = Vec::with_capacity(filled);
    for (band, eighths) in segments {
        for _ in 0..*eighths {
            owner.push(*band);
        }
    }
    for row in 0..cells {
        let base = row * 8;
        if base >= filled.min(owner.len()) {
            break;
        }
        let top = ((row + 1) * 8).min(filled).min(owner.len());
        let fill = top - base;
        let bottom_series = owner[base];
        // How far the bottom band reaches within this cell.
        let bottom_run = owner[base..top]
            .iter()
            .take_while(|m| **m == bottom_series)
            .count();
        let upper = owner[top - 1];
        let cell = if bottom_run == fill {
            // One band owns everything filled here.
            band_cell(row, bottom_series, fill)
        } else if fill == 8 && bottom_series.paint() != upper.paint() {
            // A boundary between two colors inside a full cell: lower band as
            // the glyph's filled part, whatever tops the cell as its
            // background.
            ColumnCell {
                row: row as u16,
                glyph: METER_PARTIALS[bottom_run],
                fg: bottom_series,
                bg: Some(upper),
            }
        } else {
            // Either a partially-filled top cell, or a boundary between two
            // bands with the exact same tone. Neither can hold both, so the
            // larger share takes the cell.
            let upper_run = fill - bottom_run;
            let winner = if upper_run > bottom_run {
                upper
            } else {
                bottom_series
            };
            band_cell(row, winner, fill)
        };
        out.push(cell);
    }
    out
}

/// A cell filled by a single band, `fill` eighths deep.
///
/// A cell the band fills outright is painted as a *background* rather than a
/// `█` glyph. A foreground glyph only colors the pixels the font draws, and
/// plenty of fonts draw FULL BLOCK shorter than the terminal's line box —
/// the leading is left transparent, so every row boundary shows a hairline
/// of panel background and a solid bar reads as a stack of bricks. A
/// background color fills the whole cell whatever the font does, so the bar
/// is solid on every terminal. Nothing else can be painted into a cell that
/// one band fills, so spending its background on that band costs nothing.
///
/// A partially-filled cell still needs a glyph — its empty part has to stay
/// panel background — and keeps the eighth block. The same short-glyph
/// leading applies there, but it falls at the top of a column against
/// background that is empty anyway.
pub fn band_cell<K: BandPaint>(row: usize, band: K, fill: usize) -> ColumnCell<K> {
    if fill == 8 {
        return ColumnCell {
            row: row as u16,
            glyph: " ",
            fg: band,
            bg: Some(band),
        };
    }
    ColumnCell {
        row: row as u16,
        glyph: METER_PARTIALS[fill],
        fg: band,
        bg: None,
    }
}

/// Split a column's `filled` eighths across its bands in proportion to
/// their tokens, largest-remainder so the parts sum to exactly `filled` and
/// no visible band rounds away to nothing.
pub fn stacked_eighths<K: Copy>(stacked: &[(K, u64)], total: u64, filled: usize) -> Vec<(K, usize)> {
    split_units(stacked, total, filled)
}

/// Hand out `units` of drawing space to bands in proportion to their tokens,
/// largest-remainder so the parts sum to exactly `units` and no band that has
/// volume rounds away to nothing.
///
/// A column's unit is an eighth of a cell (see [`stacked_eighths`]); a
/// horizontal bar's unit is a whole cell, because a bar drawn along a row has
/// to end on a cell boundary to keep a square edge — see the hover detail in
/// the TUI renderer.
pub fn split_units<K: Copy>(parts: &[(K, u64)], total: u64, units: usize) -> Vec<(K, usize)> {
    if total == 0 || units == 0 {
        return Vec::new();
    }
    let stacked = parts;
    let filled = units;
    let mut out: Vec<(K, usize)> = Vec::with_capacity(stacked.len());
    let mut remainders: Vec<(usize, f64)> = Vec::with_capacity(stacked.len());
    let mut assigned = 0usize;
    for (i, (band, tokens)) in stacked.iter().enumerate() {
        let exact = (*tokens as f64 / total as f64) * filled as f64;
        let floor = exact.floor() as usize;
        out.push((*band, floor));
        remainders.push((i, exact - floor as f64));
        assigned += floor;
    }
    remainders.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut leftover = filled.saturating_sub(assigned);
    for (i, _) in remainders {
        if leftover == 0 {
            break;
        }
        out[i].1 += 1;
        leftover -= 1;
    }
    out.retain(|(_, e)| *e > 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` buckets after `base`, so the tests stay correct if the bucket
    /// width is retuned again.
    fn buckets_after(base: Instant, n: u64) -> Instant {
        base + BUCKET * n as u32
    }

    #[test]
    fn advance_reports_only_visible_bucket_rollovers() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        assert!(!m.advance_to(t0 + Duration::from_secs(1)));
        assert!(m.advance_to(buckets_after(t0, 1)));
        assert!(!m.advance_to(buckets_after(t0, 1) + Duration::from_secs(1)));
    }

    #[test]
    fn samples_land_in_arrival_buckets() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, 0, t0);
        m.observe(Some("opus"), 50, 0, buckets_after(t0, 2));
        m.advance_to(buckets_after(t0, 2));
        let cols: Vec<u64> = m.window(3).map(Bucket::total).collect();
        assert_eq!(cols, vec![100, 0, 50], "one empty bucket between samples");
    }

    #[test]
    fn distinct_models_stack_within_one_bucket() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, 0, t0);
        m.observe(Some("codex"), 300, 0, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(bucket.total(), 400);
        assert_eq!(bucket.stacked().len(), 2);
    }

    /// A model's cache-served share splits its band instead of growing it:
    /// the column is the same height it would be without the split, and the
    /// two parts sit next to each other so the model stays one run.
    #[test]
    fn cached_tokens_split_a_model_band_without_changing_its_height() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 1_000, 700, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(bucket.total(), 1_000, "cached must not be added on top");
        assert_eq!(
            bucket.stacked(),
            vec![((0, Part::Cached), 700), ((0, Part::New), 300)],
            "cache-served at the base, new work directly above it"
        );
        assert_eq!(bucket.by_model(), vec![(0, 1_000, 700)]);
    }

    /// Each model splits within its own run, so two models never interleave
    /// their new and cached parts up the column.
    #[test]
    fn each_model_keeps_its_two_parts_adjacent() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, 40, t0);
        m.observe(Some("codex"), 100, 60, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(
            bucket.stacked(),
            vec![
                ((0, Part::Cached), 40),
                ((0, Part::New), 60),
                ((1, Part::Cached), 60),
                ((1, Part::New), 40),
            ]
        );
    }

    /// A part with no volume draws no band at all — a zero-width slice would
    /// round up to a visible eighth and claim work that never happened.
    #[test]
    fn an_empty_part_produces_no_band() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, 0, t0);
        m.observe(Some("codex"), 100, 100, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(
            bucket.stacked(),
            vec![((0, Part::New), 100), ((1, Part::Cached), 100)]
        );
    }

    /// A harness that reports more cached than prompt tokens would otherwise
    /// underflow the new-work subtraction. The subset contract wins.
    #[test]
    fn cached_larger_than_the_sample_is_clamped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, 900, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(bucket.total(), 100);
        assert_eq!(bucket.stacked(), vec![((0, Part::Cached), 100)]);
        assert_eq!(bucket.by_model(), vec![(0, 100, 100)]);
    }

    /// Seeded history carries its cached share too, so a restarted client
    /// draws the same two-tone columns it had before.
    #[test]
    fn seeded_samples_keep_their_cached_share() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.reserve_history(BUCKET_SECS * 3);
        m.seed([(0, Some("opus".to_string()), 1_000, 250)]);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(bucket.by_model(), vec![(0, 1_000, 250)]);
    }

    /// Every column stacks its series in the same order, whatever the
    /// per-column ranking — bands that swap places column-to-column can't be
    /// followed across the graph.
    #[test]
    fn stack_order_is_stable_across_columns() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        // Column 1: "opus" dominates. Column 2: "codex" does.
        m.observe(Some("opus"), 900, 0, t0);
        m.observe(Some("codex"), 100, 0, t0);
        let first: Vec<u16> = m
            .window(1)
            .next()
            .expect("bucket")
            .stacked()
            .iter()
            .map(|((model, _), _)| *model)
            .collect();
        m.advance_to(buckets_after(t0, 1));
        m.observe(Some("opus"), 100, 0, buckets_after(t0, 1));
        m.observe(Some("codex"), 900, 0, buckets_after(t0, 1));
        let second: Vec<u16> = m
            .window(1)
            .next()
            .expect("bucket")
            .stacked()
            .iter()
            .map(|((model, _), _)| *model)
            .collect();
        assert_eq!(first, second, "layer order must not follow rank");
        assert_eq!(m.model_label(first[0]), "opus", "first seen sits at the base");
    }

    #[test]
    fn colors_follow_first_seen_order_not_rank() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("first"), 10, 0, t0);
        m.observe(Some("second"), 10_000, 0, t0);
        // "second" dominates every ranking, but "first" keeps palette slot 0.
        assert_eq!(m.color(0), SERIES_PALETTE[0]);
        assert_eq!(m.color(1), SERIES_PALETTE[1]);
        assert_eq!(m.model_label(0), "first");
    }

    /// The legend keeps the original one-dot-per-model treatment, using the
    /// darker member for the dot and the brighter member for its text.
    #[test]
    fn legend_exposes_dark_dot_and_bright_text_tones() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 1_000, 700, t0);
        let entry = &m.legend(1)[0];
        assert_eq!(entry.color, m.band_color((0, Part::New)));
        assert_eq!(entry.dot_color, m.band_color((0, Part::Cached)));
        assert_ne!(entry.color, entry.dot_color);
    }

    #[test]
    fn scale_holds_a_floor_then_tracks_the_peak() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        assert_eq!(m.scale(80), MIN_SCALE, "idle meter uses the floor");
        m.observe(Some("opus"), 40, 0, t0);
        assert_eq!(m.scale(80), MIN_SCALE, "a tiny sample must not saturate");
        m.observe(Some("opus"), 500_000, 0, t0);
        assert!(m.scale(80) >= 500_000);
    }

    #[test]
    fn a_visible_column_always_fits_the_scale() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 1_000_000, 0, t0);
        m.advance_to(buckets_after(t0, 10));
        // The spike is 10 buckets back: still on screen at width 80, so the
        // ceiling must still contain it however far the peak has decayed.
        assert!(m.scale(80) >= 1_000_000, "{}", m.scale(80));
    }

    #[test]
    fn ceiling_descends_gradually_once_a_burst_scrolls_off() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 1_000_000, 0, t0);
        m.advance_to(buckets_after(t0, 3));
        // Narrow render: the spike has scrolled out of view, so only the
        // decayed peak holds the ceiling up.
        let narrow = m.scale(2);
        assert!(narrow < 1_000_000, "ceiling must fall: {narrow}");
        assert!(
            narrow > MIN_SCALE,
            "…but not snap straight to the floor: {narrow}"
        );
        m.advance_to(buckets_after(t0, 300));
        assert_eq!(m.scale(2), MIN_SCALE, "eventually returns to the floor");
    }

    /// The rate is tokens per second of *compute*, not of wall-clock: a
    /// model that produced 60k tokens during 20s of work ran at 3k/s even
    /// though it sat idle for the rest of the minute.
    #[test]
    fn rate_divides_by_compute_time_not_wall_clock() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 60_000, 0, t0);
        m.observe_busy(Some("opus"), 20_000, t0);
        assert_eq!(m.recent_rate(0), Some(3_000.0));
    }

    /// A model that did no work has no throughput. Reporting `0/s` would
    /// claim it was working slowly rather than not working.
    #[test]
    fn a_model_with_no_compute_time_has_no_rate() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 5_000, 0, t0);
        assert_eq!(m.recent_rate(0), None, "tokens but no measured compute");
    }

    /// The rate window slides by the second rather than resetting with the
    /// columns — otherwise every rate would flick to zero at the top of each
    /// minute, when a new column starts empty.
    #[test]
    fn rate_window_slides_across_a_column_boundary() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 30_000, 0, t0);
        m.observe_busy(Some("opus"), 10_000, t0);
        // Well into the next column, but still inside the rate window.
        let later = t0 + Duration::from_secs(BUCKET_SECS + 5);
        m.advance_to(later);
        assert_eq!(m.recent_rate(0), None, "…and out of it once a minute passes");

        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 30_000, 0, t0);
        m.observe_busy(Some("opus"), 10_000, t0);
        m.advance_to(t0 + Duration::from_secs(30));
        assert_eq!(m.recent_rate(0), Some(3_000.0), "still inside the window");
    }

    /// The fleet figure is the sum of the rates shown above it, so the
    /// legend column adds up. Pooling the tokens and compute instead would
    /// average them — 1.5k/s here — which no amount of added parallelism
    /// could ever push past the fastest single model.
    #[test]
    fn fleet_rate_sums_the_per_model_rates() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("a"), 20_000, 0, t0);
        m.observe_busy(Some("a"), 10_000, t0);
        m.observe(Some("b"), 10_000, 0, t0);
        m.observe_busy(Some("b"), 10_000, t0);
        assert_eq!(m.recent_rate(0), Some(2_000.0));
        assert_eq!(m.recent_rate(1), Some(1_000.0));
        assert_eq!(m.recent_fleet_rate(), Some(3_000.0));
    }

    /// A fleet where nothing computed has no throughput to state, the same
    /// way an individual model doesn't.
    #[test]
    fn fleet_rate_is_absent_when_nothing_computed() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("a"), 20_000, 0, t0);
        assert_eq!(m.recent_fleet_rate(), None);
    }

    #[test]
    fn unattributed_samples_are_counted_not_dropped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(None, 500, 0, t0);
        assert_eq!(m.window_total(8), 500);
        assert_eq!(m.legend(8)[0].label, UNATTRIBUTED);
    }

    /// Seeded samples land in the column their age selects, so a restarted
    /// client sees the same shape it would have drawn live.
    #[test]
    fn seeded_samples_land_in_their_age_bucket() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.reserve_history(BUCKET_SECS * 5);
        let bucket_ms = (BUCKET_SECS * 1_000) as i64;
        m.seed([
            (0, Some("opus".to_string()), 10, 0),
            (bucket_ms * 2, Some("opus".to_string()), 20, 0),
        ]);
        let cols: Vec<u64> = m.window(6).map(Bucket::total).collect();
        assert_eq!(cols, vec![0, 0, 0, 20, 0, 10], "newest column is last");
    }

    /// History the ring can't reach is dropped rather than piling onto the
    /// oldest column, which would invent a spike that never happened.
    #[test]
    fn seeds_older_than_the_ring_are_dropped_not_clamped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.reserve_history(BUCKET_SECS * 2);
        let bucket_ms = (BUCKET_SECS * 1_000) as i64;
        m.seed([(bucket_ms * 100, Some("opus".to_string()), 999, 0)]);
        assert_eq!(m.window_total(64), 0);
    }

    /// Seeding then observing live must be continuous — the live sample
    /// belongs in the newest column, next to the freshest seeded one.
    #[test]
    fn seeded_history_and_live_samples_share_one_timeline() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.reserve_history(BUCKET_SECS * 3);
        m.seed([((BUCKET_SECS * 1_000) as i64, Some("opus".to_string()), 5, 0)]);
        m.observe(Some("opus"), 7, 0, t0);
        let cols: Vec<u64> = m.window(4).map(Bucket::total).collect();
        assert_eq!(cols, vec![0, 0, 5, 7]);
    }

    #[test]
    fn zero_token_samples_are_ignored() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        // A dollar-only Cost (claude's run-level `result`) carries no tokens
        // and must not register a series or a column.
        m.observe(Some("opus"), 0, 0, t0);
        assert!(m.is_idle());
        assert_eq!(m.window_total(8), 0);
    }

    #[test]
    fn history_is_capped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 10, 0, t0);
        m.advance_to(buckets_after(t0, (MAX_BUCKETS as u64) * 3));
        assert!(m.buckets.len() <= MAX_BUCKETS, "{}", m.buckets.len());
    }

    #[test]
    fn partial_history_is_right_aligned_for_hover() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 700, 0, t0);
        m.advance_to(buckets_after(t0, 1));
        // Two buckets of history in a 10-wide render occupy the last two
        // columns; everything left of them is empty screen, not data.
        assert!(m.bucket_at(0, 10).is_none());
        assert!(m.bucket_at(7, 10).is_none());
        assert_eq!(m.bucket_at(8, 10).map(Bucket::total), Some(700));
        assert_eq!(m.bucket_at(9, 10).map(Bucket::total), Some(0));
    }

    #[test]
    fn legend_collapses_models_past_the_palette() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        for i in 0..(SERIES_PALETTE.len() + 3) {
            m.observe(Some(&format!("m{i}")), 100, 0, t0);
        }
        let legend = m.legend(8);
        assert_eq!(legend.len(), SERIES_PALETTE.len() + 1);
        let last = legend.last().expect("other row");
        assert_eq!(last.label, "other (3)");
        assert_eq!(last.tokens, 300);
        assert_eq!(last.color, OTHER_COLOR);
    }

    /// Two series sharing one terminal cell must both survive it: the lower
    /// one as the partial glyph's filled part, the upper one as its
    /// background. Painting both as foreground glyphs made the second
    /// overwrite the first, so a dominant model erased everything under it.
    #[test]
    fn column_cells_encode_a_boundary_inside_one_cell() {
        // Series 0 takes 3 eighths, series 1 the next 5 — one full cell.
        let cells = column_cells(&[(0, 3), (1, 5)], 8, 4);
        assert_eq!(cells.len(), 1);
        assert_eq!(
            cells[0],
            ColumnCell {
                row: 0,
                glyph: METER_PARTIALS[3],
                fg: 0,
                bg: Some(1),
            }
        );
    }

    /// A cell wholly owned by one series is painted as that series'
    /// background, not as a `█` glyph. Fonts whose FULL BLOCK is shorter than
    /// the terminal's line box leave the leading transparent, which drew a
    /// hairline of panel background at every row boundary and made a solid
    /// bar read as a stack of bricks.
    #[test]
    fn column_cells_paint_whole_cells_as_background() {
        let cells = column_cells(&[(2, 16)], 16, 4);
        assert_eq!(cells.len(), 2);
        assert!(
            cells.iter().all(|c| c.glyph == " " && c.bg == Some(2)),
            "a full cell must carry its color as background: {cells:?}"
        );
        assert!(cells.iter().all(|c| c.fg == 2));
    }

    /// The column's topmost cell is partially filled, so the space above the
    /// glyph has to stay panel background — there is nowhere to encode a
    /// second series, and the larger share takes the cell.
    #[test]
    fn column_cells_give_the_partial_top_cell_to_the_larger_share() {
        // Cell 0 full (series 0), cell 1 has 1 eighth of series 0 and 4 of
        // series 1.
        let cells = column_cells(&[(0, 9), (1, 4)], 13, 4);
        let top = cells.last().expect("top cell");
        assert_eq!(top.row, 1);
        assert_eq!(top.fg, 1, "series 1 owns 4 of the 5 filled eighths");
        assert_eq!(top.bg, None);
    }

    /// Cached and fresh work use solid fills in two lightnesses of one model
    /// hue. Treating the parts as distinct paints lets their boundary survive
    /// a shared terminal cell without inventing a third visual section.
    #[test]
    fn cached_and_fresh_cells_are_solid_distinct_tones() {
        assert_ne!(
            (0u16, Part::Cached).paint(),
            (0u16, Part::New).paint(),
            "the renderer must preserve the two tones"
        );
        let cached = band_cell(0, (0u16, Part::Cached), 8);
        let fresh = band_cell(0, (0u16, Part::New), 8);
        assert_eq!(cached.bg, Some((0, Part::Cached)));
        assert_eq!(fresh.bg, Some((0, Part::New)));
        assert_ne!(cached.bg, fresh.bg, "each tone fills its own cells");
    }

    /// Partial cells retain the eighth-block geometry whichever tone owns
    /// them; lightness carries cache state without changing their height.
    /// They keep a foreground glyph because the empty part of the cell has to
    /// stay panel background — a background fill there would paint the gap
    /// above the bar.
    #[test]
    fn a_partial_cell_keeps_its_eighth_block() {
        let cell = band_cell(0, 3u16, 5);
        assert_eq!(cell.glyph, METER_PARTIALS[5]);
        assert_eq!(cell.fg, 3);
        assert_eq!(cell.bg, None, "the unfilled part must stay panel background");
    }

    /// A model's two tones can occupy one cell exactly: the lower cached tone
    /// is the partial glyph and the upper fresh tone is its background.
    #[test]
    fn a_same_model_boundary_in_one_cell_keeps_both_tones() {
        let cells = column_cells(&[((0u16, Part::Cached), 3), ((0u16, Part::New), 5)], 8, 4);
        assert_eq!(
            cells,
            vec![ColumnCell {
                row: 0,
                glyph: METER_PARTIALS[3],
                fg: (0, Part::Cached),
                bg: Some((0, Part::New)),
            }]
        );
    }

    /// Bands of *different* models still differ in color, so their shared cell
    /// keeps the fg-over-bg encoding — the same-color degradation above must
    /// not leak into the ordinary case.
    #[test]
    fn column_cells_still_blend_a_boundary_between_two_models() {
        let cells = column_cells(&[((0u16, Part::New), 3), ((1u16, Part::New), 5)], 8, 4);
        assert_eq!(
            cells,
            vec![ColumnCell {
                row: 0,
                glyph: METER_PARTIALS[3],
                fg: (0, Part::New),
                bg: Some((1, Part::New)),
            }]
        );
    }

    /// A horizontal bar spends whole cells, so `split_units` has to hand out
    /// exactly the cells asked for and give a band with volume at least one —
    /// the hover detail's bars have no sub-cell resolution to fall back on.
    #[test]
    fn split_units_hands_out_whole_cells_exactly() {
        let parts = [((0u16, Part::Cached), 2_000u64), ((0u16, Part::New), 3_000)];
        let cells = split_units(&parts, 5_000, 15);
        assert_eq!(cells.iter().map(|(_, n)| *n).sum::<usize>(), 15);
        assert_eq!(cells[0], ((0, Part::Cached), 6), "2/5 of 15 cells");
        assert_eq!(cells[1], ((0, Part::New), 9));

        // A part small enough to floor to zero still takes a cell, at the
        // expense of the largest one — a band drawn as nothing would
        // contradict the figure printed beside it.
        let lopsided = split_units(
            &[((0u16, Part::Cached), 1u64), ((0u16, Part::New), 999)],
            1_000,
            8,
        );
        assert_eq!(lopsided.iter().map(|(_, n)| *n).sum::<usize>(), 8);
        assert!(
            lopsided.iter().all(|(_, n)| *n >= 1),
            "no band vanishes: {lopsided:?}"
        );
    }

    /// The column stack is the same split at eighth resolution, so the two
    /// entry points cannot drift.
    #[test]
    fn stacked_eighths_is_the_same_split_in_eighths() {
        let parts = [((0u16, Part::Cached), 1_000u64), ((1u16, Part::New), 3_000)];
        assert_eq!(
            stacked_eighths(&parts, 4_000, 32),
            split_units(&parts, 4_000, 32)
        );
    }

    /// A column's segments sum to exactly the column's height — no drift
    /// from rounding each share independently — and stay ordered by size.
    #[test]
    fn stacked_eighths_sums_exactly_to_the_column_height() {
        let stacked = vec![(0u16, 600u64), (1, 300), (2, 100)];
        let filled = 24; // three full cells
        let out = stacked_eighths(&stacked, 1_000, filled);
        assert_eq!(out.iter().map(|(_, e)| *e).sum::<usize>(), filled);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].1 > out[1].1 && out[1].1 > out[2].1, "{out:?}");
    }

    /// A share smaller than one eighth of the column is omitted from the
    /// stack rather than promoted to a visible slice — inflating 1% into 4%
    /// would have to take that space from a series that earned it. The
    /// column's own height still accounts for the tokens, and the legend
    /// still lists the model, so nothing is lost from the totals.
    #[test]
    fn stacked_eighths_drops_sub_quantum_shares_without_losing_height() {
        let stacked = vec![(0u16, 900u64), (1, 90), (2, 10)];
        let filled = 24;
        let out = stacked_eighths(&stacked, 1_000, filled);
        assert_eq!(out.iter().map(|(_, e)| *e).sum::<usize>(), filled);
        assert!(!out.iter().any(|(m, _)| *m == 2), "{out:?}");
    }

    /// A single-eighth column still belongs to whichever series produced it.
    #[test]
    fn stacked_eighths_handles_a_one_eighth_column() {
        let out = stacked_eighths(&[(4u16, 12u64)], 12, 1);
        assert_eq!(out, vec![(4, 1)]);
    }

    #[test]
    fn stacked_eighths_is_empty_without_volume() {
        assert!(stacked_eighths::<u16>(&[], 0, 8).is_empty());
        assert!(stacked_eighths(&[(0u16, 5u64)], 5, 0).is_empty());
    }
}
