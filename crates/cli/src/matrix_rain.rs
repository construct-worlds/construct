//! Ambient Matrix-rain state for the empty portion of the session list.
//!
//! The renderer owns the per-cell animation math; this module keeps the
//! semantic part small: incoming session events enqueue words that the TUI
//! renderer reveals by pinning letters when rain columns pass their target row.

use agentd_protocol::{SessionEvent, SessionState};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_ACTIVE_REVEALS: usize = 4;

/// Minimum gap between PTY-triggered reveal words for the same
/// session. PTY events arrive in bursts (many bytes per agent turn);
/// without throttling each chunk would queue a word and starve the
/// active-reveal cap.
const PTY_REVEAL_GAP: Duration = Duration::from_millis(3500);

/// Rotating activity words for PTY-driven harnesses (codex / claude
/// in interactive mode, shell). Zarvis already emits structured tool
/// events that map to richer words via `word_for_event` — these are
/// the fallback for harnesses whose only signal is "bytes happened".
const PTY_ACTIVITY_WORDS: &[&str] = &[
    "working", "thinking", "running", "writing", "reading", "typing",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashTone {
    Work,
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone)]
pub struct RevealWord {
    pub text: String,
    _tone: FlashTone,
    pub started: Instant,
    pub duration: Duration,
    pub x: f32,
    pub y: f32,
    priority: u8,
}

impl RevealWord {
    pub fn progress(&self, now: Instant) -> Option<f32> {
        let elapsed = now.checked_duration_since(self.started)?;
        if elapsed >= self.duration {
            return None;
        }
        Some(elapsed.as_secs_f32() / self.duration.as_secs_f32())
    }

    fn expired(&self, now: Instant) -> bool {
        now.checked_duration_since(self.started)
            .map(|elapsed| elapsed >= self.duration)
            .unwrap_or(false)
    }
}

#[derive(Debug, Default, Clone)]
pub struct MatrixRain {
    queue: Vec<RevealWord>,
    /// Last PTY-triggered reveal per session — used to rate-limit
    /// the heartbeat path so a single agent turn doesn't flood the
    /// reveal queue.
    pty_throttle: HashMap<String, Instant>,
    /// Monotonic counter so successive PTY heartbeats rotate through
    /// `PTY_ACTIVITY_WORDS` for visual variety.
    pty_word_cursor: u32,
}

impl MatrixRain {
    #[cfg(test)]
    pub fn active_reveal(&self, now: Instant) -> Option<&RevealWord> {
        self.active_reveals(now).max_by_key(|word| word.priority)
    }

    pub fn active_reveals(&self, now: Instant) -> impl Iterator<Item = &RevealWord> {
        self.queue
            .iter()
            .filter(move |word| word.progress(now).is_some())
    }

    pub fn observe_event(&mut self, event: &SessionEvent) {
        if let Some((text, tone, priority)) = word_for_event(event) {
            self.queue_random(text, tone, priority);
        }
    }

    /// Heartbeat from a PTY-only harness (codex / claude in
    /// interactive mode, shell). PTY adapters don't emit structured
    /// `ToolUse` / `Status` events while the agent is working, so
    /// without this the matrix rain reveals nothing for them. We
    /// rate-limit per session (one reveal per ~3.5s of byte traffic)
    /// and rotate through generic activity words so the rain still
    /// reflects that something is happening.
    pub fn observe_pty_activity(&mut self, session_id: &str, now: Instant) {
        if let Some(prev) = self.pty_throttle.get(session_id) {
            if now.duration_since(*prev) < PTY_REVEAL_GAP {
                return;
            }
        }
        self.pty_throttle.insert(session_id.to_string(), now);
        let idx = (self.pty_word_cursor as usize) % PTY_ACTIVITY_WORDS.len();
        self.pty_word_cursor = self.pty_word_cursor.wrapping_add(1);
        self.queue_random_at(PTY_ACTIVITY_WORDS[idx], FlashTone::Work, 25, now);
    }

    /// Forget per-session throttle state. Call when a session is
    /// reset, ends, or is deleted so the map doesn't grow unbounded
    /// and a future session reusing the id starts fresh.
    pub fn forget_session(&mut self, session_id: &str) {
        self.pty_throttle.remove(session_id);
    }

    pub fn observe_tool_decision(&mut self, decision: &str) {
        match decision {
            "approve" | "automode" => self.queue_random("approved", FlashTone::Good, 95),
            "deny" => self.queue_random("denied", FlashTone::Bad, 95),
            _ => {}
        }
    }

    fn queue_random(&mut self, text: &'static str, tone: FlashTone, priority: u8) {
        self.queue_random_at(text, tone, priority, Instant::now());
    }

    fn queue_random_at(&mut self, text: &'static str, tone: FlashTone, priority: u8, now: Instant) {
        let (x, y) = random_position(text, self.queue.len());
        self.queue_at(text, tone, x, y, priority, now);
    }

    pub fn queue(
        &mut self,
        text: impl Into<String>,
        tone: FlashTone,
        x: f32,
        y: f32,
        priority: u8,
    ) {
        self.queue_at(text, tone, x, y, priority, Instant::now());
    }

