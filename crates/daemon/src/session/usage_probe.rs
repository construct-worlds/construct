//! Harness usage-probe orchestration (spec 0085): `usage.query`'s
//! background half. See `crate::usage` for the cache types this module
//! populates.
//!
//! The shape deliberately mirrors the post-resume force-redraw wait
//! (`lifecycle::resume_redraw_ready` + its poll loop): a pure decision fn
//! polled on an interval against a session's `last_pty_at_ms`, capped by a
//! hard timeout. Everything that awaits (session create, submitting the
//! probe command, sleeps, pty_replay, delete) runs with no `usage_cache`
//! lock held — the lock is only ever taken for the tiny read/write
//! critical sections in `usage_query` and `refresh_usage`.

use super::*;
use std::path::Path;

/// Poll interval while waiting for a probe session's PTY to go quiet.
const USAGE_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long the PTY must stay quiet before the startup draw (step 4) or
/// the usage command's response (step 6) is considered finished.
const USAGE_PROBE_SETTLE: Duration = Duration::from_millis(500);
/// Hard cap on waiting for the harness to finish its own startup draw
/// before giving up on the whole probe — a hung/slow-starting harness must
/// not wedge the probe forever.
const USAGE_PROBE_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on waiting for the usage/status command's response. Longer
/// than the startup cap since some harnesses' usage panels fetch live
/// account data over the network.
const USAGE_PROBE_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Fixed PTY size for probe sessions — generous, since there's no live
/// client to negotiate a real size against.
const USAGE_PROBE_COLS: u16 = 100;
const USAGE_PROBE_ROWS: u16 = 40;

impl SessionManager {
    /// `usage.query` (spec 0085). Read-mostly: never blocks on the probe
    /// itself. Returns the most recently cached snapshot for `harness`
    /// (regardless of freshness — the TTL only gates whether a new probe is
    /// warranted, not whether the last one is still returned) plus whether
    /// a refresh is in flight. When `allow_refresh` is set and the cache is
    /// stale-or-missing and nothing is already running for this harness, a
    /// background probe is spawned and this call still returns immediately.
    pub async fn usage_query(
        self: &Arc<Self>,
        harness: &str,
        allow_refresh: bool,
    ) -> construct_protocol::UsageQueryResult {
        let Some(command) = self
            .config
            .effective_usage_probe(harness)
            .map(str::to_string)
        else {
            return construct_protocol::UsageQueryResult {
                snapshot: None,
                refreshing: false,
                enabled: false,
            };
        };

        let (snapshot, mut refreshing) = {
            let cache = self
                .usage_cache
                .lock()
                .expect("usage_cache mutex poisoned");
            (cache.get(harness), cache.is_refreshing(harness))
        };
        let stale = snapshot.as_ref().map(|s| !s.is_fresh()).unwrap_or(true);
        if allow_refresh && stale && !refreshing {
            let began = {
                let mut cache = self
                    .usage_cache
                    .lock()
                    .expect("usage_cache mutex poisoned");
                cache.try_begin_refresh(harness)
            };
            if began {
                refreshing = true;
                let mgr = self.clone();
                let harness_owned = harness.to_string();
                tokio::spawn(async move {
                    mgr.refresh_usage(harness_owned, command).await;
                });
            }
        }

        construct_protocol::UsageQueryResult {
            snapshot: snapshot.map(|s| construct_protocol::UsageSnapshotInfo {
                bytes: base64::engine::general_purpose::STANDARD.encode(&s.bytes),
                cols: s.cols,
                rows: s.rows,
                captured_at_ms: s.captured_at_ms,
            }),
            refreshing,
            enabled: true,
        }
    }

    /// Run one usage probe for `harness` and update the cache. Always
    /// clears the in-flight guard on the way out — including on any
    /// failure — so a later query just retries; no snapshot is stored on
    /// failure, which is exactly "not cached" from `usage_query`'s view.
    async fn refresh_usage(self: Arc<Self>, harness: String, command: String) {
        let result = self.run_usage_probe(&harness, &command).await;
        let mut cache = self
            .usage_cache
            .lock()
            .expect("usage_cache mutex poisoned");
        if let Some(snapshot) = result {
            cache.store(&harness, snapshot);
        }
        cache.finish_refresh(&harness);
    }

