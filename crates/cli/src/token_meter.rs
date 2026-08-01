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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::style::Color;

/// Width of one history bucket. One second reads as a CPU-meter tick and
/// keeps a full-width panel (~200 columns) inside a few minutes of history.
pub const BUCKET: Duration = Duration::from_secs(1);

/// Hard cap on retained history, independent of panel width — a very wide
/// terminal still can't grow this unboundedly.
const MAX_BUCKETS: usize = 600;

/// Per-bucket decay applied to the autoscale ceiling. 0.97/s puts the
/// half-life around 23 seconds: a burst's peak stays referenced long enough
/// to compare the next few columns against, then releases so a quiet fleet
/// doesn't stay pinned to the floor of a spike from minutes ago.
const PEAK_DECAY: f64 = 0.97;

/// Floor for the autoscale ceiling. Without it, a single 40-token sample on
/// an otherwise idle fleet would paint a full-height column and read as
/// saturation.
const MIN_SCALE: u64 = 1_000;

/// Distinct midtone series colors, assigned in first-seen order. Same values
/// as the context-breakdown palette so a model's color reads as "a series
/// color" in the same visual language; kept as its own const because the two
/// lists are free to diverge.
const SERIES_PALETTE: [Color; 6] = [
    Color::Rgb(250, 179, 135), // orange
    Color::Rgb(137, 180, 250), // blue
    Color::Rgb(249, 226, 175), // yellow
    Color::Rgb(203, 166, 247), // mauve
    Color::Rgb(148, 226, 213), // teal
    Color::Rgb(245, 194, 231), // pink
];

/// Every model past the palette's size shares this gray and is reported as
/// "other" in the legend, rather than reusing a color already spoken for.
const OTHER_COLOR: Color = Color::Rgb(127, 132, 156);

/// Label for a sample whose model could be established neither from the
/// report nor from the session's tracked model. Named rather than hidden so
/// the graph never silently drops volume it did measure.
pub const UNATTRIBUTED: &str = "unattributed";

/// One time bucket: sparse (model index, tokens) pairs. Most buckets hold
/// zero or one entry, so a dense per-model vector would be mostly padding.
#[derive(Debug, Default, Clone)]
pub struct Bucket {
    entries: Vec<(u16, u64)>,
}

impl Bucket {
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|(_, t)| *t).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Model-index/token pairs, largest first, so a stacked column draws its
    /// dominant series at the base.
    pub fn stacked(&self) -> Vec<(u16, u64)> {
        let mut out = self.entries.clone();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    fn add(&mut self, model: u16, tokens: u64) {
        match self.entries.iter_mut().find(|(m, _)| *m == model) {
            Some(slot) => slot.1 = slot.1.saturating_add(tokens),
            None => self.entries.push((model, tokens)),
        }
    }
}

