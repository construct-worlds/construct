//! PTY-backed adapter runtime.
//!
//! Helper that spawns a child under a real PTY, pumps bytes between
//! `portable-pty` and the adapter's [`AdapterContext`], and emits
//! [`SessionEvent::Pty`] / lifecycle events on the adapter's behalf.
//!
//! Available behind the `pty` feature of `agentd-protocol`.

use super::{AdapterContext, AdapterInboxMsg};
use crate::{PtySize, SessionEvent, SessionState};
use portable_pty::{native_pty_system, CommandBuilder};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const READ_BUF: usize = 8 * 1024;
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

/// Answers terminal queries that require a live terminal counterpart.
///
/// The child PTY cannot talk directly to the user's outer terminal: Construct
/// sits between them and renders the byte stream itself. Cursor-position
/// reports therefore have to be answered here, where every client surface
/// (native TUI, web, detached/no client) shares the same live-only behavior.
/// Handled queries are removed from the emitted stream so downstream terminal
/// emulators cannot answer a second time and persisted replay stays inert.
struct TerminalQueryResponder {
    parser: vt100::Parser,
    tail: Vec<u8>,
}

impl TerminalQueryResponder {
    fn new(size: PtySize) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows.max(2), size.cols.max(2), 0),
            tail: Vec::new(),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows.max(2), cols.max(2));
    }

    /// Feed one child-output chunk, returning bytes safe to emit/persist and
    /// terminal replies to write directly back to the child PTY.
    fn feed(&mut self, chunk: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut combined = std::mem::take(&mut self.tail);
        combined.extend_from_slice(chunk);

        let mut passthrough = Vec::with_capacity(combined.len());
        let mut responses = Vec::new();
        let mut parsed = 0usize;
        let mut i = 0usize;
        while i < combined.len() {
            let rest = &combined[i..];
            if rest.starts_with(CURSOR_POSITION_QUERY) {
                self.parser.process(&passthrough[parsed..]);
                parsed = passthrough.len();
                let (row, col) = self.parser.screen().cursor_position();
                responses.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
                i += CURSOR_POSITION_QUERY.len();
                continue;
            }
            if is_cursor_position_query_prefix(rest) {
                self.tail.extend_from_slice(rest);
                break;
            }
            passthrough.push(combined[i]);
            i += 1;
        }
        self.parser.process(&passthrough[parsed..]);
        (passthrough, responses)
    }

    /// Flush bytes held only because they might have begun a split query.
    fn finish(&mut self) -> Vec<u8> {
        let tail = std::mem::take(&mut self.tail);
        self.parser.process(&tail);
        tail
    }
}

fn is_cursor_position_query_prefix(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() < CURSOR_POSITION_QUERY.len()
        && CURSOR_POSITION_QUERY.starts_with(bytes)
}

/// What to spawn under the PTY and how.
pub struct PtySpec {
    pub bin: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub size: PtySize,
    /// Free-form label that's emitted in the initial Status event's `detail`.
    pub status_detail: Option<String>,
    /// Detect prompt-vs-busy from the PTY's foreground process group: when the
    /// terminal's foreground group is the child's own group the child is at its
    /// prompt (`AwaitingInput`); when a launched command holds the foreground a
    /// command is `Running`. Only meaningful for line-oriented shells — a
    /// full-screen TUI child holds the foreground group for its whole lifetime,
    /// so leave this `false` for those and rely on daemon-side quiescence.
    pub detect_prompt_via_pgroup: bool,
}

/// Drive a PTY-backed session. Emits `Status(Running)` → byte stream
/// (`Pty` events) → `Done`. Honors `PtyInput`, `PtyResize`, `Interrupt`,
/// `Stop`, and line-oriented `Input` (appended with `\n`).
///
/// Returns the child's exit code (or `-1` if not available).
pub async fn run_session(spec: PtySpec, ctx: AdapterContext) -> i32 {
    run_session_inner(spec, ctx, None).await
}