    /// Spin up an ephemeral `SessionKind::UsageProbe` session, run
    /// `command` in it, capture what it renders, and tear the session (and
    /// the native transcript file it caused the harness CLI to create)
    /// back down. Returns `None` on any hard failure or empty capture —
    /// the caller treats that as "nothing to cache", not an error to
    /// surface.
    async fn run_usage_probe(
        self: &Arc<Self>,
        harness: &str,
        command: &str,
    ) -> Option<crate::usage::UsageSnapshot> {
        let create_params = construct_protocol::CreateSessionParams {
            harness: harness.to_string(),
            cwd: usage_probe_cwd(),
            prompt: None,
            model: None,
            title: Some("usage probe".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(construct_protocol::PtySize {
                cols: USAGE_PROBE_COLS,
                rows: USAGE_PROBE_ROWS,
            }),
            worktree: false,
            env: HashMap::new(),
            args: Vec::new(),
            kind: construct_protocol::SessionKind::UsageProbe,
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
            forked_from: None,
        };
        let created_at_ms = Utc::now().timestamp_millis();
        let id = match self.create(create_params).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(%harness, error = %e, "usage probe: session create failed");
                return None;
            }
        };
        tracing::debug!(%harness, session = %id, "usage probe: session created");

        // Step 4: wait for the harness's own startup draw to settle before
        // sending the usage command — sending too early can land before
        // the harness has even wired up its slash-command handler. A
        // session that never settles (hung/slow-starting harness) aborts
        // the whole probe rather than sending input into a black box.
        // `since_ms` floors which `last_pty_at_ms` updates count: only
        // output from at-or-after session creation, so a stale timestamp
        // can never exist yet at this point anyway (this is the session's
        // first activity).
        if !self
            .wait_for_pty_settle(
                &id,
                created_at_ms,
                USAGE_PROBE_SETTLE,
                USAGE_PROBE_STARTUP_TIMEOUT,
            )
            .await
        {
            tracing::warn!(%harness, session = %id, "usage probe: startup timed out; aborting");
            self.cleanup_usage_probe_session(harness, &id).await;
            return None;
        }
        tracing::debug!(%harness, session = %id, "usage probe: startup settled");

        // Step 5: record the current PTY log offset, then send the probe
        // command as a bracketed paste + separate Enter
        // (`program_submit_typed_prompt`) — the same delivery the program
        // Run path uses for these exact harnesses. Plain `send_input`
        // (ahp `SESSION_INPUT`, "type it and append \n") is NOT equivalent
        // here: claude/codex/antigravity's rich interactive TUIs only
        // treat a real bracketed paste as one atomic submission, so a bulk
        // raw write lands the text in the input box without ever
        // submitting it — see `program_submit_typed_prompt`'s own doc
        // comment for the same lesson learned once already for the
        // program Run path.
        let before_offset = self.pty_log_len(&id);
        let sent_at_ms = Utc::now().timestamp_millis();
        if let Err(e) = self.program_submit_typed_prompt(&id, command).await {
            tracing::warn!(%harness, session = %id, error = %e, "usage probe: submitting command failed");
            self.cleanup_usage_probe_session(harness, &id).await;
            return None;
        }
        tracing::debug!(%harness, session = %id, before_offset, command, "usage probe: command sent");

        // Step 6: wait for the response to settle. Unlike step 4, a
        // timeout here still proceeds to capture whatever was produced —
        // partial usage output beats nothing. `since_ms` is critical here:
        // step 4 already left `last_pty_at_ms` sitting on an old,
        // already-quiet timestamp, so without a floor this would
        // immediately (and wrongly) read as "settled" before the command's
        // response ever arrives — only a PTY update at-or-after the moment
        // the command was sent counts as evidence of a real response.
        let command_settled = self
            .wait_for_pty_settle(
                &id,
                sent_at_ms,
                USAGE_PROBE_SETTLE,
                USAGE_PROBE_COMMAND_TIMEOUT,
            )
            .await;
        tracing::debug!(%harness, session = %id, command_settled, "usage probe: command wait done");

