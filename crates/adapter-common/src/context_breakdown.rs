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

/// Label of the differential fixed-overhead segment (spec 0156). One
/// shared constant so every harness's hover reads the same.
pub const FIXED_OVERHEAD_LABEL: &str = "fixed overhead";

/// Differential fixed-overhead pin (spec 0156): the part of the window the
/// adapter's data surface can't itemize — system prompt, tool schemas, MCP
/// schemas, skills listings — measured as `used − Σ estimated segments` at
/// the *first* gauge report of a context epoch, where the conversation (and
/// therefore the char-heuristic error) is smallest. The fixed prefix doesn't
/// change within an epoch, so the epoch-first residual stays valid as the
/// conversation grows; re-deriving it later would just re-absorb the
/// messages-estimate drift the pin exists to avoid.
///
/// Two driving styles, same struct:
/// - **Stateless scans** (transcript/rollout/wire-log walks): build a fresh
///   pin per scan, `observe` at each usage record in file order, `reset` at
///   each compaction record. The pin lands on the current epoch's first
///   usage record, deterministically, so restarts and re-scans agree.
/// - **Stateful watchers** (gauges with no on-disk history): keep the pin
///   across polls, `observe` whenever the gauge is read, `reset` on rebind.
///   After an adapter restart mid-conversation the pin re-measures at the
///   current turn — coarser (it inherits the messages estimate's error at
///   that point), but still the same residual the client's "unaccounted"
///   row would have shown, now labeled and frozen.
///
/// The residual is a real-number-minus-estimate, so the segment stays
/// `estimated` (spec 0156's `~` contract). `used` must be the
/// harness-reported prompt side of the gauge, never itself an estimate.
#[derive(Default)]
pub struct FixedOverheadPin {
    pinned: Option<u64>,
}

impl FixedOverheadPin {
    /// Record a gauge observation: `used_tokens` as harness-reported,
    /// `estimated_tokens` the sum of every segment estimate derivable at
    /// that same moment. Only the first observation after construction (or
    /// [`reset`](Self::reset)) pins; later calls are no-ops.
    pub fn observe(&mut self, used_tokens: u64, estimated_tokens: u64) {
        if self.pinned.is_none() {
            self.pinned = Some(used_tokens.saturating_sub(estimated_tokens));
        }
    }

    /// The context epoch changed (compaction, `/clear`, session rebind):
    /// drop the pin so the next observation re-measures.
    pub fn reset(&mut self) {
        self.pinned = None;
    }

    /// The pinned segment, placed by convention immediately before the
    /// conversation (`messages`) segment. `None` until an observation
    /// lands or when the residual is zero — a harness whose estimates
    /// already cover the window reports no overhead row.
    pub fn segment(&self) -> Option<ContextSegment> {
        self.pinned
            .filter(|tokens| *tokens > 0)
            .map(|tokens| ContextSegment::new(FIXED_OVERHEAD_LABEL, tokens, true))
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
    fn pin_holds_first_observation_until_reset() {
        let mut pin = FixedOverheadPin::default();
        assert!(pin.segment().is_none());
        pin.observe(10_000, 400);
        pin.observe(50_000, 30_000);
        let seg = pin.segment().expect("pinned");
        assert_eq!(seg.label, FIXED_OVERHEAD_LABEL);
        assert_eq!(seg.tokens, 9_600);
        assert!(seg.estimated);
        pin.reset();
        assert!(pin.segment().is_none());
        pin.observe(52_000, 31_000);
        assert_eq!(pin.segment().expect("re-pinned").tokens, 21_000);
    }

    #[test]
    fn pin_reports_nothing_on_zero_or_negative_residual() {
        let mut pin = FixedOverheadPin::default();
        pin.observe(100, 250);
        assert!(pin.segment().is_none());
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
