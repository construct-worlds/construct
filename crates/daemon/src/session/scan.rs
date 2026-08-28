//! Incremental transcript scan + on-disk checkpoint (spec 0211).
//!
//! Several of a session's summary fields are *derived* from its transcript:
//! the sequence counter, the message count, the last-message snippet, the
//! last error, the lifetime token tally (spec 0103), the context gauge
//! (spec 0104), and the fleet token-history samples (spec 0167). The daemon
//! rebuilds them at boot so a summary written before a field existed — or one
//! that lagged a crash — self-heals.
//!
//! Rebuilding them by re-reading every transcript in full costs time
//! proportional to *all history ever recorded*, paid on every start before
//! the IPC socket binds. That is fine at a few megabytes and untenable at a
//! few gigabytes.
//!
//! So the fold is checkpointed. Each session stores the derived state next to
//! its transcript along with the byte length that state reflects. On the next
//! boot the transcript is `stat`ed and only the bytes past that offset are
//! read. Transcripts are append-only, so a recorded length is always a line
//! boundary and a matching length means "nothing to do".
//!
//! Staleness is a cost, never a correctness problem: a missing, truncated, or
//! version-mismatched checkpoint just falls back to a full scan.

use chrono::{DateTime, Utc};
use construct_protocol::{
    ContextSegment, MessageRole, SessionEvent, SessionState, TimestampedEvent, TokenSample,
    TokenTally,
};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Seek, SeekFrom};

/// Bumped when the fold changes shape in a way that makes an existing
/// checkpoint wrong (a new derived field, different semantics for one that
/// exists). Old checkpoints are then discarded in favour of a full rescan
/// rather than migrated — the source of truth is still on disk.
pub const SCAN_VERSION: u32 = 1;

/// The derived state a transcript walk produces.
///
/// Every field is `#[serde(default)]` so a checkpoint written by an older
/// build still loads: a field it did not know about starts empty and is
/// filled by the tail scan, exactly as it would be on a full rebuild.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptScan {
    /// Durable sequence counter — one per non-empty transcript line.
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_message_role: Option<MessageRole>,
    #[serde(default)]
    pub last_message_text: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub tokens: TokenTally,
    #[serde(default)]
    pub context_used: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub context_segments: Vec<ContextSegment>,
    /// The model in effect where the scan stopped. Persisted because a tail
    /// scan resuming mid-transcript would otherwise attribute samples to no
    /// model until the next `ModelChanged`, silently crediting work to
    /// `unattributed` that a full scan attributes correctly (spec 0167).
    #[serde(default)]
    pub model: Option<String>,
    /// Fleet token-history samples (spec 0167), pruned to the retention
    /// window when the checkpoint is written.
    #[serde(default)]
    pub cost_samples: Vec<TokenSample>,
}

impl TranscriptScan {
    /// Fold one transcript line. `history_cutoff` bounds which cost samples
    /// are worth retaining; `session_id` stamps them for scoped meters.
    fn fold_line(&mut self, line: &str, history_cutoff: DateTime<Utc>, session_id: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.seq += 1;
        let Ok(ts) = serde_json::from_str::<TimestampedEvent>(line) else {
            return;
        };
        match ts.event {
            SessionEvent::Message { role, ref text } => {
                self.message_count += 1;
                self.last_message_at = Some(ts.at);
                construct_protocol::fold_last_message(
                    &mut self.last_message_role,
                    &mut self.last_message_text,
                    role,
                    text,
                );
            }
            SessionEvent::Error { ref message } => {
                self.last_error = Some(construct_protocol::snippet(message));
            }
            SessionEvent::Done { exit_code } if exit_code != 0 => {
                self.last_error = Some(format!("exited {exit_code}"));
            }
            SessionEvent::Status { state, .. } if state == SessionState::Running => {
                // Mirrors the live fold: a fresh turn outdates the previous
                // failure.
                self.last_error = None;
            }
            SessionEvent::ModelChanged { ref model } => {
                self.model = Some(model.clone());
            }
            SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ref model,
                ..
            } => {
                if ts.at >= history_cutoff {
                    self.cost_samples.push(TokenSample {
                        at_ms: ts.at.timestamp_millis(),
                        session_id: Some(session_id.to_string()),
                        model: model.clone().or_else(|| self.model.clone()),
                        // Cached input is a subset of the prompt side; adding
                        // it would double-count. It is carried alongside so
                        // the recovered history can still tell new work from
                        // re-served context.
                        tokens: tokens_in.saturating_add(tokens_out),
                        cached: tokens_cached,
                    });
                }
                self.tokens.add(tokens_in, tokens_out, tokens_cached);
            }
            SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            } => {
                self.context_used = Some(used_tokens);
                if window_tokens.is_some() {
                    self.context_window = window_tokens;
                }
            }
            SessionEvent::ContextBreakdown { segments: segs } => {
                self.context_segments = segs;
            }
            SessionEvent::Reset => {
                self.context_used = None;
                self.context_window = None;
                self.context_segments.clear();
                self.last_message_role = None;
                self.last_message_text = None;
                self.last_error = None;
            }
            _ => {}
        }
    }

    /// Fold every line `reader` yields onto this state.
    fn fold_reader<R: BufRead>(
        &mut self,
        reader: R,
        history_cutoff: DateTime<Utc>,
        session_id: &str,
    ) -> std::io::Result<()> {
        for line in reader.lines() {
            self.fold_line(&line?, history_cutoff, session_id);
        }
        Ok(())
    }

    /// Drop cost samples that have aged out of the retention window. Called
    /// before a checkpoint is written so the file cannot accumulate samples
    /// no consumer would accept back.
    pub fn prune_cost_samples(&mut self, history_cutoff: DateTime<Utc>) {
        let cutoff_ms = history_cutoff.timestamp_millis();
        self.cost_samples.retain(|s| s.at_ms >= cutoff_ms);
    }
}

