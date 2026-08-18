use super::*;
use std::sync::Weak;

/// Capacity of a session's ordered PTY-input delivery queue, counted in
/// input batches (one keystroke burst or one paste each), not bytes.
/// Interactive typing never comes close; the queue only fills when an
/// adapter stops ACKing `session.pty_input` for a long stretch, and then
/// failing the enqueue with a visible error beats silently buffering
/// unbounded input into a wedged session.
const PTY_INPUT_QUEUE_CAP: usize = 256;

/// One queued PTY-input batch (spec 0087). `ack` is present when the
/// enqueuer needs delivery confirmation — daemon-internal callers that
/// pace themselves against the adapter (bracketed-paste-then-Enter
/// submission, OSC 11 responses). The interactive typing path leaves it
/// `None`: "accepted into the ordered queue" is its whole contract.
pub(crate) struct PtyInputJob {
    bytes: Vec<u8>,
    ack: Option<tokio::sync::oneshot::Sender<Result<()>>>,
}

impl SessionManager {
    /// Interactive typing path (`server::dispatch`'s `SESSION_PTY_INPUT`).
    /// Returns once the bytes are accepted into the session's ordered
    /// delivery queue — NOT once the adapter ACKs delivery (spec 0087).
    /// The dispatch loop serves each connection's requests serially, and
    /// clients pump keystrokes one request at a time, so awaiting the
    /// adapter round-trip here let a single slow/starved adapter stall
    /// typing into every session plus everything else queued on the
    /// connection. Delivery failures after enqueue are logged, not
    /// returned; typing into a session with no live adapter still fails
    /// synchronously, while also closing it so the client can restart it.
    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, true, false).await
    }

    /// Like [`Self::pty_input`] (transcript capture included) but waits
    /// for the adapter to ACK delivery. For daemon-internal prompt
    /// submission (playbook runs) whose follow-up bookkeeping should only
    /// happen once the input has actually reached the harness.
    pub(crate) async fn pty_input_delivered(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, true, true).await
    }

    /// Delivery-ACKed input without transcript capture, for daemon-internal
    /// byte streams that must not pollute the user transcript (OSC 11
    /// responses, bracketed-paste submission). Waits for the adapter ACK:
    /// its callers sequence real-time behavior against delivery (e.g. the
    /// paste → settle-delay → Enter submission dance).
    pub(crate) async fn pty_input_without_capture(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, false, true).await
    }

    async fn pty_input_inner(
        &self,
        id: &str,
        bytes: Vec<u8>,
        capture: bool,
        await_delivery: bool,
    ) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Capture submitted PTY lines before forwarding them. Some interactive
        // harnesses do not echo user text as structured `Message` events, so
        // chat-mode transcript history otherwise loses those turns.
        if capture {
            let input_lines = self.capture_pty_input_lines(&entry, &bytes).await;
            let harness = entry.summary.read().await.harness.clone();
            for line in input_lines {
                if should_record_pty_user_message(&harness) {
                    self.handle_event(
                        &entry,
                        SessionEvent::Message {
                            role: MessageRole::User,
                            text: line,
                        },
                    )
                    .await;
                } else if !entry.title_gen_attempted.load(Ordering::SeqCst)
                    && line.chars().count() >= 2
                {
                    self.maybe_spawn_auto_title(entry.clone(), line).await;
                }
            }
        }
        // Typing into a session whose adapter is gone must still fail
        // synchronously: once the enqueue-ACK path returns Ok there is no
        // response channel left to report a delivery error through. Best
        // effort — the adapter can still die between this check and
        // delivery, which the writer then logs.
        self.live_adapter_or_mark_closed(&entry).await?;
        if await_delivery {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.enqueue_pty_input(
                &entry,
                PtyInputJob {
                    bytes,
                    ack: Some(tx),
                },
            )?;
            rx.await
                .map_err(|_| anyhow!("session closed before pty input was delivered"))?
        } else {
            self.enqueue_pty_input(&entry, PtyInputJob { bytes, ack: None })
        }
    }

    /// Accept `job` into `entry`'s ordered delivery queue, lazily spawning
    /// the per-session writer task on first use. All input producers —
    /// interactive typing, playbook submission, OSC 11 responses — funnel
    /// through this one queue, so per-session byte order is preserved no
    /// matter which mix of paths is active. Sync (never awaits): callers
    /// hold no locks and the dispatch loop is never delayed here.
    fn enqueue_pty_input(&self, entry: &Arc<SessionEntry>, job: PtyInputJob) -> Result<()> {
        let mut slot = entry
            .pty_input_queue
            .lock()
            .expect("pty_input_queue mutex poisoned");
        if slot.is_none() {
            let (tx, rx) = mpsc::channel::<PtyInputJob>(PTY_INPUT_QUEUE_CAP);
            tokio::spawn(pty_input_writer(Arc::downgrade(entry), rx));
            *slot = Some(tx);
        }
        slot.as_ref()
            .expect("sender installed above")
            .try_send(job)
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => anyhow!(
                    "pty input backlogged: the session's adapter is not consuming input"
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow!("session closed before pty input could be queued")
                }
            })
    }

    /// Feed PTY-input bytes through a minimal terminal-input parser (printable
    /// ASCII + backspace + CR/LF; CSI/SS3 sequences skipped) and return every
    /// submitted non-empty line. The parser is intentionally small: it is for
    /// transcript/user-title capture, not full terminal editing semantics.
    async fn capture_pty_input_lines(
        &self,
        entry: &Arc<SessionEntry>,
        bytes: &[u8],
    ) -> Vec<String> {
        let mut cap = entry.pty_input_capture.lock().await;
        let mut lines = Vec::new();
        for &b in bytes {
            match cap.esc {
                0 => match b {
                    b'\n' if cap.last_was_cr => {
                        cap.last_was_cr = false;
                    }
                    b'\r' | b'\n' => {
                        let s = cap.buf.trim().to_string();
                        cap.last_was_cr = b == b'\r';
                        cap.buf.clear();
                        if s.chars().count() >= 2 {
                            lines.push(s);
                        }
                    }
                    0x1b => cap.esc = 1,
                    0x08 | 0x7f => {
                        cap.last_was_cr = false;
                        cap.buf.pop();
                    }
                    _ if (0x20..0x7f).contains(&b) => {
                        cap.last_was_cr = false;
                        cap.buf.push(b as char);
                    }
                    _ => {
                        cap.last_was_cr = false;
                    }
                },
                1 => match b {
                    b'[' => cap.esc = 2,
                    b'O' => cap.esc = 3,
                    _ => cap.esc = 0,
                },
                2 => {
                    // CSI: parameter bytes + final byte in `@`..=`~`.
                    if (0x40..=0x7e).contains(&b) {
                        cap.esc = 0;
                    }
                }
                3 => {
                    // SS3: one byte.
                    cap.esc = 0;
                }
                _ => cap.esc = 0,
            }
        }
        lines
    }

    /// Record input or a viewport report from one client connection.
    ///
    /// Input and claiming resize reports move ownership to that exact
    /// connection. Passive resize reports update the connection's remembered
    /// viewport, but only reach the OS PTY while that connection is already
    /// the owner. This prevents delayed browser layout work from stealing
    /// geometry after another TUI/browser receives user input.
    ///
    /// This is the daemon-side half of the explicit-engagement policy. The
    /// complementary half lives in `server::dispatch`, which marks PTY input
    /// as a claim and passes through the resize request's explicit claim bit.
    pub async fn note_pty_activity(
        self: &Arc<Self>,
        id: &str,
        conn_id: u64,
        kind: crate::server::ClientKind,
        resize_to: Option<(u16, u16)>,
        claim: bool,
    ) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let (to_apply, switched) = {
            let mut policy = entry
                .pty_client_policy
                .lock()
                .expect("pty_client_policy mutex poisoned");
            policy.note(conn_id, kind, resize_to, claim)
        };
        let mut resized = false;
        if let Some((cols, rows)) = to_apply {
            // The pty_resize dedup handles the case where the OS PTY is
            // already at this size.
            resized = self
                .pty_resize_from_connection(id, cols, rows, Some(conn_id))
                .await?;
        }
        if switched && !resized {
            // Ownership moved without the geometry changing (same size, or
            // the new owner has no remembered viewport yet). Peers tracking
            // ownership through personalized resize events still need to see
            // the handoff — a viewer that missed it would keep treating its
            // own remembered grid as authoritative and stop following.
            let size = entry.pty.lock().await.size;
            if let Some(size) = size {
                let payload = construct_protocol::EventNotificationPayload {
                    session_id: id.to_string(),
                    at: chrono::Utc::now(),
                    event: SessionEvent::PtyResize {
                        cols: size.cols,
                        rows: size.rows,
                        owner: false,
                    },
                    seq: 0,
                };
                let _ = self.broadcast.send(super::BroadcastMsg::PtyResize {
                    payload,
                    owner_conn_id: conn_id,
                });
            }
        }
        Ok(())
    }

    /// Forget one disconnected client's remembered viewports. If it owned a
    /// session, leave that session temporarily ownerless; the next explicit
    /// input/click establishes a new owner without guessing which passive
    /// viewer should win.
    pub async fn clear_pty_client(&self, conn_id: u64) {
        let entries: Vec<Arc<SessionEntry>> =
            self.sessions.read().await.values().cloned().collect();
        for entry in entries {
            let mut policy = entry
                .pty_client_policy
                .lock()
                .expect("pty_client_policy mutex poisoned");
            policy.clients.remove(&conn_id);
            if policy.owner == Some(conn_id) {
                policy.owner = None;
            }
        }
    }

    pub async fn pty_resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        self.pty_resize_from_connection(id, cols, rows, None)
            .await
            .map(|_| ())
    }

    /// Returns whether the PTY size actually changed (and was therefore
    /// broadcast); a same-size call dedups and returns `Ok(false)`.
    async fn pty_resize_from_connection(
        &self,
        id: &str,
        cols: u16,
        rows: u16,
        owner_conn_id: Option<u64>,
    ) -> Result<bool> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = PtySize { cols, rows };
        // Dedup: if the adapter's PTY is already at this size, skip
        // the SIGWINCH. A no-op resize on a normal-screen TUI like
        // codex still causes the child to redraw its viewport (which
        // for codex means re-emitting its full transcript), so every
        // spurious resize looks like a "history replay" to the user.
        // Sources of spurious resizes: TUI bootstrap calling
        // `pty_resize` with the same dims it already sent, and
        // multiple SIGWINCH'd frames during a terminal-window drag
        // that all land on the same final size.
        {
            let mut pty = entry.pty.lock().await;
            if pty.size == Some(size) {
                return Ok(false);
            }
            pty.size = Some(size);
        }
        // Cache the size so the next daemon respawn can re-spawn the
        // adapter's PTY at the right dimensions from the start.
        if let Err(e) = self.storage.save_pty_size(id, size) {
            tracing::warn!(session = %id, error = ?e, "save_pty_size failed");
        }
        // Tell other attached clients the new geometry (transient, not
        // persisted) so a passive viewer (e.g. a narrower web terminal) can
        // render at the real width instead of wrapping. Only fires on an
        // actual change — the dedup above already returned for a no-op.
        let payload = construct_protocol::EventNotificationPayload {
            session_id: id.to_string(),
            at: chrono::Utc::now(),
            event: SessionEvent::PtyResize {
                cols,
                rows,
                owner: false,
            },
            seq: 0,
        };
        let broadcast = match owner_conn_id {
            Some(owner_conn_id) => super::BroadcastMsg::PtyResize {
                payload,
                owner_conn_id,
            },
            None => super::BroadcastMsg::Event(payload),
        };
        let _ = self.broadcast.send(broadcast);
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let params = serde_json::to_value(&construct_protocol::SessionPtyResizeParams {
            session_id: id.to_string(),
            cols,
            rows,
            // Adapter-facing resize has no client-ownership semantics.
            claim: false,
        })?;
        adapter
            .request(ahp_method::SESSION_PTY_RESIZE, params)
            .await?;
        Ok(true)
    }

    #[cfg(test)]
    pub async fn pty_replay(&self, id: &str) -> Result<PtyReplayResult> {
        self.pty_replay_range(id, None, None).await
    }

    pub async fn pty_replay_range(
        &self,
        id: &str,
        max_bytes: Option<usize>,
        before_offset: Option<u64>,
    ) -> Result<PtyReplayResult> {
        use base64::Engine;
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = entry.pty.lock().await.size;
        // Pull scrollback from the on-disk `pty.log`, not the (now-removed)
        // in-memory ring. Requests are capped by `PTY_REPLAY_CAP`; clients can
        // ask for older adjacent ranges and replay their local chunks in order.
        let requested = max_bytes.unwrap_or(PTY_REPLAY_CAP).min(PTY_REPLAY_CAP);
        let (bytes, start_offset, end_offset, total_bytes) = self
            .storage
            .read_pty_range_before(id, requested, before_offset)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %id, error = ?e, "pty_log range read failed");
                (Vec::new(), 0, 0, 0)
            });
        Ok(PtyReplayResult {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            start_offset,
            end_offset,
            total_bytes,
            size,
        })
    }

    /// Render the session's terminal server-side and return it as a compact
    /// escape-sequence stream (spec 0188): a bounded `pty.log` tail is fed
    /// through a native vt100 parser at the session's PTY size, then the
    /// parser's scrollback and visible screen are serialized. A client
    /// writes the result into a freshly reset terminal of the same size and
    /// is caught up in O(screen + scrollback) bytes instead of replaying
    /// the raw history through its own emulator.
    pub async fn screen_snapshot(
        &self,
        id: &str,
        strip_alt_screen: bool,
    ) -> Result<construct_protocol::ScreenSnapshotResult> {
        use base64::Engine;
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = entry.pty.lock().await.size.ok_or_else(|| {
            // Without the child's real geometry the render would wrap
            // wrongly; callers fall back to `session.pty_replay`.
            anyhow!("session {} has no known pty size", id)
        })?;
        let (bytes, start_offset, end_offset, total_bytes) = self
            .storage
            .read_pty_range_before(id, SCREEN_SNAPSHOT_REPLAY_BYTES, None)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %id, error = ?e, "pty_log range read failed");
                (Vec::new(), 0, 0, 0)
            });
        let rendered = render_screen_snapshot(&bytes, size, strip_alt_screen);
        Ok(construct_protocol::ScreenSnapshotResult {
            data: base64::engine::general_purpose::STANDARD.encode(rendered.data),
            scrollback_rows: rendered.scrollback_rows as u64,
            scrollback_truncated: rendered.scrollback_truncated,
            start_offset,
            end_offset,
            total_bytes,
            size,
        })
    }

    /// Deliver a prompt as a bracketed paste (`ESC[200~` … `ESC[201~`) when
    /// submitting to external PTY-backed agents.
    pub(super) async fn playbook_submit_typed_prompt(&self, id: &str, prompt: &str) -> Result<()> {
        // Deliver the prompt as a bracketed paste (`ESC[200~` … `ESC[201~`)
        // rather than raw keystrokes. External agent TUIs (claude/codex/
        // antigravity) enable DEC mode 2004 and only run their multiline
        // guard on a real bracketed paste: framed this way they buffer the
        // whole multi-line body as one input instead of submitting on the
        // first embedded newline. Crucially the `ESC[201~` end marker tells
        // the harness exactly where the paste stops, so the Enter we send
        // afterward is read as a submit keypress — without it the prompt
        // landed in the input box but never submitted.
        self.pty_input_without_capture(id, playbook_bracketed_paste_bytes(prompt))
            .await?;
        tokio::time::sleep(PLAYBOOK_EXTERNAL_PTY_SUBMIT_DELAY).await;
        self.pty_input_without_capture(id, vec![b'\r']).await?;
        Ok(())
    }

    /// Cold-start-tolerant variant of [`Self::playbook_submit_typed_prompt`]
    /// for prompts delivered to a just-created execution fork: the submit
    /// Enter is gated on the PTY producing output after the paste, not just
    /// a fixed delay. A cold-started harness still busy with its own
    /// startup work doesn't drain stdin during a fixed delay, so the paste
    /// and the Enter accumulate in the PTY buffer and arrive in ONE read —
    /// the Enter then sits directly after the `ESC[201~` paste-end marker
    /// in the same input batch and gets treated as part of the paste burst
    /// instead of a standalone submit keypress, leaving the prompt visibly
    /// typed but never submitted (spec 0086 documents the identical race
    /// for usage probes). Output growth after the paste is evidence the
    /// harness consumed it, so the Enter written afterwards arrives in a
    /// later read and parses as a real keypress. Unlike the usage probe's
    /// token-echo gate this cannot look for the prompt text itself: playbook
    /// prompts are long, and external TUIs collapse long pastes into a
    /// `[Pasted text #N]` placeholder that never echoes the body. On gate
    /// timeout the Enter is sent anyway — a missing echo most likely means
    /// the harness is drawing nothing yet, and the Enter can't make that
    /// outcome worse than the old fixed-delay behavior.
    ///
    /// This gate only distinguishes "the harness consumed the paste" from
    /// "the harness is still busy"; it is not a readiness check and cannot
    /// substitute for one. Any output growth satisfies it, including output
    /// that has nothing to do with the paste, so a caller that pastes into a
    /// harness which has not attached its input handler yet gets a silent
    /// pass here. Readiness is established *before* this is called — see
    /// `wait_for_fork_ready`.
    pub(super) async fn playbook_submit_typed_prompt_cold_start(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<()> {
        let before_offset = self.pty_log_len(id);
        self.pty_input_without_capture(id, playbook_bracketed_paste_bytes(prompt))
            .await?;
        let started = tokio::time::Instant::now();
        loop {
            if self.pty_log_len(id) > before_offset {
                break;
            }
            if started.elapsed() >= PLAYBOOK_PASTE_OUTPUT_GATE_TIMEOUT {
                tracing::warn!(
                    session = %id,
                    "playbook fork prompt: no PTY output after paste within the gate window; sending Enter anyway",
                );
                break;
            }
            tokio::time::sleep(PLAYBOOK_PASTE_OUTPUT_GATE_POLL).await;
        }
        // Keep the short settle between the observed echo and the Enter:
        // the echo proves the paste was consumed, the settle lets the
        // harness finish the render/state update it triggered.
        tokio::time::sleep(PLAYBOOK_EXTERNAL_PTY_SUBMIT_DELAY).await;
        self.pty_input_without_capture(id, vec![b'\r']).await?;
        Ok(())
    }
}

