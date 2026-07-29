//! Shared helpers for the per-component context breakdown (spec 0156).
//!
//! Adapters that can see their harness's conversation content (native
//! transcripts, rollouts, wire logs) estimate segment sizes with the same
//! coarse char heuristic smith's rolling-window manager uses, and emit
//! [`construct_protocol::SessionEvent::ContextBreakdown`] at the same
//! cadence as the context-usage gauge — on change, not per poll.

use construct_protocol::ContextSegment;

/// Char-heuristic token estimate (`chars / 3.5`, matching smith's
/// `context::estimate_tokens`). Segments built from this are estimates by
/// definition and must set [`ContextSegment::estimated`].
pub fn estimate_tokens_from_chars(chars: usize) -> u64 {
    (chars as f64 / 3.5) as u64
}

/// Change gate for breakdown reports: `changed` returns true (and records
/// the new value) only when the segment list differs from the last one
/// passed in, so adapters that recompute per poll don't spam identical
/// transcript rows (spec 0104's report-on-change rule applies to the
/// breakdown too).
#[derive(Default)]
pub struct BreakdownGate {
    last: Option<Vec<ContextSegment>>,
}

impl BreakdownGate {
    pub fn changed(&mut self, segments: &[ContextSegment]) -> bool {
        if self.last.as_deref() == Some(segments) {
            return false;
        }
        self.last = Some(segments.to_vec());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_matches_smith_ratio() {
        assert_eq!(estimate_tokens_from_chars(0), 0);
        assert_eq!(estimate_tokens_from_chars(35), 10);
        assert_eq!(estimate_tokens_from_chars(350_000), 100_000);
    }

    #[test]
    fn gate_fires_only_on_change() {
        let mut gate = BreakdownGate::default();
        let a = vec![ContextSegment::new("messages", 10, true)];
        assert!(gate.changed(&a));
        assert!(!gate.changed(&a));
        let b = vec![ContextSegment::new("messages", 11, true)];
        assert!(gate.changed(&b));
        assert!(!gate.changed(&b));
    }
}