/// A [`TranscriptScan`] plus the transcript length it reflects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanCheckpoint {
    pub version: u32,
    /// Transcript size in bytes at the point this state was folded. Always a
    /// line boundary, because transcripts are only ever appended to.
    pub bytes: u64,
    #[serde(flatten)]
    pub scan: TranscriptScan,
}

/// Why a boot scan read what it read. Recorded so the daemon log can
/// distinguish "checkpoint worked" from "checkpoint was rejected", which is
/// the difference between a fast boot and a slow one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// Checkpoint matched the transcript exactly; nothing was read.
    UpToDate,
    /// Checkpoint was behind; only the bytes past it were read.
    Tail,
    /// No usable checkpoint; the transcript was read from the start.
    Full,
}

/// Outcome of scanning one session's transcript at boot.
pub struct ScanOutcome {
    pub scan: TranscriptScan,
    pub mode: ScanMode,
    /// Transcript length the returned state reflects — what a fresh
    /// checkpoint should record.
    pub bytes: u64,
    /// Bytes actually read, for logging.
    pub read: u64,
}

/// Rebuild a session's derived state, reading as little of its transcript as
/// the checkpoint allows.
///
/// `prior` is the checkpoint as loaded from disk (if any). A checkpoint is
/// used when its version matches and its recorded length is no greater than
/// the transcript's current length; anything else means the file was
/// truncated, replaced, or written by a build that folded differently, and
/// the only safe response is a full rescan.
///
/// Callers must not run this concurrently with appends to the same
/// transcript — the length is sampled once, and a write landing mid-scan
/// would leave the returned `bytes` disagreeing with the returned state.
pub fn scan_transcript(
    path: &std::path::Path,
    session_id: &str,
    prior: Option<ScanCheckpoint>,
    history_cutoff: DateTime<Utc>,
) -> std::io::Result<ScanOutcome> {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        // No transcript yet (a session that never recorded an event) is an
        // empty fold, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScanOutcome {
                scan: TranscriptScan::default(),
                mode: ScanMode::Full,
                bytes: 0,
                read: 0,
            })
        }
        Err(e) => return Err(e),
    };

    let usable = prior.filter(|c| c.version == SCAN_VERSION && c.bytes <= size);
    let (mut scan, from) = match usable {
        Some(c) => (c.scan, c.bytes),
        None => (TranscriptScan::default(), 0),
    };

    if from == size {
        scan.prune_cost_samples(history_cutoff);
        return Ok(ScanOutcome {
            scan,
            mode: ScanMode::UpToDate,
            bytes: size,
            read: 0,
        });
    }

    let mode = if from == 0 {
        ScanMode::Full
    } else {
        ScanMode::Tail
    };
    let mut f = std::fs::File::open(path)?;
    if from > 0 {
        f.seek(SeekFrom::Start(from))?;
    }
    scan.fold_reader(std::io::BufReader::new(f), history_cutoff, session_id)?;
    scan.prune_cost_samples(history_cutoff);
    Ok(ScanOutcome {
        scan,
        mode,
        bytes: size,
        read: size - from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use construct_protocol::SessionEvent;
    use std::io::Write;

    /// Far enough in the past that nothing a test writes ages out of it.
    fn wide_cutoff() -> DateTime<Utc> {
        Utc::now() - chrono::Duration::days(365)
    }

    fn append(path: &std::path::Path, at: DateTime<Utc>, event: SessionEvent) {
        let ts = TimestampedEvent { seq: 0, at, event };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open transcript");
        writeln!(f, "{}", serde_json::to_string(&ts).expect("encode")).expect("append");
    }

    fn msg(text: &str) -> SessionEvent {
        SessionEvent::Message {
            role: construct_protocol::MessageRole::Assistant,
            text: text.into(),
        }
    }

    fn cost(tokens_in: u64, tokens_out: u64, model: Option<&str>) -> SessionEvent {
        SessionEvent::Cost {
            usd: 0.0,
            tokens_in,
            tokens_out,
            tokens_cached: 0,
            model: model.map(str::to_string),
        }
    }

    /// The checkpoint is an optimization, not a different fold: resuming from
    /// a mid-transcript offset must land on exactly the state a cold full
    /// scan of the same bytes produces. This is the property the whole design
    /// rests on, so it is asserted field by field rather than by spot-check.
    #[test]
    fn a_tail_scan_lands_where_a_full_scan_would() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        let base = Utc::now() - chrono::Duration::hours(1);

        // First half, then a checkpoint taken exactly there.
        append(&path, base, SessionEvent::ModelChanged { model: "opus".into() });
        append(&path, base + chrono::Duration::seconds(1), msg("one"));
        append(&path, base + chrono::Duration::seconds(2), cost(10, 5, None));

        let half = scan_transcript(&path, "s1", None, wide_cutoff()).expect("half scan");
        assert_eq!(half.mode, ScanMode::Full);
        let checkpoint = ScanCheckpoint {
            version: SCAN_VERSION,
            bytes: half.bytes,
            scan: half.scan,
        };

        // Second half.
        append(&path, base + chrono::Duration::seconds(3), msg("two"));
        append(&path, base + chrono::Duration::seconds(4), cost(7, 3, Some("sonnet")));
        append(
            &path,
            base + chrono::Duration::seconds(5),
            SessionEvent::ContextUsage {
                used_tokens: 900,
                window_tokens: Some(1000),
            },
        );

        let resumed =
            scan_transcript(&path, "s1", Some(checkpoint), wide_cutoff()).expect("tail scan");
        let cold = scan_transcript(&path, "s1", None, wide_cutoff()).expect("cold scan");

        assert_eq!(resumed.mode, ScanMode::Tail);
        assert_eq!(cold.mode, ScanMode::Full);
        assert!(
            resumed.read < cold.read,
            "the tail scan must read less than the full scan it replaces"
        );

        let (a, b) = (&resumed.scan, &cold.scan);
        assert_eq!(a.seq, b.seq);
        assert_eq!(a.message_count, b.message_count);
        assert_eq!(a.last_message_at, b.last_message_at);
        assert_eq!(a.last_message_role, b.last_message_role);
        assert_eq!(a.last_message_text, b.last_message_text);
        assert_eq!(a.last_error, b.last_error);
        assert_eq!(a.tokens, b.tokens);
        assert_eq!(a.context_used, b.context_used);
        assert_eq!(a.context_window, b.context_window);
        assert_eq!(a.model, b.model);
        assert_eq!(a.cost_samples.len(), b.cost_samples.len());
        for (x, y) in a.cost_samples.iter().zip(b.cost_samples.iter()) {
            assert_eq!(x.at_ms, y.at_ms);
            assert_eq!(x.tokens, y.tokens);
            assert_eq!(x.model, y.model);
            assert_eq!(x.session_id, y.session_id);
        }
        assert_eq!(resumed.bytes, cold.bytes);
    }

    /// The steady-state boot: a checkpoint level with the transcript reads
    /// nothing at all. This is the difference between a boot proportional to
    /// new history and one proportional to all of it.
    #[test]
    fn a_current_checkpoint_reads_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        append(&path, Utc::now(), msg("only"));

        let first = scan_transcript(&path, "s1", None, wide_cutoff()).expect("first");
        let checkpoint = ScanCheckpoint {
            version: SCAN_VERSION,
            bytes: first.bytes,
            scan: first.scan.clone(),
        };

        let again = scan_transcript(&path, "s1", Some(checkpoint), wide_cutoff()).expect("again");
        assert_eq!(again.mode, ScanMode::UpToDate);
        assert_eq!(again.read, 0, "nothing may be read when lengths agree");
        assert_eq!(again.scan.seq, first.scan.seq);
        assert_eq!(again.scan.message_count, first.scan.message_count);
    }

    /// A cost sample carries the model in effect *at that point* in the
    /// transcript (spec 0167). Since a tail scan starts after the
    /// `ModelChanged` that set it, that model has to survive in the
    /// checkpoint or the resumed sample is silently unattributed.
    #[test]
    fn model_attribution_survives_a_resume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        let base = Utc::now() - chrono::Duration::minutes(5);

        append(&path, base, SessionEvent::ModelChanged { model: "opus".into() });
        let first = scan_transcript(&path, "s1", None, wide_cutoff()).expect("first");
        let checkpoint = ScanCheckpoint {
            version: SCAN_VERSION,
            bytes: first.bytes,
            scan: first.scan,
        };

        // A usage report that names no model of its own.
        append(&path, base + chrono::Duration::seconds(1), cost(4, 2, None));

        let resumed =
            scan_transcript(&path, "s1", Some(checkpoint), wide_cutoff()).expect("resumed");
        assert_eq!(resumed.mode, ScanMode::Tail);
        assert_eq!(
            resumed.scan.cost_samples[0].model.as_deref(),
            Some("opus"),
            "the model in effect at the checkpoint must carry into the tail"
        );
    }

    /// A checkpoint claiming more bytes than the file holds describes a
    /// transcript that no longer exists — truncated, rotated, or replaced.
    /// Trusting it would fold a stale tally onto fresh content, so it is
    /// discarded in favour of a full rescan.
    #[test]
    fn a_checkpoint_past_the_end_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        append(&path, Utc::now(), msg("fresh"));

        let stale = ScanCheckpoint {
            version: SCAN_VERSION,
            bytes: 10_000_000,
            scan: TranscriptScan {
                seq: 999,
                message_count: 999,
                ..Default::default()
            },
        };
        let out = scan_transcript(&path, "s1", Some(stale), wide_cutoff()).expect("scan");
        assert_eq!(out.mode, ScanMode::Full);
        assert_eq!(out.scan.seq, 1, "the bogus counter must not survive");
        assert_eq!(out.scan.message_count, 1);
    }

    /// A checkpoint from a build that folded differently is not migrated —
    /// the transcript is still the source of truth, so it is simply rebuilt.
    #[test]
    fn a_checkpoint_from_another_version_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        append(&path, Utc::now(), msg("fresh"));

        let first = scan_transcript(&path, "s1", None, wide_cutoff()).expect("first");
        let foreign = ScanCheckpoint {
            version: SCAN_VERSION + 1,
            bytes: first.bytes,
            scan: TranscriptScan {
                seq: 999,
                ..Default::default()
            },
        };
        let out = scan_transcript(&path, "s1", Some(foreign), wide_cutoff()).expect("scan");
        assert_eq!(out.mode, ScanMode::Full);
        assert_eq!(out.scan.seq, 1);
    }

    /// Samples that have aged out of the retention window are dropped before
    /// the checkpoint is written, so the file cannot grow without bound with
    /// history no consumer would accept back.
    #[test]
    fn samples_outside_the_window_are_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("transcript.jsonl");
        let cutoff = Utc::now() - chrono::Duration::hours(12);

        append(&path, Utc::now() - chrono::Duration::hours(30), cost(100, 50, None));
        append(&path, Utc::now() - chrono::Duration::minutes(5), cost(7, 3, None));

        let out = scan_transcript(&path, "s1", None, cutoff).expect("scan");
        assert_eq!(out.scan.cost_samples.len(), 1, "only the in-window sample is kept");
        assert_eq!(out.scan.cost_samples[0].tokens, 10);
        assert_eq!(
            out.scan.tokens.total(),
            160,
            "the lifetime tally still counts the aged-out report — only the \
             rolling window forgets it"
        );
    }

    /// A session that never recorded an event has no transcript file. That is
    /// an empty fold, not a boot failure.
    #[test]
    fn a_missing_transcript_scans_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = scan_transcript(&tmp.path().join("absent.jsonl"), "s1", None, wide_cutoff())
            .expect("scan");
        assert_eq!(out.scan.seq, 0);
        assert_eq!(out.bytes, 0);
        assert_eq!(out.read, 0);
    }
}