/// One legend row: a model, its color, and what it consumed over the
/// currently visible history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendEntry {
    pub label: String,
    pub tokens: u64,
    pub color: Color,
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
        }
    }

    /// Roll the history forward to `now`, appending empty buckets for the
    /// time that passed. Called every frame so idle time scrolls as a gap
    /// rather than compressing away.
    pub fn advance_to(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.current_start);
        let steps = (elapsed.as_millis() / BUCKET.as_millis()) as usize;
        if steps == 0 {
            return;
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
    }

    /// Record one usage sample. `model` is the label to group under —
    /// callers resolve it from the report first, then the session's tracked
    /// model, and pass `None` only when neither is known.
    pub fn observe(&mut self, model: Option<&str>, tokens: u64, now: Instant) {
        if tokens == 0 {
            return;
        }
        self.advance_to(now);
        let idx = self.intern(model.unwrap_or(UNATTRIBUTED));
        let bucket = self
            .buckets
            .back_mut()
            .expect("meter always holds a current bucket");
        bucket.add(idx, tokens);
        let total = bucket.total() as f64;
        if total > self.peak {
            self.peak = total;
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

    /// Tokens-per-bucket value the top of the graph represents, for a
    /// `width`-wide render. Always at least the tallest visible column — the
    /// decaying peak only slows the ceiling's *descent* after a burst
    /// scrolls off, so the graph settles instead of snapping between scales.
    pub fn scale(&self, width: usize) -> u64 {
        self.peak
            .max(self.max_in_window(width) as f64)
            .max(MIN_SCALE as f64)
            .ceil() as u64
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

    /// Legend rows for the visible window, busiest first. Models sharing the
    /// "other" color collapse into one row so the legend can't claim a color
    /// distinguishes them.
    pub fn legend(&self, width: usize) -> Vec<LegendEntry> {
        let mut totals: Vec<u64> = vec![0; self.models.len()];
        for bucket in self.window(width) {
            for (idx, tokens) in &bucket.entries {
                if let Some(slot) = totals.get_mut(*idx as usize) {
                    *slot = slot.saturating_add(*tokens);
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
            named.push(LegendEntry {
                label: format!("other ({count})"),
                tokens: other,
                color: OTHER_COLOR,
            });
        }
        named
    }

    /// Total tokens across the visible window — the header readout.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn samples_land_in_arrival_buckets() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, t0);
        m.observe(Some("opus"), 50, at(t0, 2));
        m.advance_to(at(t0, 2));
        let cols: Vec<u64> = m.window(3).map(Bucket::total).collect();
        assert_eq!(cols, vec![100, 0, 50], "one empty bucket between samples");
    }

    #[test]
    fn distinct_models_stack_within_one_bucket() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100, t0);
        m.observe(Some("codex"), 300, t0);
        let bucket = m.window(1).next().expect("current bucket");
        assert_eq!(bucket.total(), 400);
        let stacked = bucket.stacked();
        assert_eq!(stacked.len(), 2);
        assert_eq!(m.model_label(stacked[0].0), "codex", "largest series first");
        assert_eq!(stacked[0].1, 300);
    }

    #[test]
    fn colors_follow_first_seen_order_not_rank() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("first"), 10, t0);
        m.observe(Some("second"), 10_000, t0);
        // "second" dominates every ranking, but "first" keeps palette slot 0.
        assert_eq!(m.color(0), SERIES_PALETTE[0]);
        assert_eq!(m.color(1), SERIES_PALETTE[1]);
        assert_eq!(m.model_label(0), "first");
    }

    #[test]
    fn scale_holds_a_floor_then_tracks_the_peak() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        assert_eq!(m.scale(80), MIN_SCALE, "idle meter uses the floor");
        m.observe(Some("opus"), 40, t0);
        assert_eq!(m.scale(80), MIN_SCALE, "a tiny sample must not saturate");
        m.observe(Some("opus"), 50_000, t0);
        assert!(m.scale(80) >= 50_000);
    }

    #[test]
    fn a_visible_column_always_fits_the_scale() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100_000, t0);
        m.advance_to(at(t0, 30));
        // The spike is 30 buckets back: still on screen at width 80, so the
        // ceiling must still contain it however far the peak has decayed.
        assert!(m.scale(80) >= 100_000, "{}", m.scale(80));
    }

    #[test]
    fn ceiling_descends_gradually_once_a_burst_scrolls_off() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 100_000, t0);
        m.advance_to(at(t0, 30));
        // Narrow render: the spike has scrolled out of view, so only the
        // decayed peak holds the ceiling up.
        let narrow = m.scale(10);
        assert!(narrow < 100_000, "ceiling must fall: {narrow}");
        assert!(
            narrow > MIN_SCALE,
            "…but not snap straight to the floor: {narrow}"
        );
        m.advance_to(at(t0, 600));
        assert_eq!(m.scale(10), MIN_SCALE, "eventually returns to the floor");
    }

    #[test]
    fn unattributed_samples_are_counted_not_dropped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(None, 500, t0);
        assert_eq!(m.window_total(8), 500);
        assert_eq!(m.legend(8)[0].label, UNATTRIBUTED);
    }

    #[test]
    fn zero_token_samples_are_ignored() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        // A dollar-only Cost (claude's run-level `result`) carries no tokens
        // and must not register a series or a column.
        m.observe(Some("opus"), 0, t0);
        assert!(m.is_idle());
        assert_eq!(m.window_total(8), 0);
    }

    #[test]
    fn history_is_capped() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 10, t0);
        m.advance_to(at(t0, (MAX_BUCKETS as u64) * 3));
        assert!(m.buckets.len() <= MAX_BUCKETS, "{}", m.buckets.len());
    }

    #[test]
    fn partial_history_is_right_aligned_for_hover() {
        let t0 = Instant::now();
        let mut m = TokenMeter::new(t0);
        m.observe(Some("opus"), 700, t0);
        m.advance_to(at(t0, 1));
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
            m.observe(Some(&format!("m{i}")), 100, t0);
        }
        let legend = m.legend(8);
        assert_eq!(legend.len(), SERIES_PALETTE.len() + 1);
        let last = legend.last().expect("other row");
        assert_eq!(last.label, "other (3)");
        assert_eq!(last.tokens, 300);
        assert_eq!(last.color, OTHER_COLOR);
    }
}