/// Poll interval / hard cap for the paste-output gate in
/// [`SessionManager::playbook_submit_typed_prompt_cold_start`]. The timeout
/// is generous relative to how fast a responsive harness echoes (tens of
/// ms) precisely because the case the gate exists for is a harness slow to
/// drain stdin while busy with its own startup work.
const PLAYBOOK_PASTE_OUTPUT_GATE_POLL: Duration = Duration::from_millis(50);
const PLAYBOOK_PASTE_OUTPUT_GATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-session writer task (spec 0087): drains the ordered input queue and
/// performs the adapter `session.pty_input` round-trips that used to run
/// inline in the IPC dispatch loop. One writer per session, spawned on the
/// first input; exits on its own when the owning `SessionEntry` is dropped
/// (the queue's sender lives on the entry, so the channel closes and
/// `recv` returns `None`) — delete/restart need no explicit teardown.
/// Holds only a `Weak` to the entry so a torn-down session's memory isn't
/// kept alive by its own input queue.
async fn pty_input_writer(entry: Weak<SessionEntry>, mut rx: mpsc::Receiver<PtyInputJob>) {
    while let Some(job) = rx.recv().await {
        let Some(entry) = entry.upgrade() else { break };
        let result = deliver_pty_input(&entry, &job.bytes).await;
        match job.ack {
            // A dropped receiver means the awaiting caller was cancelled;
            // delivery already happened, so there is nothing to report.
            Some(ack) => {
                let _ = ack.send(result);
            }
            None => {
                if let Err(e) = result {
                    tracing::warn!(session = %entry.id, error = %e, "pty input delivery failed");
                }
            }
        }
    }
}