        // Step 7: capture only the bytes produced since the command was sent.
        let bytes = self.capture_pty_since(&id, before_offset).await;
        tracing::debug!(%harness, session = %id, captured_bytes = bytes.len(), "usage probe: captured");

        // Steps 8-10: hard-kill the adapter, best-effort unlink the native
        // transcript file it caused to exist, delete construct's own
        // session record.
        self.cleanup_usage_probe_session(harness, &id).await;

        if bytes.is_empty() {
            tracing::warn!(%harness, session = %id, "usage probe: capture was empty; not caching");
            return None;
        }
        Some(crate::usage::UsageSnapshot {
            bytes,
            cols: USAGE_PROBE_COLS,
            rows: USAGE_PROBE_ROWS,
            captured_at: std::time::Instant::now(),
            captured_at_ms: Utc::now().timestamp_millis(),
        })
    }

    /// Poll `id`'s `last_pty_at_ms` until it has settled — a PTY update at
    /// or after `since_ms`, followed by `settle` of quiet (`true`) — or
    /// `max_wait` elapses (`false`: gave up). `since_ms` matters: this
    /// session's `last_pty_at_ms` can already hold an old, already-quiet
    /// timestamp from an earlier wait on the same session (step 4 vs. step
    /// 6), and only an update at-or-after `since_ms` counts as evidence
    /// that the thing this particular wait cares about actually happened.
    /// See [`usage_probe_wait_outcome`] for the pure decision step.
    async fn wait_for_pty_settle(
        &self,
        id: &str,
        since_ms: i64,
        settle: Duration,
        max_wait: Duration,
    ) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            let last_pty_at_ms = match self.get_entry(id).await {
                Some(entry) => entry.summary.read().await.last_pty_at_ms,
                None => return false,
            };
            let now_ms = Utc::now().timestamp_millis();
            match usage_probe_wait_outcome(
                last_pty_at_ms,
                since_ms,
                now_ms,
                started.elapsed(),
                settle,
                max_wait,
            ) {
                Some(settled) => return settled,
                None => tokio::time::sleep(USAGE_PROBE_POLL_INTERVAL).await,
            }
        }
    }

    /// Current byte length of `id`'s on-disk `pty.log`, used to mark "the
    /// probe command hasn't been sent yet" before step 5 so step 7 can
    /// slice out only what the command produced.
    fn pty_log_len(&self, id: &str) -> u64 {
        std::fs::metadata(self.storage.pty_log_path(id))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Capture the bytes appended to `id`'s `pty.log` since `before_offset`.
    async fn capture_pty_since(&self, id: &str, before_offset: u64) -> Vec<u8> {
        match self.pty_replay_range(id, None, None).await {
            Ok(result) => {
                let all = base64::engine::general_purpose::STANDARD
                    .decode(&result.data)
                    .unwrap_or_default();
                if before_offset >= result.start_offset {
                    let skip = (before_offset - result.start_offset) as usize;
                    all.get(skip..).map(|s| s.to_vec()).unwrap_or_default()
                } else {
                    // The command produced more than PTY_REPLAY_CAP bytes
                    // (extremely unlikely for a usage panel) — the earliest
                    // new bytes already scrolled out of the read window.
                    // Returning the whole (tail) window is still a
                    // reasonable capture.
                    all
                }
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "usage probe: pty_replay_range failed");
                Vec::new()
            }
        }
    }

    /// Steps 8-10: kill the adapter, best-effort unlink the native
    /// transcript file it caused to exist, then delete construct's own
    /// session record.
    async fn cleanup_usage_probe_session(&self, harness: &str, id: &str) {
        // Hard-kill (SIGKILL) rather than a graceful stop — the native
        // transcript file is resolved and unlinked below only after the
        // process is confirmed dead, so there's no benefit to waiting for
        // a graceful exit first.
        if let Some(entry) = self.get_entry(id).await {
            if let Some(adapter) = entry.adapter.lock().await.take() {
                adapter.kill();
            }
        }

        self.unlink_usage_probe_native_transcript(harness, id)
            .await;

        // Worktree removal inside `delete` is a no-op: probe sessions are
        // always created with `worktree: false`.
        if let Err(e) = self.delete(id).await {
            tracing::warn!(%harness, session = %id, error = %e, "usage probe: session delete failed");
        }
    }

    /// Best-effort: resolve and remove the native transcript file (or, for
    /// harnesses that give each session its own directory, the whole
    /// directory — see [`Self::try_unlink_usage_probe_native_transcript`])
    /// this probe session caused its harness CLI to create, so a burst of
    /// probes never leaves stray entries in the harness's own native
    /// history (`claude --resume` picker, `~/.codex/sessions/`, ...).
    /// Never fails the probe — any error here is logged and swallowed.
    /// Only ever called for `SessionKind::UsageProbe` sessions; real user
    /// sessions' native transcripts are never touched.
    ///
    /// Retries a few times with a short delay between attempts: at the
    /// point this runs the adapter has just been SIGKILL'd, but two
    /// sources of native-side latency can still be in flight — (a) some
    /// harnesses (grok, codex, antigravity) capture their own native id
    /// via a background watcher that polls periodically rather than
    /// synchronously at spawn, so the id-file this reads may not exist
    /// yet, and (b) a harness's own write of its transcript file can still
    /// land on disk a moment after the process receives SIGKILL (syscalls
    /// already in flight complete even though the process can't run more
    /// code). Confirmed empirically: an immediate single-attempt check
    /// against a live grok probe reported "file not found" and then the
    /// file appeared on disk moments later, which would have left a real
    /// stray entry. A single-shot check is not reliable enough here.
    ///
    /// Often ends up a genuine no-op even after retrying: a usage/status
    /// slash command is a local UI query, not a real conversational turn,
    /// and several harnesses only persist a transcript file once an actual
    /// turn happens (confirmed for claude: `claude_session_id.txt` is
    /// written at process startup, well before any turn, but the
    /// corresponding `~/.claude/projects/.../*.jsonl` is never created for
    /// a probe that only ran `/usage`) — that case is expected and not
    /// logged as a failure.
    async fn unlink_usage_probe_native_transcript(&self, harness: &str, id: &str) {
        const ATTEMPTS: u32 = 4;
        const RETRY_DELAY: Duration = Duration::from_millis(300);
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            match self.try_unlink_usage_probe_native_transcript(harness, id) {
                UnlinkOutcome::Removed(path) => {
                    tracing::debug!(
                        %harness, session = %id, path = %path.display(), attempt,
                        "usage probe: removed native transcript",
                    );
                    return;
                }
                UnlinkOutcome::Error(path, e) => {
                    // A real error (permission denied, etc.) — not a
                    // timing issue retrying would fix.
                    tracing::warn!(
                        %harness, session = %id, path = %path.display(), error = %e,
                        "usage probe: failed to remove native transcript",
                    );
                    return;
                }
                UnlinkOutcome::NothingYet if attempt + 1 < ATTEMPTS => continue,
                UnlinkOutcome::NothingYet => {
                    tracing::debug!(
                        %harness, session = %id, attempts = ATTEMPTS,
                        "usage probe: no native transcript found after retrying — likely no real turn happened",
                    );
                    return;
                }
            }
        }
    }

    /// One attempt at resolving + removing the native transcript. See
    /// [`Self::unlink_usage_probe_native_transcript`] for why this is
    /// retried rather than a single check.
    ///
    /// claude and codex persist a single flat file per session inside a
    /// directory *shared* with other sessions
    /// (`<home>/projects/<slug>/*.jsonl`, `<home>/sessions/**/*.jsonl`), so
    /// only that one file is removed. grok and antigravity instead give
    /// each session/conversation its own *exclusive* directory containing
    /// several sibling files (grok: `summary.json`, `prompt_context.json`,
    /// `system_prompt.txt`, ... alongside `chat_history.jsonl`;
    /// antigravity: a full `.git` history, task logs, uploads alongside
    /// `.system_generated/logs/transcript.jsonl`) — removing only the
    /// transcript file there would still leave a real entry in the
    /// harness's own session picker, so the whole directory is removed
    /// instead. Verified against a real antigravity conversation directory
    /// during manual testing (spec 0085): far more lives there than just
    /// the transcript file this module reads to mirror chat history.
    fn try_unlink_usage_probe_native_transcript(&self, harness: &str, id: &str) -> UnlinkOutcome {
        let session_dir = self.storage.session_dir(id);
        let env = self
            .config
            .adapters
            .get(harness)
            .map(|c| c.env.clone())
            .unwrap_or_default();
        let cwd = PathBuf::from(usage_probe_cwd());
        match harness {
            "claude" => remove_file_outcome(
                read_native_id_file(&session_dir.join("claude_session_id.txt")).and_then(
                    |native_id| {
                        construct_adapter_common::claude_transcript_path(&cwd, &native_id, &env)
                    },
                ),
            ),
            "codex" => remove_file_outcome(
                read_native_id_file(&session_dir.join("codex_session_id.txt")).and_then(
                    |native_id| construct_adapter_common::codex_transcript_path(&env, &native_id),
                ),
            ),
            "grok" => remove_dir_outcome(
                read_native_id_file(&session_dir.join("grok_session_id.txt")).and_then(
                    |native_id| construct_adapter_common::grok_session_dir(&cwd, &native_id, &env),
                ),
            ),
            "agy" => remove_dir_outcome(
                read_native_id_file(&session_dir.join("agy_conversation_id.txt")).and_then(
                    |native_id| construct_adapter_common::antigravity_conversation_dir(&native_id, &env),
                ),
            ),
            _ => UnlinkOutcome::NothingYet,
        }
    }
}

