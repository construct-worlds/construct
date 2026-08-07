use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use construct_protocol::adapter::{AdapterInboxMsg, EventEmitter};
use construct_protocol::SessionEvent;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;

pub mod context_breakdown;
mod transcript_paths;
pub use transcript_paths::{
    antigravity_conversation_dir, antigravity_transcript_path, claude_project_slug,
    claude_transcript_path, codex_sessions_root, codex_transcript_path, grok_session_dir,
    grok_transcript_path,
};

#[derive(Debug)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Stopped,
}

pub fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}

pub async fn drive_turn(
    child: &mut tokio::process::Child,
    inbox: &mut mpsc::Receiver<AdapterInboxMsg>,
    emit: &EventEmitter,
    pending: &mut VecDeque<String>,
) -> TurnOutcome {
    loop {
        tokio::select! {
            biased;
            msg = inbox.recv() => {
                match msg {
                    None => {
                        let _ = child.start_kill();
                        return TurnOutcome::Stopped;
                    }
                    Some(AdapterInboxMsg::Stop) => {
                        let _ = child.start_kill();
                        return TurnOutcome::Stopped;
                    }
                    Some(AdapterInboxMsg::Interrupt) => {
                        let _ = child.start_kill();
                        return TurnOutcome::Interrupted;
                    }
                    Some(AdapterInboxMsg::Input(t)) => {
                        emit.log(format!("queued input for next turn: {}", short(&t, 60)));
                        pending.push_back(t);
                    }
                    Some(AdapterInboxMsg::PtyInput(_))
                    | Some(AdapterInboxMsg::PtyResize { .. })
                    | Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => {}
                }
            }
            _ = child.wait() => {
                return TurnOutcome::Completed;
            }
        }
    }
}

pub fn spawn_stderr_log<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit.log(format!("stderr: {line}"));
        }
    })
}

/// How many trailing stderr lines [`StderrTail`] retains for launch-failure
/// reporting. A fatal CLI/config error is almost always in the last few
/// lines; the bound keeps a chatty harness from growing the buffer unbounded.
const STDERR_TAIL_LINES: usize = 20;

/// Bounded tail of a harness child's stderr, shared between the stderr
/// logging task and the adapter's turn loop so a silent launch failure can be
/// surfaced with its actual cause instead of only landing in the daemon log.
#[derive(Clone, Default)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() >= STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }
}

/// Like [`spawn_stderr_log`], but also retains the last few stderr lines so
/// the turn loop can report them if the child turns out to have died at
/// launch.
pub fn spawn_stderr_tail<R>(
    reader: R,
    emit: EventEmitter,
) -> (tokio::task::JoinHandle<()>, StderrTail)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let tail = StderrTail::default();
    let task_tail = tail.clone();
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit.log(format!("stderr: {line}"));
            task_tail.push(line);
        }
    });
    (task, tail)
}

/// Human-readable message for a harness child that exited abnormally without
/// producing any session output.
pub fn launch_failure_message(
    status: Option<&std::process::ExitStatus>,
    stderr_tail: &[String],
) -> String {
    let status = match status {
        Some(s) => s.to_string(),
        None => "unknown exit status".to_string(),
    };
    let mut message = format!("harness exited during launch ({status}) without producing output");
    if stderr_tail.is_empty() {
        message.push_str("; no stderr captured (see daemon log)");
    } else {
        message.push_str(":\n");
        message.push_str(&stderr_tail.join("\n"));
    }
    message
}

/// Surface a harness child that failed at launch as a session error.
///
/// A wrapper harness handed a flag or config it rejects exits non-zero
/// before emitting a single session event; without this the session just
/// flips back to awaiting-input with an empty transcript and looks hung,
/// while the real error sits in the adapter's stderr log. Call this after
/// the turn's child has been reaped, with the emitter's
/// [`EventEmitter::events_emitted`] count snapshotted just before spawn:
/// if the child failed and nothing was emitted since, this emits a
/// [`SessionEvent::Error`] carrying the captured stderr tail. Returns
/// whether it emitted. Only call it for turns that ran to completion —
/// an interrupted or stopped turn is killed by us and would misreport
/// the kill as a launch failure.
pub fn emit_launch_failure_if_silent(
    emit: &EventEmitter,
    events_before_spawn: u64,
    status: Option<&std::process::ExitStatus>,
    stderr_tail: &[String],
) -> bool {
    let failed = status.map(|s| !s.success()).unwrap_or(false);
    if !failed || emit.events_emitted() > events_before_spawn {
        return false;
    }
    emit.emit(SessionEvent::Error {
        message: launch_failure_message(status, stderr_tail),
    });
    true
}

