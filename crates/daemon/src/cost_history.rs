//! Fleet-wide rolling window of token-usage samples (spec 0167).
//!
//! The token meter is a client-side render, but its *history* cannot be:
//! the fleet keeps burning tokens while a TUI is closed, so a client that
//! only remembered its own samples would come back showing a hole exactly
//! where work happened. The daemon is the one process that sees every
//! session's usage report whether or not anyone is attached, so it keeps the
//! window and clients seed from it.
//!
//! The samples are recovered at boot from the transcripts the daemon already
//! walks to self-heal each session's token tally (spec 0103), so there is no
//! second capture path to keep consistent. That walk resumes from a
//! per-session checkpoint (spec 0211) which carries the in-window samples
//! with it, so a restart recovers the window without re-reading history in
//! full — and, the checkpoint being a discardable cache, loses nothing if it
//! is missing.
//!
//! Structured like [`crate::usage::UsageCache`]: a plain, non-async struct
//! behind a `std::sync::Mutex` on `SessionManager`, so every critical section
//! is a tiny push/read never held across an `.await`.

use std::collections::VecDeque;

use construct_protocol::TokenSample;

/// How far back the window reaches. Comfortably longer than a full-width
/// panel of one-minute columns, so a client can seed a wide terminal without
/// the daemon having discarded the left edge.
pub const WINDOW_SECS: i64 = 12 * 60 * 60;

/// Hard cap on retained samples, so a pathologically chatty fleet can't grow
/// this without bound between evictions.
const MAX_SAMPLES: usize = 20_000;

/// Rolling window of usage samples, oldest first.
#[derive(Default)]
pub struct CostHistory {
    samples: VecDeque<TokenSample>,
}

impl CostHistory {
    /// Build from samples recovered at boot. Input need not be sorted —
    /// transcripts are walked one session at a time, so the fleet-wide
    /// sequence only becomes chronological after a merge.
    pub fn from_scan(mut samples: Vec<TokenSample>, now_ms: i64) -> Self {
        samples.sort_by_key(|s| s.at_ms);
        let mut history = Self::default();
        for sample in samples {
            history.push(sample, now_ms);
        }
        history
    }

    pub fn push(&mut self, sample: TokenSample, now_ms: i64) {
        if sample.tokens == 0 {
            return;
        }
        self.samples.push_back(sample);
        self.evict(now_ms);
    }

    fn evict(&mut self, now_ms: i64) {
        let cutoff = now_ms - WINDOW_SECS * 1_000;
        while self.samples.front().is_some_and(|s| s.at_ms < cutoff) {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// The most recent samples within `window_secs`, oldest first, capped at
    /// `limit`. When the cap bites it keeps the *newest* samples: a client
    /// seeding a graph would rather have a complete recent picture than a
    /// complete old one.
    pub fn recent(&self, window_secs: i64, limit: usize, now_ms: i64) -> Vec<TokenSample> {
        let cutoff = now_ms - window_secs.max(0) * 1_000;
        let in_window: Vec<&TokenSample> =
            self.samples.iter().filter(|s| s.at_ms >= cutoff).collect();
        in_window
            .iter()
            .skip(in_window.len().saturating_sub(limit))
            .map(|s| (*s).clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at_ms: i64, tokens: u64) -> TokenSample {
        TokenSample {
            at_ms,
            session_id: Some("s1".into()),
            model: Some("opus".into()),
            tokens,
            cached: 0,
        }
    }

    #[test]
    fn scan_merges_sessions_into_one_chronological_window() {
        // Two sessions' transcripts, each ordered internally but interleaved
        // in wall-clock — the merged window must be sorted.
        let history = CostHistory::from_scan(
            vec![sample(300, 1), sample(100, 2), sample(200, 3)],
            1_000,
        );
        let times: Vec<i64> = history
            .recent(WINDOW_SECS, 10, 1_000)
            .iter()
            .map(|s| s.at_ms)
            .collect();
        assert_eq!(times, vec![100, 200, 300]);
        assert!(history
            .recent(WINDOW_SECS, 10, 1_000)
            .iter()
            .all(|sample| sample.session_id.as_deref() == Some("s1")));
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let now = WINDOW_SECS * 1_000 + 10_000;
        let mut history = CostHistory::default();
        history.push(sample(1_000, 5), now);
        assert_eq!(history.len(), 0, "outside the window on arrival");
        history.push(sample(now - 1_000, 5), now);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn zero_token_samples_are_not_retained() {
        let mut history = CostHistory::default();
        // A dollar-only report carries no volume and would only take up a
        // slot in the window.
        history.push(sample(10, 0), 1_000);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn the_cap_keeps_the_newest_samples() {
        let mut history = CostHistory::default();
        for i in 0..50 {
            history.push(sample(i, 1), 10_000);
        }
        let recent = history.recent(WINDOW_SECS, 10, 10_000);
        assert_eq!(recent.len(), 10);
        assert_eq!(recent.first().map(|s| s.at_ms), Some(40));
        assert_eq!(recent.last().map(|s| s.at_ms), Some(49));
    }

    #[test]
    fn a_narrower_window_than_retained_is_honored() {
        let mut history = CostHistory::default();
        history.push(sample(1_000, 1), 100_000);
        history.push(sample(95_000, 1), 100_000);
        let recent = history.recent(10, 100, 100_000);
        assert_eq!(recent.len(), 1, "only the last 10s");
        assert_eq!(recent[0].at_ms, 95_000);
    }
}