/// Drive a PTY-backed session and report the spawned child's process id.
///
/// This lets adapters bind native session metadata that records the wrapped
/// process id without guessing from global file modification order. Failure
/// to spawn drops the sender.
pub async fn run_session_with_pid(
    spec: PtySpec,
    ctx: AdapterContext,
    spawned_pid: oneshot::Sender<Option<u32>>,
) -> i32 {
    run_session_inner(spec, ctx, Some(spawned_pid)).await
}

async fn run_session_inner(
    spec: PtySpec,
    ctx: AdapterContext,
    spawned_pid: Option<oneshot::Sender<Option<u32>>>,
) -> i32 {
    let AdapterContext {
        session_id: _,
        emit,
        mut inbox,
    } = ctx;

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(portable_pty::PtySize {
        cols: spec.size.cols,
        rows: spec.size.rows,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("openpty: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 127 });
            return 127;
        }
    };

    let mut cmd = CommandBuilder::new(&spec.bin);
    for a in &spec.args {
        cmd.arg(a);
    }
    cmd.cwd(&spec.cwd);
    cmd.env(
        "TERM",
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    );
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let io_err = std::io::Error::other(e.to_string());
            emit.emit(SessionEvent::Error {
                message: super::missing_bin_hint(
                    &spec.status_detail.as_deref().unwrap_or(&spec.bin),
                    &io_err,
                ),
            });
            emit.emit(SessionEvent::Done { exit_code: 127 });
            return 127;
        }
    };

    let mut killer = child.clone_killer();
    // Captured for foreground-process-group prompt detection. portable-pty puts
    // the child in its own session/group as leader, so its pid is its pgid.
    let child_pid = child.process_id();
    if let Some(spawned_pid) = spawned_pid {
        let _ = spawned_pid.send(child_pid);
    }
    let master = pair.master;
    let slave = pair.slave;

    let reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("pty reader: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 1 });
            return 1;
        }
    };
    let writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("pty writer: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 1 });
            return 1;
        }
    };

    let (read_tx, mut read_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if read_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(bytes) = write_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let mut wait_handle = tokio::task::spawn_blocking(move || {
        let _slave_alive = slave;
        let mut child = child;
        child.wait()
    });

    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: spec.status_detail.clone(),
    });

    // Foreground-process-group prompt detection (shells only). When the
    // terminal's foreground group is the child's own group the shell is at its
    // prompt (AwaitingInput); a launched command's group means Running. The
    // first tick fires ~immediately, reflecting the freshly-spawned prompt.
    let mut pgrp_timer = tokio::time::interval(Duration::from_millis(400));
    pgrp_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pgrp_state: Option<SessionState> = None;
    let mut terminal_queries = TerminalQueryResponder::new(spec.size);

    let mut read_closed = false;
    let mut inbox_closed = false;
    let exit_code: i32;
    loop {
        tokio::select! {
            biased;
            bytes = read_rx.recv(), if !read_closed => {
                match bytes {
                    Some(b) => {
                        let (passthrough, responses) = terminal_queries.feed(&b);
                        for response in responses {
                            let _ = write_tx.send(response);
                        }
                        if !passthrough.is_empty() {
                            emit.emit_pty(&passthrough);
                        }
                    }
                    None => {
                        read_closed = true;
                        let tail = terminal_queries.finish();
                        if !tail.is_empty() {
                            emit.emit_pty(&tail);
                        }
                    }
                }
            }
            msg = inbox.recv(), if !inbox_closed => {
                match msg {
                    None => {
                        // Inbox closed = the adapter runtime is exiting. The
                        // child would die anyway once process exit closes the
                        // PTY master (SIGHUP), but only after the runtime's
                        // session-drain timeout expires — kill it now so
                        // teardown is prompt.
                        inbox_closed = true;
                        let _ = killer.kill();
                    }
                    Some(AdapterInboxMsg::PtyInput(b)) => {
                        let _ = write_tx.send(b);
                    }
                    Some(AdapterInboxMsg::PtyResize { cols, rows }) => {
                        let _ = master.resize(portable_pty::PtySize {
                            cols, rows,
                            pixel_width: 0, pixel_height: 0,
                        });
                        terminal_queries.resize(cols, rows);
                    }
                    Some(AdapterInboxMsg::Input(text)) => {
                        let mut b = text.into_bytes();
                        if !b.ends_with(b"\n") { b.push(b'\n'); }
                        let _ = write_tx.send(b);
                    }
                    Some(AdapterInboxMsg::Interrupt) => {
                        // ETX → child's SIGINT path.
                        let _ = write_tx.send(vec![0x03]);
                    }
                    Some(AdapterInboxMsg::Stop) => {
                        let _ = killer.kill();
                    }
                    // PTY-mode adapters don't gate tool calls — ignore.
                    Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => {}
                }
            }
            _ = pgrp_timer.tick(), if spec.detect_prompt_via_pgroup => {
                let desired = match (master.process_group_leader(), child_pid) {
                    (Some(fg), Some(pid)) if fg == pid as i32 => SessionState::AwaitingInput,
                    (Some(_), Some(_)) => SessionState::Running,
                    // Unknown (race during spawn/exit) → don't flip.
                    _ => continue,
                };
                if pgrp_state != Some(desired) {
                    pgrp_state = Some(desired);
                    emit.emit(SessionEvent::Status { state: desired, detail: None });
                }
            }
            res = &mut wait_handle => {
                exit_code = match res {
                    Ok(Ok(status)) => {
                        if status.success() { 0 } else { status.exit_code() as i32 }
                    }
                    _ => -1,
                };
                while let Ok(b) = read_rx.try_recv() {
                    let (passthrough, _) = terminal_queries.feed(&b);
                    if !passthrough.is_empty() {
                        emit.emit_pty(&passthrough);
                    }
                }
                let tail = terminal_queries.finish();
                if !tail.is_empty() {
                    emit.emit_pty(&tail);
                }
                break;
            }
        }
    }
    emit.emit(SessionEvent::Done { exit_code });
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn responder() -> TerminalQueryResponder {
        TerminalQueryResponder::new(PtySize { cols: 80, rows: 24 })
    }

    #[test]
    fn cursor_position_query_is_answered_and_not_emitted() {
        let mut responder = responder();
        let (output, responses) = responder.feed(b"hello\r\nxy\x1b[6nworld");

        assert_eq!(output, b"hello\r\nxyworld");
        assert_eq!(responses, vec![b"\x1b[2;3R".to_vec()]);
        assert!(!output
            .windows(CURSOR_POSITION_QUERY.len())
            .any(|window| window == CURSOR_POSITION_QUERY));
    }

    #[test]
    fn split_cursor_position_query_waits_for_completion() {
        let mut responder = responder();

        let (first, responses) = responder.feed(b"abc\x1b[");
        assert_eq!(first, b"abc");
        assert!(responses.is_empty());

        let (second, responses) = responder.feed(b"6n");
        assert!(second.is_empty());
        assert_eq!(responses, vec![b"\x1b[1;4R".to_vec()]);
    }

    #[test]
    fn false_query_prefix_is_preserved() {
        let mut responder = responder();

        let (first, responses) = responder.feed(b"before\x1b[");
        assert_eq!(first, b"before");
        assert!(responses.is_empty());

        let (second, responses) = responder.feed(b"2Jafter");
        assert_eq!(second, b"\x1b[2Jafter");
        assert!(responses.is_empty());
    }

    #[test]
    fn incomplete_query_prefix_is_preserved_at_end_of_stream() {
        let mut responder = responder();

        let (output, responses) = responder.feed(b"before\x1b[");
        assert_eq!(output, b"before");
        assert!(responses.is_empty());
        assert_eq!(responder.finish(), b"\x1b[");
    }

    #[test]
    fn each_query_observes_cursor_at_its_stream_position() {
        let mut responder = responder();
        let (output, responses) = responder.feed(b"a\x1b[6n\r\nbc\x1b[6n");

        assert_eq!(output, b"a\r\nbc");
        assert_eq!(
            responses,
            vec![b"\x1b[1;2R".to_vec(), b"\x1b[2;3R".to_vec()]
        );
    }
}