    fn queue_at(
        &mut self,
        text: impl Into<String>,
        tone: FlashTone,
        x: f32,
        y: f32,
        priority: u8,
        now: Instant,
    ) {
        self.queue.retain(|word| !word.expired(now));
        let duration = Duration::from_millis(12_000);
        self.queue.push(RevealWord {
            text: text.into(),
            _tone: tone,
            started: now,
            duration,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            priority,
        });
        while self.queue.len() > MAX_ACTIVE_REVEALS {
            if let Some((idx, _)) = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, word)| (word.priority, word.started))
            {
                self.queue.remove(idx);
            } else {
                break;
            }
        }
    }
}

fn random_position(text: &str, salt: usize) -> (f32, f32) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let seed = hash64(nanos ^ hash_text(text) ^ ((salt as u64) << 32));
    let x = 0.08 + unit_f32(seed) * 0.78;
    let y = 0.22 + unit_f32(hash64(seed)) * 0.66;
    (x, y)
}

fn unit_f32(seed: u64) -> f32 {
    ((seed >> 11) as f64 / ((1u64 << 53) as f64)) as f32
}

fn hash_text(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf29ce484222325, |acc, b| (acc ^ b as u64).wrapping_mul(0x100000001b3))
}

fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn word_for_event(event: &SessionEvent) -> Option<(&'static str, FlashTone, u8)> {
    match event {
        SessionEvent::ToolApprovalRequest { .. } => Some(("auth", FlashTone::Warn, 90)),
        SessionEvent::Error { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::Done { exit_code } if *exit_code == 0 => {
            Some(("worked", FlashTone::Good, 45))
        }
        SessionEvent::Done { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::Status { state, .. } => match state {
            SessionState::Running => Some(("working", FlashTone::Work, 20)),
            SessionState::AwaitingInput => Some(("waiting", FlashTone::Warn, 35)),
            SessionState::Done => Some(("worked", FlashTone::Good, 45)),
            SessionState::Errored => Some(("failed", FlashTone::Bad, 100)),
            SessionState::Pending | SessionState::Paused => None,
        },
        SessionEvent::ToolUse { tool, .. } => word_for_tool(tool),
        SessionEvent::TaskStart { tool, .. } => word_for_tool(tool),
        SessionEvent::TaskBackgrounded { .. } => Some(("background", FlashTone::Work, 40)),
        SessionEvent::TaskEnd { ok, .. } if *ok => Some(("worked", FlashTone::Good, 45)),
        SessionEvent::TaskEnd { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::ToolResult { ok: false, .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::AwaitingInput { .. } => Some(("waiting", FlashTone::Warn, 35)),
        SessionEvent::AgentStatus(status) if status.active => word_for_status(&status.status),
        SessionEvent::Reset => Some(("reset", FlashTone::Warn, 50)),
        SessionEvent::Message { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::Cost { .. }
        | SessionEvent::Diff { .. }
        | SessionEvent::Pty { .. }
        | SessionEvent::EditorState { .. }
        | SessionEvent::AgentStatus(_) => None,
    }
}

fn word_for_tool(tool: &str) -> Option<(&'static str, FlashTone, u8)> {
    if tool == agentd_protocol::TUI_DISPATCH_TOOL {
        return Some(("command", FlashTone::Work, 30));
    }
    match tool {
        "read_file"
        | "list_dir"
        | "find_files"
        | "agentd_get_session"
        | "agentd_get_transcript"
        | "agentd_get_output"
        | "agentd_get_diff"
        | "agentd_list_sessions"
        | "agentd_list_harnesses"
        | "agentd_get_tasks" => Some(("reading", FlashTone::Work, 55)),
        "write_file" | "edit_file" => Some(("editing", FlashTone::Work, 70)),
        "shell" => Some(("running", FlashTone::Work, 60)),
        "agentd_send_input" | "agentd_send_keys" => Some(("sending", FlashTone::Work, 65)),
        "agentd_create_session" => Some(("forking", FlashTone::Work, 60)),
        "agentd_pin_session"
        | "agentd_rename_session"
        | "agentd_set_session_group"
        | "agentd_move_session" => Some(("routing", FlashTone::Work, 45)),
        "agentd_interrupt_session"
        | "agentd_stop_session"
        | "agentd_kill_session"
        | "agentd_delete_session" => Some(("blocked", FlashTone::Warn, 85)),
        "agentd_loop_create" | "agentd_loop_update" | "agentd_loop_remove" => {
            Some(("looping", FlashTone::Work, 55))
        }
        _ => Some(("working", FlashTone::Work, 20)),
    }
}

fn word_for_status(status: &str) -> Option<(&'static str, FlashTone, u8)> {
    let s = status.to_ascii_lowercase();
    if s.contains("edit") || s.contains("patch") || s.contains("write") {
        Some(("editing", FlashTone::Work, 70))
    } else if s.contains("read") || s.contains("scan") || s.contains("search") {
        Some(("reading", FlashTone::Work, 55))
    } else if s.contains("test") || s.contains("run") || s.contains("shell") {
        Some(("running", FlashTone::Work, 60))
    } else if s.contains("wait") {
        Some(("waiting", FlashTone::Warn, 35))
    } else if s.contains("plan") || s.contains("think") {
        Some(("thinking", FlashTone::Work, 30))
    } else if s.trim().is_empty() {
        None
    } else {
        Some(("working", FlashTone::Work, 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{MessageRole, ToolRisk};

    #[test]
    fn maps_tool_events_to_words() {
        let ev = SessionEvent::ToolUse {
            tool: "edit_file".to_string(),
            args: serde_json::json!({}),
        };
        assert_eq!(word_for_event(&ev).map(|w| w.0), Some("editing"));
    }

    #[test]
    fn higher_priority_flash_wins() {
        let mut rain = MatrixRain::default();
        rain.observe_event(&SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });
        rain.observe_event(&SessionEvent::ToolApprovalRequest {
            call_id: "c".into(),
            tool: "shell".into(),
            args_summary: "x".into(),
            risk: ToolRisk::Risky,
        });
        rain.observe_event(&SessionEvent::Message {
            role: MessageRole::Assistant,
            text: "low signal".into(),
        });
        assert_eq!(
            rain.active_reveal(Instant::now()).map(|f| f.text.as_str()),
            Some("auth")
        );
    }

    #[test]
    fn queue_sets_target_position() {
        let mut rain = MatrixRain::default();
        rain.queue("matrix", FlashTone::Work, 0.2, 0.8, 10);
        let reveal = rain.active_reveal(Instant::now()).expect("reveal word");
        assert_eq!(reveal.text, "matrix");
        assert_eq!(reveal.x, 0.2);
        assert_eq!(reveal.y, 0.8);
    }

    #[test]
    fn multiple_reveals_can_be_active_together() {
        let mut rain = MatrixRain::default();
        rain.queue("working", FlashTone::Work, 0.2, 0.4, 10);
        rain.queue("worked", FlashTone::Good, 0.6, 0.7, 20);

        let active: Vec<_> = rain
            .active_reveals(Instant::now())
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(active, vec!["working", "worked"]);
    }

    #[test]
    fn active_reveal_reports_highest_priority_word() {
        let mut rain = MatrixRain::default();
        rain.queue("working", FlashTone::Work, 0.2, 0.4, 10);
        rain.queue("failed", FlashTone::Bad, 0.6, 0.7, 100);

        assert_eq!(
            rain.active_reveal(Instant::now()).map(|word| word.text.as_str()),
            Some("failed")
        );
    }

    #[test]
    fn random_position_stays_inside_comfortable_band() {
        for salt in 0..256 {
            let (x, y) = random_position("matrix", salt);
            assert!((0.08..=0.86).contains(&x));
            assert!((0.22..=0.88).contains(&y));
        }
    }

    #[test]
    fn pty_activity_queues_word_on_first_call() {
        // Codex / claude / shell emit only PTY events while the
        // agent is working — without this path the matrix rain has
        // nothing to reveal for them.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", now);
        let word = rain.active_reveal(now).map(|w| w.text.clone());
        assert!(
            word.as_deref().map(|w| PTY_ACTIVITY_WORDS.contains(&w)).unwrap_or(false),
            "expected one of {PTY_ACTIVITY_WORDS:?}, got {word:?}"
        );
    }

    #[test]
    fn pty_activity_throttles_repeated_calls_within_gap() {
        // A burst of PTY events from a single session should produce
        // exactly one reveal, not one per byte chunk.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", now);
        rain.observe_pty_activity("sess-a", now + Duration::from_millis(100));
        rain.observe_pty_activity("sess-a", now + Duration::from_millis(1000));
        let count = rain.active_reveals(now + Duration::from_millis(1100)).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn pty_activity_unblocks_after_gap_and_rotates_word() {
        // Past PTY_REVEAL_GAP a second reveal is allowed, and the
        // rotation cursor should pick a different word so the
        // animation doesn't look stuck.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", now);
        let first = rain
            .active_reveal(now)
            .map(|w| w.text.clone())
            .expect("first reveal");
        let later = now + PTY_REVEAL_GAP + Duration::from_millis(10);
        rain.observe_pty_activity("sess-a", later);
        let texts: Vec<_> = rain
            .active_reveals(later)
            .map(|w| w.text.clone())
            .collect();
        assert_eq!(texts.len(), 2);
        assert_ne!(texts[0], texts[1], "consecutive reveals should rotate");
        // Sanity: the first word is still one of the rotation words.
        assert!(PTY_ACTIVITY_WORDS.contains(&first.as_str()));
    }

    #[test]
    fn pty_activity_per_session_throttle_is_independent() {
        // Two different sessions can each get their own reveal
        // within the gap window — the throttle is per-session.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", now);
        rain.observe_pty_activity("sess-b", now);
        assert_eq!(rain.active_reveals(now).count(), 2);
    }

    #[test]
    fn forget_session_resets_throttle() {
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", now);
        rain.forget_session("sess-a");
        rain.observe_pty_activity("sess-a", now + Duration::from_millis(50));
        assert_eq!(rain.active_reveals(now + Duration::from_millis(50)).count(), 2);
    }
}