/// Post-incrementing counter for native-subagent emission ordinals
/// (`SessionEvent::NativeSubagent::seq`): returns the current ordinal and
/// advances it. Adapters number every emission derived from a child's own
/// transcript file with these, per child, starting from 0 at watcher start —
/// a re-scan from the top regenerates the same ordinals, which is what lets
/// the daemon drop already-projected replays while adapters always backfill
/// full child history.
pub fn next_native_seq(ord: &mut u64) -> u64 {
    let v = *ord;
    *ord += 1;
    v
}

#[cfg(test)]
mod launch_failure_tests {
    use super::*;
    use std::process::Stdio;
    use tokio::process::Command;

    /// Run a shell one-liner the way headless adapters run a harness child:
    /// stderr tailed, turn driven to completion, exit status captured.
    async fn run_child(
        script: &str,
        emit: &EventEmitter,
    ) -> (TurnOutcome, Option<std::process::ExitStatus>, StderrTail) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let (stderr_task, tail) =
            spawn_stderr_tail(child.stderr.take().expect("piped"), emit.clone());
        // Keep the sender alive so the inbox pends instead of signalling Stop.
        let (_inbox_tx, mut inbox) = mpsc::channel::<AdapterInboxMsg>(8);
        let mut pending = VecDeque::new();
        let outcome = drive_turn(&mut child, &mut inbox, emit, &mut pending).await;
        let _ = stderr_task.await;
        let status = child.wait().await.ok();
        (outcome, status, tail)
    }

    fn error_messages(rx: &mut mpsc::UnboundedReceiver<serde_json::Value>) -> Vec<String> {
        let mut errors = Vec::new();
        while let Ok(v) = rx.try_recv() {
            let Some(event) = v.pointer("/params/event") else {
                continue;
            };
            if event.get("type").and_then(|t| t.as_str()) == Some("error") {
                let msg = event
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                errors.push(msg.to_string());
            }
        }
        errors
    }

    /// Regression test for construct-worlds/construct#1208: a harness child
    /// that rejects its CLI flags and dies before emitting anything used to
    /// leave the transcript empty (the error only reached the daemon log via
    /// `emit.log`), so the session looked hung at awaiting-input. The failure
    /// must surface as a `SessionEvent::Error` carrying the child's stderr.
    #[tokio::test]
    async fn silent_launch_failure_surfaces_stderr_as_session_error() {
        let (emit, mut rx) = EventEmitter::channel("session");
        let baseline = emit.events_emitted();
        let (outcome, status, tail) = run_child(
            "echo 'Error: --allow \"MultiEdit(...)\": unknown tool prefix: MultiEdit' >&2; exit 2",
            &emit,
        )
        .await;

        assert!(matches!(outcome, TurnOutcome::Completed));
        assert!(emit_launch_failure_if_silent(
            &emit,
            baseline,
            status.as_ref(),
            &tail.snapshot(),
        ));

        let errors = error_messages(&mut rx);
        assert_eq!(errors.len(), 1, "exactly one error event, got {errors:?}");
        assert!(
            errors[0].contains("unknown tool prefix: MultiEdit"),
            "error must carry the child's stderr: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("harness exited during launch"),
            "error must name the failure mode: {}",
            errors[0]
        );
    }

    #[tokio::test]
    async fn successful_exit_is_not_a_launch_failure() {
        let (emit, mut rx) = EventEmitter::channel("session");
        let baseline = emit.events_emitted();
        let (_outcome, status, tail) = run_child("exit 0", &emit).await;

        assert!(!emit_launch_failure_if_silent(
            &emit,
            baseline,
            status.as_ref(),
            &tail.snapshot(),
        ));
        assert!(error_messages(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn failure_after_real_output_is_not_a_launch_failure() {
        // A harness that produced session output and then exited non-zero is
        // a failed turn, not a launch failure — the transcript already shows
        // what happened, so no synthetic error is added.
        let (emit, mut rx) = EventEmitter::channel("session");
        let baseline = emit.events_emitted();
        emit.emit(SessionEvent::Message {
            role: construct_protocol::MessageRole::Assistant,
            text: "partial output".into(),
        });
        let (_outcome, status, tail) = run_child("echo boom >&2; exit 1", &emit).await;

        assert!(!emit_launch_failure_if_silent(
            &emit,
            baseline,
            status.as_ref(),
            &tail.snapshot(),
        ));
        assert!(error_messages(&mut rx)
            .iter()
            .all(|m| !m.contains("harness exited during launch")));
    }

    #[test]
    fn launch_failure_message_without_stderr_points_at_the_daemon_log() {
        let message = launch_failure_message(None, &[]);
        assert!(message.contains("no stderr captured"));
        assert!(message.contains("daemon log"));
    }

    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_last_lines() {
        let tail = StderrTail::default();
        for i in 0..100 {
            tail.push(format!("line {i}"));
        }
        let snapshot = tail.snapshot();
        assert_eq!(snapshot.len(), STDERR_TAIL_LINES);
        assert_eq!(snapshot.last().map(String::as_str), Some("line 99"));
    }
}