/// One adapter round-trip for one queued batch. Fetches the adapter at
/// delivery time — not enqueue time — so input queued across an adapter
/// respawn reaches the new adapter instead of erroring on the dead one.
async fn deliver_pty_input(entry: &Arc<SessionEntry>, bytes: &[u8]) -> Result<()> {
    let adapter = entry
        .adapter
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow!("session has no live adapter"))?;
    let params = serde_json::to_value(&construct_protocol::SessionPtyInputParams::from_bytes(
        &entry.id, bytes,
    ))?;
    adapter
        .request(ahp_method::SESSION_PTY_INPUT, params)
        .await?;
    Ok(())
}

/// Bytes of `pty.log` tail parsed to build a screen snapshot. Only feeds
/// the server-side parser — none of it goes over the wire — so it just
/// needs to comfortably cover the visible screen plus the scrollback row
/// budget below. Kept well under `PTY_REPLAY_CAP`: a client that pages
/// into older history re-fetches this span as raw bytes, so the span
/// bounds that fallback's cost too.
const SCREEN_SNAPSHOT_REPLAY_BYTES: usize = 1024 * 1024;
/// Scrollback rows the snapshot parser retains and serializes. Bounds the
/// transient parser memory and the snapshot payload; rows beyond it are
/// reported as truncated and remain reachable through `session.pty_replay`.
const SCREEN_SNAPSHOT_SCROLLBACK_ROWS: usize = 4000;