/// Outcome of one [`SessionManager::try_unlink_usage_probe_native_transcript`]
/// attempt.
enum UnlinkOutcome {
    /// Successfully removed the file or directory at this path.
    Removed(PathBuf),
    /// Either no native id-file exists yet, or it exists but the harness
    /// hasn't created anything at the resolved path yet (or ever will, for
    /// a probe that never triggered a real turn) — retry, then treat as
    /// "nothing to unlink" once attempts are exhausted.
    NothingYet,
    /// Resolved a path and found something there, but removing it failed
    /// for a real reason (permission, etc.) — not worth retrying.
    Error(PathBuf, std::io::Error),
}

fn remove_file_outcome(path: Option<PathBuf>) -> UnlinkOutcome {
    let Some(path) = path else {
        return UnlinkOutcome::NothingYet;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => UnlinkOutcome::Removed(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::NothingYet,
        Err(e) => UnlinkOutcome::Error(path, e),
    }
}

fn remove_dir_outcome(path: Option<PathBuf>) -> UnlinkOutcome {
    let Some(path) = path else {
        return UnlinkOutcome::NothingYet;
    };
    match std::fs::remove_dir_all(&path) {
        Ok(()) => UnlinkOutcome::Removed(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::NothingYet,
        Err(e) => UnlinkOutcome::Error(path, e),
    }
}

/// cwd for an ephemeral probe session. `worktree: false` means no git repo
/// is required, so a project-less default is fine — the harness only needs
/// *a* writable directory to start in, not any particular project.
///
/// Deliberately the daemon's own process cwd (same choice
/// `ensure_orchestrator` makes for the minibuffer session), NOT the user's
/// home directory: several wrapper harnesses gate a directory they haven't
/// seen before behind a first-run interactive trust prompt (confirmed for
/// claude — `$HOME` is very often untrusted since users rarely start a real
/// claude session directly in their home directory), and that prompt
/// consumes the probe's only turn, producing no usage output at all. The
/// daemon's own cwd is where the user chose to start `construct daemon
/// run` — typically inside a project they already work in and have likely
/// already trusted with every harness — so it is far more likely to be
/// pre-trusted than an arbitrary fixed path. This is a best-effort
/// mitigation, not a guarantee: a harness can still show its trust prompt
/// for a directory it has truly never seen, in which case that prompt is
/// exactly what gets captured (see the "redisplay verbatim" decision) and
/// the next probe (after the user trusts it, e.g. by using that harness
/// normally) succeeds.
fn usage_probe_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
}

/// Trim a native id-file's contents, treating whitespace-only as absent.
fn read_native_id_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Pure decision step for [`SessionManager::wait_for_pty_settle`]'s poll
/// loop, mirroring `lifecycle::resume_redraw_ready`'s shape (checked on an
/// interval against a session's `last_pty_at_ms`) but distinguishing
/// "settled" (`Some(true)`: a PTY update at or after `since_ms` happened,
/// then went quiet for `settle`) from "gave up after `max_wait` with
/// nothing to show for it" (`Some(false)`). `None` means keep polling.
///
/// `since_ms` exists because the same session is waited on twice in a row
/// (step 4's startup wait, then step 6's post-command wait) sharing one
/// `last_pty_at_ms` field: without a floor, step 6 would immediately read
/// step 4's already-old, already-quiet timestamp as "settled" and return
/// before the command's response ever arrived. A `last_pty_at_ms` older
/// than `since_ms` is stale evidence from a previous wait and must not
/// count.
///
/// The caller treats the two `Some` outcomes differently: a startup wait
/// that never settles aborts the whole probe, while a post-command wait
/// that never settles still proceeds to capture whatever was produced.
fn usage_probe_wait_outcome(
    last_pty_at_ms: Option<i64>,
    since_ms: i64,
    now_ms: i64,
    elapsed: Duration,
    settle: Duration,
    max_wait: Duration,
) -> Option<bool> {
    if let Some(t) = last_pty_at_ms {
        if t >= since_ms && now_ms.saturating_sub(t) >= settle.as_millis() as i64 {
            return Some(true);
        }
    }
    if elapsed >= max_wait {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settle gate: keep polling while the probe session is still
    /// drawing (recent output) or hasn't drawn at all, settle once it goes
    /// quiet, and give up once the hard cap passes with nothing observed.
    #[test]
    fn usage_probe_wait_outcome_settle_gate() {
        let now = 1_000_000i64;
        let since = 0i64; // no floor for this test
        let settle = USAGE_PROBE_SETTLE;
        let max_wait = Duration::from_secs(10);

        // Nothing drawn yet, well under the cap -> keep polling.
        assert_eq!(
            usage_probe_wait_outcome(None, since, now, Duration::from_millis(0), settle, max_wait),
            None
        );
        // Output 50ms ago (< settle) -> still drawing, keep polling.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - 50),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            None
        );
        // Quiet for exactly the settle window -> settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - settle.as_millis() as i64),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
        // Quiet well past settle -> settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(now - 5_000),
                since,
                now,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
        // Never settles (recent output) but hit the hard cap -> gave up.
        assert_eq!(
            usage_probe_wait_outcome(Some(now), since, now, max_wait, settle, max_wait),
            Some(false)
        );
        // Never drew anything, but hit the hard cap -> gave up.
        assert_eq!(
            usage_probe_wait_outcome(None, since, now, max_wait, settle, max_wait),
            Some(false)
        );
    }

    /// The `since_ms` floor: a stale, already-quiet timestamp from *before*
    /// `since_ms` must not read as settled — this is exactly the step-4-vs-
    /// step-6 reuse bug the floor exists to prevent. Only a PTY update at
    /// or after `since_ms` counts as real evidence for this particular wait.
    #[test]
    fn usage_probe_wait_outcome_ignores_stale_timestamp_before_since() {
        let now = 1_000_000i64;
        let since = now; // command was just sent "now"
        let settle = USAGE_PROBE_SETTLE;
        let max_wait = Duration::from_secs(10);

        // last_pty_at_ms is from well before `since` and long "quiet" by
        // wall-clock terms, but it predates the thing we're waiting for ->
        // must NOT read as settled.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(since - 10_000),
                since,
                now,
                Duration::from_millis(0),
                settle,
                max_wait
            ),
            None
        );
        // A fresh update at/after `since`, settled -> settles normally.
        assert_eq!(
            usage_probe_wait_outcome(
                Some(since + 10),
                since,
                since + 10 + settle.as_millis() as i64,
                Duration::from_secs(1),
                settle,
                max_wait
            ),
            Some(true)
        );
    }

    #[test]
    fn read_native_id_file_trims_and_treats_blank_as_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("id.txt");
        std::fs::write(&file, "  abc-123  \n").expect("write");
        assert_eq!(read_native_id_file(&file), Some("abc-123".to_string()));

        let blank = tmp.path().join("blank.txt");
        std::fs::write(&blank, "   \n").expect("write");
        assert_eq!(read_native_id_file(&blank), None);

        assert_eq!(read_native_id_file(&tmp.path().join("missing.txt")), None);
    }
}