pub(crate) struct RenderedScreenSnapshot {
    pub data: Vec<u8>,
    pub scrollback_rows: usize,
    pub scrollback_truncated: bool,
}

/// Build the escape-sequence stream for [`SessionManager::screen_snapshot`]
/// from a raw PTY byte tail. The stream assumes a freshly reset terminal of
/// `size`: it flows the parser's scrollback rows first, feeds line feeds
/// until they have all scrolled into the client's scrollback buffer (the
/// screen repaint clears the visible area without saving it), then repaints
/// the visible screen and restores cursor, attributes, scroll region, and
/// input modes.
pub(crate) fn render_screen_snapshot(
    bytes: &[u8],
    size: construct_protocol::PtySize,
    strip_alt_screen: bool,
) -> RenderedScreenSnapshot {
    let rows = size.rows.max(1);
    let cols = size.cols.max(1);
    let stripped;
    let src: &[u8] = if strip_alt_screen {
        stripped = strip_alt_screen_sequences(bytes);
        &stripped
    } else {
        bytes
    };
    let mut parser = vt100::Parser::new(rows, cols, SCREEN_SNAPSHOT_SCROLLBACK_ROWS);
    parser.process(src);
    let screen = parser.screen();
    let mut data = Vec::new();
    let scrollback_rows = screen.scrollback_contents_formatted(&mut data);
    if scrollback_rows > 0 {
        // `rows - 1` line feeds push every prepended row off the visible
        // screen whether there were fewer of them than the screen height
        // (some feeds just walk the cursor down to the bottom row first)
        // or more (the cursor is already on the bottom row and every feed
        // scrolls). No blank line ever reaches the top before the feeds
        // stop, so exactly the prepended rows land in scrollback.
        data.extend(std::iter::repeat(b'\n').take(usize::from(rows) - 1));
    }
    data.extend(screen.contents_formatted());
    // `contents_formatted` restores contents, cursor, and attributes but
    // not the scroll region or origin mode, and live deltas following the
    // snapshot may depend on both. DECSTBM and DECOM home the cursor, so
    // re-restore its position afterwards (region-relative under DECOM).
    let (top, bottom) = screen.scroll_region();
    let origin = screen.origin_mode();
    if (top, bottom) != (0, rows - 1) || origin {
        data.extend(format!("\x1b[{};{}r", top + 1, bottom + 1).into_bytes());
        if origin {
            data.extend_from_slice(b"\x1b[?6h");
        }
        let (cur_row, cur_col) = screen.cursor_position();
        let cur_row = if origin {
            cur_row.saturating_sub(top)
        } else {
            cur_row
        };
        data.extend(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).into_bytes());
    }
    data.extend(screen.input_mode_formatted());
    RenderedScreenSnapshot {
        data,
        scrollback_rows,
        scrollback_truncated: scrollback_rows >= SCREEN_SNAPSHOT_SCROLLBACK_ROWS,
    }
}

/// Remove alternate-screen enter/exit sequences (`ESC[?1049h/l`,
/// `ESC[?1047h/l`, `ESC[?47h/l`) from a PTY byte stream — the same filter
/// the web UI applies to every byte it writes into xterm.js.
pub(crate) fn strip_alt_screen_sequences(bytes: &[u8]) -> Vec<u8> {
    const SEQS: [&[u8]; 6] = [
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?1047h",
        b"\x1b[?1047l",
        b"\x1b[?47h",
        b"\x1b[?47l",
    ];
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    'outer: while i < bytes.len() {
        if bytes[i] == 0x1b {
            for seq in SEQS {
                if bytes[i..].starts_with(seq) {
                    i += seq.len();
                    continue 'outer;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
pub(super) fn pty_caps() -> construct_protocol::Capabilities {
    construct_protocol::Capabilities {
        supports_pty: true,
        supports_silent_resume: false,
        ..Default::default()
    }
}

#[cfg(test)]
mod screen_snapshot_tests {
    use super::*;
    use construct_protocol::PtySize;

    /// Feed a byte stream into a fresh parser the way a client terminal
    /// of the same geometry would consume it.
    fn parse(data: &[u8], size: PtySize) -> vt100::Parser {
        let mut p = vt100::Parser::new(size.rows, size.cols, SCREEN_SNAPSHOT_SCROLLBACK_ROWS);
        p.process(data);
        p
    }

    /// Plain-text contents of the view scrolled all the way back, plus the
    /// scrollback depth — the two things a snapshot must reproduce beyond
    /// the visible screen.
    fn top_view(parser: &mut vt100::Parser) -> (usize, String) {
        parser.screen_mut().set_scrollback(usize::MAX);
        let depth = parser.screen().scrollback();
        let contents = parser.screen().contents();
        parser.screen_mut().set_scrollback(0);
        (depth, contents)
    }

    #[test]
    fn snapshot_round_trips_screen_scrollback_and_colors() {
        let size = PtySize { cols: 20, rows: 4 };
        let mut input = Vec::new();
        for i in 0..10 {
            input.extend(format!("\x1b[3{}mline{i:02}\x1b[m", i % 8).into_bytes());
            if i < 9 {
                input.extend_from_slice(b"\r\n");
            }
        }
        let mut orig = parse(&input, size);
        let rendered = render_screen_snapshot(&input, size, false);
        let mut rep = parse(&rendered.data, size);

        assert_eq!(rendered.scrollback_rows, 6, "10 lines on a 4-row screen");
        assert!(!rendered.scrollback_truncated);
        assert_eq!(rep.screen().contents(), orig.screen().contents());
        // Formatted comparison covers colors/attributes, not just text.
        assert_eq!(
            rep.screen().contents_formatted(),
            orig.screen().contents_formatted()
        );
        assert_eq!(
            rep.screen().cursor_position(),
            orig.screen().cursor_position()
        );
        assert_eq!(top_view(&mut rep), top_view(&mut orig));
    }

    #[test]
    fn snapshot_preserves_soft_wrapped_scrollback_rows() {
        let size = PtySize { cols: 10, rows: 3 };
        // A 25-char line soft-wraps across three rows, then enough hard
        // lines push it entirely into scrollback.
        let mut input = b"ABCDEFGHIJ0123456789abcde\r\n".to_vec();
        for i in 0..4 {
            input.extend(format!("tail{i}\r\n").into_bytes());
        }
        input.extend_from_slice(b"end");
        let mut orig = parse(&input, size);
        let rendered = render_screen_snapshot(&input, size, false);
        let mut rep = parse(&rendered.data, size);

        assert_eq!(rep.screen().contents(), orig.screen().contents());
        assert_eq!(top_view(&mut rep), top_view(&mut orig));
    }

    #[test]
    fn snapshot_strip_alt_screen_paints_alt_content_on_primary() {
        let size = PtySize { cols: 20, rows: 4 };
        let mut input = b"before\r\n".to_vec();
        input.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[HALTUI");
        let rendered = render_screen_snapshot(&input, size, true);
        let rep = parse(&rendered.data, size);

        assert!(rep.screen().contents().contains("ALTUI"));
        assert!(!rep.screen().alternate_screen());
        assert!(
            !rendered
                .data
                .windows(b"\x1b[?1049h".len())
                .any(|w| w == b"\x1b[?1049h"),
            "stripped snapshot must not smuggle alt-screen switches back in"
        );
    }

    #[test]
    fn snapshot_restores_scroll_region_and_cursor() {
        let size = PtySize { cols: 20, rows: 6 };
        let input = b"hello\x1b[2;5r\x1b[3;4H".to_vec();
        let rendered = render_screen_snapshot(&input, size, false);
        let rep = parse(&rendered.data, size);

        assert_eq!(rep.screen().scroll_region(), (1, 4));
        assert_eq!(rep.screen().cursor_position(), (2, 3));
    }

    #[test]
    fn snapshot_reports_scrollback_truncation() {
        let size = PtySize { cols: 20, rows: 4 };
        let mut input = Vec::new();
        for i in 0..(SCREEN_SNAPSHOT_SCROLLBACK_ROWS + 100) {
            input.extend(format!("row {i}\r\n").into_bytes());
        }
        let rendered = render_screen_snapshot(&input, size, false);
        assert_eq!(rendered.scrollback_rows, SCREEN_SNAPSHOT_SCROLLBACK_ROWS);
        assert!(rendered.scrollback_truncated);
    }

    #[test]
    fn strip_alt_screen_sequences_removes_only_switches() {
        let input = b"a\x1b[?1049hb\x1b[31mc\x1b[?47ld\x1b[?1047h".to_vec();
        assert_eq!(
            strip_alt_screen_sequences(&input),
            b"ab\x1b[31mcd".to_vec()
        );
    }
}

#[cfg(test)]
mod vt100_regression_tests {
    #[test]
    fn resize_clears_wide_character_stranded_at_right_edge() {
        let mut parser = vt100::Parser::new(1, 10, 0);

        parser.process(b"\x1b[1;9H");
        parser.process("世".as_bytes());
        parser.screen_mut().set_size(1, 9);
        parser.process(b"\x1b[1;9HX");

        assert_eq!(parser.screen().cell(0, 8).unwrap().contents(), "X");
    }
}
