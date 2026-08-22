//! Session management: lifecycle, adapter binding, event ingestion, broadcast.

use crate::adapter::{locate_binary, Adapter, AdapterMessage};
use crate::config::Config;
use crate::storage::Storage;
use crate::worktree;
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use chrono::Utc;
use construct_protocol::dialect;
use construct_protocol::{
    agent_context, ahp_method, ClientView, CreateSessionParams, DeletedNotificationPayload,
    EventNotificationPayload, GroupSummary, HarnessInfo, LayoutDocument,
    LayoutStateNotificationPayload, MessageRole, MoveDirection, NativeSubagentRef, PlaybookDocument,
    PlaybookEditParams, PlaybookExecuteParams, PlaybookExecuteResult, PlaybookGetResult,
    PlaybookListTemplatesResult, PlaybookListVerbsResult, PlaybookRunProgress,
    PlaybookStateNotificationPayload, PlaybookUpdateParams, PlaybookUpdateResult,
    PlaybookVerbExecuteParams, PlaybookVerbExecuteResult, ProjectDeletedNotificationPayload,
    ProjectStateNotificationPayload, PtyReplayResult, PtySize, SearchParams, SearchResult,
    SessionAttachClipboardParams, SessionAttachClipboardResult, SessionDetail,
    SessionEmitEventParams, SessionEvent, SessionStartParams, SessionState, SessionSummary,
    SmithAuthMethodInfo, SmithAuthStatusResult, SmithSetAuthMethodResult, StateNotificationPayload,
    TimestampedEvent, TranscriptResult,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

mod events;
mod groups;
mod lifecycle;
mod playbook_run;
mod pty;
mod usage_probe;
mod widgets;

#[cfg(test)]
use lifecycle::{
    force_redraw_size_on_resume, install_session_identity_env, reattached_state,
    resume_redraw_ready, should_resume_on_startup, start_params_for_create,
};

const BROADCAST_CAP: usize = 4096;
const ADAPTER_DRAIN_CAP: usize = 256;
/// Tail size (in bytes) of each session's `pty.log` returned to a TUI
/// client on attach. The client feeds these bytes through a vt100 parser
/// that retains only `SCROLLBACK_MAX` rows of formatted scrollback
/// (`crates/cli/src/app.rs`), so the practical scrollback ceiling is the
/// row cap, not this byte cap — this number just needs to be generous
/// enough that the row cap is the binding constraint on typical
/// codex/claude/antigravity sessions. 8 MiB covers ~40-80k rows of dense
/// PTY content, well above the vt100 row budget.
const PTY_REPLAY_CAP: usize = 8 * 1024 * 1024;
/// The post-resume force-redraw (a bump+restore SIGWINCH that nudges a
/// non-silent-resume child into repainting) waits until the child's PTY
/// output has *settled* rather than firing on a fixed delay. A fixed
/// delay was too short for a slow resume — codex loading a large
/// conversation — so the bump landed before the child had drawn anything
/// and the pane stayed blank until the user manually resized. We poll the
/// child's last-output timestamp every [`RESPAWN_REDRAW_POLL`] and fire
/// the bump once output has been quiet for [`RESPAWN_REDRAW_SETTLE`] (the
/// child finished its resume draw), or after [`RESPAWN_REDRAW_MAX_WAIT`]
/// as a hard cap.
const RESPAWN_REDRAW_POLL: Duration = Duration::from_millis(100);
const RESPAWN_REDRAW_SETTLE: Duration = Duration::from_millis(400);
const RESPAWN_REDRAW_MAX_WAIT: Duration = Duration::from_secs(6);
/// After delivering a playbook prompt as a bracketed paste to an external
/// agent TUI, wait this long before sending the submit Enter. The paste is
/// explicitly delimited by its `ESC[201~` end marker, so the harness has
/// already finalized the multi-line body into its input box; this short
/// settle lets its render/state update land so the trailing `\r` is read as
/// a clean submit keypress rather than being coalesced into the paste.
const PLAYBOOK_EXTERNAL_PTY_SUBMIT_DELAY: Duration = Duration::from_millis(120);
/// Poll interval while waiting for a freshly created playbook-execution fork
/// to become ready for its run/verb prompt.
const PLAYBOOK_FORK_READY_POLL: Duration = Duration::from_millis(100);
/// Fallback readiness signal for a playbook-execution fork: how long its PTY
/// must stay quiet before the daemon treats the harness as ready even though
/// it never reported `AwaitingInput`. Deliberately at least `PTY_QUIESCENCE`,
/// the daemon's own definition of "this TUI is idle" — a shorter window
/// mistakes a *pause inside* the startup draw for the end of it. Claude's
/// cold-start draw, for one, reliably pauses ~750ms partway through, so a
/// 500ms window fired mid-boot and the prompt was pasted into a harness that
/// had not attached its input handler yet; the bytes were flushed with the
/// rest of the pre-mount input and the Run silently never happened.
const PLAYBOOK_FORK_READY_SETTLE: Duration = PTY_QUIESCENCE;
/// Hard cap on waiting for a fork to become ready. On timeout the prompt is
/// delivered anyway: a slow-booting harness eventually drains stdin, and
/// delivering late can never be worse than the old deliver-immediately
/// behavior.
const PLAYBOOK_FORK_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYBOOK_CURSOR_TTL_MS: i64 = 60 * 1000;
const PLAYBOOK_AGENT_CURSOR_TTL_MS: i64 = 2 * 1000;
/// How long an interactive full-screen TUI harness's PTY may be silent before
/// the daemon treats it as awaiting input. Line-oriented shells use
/// foreground-process-group detection in the adapter and are exempt.
const PTY_QUIESCENCE: Duration = Duration::from_secs(2);
/// How long a PTY output burst must persist before it counts as genuine
/// activity for quiescence-detected harnesses. Full-screen TUI harnesses
/// repaint status-line housekeeping while idle — claude paints "Checking for
/// updates" every 30 minutes and erases it half a second later — and
/// byte-wise that is indistinguishable from a real turn; what distinguishes
/// it is that it doesn't persist. Sub-window bursts neither mark unseen
/// activity nor undo an AwaitingInput (spec 0054). A burst ends when output
/// pauses for [`PTY_QUIESCENCE`]; with the two windows equal, a lone
/// paint+erase pair can never qualify — its two events would need a gap
/// under the quiescence window spanning at least this window.
const PTY_BLIP_WINDOW: Duration = Duration::from_secs(2);
/// Bounds for the post-respawn settle window during which PTY output is
/// treated as the resumed child repainting old content rather than activity
/// (spec 0054). The window can't end before [`RESUME_SETTLE_MIN`] — the
/// resume repaint plus the delayed force-redraw cycle (see
/// [`RESPAWN_REDRAW_MAX_WAIT`]) must both land inside it — then ends once
/// output has been quiet for [`PTY_QUIESCENCE`], or unconditionally at
/// [`RESUME_SETTLE_MAX`] (a child streaming that long past a resume is
/// genuinely working; its eventual stop deserves the marker).
const RESUME_SETTLE_MIN: Duration = Duration::from_secs(10);
const RESUME_SETTLE_MAX: Duration = Duration::from_secs(30);
const PLAYBOOK_RUN_MAX_MS: i64 = 10 * 60 * 1000;
/// How long after dispatch an owning session's "I am idle" report is treated as
/// the previous turn winding down rather than as evidence that this dispatch
/// never started.
///
/// This is a debounce, never a deadline. A turn can run for hours, and a
/// session in one reports Running — so a run that has been seen running is out
/// of this rule's reach entirely and keeps shimmering for as long as the work
/// takes. It only decides what to make of a session that reports **idle**
/// without ever having reported a turn, which is the shape a dispatch takes
/// when it goes nowhere (#1090).
const PLAYBOOK_RUN_IDLE_WITHOUT_TURN_GRACE_MS: i64 = 10 * 1000;

const MAX_CLIPBOARD_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const ENV_GLOBAL_MEMORY_FILE: &str = "CONSTRUCT_GLOBAL_MEMORY_FILE";
const ENV_PROJECT_MEMORY_FILE: &str = "CONSTRUCT_PROJECT_MEMORY_FILE";
const ENV_PROJECT_ID: &str = "CONSTRUCT_PROJECT_ID";
const WIDGET_WATCH_INTERVAL: Duration = Duration::from_millis(700);

fn sanitized_file_stem(name: &str) -> Option<String> {
    let raw = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches(['-', '.', '_']).to_string();
    (!out.is_empty()).then_some(out)
}

/// MIME for serving a stored attachment back to a client, from its
/// extension (the inverse of [`extension_for_attachment`]'s common cases).
fn mime_for_attachment_ext(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("tiff") | Some("tif") => "image/tiff",
        Some("bmp") => "image/bmp",
        Some("heic") => "image/heic",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") | Some("log") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn extension_for_attachment(filename: Option<&str>, mime: Option<&str>, bytes: &[u8]) -> String {
    if let Some(ext) = filename
        .and_then(|f| std::path::Path::new(f).extension())
        .and_then(|s| s.to_str())
        .map(|s| sanitize_extension(s))
        .filter(|s| !s.is_empty())
    {
        return ext;
    }
    if let Some(ext) = mime.and_then(extension_for_mime) {
        return ext.to_string();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png".to_string()
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg".to_string()
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif".to_string()
    } else if bytes.starts_with(b"%PDF-") {
        "pdf".to_string()
    } else if std::str::from_utf8(bytes).is_ok() {
        "txt".to_string()
    } else {
        "bin".to_string()
    }
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "application/json" => Some("json"),
        _ => None,
    }
}

fn sanitize_extension(ext: &str) -> String {
    ext.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_user_session_kind(s: &SessionSummary) -> bool {
    matches!(s.kind, construct_protocol::SessionKind::User)
}

/// The operator this session is routed to, if any. Routing is a title
/// convention — `operator:<name>` or `operator:<name>:...` — and only counts
/// when `<name>` is an operator that actually exists: clients leave a session
/// whose title merely looks routed in the flat list when no such operator is
/// defined, and reordering must agree with them.
fn routed_operator<'a>(s: &SessionSummary, operator_names: &'a [String]) -> Option<&'a str> {
    let title = s.title.as_deref()?;
    operator_names
        .iter()
        .map(String::as_str)
        .find(|name| {
            let prefix = format!("operator:{name}");
            title == prefix || title.starts_with(&format!("{prefix}:"))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionEdge {
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PositionAfterPlacement {
    position: i64,
    updates: Vec<(String, i64)>,
}

fn position_after_visible_session(
    after_id: &str,
    group_id: &Option<String>,
    sessions: &[SessionSummary],
) -> Option<PositionAfterPlacement> {
    let mut region: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| &s.group_id == group_id && is_user_session_kind(s))
        .collect();
    region.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    let idx = region.iter().position(|s| s.id == after_id)?;
    let source = region[idx];
    if let Some(next) = region.get(idx + 1) {
        let gap = next.position.saturating_sub(source.position);
        if gap > 1 {
            return Some(PositionAfterPlacement {
                position: source.position + gap / 2,
                updates: Vec::new(),
            });
        }

        let base = region.first().map(|s| s.position).unwrap_or(0);
        let mut updates = Vec::new();
        for (i, s) in region.iter().enumerate() {
            let next_pos = base.saturating_add((i as i64).saturating_mul(1024));
            if s.position != next_pos {
                updates.push((s.id.clone(), next_pos));
            }
        }
        return Some(PositionAfterPlacement {
            position: base.saturating_add((idx as i64).saturating_mul(1024) + 512),
            updates,
        });
    }

    Some(PositionAfterPlacement {
        position: source.position.saturating_add(1),
        updates: Vec::new(),
    })
}

#[derive(Clone, Debug)]
pub enum BroadcastMsg {
    Event(EventNotificationPayload),
    /// A PTY resize is personalized while forwarding so the connection that
    /// owns the geometry can distinguish its own echo from a resize claimed
    /// by another TUI/web client.
    PtyResize {
        payload: EventNotificationPayload,
        owner_conn_id: u64,
    },
    State(StateNotificationPayload),
    Deleted(DeletedNotificationPayload),
    /// Project organizer state changed. Variant name still says Group
    /// because the daemon's in-memory/storage model uses `group_*`; the
    /// IPC surface only emits `project/*`.
    GroupState(ProjectStateNotificationPayload),
    GroupDeleted(ProjectDeletedNotificationPayload),
    PlaybookState(PlaybookStateNotificationPayload),
    PlaybookCursor {
        payload: construct_protocol::PlaybookCursorNotificationPayload,
        /// The connection whose own plain cursor publish produced this
        /// broadcast, if any. The per-connection forwarder skips delivering
        /// it back to that connection: a plain publish is an echo of state
        /// the publisher already applied locally, and re-delivering it can
        /// race a later local move — the receiver has no way to tell a
        /// stale echo from a genuine daemon-side rebase, so it must never
        /// see the echo at all. `None` for disconnect tombstones and
        /// rebase-driven updates (`rebase_playbook_cursors_after_edit`),
        /// which every connection — including the cursor's own owner —
        /// needs to receive.
        skip_conn_id: Option<u64>,
    },
    /// Aggregate state for the remote WS transport. Emitted when the
    /// listener starts/stops and on every client accept/drop so the local TUI
    /// can show a persistent remote-control affordance.
    RemoteState(construct_protocol::RemoteStateNotificationPayload),
    ChannelPublicationState(construct_protocol::ChannelPublicationNotificationPayload),
    /// Ambient-feature status (spec 0151). Emitted once, the first time an
    /// ambient feature actually skips work for lack of a smith credential,
    /// so clients can surface the degradation without polling.
    FeaturesState(construct_protocol::FeaturesStatusResult),
    /// A `config.toml` reload settled (spec 0190). Carries what it applied
    /// and what is still waiting on a restart. Also replayed on subscribe,
    /// so a client attaching later still learns about a pending restart.
    ConfigState(construct_protocol::ConfigStateNotificationPayload),
    /// The shared split layout changed — a client wrote it, or the daemon
    /// emptied a pane whose session went away. Carries the whole tree: there
    /// are no incremental layout deltas.
    LayoutState(LayoutStateNotificationPayload),
}

pub struct SessionEntry {
    pub id: String,
    summary: RwLock<SessionSummary>,
    transcript_count: AtomicU64,
    adapter: tokio::sync::Mutex<Option<Arc<Adapter>>>,
    pty: tokio::sync::Mutex<PtyState>,
    /// Set by [`SessionManager::delete`] before tearing down the adapter so
    /// the drain task and event handler stop writing storage after the
    /// session has been removed.
    deleted: AtomicBool,
    /// Set by [`SessionManager::archive`] *before* it terminates the adapter,
    /// so the `drain_adapter` Closed handler — which fires as a result of that
    /// termination and races the archive bookkeeping — keeps `archived = true`
    /// in the state it persists and broadcasts. Without it the Closed handler
    /// can win the race and re-broadcast/persist `archived = false`, leaving
    /// the session looking merely stopped (the "archive only closes it, needs
    /// a second archive" bug). Cleared by [`SessionManager::restart`].
    archived: AtomicBool,
    /// Set the first time we kick off an auto-title generation for this
    /// session. Stops a flurry of user messages from spawning multiple
    /// title-gen processes; a failed title-gen leaves the title unset
    /// and the session keeps its hash-derived display name.
    title_gen_attempted: AtomicBool,
    /// Monotonic suggestion-generation counter (spec 0109). Bumped when a
    /// turn ends and generation spawns; a finished generation broadcasts
    /// its hand only if the counter still matches, so a newer turn — or a
    /// user prompt racing the generator — silently discards a stale hand.
    /// Also serves as the in-flight guard: a bump invalidates any older
    /// run without needing a separate flag.
    suggest_gen: AtomicU64,
    /// PTY-input accumulator used to derive the auto-title prompt for
    /// adapters that don't echo user input back as `SessionEvent::Message`
    /// events (shell / claude / codex interactive). Decodes printable
    /// ASCII through a tiny ESC-sequence state machine; first CR/LF
    /// closes the buffer and feeds it to title-gen.
    pty_input_capture: tokio::sync::Mutex<PtyInputCapture>,
    /// Sending half of the session's ordered PTY-input delivery queue
    /// (spec 0087); `None` until the first input arrives. A dedicated
    /// per-session writer task drains the queue and performs the adapter
    /// `session.pty_input` round-trips, so the serial IPC dispatch loop
    /// ACKs typing on *enqueue* and never waits on a slow adapter — see
    /// `session::pty` for the mechanism. The writer exits on its own when
    /// this entry is dropped (the sender lives here, closing the channel),
    /// so delete/restart need no explicit teardown. `std::sync::Mutex`:
    /// locked only to install/clone the sender, never across an `.await`.
    pty_input_queue: std::sync::Mutex<Option<mpsc::Sender<pty::PtyInputJob>>>,
    /// Per-session tool-call lifecycle map. Updated from
    /// `SessionEvent::TaskStart` / `TaskBackgrounded` / `TaskEnd`.
    /// Surfaced by `session.list_tasks` for the TUI `/tasks` popup
    /// and the MCP `agentd_get_tasks` tool.
    pub tasks: tokio::sync::Mutex<TaskRegistry>,
    /// Per-connection PTY-size ownership policy. A POSIX PTY can only have one
    /// size, so explicit engagement (input or a claiming resize) chooses one
    /// connection's remembered viewport. Passive resize reports from other
    /// clients update their remembered viewport without stealing ownership.
    /// See `SessionManager::note_pty_activity`.
    pub pty_client_policy: std::sync::Mutex<PtyClientPolicy>,
    /// In-memory "the session did something while you weren't looking" flag.
    /// Set when genuine activity (PTY output, messages, tool calls, terminal
    /// events) arrives while the session is NOT the focused one; cleared by
    /// `mark_seen`. Gates the `needs_attention` marker so a session going idle
    /// only flags when there was unseen activity — not from the user's own
    /// keystrokes echoing in a focused session. Not persisted. See spec 0054.
    unseen_activity: AtomicBool,
    /// Start (epoch ms) of the current PTY output burst; 0 = no burst yet. A
    /// burst is a run of active output events with gaps shorter than
    /// [`PTY_QUIESCENCE`]; only bursts that have persisted for
    /// [`PTY_BLIP_WINDOW`] count as genuine activity for quiescence-detected
    /// harnesses, filtering out idle housekeeping repaints. Advanced by
    /// [`pty_burst_advance`]. Not persisted — restarts as "no burst".
    pty_burst_start_ms: AtomicI64,
    /// Epoch ms when a respawn-with-repaint began settling; 0 = not settling.
    /// A respawned full-screen child redraws its *old* conversation (and the
    /// post-resume force-redraw cycle repaints it again) — sustained output
    /// that would otherwise defeat the blip filter and read as genuine unseen
    /// activity, lighting the `needs_attention` dot on every backgrounded
    /// session after a daemon restart. While set, PTY output neither marks
    /// unseen activity nor undoes a quiescence-driven `AwaitingInput`.
    /// Cleared by the quiescence poll via [`resume_settle_over`]. Not
    /// persisted. See spec 0054.
    resume_settling_since_ms: AtomicI64,
    /// Carry-over for OSC 11 background-probe scanning across PTY chunk
    /// boundaries (spec 0073): holds an ambiguous trailing prefix of a query
    /// (≤7 bytes) withheld from the downstream stream until the next chunk
    /// resolves it. `std::sync::Mutex`: touched only by the session's single
    /// adapter-drain task, never held across an `.await`.
    osc11_tail: std::sync::Mutex<Vec<u8>>,
}

/// Tracking state for the per-session "explicitly engaged client wins" PTY
/// resize policy. Kept on `SessionEntry`. `std::sync::Mutex` (not tokio) is
/// deliberate — every critical section is tiny and never crosses an `.await`.
#[derive(Debug, Default)]
pub struct PtyClientPolicy {
    /// Last viewport reported by each live daemon connection.
    pub clients: HashMap<u64, PtyClientViewport>,
    /// Connection whose viewport currently owns the OS PTY geometry.
    pub owner: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct PtyClientViewport {
    pub kind: crate::server::ClientKind,
    pub size: Option<(u16, u16)>,
}

impl PtyClientPolicy {
    /// Record input or a viewport report and return the size that should be
    /// applied to the real PTY, if any, plus whether ownership moved to a
    /// different connection. A switch matters even when no resize follows:
    /// clients track ownership through personalized resize events, so the
    /// caller must still announce a size-preserving handoff.
    fn note(
        &mut self,
        conn_id: u64,
        kind: crate::server::ClientKind,
        resize_to: Option<(u16, u16)>,
        claim: bool,
    ) -> (Option<(u16, u16)>, bool) {
        let viewport = self
            .clients
            .entry(conn_id)
            .or_insert(PtyClientViewport { kind, size: None });
        viewport.kind = kind;
        if let Some(size) = resize_to {
            viewport.size = Some(size);
        }

        if claim {
            let switched = self.owner != Some(conn_id);
            self.owner = Some(conn_id);
            // A claiming resize always applies its supplied dimensions.
            // Input only needs a resize when ownership actually switches.
            let resize = if resize_to.is_some() || switched {
                viewport.size
            } else {
                None
            };
            return (resize, switched);
        }

        // Passive reports only resize when they come from the current owner.
        // Reports from every other connection are remembered for its next
        // click/keystroke, but cannot steal geometry in the background.
        let resize = (self.owner == Some(conn_id)).then_some(resize_to).flatten();
        (resize, false)
    }
}

/// Bounded log of recent + in-flight task entries. Held inside
/// each `SessionEntry`; rebuilt from event replay on rehydrate.
#[derive(Default)]
pub struct TaskRegistry {
    /// Newest-first list. Capped at [`TASK_REGISTRY_CAP`] entries;
    /// terminal-state oldest are evicted when over.
    entries: Vec<construct_protocol::TaskInfo>,
}

/// How many tasks (running + recent terminal) we keep per session.
/// Bounded so the registry doesn't grow forever; recent enough that
/// `/tasks` shows useful history.
const TASK_REGISTRY_CAP: usize = 50;

impl TaskRegistry {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn upsert_start(
        &mut self,
        call_id: String,
        tool: String,
        args_summary: String,
        started_at_ms: i64,
    ) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            // Restart-of-same-call_id is unusual but harmless; treat
            // as a fresh entry by resetting state.
            e.tool = tool;
            e.args_summary = args_summary;
            e.state = construct_protocol::TaskState::Running;
            e.started_at_ms = started_at_ms;
            e.backgrounded_at_ms = None;
            e.ended_at_ms = None;
            e.output_preview = None;
            e.ok = false;
            return;
        }
        self.entries.push(construct_protocol::TaskInfo {
            call_id,
            tool,
            args_summary,
            state: construct_protocol::TaskState::Running,
            started_at_ms,
            backgrounded_at_ms: None,
            ended_at_ms: None,
            output_preview: None,
            ok: false,
        });
        self.gc_terminal();
    }

    pub fn mark_backgrounded(&mut self, call_id: &str, at_ms: i64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            e.state = construct_protocol::TaskState::Backgrounded;
            e.backgrounded_at_ms = Some(at_ms);
        }
    }

    pub fn mark_end(&mut self, call_id: &str, ok: bool, output_preview: String, at_ms: i64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            e.state = if ok {
                construct_protocol::TaskState::Completed
            } else {
                construct_protocol::TaskState::Failed
            };
            e.ended_at_ms = Some(at_ms);
            e.output_preview = Some(output_preview);
            e.ok = ok;
        }
    }

    pub fn snapshot(&self) -> Vec<construct_protocol::TaskInfo> {
        self.entries.clone()
    }

    fn gc_terminal(&mut self) {
        if self.entries.len() <= TASK_REGISTRY_CAP {
            return;
        }
        // Evict oldest terminal entries first; keep running / bg.
        let mut to_remove: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.state,
                    construct_protocol::TaskState::Completed
                        | construct_protocol::TaskState::Failed
                        | construct_protocol::TaskState::Cancelled
                )
            })
            .map(|(i, _)| i)
            .collect();
        // Oldest first (lower index = older since we push at end).
        to_remove.sort();
        while self.entries.len() > TASK_REGISTRY_CAP {
            match to_remove.first().copied() {
                Some(i) => {
                    self.entries.remove(i);
                    to_remove.remove(0);
                    for x in to_remove.iter_mut() {
                        *x = x.saturating_sub(1);
                    }
                }
                None => break, // everything live; nothing to evict
            }
        }
    }
}

#[derive(Default)]
struct PtyInputCapture {
    buf: String,
    /// 0 = not in an escape; 1 = saw ESC; 2 = saw ESC[ (CSI); 3 = saw ESC O (SS3).
    esc: u8,
    last_was_cr: bool,
}

fn should_record_pty_user_message(harness: &str) -> bool {
    matches!(
        harness,
        "claude" | "antigravity" | "agy" | "grok" | "hermes" | "muse"
    )
}

/// How "say this to the session as if the user typed it" has to be framed for
/// a given harness. Shared by every daemon-originated delivery — Playbook Run,
/// verb-drift escalation, and operator channel deliveries — because the framing
/// is a property of the harness, not of the feature doing the talking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionInputDelivery {
    AdapterInput,
    ExternalPtyTypedSubmit,
    PtySubmit,
}

fn session_input_delivery(
    summary: &construct_protocol::SessionSummary,
) -> SessionInputDelivery {
    // `has_pty` describes an adapter's capability, while `mode` describes
    // the shape of this particular session. A PTY-capable adapter running in
    // headless mode still consumes structured `session.input`; sending a
    // bracketed paste to it leaves the turn stranded in the adapter inbox.
    if !summary.has_pty || summary.mode.as_deref() == Some("headless") {
        SessionInputDelivery::AdapterInput
    } else if matches!(
        summary.harness.as_str(),
        "claude" | "codex" | "antigravity" | "agy" | "grok" | "hermes" | "muse"
    ) {
        SessionInputDelivery::ExternalPtyTypedSubmit
    } else {
        SessionInputDelivery::PtySubmit
    }
}

/// Pure decision step behind [`SessionManager::wait_for_fork_ready`]:
/// `Some(true)` the fork can take its prompt, `Some(false)` give up (dead
/// session or `max_wait` exhausted), `None` keep polling.
///
/// The ready signal is the harness's own `AwaitingInput`, not the shape of
/// its output. `settle` is only a fallback for harnesses that never report
/// it, and it is deliberately long enough that a pause *inside* a startup
/// draw cannot be mistaken for the end of one.
///
/// `since_ms` matters: a summary can still hold pre-spawn state when this
/// first runs, so nothing counts as ready until the fork has drawn something
/// at or after the moment it was created.
fn fork_ready_outcome(
    state: construct_protocol::SessionState,
    last_pty_at_ms: Option<i64>,
    since_ms: i64,
    now_ms: i64,
    elapsed: Duration,
    settle: Duration,
    max_wait: Duration,
) -> Option<bool> {
    use construct_protocol::SessionState;
    if matches!(state, SessionState::Errored | SessionState::Done) {
        return Some(false);
    }
    if let Some(drawn_at) = last_pty_at_ms.filter(|t| *t >= since_ms) {
        if state == SessionState::AwaitingInput {
            return Some(true);
        }
        if now_ms.saturating_sub(drawn_at) >= settle.as_millis() as i64 {
            return Some(true);
        }
    }
    if elapsed >= max_wait {
        return Some(false);
    }
    None
}

/// Frame a playbook prompt as a bracketed paste for delivery to an external
/// agent TUI. The body is wrapped in the `ESC[200~` / `ESC[201~` markers a
/// real terminal sends around a paste; any embedded `ESC[201~` is stripped
/// first so it can't terminate the paste early (the same paste-injection
/// guard real terminals apply).
fn playbook_bracketed_paste_bytes(prompt: &str) -> Vec<u8> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    let sanitized = prompt.replace("\x1b[201~", "");
    let mut bytes = Vec::with_capacity(START.len() + sanitized.len() + END.len());
    bytes.extend_from_slice(START);
    bytes.extend_from_slice(sanitized.as_bytes());
    bytes.extend_from_slice(END);
    bytes
}

/// Frame a playbook prompt for delivery to a PTY-backed session that runs its
/// own line editor (smith, shell). The prompt is terminated with CR (`\r`) —
/// the byte a real terminal's Enter key sends — not LF (`\n`): smith's line
/// editor submits on CR and treats LF as "insert a newline into the buffer",
/// so an LF terminator would leave the whole prompt sitting unsubmitted in the
/// editor. Shell PTYs map CR→LF via the line discipline (ICRNL), so submission
/// works there too.
fn playbook_pty_submit_bytes(prompt: &str) -> Vec<u8> {
    let mut bytes = prompt.as_bytes().to_vec();
    bytes.push(b'\r');
    bytes
}

fn playbook_run_instructions() -> Vec<String> {
    vec![
        "Execute this construct playbook as an autonomous run.".to_string(),
        "Shimmer semantics: a shimmering block means 'work on this block is still pending in this run — queued, in progress, or not yet done; outcome unknown'. No shimmer means 'settled — done, skipped, or no work needed'. Pending is about the state of the work, not how it runs. A Run starts every executed block shimmering. Declare status with construct_playbook_edit: `pending` maps each stable block ref to a concise (≤10-word) hover status, while `settled` is an array of refs to clear. Editing semantic text advances a block's ref, so set `keep_pending: true` when changing unfinished work; the compact edit response returns any new refs. Never leave settled work shimmering or drop shimmer from work still in flight.".to_string(),
        "Planning pass — this MUST be your first playbook action, before doing or delegating work: read the playbook to obtain block ids, then make one status-only construct_playbook_edit with a `pending` map for every still-pending block and `settle_others: true`. No text edit is needed. This makes skipped/no-work blocks settle immediately and keeps planned work shimmering.".to_string(),
        "Treat playbook_run.markdown as free-form instructions and state for this turn, not as a request for a one-shot status report or as a fixed task-management schema.".to_string(),
        "Infer the user's intended objective from the document structure and prose, then keep taking useful next actions while there is actionable work you can do.".to_string(),
        "Do not ask the user to run the playbook again; if the document still implies useful work you can perform, continue in this turn.".to_string(),
        "Record meaningful state changes or results on the playbook with construct_playbook_edit (anchored find/replace edits that merge with concurrent human edits; use construct_playbook_update only for a wholesale rewrite).".to_string(),
        "When moving or reclassifying existing text between headings or sections, do it with one construct_playbook_edit call containing multiple `edits` entries: one replacement removes the text from the old location and another replacement inserts it in the new location. Do not remove in one tool call and add in a later tool call, because viewers can briefly see the block disappear.".to_string(),
        "Keep the playbook clean: do not add new sections, notes, bullet points, or execution details unless the playbook content or the user's instructions explicitly request them. Only update what the playbook already implies needs updating. The playbook should be at least as concise and readable after a run as it was before.".to_string(),
        "If blocked, write the blocker and next required external action on the playbook before ending.".to_string(),
        "Smart clips are Markdown-native typed references. The clip_id attribute identifies a specific clip instance, not the target itself; preserve clip_id values when editing existing clips. You may insert smart clips into the playbook at your discretion even without an explicit user request.".to_string(),
        "The full construct Markdown dialect is valid in the playbook: timeline and table blocks, agentd action links, and smart clips render on the playbook surface just as they do in widgets. Action links express user intent only when the user activates them — running the playbook never triggers the links it contains, and you must not treat link text as an instruction to perform that action.".to_string(),
        "When the playbook implies independent subtasks that can run concurrently or in isolation, prefer delegating each to a child agent via `agentd_subagent_create` rather than executing them inline in this session; this keeps the playbook session as a coordinator and lets smart clips reference each subagent's live output via `@{session:<id>}`.".to_string(),
    ]
}

fn playbook_execution_prompt() -> String {
    "Run the current construct playbook autonomously. Before doing work, call agentd_context (or construct_context if you are using MCP) and read the playbook_run field for the latest playbook content, smart clip reference, and run instructions. If playbook_run is unavailable, read the current playbook with the playbook get tool before acting. Then, before starting or delegating any task, your first playbook action must be one status-only construct_playbook_edit whose pending map assigns every still-pending block's stable id a concise status and whose settle_others flag is true, so omitted blocks settle immediately."
        .to_string()
}

fn playbook_execution_prompt_with_comment(comment: Option<&str>) -> String {
    let mut prompt = playbook_execution_prompt();
    if let Some(comment) = comment {
        let one_line = comment.split_whitespace().collect::<Vec<_>>().join(" ");
        if !one_line.is_empty() {
            prompt.push_str("\n\nAdditional user instruction for this Run: ");
            prompt.push_str(&one_line);
        }
    }
    prompt
}

fn forked_playbook_execution_prompt(
    owner_session_id: &str,
    fork_session_id: &str,
    comment: Option<&str>,
) -> String {
    let mut prompt = playbook_execution_prompt_with_comment(comment);
    prompt.push_str(&format!(
        "\n\nYou are running in an interactive fork (session `{fork_session_id}`). The Playbook \
         you must read and update belongs to session `{owner_session_id}`. Always pass \
         `session_id: \"{owner_session_id}\"` to construct_playbook_get and every \
         construct_playbook_edit/update call; do not edit a Playbook belonging to this fork. Apply \
         progress and results directly to that owner document as you work. Do not return a \
         result for the owner to merge.\
         \n\nWhen the dispatched work is fully complete, close this fork with a two-step \
         finish: first make a final construct_playbook_edit that settles every block you were \
         dispatched for on the owner Playbook (none of your blocks may be left shimmering), \
         then — as your last action — call the archive-session tool \
         (construct_archive_session, or agentd_archive_session if you have the agentd-prefixed \
         toolset) with `session_id: \"{fork_session_id}\"`. Never archive before the settle \
         edit has succeeded. Archiving is a soft close: the transcript and the owner Playbook's \
         session clip stay valid. If work remains pending, you are blocked, or the user has \
         joined the conversation in this fork, leave the session open instead of archiving."
    ));
    prompt
}

/// A selection-Run execution fork's dispatch record (spec 0137), tracked
/// from dispatch until the fork closes (archive or delete). Closing the
/// fork settles the shimmer of the blocks it was dispatched for — the
/// deterministic backstop behind the prompt's own settle-then-archive
/// contract, so a fork that archives without settling can never leave the
/// owner Playbook shimmering forever.
#[derive(Clone)]
struct RunForkDispatch {
    /// The session whose Playbook document this fork was dispatched from.
    owner_session_id: String,
    /// The dispatched text as annotated at dispatch time (selection plus
    /// the fork's `@{session:<id>}` clip when annotation succeeded). Its
    /// block ids settle blocks the fork never edited; blocks whose text
    /// drifted are caught separately by scanning the live document for the
    /// fork's clip.
    anchor: String,
}

/// A Playbook-verb session awaiting a structured result to merge back into
/// its owning Playbook (spec 0089), tracked from the moment its provisional
/// clip-annotation edit lands until either a mechanical merge or an
/// escalation resolves it.
#[derive(Clone)]
struct PendingVerbMerge {
    /// The session whose Playbook document this verb is refining.
    playbook_session_id: String,
    verb: construct_protocol::PlaybookVerb,
    /// The text currently anchoring this verb's selection in the document —
    /// the original selection with the verb session's `@{session:<id>}` clip
    /// appended by the provisional edit. Authoritative for drift detection:
    /// the verb session's own belief about its anchor (if any) is advisory
    /// only, since the daemon is the one source of truth for the live
    /// document.
    anchor: String,
    /// Where the verb session is instructed to write its result JSON — its
    /// own session-widgets directory, already auto-approved for that
    /// session's harness (spec 0089 "enforced by construction": the verb
    /// session has no Playbook-editing tool at all, native or MCP, so this
    /// file drop is the only channel it has back to the document).
    result_file: PathBuf,
}

/// The JSON contract a Playbook-verb session writes to its result file (spec
/// 0087). `effect` is accepted but not trusted — the daemon always applies
/// the verb definition's own declared effect, so a verb session cannot
/// change how its result is merged just by mis-stating this field.
#[derive(Debug, serde::Deserialize)]
struct VerbResultPayload {
    #[serde(default)]
    #[allow(dead_code)]
    effect: Option<String>,
    content: String,
}

/// Cap on how much of the full Playbook document is inlined directly into a
/// verb session's prompt (spec 0089). Above this the document is truncated
/// with a pointer to the live `agentd_playbook_get`/`construct_playbook_get`
/// tool instead of growing the prompt unboundedly — a fresh read is also the
/// only way an interactive verb sees a document that changed after spawn.
const PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS: usize = 100_000;

/// Build the initial prompt for a Playbook-verb session (spec 0089): the
/// verb's own purpose prompt, the full Playbook document as background
/// context, the selection framed as the session's entire jurisdiction, the
/// optional free-text instruction (same composition rule as selection Run's
/// comment, spec 0137), and the structured-return contract — including that
/// the session has no Playbook-editing tool and must not attempt to act like
/// it does.
///
/// The verb body may place any of these itself with `{{ playbook.content }}`,
/// `{{ playbook.selected_text }}`, and `{{ playbook.additional_instruction }}`
/// template placeholders (spec 0089): a referenced variable is substituted
/// in place and its default framing section below is suppressed, so an
/// author who positions a value never gets it twice. The structured-return
/// contract is not templatable — it always applies.
fn playbook_verb_prompt(
    verb: &construct_protocol::PlaybookVerb,
    owner_session_id: &str,
    full_document: &str,
    selection: &str,
    comment: Option<&str>,
    direct_target: Option<(&str, &str)>,
) -> String {
    use crate::playbook_verbs::{
        prompt_references_var, render_verb_prompt, TEMPLATE_VAR_ADDITIONAL_INSTRUCTION,
        TEMPLATE_VAR_CONTENT, TEMPLATE_VAR_SELECTED_TEXT,
    };
    use construct_protocol::{PlaybookVerbEffect, PlaybookVerbInteraction};

    let doc_char_count = full_document.chars().count();
    let doc_truncated = doc_char_count > PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS;
    let doc_excerpt: String = full_document
        .chars()
        .take(PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS)
        .collect();
    // The template substitution carries its own truncation pointer, since a
    // template author controls the surrounding text and gets no framing
    // header to explain the cut.
    let doc_for_template = if doc_truncated {
        format!(
            "{doc_excerpt}\n\n[... truncated to the first {PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS} \
             characters — call agentd_playbook_get, or construct_playbook_get if you are using \
             MCP, with session_id \"{owner_session_id}\" for the rest, or for a fresh read if \
             the document may have changed since]"
        )
    } else {
        doc_excerpt.clone()
    };
    let one_line_instruction = comment
        .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();

    let mut prompt = render_verb_prompt(
        verb.prompt.trim(),
        &[
            (TEMPLATE_VAR_CONTENT, doc_for_template.as_str()),
            (TEMPLATE_VAR_SELECTED_TEXT, selection),
            (
                TEMPLATE_VAR_ADDITIONAL_INSTRUCTION,
                one_line_instruction.as_str(),
            ),
        ],
    );
    if !prompt_references_var(&verb.prompt, TEMPLATE_VAR_CONTENT) {
        prompt.push_str("\n\n---\n\nFor context, here is the full Playbook (orchestration) document this selection is part of");
        if doc_truncated {
            prompt.push_str(&format!(
                " (truncated to its first {PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS} characters — call \
                 agentd_playbook_get, or construct_playbook_get if you are using MCP, with \
                 session_id \"{owner_session_id}\" for the rest, or for a fresh read if the \
                 document may have changed since)"
            ));
        }
        prompt.push_str(":\n\n");
        prompt.push_str(&doc_excerpt);
        if doc_truncated {
            prompt.push_str("\n\n[... truncated ...]");
        }
    }
    if !prompt_references_var(&verb.prompt, TEMPLATE_VAR_SELECTED_TEXT) {
        prompt.push_str(
            "\n\n---\n\nYour jurisdiction is exactly the following selected Markdown — a substring \
             of the document above. Use the rest of the document only as context: do not describe, \
             reference, or act on anything outside this selection; your result applies to this \
             selection alone.\n\n",
        );
        prompt.push_str(selection);
    }
    prompt.push_str("\n\n---\n\n");
    if !prompt_references_var(&verb.prompt, TEMPLATE_VAR_ADDITIONAL_INSTRUCTION)
        && !one_line_instruction.is_empty()
    {
        prompt.push_str("Additional user instruction for this verb: ");
        prompt.push_str(&one_line_instruction);
        prompt.push_str("\n\n");
    }
    match verb.interaction {
        PlaybookVerbInteraction::Interactive => prompt.push_str(
            "This is an interactive verb: do not make the final document edit yet. Hold a focused dialogue \
             with the user in this session first, following the questioning approach above, \
             until you have enough to finish. Only once you decide to stop should \
             you perform the completion action described below.\n\n",
        ),
        PlaybookVerbInteraction::SingleShot => {
            prompt.push_str("Produce your result now without asking the user anything.\n\n")
        }
    }
    let content_meaning = match verb.effect {
        PlaybookVerbEffect::Annotate => {
            "only the new Markdown to add after the selection — do not restate the selection itself"
        }
        PlaybookVerbEffect::Rewrite => "the complete replacement Markdown for the selection",
    };
    if let Some((target_session_id, live_anchor)) = direct_target {
        let edit_requirement = match verb.effect {
            PlaybookVerbEffect::Annotate => {
                "the live anchor unchanged, followed by only the new annotation Markdown"
            }
            PlaybookVerbEffect::Rewrite => {
                "the complete replacement Markdown for the selection, retaining the provenance session clip from the live anchor"
            }
        };
        prompt.push_str(&format!(
            "Update the Playbook directly when ready; do not return a result for another session \
             to merge. Read the latest document with session_id `{target_session_id}`, then call \
             construct_playbook_edit (or agentd_playbook_edit) with that same explicit session_id. \
             Your anchored old_string is initially the exact Markdown below (re-read and choose \
             sufficient surrounding context if concurrent edits changed it):\n\n{live_anchor}\n\nFor this \
             `{effect}` verb, the edit's new_string must contain {edit_requirement}. Settle the \
             affected block refs in the same edit. Preserve any @{{session:...}} or \
             @{{harness:...}} smart clips unless removing one is this verb's explicit purpose. \
             Keep the edit focused; do not pad it.",
            effect = match verb.effect {
                PlaybookVerbEffect::Annotate => "annotate",
                PlaybookVerbEffect::Rewrite => "rewrite",
            }
        ));
    } else {
        prompt.push_str(&format!(
            "You may read this Playbook document (agentd_playbook_get / construct_playbook_get) but \
             have no tool, native or MCP, that can edit it — do not attempt to use \
             construct_playbook_edit, agentd_playbook_edit, or any similar tool; the platform applies \
             your result on your behalf once you deliver it. When you are ready, \
             call agentd_context (or construct_context if you are using MCP) and read \
             `session_widgets.dir` from the response, then write a single JSON object to \
             `<that dir>/verb-result.json` with exactly one field, `content`: {content_meaning}. \
             Preserve any @{{session:...}} or @{{harness:...}} smart clips present in the \
             selection unless removing one is this verb's explicit purpose. Keep the result \
             focused; do not pad it."
        ));
    }
    prompt
}

fn playbook_run_context(
    playbook: &PlaybookDocument,
    scope: &str,
    markdown: &str,
) -> agent_context::PlaybookRunContext {
    agent_context::PlaybookRunContext {
        session_id: playbook.session_id.clone(),
        playbook_version: playbook.version,
        playbook_updated_at_ms: playbook.updated_at_ms,
        scope: scope.to_string(),
        instructions: playbook_run_instructions(),
        smart_clips: dialect::extensions_for_surface(dialect::SURFACE_PLAYBOOK)
            .filter(|ext| ext.kind == dialect::KIND_REFERENCE)
            .map(|ext| agent_context::PlaybookSmartClipReference {
                type_name: ext.name.to_string(),
                syntax: ext.syntax.to_string(),
                description: ext.description.to_string(),
            })
            .collect(),
        markdown: markdown.to_string(),
    }
}

/// One selected playbook block matching the instant-dispatch fast-path shape
/// (spec 0066): a list item whose text contains exactly one smart clip, and
/// that clip is `@{harness:<name>}`.
struct PlaybookDispatchItem {
    /// The block's raw source text (list marker included), pre-edit — the
    /// anchor for the append edit that adds the `@{session:<id>}` clip.
    text: String,
    /// The harness clip's target, e.g. "codex".
    harness: String,
    /// The item's prose with its list marker and clip syntax stripped — the
    /// dispatched subagent's initial prompt.
    prompt: String,
}

/// If every block in `blocks` is a list item containing exactly one smart
/// clip and that clip is `@{harness:<name>}`, returns one
/// [`PlaybookDispatchItem`] per block in document order. Otherwise — a
/// heading/paragraph block, a missing/non-harness/ambiguous clip, or a
/// harness name that resolves to nothing — returns `None` so the caller falls
/// the *whole* selection through to the normal execute path rather than
/// fast-pathing part of a mixed selection (spec 0066).
fn playbook_dispatch_plan(
    blocks: &[construct_protocol::PlaybookBlockSpan],
) -> Option<Vec<PlaybookDispatchItem>> {
    if blocks.is_empty() {
        return None;
    }
    let mut items = Vec::with_capacity(blocks.len());
    for block in blocks {
        let first_line = block.text.lines().next().unwrap_or("").trim();
        if !construct_protocol::playbook_is_list_item(first_line) {
            return None;
        }
        let clips = construct_protocol::playbook_scan_smart_clips(&block.text);
        if clips.len() != 1 {
            return None;
        }
        let clip = &clips[0];
        let harness = clip.target.trim();
        if clip.type_name != "harness" || harness.is_empty() {
            return None;
        }
        let mut without_clip = String::with_capacity(block.text.len());
        without_clip.push_str(&block.text[..clip.start]);
        without_clip.push_str(&block.text[clip.end..]);
        let trimmed = without_clip.trim();
        let body = construct_protocol::playbook_list_item_text(trimmed).unwrap_or(trimmed);
        let prompt = body.split_whitespace().collect::<Vec<_>>().join(" ");
        if prompt.is_empty() {
            return None;
        }
        items.push(PlaybookDispatchItem {
            text: block.text.clone(),
            harness: harness.to_string(),
            prompt,
        });
    }
    Some(items)
}

/// Legacy content ids of every block an edit flagged `keep_pending` introduces:
/// the blocks whose work the edit keeps in flight. These ids are content-
/// derived, so each block parsed from a keep_pending edit's `new_string` in
/// isolation has the same content id as in the post-edit document — and `new_string`
/// may span several blocks (e.g. a heading plus a moved item), so ALL of them
/// are returned, not just the first. The caller drops ids that already existed
/// pre-edit (so a re-stated heading is not re-lit) and `narrow_playbook_run`
/// ignores any id absent from the post-edit document, so the net effect is to
/// re-add exactly the new/changed blocks in the same narrowing call that drops
/// the old ones — the pending set never transiently empties.
fn playbook_edit_keep_ids(
    edits: &[construct_protocol::PlaybookEdit],
) -> std::collections::HashSet<String> {
    edits
        .iter()
        .filter(|e| e.keep_pending)
        .flat_map(|e| construct_protocol::playbook_block_spans(&e.new_string))
        .map(|span| span.id)
        .collect()
}

fn playbook_default_cursor_label(kind: &str) -> String {
    match kind {
        "web" => "Web".to_string(),
        "tui" => "TUI".to_string(),
        other => other.to_string(),
    }
}

fn playbook_unique_cursor_label(
    cursors: &HashMap<u64, construct_protocol::PlaybookCursor>,
    conn_id: u64,
    requested: &str,
    kind: &str,
) -> String {
    let fallback = playbook_default_cursor_label(kind);
    let base = requested.trim();
    let base = if base.is_empty() {
        fallback.as_str()
    } else {
        base
    };
    let used: std::collections::HashSet<&str> = cursors
        .iter()
        .filter(|(id, cursor)| **id != conn_id && cursor.active)
        .map(|(_, cursor)| cursor.label.as_str())
        .collect();
    let generic = matches!(base, "TUI" | "Web" | "tui" | "web");
    if !generic && !used.contains(base) {
        return base.to_string();
    }
    for n in 1.. {
        let candidate = format!("{base} {n}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaybookCursorReplacement {
    start: usize,
    old_len: usize,
    new_len: usize,
}

fn playbook_cursor_replacements(
    base: &str,
    edits: &[construct_protocol::PlaybookEdit],
) -> Result<Vec<PlaybookCursorReplacement>> {
    let mut working = base.to_string();
    let mut replacements = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_string.is_empty() {
            let prefix_len = usize::from(!working.is_empty() && !working.ends_with('\n'));
            let inserted = prefix_len + edit.new_string.chars().count();
            replacements.push(PlaybookCursorReplacement {
                start: working.chars().count(),
                old_len: 0,
                new_len: inserted,
            });
            if working.is_empty() {
                working = edit.new_string.clone();
            } else {
                if !working.ends_with('\n') {
                    working.push('\n');
                }
                working.push_str(&edit.new_string);
            }
            continue;
        }

        let match_byte_offsets: Vec<usize> = working
            .match_indices(&edit.old_string)
            .map(|(offset, _)| offset)
            .collect();
        match match_byte_offsets.len() {
            0 => anyhow::bail!(
                "playbook edit {}: old_string not found in the current playbook:\n{}",
                i + 1,
                edit.old_string
            ),
            n if n > 1 && !edit.replace_all => anyhow::bail!(
                "playbook edit {}: old_string is not unique ({} matches); add surrounding context or set replace_all",
                i + 1,
                n
            ),
            _ => {}
        }

        let old_chars: Vec<char> = edit.old_string.chars().collect();
        let new_chars: Vec<char> = edit.new_string.chars().collect();
        let mut prefix = 0usize;
        while prefix < old_chars.len()
            && prefix < new_chars.len()
            && old_chars[prefix] == new_chars[prefix]
        {
            prefix += 1;
        }
        let mut old_suffix = old_chars.len();
        let mut new_suffix = new_chars.len();
        while old_suffix > prefix
            && new_suffix > prefix
            && old_chars[old_suffix - 1] == new_chars[new_suffix - 1]
        {
            old_suffix -= 1;
            new_suffix -= 1;
        }
        let old_len = old_suffix.saturating_sub(prefix);
        let new_len = new_suffix.saturating_sub(prefix);
        let selected_offsets = if edit.replace_all {
            match_byte_offsets
        } else {
            vec![match_byte_offsets[0]]
        };
        let mut cumulative_delta = 0isize;
        for byte_offset in selected_offsets {
            let anchor_start = working[..byte_offset]
                .chars()
                .count()
                .saturating_add_signed(cumulative_delta);
            replacements.push(PlaybookCursorReplacement {
                start: anchor_start + prefix,
                old_len,
                new_len,
            });
            cumulative_delta += new_chars.len() as isize - old_chars.len() as isize;
        }
        working = if edit.replace_all {
            working.replace(&edit.old_string, &edit.new_string)
        } else {
            working.replacen(&edit.old_string, &edit.new_string, 1)
        };
    }
    Ok(replacements)
}

fn playbook_rebase_offset(offset: usize, replacements: &[PlaybookCursorReplacement]) -> usize {
    let mut pos = offset;
    for replacement in replacements {
        let start = replacement.start;
        let end = start + replacement.old_len;
        if replacement.old_len == 0 {
            if pos > start {
                pos = pos.saturating_add(replacement.new_len);
            }
            continue;
        }
        if pos < start {
            continue;
        }
        if pos >= end {
            if replacement.new_len >= replacement.old_len {
                pos = pos.saturating_add(replacement.new_len - replacement.old_len);
            } else {
                pos = pos.saturating_sub(replacement.old_len - replacement.new_len);
            }
        } else {
            let inner = pos - start;
            pos = start + inner.min(replacement.new_len);
        }
    }
    pos
}

/// The minimal char-offset span (in `after`'s coordinates) where `before`
/// and `after` actually differ, via the same common-prefix/suffix trim
/// `playbook_cursor_replacements` uses for one edit — applied here to the
/// whole before/after document instead. Used for the agent-presence
/// cursor's span (spec 0065 agent presence): unlike picking the last
/// individual edit's own replacement, this is correct even when a batch's
/// edits partially or fully cancel each other out (e.g. an edit followed by
/// one that reverts it) — it reports where the document actually ended up
/// different, not where an individual edit nominally touched. Returns
/// `None` when the two are identical.
fn playbook_edit_overall_span(before: &str, after: &str) -> Option<(usize, usize)> {
    if before == after {
        return None;
    }
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    let mut prefix = 0usize;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }
    let mut before_suffix = before_chars.len();
    let mut after_suffix = after_chars.len();
    while before_suffix > prefix
        && after_suffix > prefix
        && before_chars[before_suffix - 1] == after_chars[after_suffix - 1]
    {
        before_suffix -= 1;
        after_suffix -= 1;
    }
    Some((prefix, after_suffix))
}

impl SessionEntry {
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::SeqCst)
    }
    /// Cheap async read of the session's current SessionState —
    /// used by the loop scheduler to skip firing into a terminal
    /// session.
    pub async fn snapshot_state(&self) -> construct_protocol::SessionState {
        self.summary.read().await.state
    }
}

fn native_subagent_session_id(owner_session_id: &str, native_id: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{owner_session_id}~native~{}",
        URL_SAFE_NO_PAD.encode(native_id.as_bytes())
    )
}

/// Per-session PTY metadata. Used to hold the last known PTY dimensions
/// (so a fresh TUI attach can size its parsers correctly) and previously
/// also a 256 KiB in-memory ring of bytes for replay. Scrollback is now
/// served from the on-disk `pty.log` tail (see `pty_replay`), so the
/// in-memory ring is gone and this is just the size.
#[derive(Default)]
struct PtyState {
    size: Option<PtySize>,
}

impl SessionEntry {
    pub async fn summary(&self) -> SessionSummary {
        self.summary.read().await.clone()
    }
}

pub struct GroupEntry {
    summary: RwLock<GroupSummary>,
}

impl GroupEntry {
    pub async fn summary(&self) -> GroupSummary {
        self.summary.read().await.clone()
    }
}

use widgets::WidgetSnapshot;

pub struct SessionManager {
    storage: Arc<Storage>,
    /// The configuration in force. Swapped wholesale by a reload (spec 0190),
    /// never mutated in place — read it through [`SessionManager::config`],
    /// which clones the `Arc` out before any await. Holding the guard across
    /// one would make the enclosing future `!Send`.
    config: std::sync::RwLock<Arc<Config>>,
    /// Plugin action/event-hook runtime (spec 0152 phase 2). Set from daemon
    /// startup after construction, and replaced by a config reload so a
    /// newly installed or disabled plugin takes effect without a restart;
    /// empty when no plugins are loaded.
    plugins: std::sync::RwLock<Option<Arc<crate::plugins::PluginRuntime>>>,
    /// Handle to the operator supervisor, so any caller can ask for a
    /// definition reload without depending on its internals. Set once from
    /// daemon startup; absent in tests, which never spawn one.
    operators: std::sync::OnceLock<crate::operator_supervisor::OperatorHandle>,
    /// Handle to the config supervisor (spec 0190). Set once from daemon
    /// startup; absent in tests, which never spawn one.
    config_reloads: std::sync::OnceLock<crate::config_supervisor::ConfigHandle>,
    /// Configuration that has been read but cannot take effect until the
    /// daemon is restarted, named for a user. Sticky: it survives later
    /// reloads and client reconnects, because it is a property of this daemon
    /// process and persists until the restart it names.
    config_restart_required: std::sync::RwLock<Vec<String>>,
    /// Provider-neutral public exposure for channel-owned ingress endpoints.
    /// Separate from `operators`: future channel implementations can register
    /// endpoints without depending on operator definition internals.
    channel_publications:
        std::sync::OnceLock<crate::channel_publication::PublicationHandle>,
    adapter_runtime_dir: PathBuf,
    sessions: RwLock<HashMap<String, Arc<SessionEntry>>>,
    /// The sessions the user currently has visible/focused (via `mark_seen` or `set_focused_sessions`).
    /// Suppresses the `needs_attention` marker for these sessions. In-memory only. See spec 0054.
    focused_sessions: std::sync::Mutex<std::collections::HashSet<String>>,
    groups: RwLock<HashMap<String, Arc<GroupEntry>>>,
    /// The shared split layout every wide client renders its panes from.
    /// Daemon-owned rather than client-owned so a split made in the TUI shows
    /// up in the browser, and so pane pruning happens once here instead of
    /// each client repairing the tree independently and racing to write the
    /// result back.
    layout: std::sync::Mutex<LayoutDocument>,
    broadcast: broadcast::Sender<BroadcastMsg>,
    /// Recurring-prompt loops attached to sessions. The scheduler
    /// task (`crate::loops::run_scheduler`) iterates these.
    pub(crate) loops: Arc<crate::loops::LoopRegistry>,
    /// Set by [`Self::shutdown_adapters`] before it tells each
    /// adapter to exit, so the `drain_adapter` task can tell the
    /// resulting `AdapterMessage::Closed` events apart from real
    /// adapter crashes and *not* transition the session to `Done`.
    /// Sessions need to keep their pre-shutdown state on disk so
    /// `resume_running_sessions` picks them up on the next boot —
    /// otherwise a graceful `kill -TERM` of the daemon would mark
    /// every live session terminal and skip them on restart.
    is_shutting_down: AtomicBool,
    /// Model-route transport (specs 0113/0114/0115). Present always,
    /// inert when `[router] enabled = false`.
    pub(crate) router: Arc<crate::router::Router>,
    /// Remote-WS transport: `None` until `start_remote` is called
    /// (either by env-var-at-boot in `main.rs` or by the
    /// `remote.start` IPC method invoked from the TUI's
    /// `/remote-control` slash). Subsequent calls return the same
    /// `RemoteState` so the URL + token stay stable for the
    /// daemon's lifetime.
    ///
    /// Uses `std::sync::Mutex` deliberately — we never want to
    /// hold this guard across an `.await`, so an explicitly non-
    /// `Send` guard makes the compiler enforce that invariant.
    /// All critical sections are tiny snapshot reads / single
    /// writes.
    remote: std::sync::Mutex<Option<RemoteHandle>>,
    /// Outbound side of the channel to the remote supervisor task
    /// (`crate::remote_supervisor::run`). `start_remote` posts
    /// requests here and awaits the reply rather than spawning
    /// `serve_ws_on` directly — see the comment on
    /// `remote_supervisor` for why that indirection is mandatory.
    remote_starter: tokio::sync::mpsc::UnboundedSender<crate::remote_supervisor::SupervisorMsg>,
    /// Where the supervisor writes (and the next-boot supervisor
    /// reads) the `RemoteSnapshot`. Lives under `runtime_dir`
    /// because it's tightly coupled to the live cloudflared PID;
    /// `XDG_RUNTIME_DIR` is the natural home for such files.
    remote_snapshot_path: PathBuf,
    /// Sender side of the daemon-restart channel. Holding `Some`
    /// means `daemon.restart` has been issued and main's
    /// `tokio::select!` should observe it and `exec()` the current
    /// binary. `RestartCommand` carries the resolved exe path so
    /// the reply to the RPC caller can echo what's about to load.
    restart_tx: tokio::sync::mpsc::UnboundedSender<RestartCommand>,
    /// Dev-only: when `Some`, the remote web server serves
    /// `index.html` + `static/*` from this directory (with a live-reload
    /// poller injected) instead of the binary's embedded assets. Set via
    /// the `dev.set_assets` IPC method (debug builds only) or the
    /// `CONSTRUCT_ASSETS_DIR` env var at boot. Lets you iterate on the web
    /// UI in a worktree against a running daemon without rebuilding.
    dev_assets: std::sync::Mutex<Option<PathBuf>>,
    widget_snapshots: tokio::sync::Mutex<HashMap<String, WidgetSnapshot>>,
    playbook_runs: std::sync::Mutex<HashMap<String, PlaybookRunProgress>>,
    /// Playbook-verb sessions awaiting a structured result to merge back
    /// into their owning Playbook (spec 0089), keyed by the *verb session's*
    /// own id. In-memory only: a daemon restart mid-verb drops the pending
    /// merge (the verb session, if still running, finishes with nowhere to
    /// deliver its result — a known v1 limitation, not a data-loss risk
    /// since the result file itself survives on disk for manual recovery).
    pending_verb_merges: std::sync::Mutex<HashMap<String, PendingVerbMerge>>,
    /// Live selection-Run fork dispatches, keyed by fork session id (spec
    /// 0076): closing a tracked fork settles its dispatched blocks'
    /// shimmer. In-memory only, like `pending_verb_merges` — a daemon
    /// restart drops this content-anchor fallback for forks already in flight;
    /// terminal session clips remain the durable lifecycle relationship.
    run_fork_dispatches: std::sync::Mutex<HashMap<String, RunForkDispatch>>,
    playbook_cursors: std::sync::Mutex<HashMap<u64, construct_protocol::PlaybookCursor>>,
    /// Reserved pseudo-connection id for each session's agent-authored
    /// Playbook cursor (spec 0065 agent presence), keyed by session id and
    /// lazily allocated from the same `next_conn_id` counter as real client
    /// connections so it can never collide with one. Not cleared on session
    /// delete, matching `playbook_runs`/`playbook_cursors` which likewise
    /// outlive it — the cursor itself still ages out via the one-minute TTL.
    agent_playbook_cursor_conn_ids: std::sync::Mutex<HashMap<String, u64>>,
    /// Monotonic id handed to each client connection so its current
    /// view can be tracked and cleared on disconnect.
    next_conn_id: AtomicU64,
    /// Which session + surface each live client connection is currently
    /// viewing (`conn_id -> (session_id, view)`). Drives
    /// `chat_viewer_active`, which the `AskUserQuestion` chat-gate hook
    /// queries. A `std::sync::Mutex` is fine — every critical section is a
    /// tiny insert/remove/scan never held across an `.await`.
    conn_views: std::sync::Mutex<HashMap<u64, (String, ClientView)>>,
    /// Per-connection painted-terminal-background reports (spec 0073):
    /// `conn_id -> (report_seq, Option<rgb>)`. The most recent report among
    /// live connections is the color the daemon answers child OSC 11
    /// background probes with; `None` (background-aware client themes, e.g.
    /// matrix/basic) means "don't answer". Entries are removed on
    /// disconnect. `std::sync::Mutex`: tiny insert/remove/scan sections,
    /// never held across an `.await`.
    terminal_backgrounds: std::sync::Mutex<HashMap<u64, (u64, Option<[u8; 3]>)>>,
    /// Monotonic sequence for `terminal_backgrounds` recency ordering.
    terminal_background_seq: AtomicU64,
    /// Cache for the expensive bits of harness-availability probing
    /// (macOS keychain read, Ollama reachability) — see
    /// `crate::availability`. `std::sync::Mutex` deliberately: every
    /// critical section is a tiny read/write, never held across an
    /// `.await`.
    availability_cache: std::sync::Mutex<crate::availability::AvailabilityCache>,
    /// Cache of the most recent harness usage-probe capture per harness
    /// (spec 0086), plus the in-flight refresh guard — see
    /// `crate::usage`. `std::sync::Mutex` deliberately: every critical
    /// section is a tiny read/write, never held across an `.await` (the
    /// probe itself — session create/submit-command/sleep/delete — all runs
    /// outside the lock, in `session::usage_probe`).
    usage_cache: std::sync::Mutex<crate::usage::UsageCache>,
    /// Fleet-wide rolling window of token-usage samples (spec 0167), so a
    /// reconnecting client can seed its meter with the history that accrued
    /// while it was gone.
    cost_history: std::sync::Mutex<crate::cost_history::CostHistory>,
    /// Weak handle back to the `Arc<Self>` the daemon runs behind, bound
    /// once by [`Self::bind_self_ref`] right after construction. Lets
    /// `&self` event-path methods (which sit under many callers that don't
    /// hold an `Arc`) spawn tasks that need the full manager — the
    /// auto-title probe fallback being the first. Unbound (some unit
    /// tests) simply means those spawns are skipped.
    self_ref: std::sync::OnceLock<std::sync::Weak<SessionManager>>,
    /// Latched true the first time an ambient feature (auto-title,
    /// suggestions) actually skips work because smith has no credential
    /// (spec 0151). Reported as `degradation_observed` in
    /// `features.status` and broadcast once as `features/state` so clients
    /// can surface the degradation only on machines where it really bit.
    ambient_degraded: AtomicBool,
}

/// What the main loop should do when it receives a [`RestartCommand`].
/// All three variants flow through the same channel so the IPC handler
/// can resolve the exe + reply before the runtime tears down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAction {
    /// Re-exec the daemon in place. Adapters survive the exec and
    /// reattach, so their session-scoped MCP children keep running.
    Restart,
    /// Gracefully stop every adapter (leaving sessions resumable),
    /// then re-exec. The new daemon respawns a fresh adapter — and a
    /// fresh `construct-mcp` child — for each session.
    RestartSessions,
    /// Gracefully stop every adapter (leaving sessions resumable) and
    /// exit without re-exec'ing. Used by `daemon stop`.
    Stop,
}

/// Payload of a daemon lifecycle request (`daemon.restart` /
/// `daemon.shutdown`), sent from the IPC handler to the main loop.
/// Main resolves the exe path + args before the runtime tear-down so
/// the reply can echo what's about to load.
#[derive(Debug, Clone)]
pub struct RestartCommand {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub action: RestartAction,
}

/// Executable path captured once at daemon startup, before any
/// on-disk binary upgrade can unlink the original inode. See
/// [`capture_startup_exe`].
static STARTUP_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Record the daemon's executable path at startup. Call once,
/// early in `main`, before serving any requests.
///
/// This exists because `std::env::current_exe()` is unreliable at
/// *restart* time: the primary `/agentd restart` use case is
/// picking up an upgraded binary, and upgrades replace the file
/// via atomic rename (a new inode at the same path). On Linux,
/// `current_exe()` reads `/proc/self/exe`, which after that
/// replacement resolves to `"/path/agentd (deleted)"` — and
/// `exec()`ing that literal path fails with `ENOENT`, so the
/// daemon would never come back. Capturing the clean path at
/// startup (when the file definitely still exists) and `exec()`ing
/// *that* loads the new binary now sitting at the same path.
pub fn capture_startup_exe() {
    if let Ok(p) = std::env::current_exe() {
        let _ = STARTUP_EXE.set(p);
    }
}

/// Validate a caller-supplied restart binary: resolve it to an absolute
/// path (relative paths resolve against the *daemon's* cwd), confirm it
/// exists, is a regular file, and is executable. Returns the canonical
/// path to exec, or an error that's surfaced to the caller so a typo
/// never leaves the daemon trying to exec() a missing binary.
fn validate_restart_exe(path: &std::path::Path) -> Result<PathBuf> {
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("restart binary not found: {}", path.display()))?;
    let meta = std::fs::metadata(&abs)
        .with_context(|| format!("cannot stat restart binary: {}", abs.display()))?;
    if !meta.is_file() {
        anyhow::bail!("restart binary is not a file: {}", abs.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("restart binary is not executable: {}", abs.display());
        }
    }
    Ok(abs)
}

/// Best exe path for re-`exec()` on restart: the startup-captured
/// path if available, else `current_exe()` with any trailing
/// `" (deleted)"` marker stripped (defensive — the startup capture
/// should always win in practice).
fn restart_exe_path() -> Result<PathBuf> {
    if let Some(p) = STARTUP_EXE.get() {
        return Ok(p.clone());
    }
    let p = std::env::current_exe()?;
    if let Some(s) = p.to_str() {
        if let Some(stripped) = s.strip_suffix(" (deleted)") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(p)
}

/// Daemon-local sidecar for an active remote-WS deployment. Holds
/// the immutable `RemoteState` plus the listener-port we picked
/// (so we can construct the localhost URL without re-querying the
/// socket). Lives inside `SessionManager::remote` once the
/// listener is spawned. Visible to `remote_supervisor` because
/// that module's `handle_one` is the only place that ever
/// installs one.
pub(crate) struct RemoteHandle {
    pub(crate) state: crate::remote::RemoteState,
    pub(crate) port: u16,
}

/// Transition a session's state while maintaining its compute-time
/// accounting (spec 0080 turn info): entering `Running` opens a busy span
/// (`busy_running_since_ms`), leaving it banks the elapsed span into
/// `busy_ms`. Every state write in the daemon must go through here so the
/// accumulated compute time stays truthful.
pub(crate) fn set_state_tracked(
    s: &mut construct_protocol::SessionSummary,
    new_state: SessionState,
    now_ms: i64,
) {
    if new_state == SessionState::Running {
        if s.busy_running_since_ms.is_none() {
            s.busy_running_since_ms = Some(now_ms);
        }
        // A fresh turn outdates the previous failure: `last_error` answers
        // "errored why", and the session is no longer errored.
        s.last_error = None;
    } else if let Some(since) = s.busy_running_since_ms.take() {
        s.busy_ms = s
            .busy_ms
            .saturating_add(now_ms.saturating_sub(since).max(0) as u64);
    }
    s.state = new_state;
}

impl SessionManager {
    /// Return the live adapter for an operation, closing the session when its
    /// adapter has already disappeared. A missing adapter is terminal from a
    /// client's perspective: leaving a non-terminal summary behind traps the
    /// user in a session that cannot accept input or offer the restart flow.
    pub(crate) async fn live_adapter_or_mark_closed(
        &self,
        entry: &Arc<SessionEntry>,
    ) -> Result<Arc<Adapter>> {
        let adapter = entry.adapter.lock().await;
        if let Some(adapter) = adapter.clone() {
            return Ok(adapter);
        }

        // Keep the adapter lock while changing state so a concurrent restart
        // cannot install a new adapter between the absence check and this
        // terminal transition.
        let snapshot = {
            let mut summary = entry.summary.write().await;
            if summary.state.is_terminal() {
                None
            } else {
                set_state_tracked(
                    &mut summary,
                    SessionState::Done,
                    Utc::now().timestamp_millis(),
                );
                summary.last_event_at = Some(Utc::now());
                Some(summary.clone())
            }
        };
        drop(adapter);

        if let Some(snapshot) = snapshot {
            // Same reason as `kill`: this marks the session terminal without
            // going through the event path, so the run must be told (#1090).
            self.note_session_state_for_playbook_run(&entry.id, snapshot.state);
            let _ = self.storage.save_summary(&snapshot);
            let _ = self
                .broadcast
                .send(BroadcastMsg::State(StateNotificationPayload {
                    session: snapshot,
                }));
        }
        Err(anyhow!("session has no live adapter"))
    }

    /// Construct the manager along with the receiver side of the
    /// remote-start channel. The caller (`main.rs`) spawns the
    /// supervisor task with that receiver so on-demand
    /// `/remote-control` calls work without static recursion
    /// between `dispatch` and `serve_ws_on`.
    ///
    /// `runtime_dir` is where per-adapter Unix sockets land for
    /// the reconnectable-adapter path (PR #69); we layer the
    /// `adapters/` subdir under it.
    pub async fn new(
        storage: Arc<Storage>,
        config: Arc<Config>,
        runtime_dir: PathBuf,
    ) -> Result<(
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::remote_supervisor::SupervisorMsg>,
        tokio::sync::mpsc::UnboundedReceiver<RestartCommand>,
    )> {
        let router = crate::router::Router::new(
            storage.data_dir().to_path_buf(),
            runtime_dir.clone(),
            &config.router,
            config.smith.route_profiles(),
        );
        let summaries = storage.list_summaries()?;
        let mut sessions = HashMap::new();
        // Fleet token history (spec 0167), recovered from the same
        // transcript walk that self-heals each session's tally below — no
        // extra I/O, and no separate persistence to keep in sync.
        let history_now_ms = Utc::now().timestamp_millis();
        let history_cutoff = Utc::now()
            - chrono::Duration::seconds(crate::cost_history::WINDOW_SECS);
        let mut cost_samples: Vec<construct_protocol::TokenSample> = Vec::new();
        for s in summaries {
            // Preserve the prior state in the entry. `resume_running_sessions`
            // (called from main after construction) tries to respawn each
            // resumable session and falls back to marking Errored on failure.
            // Recover seq counter from transcript line count, and recount
            // chat messages and token tallies while we're at it —
            // `message_count`/`tokens` then self-heal for summaries saved
            // before the fields existed (or that lagged a crash). The
            // context gauge (spec 0104) restores from the LAST report seen,
            // and a Reset along the way clears it — mirroring the live fold.
            let path = storage.transcript_path(&s.id);
            let (
                count,
                message_count,
                last_message_at,
                last_message,
                last_error,
                tokens,
                context,
                context_segments,
            ) =
                if path.exists() {
                    let f = std::fs::File::open(&path)?;
                    let reader = std::io::BufReader::new(f);
                    use std::io::BufRead;
                    let mut n = 0u64;
                    let mut msgs = 0u64;
                    let mut last_msg_at: Option<chrono::DateTime<chrono::Utc>> = None;
                    // Last-message snippet slots, folded exactly as the live
                    // event path folds them so the restored snippet matches
                    // what live observation would have produced.
                    let mut last_msg_role: Option<construct_protocol::MessageRole> = None;
                    let mut last_msg_text: Option<String> = None;
                    let mut last_error: Option<String> = None;
                    let mut tally = construct_protocol::TokenTally::default();
                    let mut context: (Option<u64>, Option<u64>) = (None, None);
                    let mut segments: Vec<construct_protocol::ContextSegment> = Vec::new();
                    // The model in effect as the scan advances. Attributing a
                    // historical sample to the session's model *now* would
                    // credit the current model for work an earlier one did.
                    let mut scan_model: Option<String> = None;
                    for line in reader.lines() {
                        let line = line?;
                        if line.trim().is_empty() {
                            continue;
                        }
                        n += 1;
                        if let Ok(ts) =
                            serde_json::from_str::<construct_protocol::TimestampedEvent>(&line)
                        {
                            match ts.event {
                                SessionEvent::Message { role, ref text } => {
                                    msgs += 1;
                                    last_msg_at = Some(ts.at);
                                    construct_protocol::fold_last_message(
                                        &mut last_msg_role,
                                        &mut last_msg_text,
                                        role,
                                        text,
                                    );
                                }
                                SessionEvent::Error { ref message } => {
                                    last_error = Some(construct_protocol::snippet(message));
                                }
                                SessionEvent::Done { exit_code } if exit_code != 0 => {
                                    last_error = Some(format!("exited {exit_code}"));
                                }
                                SessionEvent::Status { state, .. }
                                    if state == SessionState::Running =>
                                {
                                    // Mirrors the live fold: a fresh turn
                                    // outdates the previous failure.
                                    last_error = None;
                                }
                                SessionEvent::ModelChanged { ref model } => {
                                    scan_model = Some(model.clone());
                                }
                                SessionEvent::Cost {
                                    tokens_in,
                                    tokens_out,
                                    tokens_cached,
                                    ref model,
                                    ..
                                } => {
                                    if ts.at >= history_cutoff {
                                        cost_samples.push(construct_protocol::TokenSample {
                                            at_ms: ts.at.timestamp_millis(),
                                            session_id: Some(s.id.clone()),
                                            model: model.clone().or_else(|| scan_model.clone()),
                                            // Cached input is a subset of the
                                            // prompt side; adding it would
                                            // double-count. It is carried
                                            // alongside so the recovered
                                            // history can still tell new work
                                            // from re-served context.
                                            tokens: tokens_in.saturating_add(tokens_out),
                                            cached: tokens_cached,
                                        });
                                    }
                                    tally.add(tokens_in, tokens_out, tokens_cached);
                                }
                                SessionEvent::ContextUsage {
                                    used_tokens,
                                    window_tokens,
                                } => {
                                    context.0 = Some(used_tokens);
                                    if window_tokens.is_some() {
                                        context.1 = window_tokens;
                                    }
                                }
                                SessionEvent::ContextBreakdown { segments: segs } => {
                                    segments = segs;
                                }
                                SessionEvent::Reset => {
                                    context = (None, None);
                                    segments.clear();
                                    last_msg_role = None;
                                    last_msg_text = None;
                                    last_error = None;
                                }
                                _ => {}
                            }
                        }
                    }
                    (
                        n,
                        msgs,
                        last_msg_at,
                        last_msg_role.zip(last_msg_text),
                        last_error,
                        tally,
                        context,
                        segments,
                    )
                } else {
                    (
                        0,
                        0,
                        None,
                        None,
                        None,
                        construct_protocol::TokenTally::default(),
                        (None, None),
                        Vec::new(),
                    )
                };
            let mut s = s;
            s.message_count = message_count;
            s.last_message_at = last_message_at;
            s.last_message_role = last_message.as_ref().map(|(role, _)| *role);
            s.last_message = last_message.map(|(_, text)| text);
            s.last_error = last_error;
            s.tokens = tokens;
            s.context_used = context.0;
            s.context_window = context.1;
            s.context_segments = context_segments;
            // Scrollback survives daemon restarts because `pty_replay`
            // serves it from the on-disk `pty.log` directly; no in-memory
            // rehydration needed.
            let pty_state = PtyState::default();
            let entry = SessionEntry {
                id: s.id.clone(),
                summary: RwLock::new(s.clone()),
                transcript_count: AtomicU64::new(count),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(pty_state),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(s.archived),
                // A title set on the previous incarnation lives in the
                // loaded summary; flagging "attempted" here stops the
                // restart from re-running title-gen for already-titled
                // sessions and is harmless for the rest.
                title_gen_attempted: AtomicBool::new(s.title.is_some() && !s.auto_title_pending),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            };
            sessions.insert(s.id.clone(), Arc::new(entry));
        }
        // Load persisted groups.
        let mut groups: HashMap<String, Arc<GroupEntry>> = HashMap::new();
        match storage.load_groups() {
            Ok(list) => {
                for g in list {
                    groups.insert(
                        g.id.clone(),
                        Arc::new(GroupEntry {
                            summary: RwLock::new(g),
                        }),
                    );
                }
            }
            Err(e) => tracing::warn!(error = ?e, "load_groups failed"),
        }

        let (broadcast, _) = broadcast::channel(BROADCAST_CAP);
        // Load each session's persisted loops into the in-memory
        // registry. Missing or unreadable per-session loop files
        // are logged + skipped.
        let session_ids: Vec<String> = sessions.keys().cloned().collect();
        // Prune the shared layout against the sessions that actually came
        // back. Sessions can vanish while the daemon is down (a deleted
        // worktree, a hand-edited data dir), and every client would otherwise
        // have to repair the tree itself and race the others writing it back.
        // Pruning empties a dead pane rather than collapsing it: the *shape*
        // of the layout is the user's.
        let layout = {
            let mut doc = storage.load_layout();
            let live: std::collections::HashSet<&str> =
                session_ids.iter().map(String::as_str).collect();
            if doc.tree.retain_sessions(&|id: &str| live.contains(id)) {
                doc.version = doc.version.saturating_add(1);
                if let Err(e) = storage.save_layout(&doc) {
                    tracing::warn!(error = ?e, "save pruned layout failed");
                }
            }
            doc
        };
        let loops = Arc::new(crate::loops::LoopRegistry::new(
            storage.data_dir().to_path_buf(),
        ));
        loops.hydrate_from_disk(&session_ids).await;
        let adapter_runtime_dir = runtime_dir.join("adapters");
        std::fs::create_dir_all(&adapter_runtime_dir).ok();
        let (remote_tx, remote_rx) = tokio::sync::mpsc::unbounded_channel();
        let (restart_tx, restart_rx) = tokio::sync::mpsc::unbounded_channel();
        let remote_snapshot_path = runtime_dir.join("remote.json");
        // Honor CONSTRUCT_ASSETS_DIR at boot in debug builds only — release
        // always serves the embedded, tamper-proof assets.
        let dev_assets = if cfg!(debug_assertions) {
            std::env::var_os("CONSTRUCT_ASSETS_DIR").map(PathBuf::from)
        } else {
            None
        };
        let widget_snapshots = session_ids
            .iter()
            .map(|id| (id.clone(), WidgetSnapshot::read(&storage, id)))
            .collect();
        Ok((
            Self {
                storage,
                config: std::sync::RwLock::new(config),
                plugins: std::sync::RwLock::new(None),
                operators: std::sync::OnceLock::new(),
                config_reloads: std::sync::OnceLock::new(),
                config_restart_required: std::sync::RwLock::new(Vec::new()),
                channel_publications: std::sync::OnceLock::new(),
                adapter_runtime_dir,
                sessions: RwLock::new(sessions),
                focused_sessions: std::sync::Mutex::new(std::collections::HashSet::new()),
                groups: RwLock::new(groups),
                layout: std::sync::Mutex::new(layout),
                broadcast,
                loops,
                is_shutting_down: AtomicBool::new(false),
                router,
                remote: std::sync::Mutex::new(None),
                remote_starter: remote_tx,
                remote_snapshot_path,
                restart_tx,
                dev_assets: std::sync::Mutex::new(dev_assets),
                widget_snapshots: tokio::sync::Mutex::new(widget_snapshots),
                playbook_runs: std::sync::Mutex::new(HashMap::new()),
                pending_verb_merges: std::sync::Mutex::new(HashMap::new()),
                run_fork_dispatches: std::sync::Mutex::new(HashMap::new()),
                playbook_cursors: std::sync::Mutex::new(HashMap::new()),
                agent_playbook_cursor_conn_ids: std::sync::Mutex::new(HashMap::new()),
                next_conn_id: AtomicU64::new(1),
                conn_views: std::sync::Mutex::new(HashMap::new()),
                terminal_backgrounds: std::sync::Mutex::new(HashMap::new()),
                terminal_background_seq: AtomicU64::new(1),
                availability_cache: std::sync::Mutex::new(
                    crate::availability::AvailabilityCache::default(),
                ),
                usage_cache: std::sync::Mutex::new(crate::usage::UsageCache::default()),
                cost_history: std::sync::Mutex::new(crate::cost_history::CostHistory::from_scan(
                    cost_samples,
                    history_now_ms,
                )),
                self_ref: std::sync::OnceLock::new(),
                ambient_degraded: AtomicBool::new(false),
            },
            remote_rx,
            restart_rx,
        ))
    }

    /// Bind the weak self-handle used by `&self` methods that need to spawn
    /// tasks holding the full manager (see the `self_ref` field). Call once,
    /// right after wrapping the manager in its `Arc`.
    pub fn bind_self_ref(self: &Arc<Self>) {
        let _ = self.self_ref.set(Arc::downgrade(self));
    }

    /// The configuration in force, cloned out of the lock before any await.
    ///
    /// Returning an owned `Arc` rather than a guard is deliberate: a
    /// `std::sync::RwLockReadGuard` is `!Send`, so a caller that held one
    /// across an await would make its whole future `!Send` and fail to
    /// compile where the IPC handler spawns it. Callers that read config
    /// around an await must bind this once, up front.
    pub(crate) fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Adopt a freshly loaded configuration (spec 0190). The reload swaps
    /// this last, after storage, plugins, and the router, so anything reading
    /// the new config sees subsystems that are at least as new.
    pub(crate) fn set_config(&self, config: Arc<Config>) {
        let mut slot = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = config;
    }

    /// Install the plugin runtime (spec 0152 phase 2). Called from daemon
    /// startup right after construction, and again by a config reload — a
    /// plugin installed or disabled since boot takes effect without a
    /// restart. Always pass a runtime built by `PluginRuntime::new`, never a
    /// mutated one: `has_hooks` is precomputed there, and a stale copy would
    /// silently stop dispatching a new plugin's event hooks.
    pub fn set_plugin_runtime(&self, runtime: Arc<crate::plugins::PluginRuntime>) {
        let mut slot = self
            .plugins
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(runtime);
    }

    pub(crate) fn plugin_runtime(&self) -> Option<Arc<crate::plugins::PluginRuntime>> {
        self.plugins
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Install the operator supervisor handle. Called once from daemon startup.
    pub fn set_operator_supervisor(&self, handle: crate::operator_supervisor::OperatorHandle) {
        let _ = self.operators.set(handle);
    }

    /// Apply the definitions currently on disk to the running operators.
    ///
    /// Errors when no supervisor is installed, which is the case in tests —
    /// callers should treat that as "nothing to reload", not as a failure of
    /// the edit they just persisted.
    pub async fn reload_operators(
        &self,
        reason: crate::operator_supervisor::ReloadReason,
    ) -> anyhow::Result<crate::operator_supervisor::ReloadReport> {
        let Some(handle) = self.operators.get() else {
            anyhow::bail!("operator supervisor is not running");
        };
        handle.reload(reason).await
    }

    pub(crate) fn operator_supervisor(
        &self,
    ) -> Option<&crate::operator_supervisor::OperatorHandle> {
        self.operators.get()
    }

    pub(crate) fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }

    /// Install the config supervisor handle. Called once from daemon startup.
    pub fn set_config_supervisor(&self, handle: crate::config_supervisor::ConfigHandle) {
        let _ = self.config_reloads.set(handle);
    }

    /// Re-read `config.toml` and apply it (spec 0190).
    ///
    /// Errors when no supervisor is installed, which is the case in tests —
    /// callers should treat that as "nothing to reload", not as a failure.
    pub async fn reload_config(
        &self,
        reason: crate::config_supervisor::ReloadReason,
    ) -> Result<construct_protocol::ConfigApplyResult> {
        let Some(handle) = self.config_reloads.get() else {
            anyhow::bail!("config supervisor is not running");
        };
        handle.reload(reason).await
    }

    /// Configuration read but waiting on a restart to take effect.
    pub(crate) fn config_restart_required(&self) -> Vec<String> {
        self.config_restart_required
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn set_config_restart_required(&self, fields: Vec<String>) {
        let mut slot = self
            .config_restart_required
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = fields;
    }

    /// The payload a subscribing client is replayed, so a pending restart
    /// survives a reconnect. `applied` is empty here on purpose: a replay is
    /// not a reload, and reporting it as one would show a stale transient
    /// status to a client that just attached.
    pub(crate) fn config_state_payload(&self) -> construct_protocol::ConfigStateNotificationPayload {
        construct_protocol::ConfigStateNotificationPayload {
            restart_required: self.config_restart_required(),
            applied: Vec::new(),
        }
    }

    pub(crate) fn broadcast_config_state(&self, result: &construct_protocol::ConfigApplyResult) {
        let _ = self.broadcast.send(BroadcastMsg::ConfigState(
            construct_protocol::ConfigStateNotificationPayload {
                restart_required: result.restart_required.clone(),
                applied: result.applied.clone(),
            },
        ));
    }

    /// Republish ambient feature status. A config reload can move
    /// `suggest.enabled` or the minibuffer harness, and clients would
    /// otherwise keep rendering the answer from before the edit.
    pub(crate) async fn broadcast_features_state(&self) {
        let status = self.features_status().await;
        let _ = self.broadcast.send(BroadcastMsg::FeaturesState(status));
    }

    pub fn set_channel_publications(
        &self,
        handle: crate::channel_publication::PublicationHandle,
    ) {
        let _ = self.channel_publications.set(handle);
    }

    pub(crate) fn channel_publications(
        &self,
    ) -> Option<&crate::channel_publication::PublicationHandle> {
        self.channel_publications.get()
    }

    /// Every plugin-contributed action, for `plugin.list_actions`.
    pub fn plugin_actions(&self) -> Vec<construct_protocol::PluginActionInfo> {
        self.plugin_runtime()
            .map(|rt| rt.actions())
            .unwrap_or_default()
    }

    /// Run one plugin action (`plugin.run_action`), fire-and-forget.
    pub fn plugin_run_action(
        &self,
        params: &construct_protocol::PluginRunActionParams,
    ) -> Result<()> {
        let runtime = self
            .plugin_runtime()
            .context("no plugins are loaded")?;
        runtime.run_action(
            &params.plugin_id,
            &params.action_id,
            params.session_id.as_deref(),
        )
    }

    /// Allocate a monotonic id for a new client connection. The connection
    /// uses it for `set_conn_view` / `clear_conn`.
    pub fn alloc_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record which session + surface a connection is currently viewing.
    /// A connection views one session at a time, so this overwrites any prior
    /// entry for `conn_id`.
    pub fn set_conn_view(&self, conn_id: u64, session_id: String, view: ClientView) {
        if let Ok(mut m) = self.conn_views.lock() {
            m.insert(conn_id, (session_id, view));
        }
    }

    /// Drop a connection's view registration when it disconnects.
    /// Record a connection's painted-terminal-background report (spec 0073).
    pub fn set_terminal_background(&self, conn_id: u64, background: Option<[u8; 3]>) {
        let seq = self.terminal_background_seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut m) = self.terminal_backgrounds.lock() {
            m.insert(conn_id, (seq, background));
        }
    }

    /// The background color child OSC 11 probes should be answered with: the
    /// most recent report among live connections. `None` when no client
    /// paints (or the most recent reporter doesn't) — probes then pass
    /// through unanswered, as before spec 0073.
    pub fn effective_terminal_background(&self) -> Option<[u8; 3]> {
        self.terminal_backgrounds
            .lock()
            .ok()?
            .values()
            .max_by_key(|(seq, _)| *seq)
            .and_then(|(_, bg)| *bg)
    }

    pub fn clear_conn(&self, conn_id: u64) {
        if let Ok(mut m) = self.conn_views.lock() {
            m.remove(&conn_id);
        }
        if let Ok(mut m) = self.terminal_backgrounds.lock() {
            m.remove(&conn_id);
        }
        if let Ok(mut cursors) = self.playbook_cursors.lock() {
            if let Some(mut cursor) = cursors.remove(&conn_id) {
                cursor.active = false;
                cursor.updated_at_ms = chrono::Utc::now().timestamp_millis();
                let _ = self.broadcast.send(BroadcastMsg::PlaybookCursor {
                    payload: construct_protocol::PlaybookCursorNotificationPayload { cursor },
                    skip_conn_id: None,
                });
            }
        }
    }

    pub fn playbook_collaborators(
        &self,
        session_id: &str,
    ) -> Vec<construct_protocol::PlaybookCursor> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.playbook_cursors
            .lock()
            .map(|cursors| {
                cursors
                    .values()
                    .filter(|cursor| playbook_cursor_is_visible(cursor, session_id, now_ms))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn playbook_cursor(
        &self,
        conn_id: u64,
        kind: &str,
        params: construct_protocol::PlaybookCursorParams,
    ) -> Result<construct_protocol::PlaybookCursorResult> {
        self.get_entry(&params.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", params.session_id))?;
        let requested_label = params
            .label
            .unwrap_or_else(|| playbook_default_cursor_label(kind));
        let mut cursor = construct_protocol::PlaybookCursor {
            session_id: params.session_id,
            client_id: format!("c{conn_id}"),
            label: requested_label.clone(),
            kind: kind.to_string(),
            cursor: params.cursor,
            selection_anchor: params.selection_anchor,
            selection_head: params.selection_head,
            version: params.version,
            color_index: (conn_id % 8) as u8,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: !params.clear,
        };
        if let Ok(mut cursors) = self.playbook_cursors.lock() {
            if cursor.active {
                cursor.label = cursors
                    .get(&conn_id)
                    .map(|existing| existing.label.clone())
                    .unwrap_or_else(|| {
                        playbook_unique_cursor_label(&cursors, conn_id, &requested_label, kind)
                    });
                cursors.insert(conn_id, cursor.clone());
            } else if let Some(existing) = cursors.remove(&conn_id) {
                cursor = existing;
                cursor.active = false;
                cursor.updated_at_ms = chrono::Utc::now().timestamp_millis();
            }
        }
        // Skip delivering this broadcast back to its own publisher: it's a
        // plain-publish echo of state `conn_id` already applied locally
        // before calling in, and letting it round-trip back opens a window
        // where a later local move races the echo and gets clobbered by it
        // (the receiver can't tell "stale echo" from "real daemon rebase").
        // Every other connection still needs it to render this cursor.
        let _ = self.broadcast.send(BroadcastMsg::PlaybookCursor {
            payload: construct_protocol::PlaybookCursorNotificationPayload {
                cursor: cursor.clone(),
            },
            skip_conn_id: Some(conn_id),
        });
        Ok(construct_protocol::PlaybookCursorResult { cursor })
    }

    /// Whether any live connection is currently watching `session_id` in the
    /// chat view. The `AskUserQuestion` chat-gate degrades the picker to text
    /// when this is true.
    pub fn chat_viewer_active(&self, session_id: &str) -> bool {
        self.conn_views
            .lock()
            .map(|m| {
                m.values()
                    .any(|(s, v)| s == session_id && *v == ClientView::Chat)
            })
            .unwrap_or(false)
    }

    fn install_memory_env(&self, env: &mut HashMap<String, String>, project_id: Option<&str>) {
        env.remove(ENV_GLOBAL_MEMORY_FILE);
        env.remove(ENV_PROJECT_MEMORY_FILE);
        env.remove(ENV_PROJECT_ID);

        match self.storage.ensure_global_memory() {
            Ok(path) => {
                env.insert(
                    ENV_GLOBAL_MEMORY_FILE.to_string(),
                    path.to_string_lossy().to_string(),
                );
            }
            Err(e) => tracing::warn!(error = ?e, "global memory file setup failed"),
        }

        let Some(project_id) = project_id else {
            return;
        };
        match self.storage.ensure_project_memory(project_id) {
            Ok(path) => {
                env.insert(ENV_PROJECT_ID.to_string(), project_id.to_string());
                env.insert(
                    ENV_PROJECT_MEMORY_FILE.to_string(),
                    path.to_string_lossy().to_string(),
                );
            }
            Err(e) => {
                tracing::warn!(project_id, error = ?e, "project memory file setup failed");
            }
        }
    }

    /// The dev-mode web-UI asset directory, if one is active. `None`
    /// means serve the embedded assets.
    pub fn dev_assets(&self) -> Option<PathBuf> {
        self.dev_assets.lock().unwrap().clone()
    }

    /// Point the remote web server at `dir` (or revert to embedded with
    /// `None`). No-op in release builds — the override is dev-only.
    pub fn set_dev_assets(&self, dir: Option<PathBuf>) {
        if cfg!(debug_assertions) {
            *self.dev_assets.lock().unwrap() = dir;
        }
    }

    /// Path where the supervisor reads / writes the remote
    /// `RemoteSnapshot`. Exposed so the supervisor can hand it to
    /// `RemoteState::with_snapshot_path`.
    pub(crate) fn remote_snapshot_path(&self) -> PathBuf {
        self.remote_snapshot_path.clone()
    }

    /// Request a daemon lifecycle action (restart / restart-with-sessions
    /// / stop). Resolves the exe path + args, sends a `RestartCommand` to
    /// main's lifecycle channel, and returns the command so the IPC
    /// handler can echo it back to the caller before the runtime tears
    /// down. Returns `Err` if the exe path can't be resolved or the
    /// receiver was dropped (which shouldn't happen — main holds it for
    /// the daemon's lifetime).
    ///
    /// The exe is resolved even for [`RestartAction::Stop`] (where it
    /// goes unused) so the command shape stays uniform; resolving it is
    /// cheap and validating a caller-supplied path early can't hurt.
    pub fn request_daemon_restart(
        &self,
        exe_override: Option<PathBuf>,
        action: RestartAction,
    ) -> Result<RestartCommand> {
        let exe = match exe_override {
            // Validate a caller-supplied binary BEFORE tearing the
            // daemon down — exec()ing a bad path would never come back.
            Some(p) => validate_restart_exe(&p)?,
            None => restart_exe_path().context("resolve restart exe")?,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cmd = RestartCommand { exe, args, action };
        self.restart_tx
            .send(cmd.clone())
            .map_err(|_| anyhow::anyhow!("restart channel closed"))?;
        Ok(cmd)
    }

    /// Access to the remote-handle slot. Used by the supervisor
    /// task to install the handle after a successful bind, and by
    /// `start_remote`'s fast path to snapshot the existing state.
    pub(crate) fn remote_slot(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, Option<RemoteHandle>>> {
        self.remote.lock()
    }

    fn adapter_socket_path(&self, id: &str) -> PathBuf {
        self.adapter_runtime_dir.join(format!("{id}.sock"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastMsg> {
        self.broadcast.subscribe()
    }

    /// The shared split layout and its version. Every client calls this on
    /// connect; narrow clients use it only to pick a starting session and
    /// must never write back.
    pub fn layout(&self) -> LayoutDocument {
        self.layout.lock().expect("layout mutex poisoned").clone()
    }

    /// Replace the shared layout wholesale. A split tree has no useful merge
    /// semantics, so `base_version` is the entire concurrency story: a writer
    /// that composed its edit against an older version is rejected and must
    /// re-read. `None` forces the write through, for a client that genuinely
    /// has no prior state.
    pub fn set_layout(
        &self,
        mut tree: construct_protocol::LayoutNode,
        base_version: Option<u64>,
    ) -> Result<LayoutDocument> {
        tree.normalize();
        let doc = {
            let mut guard = self.layout.lock().expect("layout mutex poisoned");
            if let Some(base) = base_version {
                if base != guard.version {
                    anyhow::bail!(
                        "layout conflict: current version is {}, attempted base version is {}",
                        guard.version,
                        base
                    );
                }
            }
            guard.tree = tree;
            guard.version = guard.version.saturating_add(1);
            guard.clone()
        };
        if let Err(e) = self.storage.save_layout(&doc) {
            tracing::warn!(error = ?e, "save layout failed");
        }
        let _ = self
            .broadcast
            .send(BroadcastMsg::LayoutState(LayoutStateNotificationPayload {
                layout: doc.clone(),
            }));
        Ok(doc)
    }

    /// Empty every pane showing `session_id`, if any. Called when a session
    /// is deleted or archived so panes don't point at something that no
    /// longer exists. Silent no-op when the session wasn't on screen, so it
    /// is safe to call on every removal path.
    fn clear_layout_session(&self, session_id: &str) {
        let doc = {
            let mut guard = self.layout.lock().expect("layout mutex poisoned");
            if !guard.tree.clear_session(session_id) {
                return;
            }
            guard.version = guard.version.saturating_add(1);
            guard.clone()
        };
        if let Err(e) = self.storage.save_layout(&doc) {
            tracing::warn!(error = ?e, "save layout after session removal failed");
        }
        let _ = self
            .broadcast
            .send(BroadcastMsg::LayoutState(LayoutStateNotificationPayload {
                layout: doc,
            }));
    }

    /// Snapshot whether the remote listener is running and its current client
    /// count. Used both for broadcasts and for a newly subscribed TUI so it
    /// does not have to wait for the next state transition.
    pub fn remote_state_payload(&self) -> construct_protocol::RemoteStateNotificationPayload {
        let guard = self.remote_slot().expect("remote mutex poisoned");
        match guard.as_ref() {
            Some(handle) => construct_protocol::RemoteStateNotificationPayload {
                enabled: true,
                clients: handle.state.client_count(),
            },
            None => construct_protocol::RemoteStateNotificationPayload {
                enabled: false,
                clients: 0,
            },
        }
    }

    /// Broadcast the current remote-control state. Best-effort — silently
    /// skipped if no subscribers.
    pub fn broadcast_remote_state(&self) {
        let payload = self.remote_state_payload();
        let _ = self.broadcast.send(BroadcastMsg::RemoteState(payload));
    }

    pub(crate) fn broadcast_channel_publication(
        &self,
        payload: construct_protocol::ChannelPublicationNotificationPayload,
    ) {
        let _ = self
            .broadcast
            .send(BroadcastMsg::ChannelPublicationState(payload));
    }

    /// Start (or look up) the remote WS listener and return a URL + QR
    /// ready for the user. Idempotent — calling more than once returns
    /// the existing password + URL so the QR stays stable for the
    /// listener's lifetime.
    ///
    /// With `params.provider` unset, this binds the listener and stops
    /// there: reachable from this machine and the local network,
    /// published nowhere. Naming a provider additionally starts that
    /// tunnel and waits for its URL.
    ///
    /// `port_hint` is honored when set (env-var-at-boot path);
    /// otherwise an ephemeral port is bound. Tunnel spawning is
    /// skipped entirely when `CONSTRUCT_REMOTE_NO_TUNNEL` is set, same
    /// as the boot path.
    pub async fn start_remote(
        self: Arc<Self>,
        port_hint: Option<u16>,
        params: construct_protocol::RemoteStartParams,
    ) -> anyhow::Result<construct_protocol::RemoteStartResult> {
        use anyhow::Context as _;

        // Ask the supervisor to ensure the listener is up (and, if a
        // provider was named, to start its tunnel). The static call
        // edge from here goes through an mpsc channel, NOT a direct
        // call to `serve_ws_on` — that keeps the dispatch loop's Send
        // inference from going into a cycle. Idempotent: repeat
        // requests are no-ops on the bind side, and the tunnel is
        // spawned at most once per listener lifetime.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_starter
            .send(crate::remote_supervisor::SupervisorMsg::Start(
                crate::remote_supervisor::StartRequest {
                    port_hint,
                    provider: params.provider,
                    password: params.password.clone(),
                    subdomain: params.subdomain.clone(),
                    respond: tx,
                },
            ))
            .map_err(|_| anyhow::anyhow!("remote supervisor task is not running"))?;
        let outcome = rx
            .await
            .context("remote supervisor dropped reply channel")??;

        self.build_remote_result(
            outcome.state,
            outcome.port,
            params.provider,
            params.wait_for_tunnel,
        )
        .await
    }

    /// Probe every tunnel provider. Read-only — spawns nothing — so
    /// the dialog can call it on every open to decide which buttons it
    /// can offer and what to say about the ones it can't.
    pub async fn remote_providers(&self) -> construct_protocol::RemoteProvidersResult {
        construct_protocol::RemoteProvidersResult {
            providers: crate::tunnel::probe_all().await,
        }
    }

    /// Tear down the remote transport via the supervisor. With
    /// `tunnel_only`, only the tunnel goes; the LAN listener + password
    /// stay up. Otherwise the whole thing comes down and the next
    /// `start_remote` mints fresh credentials. Idempotent — calling when
    /// nothing is running is not an error; `was_running` reports whether
    /// anything was actually torn down.
    pub async fn stop_remote(
        self: Arc<Self>,
        tunnel_only: bool,
    ) -> anyhow::Result<construct_protocol::RemoteStopResult> {
        use anyhow::Context as _;
        let provider = self
            .remote_slot()
            .ok()
            .and_then(|slot| slot.as_ref().map(|handle| handle.state.tunnel_provider()))
            .unwrap_or_default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_starter
            .send(crate::remote_supervisor::SupervisorMsg::Stop(
                crate::remote_supervisor::StopRequest {
                    tunnel_only,
                    respond: tx,
                },
            ))
            .map_err(|_| anyhow::anyhow!("remote supervisor task is not running"))?;
        let outcome = rx
            .await
            .context("remote supervisor dropped reply channel")??;
        Ok(construct_protocol::RemoteStopResult {
            was_running: outcome.was_running,
            provider,
        })
    }

    /// Render the final `RemoteStartResult`.
    ///
    /// Without a provider, this reports what is reachable right now
    /// with no tunnel: the LAN address when the machine has one,
    /// the no-auth local web UI otherwise, returned immediately. With a
    /// provider, it polls for that provider's URL and either returns it
    /// (`tunnel_ready = true`) or fails with a diagnostic — it never
    /// silently degrades to the local URL, because a user who asked to
    /// be reachable from outside must not be shown a green light and a
    /// QR code that only works from their sofa.
    async fn build_remote_result(
        &self,
        state: crate::remote::RemoteState,
        port: u16,
        provider: construct_protocol::TunnelProvider,
        wait_for_tunnel: bool,
    ) -> anyhow::Result<construct_protocol::RemoteStartResult> {
        use construct_protocol::TunnelProvider;
        use std::time::Duration;

        // Every URL here is the browser-facing `http(s)://` form, not
        // `ws://`. The page served at it boots a small JS app that does
        // the `http(s)` → `ws(s)` swap itself. A `ws://` URL cannot be
        // opened in a browser or scanned from a QR at all, which is
        // what bit us in the first phone test.
        // Local access uses the daemon's separate loopback-only web UI,
        // which deliberately needs no login. The wildcard-bound remote
        // listener remains fully Basic-auth-gated for LAN and tunnel use.
        let local_url = construct_protocol::paths::local_webui_url();
        let lan_url = crate::remote::lan_ipv4().map(|ip| format!("http://{ip}:{port}/"));
        let password = state.password().to_string();

        // No provider, or a first non-waiting leg on the way to one:
        // answer immediately with the best local address we have. The
        // interactive dialog uses the non-waiting leg to paint itself
        // instantly and then upgrades the QR in the background.
        if provider == TunnelProvider::None || !wait_for_tunnel {
            let url = lan_url.clone().unwrap_or_else(|| local_url.clone());
            let qr = crate::remote::render_qr_dense1x2(&url).unwrap_or_default();
            let hint = if provider != TunnelProvider::None {
                Some(format!(
                    "Starting {} tunnel… the QR updates when it publishes a URL.",
                    provider.label()
                ))
            } else if lan_url.is_none() {
                // Worth saying out loud rather than silently showing a
                // loopback QR the user's phone will never reach.
                Some(
                    "No local network address found — this machine can only be reached \
                     from itself. Pick a provider below to expose it."
                        .to_string(),
                )
            } else {
                None
            };
            // Report any tunnel already live from an earlier start, so a
            // freshly-opened dialog can badge that provider rather than
            // implying nothing is exposed. `provider` here is what this
            // reply's `url` is (the LAN address); `active_provider` is
            // the separate "what's actually running" signal.
            let active_provider = if state.tunnel_url().await.is_some() {
                state.tunnel_provider()
            } else {
                TunnelProvider::None
            };
            return Ok(construct_protocol::RemoteStartResult {
                url,
                qr,
                tunnel_ready: false,
                password,
                hint,
                provider: TunnelProvider::None,
                local_url,
                lan_url,
                active_provider,
                auth_url: state.auth_url().await,
            });
        }

        // Provider mode: poll the shared tunnel-url slot. Browser OAuth is
        // intentionally allowed much longer than a subprocess-only provider:
        // the user may need to switch windows or complete MFA.
        // We poll instead of wiring a notifier because the call shape is
        // request/reply over IPC — the caller is already blocked on this
        // future.
        let wait_seconds = if provider == TunnelProvider::Construct {
            10 * 60
        } else {
            15
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
        loop {
            if let Some(u) = state.tunnel_url().await {
                let qr = crate::remote::render_qr_dense1x2(&u).unwrap_or_default();
                return Ok(construct_protocol::RemoteStartResult {
                    url: u,
                    qr,
                    tunnel_ready: true,
                    password,
                    hint: None,
                    provider,
                    local_url,
                    lan_url,
                    active_provider: provider,
                    auth_url: None,
                });
            }
            if let Some(error) = state.tunnel_error().await {
                anyhow::bail!("{} tunnel failed: {error}", provider.label());
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Timed out. Ask the provider itself why — its preflight
        // already knows how to say "not installed", "not logged in",
        // "no HTTPS certs" in words the user can act on, and the CLI
        // paints whatever we return here verbatim.
        if std::env::var("CONSTRUCT_REMOTE_NO_TUNNEL").is_ok() {
            anyhow::bail!(
                "CONSTRUCT_REMOTE_NO_TUNNEL is set, so no tunnel was started. \
                 Unset it and try again."
            );
        }
        let label = provider.label();
        match crate::tunnel::preflight(provider).await {
            Err(detail) => anyhow::bail!("{detail}"),
            Ok(()) => anyhow::bail!(
                "{label} started but published no URL within {wait_seconds}s. Check the daemon log \
                 (RUST_LOG=info,construct=debug) for its output."
            ),
        }
    }

    pub async fn harnesses(&self) -> Vec<HarnessInfo> {
        // Bound once, up front: `cfg` is borrowed across the availability
        // probe below, so reading through `self.config()` inline would hold a
        // `!Send` guard across an await.
        let config = self.config();
        let mut out = Vec::with_capacity(config.adapters.len());
        for (name, cfg) in config.adapters.iter() {
            let binary_spec = cfg.binary.clone().unwrap_or_else(|| name.clone());
            let resolved = locate_binary(&binary_spec);
            let availability = self
                .probe_harness_availability(name, &binary_spec, resolved.as_deref())
                .await;
            out.push(HarnessInfo {
                name: name.clone(),
                available: availability.available,
                detail: Some(availability.detail),
                binary: resolved.as_ref().map(|p| p.to_string_lossy().to_string()),
                description: cfg.description.clone(),
                capabilities: builtin_harness_capabilities(name),
            });
        }
        out
    }

    /// Per-method smith auth detection for the `/configure` dialog's
    /// smith-auth tab (spec 0069): every method smith supports, each with
    /// live-detected status, plus which one (if any) is currently pinned.
    pub async fn smith_auth_status(&self) -> SmithAuthStatusResult {
        let methods = crate::availability::smith_auth_methods(&self.availability_cache).await;
        let config = self.config();
        let pinned = config
            .adapters
            .get("smith")
            .and_then(|a| a.env.get("CONSTRUCT_SMITH_MODEL"))
            .map(String::as_str);
        let current = crate::availability::current_smith_auth_method(pinned, &methods);
        SmithAuthStatusResult {
            methods: methods
                .into_iter()
                .map(|m| SmithAuthMethodInfo {
                    id: m.id.to_string(),
                    label: m.label.to_string(),
                    available: m.available,
                    detail: m.detail,
                })
                .collect(),
            current,
        }
    }

    /// Pin (or clear) smith's default model by writing
    /// `[adapters.smith.env] CONSTRUCT_SMITH_MODEL` in `config.toml` (spec
    /// 0069). Does not affect already-running adapters — sessions started
    /// after this pick up the new pin, which `SmithSetAuthMethodResult::note`
    /// tells the caller.
    ///
    /// Applies the write itself rather than leaving it to the watcher (spec
    /// 0190): this is the one caller that edits `config.toml` from inside the
    /// daemon, and waiting up to a watch interval to adopt our own write
    /// would make the dialog appear not to have taken. The watcher's later
    /// tick sees the changed fingerprint and reloads an already-current file,
    /// which is a no-op.
    pub async fn set_smith_auth_method(&self, method: &str) -> Result<SmithSetAuthMethodResult> {
        let methods = crate::availability::smith_auth_methods(&self.availability_cache).await;
        let model_spec = if method == "auto" {
            None
        } else {
            let m = methods
                .iter()
                .find(|m| m.id == method)
                .ok_or_else(|| anyhow!("unknown smith auth method `{method}`"))?;
            Some(format!("{}:{}", m.model_prefix, m.default_model))
        };
        let paths = construct_protocol::paths::Paths::discover();
        crate::config::set_smith_model_pin(&paths, model_spec.as_deref())?;
        // Best-effort: without a supervisor (tests) the write still stands
        // and the next daemon start reads it.
        let _ = self
            .reload_config(crate::config_supervisor::ReloadReason::Ipc)
            .await;
        Ok(SmithSetAuthMethodResult {
            model_spec,
            note: "new sessions pick up this default; already-running sessions keep their \
                   current model"
                .to_string(),
        })
    }

    /// Live status of the daemon's ambient features (spec 0151): the
    /// smith-credential-dependent conveniences (auto-naming, suggestions,
    /// minibuffer), each mapped to ok/degraded/off with a human-readable
    /// reason. This is the surface that connects "my sessions never get
    /// named" back to "smith has no credential" — the probes themselves
    /// already existed, but nothing tied the degraded features to them.
    pub async fn features_status(&self) -> construct_protocol::FeaturesStatusResult {
        let smith = crate::availability::probe_smith(&self.availability_cache).await;
        // Bound once, up front: `name` borrows from it and stays live across
        // the availability probe below, which cannot hold a `!Send` guard.
        let config = self.config();
        let minibuffer = match config.minibuffer.effective_harness() {
            None => None,
            Some(name) => {
                let avail = if name == "smith" {
                    smith.clone()
                } else {
                    let binary_spec = config
                        .adapters
                        .get(name)
                        .and_then(|c| c.binary.clone())
                        .unwrap_or_else(|| name.to_string());
                    let resolved = locate_binary(&binary_spec);
                    self.probe_harness_availability(name, &binary_spec, resolved.as_deref())
                        .await
                };
                Some((name.to_string(), avail))
            }
        };
        construct_protocol::FeaturesStatusResult {
            features: crate::availability::ambient_features(&crate::availability::FeatureInputs {
                smith,
                title_gen: crate::availability::smith_title_gen_available(),
                suggest_enabled: config.suggest.enabled,
                minibuffer,
            }),
            degradation_observed: self.ambient_degraded.load(Ordering::SeqCst),
        }
    }

    /// Latch + announce that an ambient feature just skipped work because
    /// smith has no credential (spec 0151). First call per daemon run
    /// broadcasts a `features/state` notification so clients can show a
    /// visible degradation notice; later calls are no-ops — one hint, not
    /// a nag stream.
    pub(crate) async fn note_ambient_degradation(&self, feature: &str) {
        if self.ambient_degraded.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(
            %feature,
            "ambient feature skipped: no smith credential — see `construct harnesses` / the \
             configure dialog"
        );
        let status = self.features_status().await;
        let _ = self.broadcast.send(BroadcastMsg::FeaturesState(status));
    }

    /// Probe real availability for one configured harness (spec 0068).
    ///
    /// The ladder itself lives in `availability` so `construct doctor` runs
    /// the identical probe with no daemon in the picture (spec 0168).
    async fn probe_harness_availability(
        &self,
        name: &str,
        binary_spec: &str,
        resolved_binary: Option<&std::path::Path>,
    ) -> crate::availability::Availability {
        crate::availability::probe_harness(
            &self.availability_cache,
            name,
            binary_spec,
            resolved_binary,
        )
        .await
    }

    pub async fn list(&self) -> Vec<SessionSummary> {
        let guard = self.sessions.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for entry in guard.values() {
            let summary = entry.summary().await;
            out.push(summary);
        }
        // Primary: user-controlled position ASC. Tiebreaker: newer first.
        out.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        out
    }

    pub async fn get_entry(&self, id: &str) -> Option<Arc<SessionEntry>> {
        self.sessions.read().await.get(id).cloned()
    }

    pub async fn detail(&self, id: &str) -> Result<SessionDetail> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let summary = entry.summary().await;
        let transcript = self.storage.read_transcript(id, 0, None)?;
        let ui_panels = self.storage.read_widgets(id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "read widgets failed");
            Vec::new()
        });
        Ok(SessionDetail {
            summary,
            events: transcript.events,
            ui_panels,
        })
    }

    pub async fn transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
        before: Option<u64>,
        tail: Option<usize>,
    ) -> Result<TranscriptResult> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        if let Some(n) = tail {
            // Tail mode: take `total` from the live counter (cheap, no file
            // scan) and read only the last `n` events from disk.
            let total = entry.transcript_count.load(Ordering::Relaxed);
            let events = self.storage.read_transcript_tail(id, n)?;
            return Ok(TranscriptResult { events, total });
        }
        if let Some(before) = before {
            let total = entry.transcript_count.load(Ordering::Relaxed);
            let events = self
                .storage
                .read_transcript_before(id, before, limit.unwrap_or(500))?;
            return Ok(TranscriptResult { events, total });
        }
        self.storage.read_transcript(id, from, limit)
    }

    /// Substring search across session name/metadata, stored playbook
    /// contents, and transcript history (spec 0076). The scan is
    /// synchronous file I/O over up to the global byte budget, so unlike
    /// this type's small point reads (`transcript`, `diff`, …) it runs on
    /// the blocking pool — inline it would pin a runtime worker for the
    /// whole sweep and stall the connection's dispatch loop behind it.
    pub async fn search(&self, params: SearchParams) -> Result<SearchResult> {
        let sessions = self.list().await;
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.search(&sessions, &params))
            .await
            .map_err(|e| anyhow!("search task failed: {e}"))?
    }

    pub async fn diff(&self, id: &str) -> Result<String> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let summary = entry.summary().await;
        if let Some(wt) = summary.worktree.as_deref() {
            let p = PathBuf::from(wt);
            if p.exists() {
                return worktree::diff_worktree(&p).await;
            }
        }
        // No worktree → run git diff in the original cwd.
        let cwd = PathBuf::from(&summary.cwd);
        if worktree::is_git_repo(&cwd).await {
            return worktree::diff_worktree(&cwd).await;
        }
        Ok(String::new())
    }

    pub async fn playbook_get(&self, session_id: &str) -> Result<PlaybookGetResult> {
        self.get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
        let (playbook, _) = self.storage.read_playbook_with_blocks(session_id)?;
        let revisions = self.storage.read_playbook_revisions(session_id)?;
        let active_run = self.playbook_run_snapshot(session_id);
        let blocks = self.playbook_blocks_projection(session_id, &playbook.markdown);
        let collaborators = self.playbook_collaborators(session_id);
        Ok(PlaybookGetResult {
            playbook,
            revisions,
            active_run,
            blocks,
            collaborators,
        })
    }

    pub async fn playbook_update(&self, params: PlaybookUpdateParams) -> Result<PlaybookUpdateResult> {
        self.get_entry(&params.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", params.session_id))?;
        // A complete shimmer declaration must cover every block of the new
        // document, in order (spec 0053). Validate before writing so a miscount
        // fails the call rather than persisting a doc with a stale shimmer set.
        if let Some(decl) = &params.shimmer {
            let block_count = construct_protocol::playbook_block_spans(&params.markdown).len();
            if decl.len() != block_count {
                anyhow::bail!(
                    "shimmer declaration has {} entries but the playbook has {} blocks",
                    decl.len(),
                    block_count
                );
            }
            // When present, the parallel tooltip array must line up one-to-one
            // with the shimmer declaration (spec 0057).
            if let Some(tips) = &params.shimmer_tooltips {
                if tips.len() != decl.len() {
                    anyhow::bail!(
                        "shimmer_tooltips has {} entries but shimmer has {}",
                        tips.len(),
                        decl.len()
                    );
                }
            }
        }
        let shimmer = params.shimmer;
        let shimmer_tooltips = params.shimmer_tooltips;
        let session_id = params.session_id;
        let playbook = self.storage.update_playbook(
            &session_id,
            params.markdown,
            params.actor,
            params.base_version,
            params.template_id,
            params.note,
        )?;
        match shimmer {
            Some(decl) => {
                // Pair each pending block with its tooltip (spec 0057): the
                // tooltip array is parallel to the shimmer array in document
                // order, so index i carries block i's tooltip.
                let pending: std::collections::HashMap<String, Option<String>> = self
                    .playbook_blocks_projection(&session_id, &playbook.markdown)
                    .into_iter()
                    .zip(decl)
                    .enumerate()
                    .filter(|(_, (_, on))| *on)
                    .map(|(i, (block, _))| {
                        let tip = shimmer_tooltips
                            .as_ref()
                            .and_then(|tips| tips.get(i))
                            .and_then(|t| t.clone());
                        (block.id, tip)
                    })
                    .collect();
                self.set_playbook_run_pending(&session_id, &playbook.markdown, pending);
            }
            None => {
                // Co-editing human-save path: narrow by content change only.
                self.narrow_playbook_run(&session_id, &playbook.markdown, &[]);
            }
        }
        let blocks = self.playbook_blocks_projection(&session_id, &playbook.markdown);
        let active_run = self.playbook_run_snapshot(&session_id);
        self.broadcast_playbook_state(playbook.clone());
        Ok(PlaybookUpdateResult {
            playbook,
            blocks,
            active_run,
        })
    }

    #[allow(dead_code)]
    pub async fn playbook_edit(&self, params: PlaybookEditParams) -> Result<PlaybookUpdateResult> {
        self.playbook_edit_from_conn(params, None).await
    }

    pub async fn playbook_edit_from_conn(
        &self,
        params: PlaybookEditParams,
        source_conn_id: Option<u64>,
    ) -> Result<PlaybookUpdateResult> {
        let entry = self
            .get_entry(&params.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", params.session_id))?;
        // Block ids of the pre-edit document, to tell which keep_pending blocks
        // are genuinely new (so a re-stated, unchanged heading is not re-lit).
        let (before_playbook, before_blocks) = self
            .storage
            .read_playbook_with_blocks(&params.session_id)
            .map(|(playbook, blocks)| (Some(playbook), blocks))
            .unwrap_or_default();
        let before_refs: std::collections::HashSet<String> =
            before_blocks.iter().map(|block| block.id.clone()).collect();
        let playbook = self.storage.edit_playbook(
            &params.session_id,
            &params.edits,
            params.actor,
            params.note,
        )?;
        // A source-less edit is agent-authored (server.rs only omits
        // `source_conn_id` when `actor == PlaybookUpdateActor::Agent`).
        // Reserve/find that agent's own pseudo-cursor slot *before* rebasing
        // so it can be excluded from the rebase-and-broadcast pass below: its
        // position is about to be superseded by the fresh one this edit just
        // produced, so rebasing it first would broadcast a stale span for
        // one message before the correct publish immediately follows it.
        let agent_conn_id = source_conn_id
            .is_none()
            .then(|| self.agent_playbook_cursor_conn_id(&params.session_id));
        let cursor_updates = before_playbook
            .as_ref()
            .map(|before| {
                self.rebase_playbook_cursors_after_edit(
                    &params.session_id,
                    &before.markdown,
                    &params.edits,
                    playbook.version,
                    source_conn_id,
                    agent_conn_id,
                )
            })
            .unwrap_or_default();
        // The span of the agent-presence cursor is the true difference
        // between the pre- and post-edit documents, not any individual
        // edit's own replacement — a multi-edit batch can contain edits that
        // partially or fully cancel each other out (e.g. a corrective
        // second edit reverting part of the first), and only a whole-
        // document diff reliably reports where the document actually ended
        // up different. `None` here also means "nothing changed" (a
        // genuine no-op batch), which doubles as the publish gate below.
        let last_edit_span = before_playbook
            .as_ref()
            .and_then(|before| playbook_edit_overall_span(&before.markdown, &playbook.markdown));
        // Apply the partial shimmer declaration against the post-edit document
        // (spec 0053): changed blocks drop their prior shimmer; declared ids set
        // pending/settled; ids that no longer exist are ignored (fail closed).
        // Edits flagged `keep_pending` atomically re-add every block they
        // introduce — including the moved item when the new_string also restates
        // its heading — so moving/annotating a still-pending block keeps it
        // shimmering without the pending set transiently emptying. Blocks that
        // already existed pre-edit are skipped (no re-light of an unchanged
        // heading), and explicit declarations in `shimmer` win.
        let keep_content_ids = playbook_edit_keep_ids(&params.edits);
        let post_blocks = self.playbook_blocks_projection(&params.session_id, &playbook.markdown);
        let mut decls = params.shimmer.clone();
        for block in post_blocks.iter().filter(|block| {
            keep_content_ids.contains(&block.content_id) && !before_refs.contains(&block.id)
        }) {
            if decls.iter().any(|d| d.id == block.id) {
                continue;
            }
            // keep_pending re-adds the produced block's new id with no
            // agent tooltip (spec 0057); it renders the fallback until the
            // agent declares the new id with a tooltip.
            decls.push(construct_protocol::PlaybookShimmerDecl {
                id: block.id.clone(),
                shimmer: true,
                tooltip: None,
            });
        }
        // A human editing a block the agent is still working on must not
        // settle it (#1091). Typing advances the block's content epoch, so its
        // ref stops matching and the stale-declaration rule would drop the
        // shimmer — correct for an agent's stale declaration, wrong for the
        // very ordinary act of annotating a task while it runs. Nothing would
        // ever re-light it: the agent already declared that block and has no
        // reason to declare it again.
        //
        // Block ids survive semantic edits, so carry the pending state (and
        // the agent's tooltip) onto the produced block's new ref. Agents stay
        // explicit — this is human-authored edits only, and an explicit
        // declaration in `shimmer` still wins.
        if params.actor == construct_protocol::PlaybookUpdateActor::Human {
            let pending_by_block_id = self.playbook_run_pending_by_block_id(&params.session_id);
            for block in post_blocks
                .iter()
                .filter(|block| !before_refs.contains(&block.id))
            {
                let Some(tooltip) = pending_by_block_id.get(&block.block_id) else {
                    continue;
                };
                if decls.iter().any(|d| d.id == block.id) {
                    continue;
                }
                decls.push(construct_protocol::PlaybookShimmerDecl {
                    id: block.id.clone(),
                    shimmer: true,
                    tooltip: tooltip.clone(),
                });
            }
        }
        self.narrow_playbook_run(&params.session_id, &playbook.markdown, &decls);
        let blocks = self.playbook_blocks_projection(&params.session_id, &playbook.markdown);
        let active_run = self.playbook_run_snapshot(&params.session_id);
        self.broadcast_playbook_state(playbook.clone());
        for cursor in cursor_updates {
            // A genuine rebase, not a plain-publish echo — the cursor's own
            // owner needs this broadcast just as much as every peer does, so
            // nothing is excluded.
            let _ = self.broadcast.send(BroadcastMsg::PlaybookCursor {
                payload: construct_protocol::PlaybookCursorNotificationPayload { cursor },
                skip_conn_id: None,
            });
        }
        // Publish the agent's presence cursor over the edit's real span so
        // connected clients see where the agent just wrote (spec 0065 agent
        // presence). This reuses the same `playbook_cursors` map, broadcast,
        // and one-minute TTL as human cursors. `last_edit_span` is `None`
        // for a genuine no-op batch, which skips the publish here too —
        // nothing was actually written, so there is no location to present
        // as "the agent just wrote here".
        if let (Some(conn_id), Some(span)) = (agent_conn_id, last_edit_span) {
            let harness = entry.summary.read().await.harness.clone();
            self.publish_agent_playbook_cursor(
                conn_id,
                &params.session_id,
                &harness,
                span,
                playbook.version,
            )
            .await;
        }
        Ok(PlaybookUpdateResult {
            playbook,
            blocks,
            active_run,
        })
    }

    /// Deliver `text` into `session_id` as its harness's native next input,
    /// picking the framing its harness needs (bracketed-paste typed submit
    /// for an external agent TUI, CR-terminated PTY submit for a PTY-backed
    /// line editor, or a structured adapter input for a headless harness).
    /// Shared by Playbook Run's prompt delivery, verb-drift escalation
    /// (spec 0089), and operator channel deliveries (spec 0176) — all are "say
    /// this to the session as if the user typed it," differing only in the
    /// message. Callers that need the text to appear in the transcript should
    /// go through [`Self::deliver_user_text`] instead.
    async fn deliver_text_to_session(&self, session_id: &str, text: &str) -> Result<()> {
        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let delivery = {
            let summary = entry.summary.read().await;
            session_input_delivery(&summary)
        };
        match delivery {
            SessionInputDelivery::ExternalPtyTypedSubmit => {
                self.playbook_submit_typed_prompt(session_id, text).await?;
            }
            SessionInputDelivery::PtySubmit => {
                // Delivery-awaited (not the enqueue-ACK typing path): callers
                // start run/verb bookkeeping right after this returns, so the
                // prompt must have actually reached the harness by then.
                self.pty_input_delivered(session_id, playbook_pty_submit_bytes(text))
                    .await?;
            }
            SessionInputDelivery::AdapterInput => {
                self.send_input(session_id, text.to_string()).await?;
            }
        }
        Ok(())
    }

    /// Deliver `text` to an already-running session as a user turn: record it
    /// in the transcript, then hand it to the harness with the framing that
    /// harness needs (see [`Self::deliver_text_to_session`]).
    ///
    /// This is the entry point for text that arrives from outside the session
    /// — a operator channel delivery, say — where the session is live and its
    /// harness must actually *start a turn* from the message. Plain
    /// [`Self::send_input`] is not equivalent for a PTY-backed agent TUI: it
    /// writes `text` + LF, and an LF is not the byte a terminal's Enter key
    /// sends, so the message lands in the harness's input box and sits there
    /// unsubmitted.
    ///
    /// The transcript record is conditional because who owns it differs by
    /// path. `send_input` already records on the adapter path. The PTY paths
    /// write without keystroke capture, so nothing implicit attributes the
    /// message to the user — but a harness that mirrors its own native
    /// transcript (codex rollouts) reports the turn itself, and recording it
    /// here too would show the caller's message twice. The harnesses that need
    /// the daemon to speak for them are exactly the ones
    /// [`should_record_pty_user_message`] already names.
    pub(crate) async fn deliver_user_text(&self, session_id: &str, text: &str) -> Result<()> {
        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
        let (delivery, harness) = {
            let summary = entry.summary.read().await;
            (session_input_delivery(&summary), summary.harness.clone())
        };
        if delivery != SessionInputDelivery::AdapterInput
            && should_record_pty_user_message(&harness)
        {
            self.handle_event(
                &entry,
                SessionEvent::Message {
                    role: MessageRole::User,
                    text: text.to_string(),
                },
            )
            .await;
        }
        self.deliver_text_to_session(session_id, text).await
    }

    /// Deliver a Run/verb prompt to the session that will execute it and, on
    /// success, hand the armed run its dispatch fact (spec 0176). The state is
    /// sampled *before* the prompt goes out: it is the daemon's record of how
    /// far that session's own event stream has been consumed, and an idle
    /// there is what makes the next `Running` provably this run's turn rather
    /// than the tail of the session's boot or of the turn before.
    async fn deliver_playbook_run_prompt(&self, session_id: &str, text: &str) -> Result<()> {
        let state_at_dispatch = match self.get_entry(session_id).await {
            Some(entry) => entry.summary.read().await.state,
            None => construct_protocol::SessionState::Pending,
        };
        self.deliver_text_to_session(session_id, text).await?;
        self.mark_playbook_run_dispatched(session_id, state_at_dispatch);
        Ok(())
    }

    /// Create a visible interactive same-harness fork positioned beside its
    /// Playbook owner. Prompt delivery is deliberately separate so callers can
    /// seed owner-side Playbook state before the fork starts acting.
    async fn create_playbook_execution_fork(
        self: &Arc<Self>,
        owner_session_id: &str,
        title: String,
    ) -> Result<String> {
        let entry = self
            .get_entry(owner_session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {owner_session_id}"))?;
        let (cwd, harness, group_id, busy_ms, message_count, tokens) = {
            let summary = entry.summary.read().await;
            let now_ms = Utc::now().timestamp_millis();
            (
                summary.cwd.clone(),
                summary.harness.clone(),
                summary.group_id.clone(),
                summary.busy_ms_at(now_ms),
                summary.message_count,
                summary.tokens,
            )
        };
        let transcript_seq = self
            .storage
            .read_transcript(owner_session_id, 0, None)?
            .events
            .len() as u64;
        self.create(CreateSessionParams {
            harness,
            cwd,
            prompt: None,
            model: None,
            title: Some(title),
            mode: Some("interactive".to_string()),
            pty_size: Some(PtySize {
                cols: 100,
                rows: 30,
            }),
            worktree: false,
            env: HashMap::new(),
            args: Vec::new(),
            kind: construct_protocol::SessionKind::User,
            parent_session_id: None,
            group_id,
            position_after_session_id: Some(owner_session_id.to_string()),
            forked_from: Some(construct_protocol::ForkedFrom {
                session_id: owner_session_id.to_string(),
                transcript_seq,
                at_ms: Utc::now().timestamp_millis(),
                parent_busy_ms: busy_ms,
                parent_message_count: message_count,
                parent_tokens: tokens,
                is_reset_snapshot: false,
            }),
        })
        .await
    }

    /// Wait until a just-created execution fork's harness is actually ready
    /// to receive a prompt, or `max_wait` elapses (`false`: gave up).
    ///
    /// Readiness is the harness's *own* signal — the session reaching
    /// `AwaitingInput` — not a guess derived from how its output looks. A
    /// cold-started TUI does not attach its input handler until well after
    /// its startup draw stops producing bytes, and anything written into the
    /// PTY before that point is flushed away when the harness puts the
    /// terminal into raw mode. Output-shape heuristics cannot see that
    /// boundary; the state machine that already drives the rest of the daemon
    /// can.
    ///
    /// [`PLAYBOOK_FORK_READY_SETTLE`] of PTY quiet is kept as a fallback for
    /// harnesses that never report `AwaitingInput` (one that boots straight
    /// into a turn, say), so those don't pay the full `max_wait` before their
    /// prompt is delivered. `since_ms` guards both signals: the fork must have
    /// drawn *something* since it was created, so a summary that still holds a
    /// pre-spawn state cannot read as ready.
    ///
    /// A fork that reaches `Errored`/`Done` is never going to accept input;
    /// report not-ready immediately rather than burning `max_wait` on it.
    ///
    /// See [`fork_ready_outcome`] for the pure decision step.
    async fn wait_for_fork_ready(&self, id: &str, since_ms: i64, max_wait: Duration) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            let (state, last_pty_at_ms) = match self.get_entry(id).await {
                Some(entry) => {
                    let summary = entry.summary.read().await;
                    (summary.state, summary.last_pty_at_ms)
                }
                None => return false,
            };
            match fork_ready_outcome(
                state,
                last_pty_at_ms,
                since_ms,
                Utc::now().timestamp_millis(),
                started.elapsed(),
                PLAYBOOK_FORK_READY_SETTLE,
                max_wait,
            ) {
                Some(ready) => return ready,
                None => tokio::time::sleep(PLAYBOOK_FORK_READY_POLL).await,
            }
        }
    }

    /// Deliver a run/verb prompt to a just-created execution fork. Unlike
    /// [`Self::deliver_text_to_session`], which assumes an already-running
    /// harness that is draining stdin, this first waits for the fork's
    /// cold-started harness to report itself ready (see
    /// [`Self::wait_for_fork_ready`]), then — for external agent TUIs — gates
    /// the submit Enter on the paste observably reaching the harness (see
    /// `playbook_submit_typed_prompt_cold_start`). Without both, the prompt
    /// raced the harness's boot and either vanished or sat in the input box
    /// unsubmitted, leaving the fork idle. Blocking, potentially for seconds:
    /// call via [`Self::spawn_playbook_fork_prompt_delivery`] so the IPC
    /// dispatch loop is never stalled behind a booting fork.
    async fn deliver_playbook_prompt_to_fork(
        self: &Arc<Self>,
        fork_id: &str,
        created_at_ms: i64,
        prompt: &str,
    ) -> Result<()> {
        let entry = self
            .get_entry(fork_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {fork_id}"))?;
        let (delivery, has_pty) = {
            let summary = entry.summary.read().await;
            (session_input_delivery(&summary), summary.has_pty)
        };
        if has_pty
            && !self
                .wait_for_fork_ready(fork_id, created_at_ms, PLAYBOOK_FORK_STARTUP_TIMEOUT)
                .await
        {
            tracing::warn!(
                session = %fork_id,
                "playbook fork prompt: harness never reported ready; delivering anyway",
            );
        }
        // Sampled after the readiness wait and before the paste — see
        // `deliver_playbook_run_prompt` for why this is the right instant.
        let state_at_dispatch = entry.summary.read().await.state;
        match delivery {
            SessionInputDelivery::ExternalPtyTypedSubmit => {
                self.playbook_submit_typed_prompt_cold_start(fork_id, prompt)
                    .await?;
            }
            SessionInputDelivery::PtySubmit => {
                self.pty_input_delivered(fork_id, playbook_pty_submit_bytes(prompt))
                    .await?;
            }
            SessionInputDelivery::AdapterInput => {
                self.send_input(fork_id, prompt.to_string()).await?;
            }
        }
        self.mark_playbook_run_dispatched(fork_id, state_at_dispatch);
        Ok(())
    }

    /// Background wrapper around [`Self::deliver_playbook_prompt_to_fork`]:
    /// the startup-settle wait lasts as long as the fork's harness takes to
    /// boot, and `playbook.execute` / `playbook.verb_execute` run on the IPC
    /// dispatch loop, which serves a connection's requests serially —
    /// awaiting the boot inline would freeze the requesting client's whole
    /// connection (spec 0087's lesson). Delivery failure is logged; with
    /// `verb_cleanup` set the fork's pending verb merge is dropped and the
    /// fork archived, matching the inline error path this replaced.
    fn spawn_playbook_fork_prompt_delivery(
        self: &Arc<Self>,
        fork_id: String,
        created_at_ms: i64,
        prompt: String,
        verb_cleanup: bool,
    ) {
        let mgr = self.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr
                .deliver_playbook_prompt_to_fork(&fork_id, created_at_ms, &prompt)
                .await
            {
                tracing::warn!(
                    session = %fork_id, error = %e,
                    "playbook fork prompt delivery failed; fork will sit idle",
                );
                if verb_cleanup {
                    mgr.pending_verb_merges.lock().unwrap().remove(&fork_id);
                    let _ = mgr.archive(&fork_id).await;
                }
            }
        });
    }

    pub async fn playbook_execute(
        self: &Arc<Self>,
        params: PlaybookExecuteParams,
    ) -> Result<PlaybookExecuteResult> {
        let entry = self
            .get_entry(&params.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", params.session_id))?;
        let result = PlaybookGetResult {
            playbook: self.storage.read_playbook(&params.session_id)?,
            revisions: self.storage.read_playbook_revisions(&params.session_id)?,
            active_run: self.playbook_run_snapshot(&params.session_id),
            blocks: Vec::new(),
            collaborators: Vec::new(),
        };
        if let Some(base) = params.base_version {
            if base != result.playbook.version {
                anyhow::bail!(
                    "playbook conflict: current version is {}, attempted base version is {}",
                    result.playbook.version,
                    base
                );
            }
        }
        let selected = params
            .selection
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let body = selected.unwrap_or_else(|| result.playbook.markdown.trim());
        if body.is_empty() {
            anyhow::bail!("playbook is empty");
        }
        let run_body = body.to_string();
        let is_selection = params.selection.is_some();
        let run_comment = params
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|comment| !comment.is_empty());

        // Instant-dispatch fast path (spec 0066): before the normal
        // prompt-delivery path below, check whether this is a selection run
        // whose every block is a list item naming exactly one
        // `@{harness:<name>}` clip. If so, the daemon executes the dispatch
        // mechanically — spawn a subagent per item, annotate the playbook,
        // declare each item pending — without paying an LLM round trip
        // through the owning session. Any block that doesn't match falls the
        // *whole* selection through to the normal path unchanged.
        //
        // Uses the *raw* selection (not `run_body`, which trims the whole
        // string) so a nested/indented item's leading whitespace survives
        // into the anchored edit's `old_string` below — trimming the whole
        // selection would strip that indentation from the first line only,
        // making the anchor fail to match the stored document verbatim.
        if run_comment.is_none() {
            if let Some(raw_selection) =
                params.selection.as_deref().filter(|s| !s.trim().is_empty())
            {
                let selection_blocks = construct_protocol::playbook_block_spans(raw_selection);
                if let Some(items) = playbook_dispatch_plan(&selection_blocks) {
                    let owner_cwd = entry.summary.read().await.cwd.clone();
                    return self
                        .playbook_dispatch_execute(
                            &params.session_id,
                            &owner_cwd,
                            raw_selection,
                            items,
                        )
                        .await;
                }
            }
        }

        let scope = if selected.is_some() {
            "selection"
        } else {
            "full"
        };
        let run_context = playbook_run_context(&result.playbook, scope, body);
        self.write_playbook_run_context(&params.session_id, &run_context)?;
        // Arm the run *before* the prompt goes out (#1122). Delivery is what
        // makes the harness start working, so a session can report Running —
        // and, on a fast turn, idle again — before this call returns. Any
        // transition that lands while no run exists is dropped on the floor:
        // the run would then be created with `seen_running: false` for a turn
        // already underway, report `delivered` for work in progress, and miss
        // the idle edge that should have stopped it.
        let queued_behind_current_turn = !params.fork && {
            let summary = entry.summary.read().await;
            summary.state == construct_protocol::SessionState::Running
        };
        self.start_playbook_run_with_dispatch_state(
            &params.session_id,
            &run_body,
            is_selection,
            params.shimmer.as_deref(),
            queued_behind_current_turn,
            params.selection_block_ids.as_deref(),
        );
        let (execution_session_id, prompt) = if params.fork {
            let fork_created_at_ms = Utc::now().timestamp_millis();
            let fork_id = match self
                .create_playbook_execution_fork(&params.session_id, "playbook selection run".into())
                .await
            {
                Ok(fork_id) => fork_id,
                Err(e) => {
                    // No turn will happen; disarm the run we just armed rather
                    // than leave the playbook shimmering for a dispatch that
                    // never left the building.
                    self.clear_playbook_run(&params.session_id);
                    return Err(e);
                }
            };
            // This run's turn happens in the fork, so the fork's lifecycle —
            // not the owner's — is what starts and stops it (spec 0176). Bind
            // it before the (backgrounded) delivery so nothing the owner does
            // in the meantime is mistaken for the fork's work.
            self.bind_playbook_run_execution(&params.session_id, &fork_id);
            // The fork's own context env points at this sidecar. Store the
            // owner's Playbook context there before delivering the first turn.
            self.write_playbook_run_context(&fork_id, &run_context)?;
            let prompt = forked_playbook_execution_prompt(&params.session_id, &fork_id, run_comment);
            self.spawn_playbook_fork_prompt_delivery(
                fork_id.clone(),
                fork_created_at_ms,
                prompt.clone(),
                false,
            );
            (fork_id, prompt)
        } else {
            let prompt = playbook_execution_prompt_with_comment(run_comment);
            if let Err(e) = self
                .deliver_playbook_run_prompt(&params.session_id, &prompt)
                .await
            {
                self.clear_playbook_run(&params.session_id);
                return Err(e);
            }
            (params.session_id.clone(), prompt)
        };

        // A selection Run dispatched to a fork annotates the selection with
        // the fork's session clip — parity with verbs (spec 0089) and the
        // instant-dispatch fast path: the playbook shows *where* the work
        // went, and the clip renders the fork's live state in place.
        // Owner-targeted runs (Shift) add no clip: the work stays in the
        // Playbook-owning session the user is already looking at. Best
        // effort — a drifted/ambiguous anchor skips the clip rather than
        // failing a run whose fork already exists and is booting.
        let mut dispatch_anchor: Option<String> = None;
        let annotated = if params.fork {
            match params.selection.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(raw_selection) => {
                    let anchor = format!("{} @{{session:{}}}", raw_selection, execution_session_id);
                    // The clip lands at the end of the selection, so the
                    // *last* block is the one whose content (and id) gained
                    // it and needs its shimmer re-declared.
                    let content_id = construct_protocol::playbook_block_spans(&anchor)
                        .into_iter()
                        .last()
                        .map(|span| span.id)
                        .unwrap_or_default();
                    let edit_result = self
                        .playbook_edit_from_conn(
                            PlaybookEditParams {
                                session_id: params.session_id.clone(),
                                edits: vec![construct_protocol::PlaybookEdit {
                                    old_string: raw_selection.to_string(),
                                    new_string: anchor.clone(),
                                    replace_all: false,
                                    keep_pending: true,
                                }],
                                actor: construct_protocol::PlaybookUpdateActor::Agent,
                                note: Some("selection run".to_string()),
                                shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                                    id: content_id,
                                    shimmer: true,
                                    tooltip: Some("Running".to_string()),
                                }],
                            },
                            None,
                        )
                        .await;
                    match edit_result {
                        Ok(edit_result) => {
                            dispatch_anchor = Some(anchor);
                            Some(edit_result)
                        }
                        Err(e) => {
                            tracing::warn!(
                                session = %params.session_id, error = %e,
                                "selection run fork: session-clip annotation skipped (anchor did not apply)",
                            );
                            dispatch_anchor = Some(raw_selection.to_string());
                            None
                        }
                    }
                }
                // No selection (API callers forking a full-document Run):
                // the whole executed body is the dispatch.
                None => {
                    dispatch_anchor = Some(run_body.clone());
                    None
                }
            }
        } else {
            None
        };
        // Track the fork's dispatch so closing the fork settles its
        // blocks' shimmer even if the fork never made its own settle edit
        // (see `settle_run_fork_dispatch`).
        if let Some(anchor) = dispatch_anchor {
            self.run_fork_dispatches.lock().unwrap().insert(
                execution_session_id.clone(),
                RunForkDispatch {
                    owner_session_id: params.session_id.clone(),
                    anchor,
                },
            );
        }
        // The annotation edit broadcasts the updated playbook (with the
        // seeded run) itself; only the un-annotated path still owes one.
        let (playbook, blocks) = match annotated {
            Some(edit_result) => (edit_result.playbook, edit_result.blocks),
            None => {
                self.broadcast_playbook_state(result.playbook.clone());
                let blocks =
                    self.playbook_blocks_projection(&params.session_id, &result.playbook.markdown);
                (result.playbook, blocks)
            }
        };
        // Re-snapshot after the edit: `keep_pending` remaps the annotated
        // block's pending ref to its post-clip id.
        let active_run = self.playbook_run_snapshot(&params.session_id);
        Ok(PlaybookExecuteResult {
            playbook,
            prompt,
            active_run,
            blocks,
            execution_session_id: Some(execution_session_id),
        })
    }

    /// Instant-dispatch fast path (spec 0066): spawn one subagent per
    /// `items` entry, then in a single anchored edit append each subagent's
    /// `@{session:<id>}` clip to its item and declare the item pending with
    /// tooltip "Dispatched". No prompt is delivered to `session_id` — the
    /// daemon executes this dispatch mechanically instead of routing it
    /// through the owning session's agent.
    async fn playbook_dispatch_execute(
        self: &Arc<Self>,
        session_id: &str,
        owner_cwd: &str,
        body: &str,
        items: Vec<PlaybookDispatchItem>,
    ) -> Result<PlaybookExecuteResult> {
        let mut edits = Vec::with_capacity(items.len());
        let mut shimmer = Vec::with_capacity(items.len());
        for item in &items {
            let mut env = HashMap::new();
            env.insert(
                "CONSTRUCT_PARENT_SESSION_ID".to_string(),
                session_id.to_string(),
            );
            let subagent_id = self
                .create(CreateSessionParams {
                    harness: item.harness.clone(),
                    cwd: owner_cwd.to_string(),
                    prompt: Some(item.prompt.clone()),
                    model: None,
                    title: Some(format!("subagent:{}", item.harness)),
                    mode: Some("headless".to_string()),
                    pty_size: Some(PtySize {
                        cols: 100,
                        rows: 30,
                    }),
                    worktree: false,
                    env,
                    args: Vec::new(),
                    kind: construct_protocol::SessionKind::Subagent,
                    parent_session_id: Some(session_id.to_string()),
                    group_id: None,
                    position_after_session_id: None,
                    forked_from: None,
                })
                .await?;
            let new_string = format!("{} @{{session:{}}}", item.text, subagent_id);
            let content_id = construct_protocol::playbook_block_spans(&new_string)
                .into_iter()
                .next()
                .map(|span| span.id)
                .unwrap_or_default();
            edits.push(construct_protocol::PlaybookEdit {
                old_string: item.text.clone(),
                new_string,
                replace_all: false,
                keep_pending: true,
            });
            shimmer.push(construct_protocol::PlaybookShimmerDecl {
                id: content_id,
                shimmer: true,
                tooltip: Some("Dispatched".to_string()),
            });
        }

        // Seed the run's pending set over every dispatched item now that every
        // subagent exists, so the response/active_run projection reflects the
        // started run (spec 0042) even before the edit below lands. No
        // `selection_block_ids` to thread here: `playbook_dispatch_plan` (this
        // function's only caller) only ever matches when every re-parsed span
        // is a whole list item containing exactly one `@{harness:<name>}`
        // clip, which a strict partial-line/partial-block selection can never
        // satisfy (the marker and/or clip would be cut off) — this path is
        // unreachable for the substring-selection bug this param exists to fix.
        self.start_playbook_run_with_dispatch_state(
            session_id,
            body,
            true,
            Some(&vec![true; items.len()]),
            false,
            None,
        );

        // The edit below broadcasts `playbook/state` after applying the
        // dispatch annotations, and that snapshot includes the seeded run.
        let edit_result = self
            .playbook_edit_from_conn(
                PlaybookEditParams {
                    session_id: session_id.to_string(),
                    edits,
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: Some("instant dispatch".to_string()),
                    shimmer,
                },
                None,
            )
            .await?;

        Ok(PlaybookExecuteResult {
            playbook: edit_result.playbook,
            prompt: String::new(),
            active_run: edit_result.active_run,
            blocks: edit_result.blocks,
            execution_session_id: None,
        })
    }

    pub fn playbook_templates(&self) -> Result<PlaybookListTemplatesResult> {
        Ok(PlaybookListTemplatesResult {
            templates: self.storage.playbook_templates()?,
        })
    }

    pub fn playbook_verbs(&self) -> PlaybookListVerbsResult {
        PlaybookListVerbsResult {
            verbs: self.storage.playbook_verbs(),
        }
    }

    /// Run a Playbook selection verb (spec 0089): fork the owning session
    /// into a new sibling scoped to `params.selection`, annotate the
    /// selection with that session's clip (the same in-flight affordance as
    /// the 0066 fast path), and record a pending merge that resolves once
    /// the verb session delivers a result — see
    /// [`Self::maybe_complete_verb_merge`].
    pub async fn playbook_verb_execute(
        self: &Arc<Self>,
        params: PlaybookVerbExecuteParams,
    ) -> Result<PlaybookVerbExecuteResult> {
        let entry = self
            .get_entry(&params.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", params.session_id))?;
        let verb = self
            .storage
            .playbook_verbs()
            .into_iter()
            .find(|v| v.name == params.verb)
            .ok_or_else(|| anyhow!("unknown playbook verb: {}", params.verb))?;

        let playbook = self.storage.read_playbook(&params.session_id)?;
        if let Some(base) = params.base_version {
            if base != playbook.version {
                anyhow::bail!(
                    "playbook conflict: current version is {}, attempted base version is {}",
                    playbook.version,
                    base
                );
            }
        }
        let selection_trimmed = params.selection.trim();
        if selection_trimmed.is_empty() {
            anyhow::bail!("verb selection is empty");
        }
        let comment = params
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());

        // Owner delivery implies a direct edit: there is no fork whose
        // structured result the daemon could merge back, so `direct_edit` is
        // not consulted here. Honoring `run_on_owner` on its own also keeps a
        // caller that asks for the owner from silently getting a fork.
        if params.run_on_owner {
            self.start_playbook_run_with_dispatch_state(
                &params.session_id,
                &params.selection,
                true,
                None,
                entry.summary.read().await.state == construct_protocol::SessionState::Running,
                params.selection_block_ids.as_deref(),
            );
            let prompt = playbook_verb_prompt(
                &verb,
                &params.session_id,
                &playbook.markdown,
                selection_trimmed,
                comment,
                Some((&params.session_id, &params.selection)),
            );
            self.deliver_playbook_run_prompt(&params.session_id, &prompt)
                .await?;
            self.broadcast_playbook_state(playbook.clone());
            let blocks = self.playbook_blocks_projection(&params.session_id, &playbook.markdown);
            return Ok(PlaybookVerbExecuteResult {
                playbook,
                subagent_session_id: params.session_id,
                verb: verb.name,
                blocks,
            });
        }

        // Fork, not a fresh child (spec 0089): when the owning session's
        // harness supports native fork-resume (currently claude, codex,
        // opencode, grok — see `Self::native_id_file_name`), spawning with the same
        // harness and `forked_from` set makes the daemon's existing fork-
        // resume wiring (`session/lifecycle.rs`) hand the new process the
        // owning session's actual native conversation state — real model
        // memory, not a rendered summary. Harnesses without a native fork
        // primitive just get an ordinary fresh start; `forked_from` still
        // records the lineage but has no functional effect there. Either
        // way the verb's own prompt (built below, full document + selection)
        // is still the instruction for *this* turn — a resumed conversation
        // remembers the past but still needs to be told what to do now.
        let verb_created_at_ms = Utc::now().timestamp_millis();
        let verb_session_id = self
            .create_playbook_execution_fork(&params.session_id, format!("verb:{}", verb.name))
            .await?;

        // Seed the run's pending set over the verb's selection before the
        // provisional edit below declares it, so that edit's
        // `PlaybookShimmerDecl` isn't a no-op: `narrow_playbook_run` only
        // narrows an *existing* run (spec 0053) and creates nothing on its
        // own, so without this the selected block would never visibly
        // shimmer unless a real Run happened to already be in flight on the
        // same session. `is_selection: true` means a concurrently active
        // Run's own pending set is added to, not clobbered, mirroring
        // selection Run's own semantics (spec 0042).
        self.start_playbook_run_with_dispatch_state(
            &params.session_id,
            &params.selection,
            true,
            None,
            false,
            params.selection_block_ids.as_deref(),
        );
        // The verb's turn happens in its own session (spec 0176).
        self.bind_playbook_run_execution(&params.session_id, &verb_session_id);

        let anchor = format!("{} @{{session:{}}}", params.selection, verb_session_id);
        let content_id = construct_protocol::playbook_block_spans(&anchor)
            .into_iter()
            .next()
            .map(|span| span.id)
            .unwrap_or_default();
        let edit_result = self
            .playbook_edit_from_conn(
                PlaybookEditParams {
                    session_id: params.session_id.clone(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: params.selection.clone(),
                        new_string: anchor.clone(),
                        replace_all: false,
                        keep_pending: true,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: Some(format!("verb: {}", verb.name)),
                    shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                        id: content_id,
                        shimmer: true,
                        tooltip: Some(verb.label.clone()),
                    }],
                },
                None,
            )
            .await;
        let edit_result = match edit_result {
            Ok(result) => result,
            Err(e) => {
                // The selection anchor didn't apply (e.g. it changed or is
                // not unique between the read above and this edit) — the
                // verb session already exists but nothing will ever merge
                // its result, so it would otherwise linger unexplained.
                let _ = self.archive(&verb_session_id).await;
                return Err(e);
            }
        };

        let prompt = playbook_verb_prompt(
            &verb,
            &params.session_id,
            &playbook.markdown,
            selection_trimmed,
            comment,
            params
                .direct_edit
                .then_some((params.session_id.as_str(), anchor.as_str())),
        );

        if params.direct_edit {
            self.pending_verb_merges.lock().unwrap().insert(
                verb_session_id.clone(),
                PendingVerbMerge {
                    playbook_session_id: params.session_id.clone(),
                    verb: verb.clone(),
                    anchor: anchor.clone(),
                    // Sentinel path: direct writers never receive it, so
                    // terminal cleanup can only settle/retire, never merge.
                    result_file: self
                        .storage
                        .widgets_dir(&verb_session_id)
                        .join("direct-edit-no-merge"),
                },
            );
            // Registered before delivery: the background task's failure
            // cleanup (drop merge + archive) relies on the entry existing.
            self.spawn_playbook_fork_prompt_delivery(
                verb_session_id.clone(),
                verb_created_at_ms,
                prompt,
                true,
            );
            return Ok(PlaybookVerbExecuteResult {
                playbook: edit_result.playbook,
                subagent_session_id: verb_session_id,
                verb: verb.name,
                blocks: edit_result.blocks,
            });
        }

        self.pending_verb_merges.lock().unwrap().insert(
            verb_session_id.clone(),
            PendingVerbMerge {
                playbook_session_id: params.session_id.clone(),
                verb: verb.clone(),
                anchor,
                result_file: self
                    .storage
                    .widgets_dir(&verb_session_id)
                    .join("verb-result.json"),
            },
        );
        self.spawn_playbook_fork_prompt_delivery(
            verb_session_id.clone(),
            verb_created_at_ms,
            prompt,
            true,
        );

        Ok(PlaybookVerbExecuteResult {
            playbook: edit_result.playbook,
            subagent_session_id: verb_session_id,
            verb: verb.name,
            blocks: edit_result.blocks,
        })
    }

    /// Called off every session state transition (see `session::events`).
    /// When `session_id` has a pending verb merge and its result file now
    /// exists, consumes and applies it exactly once. When the subagent
    /// reaches a terminal state with no result ever written, the verb is
    /// abandoned: the pending entry is dropped and the document is left
    /// untouched (spec 0089 — a cancelled/errored verb must not leave a
    /// stray in-flight affordance, but also must not apply a partial
    /// result).
    pub(crate) async fn maybe_complete_verb_merge(
        &self,
        session_id: &str,
        state: construct_protocol::SessionState,
    ) {
        let pending = {
            let map = self.pending_verb_merges.lock().unwrap();
            map.get(session_id).cloned()
        };
        let Some(pending) = pending else {
            return;
        };
        if !pending.result_file.exists() {
            if matches!(
                state,
                construct_protocol::SessionState::Done | construct_protocol::SessionState::Errored
            ) {
                self.pending_verb_merges.lock().unwrap().remove(session_id);
                self.settle_verb_shimmer(&pending).await;
                tracing::info!(
                    session = %session_id,
                    verb = %pending.verb.name,
                    "verb session ended without a result; leaving playbook untouched"
                );
            }
            return;
        }
        // Consume before awaiting so a second state transition arriving
        // while the merge below is in flight can't double-apply it.
        self.pending_verb_merges.lock().unwrap().remove(session_id);
        if let Err(e) = self.apply_verb_merge(session_id, pending).await {
            tracing::warn!(session = %session_id, error = %e, "playbook verb merge failed");
        }
    }

    /// Shimmer-settle declarations for every block of a verb's anchor.
    /// Content ids that no longer exist in the live document are ignored by
    /// the narrowing (fail closed), so these are always safe to over-declare.
    fn verb_anchor_settle_decls(anchor: &str) -> Vec<construct_protocol::PlaybookShimmerDecl> {
        construct_protocol::playbook_block_spans(anchor)
            .into_iter()
            .map(|span| construct_protocol::PlaybookShimmerDecl {
                id: span.id,
                shimmer: false,
                tooltip: None,
            })
            .collect()
    }

    /// Settle a verb's in-progress shimmer without touching the document
    /// (spec 0089: every terminal path — merged, drift-escalated, or
    /// abandoned — must clear the verb's in-flight affordance, but only the
    /// merge itself may change the text). Declares every anchor block
    /// settled directly against the run's pending set instead of going
    /// through an edit, so it also works when the anchor no longer matches
    /// the document — exactly the drift case an anchored no-op edit cannot
    /// reach. A multi-block anchor settles in full, not just its first block.
    async fn settle_verb_shimmer(&self, pending: &PendingVerbMerge) {
        let decls = Self::verb_anchor_settle_decls(&pending.anchor);
        if decls.is_empty() {
            return;
        }
        let Ok(playbook) = self.storage.read_playbook(&pending.playbook_session_id) else {
            return;
        };
        self.narrow_playbook_run(&pending.playbook_session_id, &playbook.markdown, &decls);
        self.broadcast_playbook_state(playbook);
    }

    /// Deterministic backstop for a closing selection-Run fork (spec 0137):
    /// settle the shimmer of every block the fork was dispatched for, so a
    /// fork that archives (or is deleted) without making its own settle
    /// edit can never leave the owner Playbook shimmering forever. Two
    /// complementary sources identify the fork's blocks:
    /// - the dispatch anchor's content ids, which settle blocks the fork
    ///   never edited (mirroring `settle_verb_shimmer`), and
    /// - a live-document scan for the fork's `@{session:<id>}` clip, which
    ///   settles blocks whose text (and therefore ids) drifted while the
    ///   fork worked on them — the common case, since the fork is told to
    ///   update its blocks in place and the clip travels with the block.
    /// No-op for untracked sessions; consumes the tracking entry so a
    /// second close path (archive then delete) cannot double-settle.
    async fn settle_run_fork_dispatch(&self, fork_id: &str) {
        let dispatch = self.run_fork_dispatches.lock().unwrap().remove(fork_id);
        let Some(dispatch) = dispatch else {
            return;
        };
        let Ok(playbook) = self.storage.read_playbook(&dispatch.owner_session_id) else {
            return;
        };
        let mut decls = Self::verb_anchor_settle_decls(&dispatch.anchor);
        let clip = format!("@{{session:{fork_id}}}");
        let blocks = self.playbook_blocks_projection(&dispatch.owner_session_id, &playbook.markdown);
        for block in &blocks {
            if block.text.contains(&clip) && !decls.iter().any(|decl| decl.id == block.content_id) {
                decls.push(construct_protocol::PlaybookShimmerDecl {
                    id: block.content_id.clone(),
                    shimmer: false,
                    tooltip: None,
                });
            }
        }
        if decls.is_empty() {
            return;
        }
        self.narrow_playbook_run(&dispatch.owner_session_id, &playbook.markdown, &decls);
        self.broadcast_playbook_state(playbook);
    }

    async fn apply_verb_merge(
        &self,
        verb_session_id: &str,
        pending: PendingVerbMerge,
    ) -> Result<()> {
        let raw = std::fs::read(&pending.result_file)
            .with_context(|| format!("read verb result {}", pending.result_file.display()))?;
        let result: VerbResultPayload =
            serde_json::from_slice(&raw).context("parse verb result JSON")?;
        let content = result.content.trim();
        if content.is_empty() {
            anyhow::bail!("verb result content is empty");
        }
        let clip = format!("@{{session:{verb_session_id}}}");
        let new_string = match pending.verb.effect {
            construct_protocol::PlaybookVerbEffect::Annotate => {
                format!("{}\n\n{}", pending.anchor, content)
            }
            construct_protocol::PlaybookVerbEffect::Rewrite => {
                if content.contains(&clip) {
                    content.to_string()
                } else {
                    format!("{content}\n\n{clip}")
                }
            }
        };
        let merge = self
            .playbook_edit_from_conn(
                PlaybookEditParams {
                    session_id: pending.playbook_session_id.clone(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: pending.anchor.clone(),
                        new_string,
                        replace_all: false,
                        keep_pending: false,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: Some(format!("verb: {}", pending.verb.name)),
                    // Settle the verb's shimmer in the same edit. An annotate
                    // merge keeps the anchor blocks' text — and therefore
                    // their content ids — alive, so without an explicit
                    // settle they would stay in the run's pending set and
                    // shimmer forever after the verb completed. A rewrite's
                    // old ids drop out of the pending set on their own; the
                    // extra declarations are ignored fail-closed.
                    shimmer: Self::verb_anchor_settle_decls(&pending.anchor),
                },
                None,
            )
            .await;
        match merge {
            Ok(_) => {
                let _ = self.archive(verb_session_id).await;
                Ok(())
            }
            Err(_) => {
                // The anchor drifted underneath the verb — the user (or
                // another edit) touched the selection while it was in
                // flight. Escalate to the Playbook-owning session rather
                // than silently discarding a completed verb session's
                // result. Archive unconditionally, even if delivery itself
                // fails (e.g. the owning session has no live adapter right
                // now) — the verb's job is done either way, and leaving it
                // un-archived would strand it in the active list forever.
                // The verb is complete either way, so its in-flight shimmer
                // settles now; the owning session re-declares shimmer as it
                // works if it wants to (partial drift can leave some anchor
                // blocks' ids alive and still pending, so this is not a
                // no-op).
                self.settle_verb_shimmer(&pending).await;
                if let Err(e) = self
                    .escalate_verb_drift(&pending, verb_session_id, content)
                    .await
                {
                    tracing::warn!(
                        session = %verb_session_id,
                        error = %e,
                        "verb drift escalation delivery failed"
                    );
                }
                let _ = self.archive(verb_session_id).await;
                Ok(())
            }
        }
    }

    async fn escalate_verb_drift(
        &self,
        pending: &PendingVerbMerge,
        verb_session_id: &str,
        content: &str,
    ) -> Result<()> {
        let effect_label = match pending.verb.effect {
            construct_protocol::PlaybookVerbEffect::Annotate => "annotate",
            construct_protocol::PlaybookVerbEffect::Rewrite => "rewrite",
        };
        let message = format!(
            "The \"{label}\" verb (session {verb_session_id}) finished on a Playbook selection \
             that has since changed, so its result could not be merged automatically.\n\n\
             Original selection anchor:\n{anchor}\n\n\
             Verb result ({effect_label}):\n{content}\n\n\
             Please reconcile: read the current playbook, then apply an anchored edit that \
             incorporates this result into the document as it stands now, using whichever \
             playbook-edit tool you have available (construct_playbook_edit via MCP, or \
             agentd_playbook_edit natively).",
            label = pending.verb.label,
            anchor = pending.anchor,
        );
        self.deliver_text_to_session(&pending.playbook_session_id, &message)
            .await
    }

    fn broadcast_playbook_state(&self, playbook: PlaybookDocument) {
        let active_run = self.playbook_run_snapshot(&playbook.session_id);
        let blocks = self.playbook_blocks_projection(&playbook.session_id, &playbook.markdown);
        let _ = self.broadcast.send(BroadcastMsg::PlaybookState(
            PlaybookStateNotificationPayload {
                playbook,
                active_run,
                blocks,
            },
        ));
    }

    /// Rebases every peer's Playbook cursor through a just-applied batch of
    /// edits, returning the updates to broadcast. `exclude_conn_id`
    /// additionally skips one more cursor besides the source connection —
    /// the agent's own reserved pseudo-cursor from a prior edit, whose stale
    /// span is about to be superseded by the fresh one this edit produces
    /// (see [`Self::publish_agent_playbook_cursor`]), so rebasing (and
    /// broadcasting) it here would be a spurious update immediately followed
    /// by the correct one.
    fn rebase_playbook_cursors_after_edit(
        &self,
        session_id: &str,
        before_markdown: &str,
        edits: &[construct_protocol::PlaybookEdit],
        version: u64,
        source_conn_id: Option<u64>,
        exclude_conn_id: Option<u64>,
    ) -> Vec<construct_protocol::PlaybookCursor> {
        let Ok(replacements) = playbook_cursor_replacements(before_markdown, edits) else {
            return Vec::new();
        };
        if replacements.is_empty() {
            return Vec::new();
        }
        let now = chrono::Utc::now().timestamp_millis();
        let mut updates = Vec::new();
        if let Ok(mut cursors) = self.playbook_cursors.lock() {
            for (conn_id, cursor) in cursors.iter_mut() {
                if Some(*conn_id) == source_conn_id
                    || Some(*conn_id) == exclude_conn_id
                    || !cursor.active
                    || cursor.session_id != session_id
                {
                    continue;
                }
                let old_cursor = cursor.cursor;
                let old_anchor = cursor.selection_anchor;
                let old_head = cursor.selection_head;
                cursor.cursor = playbook_rebase_offset(cursor.cursor, &replacements);
                cursor.selection_anchor = cursor
                    .selection_anchor
                    .map(|p| playbook_rebase_offset(p, &replacements));
                cursor.selection_head = cursor
                    .selection_head
                    .map(|p| playbook_rebase_offset(p, &replacements));
                cursor.version = Some(version);
                if cursor.cursor != old_cursor
                    || cursor.selection_anchor != old_anchor
                    || cursor.selection_head != old_head
                {
                    // Agent presence (spec 0065 agent presence): don't renew
                    // this cursor's freshness stamp just because a
                    // *different* edit shifted its position underneath it —
                    // only the agent's own writes
                    // (`publish_agent_playbook_cursor`) should renew "the
                    // agent just wrote here" for clients gating the reveal
                    // highlight off `updated_at_ms`. The position is still
                    // corrected and broadcast either way.
                    if cursor.kind != "agent" {
                        cursor.updated_at_ms = now;
                    }
                    updates.push(cursor.clone());
                }
            }
        }
        updates
    }

    /// Reserve (or reuse) a synthetic connection id for `session_id`'s
    /// agent-authored Playbook cursor. Allocated from the same counter as real
    /// client connections so it can never collide with one.
    fn agent_playbook_cursor_conn_id(&self, session_id: &str) -> u64 {
        if let Ok(mut ids) = self.agent_playbook_cursor_conn_ids.lock() {
            if let Some(id) = ids.get(session_id) {
                return *id;
            }
            let id = self.alloc_conn_id();
            ids.insert(session_id.to_string(), id);
            return id;
        }
        self.alloc_conn_id()
    }

    /// Publish an ephemeral Playbook cursor for an agent-authored edit (spec
    /// 0065 agent presence): a labeled point cursor at the end of the last
    /// applied edit (`span.1`), with `selection_anchor`/`selection_head` set
    /// to `span` so clients can briefly reveal-highlight where it landed.
    /// Delegates to the same [`Self::playbook_cursor`] human cursors go
    /// through, so label-uniqueness, storage, and broadcast stay in one
    /// place; `kind: "agent"` is what lets renderers style it distinctly and
    /// interpret the selection fields as a reveal span rather than a real
    /// text selection. The caller (`conn_id`) is the session's reserved
    /// pseudo-connection id from [`Self::agent_playbook_cursor_conn_id`].
    async fn publish_agent_playbook_cursor(
        &self,
        conn_id: u64,
        session_id: &str,
        harness: &str,
        span: (usize, usize),
        version: u64,
    ) {
        // An empty/unknown harness name defers to `playbook_cursor`'s own
        // `playbook_default_cursor_label("agent")` fallback rather than
        // hardcoding "agent" here too, so the two stay in sync if that
        // fallback is ever given a nicer label (as "web"/"tui" already have).
        let trimmed = harness.trim();
        let label = (!trimmed.is_empty()).then(|| trimmed.to_string());
        let _ = self
            .playbook_cursor(
                conn_id,
                "agent",
                construct_protocol::PlaybookCursorParams {
                    session_id: session_id.to_string(),
                    cursor: span.1,
                    selection_anchor: Some(span.0),
                    selection_head: Some(span.1),
                    version: Some(version),
                    label,
                    clear: false,
                },
            )
            .await;
    }

    fn playbook_run_context_path(&self, session_id: &str) -> PathBuf {
        self.storage
            .session_dir(session_id)
            .join("playbook-run-context.json")
    }

    fn install_playbook_run_context_env(&self, env: &mut HashMap<String, String>, session_id: &str) {
        env.insert(
            agent_context::ENV_PLAYBOOK_RUN_CONTEXT_FILE.to_string(),
            self.playbook_run_context_path(session_id)
                .to_string_lossy()
                .to_string(),
        );
    }

    fn write_playbook_run_context(
        &self,
        session_id: &str,
        context: &agent_context::PlaybookRunContext,
    ) -> Result<()> {
        let path = self.playbook_run_context_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(context).context("serialize playbook run context")?;
        std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Gracefully stop every live adapter. Used for intentional daemon
    /// termination; restart-oriented signals skip this so reconnectable
    /// adapters can survive the daemon process.
    ///
    /// Sets [`Self::is_shutting_down`] before sending the SHUTDOWN
    /// RPCs so the `drain_adapter` task knows to keep the session's
    /// pre-shutdown state on disk (instead of marking it `Done` from
    /// the resulting `AdapterMessage::Closed`). Without that,
    /// `resume_running_sessions` would skip every session on the
    /// next boot because they'd all be terminal.
    pub async fn shutdown_adapters(&self) {
        self.is_shutting_down.store(true, Ordering::Release);
        let entries: Vec<Arc<SessionEntry>> = {
            let guard = self.sessions.read().await;
            guard.values().cloned().collect()
        };
        for entry in entries {
            if let Some(adapter) = entry.adapter.lock().await.clone() {
                let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
            }
        }
    }

    async fn drain_adapter(
        self: Arc<Self>,
        entry: Arc<SessionEntry>,
        mut msg_rx: mpsc::Receiver<AdapterMessage>,
    ) {
        while let Some(msg) = msg_rx.recv().await {
            match msg {
                AdapterMessage::Event(env) => {
                    self.handle_event(&entry, env.event).await;
                }
                AdapterMessage::Log {
                    _session_id: _,
                    line,
                } => {
                    tracing::info!(session = %entry.id, "adapter: {line}");
                }
                AdapterMessage::Closed { exit_code } => {
                    if entry.is_deleted() {
                        // Session was deleted out from under us — don't
                        // resurrect storage or broadcast a stale state.
                        *entry.adapter.lock().await = None;
                        break;
                    }
                    // Minibuffer-initiated shutdown (SIGINT/SIGTERM →
                    // `shutdown_adapters`): the adapter exiting is
                    // *expected*, not a session ending. Leave the
                    // session's persisted state untouched so it's
                    // resumable on the next daemon boot. Without
                    // this guard a graceful daemon restart marks
                    // every live session `Done` and the next start's
                    // `resume_running_sessions` skips them all.
                    if self.is_shutting_down.load(Ordering::Acquire) {
                        *entry.adapter.lock().await = None;
                        break;
                    }
                    let mut summary = entry.summary.write().await;
                    if !summary.state.is_terminal() {
                        let terminal = if exit_code.unwrap_or(0) == 0 {
                            SessionState::Done
                        } else {
                            SessionState::Errored
                        };
                        set_state_tracked(
                            &mut summary,
                            terminal,
                            chrono::Utc::now().timestamp_millis(),
                        );
                    }
                    // `archive` records its intent on the entry before
                    // terminating the adapter — which is what triggered this
                    // Closed event. Whichever of the two writers lands last,
                    // keep the session archived so we never persist/broadcast a
                    // stale `archived = false` and downgrade the row back to a
                    // plain stopped session (the "needs a second archive" bug).
                    if entry.archived.load(Ordering::Acquire) {
                        summary.archived = true;
                    }
                    summary.last_event_at = Some(Utc::now());
                    let snapshot = summary.clone();
                    drop(summary);
                    let _ = self.storage.save_summary(&snapshot);
                    *entry.adapter.lock().await = None;
                    let _ = self
                        .broadcast
                        .send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                    break;
                }
            }
        }
    }

    pub async fn emit_session_event(&self, p: SessionEmitEventParams) -> Result<()> {
        let entry = self
            .get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        self.handle_event(&entry, p.event).await;
        Ok(())
    }

    pub async fn attach_clipboard(
        &self,
        p: SessionAttachClipboardParams,
    ) -> Result<SessionAttachClipboardResult> {
        self.get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&p.data)
            .context("decode clipboard attachment")?;
        if bytes.is_empty() {
            anyhow::bail!("clipboard attachment is empty");
        }
        if bytes.len() > MAX_CLIPBOARD_ATTACHMENT_BYTES {
            anyhow::bail!(
                "clipboard attachment is too large: {} bytes (max {})",
                bytes.len(),
                MAX_CLIPBOARD_ATTACHMENT_BYTES
            );
        }

        let dir = self
            .storage
            .data_dir()
            .join("sessions")
            .join(&p.session_id)
            .join("attachments");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("create {}", dir.display()))?;

        let ext = extension_for_attachment(p.filename.as_deref(), p.mime.as_deref(), &bytes);
        let stem = p
            .filename
            .as_deref()
            .and_then(sanitized_file_stem)
            .unwrap_or_else(|| "clipboard".to_string());
        let ts = Utc::now().format("%Y%m%d-%H%M%S%.3f");
        let mut path = dir.join(format!("{stem}-{ts}.{ext}"));
        let mut suffix = 1usize;
        while tokio::fs::try_exists(&path).await.unwrap_or(false) {
            path = dir.join(format!("{stem}-{ts}-{suffix}.{ext}"));
            suffix += 1;
        }
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("write {}", path.display()))?;

        let path_str = path.display().to_string();
        let reference = format!("[#file:{}]", path_str);
        Ok(SessionAttachClipboardResult {
            path: path_str,
            reference,
        })
    }

    /// Read one file back from a session's attachments dir (spec 0099: web
    /// attachment previews). `filename` must be a bare name — any path
    /// separator or traversal component is rejected, so this can never serve
    /// a file outside the attachments directory.
    pub async fn read_attachment(
        &self,
        p: construct_protocol::SessionReadAttachmentParams,
    ) -> Result<construct_protocol::SessionReadAttachmentResult> {
        self.get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        if p.filename.is_empty()
            || p.filename.contains('/')
            || p.filename.contains('\\')
            || p.filename == "."
            || p.filename == ".."
        {
            anyhow::bail!("invalid attachment filename: {}", p.filename);
        }
        let path = self
            .storage
            .data_dir()
            .join("sessions")
            .join(&p.session_id)
            .join("attachments")
            .join(&p.filename);
        let meta = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("attachment not found: {}", p.filename))?;
        if !meta.is_file() {
            anyhow::bail!("attachment is not a file: {}", p.filename);
        }
        if meta.len() as usize > MAX_CLIPBOARD_ATTACHMENT_BYTES {
            anyhow::bail!("attachment too large: {} bytes", meta.len());
        }
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        Ok(construct_protocol::SessionReadAttachmentResult {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            mime: mime_for_attachment_ext(&path),
        })
    }

    pub async fn send_input(&self, id: &str, text: String) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        // Record the input as a user message so it shows in the transcript.
        // Auto-title is triggered inside handle_event for any User message.
        self.handle_event(
            &entry,
            SessionEvent::Message {
                role: MessageRole::User,
                text: text.clone(),
            },
        )
        .await;
        let params = serde_json::to_value(&construct_protocol::SessionInputParams {
            session_id: id.to_string(),
            text,
        })?;
        adapter.request(ahp_method::SESSION_INPUT, params).await?;
        Ok(())
    }

    /// Kick off auto-title generation in the background if (a) the user
    /// has not set a title yet (i.e. the `title` field is `None` — the
    /// hash shown in the UI is just `primary_label`'s display fallback),
    /// (b) we haven't already attempted this incarnation, and (c) `prompt`
    /// is the first *non-slash-command* message seen (leading
    /// `/model gpt-5.5`-style messages are ignored entirely).
    ///
    /// Generator choice (spec 0151): a *title-capable* smith credential runs
    /// the cheap `--title-mode` process one-shot; without one, sessions on
    /// model harnesses fall back to a hidden probe session on their own
    /// harness — the same credentials/subscription the user's session
    /// already proved work. The probe is also where a one-shot that ran but
    /// produced nothing lands, since the per-session attempt latch is spent
    /// by then and would otherwise strand the session on its hash name until
    /// a daemon restart. smith and shell sessions have no fallback; their
    /// skip is recorded via [`Self::note_ambient_degradation`] so the gap is
    /// visible instead of silent.
    async fn maybe_spawn_auto_title(&self, entry: Arc<SessionEntry>, prompt: String) {
        // Cheap checks first so we don't burn the per-session attempt
        // budget (the AtomicBool flip is one-way until a daemon
        // restart) on inputs that wouldn't have produced a title
        // anyway.
        let Some(prompt) = auto_title_prompt(prompt) else {
            return;
        };
        // Already claimed (title-gen ran, or the user set a title
        // directly) — skip entirely.
        if entry.title_gen_attempted.load(Ordering::SeqCst) {
            return;
        }
        // Ask whether title-gen can resolve a model, not the broader
        // `probe_smith` question of whether *a session* could start: the
        // latter also counts OAuth subscriptions and Ollama, which
        // `--title-mode` refuses (spec 0071). Using it here spent the latch
        // on a one-shot that could only fail, so OAuth-only machines never
        // reached the fallback below that exists for them.
        let smith_cmd = if crate::availability::smith_title_gen_available() {
            self.config().adapters.get("smith").and_then(|smith_adapter| {
                let binary_spec = smith_adapter
                    .binary
                    .clone()
                    .unwrap_or_else(|| "construct".to_string());
                locate_binary(&binary_spec).map(|binary| (binary, smith_adapter.args.clone()))
            })
        } else {
            None
        };
        let (harness, kind) = {
            let s = entry.summary.read().await;
            (s.harness.clone(), s.kind)
        };
        // No model to fall back to — smith sessions ARE the missing
        // credential and shell has no model at all.
        let harness_can_probe = harness != "smith" && harness != "shell";
        // Probe fallback only for user sessions: minibuffer/subagent/
        // probe sessions are normally titled by whatever created them, and
        // an automatic per-session harness spawn is too heavy to run for a
        // whole fleet's worth of children. Needs the full manager to create
        // / poll / delete the probe session; unbound self_ref (unit tests)
        // just skips — best-effort, like everything else on this path.
        let probe_eligible =
            harness_can_probe && kind == construct_protocol::SessionKind::User;
        let mgr = self.self_ref.get().and_then(std::sync::Weak::upgrade);

        let Some((binary, prefix_args)) = smith_cmd else {
            if !harness_can_probe {
                // Claim the attempt (same one-way semantics as the
                // generating paths) and make the skip visible once instead
                // of silently keeping the hash name (spec 0151).
                if entry.title_gen_attempted.swap(true, Ordering::SeqCst) {
                    return;
                }
                self.note_ambient_degradation("auto_title").await;
                return;
            }
            let Some(mgr) = mgr.filter(|_| probe_eligible) else {
                return;
            };
            if entry.title_gen_attempted.swap(true, Ordering::SeqCst) {
                return;
            }
            let replace_pending_title = entry.summary.read().await.auto_title_pending;
            tokio::spawn(mgr.generate_auto_title_via_probe(entry, prompt, replace_pending_title));
            return;
        };

        // Now claim the attempt. `swap` is the one place we mark this
        // session as "tried"; the user-renamed path is handled by
        // `title_gen_attempted` being initialized to `title.is_some()`
        // when the entry is constructed (both at create-time and when
        // loaded from disk on daemon restart).
        if entry.title_gen_attempted.swap(true, Ordering::SeqCst) {
            return;
        }
        let replace_pending_title = entry.summary.read().await.auto_title_pending;
        let storage = self.storage.clone();
        let broadcast_tx = self.broadcast.clone();
        tokio::spawn(async move {
            let outcome = generate_auto_title(
                binary,
                prefix_args,
                entry.clone(),
                prompt.clone(),
                replace_pending_title,
                storage,
                broadcast_tx,
            )
            .await;
            if matches!(outcome, TitleOutcome::Settled) {
                return;
            }
            // The credential resolved but the one-shot still came back
            // empty (network error, revoked key, model refusal). The latch
            // is already spent, so treat this exactly like having had no
            // credential at all rather than leaving the session unnamed.
            let Some(mgr) = mgr else {
                return;
            };
            if probe_eligible {
                mgr.generate_auto_title_via_probe(entry, prompt, replace_pending_title)
                    .await;
            } else if !harness_can_probe {
                mgr.note_ambient_degradation("auto_title").await;
            }
        });
    }

    /// Same-harness auto-title fallback (spec 0151): when smith has no
    /// credential, spawn a hidden probe session on the target session's own
    /// harness, wait for its reply, sanitize it into a title, tear the
    /// probe down, and apply the result. Mirrors
    /// [`Self::generate_suggestions_via_probe`]'s lifecycle; best-effort
    /// throughout — any failure just leaves the session's title unset.
    ///
    /// Returns a boxed (type-erased) future rather than being an `async
    /// fn`: the probe calls `create`, whose future runs the prompt-as-event
    /// hook, which is `maybe_spawn_auto_title` itself — as plain opaque
    /// futures that cycle would give the compiler an infinitely recursive
    /// future type. The `dyn` boundary here is what breaks the cycle.
    fn generate_auto_title_via_probe(
        self: Arc<Self>,
        entry: Arc<SessionEntry>,
        prompt: String,
        replace_pending_title: bool,
    ) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(self.generate_auto_title_via_probe_inner(entry, prompt, replace_pending_title))
    }

    async fn generate_auto_title_via_probe_inner(
        self: Arc<Self>,
        entry: Arc<SessionEntry>,
        prompt: String,
        replace_pending_title: bool,
    ) {
        let (harness, cwd, model) = {
            let s = entry.summary.read().await;
            (s.harness.clone(), s.cwd.clone(), s.model.clone())
        };
        let create_params = construct_protocol::CreateSessionParams {
            harness: harness.clone(),
            cwd,
            prompt: Some(format!("{AUTO_TITLE_PROBE_INSTRUCTIONS}\n\n{prompt}")),
            model,
            title: Some("title probe".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(construct_protocol::PtySize {
                cols: 100,
                rows: 40,
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
        let probe_id = match self.create(create_params).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!(%harness, error = %e, "title probe: create failed");
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + TITLE_PROBE_TIMEOUT;
        let mut title: Option<String> = None;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(700)).await;
            if entry.is_deleted() {
                break;
            }
            let probe_state = match self.get_entry(&probe_id).await {
                Some(p) => p.summary.read().await.state,
                None => break,
            };
            // Track the latest assistant message each tick — a preamble
            // ("Sure, here's a title…") gets overwritten by the real reply
            // — but only accept the result once the probe's turn has
            // settled, so a mid-turn message can't decide the title early.
            let evs = self
                .storage
                .read_transcript_tail(&probe_id, 40)
                .unwrap_or_default();
            for te in evs.iter().rev() {
                if let SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text,
                } = &te.event
                {
                    let t = construct_protocol::sanitize_auto_title(text);
                    if !t.is_empty() {
                        title = Some(t);
                    }
                    break;
                }
            }
            let settled = matches!(
                probe_state,
                SessionState::AwaitingInput | SessionState::Done | SessionState::Errored
            );
            if settled && title.is_some() {
                break;
            }
            // Terminal with nothing extracted (this tick already re-read
            // the transcript): the probe died without a usable reply.
            if matches!(probe_state, SessionState::Done | SessionState::Errored) {
                break;
            }
        }
        if let Err(e) = self.delete(&probe_id).await {
            tracing::debug!(%probe_id, error = %e, "title probe: delete failed");
        }
        let Some(title) = title else {
            tracing::debug!(session = %entry.id, %harness, "title probe: no usable reply");
            return;
        };
        apply_auto_title(
            &entry,
            title,
            replace_pending_title,
            &self.storage,
            &self.broadcast,
        )
        .await;
    }

    /// Record a user prompt into the global prompt history (spec 0155).
    /// Called from `handle_event` for every User message, so it catches
    /// the create() prompt-as-event, `session.input`, and adapters that
    /// re-emit typed prompts alike. Only user-kind sessions count —
    /// minibuffer observations, subagent briefs, and probe prompts are
    /// machine-written, not the user's voice. [`crate::storage::is_user_prompt_for_history`]
    /// also drops slash commands and `OBSERVATION:` pseudo-user messages
    /// (background tool completion, ambient ticks, widget actions).
    pub(crate) async fn record_prompt_history(&self, entry: &Arc<SessionEntry>, text: &str) {
        if !crate::storage::is_user_prompt_for_history(text) {
            return;
        }
        let trimmed = text.trim();
        let harness = {
            let s = entry.summary.read().await;
            if s.kind != construct_protocol::SessionKind::User {
                return;
            }
            s.harness.clone()
        };
        if let Err(e) = self
            .storage
            .record_prompt(construct_protocol::PromptHistoryEntry {
                text: trimmed.to_string(),
                at_ms: Utc::now().timestamp_millis(),
                session_id: Some(entry.id.clone()),
                harness: Some(harness),
            })
        {
            tracing::debug!(session = %entry.id, error = %e, "prompt history record failed");
        }
    }

    /// `prompt_history.list`: the retained global prompt history,
    /// newest first (spec 0155).
    pub fn prompt_history(
        &self,
        limit: Option<usize>,
    ) -> Vec<construct_protocol::PromptHistoryEntry> {
        self.storage
            .read_prompt_history(limit.unwrap_or(crate::storage::PROMPT_HISTORY_CAP))
    }

    /// `session.suggest` (spec 0109): on-demand next-prompt suggestion
    /// generation, kicked off when the user opens the suggestion orb —
    /// never automatically. Returns whether generation started. The hand
    /// arrives later as a broadcast `SessionEvent::Suggestions`; every
    /// failure past this point is silent — suggestions are best-effort.
    ///
    /// Generation uses the target session's own harness: smith sessions
    /// run the cheap `--suggest-mode` process one-shot; every other
    /// harness gets a hidden probe session of the same harness (so it
    /// uses the same credentials/subscription the user's session does).
    pub async fn request_suggestions(
        self: &Arc<Self>,
        id: &str,
        keywords: Option<String>,
    ) -> Result<bool> {
        if !self.config().suggest.enabled {
            return Ok(false);
        }
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let harness = {
            let s = entry.summary.read().await;
            if s.kind != construct_protocol::SessionKind::User {
                return Ok(false);
            }
            if s.state != SessionState::AwaitingInput {
                return Ok(false);
            }
            s.harness.clone()
        };
        // Claim this generation slot; a new turn starting (which bumps
        // the counter in `handle_event`) makes this run's result stale.
        let my_gen = entry.suggest_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let events = match self
            .storage
            .read_transcript_tail(&entry.id, SUGGEST_CONTEXT_EVENTS)
        {
            Ok(evs) if !evs.is_empty() => evs,
            _ => return Ok(false),
        };
        let context = render_suggest_context(&events);
        if context.trim().is_empty() {
            return Ok(false);
        }
        // Inject the user's recent prompts across all sessions (spec
        // 0155) so the generator can mirror their real voice and
        // recurring workflows, not just this session's tail.
        let history_block =
            render_suggest_history(&self.storage.read_prompt_history(SUGGEST_HISTORY_PROMPTS));
        let keyword_block = render_suggest_keywords(keywords.as_deref());
        let broadcast = self.broadcast.clone();
        // Smith generates through its own cheap process one-shot. Shell has
        // no model at all — "same harness" is impossible — so it borrows the
        // smith one-shot too rather than spawning a probe that can never
        // answer.
        if harness == "smith" || harness == "shell" {
            if !crate::availability::probe_smith(&self.availability_cache)
                .await
                .available
            {
                // No model to borrow — record the visible skip (spec 0151)
                // instead of letting the one-shot fail silently downstream.
                self.note_ambient_degradation("suggestions").await;
                return Ok(false);
            }
            let Some(smith_adapter) = self.config().adapters.get("smith").cloned() else {
                return Ok(false);
            };
            let binary_spec = smith_adapter
                .binary
                .clone()
                .unwrap_or_else(|| "construct".to_string());
            let prefix_args = smith_adapter.args.clone();
            let Some(binary) = locate_binary(&binary_spec) else {
                return Ok(false);
            };
            let mut context = context;
            if !history_block.is_empty() {
                context.push_str("\n\n");
                context.push_str(&history_block);
            }
            if !keyword_block.is_empty() {
                context.push_str("\n\n");
                context.push_str(&keyword_block);
            }
            tokio::spawn(async move {
                generate_suggestions(binary, prefix_args, entry, my_gen, context, broadcast).await;
            });
        } else {
            let mgr = self.clone();
            tokio::spawn(async move {
                mgr.generate_suggestions_via_probe(
                    entry,
                    my_gen,
                    context,
                    history_block,
                    keyword_block,
                    broadcast,
                )
                .await;
            });
        }
        Ok(true)
    }

    /// Same-harness suggestion generation (spec 0109): spawn a hidden
    /// probe session running the target session's harness, wait for the
    /// model's reply to land in the probe's structured transcript, parse
    /// the hand, tear the probe down, and broadcast — unless a newer
    /// turn made this run stale. The probe reuses the target's cwd so
    /// harness-side trust/config resolution matches the user's session.
    ///
    /// For harnesses that fork natively (claude/codex/…), the probe is
    /// created as a hidden *fork* of the target session: the harness
    /// resumes the same native conversation, so the provider's prompt
    /// cache already covers the entire history (the suggestion request
    /// is one appended message, near-free on input tokens) and the model
    /// predicts from full context instead of a rendered tail. Other
    /// harnesses get a fresh probe fed the rendered transcript tail.
    async fn generate_suggestions_via_probe(
        self: Arc<Self>,
        entry: Arc<SessionEntry>,
        my_gen: u64,
        context: String,
        history_block: String,
        keyword_block: String,
        broadcast: tokio::sync::broadcast::Sender<BroadcastMsg>,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        let (harness, cwd, model, forked_from) = {
            let s = entry.summary.read().await;
            let native_fork = matches!(
                s.harness.as_str(),
                "claude" | "codex" | "opencode" | "grok" | "pi" | "prime-agent"
            ) && s.has_pty
                && s.mode.as_deref() != Some("headless");
            let forked_from = native_fork.then(|| construct_protocol::ForkedFrom {
                session_id: s.id.clone(),
                transcript_seq: entry.transcript_count.load(Ordering::Relaxed),
                at_ms: now_ms,
                parent_busy_ms: s.busy_ms_at(now_ms),
                parent_message_count: s.message_count,
                parent_tokens: s.tokens,
                is_reset_snapshot: false,
            });
            (
                s.harness.clone(),
                s.cwd.clone(),
                s.model.clone(),
                forked_from,
            )
        };
        let mut prompt = String::from(construct_protocol::SuggestionHand::PROMPT_INSTRUCTIONS);
        if forked_from.is_some() {
            // The forked native conversation already carries the history.
            prompt.push_str("\n\n(The transcript is this conversation itself.)");
        } else {
            prompt.push_str("\n\nTranscript tail:\n\n");
            prompt.push_str(&context);
        }
        if !history_block.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&history_block);
        }
        if !keyword_block.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&keyword_block);
        }
        prompt.push_str("\n\nJSON:");
        let create_params = construct_protocol::CreateSessionParams {
            harness: harness.clone(),
            cwd,
            prompt: Some(prompt),
            model,
            title: Some("suggestion probe".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(construct_protocol::PtySize {
                cols: 100,
                rows: 40,
            }),
            worktree: false,
            env: HashMap::new(),
            args: Vec::new(),
            kind: construct_protocol::SessionKind::UsageProbe,
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
            forked_from,
        };
        let probe_id = match self.create(create_params).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!(%harness, error = %e, "suggestion probe: create failed");
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + SUGGEST_PROBE_TIMEOUT;
        let mut hand: Option<construct_protocol::SuggestionHand> = None;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(700)).await;
            // Stale already? Stop early and free the probe.
            if entry.suggest_gen.load(Ordering::SeqCst) != my_gen || entry.is_deleted() {
                break;
            }
            let evs = match self.storage.read_transcript_tail(&probe_id, 40) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for te in evs.iter().rev() {
                if let SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text,
                } = &te.event
                {
                    if let Ok(h) = construct_protocol::SuggestionHand::parse_loose(text) {
                        hand = Some(h);
                        break;
                    }
                }
            }
            if hand.is_some() {
                break;
            }
            // Probe reached a terminal state without a parseable reply —
            // one full transcript pass already ran above, so give up.
            let probe_state = match self.get_entry(&probe_id).await {
                Some(p) => p.summary.read().await.state,
                None => break,
            };
            if matches!(probe_state, SessionState::Done | SessionState::Errored) {
                break;
            }
        }
        if let Err(e) = self.delete(&probe_id).await {
            tracing::debug!(%probe_id, error = %e, "suggestion probe: delete failed");
        }
        let Some(hand) = hand else {
            tracing::debug!(session = %entry.id, %harness, "suggestion probe: no usable reply");
            return;
        };
        if entry.suggest_gen.load(Ordering::SeqCst) != my_gen || entry.is_deleted() {
            return;
        }
        {
            let s = entry.summary.read().await;
            if s.state != SessionState::AwaitingInput {
                return;
            }
        }
        let seq = entry.transcript_count.load(Ordering::Relaxed);
        let _ = broadcast.send(BroadcastMsg::Event(EventNotificationPayload {
            session_id: entry.id.clone(),
            at: Utc::now(),
            event: SessionEvent::Suggestions(hand),
            seq,
        }));
        tracing::info!(session = %entry.id, %harness, "suggestion hand broadcast (probe)");
    }

    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let params = serde_json::to_value(&construct_protocol::SessionIdParams {
            session_id: id.to_string(),
        })?;
        adapter
            .request(ahp_method::SESSION_INTERRUPT, params)
            .await?;
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let params = serde_json::to_value(&construct_protocol::SessionIdParams {
            session_id: id.to_string(),
        })?;
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            adapter.request(ahp_method::SESSION_STOP, params),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
        Ok(())
    }

    /// Collect the ids of direct child subagent sessions parented under
    /// `parent_id`. Used by [`archive`](Self::archive) and
    /// [`delete`](Self::delete) to cascade onto a session's subagents instead
    /// of leaving them as orphaned rows once their owner is gone.
    async fn child_subagent_ids(&self, parent_id: &str) -> Vec<String> {
        let sessions = self.sessions.read().await;
        let mut ids = Vec::new();
        for (sid, entry) in sessions.iter() {
            let s = entry.summary.read().await;
            if s.parent_session_id.as_deref() == Some(parent_id)
                && matches!(s.kind, construct_protocol::SessionKind::Subagent)
            {
                ids.push(sid.clone());
            }
        }
        ids
    }

    async fn archive_native_mirror(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {id}"))?;
        let children = self.child_subagent_ids(id).await;
        let snapshot = {
            let mut summary = entry.summary.write().await;
            summary.archived = true;
            if !summary.state.is_terminal() {
                summary.state = SessionState::Done;
            }
            summary.pending_input = false;
            summary.clone()
        };
        entry.archived.store(true, Ordering::SeqCst);
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        for child in children {
            Box::pin(self.archive_native_mirror(&child)).await?;
        }
        Ok(())
    }

    async fn delete_native_mirror(&self, id: &str) -> Result<()> {
        let children = self.child_subagent_ids(id).await;
        for child in children {
            Box::pin(self.delete_native_mirror(&child)).await?;
        }
        let entry = self
            .sessions
            .write()
            .await
            .remove(id)
            .ok_or_else(|| anyhow!("session not found: {id}"))?;
        entry.deleted.store(true, Ordering::SeqCst);
        self.storage.remove_session(id)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::Deleted(DeletedNotificationPayload {
                session_id: id.to_string(),
            }));
        Ok(())
    }

    /// Archive a session: terminate its adapter (if any) but keep the
    /// transcript, worktree, and start params on disk so it can be restarted
    /// later. The session is marked `archived` (hidden from the list by
    /// default and skipped by startup auto-resume) and persisted. Archiving an
    /// already-terminal session just sets the flag. Reversed by `restart`,
    /// which clears `archived` and brings the session back to the active list.
    ///
    /// Cascades onto the session's subagents: archiving an owner archives the
    /// child subagents it spawned (recursively), so they don't linger as
    /// orphaned rows once their parent is gone.
    pub async fn archive(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        if entry.summary.read().await.native_subagent.is_some() {
            // Archiving is a local visibility/history operation, not control
            // of the harness-owned child. Keep resume/interrupt restrictions
            // for native mirrors, but allow users to hide one explicitly.
            return self.archive_native_mirror(id).await;
        }
        // A closing Run fork settles its dispatched blocks' shimmer (spec
        // 0076) — this covers the fork's own self-archive, the auto-close
        // path, and a manual archive alike. No-op for untracked sessions.
        self.settle_run_fork_dispatch(id).await;
        // Snapshot the child subagents before we start mutating state so a
        // concurrently-finishing subagent can't slip out of the cascade.
        let child_subagents = self.child_subagent_ids(id).await;
        // Record the archive intent before anything that can yield. Terminating
        // the adapter (below) makes its `drain_adapter` Closed handler fire on a
        // separate task and race this bookkeeping; that handler reads this flag
        // and keeps the session archived, so the close can never downgrade the
        // row back to a plain stopped session ("archive needs a second press").
        entry.archived.store(true, Ordering::SeqCst);
        // Flip + persist + broadcast the archived/terminal state up front,
        // before terminating the adapter. The UI reflects the archive on the
        // first action instead of waiting out the graceful-stop timeout, and the
        // summary the Closed handler later observes already reads as archived.
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.archived = true;
            // A live session we just stopped should read as cleanly terminated,
            // not mid-run; leave an already-terminal state (Done/Errored) as-is.
            if !s.state.is_terminal() {
                set_state_tracked(
                    &mut s,
                    SessionState::Done,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            s.pending_input = false;
            s.clone()
        };
        // Archiving is a terminal lifecycle transition. Clear this session's
        // own managed run and settle pending blocks which delegate to it before
        // the adapter's eventual Closed event races in (spec 0042).
        self.note_session_state_for_playbook_run(id, snapshot.state);
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        // Gracefully terminate the live adapter, if there is one. The adapter's
        // Closed event clears `entry.adapter` so a later restart sees no live
        // adapter. Tolerate sessions that are already terminal (no adapter).
        //
        // IMPORTANT: spawn the potentially-slow stop/shutdown so the caller
        // (TUI / CLI) returns immediately after the state flip + broadcast.
        // This prevents archive from hanging the UI for up to ~13s.
        if let Some(adapter) = entry.adapter.lock().await.take() {
            let params = serde_json::to_value(&construct_protocol::SessionIdParams {
                session_id: id.to_string(),
            })?;
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    adapter.request(ahp_method::SESSION_STOP, params),
                )
                .await;
                let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
            });
        }
        // Cascade onto subagents. Recurses through each child's own `archive`,
        // so nested subagents archive too. A failure on one child is logged but
        // never aborts the rest or the parent archive.
        for sid in &child_subagents {
            let native = if let Some(entry) = self.get_entry(sid).await {
                entry.summary.read().await.native_subagent.is_some()
            } else {
                false
            };
            let result = if native {
                Box::pin(self.archive_native_mirror(sid)).await
            } else {
                Box::pin(self.archive(sid)).await
            };
            if let Err(e) = result {
                tracing::warn!(
                    parent = %id,
                    subagent = %sid,
                    error = %e,
                    "subagent cascade-archive failed",
                );
            }
        }
        Ok(())
    }

    /// Persist the terminal outcome of a fork. Archiving remains a
    /// separate primitive so callers can inject a result before retiring it.
    pub async fn merge(&self, id: &str, mode: construct_protocol::ForkMergeMode) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown session: {id}"))?;
        let parent_id = entry
            .summary
            .read()
            .await
            .forked_from
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("session is not a fork"))?
            .session_id
            .clone();
        // The parent's CURRENT `event_count` at the moment of the merge is
        // exactly where this fork's result (or discard) lands on the
        // parent's own timeline — same counter scale as
        // `ForkedFrom::transcript_seq` (see the doc comment on
        // `ForkMerge::merged_seq`), so lineage rendering can later carve
        // that timeline into segments without an extra fetch. A parent that
        // has since been deleted has no timeline left to mark; fall back to
        // 0 rather than failing the merge over it.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (merged_seq, merged_busy_ms, merged_message_count, merged_tokens) =
            match self.get_entry(&parent_id).await {
                Some(parent) => {
                    let p = parent.summary().await;
                    (
                        p.event_count,
                        p.busy_ms_at(now_ms),
                        p.message_count,
                        p.tokens,
                    )
                }
                None => (0, 0, 0, construct_protocol::TokenTally::default()),
            };
        let snapshot = {
            let mut summary = entry.summary.write().await;
            summary.merge = Some(construct_protocol::ForkMerge {
                mode,
                at_ms: now_ms,
                merged_seq,
                merged_busy_ms,
                merged_message_count,
                merged_tokens,
            });
            summary.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Delete a session entirely: kill the adapter if still alive, remove the
    /// worktree (best effort), drop the on-disk record, evict from the live
    /// map, and broadcast a `session/deleted` notification.
    ///
    /// Cascades onto the session's subagents: deleting an owner deletes the
    /// child subagents it spawned (recursively), so they don't linger as
    /// orphaned rows once their parent is gone.
    pub async fn delete(&self, id: &str) -> Result<()> {
        if let Some(entry) = self.get_entry(id).await {
            if entry.summary.read().await.native_subagent.is_some() {
                return Err(anyhow!(
                    "native harness subagents are read-only; manage them through their parent harness"
                ));
            }
        }
        // A deleted Run fork settles its dispatched blocks' shimmer just
        // like an archived one (spec 0137); consuming the tracking entry
        // here also makes an archive-then-delete sequence settle once.
        self.settle_run_fork_dispatch(id).await;
        // Deletion may remove the session before its adapter can publish a
        // terminal event. Apply the same owner/worker orphan cleanup eagerly.
        self.note_session_state_for_playbook_run(id, SessionState::Done);
        // Release the session's routing credential so a deleted session's
        // token can never be reused to reach a route.
        self.router.detach_session(id);
        // Pull out the entry so the in-memory map releases the Arc; the
        // entry itself stays alive via our local Arc until the function ends.
        let entry = {
            let mut map = self.sessions.write().await;
            map.remove(id)
                .ok_or_else(|| anyhow!("session not found: {}", id))?
        };

        // Snapshot child subagents now (the parent is out of the map, the
        // children are still in it) so a deleted owner takes its subagents with
        // it instead of leaving them orphaned in the list.
        let child_subagents = self.child_subagent_ids(id).await;

        // Tell the drain task and event handler not to write storage anymore
        // before we tear the adapter down (killing the adapter triggers a
        // Closed event that the drain task would otherwise persist).
        entry.deleted.store(true, Ordering::SeqCst);

        // Kill the adapter if it's still running.
        if let Some(adapter) = entry.adapter.lock().await.take() {
            adapter.kill();
        }

        // Broadcast deletion immediately so clients (TUI) see the effect
        // without waiting for slow fs/storage work.
        let _ = self
            .broadcast
            .send(BroadcastMsg::Deleted(DeletedNotificationPayload {
                session_id: id.to_string(),
            }));

        // Empty any pane that was showing it, so every client sees the same
        // repaired tree instead of each deciding for itself what a pane
        // pointing at a deleted session should do.
        self.clear_layout_session(id);

        // The rest (sleep, worktree remove, storage, loops, mcp file) can be
        // slow. Spawn it so delete() returns promptly and does not hang the TUI.
        // Best-effort cleanup; errors are only logged.
        let storage = self.storage.clone();
        let loops = self.loops.clone();
        let id_owned = id.to_string();
        let summary = entry.summary.read().await.clone();
        tokio::spawn(async move {
            // Give the drain task a moment to observe the Closed event.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Remove the worktree if there is one. Best effort.
            if let Some(wt) = summary.worktree.as_deref() {
                let wt_path = PathBuf::from(wt);
                if let Err(e) = worktree::remove_worktree(&wt_path).await {
                    tracing::warn!(%id_owned, error = %e, "remove_worktree failed");
                }
            }

            // Loops are attached to the session — drop them from the
            // in-memory registry.
            loops.drop_session(&id_owned).await;

            // Drop the on-disk record. Best effort.
            if let Err(e) = storage.remove_session(&id_owned) {
                tracing::warn!(%id_owned, error = ?e, "remove_session failed");
            }

            // Best-effort: remove the per-session MCP config.
            let mcp_path = construct_protocol::paths::Paths::discover()
                .state_dir
                .join("mcp")
                .join(format!("{}.json", id_owned));
            if mcp_path.exists() {
                let _ = std::fs::remove_file(&mcp_path);
            }
        });

        // Cascade onto subagents. Recurses through each child's own `delete`,
        // so nested subagents are torn down too. A failure on one child is
        // logged but never aborts the rest or the parent delete.
        for sid in &child_subagents {
            let native = if let Some(entry) = self.get_entry(sid).await {
                entry.summary.read().await.native_subagent.is_some()
            } else {
                false
            };
            let result = if native {
                Box::pin(self.delete_native_mirror(sid)).await
            } else {
                Box::pin(self.delete(sid)).await
            };
            if let Err(e) = result {
                tracing::warn!(
                    parent = %id,
                    subagent = %sid,
                    error = %e,
                    "subagent cascade-delete failed",
                );
            }
        }

        Ok(())
    }

    /// Move a session by one slot in the list view.
    ///
    /// A forked session renders nested under its fork parent (see
    /// `forked_from`), independent of `group_id`. Reordering it swaps
    /// position with a sibling fork of the same immediate parent instead —
    /// see the early return below. Everything past that handles ordinary
    /// top-level sessions:
    ///
    /// Within a single region (ungrouped or one group), this swaps positions
    /// with the neighbor. At a region boundary, the session either *enters*
    /// the adjacent group or *exits* its current group:
    ///
    /// - Move-down past the bottom of a region → enter the next region as
    ///   its first child (top of next group).
    /// - Move-up past the top of a region → enter the previous region as
    ///   its last child (bottom of previous group, or end of ungrouped).
    ///
    /// Collapsed groups are skipped at boundaries: their members are hidden,
    /// so the session jumps the whole project in one step instead of swapping
    /// with each hidden member. When *every* region below is collapsed the
    /// skip has nowhere to land, so move-down enters the nearest one and
    /// expands it rather than refusing — otherwise the last session of the
    /// first project could never move down at all in the common steady state
    /// of one expanded project and the rest collapsed.
    ///
    /// No-op at the absolute top (ungrouped session #0) or bottom (last
    /// member of last group).
    ///
    /// Returns whether a move actually happened, so callers can tell a real
    /// reorder apart from hitting a boundary — both look identical from the
    /// `Ok(())` return alone otherwise, and the boundary case is easy to
    /// mistake for a bug (e.g. a fork stuck at the edge of its sibling
    /// forks, which renders at the same indent as its parent).
    pub async fn move_session(&self, id: &str, dir: MoveDirection) -> Result<bool> {
        // Routed sessions are matched by title against the operators defined
        // on disk — the same source every list client renders from. Loaded
        // here (not cached) so a definition added or removed since the last
        // reorder is already in force.
        let operator_names = crate::operator::known_operator_names(
            &construct_protocol::paths::Paths::discover().operators_dir(),
        );
        self.move_session_with_operator_names(id, dir, &operator_names)
            .await
    }

    /// [`Self::move_session`] with the defined operator names injected, so
    /// tests can exercise routed-session reordering without touching the
    /// process-global `CONSTRUCT_CONFIG_DIR` discovery.
    pub(crate) async fn move_session_with_operator_names(
        &self,
        id: &str,
        dir: MoveDirection,
        operator_names: &[String],
    ) -> Result<bool> {
        let all_sessions: Vec<SessionSummary> = self.list().await;
        let all_groups: Vec<GroupSummary> = self.list_groups().await;
        let me = all_sessions
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("session not found: {}", id))?;

        if let Some(parent_id) = me.forked_from.as_ref().map(|f| f.session_id.clone()) {
            // A fork shares `group_id` with its source session (and thus the
            // flat reorder region below), but the TUI never places it there —
            // it's rendered nested under its parent, ordered only among
            // sibling forks of that same parent. Swapping `position` with the
            // flat-region neighbor (typically the parent itself) doesn't
            // change the tree layout at all, so the row appears stuck. Scope
            // the region to siblings instead: other forks whose
            // `forked_from.session_id` matches this one's, in the same
            // archive partition, sorted the same way the TUI sorts them.
            let siblings: Vec<&SessionSummary> = all_sessions
                .iter()
                .filter(|s| {
                    s.forked_from.as_ref().map(|f| f.session_id.as_str())
                        == Some(parent_id.as_str())
                        && s.archived == me.archived
                })
                .collect();
            let mut siblings = siblings;
            siblings.sort_by(|a, b| {
                a.position
                    .cmp(&b.position)
                    .then_with(|| b.created_at.cmp(&a.created_at))
            });
            let pos_in_siblings = siblings.iter().position(|s| s.id == id).unwrap();
            return match dir {
                MoveDirection::Up if pos_in_siblings > 0 => {
                    let other = siblings[pos_in_siblings - 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    Ok(true)
                }
                MoveDirection::Down if pos_in_siblings + 1 < siblings.len() => {
                    let other = siblings[pos_in_siblings + 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    Ok(true)
                }
                // At the edge of the sibling-fork cluster: no-op. Forks don't
                // cross into neighboring groups/regions the way top-level
                // sessions do — their nesting is fixed to `forked_from` for
                // the session's lifetime.
                _ => Ok(false),
            };
        }

        if let Some(operator) = routed_operator(&me, operator_names) {
            // A routed session keeps whatever `group_id` it has, but clients
            // never render it in that flat region — it nests under its
            // operator's row, ordered only among sessions routed to the same
            // operator. Same shape as the fork case above: swapping with the
            // flat-region neighbor wouldn't move the row the user sees, so
            // scope the region to routed siblings instead, sorted the way
            // the clients sort them. The archive partition still applies:
            // operator rows list active sessions only.
            let mut siblings: Vec<&SessionSummary> = all_sessions
                .iter()
                .filter(|s| {
                    routed_operator(s, operator_names) == Some(operator)
                        && is_user_session_kind(s)
                        && s.archived == me.archived
                        && s.forked_from.is_none()
                })
                .collect();
            siblings.sort_by(|a, b| {
                a.position
                    .cmp(&b.position)
                    .then_with(|| b.created_at.cmp(&a.created_at))
            });
            let pos_in_siblings = siblings.iter().position(|s| s.id == id).unwrap();
            return match dir {
                MoveDirection::Up if pos_in_siblings > 0 => {
                    let other = siblings[pos_in_siblings - 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    Ok(true)
                }
                MoveDirection::Down if pos_in_siblings + 1 < siblings.len() => {
                    let other = siblings[pos_in_siblings + 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    Ok(true)
                }
                // At the edge of the routed cluster: no-op. Routing follows
                // the title, not `position`, so a routed session can't leave
                // its operator's row by reordering.
                _ => Ok(false),
            };
        }

        // Find neighbors in `me`'s visible reorder region (same group_id,
        // user sessions and archive partition only), sorted by position. The
        // daemon list includes hidden minibuffer/subagent records so clients
        // can render them in specialized places, but the TUI's session list
        // filters those out. It also puts archived sessions behind a disclosure
        // row. If reordering considers either kind of hidden record, a visible
        // row can appear stuck because it swaps with a row the user cannot see.
        //
        // Keep archived sessions in their own partition as well: when their
        // disclosure row is expanded, they can still be reordered among
        // themselves without perturbing the active rows above it.
        //
        // Forks are hidden from the flat list too: they're user-kind and share
        // their parent's group_id, but the TUI renders them nested under their
        // fork parent, never at their own flat position. Fork placement puts
        // them right after their parent by position, so without this filter a
        // session below a fork-parent needs 1 + #forks presses to cross it —
        // every press before the last swaps with an invisible fork row.
        //
        // Operator-routed sessions are hidden from the flat list for the same
        // reason — they nest under their operator's row — and their fresh
        // creation positions typically interleave with everyone else's, so
        // without this filter a reorder near them burns one silent press per
        // routed session before anything visible moves.
        let region: Vec<&SessionSummary> = all_sessions
            .iter()
            .filter(|s| {
                s.group_id == me.group_id
                    && is_user_session_kind(s)
                    && s.archived == me.archived
                    && s.forked_from.is_none()
                    && routed_operator(s, operator_names).is_none()
            })
            .collect();
        let pos_in_region = region.iter().position(|s| s.id == id).unwrap();

        match dir {
            MoveDirection::Up => {
                if pos_in_region > 0 {
                    // Same-region swap.
                    let other = region[pos_in_region - 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    return Ok(true);
                }
                // At top of region — try to exit into the previous region,
                // skipping collapsed projects.
                let prev =
                    groups::region_above_skipping_collapsed(me.group_id.as_deref(), &all_groups);
                let Some(prev_region) = prev else {
                    return Ok(false);
                };
                self.move_session_into_region(
                    &me.id,
                    &prev_region,
                    RegionEdge::Bottom,
                    &all_sessions,
                )
                .await?;
                Ok(true)
            }
            MoveDirection::Down => {
                if pos_in_region + 1 < region.len() {
                    let other = region[pos_in_region + 1];
                    self.swap_session_positions(&me.id, &other.id).await?;
                    return Ok(true);
                }
                // At bottom of region — try to enter the next region,
                // skipping collapsed projects.
                let next =
                    groups::region_below_skipping_collapsed(me.group_id.as_deref(), &all_groups);
                let next_region = match next {
                    Some(region) => region,
                    None => {
                        // Every region below is a collapsed project, so the
                        // skip has nowhere to land. Refusing here makes the key
                        // look broken rather than bounded: the steady state of
                        // a working fleet is one expanded project with the rest
                        // collapsed, which leaves the last session of the first
                        // project unable to move down *at all* even though
                        // project rows sit plainly below it. Land in the
                        // nearest one instead and expand it, so the session
                        // stays visible where the user dropped it — the
                        // rendered list keeps matching the reorder model.
                        //
                        // Move-up needs no counterpart: the ungrouped region
                        // always renders above every project and can never be
                        // collapsed, so the upward skip always has somewhere to
                        // land. That asymmetry is why moving up kept working
                        // here while moving down did nothing.
                        let Some(collapsed_region) =
                            groups::region_below(me.group_id.as_deref(), &all_groups)
                        else {
                            return Ok(false);
                        };
                        if let Some(gid) = collapsed_region.as_deref() {
                            self.set_group_collapsed(gid, false).await?;
                        }
                        collapsed_region
                    }
                };
                self.move_session_into_region(&me.id, &next_region, RegionEdge::Top, &all_sessions)
                    .await?;
                Ok(true)
            }
        }
    }

    async fn swap_session_positions(&self, a_id: &str, b_id: &str) -> Result<()> {
        let entry_a = self
            .get_entry(a_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", a_id))?;
        let entry_b = self
            .get_entry(b_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", b_id))?;
        let a_pos = entry_a.summary.read().await.position;
        let b_pos = entry_b.summary.read().await.position;
        let snap_a = {
            let mut s = entry_a.summary.write().await;
            s.position = b_pos;
            s.clone()
        };
        let snap_b = {
            let mut s = entry_b.summary.write().await;
            s.position = a_pos;
            s.clone()
        };
        self.storage.save_summary(&snap_a)?;
        self.storage.save_summary(&snap_b)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snap_a,
            }));
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snap_b,
            }));
        Ok(())
    }

    pub async fn set_title(&self, id: &str, title: Option<String>) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Normalize: trim, treat empty as None.
        let normalized = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        // Every explicit update, including clearing the title, opts a fork out
        // of pending first-prompt auto-title generation.
        entry.title_gen_attempted.store(true, Ordering::SeqCst);
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.title = normalized;
            s.auto_title_pending = false;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.pinned = pinned;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Clear a session's `needs_attention` marker and record it as the
    /// currently-focused session, so a concurrent non-`Running` transition
    /// won't immediately re-raise the marker for the session being viewed.
    pub async fn mark_seen(&self, id: &str) -> Result<()> {
        {
            let mut focused = self.focused_sessions.lock().unwrap();
            focused.clear();
            focused.insert(id.to_string());
        }
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Viewing the session also consumes any unseen activity.
        entry.unseen_activity.store(false, Ordering::Relaxed);
        let snapshot = {
            let mut s = entry.summary.write().await;
            if !s.needs_attention {
                // Already clear — focus is recorded above; skip the churn.
                return Ok(());
            }
            s.needs_attention = false;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Update the set of visible/focused sessions, consuming their unseen activity
    /// and clearing their needs_attention markers.
    pub async fn set_focused_sessions(&self, ids: &[String]) -> Result<()> {
        {
            let mut focused = self.focused_sessions.lock().unwrap();
            focused.clear();
            for id in ids {
                focused.insert(id.clone());
            }
        }
        for id in ids {
            if let Some(entry) = self.get_entry(id).await {
                // Viewing the session also consumes any unseen activity.
                entry.unseen_activity.store(false, Ordering::Relaxed);
                let snapshot = {
                    let mut s = entry.summary.write().await;
                    if s.needs_attention {
                        s.needs_attention = false;
                        Some(s.clone())
                    } else {
                        None
                    }
                };
                if let Some(snapshot) = snapshot {
                    self.storage.save_summary(&snapshot)?;
                    let _ = self
                        .broadcast
                        .send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                }
            }
        }
        Ok(())
    }

    /// Daemon-side idle detection for interactive full-screen TUI harnesses:
    /// they never emit `AwaitingInput`, so when their PTY has produced no output
    /// for [`PTY_QUIESCENCE`] we synthesize the transition. Shells use
    /// foreground-process-group detection in the adapter and are excluded.
    pub(crate) async fn poll_pty_quiescence(&self) {
        let now_ms = Utc::now().timestamp_millis();
        let threshold = PTY_QUIESCENCE.as_millis() as i64;
        let entries: Vec<Arc<SessionEntry>> =
            self.sessions.read().await.values().cloned().collect();
        for entry in entries {
            let stale = {
                let s = entry.summary.read().await;
                // End a post-respawn settle window once the resume repaint has
                // gone quiet, so later output counts as activity again. See
                // spec 0054 and `SessionEntry::resume_settling_since_ms`.
                let settle_since = entry.resume_settling_since_ms.load(Ordering::Relaxed);
                if settle_since > 0 && resume_settle_over(settle_since, s.last_pty_at_ms, now_ms) {
                    entry.resume_settling_since_ms.store(0, Ordering::Relaxed);
                }
                harness_uses_quiescence(&s)
                    && s.state == SessionState::Running
                    && s.last_pty_at_ms
                        .is_some_and(|last| now_ms.saturating_sub(last) >= threshold)
            };
            if stale {
                self.handle_event(
                    &entry,
                    SessionEvent::Status {
                        state: SessionState::AwaitingInput,
                        detail: None,
                    },
                )
                .await;
            }
        }
    }

    async fn persist_approval_mode(
        &self,
        entry: &Arc<SessionEntry>,
        mode: construct_protocol::ApprovalMode,
    ) -> Result<()> {
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.approval_mode = mode;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Record whether the minibuffer ambient loop is enabled/disabled after a
    /// `/minibuffer enable|disable` command. Persisted so the choice survives
    /// daemon restart — `respawn` re-injects the flag via env.
    async fn persist_minibuffer_loop(&self, entry: &Arc<SessionEntry>, enabled: bool) -> Result<()> {
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.minibuffer_loop_disabled = !enabled;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Register a session with the router, returning the environment its
    /// harness process needs. `None` when this session gets no routing
    /// transport — routing disabled, harness not route-capable, or the
    /// listener never bound. That is a normal outcome, not an error: the
    /// session runs exactly as it would in a build without routing.
    pub(crate) fn attach_router(
        &self,
        session_id: &str,
        harness: &str,
        existing_token: Option<String>,
    ) -> Option<HashMap<String, String>> {
        if !self.router.can_route_harness(harness) {
            return None;
        }
        match self
            .router
            .attach_session(session_id, harness, existing_token)
        {
            Ok(env) => Some(env),
            Err(e) => {
                tracing::warn!(
                    session = %session_id,
                    %harness,
                    error = %format!("{e:#}"),
                    "router attach failed; session runs unrouted"
                );
                None
            }
        }
    }

    /// Re-arm a persisted route after the session's transport is back
    /// (spec 0114: a resumed session comes back on the route it was last
    /// running).
    pub(crate) async fn restore_route(&self, entry: &Arc<SessionEntry>) {
        let (id, harness, route) = {
            let s = entry.summary.read().await;
            (s.id.clone(), s.harness.clone(), s.route.clone())
        };
        let Some(route) = route else { return };
        if let Err(e) = self.router.set_route(
            &id,
            &harness,
            Some(&route.name),
            // A resumed session comes back on the model it was running,
            // not the target's current default (spec 0114).
            Some(route.model.as_str()),
            route.origin_model.clone(),
            route.effort.clone(),
        ) {
            // The route no longer resolves (renamed in config, key gone).
            // Surface it rather than silently pinning a stale endpoint or
            // silently falling back to pass-through.
            tracing::warn!(
                session = %id,
                route = %route.name,
                error = %format!("{e:#}"),
                "stored route could not be restored"
            );
        }
    }

    /// Persist the first proof that a session's armed route is actually
    /// carrying traffic. Until this lands, a route is armed but unproven —
    /// the harness may resolve its endpoint through a channel that ignores
    /// our injection (spec 0115), and reporting it as working would be a
    /// lie the user has no way to check.
    pub(crate) async fn mark_route_observed(&self, session_id: &str) {
        let Some(entry) = self.get_entry(session_id).await else {
            return;
        };
        let snapshot = {
            let mut s = entry.summary.write().await;
            match s.route.as_mut() {
                Some(route) if !route.observed => route.observed = true,
                _ => return,
            }
            s.clone()
        };
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
    }

    /// Arm, change, or clear a session's route (spec 0114 / 0165).
    pub async fn set_route(
        &self,
        session_id: &str,
        route: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        let (harness, origin_model, existing) = {
            let s = entry.summary.read().await;
            (
                s.harness.clone(),
                // The model the harness reports is the origin we display
                // the substitution against. Once a route is armed the
                // harness keeps reporting its own model, so the value is
                // captured on the first arm and carried across changes.
                s.route
                    .as_ref()
                    .and_then(|r| r.origin_model.clone())
                    .or_else(|| s.model.clone()),
                s.route.clone(),
            )
        };
        let armed = self.router.set_route(
            session_id,
            &harness,
            route.as_deref(),
            model.as_deref(),
            origin_model,
            effort,
        )?;
        if armed == existing {
            return Ok(());
        }
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.route = armed;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// Routes offered for a session's picker (spec 0115).
    pub async fn list_routes(
        &self,
        session_id: Option<&str>,
    ) -> Result<construct_protocol::RouterListRoutesResult> {
        // Bring live model listings up to date before building the menu
        // (spec 0209). Fresh cache entries make this free; a cold cache
        // costs one bounded fetch round, never a hang.
        self.router.refresh_discovered_models().await;
        let Some(session_id) = session_id else {
            return Ok(self.router.list_routes("", false, None, false));
        };
        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("unknown session {session_id}"))?;
        let (harness, active) = {
            let s = entry.summary.read().await;
            (s.harness.clone(), s.route.as_ref().map(|r| r.name.clone()))
        };
        Ok(self.router.list_routes(
            &harness,
            self.router.is_attached(session_id),
            active,
            self.router.session_native_catalog(session_id),
        ))
    }

    /// Start a sign-in for a subscription route: a shell session running
    /// the owning CLI's login command (spec 0117 — the owning tool is the
    /// only credential writer; this only reaches for it). A watcher polls
    /// for the credential to land and archives the session the moment it
    /// does — most of these CLIs stay open after sign-in, so the watched
    /// fact is the credential, not the process exiting. A session that
    /// ends without a credential stays visible so its output can explain
    /// what went wrong.
    pub async fn start_login_session(
        self: &Arc<Self>,
        p: construct_protocol::RouterLoginParams,
    ) -> Result<String> {
        let provider = crate::router::oauth::OauthProvider::ALL
            .iter()
            .copied()
            .find(|prov| prov.name() == p.route)
            .ok_or_else(|| anyhow!("no subscription login named \"{}\"", p.route))?;
        let command = provider.login_command();
        let cwd = p
            .cwd
            .filter(|c| !c.trim().is_empty())
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| ".".to_string());
        let id = self
            .create(construct_protocol::CreateSessionParams {
                harness: "shell".into(),
                cwd,
                prompt: Some(command),
                model: None,
                title: Some(format!("{} login", provider.name())),
                mode: None,
                pty_size: p.pty_size,
                worktree: false,
                env: std::collections::HashMap::new(),
                args: Vec::new(),
                kind: construct_protocol::SessionKind::User,
                parent_session_id: None,
                group_id: None,
                position_after_session_id: None,
                forked_from: None,
            })
            .await?;
        let manager = Arc::clone(self);
        let session_id = id.clone();
        tokio::spawn(async move {
            // ~10 minutes of patience; a login the user abandons should
            // not leave a poller running forever.
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let login_ok = crate::router::oauth::check_login(provider).is_ok();
                let state = match manager.get_entry(&session_id).await {
                    Some(entry) => Some(entry.summary.read().await.state),
                    None => None,
                };
                match login_watch_step(login_ok, state) {
                    LoginWatch::Archive => {
                        if let Err(e) = manager.archive(&session_id).await {
                            tracing::warn!(
                                session = %session_id,
                                error = ?e,
                                "archive completed login session failed"
                            );
                        }
                        return;
                    }
                    LoginWatch::Abandon => return,
                    LoginWatch::Continue => {}
                }
            }
        });
        Ok(id)
    }

    /// Record the session's active model after an adapter `/model` switch.
    /// Updates the summary (so the UI label tracks the change) and persists
    /// it, so `respawn` re-injects the model into the adapter's start params
    /// on the next restart instead of reverting to the creation-time value.
    async fn persist_model(&self, entry: &Arc<SessionEntry>, model: String) -> Result<()> {
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.model = Some(model);
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// The native-id file name a harness's adapter reads/writes for same-
    /// harness fork resume (`crates/adapter-{claude,codex,grok}`), or `None`
    /// for harnesses with no native fork primitive (Antigravity, shell,
    /// smith, ...). Shared between ordinary fork spawn (`lifecycle.rs`,
    /// which also needs the paired `CONSTRUCT_*_FORK_FROM` env var name) and
    /// [`SessionManager::synthesize_reset_snapshot`] (which only needs the
    /// filename, to seed an archived snapshot's own native id).
    fn native_id_file_name(harness: &str) -> Option<&'static str> {
        match harness {
            "claude" => Some("claude_session_id.txt"),
            "codex" => Some("codex_session_id.txt"),
            "opencode" => Some("opencode_session_id.txt"),
            "grok" => Some("grok_session_id.txt"),
            "kimi" => Some("kimi_session_id.txt"),
            "hermes" => Some("hermes_session_id.txt"),
            "pi" => Some("pi_session_id.txt"),
            "prime-agent" => Some("prime_agent_session_id.txt"),
            "muse" => Some("muse_session_id.txt"),
            _ => None,
        }
    }

    /// Record a harness-native context reset (`/clear` and equivalents,
    /// spec 0079) detected by an adapter, by synthesizing a real, ordinary
    /// archived child session — "reset is fork-and-archive, plus switching
    /// the live session's own resume id" (spec 0085): the live session
    /// `entry` never changes identity, only its native id file (rewritten
    /// in place by the adapter's own hook, untouched here); this method's
    /// job is the other half — a new session holding a frozen copy of
    /// `entry`'s transcript up to this point, `forked_from` it, born
    /// already archived with its OWN native id file set to the id that's
    /// about to be retired, so forking from it later is the ordinary,
    /// unmodified fork-resume path — no special casing anywhere else.
    /// Same "daemon builds a `SessionSummary`/`SessionEntry` directly, no
    /// adapter, persist + insert + broadcast" shape as native-subagent
    /// projection (`handle_native_subagent_event`).
    async fn synthesize_reset_snapshot(
        &self,
        entry: &Arc<SessionEntry>,
        prior_native_id: String,
    ) -> Result<()> {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let (
            harness,
            cwd,
            title,
            group_id,
            approval_mode,
            event_count,
            busy_ms,
            message_count,
            tokens,
            has_pty,
            mode,
        ) = {
            let s = entry.summary.read().await;
            (
                s.harness.clone(),
                s.cwd.clone(),
                s.title.clone(),
                s.group_id.clone(),
                s.approval_mode,
                s.event_count,
                s.busy_ms_at(now_ms),
                s.message_count,
                s.tokens,
                // A fork of this snapshot should spawn the same way a fork
                // of the live session would (interactive PTY vs headless) —
                // carried from the live session at the moment of reset, not
                // hardcoded, since `Client::fork_session` decides
                // interactive-vs-headless from the SOURCE's own `has_pty`.
                s.has_pty,
                s.mode.clone(),
            )
        };
        let existing = self.storage.read_transcript(&entry.id, 0, None)?;

        let child_id = format!("s{}", uuid::Uuid::new_v4().simple());
        let child_title = Some(match &title {
            Some(t) => format!("(cleared) {t}"),
            None => format!("(cleared) {harness}"),
        });
        let summary = construct_protocol::SessionSummary {
            id: child_id.clone(),
            harness: harness.clone(),
            cwd,
            title: child_title,
            auto_title_pending: false,
            state: construct_protocol::SessionState::Done,
            created_at: now,
            last_event_at: None,
            last_message_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count,
            has_pty,
            mode,
            pinned: false,
            position: -now_ms,
            group_id,
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms,
            busy_running_since_ms: None,
            message_count,
            tokens,
            // The snapshot is the frozen pre-reset conversation; its gauge is
            // recovered from the copied transcript at next load anyway.
            context_used: None,
            context_window: None,
            context_segments: Vec::new(),
            approval_mode,
            kind: construct_protocol::SessionKind::User,
            archived: true,
            minibuffer_loop_disabled: true,
            needs_attention: false,
            forked_from: Some(construct_protocol::ForkedFrom {
                session_id: entry.id.clone(),
                transcript_seq: existing.events.len() as u64,
                at_ms: now_ms,
                parent_busy_ms: busy_ms,
                parent_message_count: message_count,
                parent_tokens: tokens,
                is_reset_snapshot: true,
            }),
            merge: None,
        };
        let created = Arc::new(SessionEntry {
            id: child_id.clone(),
            summary: RwLock::new(summary.clone()),
            transcript_count: AtomicU64::new(existing.events.len() as u64),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(true),
            title_gen_attempted: AtomicBool::new(true),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(0),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });

        self.storage.save_summary(&summary)?;
        if let Some(id_file) = Self::native_id_file_name(&harness) {
            let path = self.storage.session_dir(&child_id).join(id_file);
            if let Err(error) = std::fs::write(&path, &prior_native_id) {
                tracing::warn!(session = %child_id, ?error, "write archived reset-snapshot native id failed");
            }
        }
        for ev in &existing.events {
            if let Err(error) = self.storage.append_event(&child_id, ev) {
                tracing::warn!(session = %child_id, ?error, "copy transcript event into reset snapshot failed");
            }
        }

        self.sessions
            .write()
            .await
            .insert(child_id.clone(), created);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: summary,
            }));
        Ok(())
    }

    async fn persist_effort(&self, entry: &Arc<SessionEntry>, effort: String) -> Result<()> {
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.effort = Some(effort);
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    pub async fn set_approval_mode(
        &self,
        id: &str,
        mode: construct_protocol::ApprovalMode,
    ) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        self.persist_approval_mode(&entry, mode).await?;
        // Forward to the adapter so it picks up the change for the next tool
        // classification. If the adapter is gone (session ended), skip.
        if let Some(adapter) = entry.adapter.lock().await.clone() {
            let params = serde_json::to_value(&construct_protocol::SessionSetApprovalModeParams {
                session_id: id.to_string(),
                mode,
            })?;
            // Best-effort: don't fail the call if the adapter doesn't recognize
            // the method (e.g. claude/codex, which don't gate tools).
            let _ = adapter
                .request(ahp_method::SESSION_SET_APPROVAL_MODE, params)
                .await;
        }
        Ok(())
    }

    pub async fn tool_decision(&self, id: &str, call_id: String, decision: String) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let mode = match decision.as_str() {
            "auto_review" => Some(construct_protocol::ApprovalMode::AutoReview),
            "unsafe_auto" => Some(construct_protocol::ApprovalMode::UnsafeAuto),
            _ => None,
        };
        if let Some(mode) = mode {
            self.persist_approval_mode(&entry, mode).await?;
        }
        let params = serde_json::to_value(&construct_protocol::SessionToolDecisionParams {
            session_id: id.to_string(),
            call_id,
            decision,
        })?;
        adapter
            .request(ahp_method::SESSION_TOOL_DECISION, params)
            .await?;
        Ok(())
    }

    /// Snapshot the per-session task registry (running, backgrounded,
    /// and recent terminal states). Returns an empty list when the
    /// session has no entry — adapters that don't emit `TaskStart`
    /// (claude / codex / shell today) simply never populate it.
    pub async fn loop_create(
        &self,
        params: construct_protocol::LoopCreateParams,
    ) -> Result<construct_protocol::Loop> {
        // Reject on unknown session — the daemon's source of truth
        // for "is this session real" is sessions map.
        if self.get_entry(&params.session_id).await.is_none() {
            return Err(anyhow!("session not found: {}", params.session_id));
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let next = crate::loops::next_fire_after_ms(&params.spec, now_ms);
        let l = construct_protocol::Loop {
            id: String::new(), // assigned in registry
            session_id: params.session_id,
            spec: params.spec,
            prompt: params.prompt,
            created_at_ms: now_ms,
            next_fire_at_ms: next,
            expires_at_ms: params.expires_at_ms,
            last_fired_at_ms: None,
            fire_count: 0,
        };
        self.loops.create(l).await
    }

    pub async fn loop_list(&self, session_id: Option<&str>) -> Vec<construct_protocol::Loop> {
        self.loops.list(session_id).await
    }

    pub async fn loop_update(
        &self,
        params: construct_protocol::LoopUpdateParams,
    ) -> Result<construct_protocol::Loop> {
        self.loops
            .update(
                &params.loop_id,
                params.spec,
                params.prompt,
                params.expires_at_ms,
            )
            .await
    }

    pub async fn loop_remove(&self, loop_id: &str) -> Result<()> {
        self.loops.remove(loop_id).await
    }

    pub async fn list_tasks(&self, id: &str) -> Result<Vec<construct_protocol::TaskInfo>> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let g = entry.tasks.lock().await;
        Ok(g.snapshot())
    }

    /// Forward a client-initiated tool action (`"kill"` / `"background"`)
    /// to the adapter. Adapters that don't know the action ignore it
    /// with a debug log; adapters that don't know the `call_id`
    /// likewise no-op. No daemon-side state changes — the adapter is
    /// authoritative for the running-tasks registry.
    pub async fn tool_action(&self, id: &str, call_id: String, action: String) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let params = serde_json::to_value(&construct_protocol::SessionToolActionParams {
            session_id: id.to_string(),
            call_id,
            action,
        })?;
        adapter
            .request(ahp_method::SESSION_TOOL_ACTION, params)
            .await?;
        Ok(())
    }

    pub async fn kill(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry.adapter.lock().await.clone();
        if let Some(a) = adapter {
            a.kill();
        }
        let mut s = entry.summary.write().await;
        if !s.state.is_terminal() {
            set_state_tracked(
                &mut s,
                SessionState::Errored,
                chrono::Utc::now().timestamp_millis(),
            );
        }
        let snapshot = s.clone();
        drop(s);
        // A force-kill writes the terminal state straight onto the summary
        // instead of going through the event path, so the playbook run has to
        // be told here too. Without this the run only cleared when the dying
        // adapter happened to also emit a terminal event first — a race, and
        // the reason a killed session's playbook kept shimmering most of the
        // time (#1090).
        self.note_session_state_for_playbook_run(id, snapshot.state);
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }
}

fn playbook_cursor_is_visible(
    cursor: &construct_protocol::PlaybookCursor,
    session_id: &str,
    now_ms: i64,
) -> bool {
    let ttl_ms = if cursor.kind == "agent" {
        PLAYBOOK_AGENT_CURSOR_TTL_MS
    } else {
        PLAYBOOK_CURSOR_TTL_MS
    };
    cursor.active
        && cursor.session_id == session_id
        && now_ms.saturating_sub(cursor.updated_at_ms) <= ttl_ms
}

/// True if `text` is a slash command (`/model gpt-5.5`, `/compact`, ...)
/// rather than a message that describes what the session is for.
/// `maybe_spawn_auto_title` uses this to ignore a leading run of slash
/// commands entirely and wait for the first ordinary prompt.
fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

/// Return title-generation input only for a substantive ordinary prompt.
fn auto_title_prompt(text: String) -> Option<String> {
    (!text.trim().is_empty() && !is_slash_command(&text)).then_some(text)
}

/// Instruction preamble for the same-harness title-probe fallback (spec
/// 0151). Kept in the same shape as smith's `--title-mode` system prompt so
/// both generators produce equivalent titles; the reply is further cleaned
/// by `construct_protocol::sanitize_auto_title`.
const AUTO_TITLE_PROBE_INSTRUCTIONS: &str = "Reply with ONLY a 3-5 word title in Title Case \
that summarizes the request below. No quotes, no punctuation, no markdown, no preamble. Do \
not use any tools and do not act on the request itself. The request:";

/// Hard cap on one same-harness title probe's lifetime: adapter spawn plus
/// one short model turn. Past this the probe is torn down and the attempt
/// silently dropped.
const TITLE_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// What one auto-title generator attempt left behind, so a caller holding a
/// spent attempt latch knows whether another generator is still worth
/// running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleOutcome {
    /// Nothing more to do: a title was applied, or the session stopped
    /// wanting one (deleted, or manually renamed while generation ran).
    Settled,
    /// The generator produced nothing usable. Another generator may still
    /// name this session.
    Failed,
}

/// Shell out to `construct-adapter-smith --title-mode "<prompt>"`, capture
/// stdout, and apply the title to the session summary. Best-effort: any
/// failure (smith missing keys, network error, non-zero exit, empty output)
/// returns [`TitleOutcome::Failed`] so the caller can fall back to the
/// same-harness probe instead of leaving the session unnamed.
async fn generate_auto_title(
    binary: PathBuf,
    prefix_args: Vec<String>,
    entry: Arc<SessionEntry>,
    prompt: String,
    replace_pending_title: bool,
    storage: Arc<Storage>,
    broadcast: tokio::sync::broadcast::Sender<BroadcastMsg>,
) -> TitleOutcome {
    use std::process::Stdio;
    let output = tokio::process::Command::new(&binary)
        .args(&prefix_args)
        .arg("--title-mode")
        .arg(&prompt)
        // Same credential floor the adapters get (spec 0180), so a key
        // declared in `[daemon.env]` names sessions as an exported one does.
        .envs(crate::daemon_env::child_env_base())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    let out = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = ?e, "auto-title spawn failed");
            return TitleOutcome::Failed;
        }
    };
    if !out.status.success() {
        tracing::info!(
            session = %entry.id,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "auto-title exit non-zero; falling back",
        );
        return TitleOutcome::Failed;
    }
    let title = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if title.is_empty() {
        return TitleOutcome::Failed;
    }
    // Settled either way: `apply_auto_title` declining means the session no
    // longer wants a generated title (deleted, or renamed mid-flight), which
    // a second generator must not override either.
    apply_auto_title(&entry, title, replace_pending_title, &storage, &broadcast).await;
    TitleOutcome::Settled
}

/// Apply a generated title to the session — if the eligibility that
/// launched generation still holds — then persist and broadcast. Shared by
/// the smith `--title-mode` one-shot and the same-harness probe fallback
/// (spec 0151).
async fn apply_auto_title(
    entry: &Arc<SessionEntry>,
    title: String,
    replace_pending_title: bool,
    storage: &Arc<Storage>,
    broadcast: &tokio::sync::broadcast::Sender<BroadcastMsg>,
) {
    if entry.is_deleted() {
        return;
    }
    let snapshot = {
        let mut s = entry.summary.write().await;
        // Apply only if the same eligibility that launched generation still
        // holds. A manual rename (including clearing the title) clears a fork's
        // pending bit, so an in-flight result cannot clobber that choice.
        let title_is_empty = s.title.as_ref().is_none_or(|t| t.trim().is_empty());
        let still_eligible = if replace_pending_title {
            s.auto_title_pending
        } else {
            !s.auto_title_pending && title_is_empty
        };
        if !still_eligible {
            return;
        }
        s.title = Some(title.clone());
        s.auto_title_pending = false;
        s.clone()
    };
    if let Err(e) = storage.save_summary(&snapshot) {
        tracing::warn!(session = %entry.id, error = ?e, "auto-title save_summary failed");
        return;
    }
    let _ = broadcast.send(BroadcastMsg::State(StateNotificationPayload {
        session: snapshot,
    }));
    tracing::info!(session = %entry.id, %title, "auto-title applied");
}

/// How many transcript-tail events feed suggestion generation (spec 0109).
/// Enough to cover a long agent turn (tool calls + results) plus the user
/// prompts before it; the rendered text is further capped by the
/// suggest-mode process itself.
const SUGGEST_CONTEXT_EVENTS: usize = 80;

/// How many global prompt-history entries feed suggestion generation
/// (spec 0155): enough to show the user's voice and recurring
/// workflows without drowning the transcript tail.
const SUGGEST_HISTORY_PROMPTS: usize = 15;

/// Render recent global prompt-history entries into the labeled block
/// appended to suggestion-generation context (spec 0155). Newlines
/// collapse so each prompt stays one list line; empty history renders
/// to an empty string (the block is simply omitted).
fn render_suggest_history(entries: &[construct_protocol::PromptHistoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("Recent prompts from this user across sessions (most recent first):");
    for e in entries {
        let one_line = e.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let clipped: String = one_line.chars().take(200).collect();
        out.push_str("\n- ");
        out.push_str(&clipped);
        if one_line.chars().count() > 200 {
            out.push('…');
        }
    }
    out
}

/// Render optional regeneration guidance supplied by the user. This is
/// deliberately labeled as preference rather than transcript evidence:
/// it steers which valid next steps the generator emphasizes without
/// authorizing invented state. Whitespace and length are bounded before
/// the text reaches any harness.
fn render_suggest_keywords(keywords: Option<&str>) -> String {
    let Some(keywords) = keywords else {
        return String::new();
    };
    let normalized = keywords.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = normalized.chars().take(200).collect();
    if clipped.is_empty() {
        return String::new();
    }
    format!(
        "User guidance for this regeneration:\n\
         Prioritize relevant suggestions around these keywords: {clipped}\n\
         Treat the keywords as intent guidance, not evidence; do not invent state."
    )
}

/// Hard cap on one same-harness suggestion probe's lifetime: adapter
/// spawn plus a full model turn over a long prompt. Past this the probe
/// is torn down and the attempt silently dropped.
const SUGGEST_PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// Render a transcript tail into the plain-text context the suggest-mode
/// one-shot consumes. Structured events (messages, tool activity, diffs)
/// are preferred; for PTY harnesses whose tail is mostly raw terminal
/// bytes, a cleaned tail of that output is appended so claude/codex/shell
/// sessions still produce usable context.
fn render_suggest_context(events: &[construct_protocol::TimestampedEvent]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut structured_signal = 0usize;
    let mut pty_text = String::new();
    let trunc = |s: &str, max: usize| -> String {
        let t = s.trim();
        if t.chars().count() <= max {
            t.to_string()
        } else {
            let cut: String = t.chars().take(max).collect();
            format!("{cut}…")
        }
    };
    for te in events {
        match &te.event {
            SessionEvent::Message { role, text } => {
                let tag = match role {
                    MessageRole::User => "USER",
                    MessageRole::Assistant => "AGENT",
                    MessageRole::System => "SYSTEM",
                    MessageRole::Tool => "TOOL",
                };
                lines.push(format!("{tag}: {}", trunc(text, 1200)));
                structured_signal += 1;
            }
            SessionEvent::ToolUse { tool, args, .. } => {
                lines.push(format!(
                    "TOOL CALL {tool}: {}",
                    trunc(&args.to_string(), 200)
                ));
                structured_signal += 1;
            }
            SessionEvent::ToolResult {
                tool, ok, output, ..
            } => {
                lines.push(format!(
                    "TOOL RESULT {tool} ({}): {}",
                    if *ok { "ok" } else { "failed" },
                    trunc(output, 300)
                ));
                structured_signal += 1;
            }
            SessionEvent::Diff { patch } => {
                lines.push(format!("DIFF:\n{}", trunc(patch, 1500)));
                structured_signal += 1;
            }
            SessionEvent::Error { message } => {
                lines.push(format!("ERROR: {}", trunc(message, 300)));
            }
            SessionEvent::Pty { .. } => {
                if let Some(bytes) = te.event.pty_bytes() {
                    pty_text.push_str(&strip_terminal_bytes(&bytes));
                }
            }
            _ => {}
        }
    }
    // Raw terminal output is noisy; only lean on it when the structured
    // stream is too thin to describe the turn (PTY-only harnesses).
    if structured_signal < 3 && !pty_text.trim().is_empty() {
        let tail: String = {
            let chars: Vec<char> = pty_text.chars().collect();
            let start = chars.len().saturating_sub(4000);
            chars[start..].iter().collect()
        };
        lines.push(format!(
            "TERMINAL OUTPUT (most recent last):\n{}",
            tail.trim()
        ));
    }
    lines.join("\n")
}

/// Strip ANSI escape sequences and non-printable bytes from raw PTY
/// output, keeping plain text and newlines. Deliberately crude: this
/// feeds a language model, not a terminal emulator — a mangled cursor
/// dance degrades into whitespace, which is fine.
fn strip_terminal_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut iter = bytes.iter().copied().peekable();
    while let Some(b) = iter.next() {
        match b {
            0x1b => {
                // ESC sequence: skip CSI (`ESC [ ... final`), OSC
                // (`ESC ] ... BEL/ST`), and single-char escapes.
                match iter.next() {
                    Some(b'[') => {
                        for nb in iter.by_ref() {
                            if (0x40..=0x7e).contains(&nb) {
                                break;
                            }
                        }
                    }
                    Some(b']') => {
                        let mut prev = 0u8;
                        for nb in iter.by_ref() {
                            if nb == 0x07 || (prev == 0x1b && nb == b'\\') {
                                break;
                            }
                            prev = nb;
                        }
                    }
                    _ => {}
                }
            }
            b'\n' => out.push('\n'),
            b'\r' | 0x07 | 0x08 => {}
            0x20..=0x7e => out.push(b as char),
            0x80..=0xff => {
                // Pass UTF-8 continuation content through best-effort by
                // buffering the byte; invalid sequences degrade to nothing.
                out.push(b as char);
            }
            _ => {}
        }
    }
    // Collapse runs of blank lines the stripping leaves behind.
    let mut cleaned = String::new();
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        cleaned.push_str(line.trim_end());
        cleaned.push('\n');
    }
    cleaned
}

/// Shell out to `construct-adapter-smith --suggest-mode`, feed the rendered
/// transcript context on stdin, and broadcast the returned hand — unless a
/// newer turn made this run stale. Best-effort: any failure just means no
/// suggestions for this turn.
async fn generate_suggestions(
    binary: PathBuf,
    prefix_args: Vec<String>,
    entry: Arc<SessionEntry>,
    my_gen: u64,
    context: String,
    broadcast: tokio::sync::broadcast::Sender<BroadcastMsg>,
) {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let child = tokio::process::Command::new(&binary)
        .args(&prefix_args)
        .arg("--suggest-mode")
        // Same credential floor as the adapters (spec 0180).
        .envs(crate::daemon_env::child_env_base())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = ?e, "suggest-mode spawn failed");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(context.as_bytes()).await.is_err() {
            return;
        }
        drop(stdin);
    }
    let out = match tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::debug!(error = ?e, "suggest-mode wait failed");
            return;
        }
        Err(_) => {
            tracing::debug!(session = %entry.id, "suggest-mode timed out");
            return;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            session = %entry.id,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "suggest-mode exit non-zero; skipping",
        );
        return;
    }
    let hand: construct_protocol::SuggestionHand = match serde_json::from_slice(&out.stdout) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(session = %entry.id, error = ?e, "suggest-mode output parse failed");
            return;
        }
    };
    if entry.is_deleted() {
        return;
    }
    // Stale if a newer turn ended (counter bumped) or the session moved
    // on from AwaitingInput while we generated.
    if entry.suggest_gen.load(Ordering::SeqCst) != my_gen {
        return;
    }
    {
        let s = entry.summary.read().await;
        if s.state != SessionState::AwaitingInput {
            return;
        }
    }
    let seq = entry.transcript_count.load(Ordering::Relaxed);
    let _ = broadcast.send(BroadcastMsg::Event(EventNotificationPayload {
        session_id: entry.id.clone(),
        at: Utc::now(),
        event: SessionEvent::Suggestions(hand),
        seq,
    }));
    tracing::info!(session = %entry.id, "suggestion hand broadcast");
}

/// PTY harnesses whose child is a full-screen TUI that holds the terminal's
/// foreground process group for its whole lifetime — so it never returns to a
/// detectable shell prompt and never emits `AwaitingInput` itself. The daemon
/// falls back to output-quiescence detection for these; shells are excluded
/// (they use foreground-pgroup detection in the adapter).
fn harness_uses_quiescence(s: &SessionSummary) -> bool {
    s.has_pty
        && matches!(
            s.harness.as_str(),
            "claude"
                | "codex"
                | "antigravity"
                | "agy"
                | "grok"
                | "hermes"
                | "kimi"
                | "opencode"
                | "pi"
                | "prime-agent"
                | "muse"
        )
}

/// Advance a session's PTY burst tracker for one active output event.
/// Returns the burst's start time and whether the burst has persisted for
/// [`PTY_BLIP_WINDOW`] — i.e. whether this event counts as genuine activity
/// rather than an idle housekeeping blip. A gap of [`PTY_QUIESCENCE`] or more
/// since the previous output starts a new burst: that is the same silence
/// that flips the session to AwaitingInput, so a burst can't straddle it.
/// Whether a post-respawn settle window that began at `since_ms` is over
/// (spec 0054). Never before [`RESUME_SETTLE_MIN`] — the resume repaint and
/// the delayed force-redraw cycle must both land inside the window. After
/// that, over once PTY output has been quiet for [`PTY_QUIESCENCE`] (a stale
/// pre-restart `last_pty_at_ms`, or none at all, counts as quiet — a child
/// that never repainted has nothing to suppress). Unconditionally over at
/// [`RESUME_SETTLE_MAX`] so a child streaming continuously past the resume
/// regains normal activity tracking.
fn resume_settle_over(since_ms: i64, last_pty_at_ms: Option<i64>, now_ms: i64) -> bool {
    let elapsed = now_ms.saturating_sub(since_ms);
    if elapsed >= RESUME_SETTLE_MAX.as_millis() as i64 {
        return true;
    }
    if elapsed < RESUME_SETTLE_MIN.as_millis() as i64 {
        return false;
    }
    last_pty_at_ms.map_or(true, |last| {
        now_ms.saturating_sub(last) >= PTY_QUIESCENCE.as_millis() as i64
    })
}

fn pty_burst_advance(burst_start_ms: i64, prev_pty_at_ms: Option<i64>, now_ms: i64) -> (i64, bool) {
    let new_burst = burst_start_ms <= 0
        || prev_pty_at_ms.map_or(true, |last| {
            now_ms.saturating_sub(last) >= PTY_QUIESCENCE.as_millis() as i64
        });
    let start = if new_burst { now_ms } else { burst_start_ms };
    let sustained = now_ms.saturating_sub(start) >= PTY_BLIP_WINDOW.as_millis() as i64;
    (start, sustained)
}

fn effective_mode(params: &CreateSessionParams) -> String {
    match params.mode.as_ref() {
        Some(mode) => mode.clone(),
        None if params.pty_size.is_some() => "interactive".to_string(),
        None => "headless".to_string(),
    }
}

fn builtin_harness_capabilities(name: &str) -> construct_protocol::Capabilities {
    match name {
        "shell" | "claude" | "codex" | "opencode" | "antigravity" | "agy" | "grok" | "kimi"
        | "hermes" | "pi" | "prime-agent" | "muse" | "smith" => construct_protocol::Capabilities {
            supports_pty: true,
            ..Default::default()
        },
        _ => Default::default(),
    }
}

/// One tick of the login-session watcher (spec 0117).
enum LoginWatch {
    /// The credential landed: the login did its job, stop the session and
    /// archive it out of the list (transcript kept).
    Archive,
    /// The session is gone or ended without a credential landing: leave
    /// whatever remains visible — its output is the explanation — and
    /// stop watching.
    Abandon,
    Continue,
}

/// The credential is checked before the session's state so a login CLI
/// that exits the instant the credential is written (`codex login`) still
/// archives instead of being abandoned as "ended without a credential".
fn login_watch_step(
    login_ok: bool,
    state: Option<construct_protocol::SessionState>,
) -> LoginWatch {
    use construct_protocol::SessionState;
    if login_ok {
        return LoginWatch::Archive;
    }
    match state {
        None | Some(SessionState::Done) | Some(SessionState::Errored) => LoginWatch::Abandon,
        Some(_) => LoginWatch::Continue,
    }
}

#[cfg(test)]
mod tests {
    /// The watcher archives on the credential, waits while the session
    /// lives without one, and abandons a session that ended (or vanished)
    /// without one — in that order, so a login CLI that exits as it
    /// writes the credential still archives.
    #[test]
    fn login_watcher_archives_on_credential_and_abandons_on_dead_ends() {
        use construct_protocol::SessionState;
        assert!(matches!(
            super::login_watch_step(true, Some(SessionState::Running)),
            super::LoginWatch::Archive
        ));
        assert!(matches!(
            super::login_watch_step(true, Some(SessionState::Done)),
            super::LoginWatch::Archive
        ));
        assert!(matches!(
            super::login_watch_step(false, Some(SessionState::Running)),
            super::LoginWatch::Continue
        ));
        assert!(matches!(
            super::login_watch_step(false, Some(SessionState::AwaitingInput)),
            super::LoginWatch::Continue
        ));
        assert!(matches!(
            super::login_watch_step(false, Some(SessionState::Done)),
            super::LoginWatch::Abandon
        ));
        assert!(matches!(
            super::login_watch_step(false, Some(SessionState::Errored)),
            super::LoginWatch::Abandon
        ));
        assert!(matches!(
            super::login_watch_step(false, None),
            super::LoginWatch::Abandon
        ));
    }

    #[test]
    fn suggestion_regeneration_keywords_are_bounded_and_labeled_as_guidance() {
        let rendered =
            super::render_suggest_keywords(Some("  tests\n\n docs   mobile-layout  "));
        assert!(rendered.contains("tests docs mobile-layout"));
        assert!(rendered.contains("intent guidance, not evidence"));
        assert_eq!(super::render_suggest_keywords(Some(" \n ")), "");

        let long = "x".repeat(250);
        let rendered = super::render_suggest_keywords(Some(&long));
        assert_eq!(
            rendered
                .lines()
                .find_map(|line| line.strip_prefix(
                    "Prioritize relevant suggestions around these keywords: "
                ))
                .expect("keyword line")
                .chars()
                .count(),
            200
        );
    }

    use super::*;
    use construct_protocol::{Capabilities, PtySize};

    #[test]
    fn pty_geometry_ownership_is_explicit_and_connection_scoped() {
        use crate::server::ClientKind;

        let mut policy = PtyClientPolicy::default();

        // First TUI deliberately focuses the session.
        assert_eq!(
            policy.note(1, ClientKind::Tui, Some((100, 40)), true),
            (Some((100, 40)), true)
        );
        assert_eq!(policy.owner, Some(1));

        // A browser can keep its viewport current without stealing the PTY.
        assert_eq!(
            policy.note(2, ClientKind::Remote, Some((80, 25)), false),
            (None, false)
        );
        assert_eq!(policy.owner, Some(1));

        // Browser input claims its remembered viewport; later TUI input
        // restores the first TUI's own remembered viewport. Both are
        // ownership switches the caller must announce.
        assert_eq!(
            policy.note(2, ClientKind::Remote, None, true),
            (Some((80, 25)), true)
        );
        assert_eq!(
            policy.note(1, ClientKind::Tui, None, true),
            (Some((100, 40)), true)
        );

        // Re-claiming while already owner is not a switch.
        assert_eq!(policy.note(1, ClientKind::Tui, None, true), (None, false));

        // Delayed browser layout churn after TUI input stays passive.
        assert_eq!(
            policy.note(2, ClientKind::Remote, Some((90, 30)), false),
            (None, false)
        );
        assert_eq!(policy.owner, Some(1));

        // Ownership is per connection, not per transport kind: a second TUI
        // has an independent viewport and must explicitly engage to win.
        assert_eq!(
            policy.note(3, ClientKind::Tui, Some((120, 50)), false),
            (None, false)
        );
        assert_eq!(
            policy.note(3, ClientKind::Tui, None, true),
            (Some((120, 50)), true)
        );
        assert_eq!(policy.owner, Some(3));
    }

    #[test]
    fn passive_resize_applies_only_for_current_pty_owner() {
        use crate::server::ClientKind;

        let mut policy = PtyClientPolicy::default();
        assert_eq!(
            policy.note(7, ClientKind::Remote, None, true),
            (None, true),
            "input may claim before the first measured viewport; the \
             size-preserving handoff must still be reported"
        );
        assert_eq!(
            policy.note(7, ClientKind::Remote, Some((72, 20)), false),
            (Some((72, 20)), false),
            "the owner's later viewport report reaches the OS PTY"
        );
        assert_eq!(
            policy.note(8, ClientKind::Remote, Some((90, 35)), false),
            (None, false),
            "another browser connection cannot steal through ResizeObserver"
        );
    }

    /// The ambient-degradation latch (spec 0151): first skip flips
    /// `degradation_observed` and broadcasts exactly one `FeaturesState`;
    /// later skips are silent. Deliberately independent of whether this
    /// machine actually has a smith credential — only the latch and the
    /// broadcast count are asserted.
    #[tokio::test]
    async fn ambient_degradation_latches_once_and_broadcasts_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut rx = mgr.broadcast.subscribe();

        assert!(!mgr.features_status().await.degradation_observed);
        mgr.note_ambient_degradation("auto_title").await;
        mgr.note_ambient_degradation("suggestions").await;
        assert!(mgr.features_status().await.degradation_observed);

        let mut features_broadcasts = 0;
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::FeaturesState(status) = msg {
                assert!(status.degradation_observed);
                features_broadcasts += 1;
            }
        }
        assert_eq!(features_broadcasts, 1, "one hint, not a nag stream");
    }

    #[test]
    fn validate_restart_exe_accepts_executable_rejects_bad_paths() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        // An executable file → returns the canonical (absolute) path.
        let good = dir.path().join("agentd");
        std::fs::write(&good, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = validate_restart_exe(&good).expect("executable accepted");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, std::fs::canonicalize(&good).unwrap());

        // Missing path → error.
        assert!(validate_restart_exe(&dir.path().join("nope")).is_err());

        // A directory → error (not a regular file).
        assert!(validate_restart_exe(dir.path()).is_err());

        // A non-executable file → error.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"data").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_restart_exe(&plain).is_err());
    }

    fn test_verb_for_prompt(
        effect: construct_protocol::PlaybookVerbEffect,
    ) -> construct_protocol::PlaybookVerb {
        construct_protocol::PlaybookVerb {
            name: "simplify".to_string(),
            label: "Simplify".to_string(),
            description: None,
            effect,
            interaction: construct_protocol::PlaybookVerbInteraction::SingleShot,
            order: 0,
            built_in: true,
            prompt: "Be the Simplifier.".to_string(),
        }
    }

    /// spec 0089: the full Playbook document is inlined as context alongside
    /// the selection, and the "no edit tool" instruction no longer claims the
    /// document is hidden — it's readable, just not editable.
    #[test]
    fn playbook_verb_prompt_includes_full_document_as_context() {
        let verb = test_verb_for_prompt(construct_protocol::PlaybookVerbEffect::Rewrite);
        let doc = "# Plan\n\nSection A.\n\nSection B: do the thing.\n\nSection C.\n";
        let prompt =
            playbook_verb_prompt(&verb, "owner1", doc, "Section B: do the thing.", None, None);
        assert!(
            prompt.contains("Section A.") && prompt.contains("Section C."),
            "full document, not just the selection, must appear in the prompt: {prompt}"
        );
        assert!(
            prompt.contains("Section B: do the thing."),
            "selection itself must still appear: {prompt}"
        );
        assert!(
            !prompt.contains("is not shown to you"),
            "prompt must not claim the rest of the document is hidden now that it's included: {prompt}"
        );
    }

    /// spec 0089: a verb body may place the document, selection, and
    /// instruction itself with `{{ ... }}` template variables — each
    /// referenced variable substitutes in place and suppresses its default
    /// framing section, so nothing appears twice.
    #[test]
    fn playbook_verb_prompt_substitutes_template_variables() {
        let mut verb = test_verb_for_prompt(construct_protocol::PlaybookVerbEffect::Rewrite);
        verb.prompt = "Given this document:\n{{ playbook.content }}\nfocus on:\n{{ playbook.selected_text }}\nwith guidance: {{ playbook.additional_instruction }}".to_string();
        let doc = "# Plan\n\nSection B: do the thing.\n";
        let prompt = playbook_verb_prompt(
            &verb,
            "owner7",
            doc,
            "Section B: do the thing.",
            Some("keep it short"),
            None,
        );
        assert!(
            prompt.contains("Given this document:\n# Plan"),
            "content substitutes in place: {prompt}"
        );
        assert!(
            prompt.contains("focus on:\nSection B: do the thing."),
            "selection substitutes in place: {prompt}"
        );
        assert!(
            prompt.contains("with guidance: keep it short"),
            "instruction substitutes in place: {prompt}"
        );
        assert!(
            !prompt.contains("For context, here is the full Playbook"),
            "referencing playbook.content suppresses the default document framing: {prompt}"
        );
        assert!(
            !prompt.contains("Your jurisdiction is exactly the following"),
            "referencing playbook.selected_text suppresses the default jurisdiction block: {prompt}"
        );
        assert!(
            !prompt.contains("Additional user instruction for this verb:"),
            "referencing playbook.additional_instruction suppresses the default framing: {prompt}"
        );
        assert!(
            prompt.contains("verb-result.json"),
            "the structured-return contract always applies: {prompt}"
        );
    }

    /// A document over the inline cap is truncated with a pointer to the
    /// live read tool + the owning session id, rather than growing the
    /// prompt unboundedly.
    #[test]
    fn playbook_verb_prompt_truncates_oversized_document_with_a_pointer() {
        let verb = test_verb_for_prompt(construct_protocol::PlaybookVerbEffect::Annotate);
        let huge_doc = "x".repeat(PLAYBOOK_VERB_INLINE_DOC_MAX_CHARS + 5_000);
        let prompt = playbook_verb_prompt(&verb, "owner42", &huge_doc, "selected bit", None, None);
        assert!(
            prompt.contains("truncated"),
            "oversized document must be flagged as truncated: {prompt}"
        );
        assert!(
            prompt.contains("agentd_playbook_get") && prompt.contains("owner42"),
            "truncation notice must point at the live read tool with the owning session id: {prompt}"
        );
        assert!(
            prompt.len() < huge_doc.len() + 2_000,
            "prompt must not embed the full oversized document"
        );
    }

    #[test]
    fn direct_playbook_verb_prompt_targets_owner_and_forbids_return_merge() {
        let verb = test_verb_for_prompt(construct_protocol::PlaybookVerbEffect::Rewrite);
        let prompt = playbook_verb_prompt(
            &verb,
            "owner-direct",
            "# Plan\n\nold text @{session:fork1}",
            "old text",
            None,
            Some(("owner-direct", "old text @{session:fork1}")),
        );
        assert!(prompt.contains("Update the Playbook directly"));
        assert!(prompt.contains("session_id `owner-direct`"));
        assert!(prompt.contains("old text @{session:fork1}"));
        assert!(prompt.contains("do not return a result"));
        assert!(!prompt.contains("verb-result.json"));
    }

    #[test]
    fn is_slash_command_detects_leading_slash() {
        assert!(is_slash_command("/model gpt-5.5"));
        assert!(is_slash_command("/compact"));
        // Leading whitespace before the slash still counts.
        assert!(is_slash_command("  /model gpt-5.5"));
    }

    #[test]
    fn is_slash_command_rejects_ordinary_messages() {
        assert!(!is_slash_command("fix the login bug"));
        assert!(!is_slash_command(""));
        // A slash later in the text isn't a command.
        assert!(!is_slash_command("look at src/main.rs"));
    }

    #[test]
    fn auto_title_prompt_ignores_commands_and_uses_next_ordinary_prompt() {
        assert_eq!(auto_title_prompt("/model sonnet".into()), None);
        assert_eq!(auto_title_prompt("  /compact".into()), None);
        assert_eq!(auto_title_prompt("   ".into()), None);
        assert_eq!(
            auto_title_prompt("fix the login bug".into()).as_deref(),
            Some("fix the login bug")
        );
    }

    /// A one-shot that ran but produced nothing has to be distinguishable
    /// from one that named the session: the per-session attempt latch is
    /// already spent by the time this returns, so the caller's decision to
    /// fall back to the same-harness probe rests entirely on this outcome.
    #[tokio::test]
    async fn generate_auto_title_separates_failure_from_a_named_session() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let sh = which::which("sh").expect("sh on PATH");

        // Non-zero exit — what a missing/rejected credential looks like.
        let failed = synthetic_entry("t-fail", construct_protocol::SessionKind::User, 0);
        let outcome = generate_auto_title(
            sh.clone(),
            vec!["-c".into(), "echo boom >&2; exit 1".into()],
            failed.clone(),
            "fix the login bug".into(),
            false,
            storage.clone(),
            tx.clone(),
        )
        .await;
        assert_eq!(outcome, TitleOutcome::Failed);
        assert!(failed.summary.read().await.title.is_none());

        // Empty stdout on a clean exit is just as unusable.
        let empty = synthetic_entry("t-empty", construct_protocol::SessionKind::User, 1);
        let outcome = generate_auto_title(
            sh.clone(),
            vec!["-c".into(), "printf ''".into()],
            empty.clone(),
            "fix the login bug".into(),
            false,
            storage.clone(),
            tx.clone(),
        )
        .await;
        assert_eq!(outcome, TitleOutcome::Failed);
        assert!(empty.summary.read().await.title.is_none());

        let ok = synthetic_entry("t-ok", construct_protocol::SessionKind::User, 2);
        let outcome = generate_auto_title(
            sh,
            vec!["-c".into(), "echo 'Fix The Login Bug'".into()],
            ok.clone(),
            "fix the login bug".into(),
            false,
            storage,
            tx,
        )
        .await;
        assert_eq!(outcome, TitleOutcome::Settled);
        assert_eq!(
            ok.summary.read().await.title.as_deref(),
            Some("Fix The Login Bug")
        );
    }

    /// A rename that lands while the one-shot is in flight settles the
    /// session: the caller must not read "no title applied" as licence to
    /// spawn a probe that would overwrite the user's choice.
    #[tokio::test]
    async fn generate_auto_title_settles_when_the_user_renamed_mid_flight() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let sh = which::which("sh").expect("sh on PATH");

        let entry = synthetic_entry("t-renamed", construct_protocol::SessionKind::User, 0);
        entry.summary.write().await.title = Some("Chosen By Hand".to_string());
        let outcome = generate_auto_title(
            sh,
            vec!["-c".into(), "echo 'Generated Title'".into()],
            entry.clone(),
            "fix the login bug".into(),
            false,
            storage,
            tx,
        )
        .await;
        assert_eq!(outcome, TitleOutcome::Settled);
        assert_eq!(
            entry.summary.read().await.title.as_deref(),
            Some("Chosen By Hand")
        );
    }

    /// A probe session persisted at startup is a leftover from a daemon
    /// restart that interrupted generation mid-run; startup must reap it
    /// instead of resuming, while other kinds stay untouched.
    #[tokio::test]
    async fn startup_reaps_leftover_usage_probe_sessions() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mgr = Arc::new(mgr);

        let probe = synthetic_entry("probe1", construct_protocol::SessionKind::UsageProbe, 0);
        // Done: not resumable, so the resume loop can't mask a missing reap
        // by erroring the entry out, and the user session stays untouched.
        probe.summary.write().await.state = SessionState::Done;
        let user = synthetic_entry("user1", construct_protocol::SessionKind::User, 1);
        user.summary.write().await.state = SessionState::Done;
        {
            let mut map = mgr.sessions.write().await;
            map.insert("probe1".into(), probe);
            map.insert("user1".into(), user);
        }

        mgr.clone().resume_running_sessions().await;

        let map = mgr.sessions.read().await;
        assert!(
            !map.contains_key("probe1"),
            "leftover probe must be reaped at startup"
        );
        assert!(
            map.contains_key("user1"),
            "user sessions must survive the probe reap"
        );
    }

    #[test]
    fn startup_resume_retries_errored_sessions() {
        assert!(should_resume_on_startup(SessionState::Pending));
        assert!(should_resume_on_startup(SessionState::Running));
        assert!(should_resume_on_startup(SessionState::AwaitingInput));
        assert!(should_resume_on_startup(SessionState::Paused));
        assert!(should_resume_on_startup(SessionState::Errored));
        assert!(!should_resume_on_startup(SessionState::Done));
    }

    /// Reattaching to an adapter that outlived the daemon must not invent a
    /// turn. The adapter is parked exactly where it was, and for a headless
    /// session it will not speak again until one runs — so a false `Running`
    /// there is unfalsifiable by anything except a client opening the session,
    /// and meanwhile it spins a working glyph and banks fake compute time.
    #[test]
    fn reattach_leaves_an_idle_session_idle() {
        assert_eq!(
            reattached_state(SessionState::AwaitingInput),
            SessionState::AwaitingInput
        );
        // Everything else: a live adapter means the session is live. Mid-turn
        // stays mid-turn, a never-started session gets a non-terminal
        // placeholder, and an `Errored` one must stop looking dead now that
        // its adapter answered.
        assert_eq!(
            reattached_state(SessionState::Running),
            SessionState::Running
        );
        assert_eq!(
            reattached_state(SessionState::Pending),
            SessionState::Running
        );
        assert_eq!(
            reattached_state(SessionState::Errored),
            SessionState::Running
        );
    }

    #[test]
    fn clipboard_attachment_names_are_safe_and_typed() {
        assert_eq!(
            sanitized_file_stem("../../My Screen Shot.png").as_deref(),
            Some("My-Screen-Shot")
        );
        assert_eq!(sanitized_file_stem("😵").as_deref(), None);
        assert_eq!(
            extension_for_attachment(
                Some("photo.jpeg"),
                Some("application/octet-stream"),
                b"plain"
            ),
            "jpeg"
        );
        assert_eq!(
            extension_for_attachment(None, Some("image/png; charset=binary"), b"plain"),
            "png"
        );
        assert_eq!(
            extension_for_attachment(None, None, b"\x89PNG\r\n\x1a\nrest"),
            "png"
        );
        assert_eq!(extension_for_attachment(None, None, b"hello"), "txt");
    }

    fn placement_summary(
        id: &str,
        position: i64,
        group_id: Option<&str>,
        kind: construct_protocol::SessionKind,
    ) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            harness: "smith".to_string(),
            cwd: "/tmp".to_string(),
            title: None,
            auto_title_pending: false,
            state: SessionState::Running,
            created_at: "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            last_event_at: None,
            last_message_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count: 0,
            has_pty: true,
            mode: Some("interactive".to_string()),
            pinned: false,
            position,
            group_id: group_id.map(str::to_string),
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: Default::default(),
            context_used: None,
            context_window: None,
            context_segments: Vec::new(),
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind,
            archived: false,
            minibuffer_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    #[test]
    fn set_state_tracked_accumulates_running_spans_into_busy_ms() {
        let mut s = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);
        s.state = SessionState::AwaitingInput;

        // Entering Running opens a span but banks nothing yet.
        set_state_tracked(&mut s, SessionState::Running, 1_000);
        assert_eq!(s.state, SessionState::Running);
        assert_eq!(s.busy_ms, 0);
        assert_eq!(s.busy_running_since_ms, Some(1_000));

        // Running → Running keeps the ORIGINAL span open — re-asserting the
        // state (status events repeat it) must not reset the clock.
        set_state_tracked(&mut s, SessionState::Running, 2_000);
        assert_eq!(s.busy_running_since_ms, Some(1_000));

        // Leaving Running banks the span into busy_ms.
        set_state_tracked(&mut s, SessionState::AwaitingInput, 4_000);
        assert_eq!(s.busy_ms, 3_000);
        assert_eq!(s.busy_running_since_ms, None);

        // Idle transitions with no open span bank nothing.
        set_state_tracked(&mut s, SessionState::Done, 9_000);
        assert_eq!(s.busy_ms, 3_000);

        // A later Running span adds to the running total.
        set_state_tracked(&mut s, SessionState::Running, 10_000);
        set_state_tracked(&mut s, SessionState::Errored, 10_500);
        assert_eq!(s.busy_ms, 3_500);

        // Mid-span, busy_ms_at reports banked time plus the open span.
        set_state_tracked(&mut s, SessionState::Running, 20_000);
        assert_eq!(s.busy_ms_at(20_250), 3_750);
    }

    #[test]
    fn playbook_execution_submits_to_pty_backed_sessions() {
        let summary = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);

        assert_eq!(
            session_input_delivery(&summary),
            SessionInputDelivery::PtySubmit
        );
    }

    #[test]
    fn playbook_pty_submit_terminates_with_carriage_return() {
        // Smith's line editor submits on CR and treats LF as a newline
        // insertion, so the PtySubmit terminator must be `\r`. An LF here
        // regresses to the bug where the playbook prompt landed in smith's
        // input editor but never submitted.
        let bytes = playbook_pty_submit_bytes("run the playbook");
        assert_eq!(bytes.last(), Some(&b'\r'));
        assert!(!bytes.contains(&b'\n'));
        assert_eq!(&bytes[..bytes.len() - 1], b"run the playbook");
    }

    #[test]
    fn create_forwards_initial_prompt_to_session_start() {
        // The seed prompt must reach the adapter via session.start params —
        // that is the only channel by which a freshly created session starts
        // its first turn (headless adapters run it off their queue;
        // interactive PTY harnesses get it as a launch arg). Dropping it here
        // reproduces the reported symptom where a created subagent sits idle
        // in AwaitingInput until a manual follow-up send_input.
        let start = start_params_for_create(
            "s1".to_string(),
            "/tmp".to_string(),
            Some("do the task".to_string()),
            None,
            Some("headless".to_string()),
            None,
            HashMap::new(),
            Vec::new(),
        );
        assert_eq!(start.prompt.as_deref(), Some("do the task"));
        assert_eq!(start.session_id, "s1");
        assert_eq!(start.mode.as_deref(), Some("headless"));

        // A create with no prompt stays None, so the adapter correctly idles
        // in AwaitingInput waiting for the first send_input.
        let empty = start_params_for_create(
            "s2".to_string(),
            "/tmp".to_string(),
            None,
            None,
            None,
            None,
            HashMap::new(),
            Vec::new(),
        );
        assert_eq!(empty.prompt, None);
    }

    #[test]
    fn session_identity_env_is_daemon_owned_and_stable() {
        let mut env = HashMap::from([(
            construct_protocol::agent_context::ENV_SESSION_ID.to_string(),
            "caller-supplied".to_string(),
        )]);

        install_session_identity_env(&mut env, "s-cache-stable");

        assert_eq!(
            env.get(construct_protocol::agent_context::ENV_SESSION_ID)
                .map(String::as_str),
            Some("s-cache-stable")
        );
    }

    #[test]
    fn playbook_execution_typed_submit_for_external_agent_pty_sessions() {
        let mut summary = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);
        summary.harness = "claude".to_string();

        assert_eq!(
            session_input_delivery(&summary),
            SessionInputDelivery::ExternalPtyTypedSubmit
        );
    }

    #[test]
    fn interactive_codex_takes_the_typed_submit_framing() {
        // The harness behind interactive operator sessions (spec 0176). Codex
        // is a crossterm TUI in raw mode, where LF is Ctrl+J ("insert a
        // newline"), not Enter — classifying it as anything but a typed
        // submit regresses to a delivery that types the message into the
        // composer and never submits it.
        let mut summary = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);
        summary.harness = "codex".to_string();

        assert_eq!(
            session_input_delivery(&summary),
            SessionInputDelivery::ExternalPtyTypedSubmit
        );
    }

    #[test]
    fn playbook_bracketed_paste_frames_and_sanitizes_body() {
        // A plain multi-line body is wrapped in the paste markers a real
        // terminal sends, so the external agent TUI buffers it as one paste
        // (its multiline guard fires) instead of submitting on the first
        // newline.
        assert_eq!(
            playbook_bracketed_paste_bytes("line one\nline two"),
            b"\x1b[200~line one\nline two\x1b[201~".to_vec()
        );
        // An embedded end marker is stripped so a malicious / accidental
        // `ESC[201~` in the playbook can't terminate the paste early.
        assert_eq!(
            playbook_bracketed_paste_bytes("a\x1b[201~b"),
            b"\x1b[200~ab\x1b[201~".to_vec()
        );
    }

    #[test]
    fn playbook_execution_prompt_requires_autonomous_run() {
        let prompt = playbook_execution_prompt();

        assert!(prompt.contains("autonomously"));
        assert!(prompt.contains("agentd_context"));
        assert!(prompt.contains("construct_context"));
        assert!(prompt.contains("playbook_run"));
        assert!(prompt.contains("latest playbook content"));
        assert!(prompt.contains("pending map"));
        assert!(prompt.contains("settle_others"));
        assert!(!prompt.contains("Compare options and summarize findings."));
    }

    #[test]
    fn playbook_execution_prompt_appends_run_comment_as_one_line() {
        let prompt =
            playbook_execution_prompt_with_comment(Some("  focus tests\nand keep output short  "));

        assert!(prompt.contains("autonomously"));
        assert!(prompt.contains(
            "Additional user instruction for this Run: focus tests and keep output short"
        ));
        assert!(!prompt.contains("focus tests\nand keep output short"));
    }

    /// Spec 0076 auto-close contract: a Run fork's prompt tells it to
    /// archive ITSELF (its own session id, not the owner's) as its final
    /// action once every dispatched block has settled, and to stay open
    /// when blocked / work remains / the user joined the conversation.
    /// The owner-targeting contract must survive alongside it.
    #[test]
    fn forked_run_prompt_instructs_self_archive_on_completion() {
        let prompt = forked_playbook_execution_prompt("owner1", "fork1", None);

        assert!(prompt.contains("session_id: \"owner1\""));
        assert!(prompt.contains("do not edit a Playbook belonging to this fork"));
        assert!(prompt.contains("construct_archive_session"));
        assert!(prompt.contains("session_id: \"fork1\""));
        assert!(prompt.contains("settles every block"));
        assert!(prompt.contains("Never archive before the settle edit"));
        assert!(prompt.contains("last action"));
        assert!(prompt.contains("leave the session open"));
    }

    #[test]
    fn playbook_run_context_carries_run_contract_and_registered_smart_clips() {
        let playbook = PlaybookDocument {
            session_id: "s123".to_string(),
            markdown: "# Research brief\n\nCompare options and summarize findings.\n".to_string(),
            version: 9,
            updated_at_ms: 1234,
            template_id: None,
        };
        let context = playbook_run_context(&playbook, "selection", "- selected work");

        assert_eq!(context.session_id, "s123");
        assert_eq!(context.playbook_version, 9);
        assert_eq!(context.playbook_updated_at_ms, 1234);
        assert_eq!(context.scope, "selection");
        assert_eq!(context.markdown, "- selected work");
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("free-form instructions and state")));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("Infer the user's intended objective")));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("keep taking useful next actions")));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("Do not ask the user to run the playbook again")));
        assert!(context.instructions.iter().any(|s| s
            .contains("status-only construct_playbook_edit")
            && s.contains("settle_others")));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("clip_id attribute identifies a specific clip instance")));
        assert!(context
            .instructions
            .iter()
            .any(|s| s.contains("running the playbook never triggers the links")));
        for ext in dialect::extensions_for_surface(dialect::SURFACE_PLAYBOOK)
            .filter(|ext| ext.kind == dialect::KIND_REFERENCE)
        {
            assert!(
                context
                    .smart_clips
                    .iter()
                    .any(|registered| registered.syntax == ext.syntax
                        && registered.description == ext.description),
                "context should include registered smart clip syntax and description for {}",
                ext.syntax
            );
        }
        assert!(
            !context
                .smart_clips
                .iter()
                .any(|registered| registered.type_name == "playbook-section"),
            "widget-only extensions stay out of the playbook run reference"
        );
    }

    #[test]
    fn playbook_execution_uses_adapter_input_for_headless_sessions() {
        let mut summary = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);
        summary.has_pty = false;

        assert_eq!(
            session_input_delivery(&summary),
            SessionInputDelivery::AdapterInput
        );
    }

    #[test]
    fn pty_capable_headless_session_uses_adapter_input() {
        let mut summary = placement_summary("s1", 0, None, construct_protocol::SessionKind::User);
        summary.harness = "muse".to_string();
        summary.has_pty = true;
        summary.mode = Some("headless".to_string());

        assert_eq!(
            session_input_delivery(&summary),
            SessionInputDelivery::AdapterInput
        );
    }

    fn dispatch_plan(markdown: &str) -> Option<Vec<PlaybookDispatchItem>> {
        playbook_dispatch_plan(&construct_protocol::playbook_block_spans(markdown))
    }

    #[test]
    fn playbook_dispatch_plan_matches_single_item_with_harness_clip() {
        let items = dispatch_plan("- Fix bug @{harness:codex}\n").expect("plan");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].harness, "codex");
        assert_eq!(items[0].prompt, "Fix bug");
        assert_eq!(items[0].text, "- Fix bug @{harness:codex}");
    }

    #[test]
    fn playbook_dispatch_plan_strips_ordered_marker_and_collapses_wrapped_lines() {
        let items =
            dispatch_plan("1. Investigate\n   the flaky test @{harness:claude}\n").expect("plan");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].harness, "claude");
        assert_eq!(items[0].prompt, "Investigate the flaky test");
    }

    #[test]
    fn playbook_dispatch_plan_none_for_heading() {
        assert!(dispatch_plan("# Title @{harness:codex}\n").is_none());
    }

    #[test]
    fn playbook_dispatch_plan_none_for_missing_clip() {
        assert!(dispatch_plan("- just a task\n").is_none());
    }

    #[test]
    fn playbook_dispatch_plan_none_for_multiple_clips() {
        assert!(dispatch_plan("- do X @{harness:codex} and @{session:s1}\n").is_none());
    }

    #[test]
    fn playbook_dispatch_plan_none_for_non_harness_clip() {
        assert!(dispatch_plan("- do X @{session:s1}\n").is_none());
    }

    #[test]
    fn playbook_dispatch_plan_none_for_mixed_selection() {
        // Whole selection falls through to the normal path when any block
        // doesn't match the fast-path shape, even if another block does.
        assert!(dispatch_plan("- item one @{harness:codex}\n- item two\n").is_none());
    }

    #[test]
    fn position_after_visible_session_uses_gap_below_source() {
        let sessions = vec![
            placement_summary("a", 10, None, construct_protocol::SessionKind::User),
            placement_summary("b", 30, None, construct_protocol::SessionKind::User),
        ];

        let placement = position_after_visible_session("a", &None, &sessions).expect("placement");

        assert_eq!(placement.position, 20);
        assert!(placement.updates.is_empty());
    }

    #[test]
    fn position_after_visible_session_renumbers_dense_region() {
        let sessions = vec![
            placement_summary("a", 10, Some("g1"), construct_protocol::SessionKind::User),
            placement_summary("b", 11, Some("g1"), construct_protocol::SessionKind::User),
            placement_summary("c", 12, Some("g1"), construct_protocol::SessionKind::User),
            placement_summary(
                "hidden",
                13,
                Some("g1"),
                construct_protocol::SessionKind::Subagent,
            ),
        ];
        let group = Some("g1".to_string());

        let placement = position_after_visible_session("a", &group, &sessions).expect("placement");

        assert_eq!(placement.position, 522);
        assert_eq!(
            placement.updates,
            vec![("b".to_string(), 1034), ("c".to_string(), 2058)]
        );
    }

    /// Regression: codex / claude / shell sessions painted only their
    /// startup banner after a daemon restart because the PTY was
    /// spawned at the cached size and no SIGWINCH ever fired (kernel
    /// dedup on `ioctl(TIOCSWINSZ)` when new size == current size).
    /// The respawn path must schedule a bump+restore for these.
    #[test]
    fn force_redraw_runs_for_pty_adapters_with_cached_size() {
        let caps = pty::pty_caps();
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(
            force_redraw_size_on_resume(&caps, size),
            Some(PtySize {
                cols: 160,
                rows: 50
            })
        );
    }

    #[test]
    fn opencode_native_fork_uses_its_persisted_session_id() {
        assert_eq!(
            lifecycle::native_fork_spec("opencode"),
            Some(("opencode_session_id.txt", "CONSTRUCT_OPENCODE_FORK_FROM"))
        );
        assert_eq!(
            SessionManager::native_id_file_name("opencode"),
            Some("opencode_session_id.txt")
        );
    }

    /// Smith advertises `supports_silent_resume = true` because its
    /// `interactive.rs` deliberately paints nothing on resume — the
    /// PTY ring carries the prior screen forward and the next
    /// keystroke triggers a redraw. A forced SIGWINCH here would
    /// double-paint the editor pane and confuse the line editor's
    /// stored cursor.
    #[test]
    fn force_redraw_skipped_for_silent_resume_adapters() {
        let mut caps = pty::pty_caps();
        caps.supports_silent_resume = true;
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(force_redraw_size_on_resume(&caps, size), None);
    }

    /// No cached size on disk (e.g., fresh session never had its
    /// pty_size persisted) → nothing to restore to, skip the redraw.
    /// The TUI's normal first-render pty_resize handles sizing.
    #[test]
    fn force_redraw_skipped_without_cached_size() {
        let caps = pty::pty_caps();
        assert_eq!(force_redraw_size_on_resume(&caps, None), None);
    }

    /// The settle gate: don't fire while the child is still drawing
    /// (recent output) or hasn't drawn at all, but do fire once it goes
    /// quiet, and always fire past the hard cap.
    #[test]
    fn resume_redraw_settle_gate() {
        let now = 1_000_000i64;
        let settle = RESPAWN_REDRAW_SETTLE.as_millis() as i64;
        // Nothing drawn yet, well under the cap → wait.
        assert!(!resume_redraw_ready(None, now, Duration::from_millis(0)));
        // Output 50ms ago (< settle) → still drawing, wait.
        assert!(!resume_redraw_ready(
            Some(now - 50),
            now,
            Duration::from_secs(1)
        ));
        // Quiet for exactly the settle window → fire.
        assert!(resume_redraw_ready(
            Some(now - settle),
            now,
            Duration::from_secs(1)
        ));
        // Quiet well past settle → fire.
        assert!(resume_redraw_ready(
            Some(now - 5_000),
            now,
            Duration::from_secs(1)
        ));
        // Never settles (recent output) but hit the hard cap → fire anyway.
        assert!(resume_redraw_ready(Some(now), now, RESPAWN_REDRAW_MAX_WAIT));
        // Never drew anything, but hit the hard cap → fire anyway.
        assert!(resume_redraw_ready(None, now, RESPAWN_REDRAW_MAX_WAIT));
    }

    /// Headless / non-PTY adapters (anything smith-headless-only or
    /// future structured-only harnesses) shouldn't get a SIGWINCH.
    #[test]
    fn force_redraw_skipped_for_non_pty_adapters() {
        let caps = Capabilities {
            supports_pty: false,
            supports_silent_resume: false,
            ..Default::default()
        };
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(force_redraw_size_on_resume(&caps, size), None);
    }

    /// Degenerate size (0×anything or anything×0) is unrepresentable
    /// to `ioctl(TIOCSWINSZ)` — skip instead of forwarding garbage.
    #[test]
    fn force_redraw_skipped_for_degenerate_size() {
        let caps = pty::pty_caps();
        assert_eq!(
            force_redraw_size_on_resume(&caps, Some(PtySize { cols: 0, rows: 50 })),
            None
        );
        assert_eq!(
            force_redraw_size_on_resume(&caps, Some(PtySize { cols: 160, rows: 0 })),
            None
        );
    }

    fn synthetic_entry(
        id: &str,
        kind: construct_protocol::SessionKind,
        position: i64,
    ) -> Arc<SessionEntry> {
        synthetic_entry_with_group(id, kind, position, None)
    }

    fn synthetic_entry_with_group(
        id: &str,
        kind: construct_protocol::SessionKind,
        position: i64,
        group_id: Option<String>,
    ) -> Arc<SessionEntry> {
        use chrono::Utc;
        use std::sync::atomic::AtomicU64;
        use tokio::sync::RwLock;

        Arc::new(SessionEntry {
            id: id.to_string(),
            summary: RwLock::new(construct_protocol::SessionSummary {
                id: id.to_string(),
                harness: "shell".into(),
                cwd: "/tmp".into(),
                title: None,
                auto_title_pending: false,
                state: SessionState::Running,
                created_at: Utc::now(),
                last_event_at: None,
                last_message_at: None,
                cost_usd: None,
                model: None,
                effort: None,
                route: None,
                route_capable: false,
                worktree: None,
                pending_input: false,
                last_prompt: None,
                last_message_role: None,
                last_message: None,
                last_error: None,
                event_count: 0,
                has_pty: true,
                mode: None,
                pinned: false,
                position,
                group_id,
                parent_session_id: None,
                native_subagent: None,
                last_pty_at_ms: None,
                busy_ms: 0,
                busy_running_since_ms: None,
                message_count: 0,
                tokens: Default::default(),
                context_used: None,
                context_window: None,
                context_segments: Vec::new(),
                approval_mode: construct_protocol::ApprovalMode::Manual,
                kind,
                archived: false,
                minibuffer_loop_disabled: false,
                needs_attention: false,
                forked_from: None,
                merge: None,
            }),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(0),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// A `SessionKind::Subagent` entry whose `parent_session_id` points at
    /// `parent_id`, mirroring how `construct_subagent_create` links a child to
    /// its owner.
    async fn synthetic_subagent_entry(
        id: &str,
        parent_id: &str,
        position: i64,
    ) -> Arc<SessionEntry> {
        let entry = synthetic_entry(id, construct_protocol::SessionKind::Subagent, position);
        entry.summary.write().await.parent_session_id = Some(parent_id.to_string());
        entry
    }

    /// A `SessionKind::User` entry with `forked_from` pointing at
    /// `parent_id`, mirroring how `Client::fork_session` creates a fork as a
    /// top-level sibling of its source.
    async fn synthetic_fork_entry(
        id: &str,
        parent_id: &str,
        position: i64,
        group_id: Option<String>,
    ) -> Arc<SessionEntry> {
        let entry = synthetic_entry_with_group(
            id,
            construct_protocol::SessionKind::User,
            position,
            group_id,
        );
        entry.summary.write().await.forked_from = Some(construct_protocol::ForkedFrom {
            session_id: parent_id.to_string(),
            transcript_seq: 0,
            at_ms: 0,
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: false,
        });
        entry
    }

    fn test_verb(effect: construct_protocol::PlaybookVerbEffect) -> construct_protocol::PlaybookVerb {
        construct_protocol::PlaybookVerb {
            name: "simplify".to_string(),
            label: "Simplify".to_string(),
            description: None,
            effect,
            interaction: construct_protocol::PlaybookVerbInteraction::SingleShot,
            order: 0,
            built_in: true,
            prompt: "test verb".to_string(),
        }
    }

    /// spec 0089/0042: `narrow_playbook_run` only narrows an *existing* run —
    /// it creates nothing on its own — so a `PlaybookShimmerDecl` on an edit
    /// is silently dropped unless something seeded a `PlaybookRunProgress`
    /// for the session first. This reproduces `playbook_verb_execute`'s own
    /// seed-then-edit sequence directly (without spawning a real verb
    /// session) to prove the seed step is load-bearing: the shimmer decl
    /// takes effect only when `start_playbook_run_with_dispatch_state` runs
    /// first.
    #[tokio::test]
    async fn playbook_shimmer_decl_is_a_no_op_without_seeding_a_run_first() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "vshim".into(),
            synthetic_entry("vshim", construct_protocol::SessionKind::User, 0),
        );
        storage
            .update_playbook(
                "vshim",
                "# Plan\n\nDo the thing.\n".to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");

        let content_id = construct_protocol::playbook_block_spans("Do the thing.")
            .into_iter()
            .next()
            .map(|span| span.id)
            .unwrap_or_default();
        let decl = construct_protocol::PlaybookShimmerDecl {
            id: content_id,
            shimmer: true,
            tooltip: Some("Simplify".to_string()),
        };

        // Without seeding: no run exists yet, so the decl is dropped.
        mgr.playbook_edit_from_conn(
            PlaybookEditParams {
                session_id: "vshim".to_string(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "Do the thing.".to_string(),
                    new_string: "Do the thing. @{session:vshim-verb}".to_string(),
                    replace_all: false,
                    keep_pending: true,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![decl.clone()],
            },
            None,
        )
        .await
        .expect("edit applies");
        assert!(
            mgr.playbook_run_snapshot("vshim").is_none(),
            "shimmer decl must not fabricate a run out of thin air"
        );

        // Reset the doc and repeat, this time seeding first — mirrors
        // playbook_verb_execute's own call order.
        storage
            .update_playbook(
                "vshim",
                "# Plan\n\nDo the thing.\n".to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("reset playbook");
        mgr.start_playbook_run_with_dispatch_state(
            "vshim",
            "Do the thing.",
            true,
            None,
            false,
            None,
        );
        assert!(
            mgr.playbook_run_snapshot("vshim").is_some(),
            "seeding creates a run"
        );
        let edit_result = mgr
            .playbook_edit_from_conn(
                PlaybookEditParams {
                    session_id: "vshim".to_string(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: "Do the thing.".to_string(),
                        new_string: "Do the thing. @{session:vshim-verb}".to_string(),
                        replace_all: false,
                        keep_pending: true,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: None,
                    shimmer: vec![decl],
                },
                None,
            )
            .await
            .expect("edit applies");
        let shimmering = edit_result
            .blocks
            .iter()
            .any(|block| block.shimmer && block.text.contains("Do the thing."));
        assert!(
            shimmering,
            "with a run seeded first, the shimmer decl takes effect: {:?}",
            edit_result.blocks
        );
    }

    /// spec 0089: once a verb session's result file exists and its anchor
    /// still matches the live document, `maybe_complete_verb_merge` applies
    /// it mechanically — no escalation, no LLM round trip — and retires the
    /// verb session.
    #[tokio::test]
    async fn playbook_verb_merge_applies_mechanically_when_anchor_is_unchanged() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "vowner".into(),
            synthetic_entry("vowner", construct_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "vsub".into(),
            synthetic_subagent_entry("vsub", "vowner", 1).await,
        );

        storage
            .update_playbook(
                "vowner",
                "# Plan\n\nDo the thing.\n".to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");
        // Mirrors the provisional clip-annotation edit `playbook_verb_execute`
        // applies at spawn time, before any merge is pending.
        let anchor = "Do the thing. @{session:vsub}".to_string();
        storage
            .edit_playbook(
                "vowner",
                &[construct_protocol::PlaybookEdit {
                    old_string: "Do the thing.".to_string(),
                    new_string: anchor.clone(),
                    replace_all: false,
                    keep_pending: false,
                }],
                construct_protocol::PlaybookUpdateActor::Agent,
                None,
            )
            .expect("seed provisional anchor");

        let widgets_dir = storage.widgets_dir("vsub");
        std::fs::create_dir_all(&widgets_dir).unwrap();
        let result_file = widgets_dir.join("verb-result.json");
        std::fs::write(&result_file, r#"{"content":"Do the thing, carefully."}"#).unwrap();

        mgr.pending_verb_merges.lock().unwrap().insert(
            "vsub".to_string(),
            PendingVerbMerge {
                playbook_session_id: "vowner".to_string(),
                verb: test_verb(construct_protocol::PlaybookVerbEffect::Rewrite),
                anchor,
                result_file,
            },
        );

        mgr.maybe_complete_verb_merge("vsub", construct_protocol::SessionState::Done)
            .await;

        assert!(
            mgr.pending_verb_merges
                .lock()
                .unwrap()
                .get("vsub")
                .is_none(),
            "pending merge is consumed"
        );
        let playbook = storage.read_playbook("vowner").expect("read playbook");
        assert!(
            playbook.markdown.contains("Do the thing, carefully."),
            "verb result applied: {}",
            playbook.markdown
        );
        assert!(
            playbook.markdown.contains("@{session:vsub}"),
            "rewrite preserves the subagent's provenance clip: {}",
            playbook.markdown
        );
        assert!(
            mgr.get_entry("vsub")
                .await
                .unwrap()
                .archived
                .load(Ordering::SeqCst),
            "merged verb session is archived"
        );
    }

    /// spec 0089: when the selection changed underneath an in-flight verb,
    /// the mechanical merge's anchor no longer matches — the document must be
    /// left exactly as-is (no partial/garbled merge), and the subagent still
    /// retires even though delivering the escalation message fails here (the
    /// synthetic owner session has no live adapter to deliver into).
    #[tokio::test]
    async fn playbook_verb_merge_leaves_document_untouched_when_anchor_has_drifted() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "vowner2".into(),
            synthetic_entry("vowner2", construct_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "vsub2".into(),
            synthetic_subagent_entry("vsub2", "vowner2", 1).await,
        );

        let drifted_markdown = "# Plan\n\nSomething entirely different now.\n";
        storage
            .update_playbook(
                "vowner2",
                drifted_markdown.to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");

        let widgets_dir = storage.widgets_dir("vsub2");
        std::fs::create_dir_all(&widgets_dir).unwrap();
        let result_file = widgets_dir.join("verb-result.json");
        std::fs::write(&result_file, r#"{"content":"Do the thing, carefully."}"#).unwrap();

        mgr.pending_verb_merges.lock().unwrap().insert(
            "vsub2".to_string(),
            PendingVerbMerge {
                playbook_session_id: "vowner2".to_string(),
                verb: test_verb(construct_protocol::PlaybookVerbEffect::Rewrite),
                // References text no longer present in the document.
                anchor: "Do the thing. @{session:vsub2}".to_string(),
                result_file,
            },
        );

        mgr.maybe_complete_verb_merge("vsub2", construct_protocol::SessionState::Done)
            .await;

        assert!(
            mgr.pending_verb_merges
                .lock()
                .unwrap()
                .get("vsub2")
                .is_none(),
            "pending merge is consumed even when escalation delivery fails"
        );
        let playbook = storage.read_playbook("vowner2").expect("read playbook");
        assert_eq!(
            playbook.markdown, drifted_markdown,
            "a drifted anchor must never partially merge into the document"
        );
        assert!(
            mgr.get_entry("vsub2")
                .await
                .unwrap()
                .archived
                .load(Ordering::SeqCst),
            "verb session retires even when escalation delivery fails"
        );
    }

    /// spec 0089: a verb session that reaches a terminal state without ever
    /// writing a result is abandoned — the document is untouched and the
    /// pending entry is cleared so it doesn't linger forever.
    #[tokio::test]
    async fn playbook_verb_merge_abandoned_when_session_ends_without_result() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "vowner3".into(),
            synthetic_entry("vowner3", construct_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "vsub3".into(),
            synthetic_subagent_entry("vsub3", "vowner3", 1).await,
        );

        let markdown = "# Plan\n\nDo the thing.\n";
        storage
            .update_playbook(
                "vowner3",
                markdown.to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");

        mgr.pending_verb_merges.lock().unwrap().insert(
            "vsub3".to_string(),
            PendingVerbMerge {
                playbook_session_id: "vowner3".to_string(),
                verb: test_verb(construct_protocol::PlaybookVerbEffect::Annotate),
                anchor: "Do the thing.".to_string(),
                // Never written — the subagent errored before producing one.
                result_file: storage.widgets_dir("vsub3").join("verb-result.json"),
            },
        );

        mgr.maybe_complete_verb_merge("vsub3", construct_protocol::SessionState::Errored)
            .await;

        assert!(
            mgr.pending_verb_merges
                .lock()
                .unwrap()
                .get("vsub3")
                .is_none(),
            "abandoned verb clears its pending entry"
        );
        let playbook = storage.read_playbook("vowner3").expect("read playbook");
        assert_eq!(
            playbook.markdown, markdown,
            "an abandoned verb must not touch the document"
        );
    }

    /// spec 0089: completing a verb settles its in-flight shimmer. The
    /// annotate case is the one that regresses silently: the merge keeps the
    /// anchor block's text — and therefore its content id — alive, so unless
    /// the merge explicitly declares it settled, the block stays in the
    /// run's pending set and shimmers forever after the verb is done.
    #[tokio::test]
    async fn playbook_verb_annotate_merge_settles_anchor_shimmer() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "vowner4".into(),
            synthetic_entry("vowner4", construct_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "vsub4".into(),
            synthetic_subagent_entry("vsub4", "vowner4", 1).await,
        );

        storage
            .update_playbook(
                "vowner4",
                "# Plan\n\nDo the thing.\n".to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");
        // Mirror `playbook_verb_execute`'s spawn sequence: seed the run's
        // pending set over the selection, then apply the provisional
        // clip-annotation edit with its shimmer declaration.
        mgr.start_playbook_run_with_dispatch_state(
            "vowner4",
            "Do the thing.",
            true,
            None,
            false,
            None,
        );
        let anchor = "Do the thing. @{session:vsub4}".to_string();
        let content_id = construct_protocol::playbook_block_spans(&anchor)
            .into_iter()
            .next()
            .map(|span| span.id)
            .unwrap_or_default();
        let edit_result = mgr
            .playbook_edit_from_conn(
                PlaybookEditParams {
                    session_id: "vowner4".to_string(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: "Do the thing.".to_string(),
                        new_string: anchor.clone(),
                        replace_all: false,
                        keep_pending: true,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: Some("verb: test".to_string()),
                    shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                        id: content_id,
                        shimmer: true,
                        tooltip: Some("Test verb".to_string()),
                    }],
                },
                None,
            )
            .await
            .expect("provisional clip edit");
        assert!(
            edit_result
                .blocks
                .iter()
                .any(|block| block.shimmer && block.text.contains("Do the thing.")),
            "the anchor block shimmers while the verb is in flight: {:?}",
            edit_result.blocks
        );

        let widgets_dir = storage.widgets_dir("vsub4");
        std::fs::create_dir_all(&widgets_dir).unwrap();
        let result_file = widgets_dir.join("verb-result.json");
        std::fs::write(
            &result_file,
            r#"{"content":"> Assumption: the thing exists."}"#,
        )
        .unwrap();
        mgr.pending_verb_merges.lock().unwrap().insert(
            "vsub4".to_string(),
            PendingVerbMerge {
                playbook_session_id: "vowner4".to_string(),
                verb: test_verb(construct_protocol::PlaybookVerbEffect::Annotate),
                anchor: anchor.clone(),
                result_file,
            },
        );

        mgr.maybe_complete_verb_merge("vsub4", construct_protocol::SessionState::Done)
            .await;

        let playbook = storage.read_playbook("vowner4").expect("read playbook");
        assert!(
            playbook.markdown.contains(&anchor)
                && playbook.markdown.contains("Assumption: the thing exists."),
            "annotate keeps the anchor and inserts the result below it: {}",
            playbook.markdown
        );
        let blocks = mgr.playbook_blocks_projection("vowner4", &playbook.markdown);
        assert!(
            blocks.iter().all(|block| !block.shimmer),
            "a completed verb leaves no block shimmering: {blocks:?}"
        );
    }

    /// Spec 0076: closing a selection-Run fork settles its dispatched
    /// blocks' shimmer deterministically — including the drift case, where
    /// the fork edited its block's text (advancing the content id past the
    /// stored dispatch anchor) but the block still carries the fork's
    /// `@{session:<id>}` clip. Without the backstop, a fork that archives
    /// itself without a final settle edit leaves the block shimmering
    /// forever, since the run only auto-clears on the OWNER's terminal
    /// state and the owner never ran.
    #[tokio::test]
    async fn run_fork_close_settles_dispatched_shimmer_even_after_drift() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "fowner".into(),
            synthetic_entry("fowner", construct_protocol::SessionKind::User, 0),
        );

        // The document as the fork left it: the dispatched item's text was
        // rewritten by the fork (drifted from the dispatch anchor) but its
        // session clip survived, and the fork never settled its shimmer.
        let anchor = "- say hello @{session:ffork}".to_string();
        storage
            .update_playbook(
                "fowner",
                "# Todo\n\n- said hello, done @{session:ffork}\n".to_string(),
                construct_protocol::PlaybookUpdateActor::Agent,
                None,
                None,
                None,
            )
            .expect("seed playbook");
        mgr.start_playbook_run_with_dispatch_state(
            "fowner",
            "- said hello, done @{session:ffork}",
            true,
            None,
            false,
            None,
        );
        let before = mgr.playbook_run_snapshot("fowner").expect("run seeded");
        assert!(
            !before.pending_block_refs.is_empty(),
            "the dispatched block starts pending"
        );

        mgr.run_fork_dispatches.lock().unwrap().insert(
            "ffork".to_string(),
            RunForkDispatch {
                owner_session_id: "fowner".to_string(),
                anchor,
            },
        );

        mgr.settle_run_fork_dispatch("ffork").await;

        let after = mgr.playbook_run_snapshot("fowner");
        assert!(
            after
                .as_ref()
                .map(|run| run.pending_block_refs.is_empty())
                .unwrap_or(true),
            "closing the fork settles its dispatched block: {after:?}"
        );
        assert!(
            mgr.run_fork_dispatches
                .lock()
                .unwrap()
                .get("ffork")
                .is_none(),
            "the tracking entry is consumed so a later delete cannot double-settle"
        );
    }

    #[tokio::test]
    async fn move_session_ignores_hidden_subagents_in_reorder_region() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Hidden subagents can share the same ungrouped/group region as visible
        // user sessions. Reordering must use visible user-session neighbors,
        // not these hidden records, otherwise a TUI row can appear not to move.
        for (id, kind, position) in [
            ("ssub-before", construct_protocol::SessionKind::Subagent, 0),
            ("suser-a", construct_protocol::SessionKind::User, 10),
            (
                "ssub-between",
                construct_protocol::SessionKind::Subagent,
                20,
            ),
            ("suser-b", construct_protocol::SessionKind::User, 30),
            ("ssub-after", construct_protocol::SessionKind::Subagent, 40),
        ] {
            mgr.sessions
                .write()
                .await
                .insert(id.into(), synthetic_entry(id, kind, position));
        }

        mgr.move_session("suser-b", construct_protocol::MoveDirection::Up)
            .await
            .expect("move up");

        let sessions = mgr.list().await;
        let a = sessions.iter().find(|s| s.id == "suser-a").unwrap();
        let b = sessions.iter().find(|s| s.id == "suser-b").unwrap();
        let hidden = sessions.iter().find(|s| s.id == "ssub-between").unwrap();
        assert_eq!(b.position, 10);
        assert_eq!(a.position, 30);
        assert_eq!(hidden.position, 20);
    }

    #[tokio::test]
    async fn move_session_reorders_fork_among_sibling_forks() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // A fork shares group_id with its parent, so the flat reorder region
        // (same group_id, user-kind, same archive partition) can put an
        // unrelated top-level session between two sibling forks by position
        // — e.g. "other" landed at 12 after some earlier reorder. Swapping by
        // flat-region neighbor would swap fork-b with "other" (12 <-> 13),
        // which doesn't cross fork-a and so looks like a no-op in the
        // fork-nested tree view, while silently perturbing "other"'s
        // position. Reordering must instead use sibling-fork neighbors.
        mgr.sessions.write().await.insert(
            "fparent".into(),
            synthetic_entry("fparent", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "ffork-a".into(),
            synthetic_fork_entry("ffork-a", "fparent", 11, None).await,
        );
        mgr.sessions.write().await.insert(
            "fother".into(),
            synthetic_entry("fother", construct_protocol::SessionKind::User, 12),
        );
        mgr.sessions.write().await.insert(
            "ffork-b".into(),
            synthetic_fork_entry("ffork-b", "fparent", 13, None).await,
        );

        let moved = mgr
            .move_session("ffork-b", construct_protocol::MoveDirection::Up)
            .await
            .expect("move up");
        assert!(moved, "swapping with a sibling fork is a real move");

        let sessions = mgr.list().await;
        let parent = sessions.iter().find(|s| s.id == "fparent").unwrap();
        let fork_a = sessions.iter().find(|s| s.id == "ffork-a").unwrap();
        let fork_b = sessions.iter().find(|s| s.id == "ffork-b").unwrap();
        let other = sessions.iter().find(|s| s.id == "fother").unwrap();
        assert_eq!(parent.position, 10, "unrelated parent must not move");
        assert_eq!(fork_b.position, 11, "fork-b swaps with sibling fork-a");
        assert_eq!(fork_a.position, 13);
        assert_eq!(other.position, 12, "unrelated top-level session untouched");
    }

    #[tokio::test]
    async fn move_session_fork_is_noop_at_sibling_edge() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // A fork with no sibling forks (only child of its parent) has no
        // sibling to swap with. It must not fall back to swapping with its
        // parent's flat-region neighbor — forks don't cross into other
        // regions the way top-level sessions do.
        mgr.sessions.write().await.insert(
            "gparent".into(),
            synthetic_entry("gparent", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "gfork".into(),
            synthetic_fork_entry("gfork", "gparent", 11, None).await,
        );

        let moved = mgr
            .move_session("gfork", construct_protocol::MoveDirection::Up)
            .await
            .expect("move up");
        assert!(!moved, "no sibling fork to swap with — reports as a no-op");

        let sessions = mgr.list().await;
        let parent = sessions.iter().find(|s| s.id == "gparent").unwrap();
        let fork = sessions.iter().find(|s| s.id == "gfork").unwrap();
        assert_eq!(parent.position, 10);
        assert_eq!(fork.position, 11);
    }

    /// A user-kind entry whose title marks it as routed to an operator's
    /// channel, mirroring how operator ingress titles the sessions it opens.
    async fn synthetic_routed_entry(id: &str, operator: &str, position: i64) -> Arc<SessionEntry> {
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, position);
        entry.summary.write().await.title = Some(format!("operator:{operator}:chan"));
        entry
    }

    #[tokio::test]
    async fn move_session_skips_operator_routed_sessions_in_flat_region() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Routed sessions share the flat region's group_id but render nested
        // under their operator's row, never as flat rows. Moving `ruser-b` up
        // must swap with the visible `ruser-a` in one press, not silently
        // swap with the invisible routed session sitting between them.
        mgr.sessions.write().await.insert(
            "ruser-a".into(),
            synthetic_entry("ruser-a", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "rrouted".into(),
            synthetic_routed_entry("rrouted", "helpdesk", 20).await,
        );
        mgr.sessions.write().await.insert(
            "ruser-b".into(),
            synthetic_entry("ruser-b", construct_protocol::SessionKind::User, 30),
        );

        let moved = mgr
            .move_session_with_operator_names(
                "ruser-b",
                construct_protocol::MoveDirection::Up,
                &["helpdesk".to_string()],
            )
            .await
            .expect("move up");
        assert!(moved);

        let sessions = mgr.list().await;
        let a = sessions.iter().find(|s| s.id == "ruser-a").unwrap();
        let b = sessions.iter().find(|s| s.id == "ruser-b").unwrap();
        let routed = sessions.iter().find(|s| s.id == "rrouted").unwrap();
        assert_eq!(b.position, 10, "one press crosses the hidden routed row");
        assert_eq!(a.position, 30);
        assert_eq!(routed.position, 20, "hidden routed session untouched");
    }

    #[tokio::test]
    async fn move_session_reorders_routed_session_among_same_operator_siblings() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // A routed session renders under its operator's row, ordered only
        // among sessions routed to the same operator. Its flat-region
        // neighbors by position — an unrelated top-level session and a
        // session routed to a different operator — must not be its swap
        // partners.
        mgr.sessions.write().await.insert(
            "qrouted-a".into(),
            synthetic_routed_entry("qrouted-a", "helpdesk", 10).await,
        );
        mgr.sessions.write().await.insert(
            "qflat".into(),
            synthetic_entry("qflat", construct_protocol::SessionKind::User, 15),
        );
        mgr.sessions.write().await.insert(
            "qother-op".into(),
            synthetic_routed_entry("qother-op", "triage", 17).await,
        );
        mgr.sessions.write().await.insert(
            "qrouted-b".into(),
            synthetic_routed_entry("qrouted-b", "helpdesk", 20).await,
        );

        let names = vec!["helpdesk".to_string(), "triage".to_string()];
        let moved = mgr
            .move_session_with_operator_names(
                "qrouted-b",
                construct_protocol::MoveDirection::Up,
                &names,
            )
            .await
            .expect("move up");
        assert!(moved, "swapping with a same-operator sibling is a real move");

        let sessions = mgr.list().await;
        let a = sessions.iter().find(|s| s.id == "qrouted-a").unwrap();
        let b = sessions.iter().find(|s| s.id == "qrouted-b").unwrap();
        let flat = sessions.iter().find(|s| s.id == "qflat").unwrap();
        let other = sessions.iter().find(|s| s.id == "qother-op").unwrap();
        assert_eq!(b.position, 10, "routed-b swaps with same-operator sibling");
        assert_eq!(a.position, 20);
        assert_eq!(flat.position, 15, "unrelated flat session untouched");
        assert_eq!(other.position, 17, "other operator's session untouched");
    }

    #[tokio::test]
    async fn move_session_routed_session_is_noop_at_sibling_edge() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // The only session routed to its operator has no sibling to swap
        // with. It must not fall back to swapping with a flat-region
        // neighbor — routing follows the title, so the row can't leave its
        // operator's cluster by reordering.
        mgr.sessions.write().await.insert(
            "euser-a".into(),
            synthetic_entry("euser-a", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "erouted".into(),
            synthetic_routed_entry("erouted", "helpdesk", 20).await,
        );
        mgr.sessions.write().await.insert(
            "euser-b".into(),
            synthetic_entry("euser-b", construct_protocol::SessionKind::User, 30),
        );

        for dir in [
            construct_protocol::MoveDirection::Up,
            construct_protocol::MoveDirection::Down,
        ] {
            let moved = mgr
                .move_session_with_operator_names("erouted", dir, &["helpdesk".to_string()])
                .await
                .expect("move");
            assert!(!moved, "no same-operator sibling — reports as a no-op");
        }

        let sessions = mgr.list().await;
        assert_eq!(
            sessions.iter().find(|s| s.id == "euser-a").unwrap().position,
            10
        );
        assert_eq!(
            sessions.iter().find(|s| s.id == "erouted").unwrap().position,
            20
        );
        assert_eq!(
            sessions.iter().find(|s| s.id == "euser-b").unwrap().position,
            30
        );
    }

    #[tokio::test]
    async fn move_session_treats_unmatched_operator_title_as_flat() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // A title that merely looks routed ("operator:ghost:...") stays a
        // flat row in every client when no operator named "ghost" exists, so
        // reordering must treat it as an ordinary flat-region member.
        mgr.sessions.write().await.insert(
            "tuser-a".into(),
            synthetic_entry("tuser-a", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "tghost".into(),
            synthetic_routed_entry("tghost", "ghost", 20).await,
        );

        let moved = mgr
            .move_session_with_operator_names(
                "tghost",
                construct_protocol::MoveDirection::Up,
                &["helpdesk".to_string()],
            )
            .await
            .expect("move up");
        assert!(moved, "unmatched routed-looking title reorders as flat");

        let sessions = mgr.list().await;
        assert_eq!(
            sessions.iter().find(|s| s.id == "tghost").unwrap().position,
            10
        );
        assert_eq!(
            sessions.iter().find(|s| s.id == "tuser-a").unwrap().position,
            20
        );
    }

    #[tokio::test]
    async fn move_session_ignores_hidden_archived_sessions_in_reorder_region() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // The TUI shows active sessions directly but puts archived ones behind
        // an initially-collapsed disclosure row. Moving `suser-b` up must swap
        // with the visible `suser-a`, not only with invisible `sarchived`.
        let active_a = synthetic_entry("suser-a", construct_protocol::SessionKind::User, 0);
        let archived = synthetic_entry("sarchived", construct_protocol::SessionKind::User, 10);
        archived.summary.write().await.archived = true;
        let active_b = synthetic_entry("suser-b", construct_protocol::SessionKind::User, 20);
        for (id, entry) in [
            ("suser-a", active_a),
            ("sarchived", archived),
            ("suser-b", active_b),
        ] {
            mgr.sessions.write().await.insert(id.into(), entry);
        }

        mgr.move_session("suser-b", construct_protocol::MoveDirection::Up)
            .await
            .expect("move up");

        let sessions = mgr.list().await;
        let a = sessions.iter().find(|s| s.id == "suser-a").unwrap();
        let archived = sessions.iter().find(|s| s.id == "sarchived").unwrap();
        let b = sessions.iter().find(|s| s.id == "suser-b").unwrap();
        assert_eq!(b.position, 0);
        assert_eq!(a.position, 20);
        assert_eq!(archived.position, 10);
    }

    #[tokio::test]
    async fn move_session_ignores_forks_of_other_sessions_in_reorder_region() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Forks are user-kind and share their parent's group_id, but the TUI
        // nests them under their fork parent instead of rendering them at
        // their flat position. Fork placement puts them right after their
        // parent, so a session below the fork cluster ("huser-x") that moves
        // up must swap with the visible parent in ONE step — not burn a press
        // per invisible fork row in between.
        mgr.sessions.write().await.insert(
            "hparent".into(),
            synthetic_entry("hparent", construct_protocol::SessionKind::User, 10),
        );
        mgr.sessions.write().await.insert(
            "hfork-a".into(),
            synthetic_fork_entry("hfork-a", "hparent", 11, None).await,
        );
        mgr.sessions.write().await.insert(
            "hfork-b".into(),
            synthetic_fork_entry("hfork-b", "hparent", 12, None).await,
        );
        mgr.sessions.write().await.insert(
            "huser-x".into(),
            synthetic_entry("huser-x", construct_protocol::SessionKind::User, 20),
        );

        let moved = mgr
            .move_session("huser-x", construct_protocol::MoveDirection::Up)
            .await
            .expect("move up");
        assert!(moved, "crossing the fork-parent is a real move");

        let sessions = mgr.list().await;
        let parent = sessions.iter().find(|s| s.id == "hparent").unwrap();
        let fork_a = sessions.iter().find(|s| s.id == "hfork-a").unwrap();
        let fork_b = sessions.iter().find(|s| s.id == "hfork-b").unwrap();
        let x = sessions.iter().find(|s| s.id == "huser-x").unwrap();
        assert_eq!(x.position, 10, "one press crosses the visible fork-parent");
        assert_eq!(parent.position, 20);
        assert_eq!(fork_a.position, 11, "hidden fork rows must not move");
        assert_eq!(fork_b.position, 12, "hidden fork rows must not move");
    }

    /// Spec 0073: with a painted background reported, the daemon strips
    /// child OSC 11 probes from the persisted/broadcast stream (so no
    /// attached terminal answers a second time); with no report, the probe
    /// passes through untouched.
    #[tokio::test]
    async fn painted_background_strips_osc11_probe_from_stream() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        mgr.sessions.write().await.insert(
            "s1".into(),
            synthetic_entry("s1", construct_protocol::SessionKind::User, 0),
        );
        let entry = mgr.get_entry("s1").await.expect("entry");

        // No painted background reported: the probe passes through.
        mgr.handle_event(&entry, SessionEvent::pty(b"a\x1b]11;?\x07b"))
            .await;
        assert_eq!(
            storage.read_pty_tail("s1", 64).expect("pty tail"),
            b"a\x1b]11;?\x07b",
            "without a report the probe must pass through untouched",
        );

        // A painted background is reported: the probe is stripped.
        mgr.set_terminal_background(7, Some([0x0c, 0x12, 0x1b]));
        let mut rx = mgr.subscribe();
        mgr.handle_event(&entry, SessionEvent::pty(b"c\x1b]11;?\x07d"))
            .await;
        assert_eq!(
            storage.read_pty_tail("s1", 64).expect("pty tail"),
            b"a\x1b]11;?\x07bcd",
            "with a painted background the probe must not reach the pty log",
        );
        let broadcast_bytes = loop {
            match rx.try_recv().expect("broadcast event") {
                BroadcastMsg::Event(p) => {
                    if let Some(bytes) = p.event.pty_bytes() {
                        break bytes;
                    }
                }
                _ => continue,
            }
        };
        assert_eq!(
            broadcast_bytes, b"cd",
            "clients must receive the stripped stream",
        );

        // The reporting connection goes away: probes pass through again.
        mgr.clear_conn(7);
        assert_eq!(mgr.effective_terminal_background(), None);
    }

    /// Spec 0073: the effective background is the most recent report among
    /// live connections; a later "none" report (background-aware theme)
    /// overrides an older painted one.
    #[tokio::test]
    async fn effective_terminal_background_is_latest_report() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        assert_eq!(mgr.effective_terminal_background(), None);
        mgr.set_terminal_background(1, Some([1, 2, 3]));
        assert_eq!(mgr.effective_terminal_background(), Some([1, 2, 3]));
        mgr.set_terminal_background(2, None);
        assert_eq!(
            mgr.effective_terminal_background(),
            None,
            "the most recent reporter wins even when it reports none",
        );
        mgr.set_terminal_background(1, Some([4, 5, 6]));
        assert_eq!(mgr.effective_terminal_background(), Some([4, 5, 6]));
        mgr.clear_conn(1);
        assert_eq!(
            mgr.effective_terminal_background(),
            None,
            "conn 2's `none` remains after the painted reporter disconnects",
        );
    }

    #[tokio::test]
    async fn archive_marks_terminal_and_keeps_session() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "s1".into(),
            synthetic_entry("s1", construct_protocol::SessionKind::User, 0),
        );

        // Clients reflect archived state from the broadcast `State` event, so
        // the emitted event — not just the in-memory/persisted summary — must
        // carry `archived = true`. Subscribe before archiving so we see it.
        let mut rx = mgr.subscribe();

        mgr.archive("s1").await.expect("archive");

        let entry = mgr.get_entry("s1").await.expect("entry still present");
        {
            let s = entry.summary.read().await;
            assert!(s.archived, "session should be marked archived");
            assert!(
                s.state.is_terminal(),
                "a running session should read as terminated after archive",
            );
        }
        // Archived sessions stay in the manager (unlike delete) so they can be
        // listed when the toggle is on and later restarted.
        assert!(
            mgr.list().await.iter().any(|s| s.id == "s1"),
            "archived session must remain in the manager",
        );
        // The persisted meta.json carries the archived flag across restarts.
        let persisted = mgr.storage.load_summary("s1").expect("load meta");
        assert!(persisted.archived, "archived flag must be persisted");

        // The broadcast `State` event for s1 must report archived + terminal,
        // so the first archive action makes the row jump to the archived group
        // in every client without a second action.
        let mut saw_archived_event = false;
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::State(p) = msg {
                if p.session.id == "s1" {
                    assert!(
                        p.session.archived,
                        "broadcast State for s1 must carry archived = true",
                    );
                    assert!(
                        p.session.state.is_terminal(),
                        "broadcast State for s1 must report a terminal state",
                    );
                    saw_archived_event = true;
                }
            }
        }
        assert!(
            saw_archived_event,
            "archive must broadcast a State event for the session",
        );
    }

    /// The TUI's lineage preview carves a node's own timeline into activity
    /// segments using `ForkedFrom::transcript_seq` and `ForkMerge::
    /// merged_seq` as matching checkpoints on the SAME counter
    /// (`SessionSummary::event_count`) — so `merge()` must stamp
    /// `merged_seq` from the PARENT's event_count at the moment of the
    /// merge, not the fork's own.
    #[tokio::test]
    async fn merge_stamps_merged_seq_from_the_parents_current_event_count() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let parent = synthetic_entry("parent", construct_protocol::SessionKind::User, 0);
        {
            let mut s = parent.summary.write().await;
            s.event_count = 17;
            s.busy_ms = 7_500;
            s.message_count = 6;
            s.tokens = construct_protocol::TokenTally {
                input: 12_000,
                output: 800,
                cached: 9_000,
            };
        }
        mgr.sessions.write().await.insert("parent".into(), parent);

        let fork = synthetic_entry("fork", construct_protocol::SessionKind::User, 10);
        fork.summary.write().await.forked_from = Some(construct_protocol::ForkedFrom {
            session_id: "parent".into(),
            transcript_seq: 5,
            at_ms: 0,
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: false,
        });
        fork.summary.write().await.event_count = 2;
        mgr.sessions.write().await.insert("fork".into(), fork);

        mgr.merge("fork", construct_protocol::ForkMergeMode::Result)
            .await
            .expect("merge");

        let fork_summary = mgr
            .get_entry("fork")
            .await
            .expect("fork entry")
            .summary()
            .await;
        let merge = fork_summary.merge.expect("merge outcome recorded");
        assert_eq!(merge.mode, construct_protocol::ForkMergeMode::Result);
        assert_eq!(
            merge.merged_seq, 17,
            "merged_seq must come from the PARENT's event_count, not the fork's own"
        );
        assert_eq!(
            merge.merged_busy_ms, 7_500,
            "the merge boundary snapshots the PARENT's accumulated compute time \
             so lineage turn-info windows can label busy deltas"
        );
        assert_eq!(
            merge.merged_message_count, 6,
            "the merge boundary snapshots the PARENT's chat-message tally \
             so lineage turn-info windows can count actual messages"
        );
        assert_eq!(
            merge.merged_tokens,
            construct_protocol::TokenTally {
                input: 12_000,
                output: 800,
                cached: 9_000,
            },
            "the merge boundary snapshots the PARENT's token tally so lineage \
             turn-info windows can label token deltas (spec 0103)"
        );

        // The parent's own event_count is untouched by merge() itself — the
        // caller injects the result message through the parent's ordinary
        // input path first (spec 0078), which is what actually advances it;
        // merge() only records the outcome/checkpoint.
        let parent_summary = mgr
            .get_entry("parent")
            .await
            .expect("parent entry")
            .summary()
            .await;
        assert_eq!(parent_summary.event_count, 17);
    }

    #[tokio::test]
    async fn merge_with_a_gone_parent_falls_back_to_zero_merged_seq() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // The fork's parent was deleted before the fork got merged/discarded
        // — merge() must still succeed (there's still a terminal outcome to
        // record for the fork itself), just with no parent timeline to stamp.
        let fork = synthetic_entry("orphan-fork", construct_protocol::SessionKind::User, 0);
        fork.summary.write().await.forked_from = Some(construct_protocol::ForkedFrom {
            session_id: "long-gone".into(),
            transcript_seq: 3,
            at_ms: 0,
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: false,
        });
        mgr.sessions
            .write()
            .await
            .insert("orphan-fork".into(), fork);

        mgr.merge("orphan-fork", construct_protocol::ForkMergeMode::Discard)
            .await
            .expect("merge succeeds even with no parent entry left");

        let summary = mgr
            .get_entry("orphan-fork")
            .await
            .expect("entry")
            .summary()
            .await;
        assert_eq!(summary.merge.expect("merge recorded").merged_seq, 0);
    }

    #[tokio::test]
    async fn merge_on_a_non_fork_session_fails() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "solo".into(),
            synthetic_entry("solo", construct_protocol::SessionKind::User, 0),
        );

        assert!(
            mgr.merge("solo", construct_protocol::ForkMergeMode::Result)
                .await
                .is_err(),
            "an ordinary session with no forked_from must not be mergeable"
        );
    }

    /// Regression for the "archive only closes the session, needs a second
    /// archive" bug. `archive` terminates the live adapter, which makes the
    /// `drain_adapter` Closed handler fire on a separate task and race the
    /// archive bookkeeping. If that handler re-persists/re-broadcasts the
    /// session with `archived = false`, the row shows up as merely stopped and
    /// the user has to archive again.
    ///
    /// This drives the handler directly with the archive *intent* recorded
    /// (the `entry.archived` flag set, but the summary not yet flipped — the
    /// exact window where the Closed event clones a stale, unarchived summary)
    /// and asserts the handler keeps the session archived in both the persisted
    /// meta and the emitted event.
    #[tokio::test]
    async fn drain_close_after_archive_intent_keeps_archived() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        // A still-running session whose summary has NOT yet been flipped to
        // archived — modelling the Closed handler observing the summary before
        // `archive` writes `archived = true` to it.
        let entry = synthetic_entry("s_race", construct_protocol::SessionKind::User, 0);
        {
            let mut s = entry.summary.write().await;
            s.state = SessionState::Running;
            s.archived = false;
        }
        manager
            .sessions
            .write()
            .await
            .insert("s_race".into(), entry.clone());

        // `archive` records its intent on the entry *before* terminating the
        // adapter; reproduce just that step.
        entry.archived.store(true, Ordering::SeqCst);

        let mut rx = manager.subscribe();

        // Drive the drain loop with a single Closed message (what the
        // terminated adapter emits), then let it run to completion.
        let (tx, drain_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);
        tx.send(AdapterMessage::Closed { exit_code: Some(0) })
            .await
            .expect("send Closed");
        drop(tx);
        manager.clone().drain_adapter(entry.clone(), drain_rx).await;

        // In-memory summary stays archived.
        assert!(
            entry.summary.read().await.archived,
            "Closed handler must not clear archived when archive intent is set",
        );
        // Persisted meta stays archived.
        let persisted = manager.storage.load_summary("s_race").expect("load meta");
        assert!(
            persisted.archived,
            "persisted meta must keep archived = true after the Closed race",
        );
        // The broadcast event stays archived, so no client downgrades the row
        // back to a plain stopped session.
        let mut saw_archived_event = false;
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::State(p) = msg {
                if p.session.id == "s_race" {
                    assert!(
                        p.session.archived,
                        "broadcast State from the Closed handler must keep archived = true",
                    );
                    saw_archived_event = true;
                }
            }
        }
        assert!(
            saw_archived_event,
            "Closed handler must broadcast a State event",
        );
    }

    async fn insert_group(mgr: &SessionManager, id: &str, position: i64, collapsed: bool) {
        use chrono::Utc;
        use tokio::sync::RwLock;
        mgr.groups.write().await.insert(
            id.into(),
            Arc::new(GroupEntry {
                summary: RwLock::new(GroupSummary {
                    id: id.into(),
                    name: id.into(),
                    created_at: Utc::now(),
                    position,
                    collapsed,
                }),
            }),
        );
    }

    #[tokio::test]
    async fn move_session_jumps_over_collapsed_project() {
        use construct_protocol::{MoveDirection, SessionKind};
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Display order: ungrouped (su-top, su-mover), then collapsed group
        // `gcol` (members hidden), then expanded group `gexp`.
        insert_group(&mgr, "gcol", 0, true).await;
        insert_group(&mgr, "gexp", 1, false).await;
        for (id, position, group) in [
            ("su-top", 0, None),
            ("su-mover", 10, None),
            ("gc-1", 0, Some("gcol".to_string())),
            ("gc-2", 1, Some("gcol".to_string())),
            ("ge-1", 0, Some("gexp".to_string())),
        ] {
            mgr.sessions.write().await.insert(
                id.into(),
                synthetic_entry_with_group(id, SessionKind::User, position, group),
            );
        }

        // Moving down past the collapsed group jumps the whole project in one
        // step: the session skips `gcol`'s hidden members and lands at the top
        // of the next visible region (`gexp`) without interleaving with them.
        mgr.move_session("su-mover", MoveDirection::Down)
            .await
            .expect("move down");

        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "su-mover").unwrap();
        assert_eq!(mover.group_id.as_deref(), Some("gexp"));
        assert!(mover.position < 0, "should land above ge-1 (pos 0)");
        // The collapsed project's members are untouched.
        let gc1 = sessions.iter().find(|s| s.id == "gc-1").unwrap();
        let gc2 = sessions.iter().find(|s| s.id == "gc-2").unwrap();
        assert_eq!(gc1.group_id.as_deref(), Some("gcol"));
        assert_eq!(gc2.group_id.as_deref(), Some("gcol"));
        assert_eq!(gc1.position, 0);
        assert_eq!(gc2.position, 1);

        // Moving back up jumps the collapsed group the other way, returning the
        // session to the bottom of the ungrouped region.
        mgr.move_session("su-mover", MoveDirection::Up)
            .await
            .expect("move up");
        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "su-mover").unwrap();
        assert_eq!(mover.group_id, None);
        assert!(mover.position > 0, "should land below su-top (pos 0)");
    }

    #[tokio::test]
    async fn move_session_down_expands_the_only_collapsed_region_below() {
        use construct_protocol::{MoveDirection, SessionKind};
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // The steady state of a working fleet: one expanded project on top,
        // every project below it collapsed. Nothing the skip can land on.
        insert_group(&mgr, "gexp", 0, false).await;
        insert_group(&mgr, "gcol-1", 1, true).await;
        insert_group(&mgr, "gcol-2", 2, true).await;
        for (id, position, group) in [
            ("ge-1", 0, Some("gexp".to_string())),
            ("ge-mover", 1, Some("gexp".to_string())),
            ("gc1-1", 0, Some("gcol-1".to_string())),
            ("gc2-1", 0, Some("gcol-2".to_string())),
        ] {
            mgr.sessions.write().await.insert(
                id.into(),
                synthetic_entry_with_group(id, SessionKind::User, position, group),
            );
        }

        // Moving down from the bottom of `gexp` must not be a no-op just
        // because every region below is collapsed: two project rows are
        // plainly visible below the session, so refusing looks like a broken
        // key. Land in the nearest one and expand it so the session stays
        // where the user dropped it.
        let moved = mgr
            .move_session("ge-mover", MoveDirection::Down)
            .await
            .expect("move down");
        assert!(moved, "move down must report a real reorder");

        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "ge-mover").unwrap();
        assert_eq!(mover.group_id.as_deref(), Some("gcol-1"));
        assert!(mover.position < 0, "should land above gc1-1 (pos 0)");

        let groups = mgr.list_groups().await;
        let gcol1 = groups.iter().find(|g| g.id == "gcol-1").unwrap();
        assert!(
            !gcol1.collapsed,
            "the entered project must expand so the moved session stays visible",
        );
        // Only the project the session entered expands; the rest keep the
        // collapse state the user chose.
        let gcol2 = groups.iter().find(|g| g.id == "gcol-2").unwrap();
        assert!(gcol2.collapsed, "untouched projects stay collapsed");
    }

    #[tokio::test]
    async fn move_session_down_stops_at_the_last_project() {
        use construct_protocol::{MoveDirection, SessionKind};
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        insert_group(&mgr, "glast", 0, false).await;
        for (id, position, group) in [
            ("gl-1", 0, Some("glast".to_string())),
            ("gl-mover", 1, Some("glast".to_string())),
        ] {
            mgr.sessions.write().await.insert(
                id.into(),
                synthetic_entry_with_group(id, SessionKind::User, position, group),
            );
        }

        // The last member of the last project is the true bottom of the list:
        // there is no region below at all, collapsed or otherwise, so the
        // boundary still reports "nothing to reorder past".
        let moved = mgr
            .move_session("gl-mover", MoveDirection::Down)
            .await
            .expect("move down");
        assert!(!moved, "the absolute bottom is still a no-op");

        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "gl-mover").unwrap();
        assert_eq!(mover.group_id.as_deref(), Some("glast"));
        assert_eq!(mover.position, 1);
    }

    #[tokio::test]
    async fn install_memory_env_sets_global_and_project_paths() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut env = HashMap::new();

        mgr.install_memory_env(&mut env, Some("g123"));

        assert_eq!(
            env.get(ENV_GLOBAL_MEMORY_FILE),
            Some(&storage.global_memory_path().to_string_lossy().to_string())
        );
        assert_eq!(
            env.get(ENV_PROJECT_MEMORY_FILE),
            Some(
                &storage
                    .project_memory_path("g123")
                    .to_string_lossy()
                    .to_string()
            )
        );
        assert_eq!(env.get(ENV_PROJECT_ID).map(String::as_str), Some("g123"));
    }

    #[tokio::test]
    async fn install_memory_env_ungrouped_sets_global_only() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut env = HashMap::from([
            (ENV_PROJECT_ID.to_string(), "old".to_string()),
            (
                ENV_PROJECT_MEMORY_FILE.to_string(),
                "/old/memory.md".to_string(),
            ),
        ]);

        mgr.install_memory_env(&mut env, None);

        assert!(env.contains_key(ENV_GLOBAL_MEMORY_FILE));
        assert!(!env.contains_key(ENV_PROJECT_MEMORY_FILE));
        assert!(!env.contains_key(ENV_PROJECT_ID));
    }

    #[tokio::test]
    async fn playbook_run_context_env_points_at_session_sidecar() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut env = HashMap::new();

        mgr.install_playbook_run_context_env(&mut env, "s123");

        assert_eq!(
            env.get(agent_context::ENV_PLAYBOOK_RUN_CONTEXT_FILE),
            Some(
                &storage
                    .session_dir("s123")
                    .join("playbook-run-context.json")
                    .to_string_lossy()
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn write_playbook_run_context_persists_readable_json() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let context = agent_context::PlaybookRunContext {
            session_id: "s123".to_string(),
            playbook_version: 1,
            playbook_updated_at_ms: 2,
            scope: "full".to_string(),
            instructions: vec!["run".to_string()],
            smart_clips: Vec::new(),
            markdown: "# Playbook".to_string(),
        };

        mgr.write_playbook_run_context("s123", &context)
            .expect("write playbook run context");

        let bytes = std::fs::read(mgr.playbook_run_context_path("s123")).unwrap();
        let parsed: agent_context::PlaybookRunContext = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, context);
    }

    #[test]
    fn playbook_run_instructions_require_atomic_move_edits() {
        let instructions = playbook_run_instructions().join("\n");

        assert!(
            instructions
                .contains("one construct_playbook_edit call containing multiple `edits` entries"),
            "playbook runs should tell agents to move blocks in one edit call"
        );
        assert!(
            instructions.contains("viewers can briefly see the block disappear"),
            "instruction should explain the transient Playbook-view failure mode"
        );
    }

    #[tokio::test]
    async fn attach_clipboard_writes_session_attachment_and_reference() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        mgr.sessions.write().await.insert(
            "spaste".into(),
            synthetic_entry("spaste", construct_protocol::SessionKind::User, 0),
        );

        let result = mgr
            .attach_clipboard(SessionAttachClipboardParams {
                session_id: "spaste".into(),
                data: base64::engine::general_purpose::STANDARD.encode(b"hello paste"),
                filename: Some("../../screen shot.png".into()),
                mime: Some("image/png".into()),
            })
            .await
            .expect("attach clipboard");

        assert!(result.reference.starts_with("[#file:"));
        assert!(result.reference.ends_with(']'));
        assert!(result.path.contains("/sessions/spaste/attachments/"));
        assert!(result.path.ends_with(".png"));
        assert_eq!(
            tokio::fs::read(&result.path)
                .await
                .expect("read attachment"),
            b"hello paste"
        );

        // Read-back (spec 0099): the stored file round-trips through
        // session.read_attachment with its extension-derived MIME…
        let filename = std::path::Path::new(&result.path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let read = mgr
            .read_attachment(construct_protocol::SessionReadAttachmentParams {
                session_id: "spaste".into(),
                filename,
            })
            .await
            .expect("read attachment back");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&read.data)
                .unwrap(),
            b"hello paste"
        );
        assert_eq!(read.mime, "image/png");
        // …and traversal / separator names are rejected outright.
        for bad in ["../../etc/passwd", "a/b.png", "..", "", "a\\b.png"] {
            assert!(
                mgr.read_attachment(construct_protocol::SessionReadAttachmentParams {
                    session_id: "spaste".into(),
                    filename: bad.into(),
                })
                .await
                .is_err(),
                "filename {bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn list_includes_subagent_sessions_for_clients_to_nest() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "suser".into(),
            synthetic_entry("suser", construct_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "ssub".into(),
            synthetic_entry("ssub", construct_protocol::SessionKind::Subagent, -1),
        );

        let sessions = mgr.list().await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions
            .iter()
            .any(|s| s.id == "suser" && s.kind == construct_protocol::SessionKind::User));
        assert!(sessions
            .iter()
            .any(|s| s.id == "ssub" && s.kind == construct_protocol::SessionKind::Subagent));
    }

    /// Browser previews are ephemeral, live-only UI (a base64 PNG shown as
    /// an overlay / matrix-rain wallpaper). They must NEVER reach the
    /// transcript: persisting full-size screenshots would bloat
    /// transcript.jsonl and slow every load (`read_transcript` parses each
    /// line), with no transcript consumer — clients render them only from
    /// the live broadcast. Normal structured events must still persist.
    #[tokio::test]
    async fn browser_preview_is_not_persisted_to_transcript() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sbrowser";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Control: a normal structured event MUST be persisted.
        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: construct_protocol::MessageRole::Assistant,
                text: "hi".into(),
            },
        )
        .await;

        // The browser preview (with a stand-in base64 image) MUST NOT be.
        mgr.handle_event(
            &entry,
            SessionEvent::BrowserPreview(construct_protocol::BrowserPreview {
                url: "https://example.test".into(),
                title: Some("Example".into()),
                image: "QUJD".into(), // base64("ABC")
                width: 2,
                height: 1,
            }),
        )
        .await;

        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::BrowserPreview(_))),
            "BrowserPreview must not be written to the transcript"
        );
        assert!(
            transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::Message { .. })),
            "control: a normal Message event should still be persisted"
        );
    }

    /// `ToolApprovalResolved` is a transient UI-dismissal signal: it must
    /// be broadcast live (so passive clients can close a stale approval
    /// prompt) but never written to the transcript — same treatment as
    /// `BrowserPreview` / `AgentStatus`.
    #[tokio::test]
    async fn tool_approval_resolved_is_not_persisted_to_transcript() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sresolved";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Control: a normal structured event MUST be persisted.
        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: construct_protocol::MessageRole::Assistant,
                text: "hi".into(),
            },
        )
        .await;

        // The transient approval-resolved signal MUST NOT be.
        mgr.handle_event(
            &entry,
            SessionEvent::ToolApprovalResolved {
                call_id: "call-1".into(),
            },
        )
        .await;

        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ToolApprovalResolved { .. })),
            "ToolApprovalResolved must not be written to the transcript"
        );
        assert!(
            transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::Message { .. })),
            "control: a normal Message event should still be persisted"
        );
    }

    /// Inline PTY approval prompts can change the approval mode locally
    /// inside the adapter (`a` / `f`). The adapter reports that state change
    /// back to the daemon with `ApprovalModeChanged`; the daemon must update
    /// the session summary so modelines and other clients stop showing the
    /// stale mode, without recording a transcript row.
    #[tokio::test]
    async fn approval_mode_changed_updates_summary_without_transcript_row() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sapprovalmode";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        mgr.handle_event(
            &entry,
            SessionEvent::ApprovalModeChanged {
                mode: construct_protocol::ApprovalMode::UnsafeAuto,
            },
        )
        .await;

        let summary = storage.load_summary(id).expect("summary");
        assert_eq!(
            summary.approval_mode,
            construct_protocol::ApprovalMode::UnsafeAuto
        );
        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ApprovalModeChanged { .. })),
            "ApprovalModeChanged must not be written to the transcript"
        );
    }

    /// A smith `/model` switch reports the new model to the daemon with
    /// `ModelChanged`. The daemon must record it on the session summary so the
    /// choice survives restart (`respawn` re-injects `summary.model`) and the
    /// UI label tracks it — without recording a transcript row.
    #[tokio::test]
    async fn model_changed_updates_summary_without_transcript_row() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "smodelchange";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        mgr.handle_event(
            &entry,
            SessionEvent::ModelChanged {
                model: "anthropic:claude-opus-4-8".into(),
            },
        )
        .await;

        let summary = storage.load_summary(id).expect("summary");
        assert_eq!(summary.model.as_deref(), Some("anthropic:claude-opus-4-8"));
        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ModelChanged { .. })),
            "ModelChanged must not be written to the transcript"
        );
    }

    /// spec 0085: a native-id-change event synthesizes a real, archived
    /// child session — "fork and archive it" — holding a copy of the live
    /// session's transcript up to that point, with its OWN native id file
    /// set to the id that's being retired. Firing it twice produces two
    /// SIBLING children (both `forked_from.session_id` pointing at the same
    /// live session), not one growing record.
    #[tokio::test]
    async fn native_id_changed_synthesizes_an_archived_fork_snapshot() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "snativeidchange";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        entry.summary.write().await.harness = "opencode".into();
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: construct_protocol::MessageRole::User,
                text: "hello".into(),
            },
        )
        .await;
        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: construct_protocol::MessageRole::Assistant,
                text: "hi".into(),
            },
        )
        .await;

        mgr.handle_event(
            &entry,
            SessionEvent::NativeIdChanged {
                prior_native_id: "native-a".into(),
                new_native_id: "native-b".into(),
            },
        )
        .await;

        // The live session itself is untouched by this handler.
        let live = storage.load_summary(id).expect("live summary");
        assert!(live.forked_from.is_none());
        assert_eq!(live.event_count, 2);

        let all = mgr.list().await;
        let children: Vec<_> = all
            .iter()
            .filter(|s| s.forked_from.as_ref().is_some_and(|f| f.session_id == id))
            .collect();
        assert_eq!(children.len(), 1, "exactly one archived snapshot so far");
        let child = children[0];
        assert!(child.archived, "snapshot is born archived");
        let forked_from = child.forked_from.as_ref().unwrap();
        assert!(forked_from.is_reset_snapshot);
        assert_eq!(forked_from.transcript_seq, 2);
        assert_eq!(child.event_count, 2);
        assert_eq!(child.title.as_deref(), Some("(cleared) opencode"));
        // Forking the snapshot must spawn the same way forking the live
        // session would (interactive PTY, not headless) — `has_pty`/`mode`
        // are carried from the live session, not hardcoded to headless
        // defaults (`Client::fork_session` decides interactive-vs-headless
        // purely from the fork source's own `has_pty`/`mode`; `synthetic_
        // entry`'s live fixture is `has_pty: true, mode: None`, which
        // already counts as "terminal" since only `mode == Some("headless")`
        // overrides it).
        assert!(
            child.has_pty,
            "snapshot must carry the live session's has_pty"
        );
        assert_eq!(child.mode, None);

        let child_native_id = std::fs::read_to_string(
            storage
                .session_dir(&child.id)
                .join("opencode_session_id.txt"),
        )
        .expect("child native id file");
        assert_eq!(child_native_id, "native-a");

        let child_transcript = storage
            .read_transcript(&child.id, 0, None)
            .expect("child transcript");
        assert_eq!(child_transcript.events.len(), 2, "transcript was copied");

        // A second reset must produce a SIBLING, not overwrite the first.
        mgr.handle_event(
            &entry,
            SessionEvent::NativeIdChanged {
                prior_native_id: "native-b".into(),
                new_native_id: "native-c".into(),
            },
        )
        .await;
        let all = mgr.list().await;
        let children: Vec<_> = all
            .iter()
            .filter(|s| s.forked_from.as_ref().is_some_and(|f| f.session_id == id))
            .collect();
        assert_eq!(
            children.len(),
            2,
            "two sibling snapshots, not one merged record"
        );
    }

    /// Regression for the post-#69 "all sessions go to `done` after
    /// graceful daemon restart" bug: when `shutdown_adapters` is in
    /// flight, any `SessionEvent::Done` or `AdapterMessage::Closed`
    /// that flushes out of a dying adapter must NOT transition the
    /// session to a terminal state. Otherwise `resume_running_sessions`
    /// on the next boot skips the session and the user has to restart
    /// it manually.
    #[tokio::test]
    async fn handle_event_preserves_state_during_shutdown() {
        use chrono::Utc;
        use std::sync::atomic::Ordering;
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        // Synthetic session in `Running` (what a live shell / smith
        // session looks like just before the user hits Ctrl-C on the
        // daemon).
        let id = "stest_shutdown".to_string();
        let summary = construct_protocol::SessionSummary {
            id: id.clone(),
            harness: "shell".into(),
            cwd: "/tmp".into(),
            title: None,
            auto_title_pending: false,
            state: SessionState::Running,
            created_at: Utc::now(),
            last_event_at: None,
            last_message_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count: 0,
            has_pty: true,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: Default::default(),
            context_used: None,
            context_window: None,
            context_segments: Vec::new(),
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind: construct_protocol::SessionKind::User,
            forked_from: None,
            merge: None,
            archived: false,
            minibuffer_loop_disabled: false,
            needs_attention: false,
        };
        let entry = Arc::new(SessionEntry {
            id: id.clone(),
            summary: RwLock::new(summary),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(0),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });
        manager
            .sessions
            .write()
            .await
            .insert(id.clone(), entry.clone());

        // Pre-shutdown: a `Done` event WOULD transition state.
        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Done,
            "sanity: without the shutdown flag, Done transitions state",
        );

        // Reset and flip the shutdown flag (what `shutdown_adapters`
        // does before sending SHUTDOWN to each adapter).
        entry.summary.write().await.state = SessionState::Running;
        manager.is_shutting_down.store(true, Ordering::Release);

        // Same `Done` event during shutdown must be dropped — the
        // session needs to keep its `Running` state on disk so the
        // next boot's `resume_running_sessions` picks it up.
        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Running,
            "Done during shutdown must NOT transition state — that's \
             the resume regression we're guarding against",
        );

        // Error events are the same shape and must also be dropped.
        manager
            .handle_event(
                &entry,
                SessionEvent::Error {
                    message: "adapter died".into(),
                },
            )
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Running,
            "Error during shutdown must NOT transition state either",
        );
    }

    /// Output-quiescence detection applies only to PTY full-screen TUI LLM
    /// harnesses; shells (foreground-pgroup) and headless sessions are excluded.
    #[test]
    fn quiescence_targets_tui_llm_harnesses() {
        let mut s = placement_summary("q", 0, None, construct_protocol::SessionKind::User);
        s.has_pty = true;
        for h in [
            "claude",
            "codex",
            "antigravity",
            "agy",
            "grok",
            "hermes",
            "kimi",
            "opencode",
            "pi",
            "prime-agent",
            "muse",
        ] {
            s.harness = h.into();
            assert!(harness_uses_quiescence(&s), "{h} should use quiescence");
        }
        s.harness = "shell".into();
        assert!(
            !harness_uses_quiescence(&s),
            "shell uses foreground-pgroup detection, not quiescence",
        );
        s.harness = "claude".into();
        s.has_pty = false;
        assert!(
            !harness_uses_quiescence(&s),
            "headless (no PTY) sessions emit their own AwaitingInput",
        );
    }

    #[tokio::test]
    async fn pty_activity_filtering_avoids_quiescence_reset() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let id = "stest_pty_activity".to_string();
        let summary = construct_protocol::SessionSummary {
            id: id.clone(),
            harness: "grok".into(),
            cwd: "/tmp".into(),
            title: None,
            auto_title_pending: false,
            state: SessionState::AwaitingInput,
            created_at: Utc::now(),
            last_event_at: None,
            last_message_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count: 0,
            has_pty: true,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: Default::default(),
            context_used: None,
            context_window: None,
            context_segments: Vec::new(),
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind: construct_protocol::SessionKind::User,
            archived: false,
            minibuffer_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        };
        let entry = Arc::new(SessionEntry {
            id: id.clone(),
            summary: RwLock::new(summary),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(0),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });
        manager
            .sessions
            .write()
            .await
            .insert(id.clone(), entry.clone());

        // 1. Send ignorable PTY event (synchronized update + SGR color style resets).
        manager
            .handle_event(
                &entry,
                SessionEvent::pty(b"\x1b[?2026h\x1b[39m\x1b[49m\x1b[59m\x1b[0m\x1b[?2026l"),
            )
            .await;

        let sum = entry.summary.read().await;
        assert_eq!(
            sum.state,
            SessionState::AwaitingInput,
            "should stay in AwaitingInput"
        );
        assert!(
            sum.last_pty_at_ms.is_none(),
            "should NOT update last_pty_at_ms"
        );
        drop(sum);

        // 2. Send active/visible PTY event (actual printable character). It
        // starts an output burst but is not yet sustained (PTY_BLIP_WINDOW),
        // so the state must NOT flip yet — only the activity timestamp moves.
        manager
            .handle_event(&entry, SessionEvent::pty(b"visible output"))
            .await;

        let sum = entry.summary.read().await;
        assert_eq!(
            sum.state,
            SessionState::AwaitingInput,
            "a fresh burst is still a potential housekeeping blip"
        );
        assert!(sum.last_pty_at_ms.is_some(), "should update last_pty_at_ms");
        drop(sum);

        // 3. Simulate the burst having persisted past PTY_BLIP_WINDOW (as a
        // real turn's continuous repaints do): backdate the burst start, keep
        // the last-output gap under PTY_QUIESCENCE so the burst is unbroken.
        let now_ms = Utc::now().timestamp_millis();
        entry.pty_burst_start_ms.store(
            now_ms - PTY_BLIP_WINDOW.as_millis() as i64 - 1_000,
            Ordering::Relaxed,
        );
        entry.summary.write().await.last_pty_at_ms = Some(now_ms - 100);
        manager
            .handle_event(&entry, SessionEvent::pty(b"more visible output"))
            .await;

        let sum = entry.summary.read().await;
        assert_eq!(
            sum.state,
            SessionState::Running,
            "sustained output transitions to Running"
        );
    }

    /// A paint→erase housekeeping blip (claude's periodic "Checking for
    /// updates") must never register as sustained activity; output that keeps
    /// arriving past PTY_BLIP_WINDOW must. Spec 0054.
    #[test]
    fn pty_burst_blips_are_not_sustained() {
        let t0: i64 = 1_000_000;
        // First output after long silence starts a burst; not sustained.
        let (start, sustained) = pty_burst_advance(0, None, t0);
        assert_eq!(start, t0);
        assert!(!sustained);
        // The erase repaint ~530ms later (observed updater cadence): same
        // burst, still a blip.
        let (start, sustained) = pty_burst_advance(start, Some(t0), t0 + 530);
        assert_eq!(start, t0);
        assert!(!sustained);
        // The next updater blip 30 minutes later: the gap ends the burst.
        let (start, sustained) = pty_burst_advance(start, Some(t0 + 530), t0 + 30 * 60 * 1_000);
        assert_eq!(start, t0 + 30 * 60 * 1_000);
        assert!(!sustained);
    }

    #[test]
    fn pty_burst_sustained_output_is_genuine() {
        let t0: i64 = 5_000_000;
        // A real turn: output keeps arriving with sub-quiescence gaps and
        // becomes genuine once the burst spans PTY_BLIP_WINDOW.
        let (start, sustained) = pty_burst_advance(0, Some(t0 - 60_000), t0);
        assert!(!sustained);
        let (start, sustained) = pty_burst_advance(start, Some(t0), t0 + 1_000);
        assert!(!sustained);
        let (start, sustained) = pty_burst_advance(start, Some(t0 + 1_000), t0 + 2_000);
        assert_eq!(start, t0, "the burst is unbroken");
        assert!(sustained, "a burst spanning PTY_BLIP_WINDOW is genuine");
    }

    /// A lone paint→erase pair can never qualify as sustained regardless of
    /// its spacing: a gap under PTY_QUIESCENCE keeps the pair one burst but
    /// spans less than PTY_BLIP_WINDOW; a wider gap starts a new burst.
    #[test]
    fn pty_burst_two_event_pair_never_sustained() {
        let t0: i64 = 1_000_000;
        for gap in [100, 500, 1_999, 2_000, 5_000, 30 * 60 * 1_000] {
            let (start, _) = pty_burst_advance(0, None, t0);
            let (_, sustained) = pty_burst_advance(start, Some(t0), t0 + gap);
            assert!(
                !sustained,
                "paint→erase pair with {gap}ms spacing must stay a blip"
            );
        }
    }

    /// The post-respawn settle window: never over before RESUME_SETTLE_MIN,
    /// over once output has been quiet for PTY_QUIESCENCE after that (stale
    /// pre-restart output or none at all counts as quiet), and always over
    /// at RESUME_SETTLE_MAX. Spec 0054.
    #[test]
    fn resume_settle_window_bounds() {
        let since: i64 = 10_000_000;
        let min = RESUME_SETTLE_MIN.as_millis() as i64;
        let max = RESUME_SETTLE_MAX.as_millis() as i64;
        let quiet = PTY_QUIESCENCE.as_millis() as i64;

        // Repaint mid-stream inside the minimum window: not over.
        assert!(!resume_settle_over(since, Some(since + 500), since + 1_000));
        // Quiet, but the minimum window (which must also cover the delayed
        // force-redraw repaint) hasn't elapsed: not over.
        assert!(!resume_settle_over(
            since,
            Some(since + 500),
            since + min - 1
        ));
        // Past the minimum and the repaint has gone quiet: over.
        assert!(resume_settle_over(
            since,
            Some(since + min - quiet),
            since + min
        ));
        // Past the minimum but output is still arriving: not over.
        assert!(!resume_settle_over(
            since,
            Some(since + min - 100),
            since + min
        ));
        // A child that never repainted has nothing to suppress: the stale
        // pre-restart timestamp (or none) reads as quiet at the minimum.
        assert!(resume_settle_over(since, Some(since - 60_000), since + min));
        assert!(resume_settle_over(since, None, since + min));
        // A child streaming continuously past the resume: hard cap.
        assert!(!resume_settle_over(
            since,
            Some(since + max - 100),
            since + max - 1
        ));
        assert!(resume_settle_over(
            since,
            Some(since + max - 100),
            since + max
        ));
    }

    /// Regression: claude's idle housekeeping — a "Checking for updates"
    /// painted into the status area every 30 minutes and erased half a second
    /// later — must not flip an idle unfocused session to Running, and must
    /// not re-raise its needs_attention dot. Before the burst filter, each
    /// blip marked unseen activity and undid the AwaitingInput, so the next
    /// quiescence sweep re-flagged the session: a blue dot with nothing to
    /// see. Payloads captured from a real session transcript. Spec 0054.
    #[tokio::test]
    async fn marker_ignores_idle_housekeeping_blips() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let build = |id: &str, state: SessionState| {
            let mut summary = placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            summary.harness = "claude".into();
            summary.has_pty = true;
            summary.state = state;
            Arc::new(SessionEntry {
                id: id.to_string(),
                summary: RwLock::new(summary),
                transcript_count: AtomicU64::new(0),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(false),
                title_gen_attempted: AtomicBool::new(false),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            })
        };

        // An idle claude session; the user is looking at another one.
        let s = build("idle", SessionState::AwaitingInput);
        let other = build("other", SessionState::Running);
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert("idle".into(), s.clone());
            sessions.insert("other".into(), other.clone());
        }
        manager.mark_seen("other").await.expect("mark_seen other");

        // The exact bytes claude paints while idle: "Checking for updates"
        // in the status area, erased ~530ms later.
        let paint: &[u8] = b"\x1b[?25l\x1b[H\r\x1b[27C\x1b[48B\x1b[38;2;153;153;153mChecking for updates\x1b[39m\x1b[53;1H\x1b[51;3H\x1b[?25h";
        let erase: &[u8] = b"\x1b[?25l\x1b[H\r\x1b[27C\x1b[48B\x1b[K\x1b[53;1H\x1b[51;3H\x1b[?25h";
        manager.handle_event(&s, SessionEvent::pty(paint)).await;
        manager.handle_event(&s, SessionEvent::pty(erase)).await;

        // The blip must not undo the idle state — that flip is what used to
        // hand the quiescence sweep a fresh Running→AwaitingInput transition.
        assert_eq!(
            s.summary.read().await.state,
            SessionState::AwaitingInput,
            "housekeeping blip must not flip an idle session to Running",
        );
        assert!(
            !s.unseen_activity.load(Ordering::Relaxed),
            "housekeeping blip must not count as unseen activity",
        );

        // Even if a quiescence sweep lands afterwards, no dot.
        manager
            .handle_event(
                &s,
                SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                },
            )
            .await;
        assert!(
            !s.summary.read().await.needs_attention,
            "an idle session repainting housekeeping must not light the blue dot",
        );
    }

    /// Regression: a daemon restart must not light the needs_attention dot on
    /// every backgrounded session. A respawned full-screen child repaints its
    /// old conversation — a sustained burst that passes the blip filter — and
    /// the quiescence sweep then flips the session idle, which used to raise
    /// the dot with nothing new to see. While the resume settles, repaint
    /// output must neither count as unseen activity nor undo an idle; once
    /// the settle window ends, output counts again. Spec 0054.
    #[tokio::test]
    async fn marker_ignores_resume_repaint() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let build = |id: &str, state: SessionState| {
            let mut summary = placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            summary.harness = "claude".into();
            summary.has_pty = true;
            summary.state = state;
            Arc::new(SessionEntry {
                id: id.to_string(),
                summary: RwLock::new(summary),
                transcript_count: AtomicU64::new(0),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(false),
                title_gen_attempted: AtomicBool::new(false),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            })
        };

        // Two just-respawned sessions; the user is looking at a third.
        let resumed = build("resumed-running", SessionState::Running);
        let idle = build("resumed-idle", SessionState::AwaitingInput);
        let other = build("other", SessionState::Running);
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert("resumed-running".into(), resumed.clone());
            sessions.insert("resumed-idle".into(), idle.clone());
            sessions.insert("other".into(), other.clone());
        }
        manager.mark_seen("other").await.expect("mark_seen other");

        // Respawn opened the settle window, and the resume repaint has
        // already sustained past the blip window.
        let now_ms = Utc::now().timestamp_millis();
        for entry in [&resumed, &idle] {
            entry
                .resume_settling_since_ms
                .store(now_ms, Ordering::Relaxed);
            entry
                .pty_burst_start_ms
                .store(now_ms - 3_000, Ordering::Relaxed);
            entry.summary.write().await.last_pty_at_ms = Some(now_ms - 100);
        }

        // More repaint output: no unseen activity, and the idle session must
        // not flip back to Running.
        manager
            .handle_event(&resumed, SessionEvent::pty(b"repaint"))
            .await;
        manager
            .handle_event(&idle, SessionEvent::pty(b"repaint"))
            .await;
        assert!(
            !resumed.unseen_activity.load(Ordering::Relaxed),
            "a resume repaint must not count as unseen activity",
        );
        assert!(!idle.unseen_activity.load(Ordering::Relaxed));
        assert_eq!(
            idle.summary.read().await.state,
            SessionState::AwaitingInput,
            "a resume repaint must not undo an idle session's AwaitingInput",
        );

        // The quiescence sweep flips the running one idle → no dot.
        manager
            .handle_event(
                &resumed,
                SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                },
            )
            .await;
        assert!(
            !resumed.summary.read().await.needs_attention,
            "going idle after a resume repaint must not light the dot",
        );

        // Settle window over (the quiescence poll cleared it): a fresh
        // sustained burst counts again, and the next stop raises the dot.
        resumed.resume_settling_since_ms.store(0, Ordering::Relaxed);
        let now_ms = Utc::now().timestamp_millis();
        resumed
            .pty_burst_start_ms
            .store(now_ms - 3_000, Ordering::Relaxed);
        resumed.summary.write().await.last_pty_at_ms = Some(now_ms - 100);
        manager
            .handle_event(&resumed, SessionEvent::pty(b"new turn"))
            .await;
        assert!(
            resumed.unseen_activity.load(Ordering::Relaxed),
            "post-settle output must count as unseen activity again",
        );
        assert_eq!(
            resumed.summary.read().await.state,
            SessionState::Running,
            "post-settle output must undo a quiescence idle again",
        );
        manager
            .handle_event(
                &resumed,
                SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                },
            )
            .await;
        assert!(
            resumed.summary.read().await.needs_attention,
            "a genuine post-settle stop must raise the marker",
        );
    }

    /// Regression: a resumed Grok session whose persisted native session no
    /// longer exists exits immediately with `Done { exit_code: 1 }`. That
    /// terminal event belongs to the daemon's resume attempt, not to new work,
    /// so it must not manufacture a needs-attention marker during settling.
    /// Spec 0054.
    #[tokio::test]
    async fn marker_ignores_terminal_resume_failure() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let mut summary = placement_summary(
            "missing-native-grok",
            0,
            None,
            construct_protocol::SessionKind::User,
        );
        summary.harness = "grok".into();
        summary.has_pty = true;
        summary.state = SessionState::Running;
        let entry = Arc::new(SessionEntry {
            id: summary.id.clone(),
            summary: RwLock::new(summary),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(Utc::now().timestamp_millis()),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });
        manager
            .sessions
            .write()
            .await
            .insert(entry.id.clone(), entry.clone());

        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 1 })
            .await;

        let result = entry.summary.read().await;
        assert_eq!(result.state, SessionState::Errored);
        assert!(
            !entry.unseen_activity.load(Ordering::Relaxed),
            "the resume attempt's terminal event is not unseen session work",
        );
        assert!(
            !result.needs_attention,
            "a terminal failure during resume settling must not raise the dot",
        );
    }

    /// The `needs_attention` marker tracks "this session needs you": raised when
    /// a session with unseen activity leaves `Running` while unfocused, cleared
    /// by `mark_seen` or a return to `Running`, suppressed for the focused
    /// session. Spec 0054.
    #[tokio::test]
    async fn needs_attention_marker_lifecycle() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let make_entry = |id: &str| {
            let mut summary = placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            summary.harness = "claude".into();
            summary.has_pty = true;
            summary.state = SessionState::Running;
            Arc::new(SessionEntry {
                id: id.to_string(),
                summary: RwLock::new(summary),
                transcript_count: AtomicU64::new(0),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(false),
                title_gen_attempted: AtomicBool::new(false),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            })
        };

        let entry = make_entry("mtest");
        let other = make_entry("other");
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert("mtest".into(), entry.clone());
            sessions.insert("other".into(), other.clone());
        }

        let awaiting = || SessionEvent::Status {
            state: SessionState::AwaitingInput,
            detail: None,
        };
        let running = || SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        };
        // Genuine agent output — what the marker requires before a stop counts
        // as "needs you".
        let content = || SessionEvent::Message {
            role: construct_protocol::MessageRole::Assistant,
            text: "out".into(),
        };

        // Unfocused activity + leaving Running raises the marker.
        manager.handle_event(&entry, content()).await;
        manager.handle_event(&entry, awaiting()).await;
        assert!(
            entry.summary.read().await.needs_attention,
            "AwaitingInput while unfocused must raise the marker",
        );

        // Switching to it (mark_seen) clears the marker and records focus.
        manager.mark_seen("mtest").await.expect("mark_seen");
        assert!(!entry.summary.read().await.needs_attention);

        // A stop that lands while it's the focused session must NOT re-raise.
        manager.handle_event(&entry, running()).await;
        manager.handle_event(&entry, awaiting()).await;
        assert!(
            !entry.summary.read().await.needs_attention,
            "the focused session must stay quiet when it stops",
        );

        // Focus moves elsewhere → fresh unfocused activity + the next stop
        // raises the marker again.
        manager.mark_seen("other").await.expect("mark_seen other");
        manager.handle_event(&entry, content()).await;
        manager.handle_event(&entry, running()).await;
        manager.handle_event(&entry, awaiting()).await;
        assert!(
            entry.summary.read().await.needs_attention,
            "an unfocused session that stops must raise the marker",
        );

        // A return to Running clears it.
        manager.handle_event(&entry, running()).await;
        assert!(
            !entry.summary.read().await.needs_attention,
            "resuming work clears the marker",
        );

        // A clean finish from Running is itself unseen terminal activity and
        // raises the marker even if the harness emitted no content first.
        manager.mark_seen("mtest").await.expect("mark_seen mtest");
        manager.mark_seen("other").await.expect("mark_seen other");
        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;
        assert!(entry.summary.read().await.needs_attention);
    }

    /// Regression: an idle interactive harness may exit cleanly immediately
    /// before an externally-driven daemon restart. Its terminal cleanup is not
    /// a completed turn, so the resulting AwaitingInput -> Done transition must
    /// not manufacture a persisted attention dot. Spec 0054.
    #[tokio::test]
    async fn idle_clean_exit_does_not_raise_attention() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let mut summary = placement_summary(
            "idle-pi",
            0,
            None,
            construct_protocol::SessionKind::User,
        );
        summary.harness = "pi".into();
        summary.has_pty = true;
        summary.state = SessionState::AwaitingInput;
        let entry = Arc::new(SessionEntry {
            id: summary.id.clone(),
            summary: RwLock::new(summary),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            pty_input_queue: std::sync::Mutex::new(None),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            resume_settling_since_ms: AtomicI64::new(0),
            suggest_gen: AtomicU64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });
        manager
            .sessions
            .write()
            .await
            .insert(entry.id.clone(), entry.clone());

        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;

        let result = entry.summary.read().await;
        assert_eq!(result.state, SessionState::Done);
        assert!(!result.needs_attention);
        assert!(!entry.unseen_activity.load(Ordering::Relaxed));
        drop(result);
        assert!(
            !manager
                .storage
                .load_summary("idle-pi")
                .expect("persisted summary")
                .needs_attention,
            "a daemon restart must not reload an attention dot from an idle close",
        );
    }

    /// Regression: focusing an inactive interactive session, typing at its
    /// prompt (PTY echo) then switching away without submitting must NOT raise
    /// the marker when quiescence later flips it back to AwaitingInput — the
    /// only activity was the user's own keystrokes while looking. Spec 0054.
    #[tokio::test]
    async fn marker_ignores_own_typing_in_focused_session() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let build = |id: &str, state: SessionState| {
            let mut summary = placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            summary.harness = "claude".into();
            summary.has_pty = true;
            summary.state = state;
            Arc::new(SessionEntry {
                id: id.to_string(),
                summary: RwLock::new(summary),
                transcript_count: AtomicU64::new(0),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(false),
                title_gen_attempted: AtomicBool::new(false),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            })
        };

        // An inactive interactive session (at its prompt) plus somewhere to go.
        let s = build("s", SessionState::AwaitingInput);
        let other = build("other", SessionState::Running);
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert("s".into(), s.clone());
            sessions.insert("other".into(), other.clone());
        }

        // Focus it, then type at the prompt (PTY echo while focused). The
        // minibuffer has been typing for a bit, so the echo burst is sustained
        // (past PTY_BLIP_WINDOW) and flips it to Running (busy look) — but it
        // is NOT unseen activity.
        manager.mark_seen("s").await.expect("mark_seen s");
        let now_ms = Utc::now().timestamp_millis();
        s.pty_burst_start_ms.store(
            now_ms - PTY_BLIP_WINDOW.as_millis() as i64 - 1_000,
            Ordering::Relaxed,
        );
        s.summary.write().await.last_pty_at_ms = Some(now_ms - 100);
        manager
            .handle_event(&s, SessionEvent::pty(b"sleep 30"))
            .await;
        assert_eq!(s.summary.read().await.state, SessionState::Running);

        // Switch away without submitting.
        manager.mark_seen("other").await.expect("mark_seen other");

        // The quiescence sweep flips it back to AwaitingInput.
        manager
            .handle_event(
                &s,
                SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                },
            )
            .await;

        assert!(
            !s.summary.read().await.needs_attention,
            "own keystrokes in a focused session must not raise the marker on idle",
        );
    }

    #[tokio::test]
    async fn marker_ignores_own_typing_in_split_view_focused_sessions() {
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        let build = |id: &str, state: SessionState| {
            let mut summary = placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            summary.harness = "claude".into();
            summary.has_pty = true;
            summary.state = state;
            Arc::new(SessionEntry {
                id: id.to_string(),
                summary: RwLock::new(summary),
                transcript_count: AtomicU64::new(0),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
                deleted: AtomicBool::new(false),
                archived: AtomicBool::new(false),
                title_gen_attempted: AtomicBool::new(false),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                pty_input_queue: std::sync::Mutex::new(None),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
                unseen_activity: AtomicBool::new(false),
                pty_burst_start_ms: AtomicI64::new(0),
                resume_settling_since_ms: AtomicI64::new(0),
                suggest_gen: AtomicU64::new(0),
                osc11_tail: std::sync::Mutex::new(Vec::new()),
            })
        };

        // Create two sessions running in split view.
        let s1 = build("s1", SessionState::Running);
        let s2 = build("s2", SessionState::Running);
        {
            let mut sessions = manager.sessions.write().await;
            sessions.insert("s1".into(), s1.clone());
            sessions.insert("s2".into(), s2.clone());
        }

        // Both s1 and s2 are visible/focused.
        manager
            .set_focused_sessions(&["s1".to_string(), "s2".to_string()])
            .await
            .expect("set_focused_sessions");

        // PTY activity in s2 (the sibling pane) happens while it is visible.
        manager
            .handle_event(&s2, SessionEvent::pty(b"active output"))
            .await;

        // Quiescence sweep flips s2 back to AwaitingInput.
        manager
            .handle_event(
                &s2,
                SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                },
            )
            .await;

        assert!(
            !s2.summary.read().await.needs_attention,
            "visible split-pane sessions must not raise the needs_attention marker on idle",
        );
    }

    fn create_params(mode: Option<&str>, pty: Option<PtySize>) -> CreateSessionParams {
        CreateSessionParams {
            harness: "shell".into(),
            cwd: "/tmp".into(),
            prompt: None,
            model: None,
            title: None,
            mode: mode.map(str::to_string),
            pty_size: pty,
            worktree: false,
            env: Default::default(),
            args: Vec::new(),
            kind: construct_protocol::SessionKind::User,
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
            forked_from: None,
        }
    }

    /// An explicit `mode` from the client always wins, regardless of
    /// whether a PTY size was supplied.
    #[test]
    fn effective_mode_honors_explicit_mode() {
        assert_eq!(
            effective_mode(&create_params(Some("headless"), None)),
            "headless"
        );
        assert_eq!(
            effective_mode(&create_params(Some("interactive"), None)),
            "interactive"
        );
        // Explicit mode wins even when a PTY size is also present.
        assert_eq!(
            effective_mode(&create_params(
                Some("headless"),
                Some(PtySize { cols: 80, rows: 24 })
            )),
            "headless"
        );
    }

    /// No explicit mode, but a PTY size was requested → the session is
    /// interactive (matches the adapters' own default heuristic).
    #[test]
    fn effective_mode_defaults_to_interactive_with_pty() {
        assert_eq!(
            effective_mode(&create_params(None, Some(PtySize { cols: 80, rows: 24 }))),
            "interactive"
        );
    }

    /// No explicit mode and no PTY size → headless. This is the case
    /// the PR fixes: previously `mode` stayed `None` on disk, so the
    /// remote UI couldn't tell a headless session apart from an
    /// interactive one and rendered it as a terminal instead of chat.
    #[test]
    fn effective_mode_defaults_to_headless_without_pty() {
        assert_eq!(effective_mode(&create_params(None, None)), "headless");
    }

    /// `harness.list` is the Web UI's source of truth for deciding whether a
    /// new session should request an interactive PTY. Keep this lightweight
    /// metadata aligned with every built-in adapter that supports one.
    #[test]
    fn builtin_interactive_harnesses_report_pty_support() {
        for harness in [
            "shell",
            "claude",
            "codex",
            "opencode",
            "antigravity",
            "agy",
            "grok",
            "kimi",
            "hermes",
            "pi",
            "prime-agent",
            "muse",
            "smith",
        ] {
            assert!(
                builtin_harness_capabilities(harness).supports_pty,
                "{harness} should be advertised as PTY-capable"
            );
        }
        assert!(!builtin_harness_capabilities("custom").supports_pty);
    }

    #[tokio::test]
    async fn transcript_tail_returns_last_n_with_live_total() {
        use chrono::Utc;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "stail";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Simulate 1234 persisted events. The live transcript_count is what
        // `transcript(.., tail: …)` must surface as `total` — that's the
        // signal the webui uses to decide whether to background-load older
        // pages above the tail.
        for seq in 1..=1234u64 {
            let ev = construct_protocol::TimestampedEvent {
                seq,
                at: Utc::now(),
                event: construct_protocol::SessionEvent::Message {
                    role: construct_protocol::MessageRole::Assistant,
                    text: format!("e{seq}"),
                },
            };
            storage_handle.append_event(id, &ev).expect("append event");
            entry
                .transcript_count
                .store(seq, std::sync::atomic::Ordering::Relaxed);
        }

        let result = mgr
            .transcript(id, 0, None, None, Some(50))
            .await
            .expect("transcript tail");

        assert_eq!(result.total, 1234, "total must come from the live counter");
        assert_eq!(result.events.len(), 50);
        assert_eq!(result.events.first().unwrap().seq, 1185);
        assert_eq!(result.events.last().unwrap().seq, 1234);
    }

    /// Adapters re-scan native-child transcript files from the top on every
    /// (re)start, so pre-existing history BACKFILLS into the mirror — and
    /// the per-child emission ordinals let the daemon drop replays instead
    /// of duplicating the transcript on each restart.
    #[tokio::test]
    async fn native_child_backfill_is_replay_safe() {
        use construct_protocol::MessageRole;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(Config::default());
        let (manager, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("manager");
        let owner = synthetic_entry("owner", construct_protocol::SessionKind::User, 0);
        manager
            .sessions
            .write()
            .await
            .insert(owner.id.clone(), owner.clone());

        let tagged = |seq: u64, text: &str| SessionEvent::NativeSubagent {
            id: "child".into(),
            parent_id: None,
            title: None,
            state: SessionState::Running,
            event: Some(Box::new(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: text.into(),
            })),
            seq: Some(seq),
        };

        // Initial backfill: two file-derived emissions project.
        manager.handle_event(&owner, tagged(0, "one")).await;
        manager.handle_event(&owner, tagged(1, "two")).await;
        let projected_id = native_subagent_session_id("owner", "child");
        let child = manager.detail(&projected_id).await.expect("child");
        assert_eq!(child.events.len(), 2);
        assert_eq!(
            child
                .summary
                .native_subagent
                .as_ref()
                .map(|n| n.projected_seq),
            Some(2),
            "the high-water mark advances past the projected ordinals"
        );

        // Adapter restart: the same file re-scans from the top and re-emits
        // ordinals 0 and 1 — both must be dropped, not duplicated.
        manager.handle_event(&owner, tagged(0, "one")).await;
        manager.handle_event(&owner, tagged(1, "two")).await;
        let child = manager.detail(&projected_id).await.expect("child");
        assert_eq!(
            child.events.len(),
            2,
            "replayed ordinals below the watermark never re-project"
        );

        // Genuinely new lines continue past the watermark.
        manager.handle_event(&owner, tagged(2, "three")).await;
        let child = manager.detail(&projected_id).await.expect("child");
        assert_eq!(child.events.len(), 3);
        assert_eq!(
            child
                .summary
                .native_subagent
                .as_ref()
                .map(|n| n.projected_seq),
            Some(3)
        );
    }

    /// An untagged state-only emission (discovery/lifecycle scans, which the
    /// ordinals don't cover) must never flip a finished, still-visible
    /// mirror back to running — but tagged new activity still does.
    #[tokio::test]
    async fn untagged_replay_cannot_resurrect_a_finished_mirror() {
        use construct_protocol::MessageRole;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(Config::default());
        let (manager, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("manager");
        let owner = synthetic_entry("owner", construct_protocol::SessionKind::User, 0);
        manager
            .sessions
            .write()
            .await
            .insert(owner.id.clone(), owner.clone());

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Done,
                    event: None,
                    seq: None,
                },
            )
            .await;
        let projected_id = native_subagent_session_id("owner", "child");
        // Terminal handling archives native mirrors immediately. This test
        // specifically covers a finished mirror that is still visible, so
        // model that state explicitly before replaying discovery output.
        let projected = manager.get_entry(&projected_id).await.expect("child");
        projected.summary.write().await.archived = false;
        projected.archived.store(false, Ordering::SeqCst);

        // Replayed lifecycle discovery re-announces the child as Running.
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Running,
                    event: None,
                    seq: None,
                },
            )
            .await;
        let child = manager.detail(&projected_id).await.expect("child");
        assert_eq!(
            child.summary.state,
            SessionState::Done,
            "an untagged state-only replay must not resurrect a finished mirror"
        );

        // Tagged new activity is genuine and does resurrect it.
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Running,
                    event: Some(Box::new(SessionEvent::Message {
                        role: MessageRole::User,
                        text: "again".into(),
                    })),
                    seq: Some(0),
                },
            )
            .await;
        let child = manager.detail(&projected_id).await.expect("child");
        assert_eq!(child.summary.state, SessionState::Running);
    }

    /// Loading a session recounts chat messages from its transcript, so
    /// `message_count` self-heals for summaries saved before the field
    /// existed (they deserialize as 0) and for summaries that lagged a
    /// crash. Only `Message` events count — other persisted transcript
    /// events (reasoning, tool blocks, status rows) must not inflate it.
    #[tokio::test]
    async fn load_recounts_chat_messages_from_the_transcript() {
        use chrono::Utc;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));

        // Saved summary carries no message tally (a pre-field record)...
        let summary = placement_summary("recount", 0, None, construct_protocol::SessionKind::User);
        storage.save_summary(&summary).expect("save summary");
        // ...but its transcript holds 2 chat messages among 5 events.
        let events = [
            construct_protocol::SessionEvent::Message {
                role: construct_protocol::MessageRole::User,
                text: "hi".into(),
            },
            construct_protocol::SessionEvent::Reasoning {
                text: "thinking".into(),
            },
            construct_protocol::SessionEvent::Message {
                role: construct_protocol::MessageRole::Assistant,
                text: "hello".into(),
            },
            construct_protocol::SessionEvent::Reasoning {
                text: "more thinking".into(),
            },
            construct_protocol::SessionEvent::Reasoning {
                text: "even more".into(),
            },
        ];
        // Distinct timestamps per event: the last MESSAGE is at index 2,
        // while later Reasoning events carry newer stamps.
        let base = Utc::now();
        let mut last_message_stamp = None;
        for (i, event) in events.into_iter().enumerate() {
            let at = base + chrono::Duration::seconds(i as i64);
            if matches!(event, construct_protocol::SessionEvent::Message { .. }) {
                last_message_stamp = Some(at);
            }
            let ts = construct_protocol::TimestampedEvent {
                seq: i as u64 + 1,
                at,
                event,
            };
            storage.append_event("recount", &ts).expect("append event");
        }

        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let loaded = mgr
            .get_entry("recount")
            .await
            .expect("loaded entry")
            .summary()
            .await;
        assert_eq!(loaded.message_count, 2, "only Message events count");
        assert_eq!(
            loaded.last_message_at, last_message_stamp,
            "last_message_at restores from the newest Message event's own \
             timestamp — the trailing Reasoning events don't move it"
        );
        assert_eq!(
            loaded.last_message.as_deref(),
            Some("hello"),
            "the last-message snippet restores from the newest Message"
        );
        assert_eq!(
            loaded.last_message_role,
            Some(construct_protocol::MessageRole::Assistant)
        );
    }

    /// Loading restores the last-error snippet from the transcript, and a
    /// later Running status outdates it — exactly as the live fold would
    /// have (an error the session already moved past is not "why it's
    /// errored now").
    #[tokio::test]
    async fn load_restores_last_error_until_a_newer_run_outdates_it() {
        use chrono::Utc;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));

        for (id, events) in [
            (
                "still-errored",
                vec![construct_protocol::SessionEvent::Error {
                    message: "cargo test exited 101".into(),
                }],
            ),
            (
                "ran-again",
                vec![
                    construct_protocol::SessionEvent::Error {
                        message: "cargo test exited 101".into(),
                    },
                    construct_protocol::SessionEvent::Status {
                        state: SessionState::Running,
                        detail: None,
                    },
                ],
            ),
        ] {
            let summary =
                placement_summary(id, 0, None, construct_protocol::SessionKind::User);
            storage.save_summary(&summary).expect("save summary");
            for (i, event) in events.into_iter().enumerate() {
                let ts = construct_protocol::TimestampedEvent {
                    seq: i as u64 + 1,
                    at: Utc::now(),
                    event,
                };
                storage.append_event(id, &ts).expect("append event");
            }
        }

        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let errored = mgr
            .get_entry("still-errored")
            .await
            .expect("entry")
            .summary()
            .await;
        assert_eq!(
            errored.last_error.as_deref(),
            Some("cargo test exited 101")
        );
        let recovered = mgr
            .get_entry("ran-again")
            .await
            .expect("entry")
            .summary()
            .await;
        assert_eq!(recovered.last_error, None);
    }

    /// `SessionManager::search` must use the live, in-memory session list
    /// (`self.list()`), not a fresh read of `meta.json` — a session whose
    /// title was only just updated in memory (not yet flushed to disk)
    /// must still be found by name.
    #[tokio::test]
    async fn search_finds_name_and_transcript_hits_from_live_state() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "ssearch";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        entry.summary.write().await.title = Some("Investigate flaky needle test".to_string());
        mgr.sessions.write().await.insert(id.into(), entry.clone());
        // Never written to disk — proves `search` isn't relying on
        // `meta.json`.
        assert!(!storage_handle.meta_path(id).exists());

        storage_handle
            .append_event(
                id,
                &construct_protocol::TimestampedEvent {
                    seq: 1,
                    at: chrono::Utc::now(),
                    event: construct_protocol::SessionEvent::Message {
                        role: construct_protocol::MessageRole::Assistant,
                        text: "found the needle in the haystack".to_string(),
                    },
                },
            )
            .expect("append event");

        let result = mgr
            .search(construct_protocol::SearchParams {
                query: "needle".to_string(),
                scopes: None,
                session_ids: None,
                limit: None,
                per_session_limit: None,
            })
            .await
            .expect("search");

        let scopes: std::collections::HashSet<_> = result.hits.iter().map(|h| h.scope).collect();
        assert!(scopes.contains(&construct_protocol::SearchScope::Name));
        assert!(scopes.contains(&construct_protocol::SearchScope::Transcript));
        assert!(result.hits.iter().all(|h| h.session_id == id));
    }

    #[tokio::test]
    async fn pty_replay_returns_full_disk_tail_not_just_old_ring() {
        use base64::Engine;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sreplay";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, construct_protocol::SessionKind::User, 0),
        );

        // Write 1 MiB to pty.log — that's 4× the size of the old in-memory
        // ring. Previously pty_replay would have returned only the tail
        // 256 KiB; now it must return the whole file.
        let bytes: Vec<u8> = (0..1024u32 * 1024).map(|i| (i % 251) as u8).collect();
        storage_handle
            .append_pty_bytes(id, &bytes)
            .expect("append pty bytes");

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert_eq!(
            decoded, bytes,
            "pty_replay must return the full on-disk tail, not a truncated window"
        );
    }

    #[tokio::test]
    async fn pty_replay_returns_empty_when_pty_log_missing() {
        use base64::Engine;
        use tempfile::tempdir;

        // No bytes have ever been written for this session. pty_replay must
        // return an empty body (not error) and surface the stored PTY size
        // so the TUI can still size its parsers on attach.
        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "snopty";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, construct_protocol::SessionKind::User, 0),
        );

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert!(
            decoded.is_empty(),
            "no pty.log → empty replay, got {} bytes",
            decoded.len()
        );
    }

    #[tokio::test]
    async fn pty_replay_preserves_pty_size_through_round_trip() {
        use tempfile::tempdir;

        // Refactor moved pty_replay off `PtyState` for the bytes but it
        // still reads `size` from the same lock. Lock that the change
        // didn't accidentally start returning None.
        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "ssize";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());
        entry.pty.lock().await.size = Some(PtySize {
            cols: 132,
            rows: 50,
        });

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        assert_eq!(
            result.size,
            Some(PtySize {
                cols: 132,
                rows: 50
            })
        );
    }

    #[tokio::test]
    async fn screen_snapshot_reconstructs_screen_from_disk_history() {
        use base64::Engine;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "ssnap";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());
        entry.pty.lock().await.size = Some(PtySize { cols: 40, rows: 5 });

        let mut bytes = Vec::new();
        for i in 0..50 {
            bytes.extend(format!("history line {i}\r\n").into_bytes());
        }
        bytes.extend_from_slice(b"prompt> ");
        storage_handle
            .append_pty_bytes(id, &bytes)
            .expect("append pty bytes");

        let result = mgr
            .screen_snapshot(id, false)
            .await
            .expect("screen_snapshot");
        assert_eq!(result.size, PtySize { cols: 40, rows: 5 });
        assert_eq!(result.start_offset, 0);
        assert_eq!(result.end_offset, bytes.len() as u64);
        assert_eq!(result.total_bytes, bytes.len() as u64);
        assert_eq!(result.scrollback_rows, 46, "51 rendered rows on a 5-row screen");
        assert!(!result.scrollback_truncated);

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        let mut parser = vt100::Parser::new(5, 40, 100);
        parser.process(&decoded);
        let contents = parser.screen().contents();
        assert!(
            contents.contains("history line 49") && contents.contains("prompt> "),
            "snapshot must repaint the current screen, got: {contents:?}"
        );
        assert_eq!(parser.screen().cursor_position(), (4, 8));
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 46);
        assert!(
            parser.screen().contents().starts_with("history line 0"),
            "oldest retained row must lead the rebuilt scrollback"
        );
    }

    #[tokio::test]
    async fn screen_snapshot_requires_known_pty_size() {
        use tempfile::tempdir;

        // Without the child's real geometry a server-side render would
        // wrap wrongly; the RPC must fail so clients fall back to raw
        // pty_replay instead of showing a mis-wrapped screen.
        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "ssnapnosize";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, construct_protocol::SessionKind::User, 0),
        );

        assert!(mgr.screen_snapshot(id, false).await.is_err());
    }

    #[tokio::test]
    async fn delete_cascades_to_subagents() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // A user session owning two subagents, one of which itself owns a
        // nested subagent. Deleting the owner must take all three with it.
        mgr.sessions.write().await.insert(
            "parent".into(),
            synthetic_entry("parent", construct_protocol::SessionKind::User, 0),
        );
        for (id, parent) in [("subA", "parent"), ("subB", "parent"), ("subA1", "subA")] {
            let e = synthetic_subagent_entry(id, parent, 0).await;
            mgr.sessions.write().await.insert(id.into(), e);
        }
        // An unrelated user session must survive the cascade untouched.
        mgr.sessions.write().await.insert(
            "other".into(),
            synthetic_entry("other", construct_protocol::SessionKind::User, 10),
        );

        mgr.delete("parent").await.expect("delete parent");

        let ids: Vec<String> = mgr.list().await.into_iter().map(|s| s.id).collect();
        assert!(
            !ids.contains(&"parent".to_string()),
            "parent must be deleted"
        );
        assert!(
            !ids.contains(&"subA".to_string()),
            "direct subagent must be deleted"
        );
        assert!(
            !ids.contains(&"subB".to_string()),
            "direct subagent must be deleted"
        );
        assert!(
            !ids.contains(&"subA1".to_string()),
            "nested subagent must be deleted"
        );
        assert!(
            ids.contains(&"other".to_string()),
            "unrelated session must survive the cascade",
        );
    }

    #[tokio::test]
    async fn archive_cascades_to_subagents() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "parent".into(),
            synthetic_entry("parent", construct_protocol::SessionKind::User, 0),
        );
        for (id, parent) in [("subA", "parent"), ("subA1", "subA")] {
            let e = synthetic_subagent_entry(id, parent, 0).await;
            mgr.sessions.write().await.insert(id.into(), e);
        }
        // A subagent owned by a *different* parent must not be archived.
        mgr.sessions.write().await.insert(
            "other".into(),
            synthetic_subagent_entry("other", "someone-else", 10).await,
        );

        mgr.archive("parent").await.expect("archive parent");

        // Archived sessions stay in the manager (unlike delete) but carry the
        // archived flag — recursively, down to the nested subagent.
        for id in ["parent", "subA", "subA1"] {
            let entry = mgr.get_entry(id).await.expect("entry present");
            assert!(
                entry.summary.read().await.archived,
                "{id} should be archived by the cascade",
            );
        }
        let other = mgr.get_entry("other").await.expect("other present");
        assert!(
            !other.summary.read().await.archived,
            "a subagent of a different parent must not be archived",
        );
    }

    async fn layout_test_manager(dir: &std::path::Path) -> SessionManager {
        let storage = Arc::new(crate::storage::Storage::new(dir.join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) = SessionManager::new(storage, config, dir.join("run"))
            .await
            .expect("session manager");
        mgr
    }

    fn layout_leaf(id: u64, session: Option<&str>) -> construct_protocol::LayoutNode {
        construct_protocol::LayoutNode::Leaf {
            id,
            session_id: session.map(str::to_string),
            operator_name: None,
        }
    }

    fn layout_split(
        first: construct_protocol::LayoutNode,
        second: construct_protocol::LayoutNode,
    ) -> construct_protocol::LayoutNode {
        construct_protocol::LayoutNode::Split {
            direction: construct_protocol::LayoutSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[tokio::test]
    async fn layout_starts_as_one_empty_pane_and_versions_each_write() {
        use tempfile::tempdir;
        let tmp = tempdir().expect("tempdir");
        let mgr = layout_test_manager(tmp.path()).await;

        let initial = mgr.layout();
        assert_eq!(initial.version, 0);
        assert_eq!(initial.tree.leaf_count(), 1);

        let doc = mgr
            .set_layout(
                layout_split(layout_leaf(1, Some("a")), layout_leaf(2, Some("b"))),
                Some(0),
            )
            .expect("first write");
        assert_eq!(doc.version, 1);
        assert_eq!(doc.tree.session_ids(), vec!["a", "b"]);
        assert_eq!(mgr.layout().version, 1);
    }

    #[tokio::test]
    async fn a_stale_writer_is_rejected_rather_than_clobbering() {
        use tempfile::tempdir;
        let tmp = tempdir().expect("tempdir");
        let mgr = layout_test_manager(tmp.path()).await;

        // Two clients both read version 0; the first one wins.
        mgr.set_layout(layout_leaf(1, Some("first")), Some(0))
            .expect("first writer");
        let err = mgr
            .set_layout(layout_leaf(1, Some("second")), Some(0))
            .expect_err("stale writer must be rejected");
        assert!(err.to_string().contains("layout conflict"), "{err}");
        assert_eq!(
            mgr.layout().tree.session_ids(),
            vec!["first"],
            "the rejected write must not have landed"
        );

        // Re-reading and retrying against the current version succeeds.
        let current = mgr.layout().version;
        mgr.set_layout(layout_leaf(1, Some("second")), Some(current))
            .expect("retry after re-read");
        assert_eq!(mgr.layout().tree.session_ids(), vec!["second"]);
    }

    #[tokio::test]
    async fn deleting_a_session_empties_its_pane_and_keeps_the_split() {
        use tempfile::tempdir;
        let tmp = tempdir().expect("tempdir");
        let mgr = layout_test_manager(tmp.path()).await;

        let doomed = "s-doomed";
        mgr.sessions.write().await.insert(
            doomed.into(),
            synthetic_entry(doomed, construct_protocol::SessionKind::User, 0),
        );
        mgr.set_layout(
            layout_split(layout_leaf(1, Some("keeper")), layout_leaf(2, Some(doomed))),
            Some(0),
        )
        .expect("seed layout");
        let before = mgr.layout().version;

        mgr.delete(doomed).await.expect("delete");

        let after = mgr.layout();
        assert!(after.version > before, "removal must bump the version");
        assert_eq!(
            after.tree.leaf_count(),
            2,
            "the split shape is the user's; only the pane empties"
        );
        assert_eq!(after.tree.session_ids(), vec!["keeper"]);
        assert_eq!(after.tree.session_for_leaf(2), None);
    }

    #[tokio::test]
    async fn layout_survives_a_daemon_restart_and_is_pruned_on_load() {
        use tempfile::tempdir;
        let tmp = tempdir().expect("tempdir");
        {
            let mgr = layout_test_manager(tmp.path()).await;
            // "ghost" is never a real session, so it should not survive load.
            mgr.set_layout(
                layout_split(layout_leaf(1, Some("ghost")), layout_leaf(2, None)),
                Some(0),
            )
            .expect("seed layout");
        }

        let mgr = layout_test_manager(tmp.path()).await;
        let doc = mgr.layout();
        assert_eq!(doc.tree.leaf_count(), 2, "the tree itself persists");
        assert_eq!(
            doc.tree.session_ids(),
            Vec::<&str>::new(),
            "a pane pointing at a session that did not come back is emptied"
        );
    }

    #[tokio::test]
    async fn archive_and_delete_return_quickly_without_blocking_caller() {
        // TDD test: before the spawn change these would block on slow adapter
        // stop / worktree remove. After the change the public methods return
        // promptly (state is updated + broadcast sent; slow work is spawned).
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "s-fast-archive";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, construct_protocol::SessionKind::User, 0),
        );

        let start = std::time::Instant::now();
        // No adapter attached → fast path only.
        mgr.archive(id).await.expect("archive fast");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "archive must return quickly (was {elapsed:?})"
        );

        // Re-insert for delete test (previous archive removed it from map in some paths).
        let id2 = "s-fast-delete";
        mgr.sessions.write().await.insert(
            id2.into(),
            synthetic_entry(id2, construct_protocol::SessionKind::User, 0),
        );
        let start2 = std::time::Instant::now();
        mgr.delete(id2).await.expect("delete fast");
        let elapsed2 = start2.elapsed();
        assert!(
            elapsed2 < Duration::from_millis(500),
            "delete must return quickly (was {elapsed2:?})"
        );
    }

    #[tokio::test]
    async fn pty_replay_caps_at_replay_max_for_huge_logs() {
        use base64::Engine;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sreplaybig";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, construct_protocol::SessionKind::User, 0),
        );

        // Write PTY_REPLAY_CAP + 1 MiB. Replay must return at most
        // PTY_REPLAY_CAP, and the bytes returned must be the *tail* (most
        // recent) of the file — older content is what we're willing to
        // drop, not newer.
        let extra: usize = 1024 * 1024;
        let total: usize = PTY_REPLAY_CAP + extra;
        let bytes: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
        storage_handle
            .append_pty_bytes(id, &bytes)
            .expect("append pty bytes");

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert_eq!(
            decoded.len(),
            PTY_REPLAY_CAP,
            "replay must cap at PTY_REPLAY_CAP"
        );
        assert_eq!(
            decoded,
            bytes[extra..],
            "replay must be the tail of the file (most recent bytes), not the head"
        );
    }

    #[tokio::test]
    async fn test_playbook_run_lifecycle_daemon() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "splaybookrun";
        let entry = synthetic_entry(id, construct_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Start a playbook run
        let body = "# Todo\n- a\n";
        let run = mgr
            .start_playbook_run(id, body, false, None)
            .expect("start_playbook_run");
        assert!(!run.seen_running);
        assert!(!run.first_output_seen);

        // Transition session state to Running
        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::Running,
                detail: None,
            },
        )
        .await;

        let run = mgr.playbook_run_snapshot(id).expect("run snapshot");
        assert!(run.seen_running);
        assert!(!run.first_output_seen);

        // Send playbook output (reasoning)
        mgr.handle_event(
            &entry,
            SessionEvent::Reasoning {
                text: "thinking".into(),
            },
        )
        .await;

        // Run is NOT cleared, but first_output_seen is set to true
        let run = mgr.playbook_run_snapshot(id).expect("run snapshot");
        assert!(run.seen_running);
        assert!(run.first_output_seen);

        // Transition session state back to idle (AwaitingInput)
        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            },
        )
        .await;

        // Run should now be cleared
        assert!(mgr.playbook_run_snapshot(id).is_none());
    }

    /// Regression for #1090: a session that dies before it ever reports a turn
    /// still clears its run.
    ///
    /// Every stop signal used to be gated on `seen_running`, so a harness that
    /// crashed, rejected the prompt, or was killed left the whole playbook
    /// shimmering until its safety deadline — with nothing alive that could
    /// ever settle it.
    #[tokio::test]
    async fn playbook_run_clears_when_the_session_dies_before_running() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let entry = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("session entry");

        let run = mgr
            .start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");
        assert!(
            !run.seen_running,
            "precondition: the run has not seen a turn yet"
        );

        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::Errored,
                detail: None,
            },
        )
        .await;

        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "a terminal session can never settle its blocks, so the run must \
             clear even though it was never seen running"
        );
    }

    /// Regression for #1090: a session that reports idle without ever having
    /// reported a turn had a dispatch that went nowhere, and nothing else can
    /// ever stop it.
    ///
    /// Deliberately *not* a deadline. A turn can run for hours, and a session
    /// doing that work reports Running — which takes it out of this rule
    /// entirely. See the sibling test for the case this must not touch.
    #[tokio::test]
    async fn playbook_run_clears_when_the_session_idles_without_ever_running() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let entry = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("session entry");

        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");

        // Backdate past the dispatch debounce: this idle report is no longer
        // the previous turn winding down. The debounce runs from delivery.
        {
            let mut runs = mgr.playbook_runs.lock().expect("runs");
            let run = runs.get_mut(&id).expect("run");
            let backdate = PLAYBOOK_RUN_IDLE_WITHOUT_TURN_GRACE_MS + 1_000;
            run.started_at_ms -= backdate;
            run.dispatched_at_ms = run.dispatched_at_ms.map(|ms| ms - backdate);
        }

        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            },
        )
        .await;

        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "a dispatch that never produced a turn has no other stop signal; \
             it must not wait for the unmanaged safety deadline"
        );
    }

    /// The boundary the rule above must respect: an idle report inside the
    /// dispatch debounce is the previous turn winding down, not evidence that
    /// this dispatch went nowhere.
    #[tokio::test]
    async fn playbook_run_survives_an_idle_report_inside_the_dispatch_debounce() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let entry = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("session entry");

        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");

        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            },
        )
        .await;

        assert!(
            mgr.playbook_run_snapshot(&id).is_some(),
            "a trailing idle right after dispatch must not kill the run before \
             the turn has had a chance to start"
        );
    }

    /// A turn can take hours. Once the session has reported one, the run is
    /// out of the idle-without-a-turn rule's reach and keeps shimmering for as
    /// long as the work takes.
    #[tokio::test]
    async fn playbook_run_survives_a_long_turn() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let entry = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("session entry");

        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");
        mgr.handle_event(
            &entry,
            SessionEvent::Status {
                state: SessionState::Running,
                detail: None,
            },
        )
        .await;

        // Hours in, still working: far past every debounce and well past the
        // point where a deadline-based rule would have killed it.
        {
            let mut runs = mgr.playbook_runs.lock().expect("runs");
            let run = runs.get_mut(&id).expect("run");
            run.started_at_ms -= 4 * 60 * 60 * 1_000;
            run.expires_at_ms += 4 * 60 * 60 * 1_000;
        }

        let run = mgr
            .playbook_run_snapshot(&id)
            .expect("a long turn must keep its run");
        assert!(run.seen_running);
        assert_eq!(run.pending_block_count(), 2, "both blocks are still pending");
    }

    /// Guard for #1122: a dispatch that never leaves the building disarms the
    /// run it armed.
    ///
    /// The run is now created *before* the prompt is delivered, so that the
    /// transitions the delivery itself causes land on a run that exists. The
    /// cost of arming early is that a failed delivery would otherwise leave the
    /// playbook shimmering for a turn that is never going to happen.
    #[tokio::test]
    async fn playbook_execute_disarms_the_run_when_delivery_fails() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let mgr = Arc::new(mgr);

        // The synthetic session has no adapter, so delivery cannot succeed.
        let result = mgr
            .playbook_execute(construct_protocol::PlaybookExecuteParams {
                session_id: id.clone(),
                selection: None,
                base_version: None,
                comment: None,
                shimmer: None,
                selection_block_ids: None,
                fork: false,
            })
            .await;
        assert!(result.is_err(), "precondition: delivery must fail here");

        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "an undelivered dispatch must not leave the playbook shimmering"
        );
    }

    /// Regression for #1100: a PTY-only harness's run reports that the turn is
    /// producing output.
    ///
    /// The progress ladder reads structured events, which such a harness never
    /// emits, so its runs reported `delivered` for the whole turn while the
    /// session was visibly streaming bytes.
    #[tokio::test]
    async fn playbook_run_counts_pty_output_as_the_turn_producing() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        let entry = mgr
            .sessions
            .read()
            .await
            .get(&id)
            .cloned()
            .expect("session entry");

        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");
        let run = mgr.playbook_run_snapshot(&id).expect("run snapshot");
        assert!(
            !run.first_output_seen,
            "precondition: nothing has been produced yet"
        );

        mgr.handle_event(
            &entry,
            SessionEvent::Pty {
                // PTY payloads ride the wire base64-encoded.
                data: {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD.encode("hello from the shell\r\n")
                },
            },
        )
        .await;

        let run = mgr.playbook_run_snapshot(&id).expect("run snapshot");
        assert!(
            run.first_output_seen,
            "raw bytes are the only output signal a PTY-only harness has"
        );
        assert_eq!(
            run.system_status.as_deref(),
            Some(construct_protocol::PLAYBOOK_SHIMMER_STATUS_AGENT_WORKING),
            "the run must stop reporting `delivered` once the turn produces output"
        );
    }

    /// Regression for #1091: a human editing a block the agent is working on
    /// must not settle it.
    ///
    /// Typing advances the block's content epoch, so its ref stops matching and
    /// the stale-declaration rule dropped the shimmer — and nothing re-lit it,
    /// because the agent had already declared that block and had no reason to
    /// declare it again. Annotating a task while it runs is ordinary use.
    #[tokio::test]
    async fn human_edit_keeps_a_pending_block_shimmering() {
        let body = "# T\n\n- alpha\n- beta\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");

        // The agent's planning pass: only `alpha` is pending.
        let alpha = mgr
            .playbook_blocks_projection(&id, body)
            .into_iter()
            .find(|b| b.text.contains("alpha"))
            .expect("alpha block");
        mgr.set_playbook_run_pending(
            &id,
            body,
            [(alpha.id.clone(), Some("Implementing".to_string()))]
                .into_iter()
                .collect(),
        );

        // The human annotates the task the agent is working on.
        let result = mgr
            .playbook_edit(construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "- alpha".to_string(),
                    new_string: "- alpha (also check the logs)".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Human,
                note: None,
                shimmer: Vec::new(),
            })
            .await
            .expect("human edit");

        let edited = result
            .blocks
            .iter()
            .find(|b| b.text.contains("alpha"))
            .expect("edited block");
        assert!(
            edited.shimmer,
            "the work is still in flight; typing in the block must not settle \
             it: {:?}",
            result.blocks
        );
        assert_eq!(
            edited.tooltip.as_deref(),
            Some("Implementing"),
            "the agent's status should follow the block across the edit"
        );

        let beta = result
            .blocks
            .iter()
            .find(|b| b.text.contains("beta"))
            .expect("beta block");
        assert!(
            !beta.shimmer,
            "a settled block stays settled through someone else's edit"
        );
    }

    /// The counterpart to `human_edit_keeps_a_pending_block_shimmering`: agents
    /// stay explicit. An agent that rewrites a pending block without
    /// `keep_pending` still drops its shimmer, which is what makes a stale
    /// declaration fail closed (spec 0053).
    #[tokio::test]
    async fn agent_edit_still_settles_a_pending_block_without_keep_pending() {
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");

        let alpha = mgr
            .playbook_blocks_projection(&id, body)
            .into_iter()
            .find(|b| b.text.contains("alpha"))
            .expect("alpha block");
        mgr.set_playbook_run_pending(
            &id,
            body,
            [(alpha.id.clone(), Some("Implementing".to_string()))]
                .into_iter()
                .collect(),
        );

        let result = mgr
            .playbook_edit(construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "- alpha".to_string(),
                    new_string: "- alpha done".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            })
            .await
            .expect("agent edit");

        let edited = result
            .blocks
            .iter()
            .find(|b| b.text.contains("alpha"))
            .expect("edited block");
        assert!(
            !edited.shimmer,
            "an agent edit without keep_pending settles the block: {:?}",
            result.blocks
        );
    }

    // Build a SessionManager with one synthetic session that owns `markdown` as
    // its playbook. Returns (manager, storage, session_id) for playbook tests.
    async fn playbook_test_mgr(
        markdown: &str,
    ) -> (SessionManager, Arc<crate::storage::Storage>, String) {
        use tempfile::tempdir;
        let tmp = Box::leak(Box::new(tempdir().expect("tempdir")));
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let id = "sprog".to_string();
        mgr.sessions.write().await.insert(
            id.clone(),
            synthetic_entry(&id, construct_protocol::SessionKind::User, 0),
        );
        storage
            .update_playbook(
                &id,
                markdown.to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("seed playbook");
        (mgr, storage, id)
    }

    #[tokio::test]
    async fn playbook_cursor_presence_snapshots_broadcasts_and_clears() {
        let (mgr, _storage, id) = playbook_test_mgr("- one\n").await;
        let mut rx = mgr.subscribe();

        let result = mgr
            .playbook_cursor(
                7,
                "tui",
                construct_protocol::PlaybookCursorParams {
                    session_id: id.clone(),
                    cursor: 3,
                    selection_anchor: Some(1),
                    selection_head: Some(3),
                    version: Some(1),
                    label: Some("Desk".to_string()),
                    clear: false,
                },
            )
            .await
            .expect("cursor");

        assert_eq!(result.cursor.client_id, "c7");
        assert_eq!(result.cursor.label, "Desk");
        assert_eq!(result.cursor.kind, "tui");
        assert!(result.cursor.active);
        assert_eq!(mgr.playbook_collaborators(&id).len(), 1);
        let get = mgr.playbook_get(&id).await.expect("playbook get");
        assert_eq!(get.collaborators.len(), 1);
        assert_eq!(get.collaborators[0].client_id, "c7");

        let broadcast = rx.recv().await.expect("broadcast");
        match broadcast {
            BroadcastMsg::PlaybookCursor {
                payload,
                skip_conn_id,
            } => {
                assert_eq!(payload.cursor.client_id, "c7");
                assert!(payload.cursor.active);
                assert_eq!(
                    skip_conn_id,
                    Some(7),
                    "a plain publish must be skipped for its own publisher"
                );
            }
            other => panic!("unexpected broadcast: {other:?}"),
        }

        mgr.clear_conn(7);
        assert!(mgr.playbook_collaborators(&id).is_empty());
        let broadcast = rx.recv().await.expect("clear broadcast");
        match broadcast {
            BroadcastMsg::PlaybookCursor {
                payload,
                skip_conn_id,
            } => {
                assert_eq!(payload.cursor.client_id, "c7");
                assert!(!payload.cursor.active);
                assert_eq!(skip_conn_id, None, "a disconnect tombstone excludes no one");
            }
            other => panic!("unexpected broadcast: {other:?}"),
        }
    }

    #[tokio::test]
    async fn playbook_cursor_presence_expires_from_snapshots_after_inactivity() {
        let (mgr, _storage, id) = playbook_test_mgr("- one\n").await;
        let cursor = put_playbook_cursor(&mgr, &id, 7, "tui", 3).await;
        assert_eq!(cursor.client_id, "c7");
        assert_eq!(mgr.playbook_collaborators(&id).len(), 1);

        if let Ok(mut cursors) = mgr.playbook_cursors.lock() {
            let cursor = cursors.get_mut(&7).expect("stored cursor");
            cursor.updated_at_ms = chrono::Utc::now()
                .timestamp_millis()
                .saturating_sub(PLAYBOOK_CURSOR_TTL_MS + 1);
        }

        assert!(mgr.playbook_collaborators(&id).is_empty());
        let get = mgr.playbook_get(&id).await.expect("playbook get");
        assert!(
            get.collaborators.is_empty(),
            "playbook.get should not return stale collaborators"
        );
    }

    async fn put_playbook_cursor(
        mgr: &SessionManager,
        session_id: &str,
        conn_id: u64,
        kind: &str,
        cursor: usize,
    ) -> construct_protocol::PlaybookCursor {
        mgr.playbook_cursor(
            conn_id,
            kind,
            construct_protocol::PlaybookCursorParams {
                session_id: session_id.to_string(),
                cursor,
                selection_anchor: None,
                selection_head: None,
                version: Some(1),
                label: Some(match kind {
                    "web" => "Web".to_string(),
                    "tui" => "TUI".to_string(),
                    other => other.to_string(),
                }),
                clear: false,
            },
        )
        .await
        .expect("cursor")
        .cursor
    }

    fn collaborator(
        mgr: &SessionManager,
        session_id: &str,
        client_id: &str,
    ) -> construct_protocol::PlaybookCursor {
        mgr.playbook_collaborators(session_id)
            .into_iter()
            .find(|cursor| cursor.client_id == client_id)
            .expect("collaborator")
    }

    /// Find the (assumed single) agent-presence cursor for a session, without
    /// depending on its allocated `client_id` value.
    fn agent_collaborator(
        mgr: &SessionManager,
        session_id: &str,
    ) -> construct_protocol::PlaybookCursor {
        mgr.playbook_collaborators(session_id)
            .into_iter()
            .find(|cursor| cursor.kind == "agent")
            .expect("agent collaborator")
    }

    #[tokio::test]
    async fn playbook_cursor_labels_are_unique_per_connected_client() {
        let (mgr, _storage, id) = playbook_test_mgr("abc\n").await;

        let a = put_playbook_cursor(&mgr, &id, 1, "tui", 0).await;
        let b = put_playbook_cursor(&mgr, &id, 2, "tui", 1).await;
        let c = put_playbook_cursor(&mgr, &id, 3, "web", 2).await;
        let d = mgr
            .playbook_cursor(
                4,
                "tui",
                construct_protocol::PlaybookCursorParams {
                    session_id: id.clone(),
                    cursor: 3,
                    selection_anchor: None,
                    selection_head: None,
                    version: Some(1),
                    label: Some("moon".to_string()),
                    clear: false,
                },
            )
            .await
            .expect("custom cursor")
            .cursor;
        let e = mgr
            .playbook_cursor(
                5,
                "tui",
                construct_protocol::PlaybookCursorParams {
                    session_id: id.clone(),
                    cursor: 4,
                    selection_anchor: None,
                    selection_head: None,
                    version: Some(1),
                    label: Some("moon".to_string()),
                    clear: false,
                },
            )
            .await
            .expect("duplicate custom cursor")
            .cursor;

        assert_eq!(a.label, "TUI 1");
        assert_eq!(b.label, "TUI 2");
        assert_eq!(c.label, "Web 1");
        assert_eq!(d.label, "moon");
        assert_eq!(e.label, "moon 1");
    }

    #[tokio::test]
    async fn playbook_edit_rebases_peer_cursor_after_source_insert() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;
        put_playbook_cursor(&mgr, &id, 1, "tui", 4).await;
        put_playbook_cursor(&mgr, &id, 2, "web", 6).await;

        let result = mgr
            .playbook_edit_from_conn(
                construct_protocol::PlaybookEditParams {
                    session_id: id.clone(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: "123456".to_string(),
                        new_string: "123X456".to_string(),
                        replace_all: false,
                        keep_pending: false,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Human,
                    note: None,
                    shimmer: Vec::new(),
                },
                Some(1),
            )
            .await
            .expect("edit");

        assert_eq!(result.playbook.markdown, "123X456789\n");
        assert_eq!(
            collaborator(&mgr, &id, "c1").cursor,
            4,
            "source cursor is already in post-edit coordinates"
        );
        let peer = collaborator(&mgr, &id, "c2");
        assert_eq!(peer.cursor, 7);
        assert_eq!(peer.version, Some(result.playbook.version));
    }

    #[tokio::test]
    async fn playbook_edit_rebases_peer_cursor_and_selection_after_delete() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;
        mgr.playbook_cursor(
            2,
            "web",
            construct_protocol::PlaybookCursorParams {
                session_id: id.clone(),
                cursor: 6,
                selection_anchor: Some(4),
                selection_head: Some(6),
                version: Some(1),
                label: Some("Web".to_string()),
                clear: false,
            },
        )
        .await
        .expect("peer cursor");

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "123456".to_string(),
                    new_string: "12356".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Human,
                note: None,
                shimmer: Vec::new(),
            },
            Some(1),
        )
        .await
        .expect("edit");

        let peer = collaborator(&mgr, &id, "c2");
        assert_eq!(peer.cursor, 5);
        assert_eq!(peer.selection_anchor, Some(3));
        assert_eq!(peer.selection_head, Some(5));
    }

    #[tokio::test]
    async fn playbook_edit_clamps_peer_cursor_inside_deleted_text() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;
        put_playbook_cursor(&mgr, &id, 2, "web", 4).await;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "3456".to_string(),
                    new_string: "".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Human,
                note: None,
                shimmer: Vec::new(),
            },
            Some(1),
        )
        .await
        .expect("delete range");

        assert_eq!(collaborator(&mgr, &id, "c2").cursor, 2);
    }

    #[tokio::test]
    async fn playbook_edit_rebases_replacements_after_multiple_replace_all_matches() {
        let (mgr, _storage, id) = playbook_test_mgr("aXaXa\n").await;
        put_playbook_cursor(&mgr, &id, 2, "web", 2).await; // before second `a`
        put_playbook_cursor(&mgr, &id, 3, "web", 3).await; // after second `a`
        put_playbook_cursor(&mgr, &id, 4, "web", 5).await; // after third `a`

        mgr.playbook_edit(construct_protocol::PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "a".to_string(),
                new_string: "ab".to_string(),
                replace_all: true,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: Vec::new(),
        })
        .await
        .expect("replace all");

        assert_eq!(collaborator(&mgr, &id, "c2").cursor, 3);
        assert_eq!(collaborator(&mgr, &id, "c3").cursor, 4);
        assert_eq!(collaborator(&mgr, &id, "c4").cursor, 7);
    }

    #[tokio::test]
    async fn playbook_edit_agent_publishes_presence_cursor_at_end_of_last_edit() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;

        let result = mgr
            .playbook_edit_from_conn(
                construct_protocol::PlaybookEditParams {
                    session_id: id.clone(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: "456".to_string(),
                        new_string: "XYZ".to_string(),
                        replace_all: false,
                        keep_pending: false,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: None,
                    shimmer: Vec::new(),
                },
                None,
            )
            .await
            .expect("agent edit");

        assert_eq!(result.playbook.markdown, "123XYZ789\n");
        let cursor = agent_collaborator(&mgr, &id);
        assert_eq!(cursor.kind, "agent");
        assert_eq!(
            cursor.label, "shell",
            "labeled with the owning session's harness"
        );
        assert!(cursor.active);
        assert_eq!(
            cursor.cursor, 6,
            "positioned at the end of the last applied edit"
        );
        assert_eq!(cursor.selection_anchor, Some(3), "start of the edited span");
        assert_eq!(cursor.selection_head, Some(6), "end of the edited span");
        assert_eq!(cursor.version, Some(result.playbook.version));
    }

    #[tokio::test]
    async fn playbook_edit_agent_presence_cursor_falls_back_to_generic_label() {
        let (mgr, _storage, id) = playbook_test_mgr("hello\n").await;
        {
            let entry = mgr.get_entry(&id).await.expect("entry");
            entry.summary.write().await.harness = String::new();
        }

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "hello".to_string(),
                    new_string: "hello world".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("agent edit");

        assert_eq!(
            agent_collaborator(&mgr, &id).label,
            "agent",
            "an unknown/empty harness name falls back to a generic label"
        );
    }

    #[tokio::test]
    async fn playbook_edit_agent_presence_cursor_reuses_same_client_across_edits() {
        let (mgr, _storage, id) = playbook_test_mgr("abc def\n").await;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "abc".to_string(),
                    new_string: "abcd".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("first agent edit");
        let first_client_id = agent_collaborator(&mgr, &id).client_id.clone();

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "def".to_string(),
                    new_string: "defg".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("second agent edit");

        let collaborators = mgr.playbook_collaborators(&id);
        let agent_cursors: Vec<_> = collaborators.iter().filter(|c| c.kind == "agent").collect();
        assert_eq!(
            agent_cursors.len(),
            1,
            "the agent's presence cursor updates in place, it does not append a new one per edit"
        );
        assert_eq!(agent_cursors[0].client_id, first_client_id);
    }

    #[tokio::test]
    async fn playbook_edit_human_edit_does_not_publish_agent_presence_cursor() {
        let (mgr, _storage, id) = playbook_test_mgr("abc\n").await;
        put_playbook_cursor(&mgr, &id, 1, "tui", 0).await;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "abc".to_string(),
                    new_string: "abcd".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Human,
                note: None,
                shimmer: Vec::new(),
            },
            Some(1),
        )
        .await
        .expect("human edit");

        assert!(
            mgr.playbook_collaborators(&id)
                .iter()
                .all(|c| c.kind != "agent"),
            "a human-sourced edit must not publish an agent presence cursor"
        );
    }

    #[tokio::test]
    async fn playbook_edit_agent_edit_rebases_peer_cursor_and_publishes_agent_cursor() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;
        // conn_id 5: distinct from the agent's own reserved pseudo-conn-id,
        // which `put_playbook_cursor`'s hardcoded conn_id bypasses allocating
        // from the same counter (unlike a real connection).
        put_playbook_cursor(&mgr, &id, 5, "tui", 8).await; // sits after the edit region

        let result = mgr
            .playbook_edit_from_conn(
                construct_protocol::PlaybookEditParams {
                    session_id: id.clone(),
                    edits: vec![construct_protocol::PlaybookEdit {
                        old_string: "456".to_string(),
                        new_string: "XY".to_string(),
                        replace_all: false,
                        keep_pending: false,
                    }],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: None,
                    shimmer: Vec::new(),
                },
                None,
            )
            .await
            .expect("agent edit");

        assert_eq!(result.playbook.markdown, "123XY789\n");
        // Spec 0065: a source-less (agent-authored) edit rebases every active
        // cursor, not just the ones excluding a source connection.
        assert_eq!(collaborator(&mgr, &id, "c5").cursor, 7);
        let agent = agent_collaborator(&mgr, &id);
        assert_eq!(agent.selection_anchor, Some(3));
        assert_eq!(agent.selection_head, Some(5));
        assert_eq!(agent.cursor, 5);
    }

    #[tokio::test]
    async fn playbook_edit_second_agent_edit_does_not_rebroadcast_stale_own_cursor() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;
        let mut rx = mgr.subscribe();

        // Edit 1 lands near the end and grows the document, so the agent's
        // stored presence cursor sits at (6, 10) afterward.
        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "789".to_string(),
                    new_string: "XYZW".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("first agent edit");

        // Edit 2 lands earlier and shrinks the document — an edit that, if
        // the agent's own stale cursor from edit 1 were rebased through it
        // (rather than excluded), would shift that stale (6, 10) span to a
        // different, still-wrong (4, 8) and get broadcast before the correct
        // publish overwrites it.
        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "123".to_string(),
                    new_string: "Q".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("second agent edit");

        // Collect every PlaybookCursor notification for the agent's own
        // client_id across both edits.
        let agent_client_id = agent_collaborator(&mgr, &id).client_id.clone();
        let mut agent_cursor_broadcasts = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::PlaybookCursor { payload, .. } = msg {
                if payload.cursor.client_id == agent_client_id {
                    agent_cursor_broadcasts.push(payload.cursor);
                }
            }
        }
        assert_eq!(
            agent_cursor_broadcasts.len(),
            2,
            "exactly one PlaybookCursor broadcast per edit for the agent's own \
             cursor, not a stale rebase-broadcast followed by the fresh publish: {agent_cursor_broadcasts:?}"
        );
        // The second edit's broadcast (the one that matters here) must carry
        // the *second* edit's own span, not the first edit's stale span
        // rebased forward through the second edit.
        let second = &agent_cursor_broadcasts[1];
        assert_eq!(second.selection_anchor, Some(0));
        assert_eq!(second.selection_head, Some(1));
    }

    #[tokio::test]
    async fn playbook_edit_noop_agent_edit_does_not_publish_presence_cursor() {
        let (mgr, _storage, id) = playbook_test_mgr("abc\n").await;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "abc".to_string(),
                    new_string: "abc".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("no-op agent edit");

        assert!(
            mgr.playbook_collaborators(&id)
                .iter()
                .all(|c| c.kind != "agent"),
            "a no-op edit (old_string == new_string) writes nothing, so it must not \
             publish a presence cursor claiming the agent just wrote somewhere"
        );
    }

    #[tokio::test]
    async fn playbook_edit_agent_presence_cursor_ignores_trailing_noop_edit_in_batch() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;

        // A logical change submitted as two edits in one call (spec 0041):
        // a real rewrite, plus a no-op restatement of an unrelated anchor
        // (e.g. an unchanged heading) tacked on afterward. The presence
        // cursor must land at the real edit, not the degenerate trailing one.
        let result = mgr
            .playbook_edit_from_conn(
                construct_protocol::PlaybookEditParams {
                    session_id: id.clone(),
                    edits: vec![
                        construct_protocol::PlaybookEdit {
                            old_string: "456".to_string(),
                            new_string: "XYZ".to_string(),
                            replace_all: false,
                            keep_pending: false,
                        },
                        construct_protocol::PlaybookEdit {
                            old_string: "789".to_string(),
                            new_string: "789".to_string(),
                            replace_all: false,
                            keep_pending: false,
                        },
                    ],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: None,
                    shimmer: Vec::new(),
                },
                None,
            )
            .await
            .expect("agent edit batch");

        assert_eq!(result.playbook.markdown, "123XYZ789\n");
        let cursor = agent_collaborator(&mgr, &id);
        assert_eq!(
            cursor.selection_anchor,
            Some(3),
            "the real edit's span, not the trailing no-op's zero-width one"
        );
        assert_eq!(cursor.selection_head, Some(6));
        assert_eq!(cursor.cursor, 6);
    }

    #[tokio::test]
    async fn playbook_edit_agent_presence_cursor_ignores_edits_that_cancel_out() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;

        // Each individual edit is non-degenerate (real old_len/new_len), but
        // the second and third cancel each other out — the batch's real
        // effect is only the first edit. The presence cursor must land on
        // the real change ("123" -> "abc"), not on the untouched "789"
        // that the last individual replacement happens to reference.
        let result = mgr
            .playbook_edit_from_conn(
                construct_protocol::PlaybookEditParams {
                    session_id: id.clone(),
                    edits: vec![
                        construct_protocol::PlaybookEdit {
                            old_string: "123".to_string(),
                            new_string: "abc".to_string(),
                            replace_all: false,
                            keep_pending: false,
                        },
                        construct_protocol::PlaybookEdit {
                            old_string: "789".to_string(),
                            new_string: "XYZ".to_string(),
                            replace_all: false,
                            keep_pending: false,
                        },
                        construct_protocol::PlaybookEdit {
                            old_string: "XYZ".to_string(),
                            new_string: "789".to_string(),
                            replace_all: false,
                            keep_pending: false,
                        },
                    ],
                    actor: construct_protocol::PlaybookUpdateActor::Agent,
                    note: None,
                    shimmer: Vec::new(),
                },
                None,
            )
            .await
            .expect("agent edit batch");

        assert_eq!(result.playbook.markdown, "abc456789\n");
        let cursor = agent_collaborator(&mgr, &id);
        assert_eq!(
            cursor.selection_anchor,
            Some(0),
            "the real change's span, not the cancelled-out edit's untouched text"
        );
        assert_eq!(cursor.selection_head, Some(3));
        assert_eq!(cursor.cursor, 3);
    }

    #[tokio::test]
    async fn playbook_edit_rebasing_agent_cursor_for_unrelated_edit_does_not_renew_its_freshness() {
        let (mgr, _storage, id) = playbook_test_mgr("123456789\n").await;

        // The agent writes near the end; its presence cursor is now fresh.
        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "789".to_string(),
                    new_string: "XYZ".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("agent edit");
        let stamped_at = agent_collaborator(&mgr, &id).updated_at_ms;

        // Backdate it inside the short agent presence TTL so a renewed stamp
        // would be obvious, then let a *human* edit elsewhere shift its
        // position.
        if let Ok(mut cursors) = mgr.playbook_cursors.lock() {
            let agent_conn_id = *mgr
                .agent_playbook_cursor_conn_ids
                .lock()
                .expect("lock")
                .get(&id)
                .expect("reserved agent conn id");
            cursors
                .get_mut(&agent_conn_id)
                .expect("stored cursor")
                .updated_at_ms = stamped_at - 500;
        }
        let backdated_at = stamped_at - 500;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "123".to_string(),
                    new_string: "Q".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Human,
                note: None,
                shimmer: Vec::new(),
            },
            Some(999),
        )
        .await
        .expect("human edit");

        let agent = agent_collaborator(&mgr, &id);
        // Position corrected for the human edit's shrink (3 chars -> 1).
        assert_eq!(agent.selection_anchor, Some(4));
        assert_eq!(agent.selection_head, Some(7));
        assert_eq!(
            agent.updated_at_ms, backdated_at,
            "an unrelated edit rebasing the agent's cursor must not renew its \
             freshness stamp — only the agent's own writes should, or a client \
             gating the reveal highlight off recency would flash it for text \
             the agent never touched"
        );
    }

    #[tokio::test]
    async fn playbook_edit_agent_presence_cursor_expires_after_inactivity() {
        let (mgr, _storage, id) = playbook_test_mgr("abc\n").await;

        mgr.playbook_edit_from_conn(
            construct_protocol::PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "abc".to_string(),
                    new_string: "abcd".to_string(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: Vec::new(),
            },
            None,
        )
        .await
        .expect("agent edit");
        assert!(mgr
            .playbook_collaborators(&id)
            .iter()
            .any(|c| c.kind == "agent"));

        let agent_conn_id = *mgr
            .agent_playbook_cursor_conn_ids
            .lock()
            .expect("lock")
            .get(&id)
            .expect("reserved agent conn id");
        if let Ok(mut cursors) = mgr.playbook_cursors.lock() {
            let cursor = cursors
                .get_mut(&agent_conn_id)
                .expect("stored agent cursor");
            cursor.updated_at_ms = chrono::Utc::now()
                .timestamp_millis()
                .saturating_sub(PLAYBOOK_AGENT_CURSOR_TTL_MS + 1);
        }

        assert!(
            mgr.playbook_collaborators(&id)
                .iter()
                .all(|c| c.kind != "agent"),
            "an idle agent cursor must age out via the shorter agent-specific TTL"
        );
        let get = mgr.playbook_get(&id).await.expect("playbook get");
        assert!(get.collaborators.iter().all(|c| c.kind != "agent"));
    }

    // The bug that motivated spec 0053: a planning pass must clear shimmer on
    // no-work blocks WITHOUT changing their text, while keeping the worked
    // block pending. Under the old content-change inference this was impossible
    // (inert blocks never change, so they could never settle) and the animation
    // came out inverted.
    #[tokio::test]
    async fn playbook_planning_pass_clears_inert_blocks_without_text_change() {
        // `## In progress` and its two items are one block (no blank line between).
        let md = "# Rule\n\n## TODO\n\n## In progress\n* item one\n* item two\n\n## Done\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;

        // A Run starts every block shimmering (optimistic).
        mgr.start_playbook_run(&id, md, false, None)
            .expect("start run");
        let before = mgr.playbook_get(&id).await.expect("get");
        assert!(
            before.blocks.iter().all(|b| b.shimmer),
            "every block shimmers at run start"
        );
        let id_of = |needle: &str| {
            before
                .blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap_or_else(|| panic!("no block containing {needle:?}"))
                .id
                .clone()
        };

        // Planning pass: declare the in-progress block pending and the inert
        // headings settled. The only edit is a content no-op (anchor == new),
        // so no block's text — and no id — changes.
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "# Rule".into(),
                    new_string: "# Rule".into(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![
                    construct_protocol::PlaybookShimmerDecl {
                        id: id_of("# Rule"),
                        shimmer: false,
                        tooltip: None,
                    },
                    construct_protocol::PlaybookShimmerDecl {
                        id: id_of("## TODO"),
                        shimmer: false,
                        tooltip: None,
                    },
                    construct_protocol::PlaybookShimmerDecl {
                        id: id_of("## Done"),
                        shimmer: false,
                        tooltip: None,
                    },
                    construct_protocol::PlaybookShimmerDecl {
                        id: id_of("item one"),
                        shimmer: true,
                        tooltip: Some("Running item one".into()),
                    },
                ],
            })
            .await
            .expect("planning-pass edit");

        let shimmering = |needle: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap()
                .shimmer
        };
        assert!(!shimmering("# Rule"), "inert Rule heading must settle");
        assert!(!shimmering("## TODO"), "empty TODO heading must settle");
        assert!(!shimmering("## Done"), "Done heading must settle");
        assert!(
            shimmering("item one"),
            "the in-progress block stays pending"
        );
        // The declared tooltip travels with the projection (spec 0057); settled
        // blocks carry none.
        let tooltip = |needle: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap()
                .tooltip
                .clone()
        };
        assert_eq!(tooltip("item one").as_deref(), Some("Running item one"));
        assert_eq!(tooltip("# Rule"), None, "settled block has no tooltip");
    }

    // A complete declaration on update authoritatively sets the pending set, and
    // a length mismatch is rejected before anything is written.
    #[tokio::test]
    async fn playbook_update_complete_shimmer_declaration() {
        let md = "# A\n\n# B\n\n# C\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None)
            .expect("start run");

        // Wrong length fails (3 blocks, 2 booleans).
        let err = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: md.to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                template_id: None,
                note: None,
                shimmer: Some(vec![true, false]),
                shimmer_tooltips: None,
            })
            .await
            .expect_err("length mismatch must fail");
        assert!(err.to_string().contains("blocks"), "got: {err}");

        // Correct length: only the middle block stays pending.
        let res = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: md.to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                template_id: None,
                note: None,
                shimmer: Some(vec![false, true, false]),
                shimmer_tooltips: Some(vec![None, Some("Building B".into()), None]),
            })
            .await
            .expect("update");
        let shimmering = |needle: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap()
                .shimmer
        };
        assert!(!shimmering("# A"));
        assert!(shimmering("# B"));
        assert!(!shimmering("# C"));
        // The parallel tooltip array lands on the pending block (spec 0057).
        let tooltip = |needle: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap()
                .tooltip
                .clone()
        };
        assert_eq!(tooltip("# B").as_deref(), Some("Building B"));
        assert_eq!(tooltip("# A"), None);
    }

    #[tokio::test]
    async fn playbook_update_then_execute_broadcasts_started_run() {
        use std::os::unix::fs::PermissionsExt;

        let (mgr, _storage, id) = playbook_test_mgr("# Old\n").await;
        let mgr = Arc::new(mgr);
        let adapter_dir = tempfile::tempdir().expect("adapter tempdir");
        let adapter_path = adapter_dir.path().join("mock-adapter.sh");
        std::fs::write(
            &adapter_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=null; fi
  case "$line" in
    *'"method":"initialize"'*|*'"method": "initialize"'*)
      result='{"name":"test","version":"0.0.0","capabilities":{"supports_pty":true}}'
      ;;
    *)
      result='null'
      ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#,
        )
        .expect("write mock adapter");
        let mut perms = std::fs::metadata(&adapter_path)
            .expect("mock adapter metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&adapter_path, perms).expect("chmod mock adapter");
        let (adapter_tx, _adapter_rx) = mpsc::channel(8);
        let (adapter, _init) = Adapter::spawn(
            "test".to_string(),
            adapter_path,
            Vec::new(),
            HashMap::new(),
            adapter_tx,
        )
        .await
        .expect("spawn mock adapter");
        let entry = mgr.get_entry(&id).await.expect("entry");
        *entry.adapter.lock().await = Some(adapter);

        let mut rx = mgr.subscribe();
        let update = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: "# New\n".to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: None,
                note: None,
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await
            .expect("dirty save update");
        assert!(update.active_run.is_none());

        let execute = mgr
            .playbook_execute(PlaybookExecuteParams {
                session_id: id.clone(),
                selection: None,
                base_version: Some(update.playbook.version),
                comment: None,
                shimmer: None,
                selection_block_ids: None,
                fork: false,
            })
            .await
            .expect("execute");
        assert!(
            execute.active_run.is_some(),
            "execute response should contain the started run"
        );

        let mut state_runs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let BroadcastMsg::PlaybookState(payload) = msg {
                state_runs.push(payload.active_run.is_some());
            }
        }
        assert!(
            state_runs.len() >= 2,
            "dirty save and execute should each broadcast playbook/state, got {state_runs:?}"
        );
        assert_eq!(
            state_runs.last().copied(),
            Some(true),
            "execute must broadcast a corrective run-present state after the save's stale clear: {state_runs:?}"
        );
    }

    /// Shell mock adapter for the spec-0087 pty-input queue tests: ACKs
    /// `initialize` immediately, but for each `session.pty_input` request
    /// appends the raw request line to `$RECORD` and then withholds the
    /// ACK until `$RELEASE` exists. Installed straight into the session's
    /// entry; returns the record/release paths plus the adapter message
    /// receiver (which the caller must keep alive for the test's duration).
    async fn install_blocking_pty_mock_adapter(
        mgr: &SessionManager,
        id: &str,
        dir: &std::path::Path,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        mpsc::Receiver<AdapterMessage>,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let record = dir.join("record.jsonl");
        let release = dir.join("release");
        let adapter_path = dir.join("mock-pty-adapter.sh");
        std::fs::write(
            &adapter_path,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=null; fi
  case "$line" in
    *'"method":"initialize"'*|*'"method": "initialize"'*)
      result='{"name":"test","version":"0.0.0","capabilities":{"supports_pty":true}}'
      ;;
    *'"method":"session.pty_input"'*|*'"method": "session.pty_input"'*)
      printf '%s\n' "$line" >>"$RECORD"
      while [ ! -e "$RELEASE" ]; do sleep 0.05; done
      result='null'
      ;;
    *)
      result='null'
      ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#,
        )
        .expect("write mock adapter");
        let mut perms = std::fs::metadata(&adapter_path)
            .expect("mock adapter metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&adapter_path, perms).expect("chmod mock adapter");
        let mut env = HashMap::new();
        env.insert("RECORD".to_string(), record.to_string_lossy().to_string());
        env.insert("RELEASE".to_string(), release.to_string_lossy().to_string());
        let (adapter_tx, adapter_rx) = mpsc::channel(8);
        let (adapter, _init) = Adapter::spawn(
            "test".to_string(),
            adapter_path,
            Vec::new(),
            env,
            adapter_tx,
        )
        .await
        .expect("spawn mock adapter");
        let entry = mgr.get_entry(id).await.expect("entry");
        *entry.adapter.lock().await = Some(adapter);
        (record, release, adapter_rx)
    }

    /// Spec 0087: the interactive typing path must ACK once the bytes are
    /// accepted into the session's ordered queue — not once the adapter
    /// round-trip completes. The mock withholds its `session.pty_input`
    /// ACK indefinitely, so if `pty_input` still awaited delivery this
    /// would hit the 5s timeout instead of returning immediately.
    #[tokio::test]
    async fn pty_input_acks_on_enqueue_not_on_adapter_delivery() {
        use base64::Engine as _;
        let (mgr, _storage, id) = playbook_test_mgr("# T\n").await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (record, release, _adapter_rx) =
            install_blocking_pty_mock_adapter(&mgr, &id, dir.path()).await;

        tokio::time::timeout(
            Duration::from_secs(5),
            mgr.pty_input(&id, b"hello".to_vec()),
        )
        .await
        .expect("pty_input must return on enqueue while the adapter ACK is withheld")
        .expect("enqueue should succeed");

        // Delivery still happens — asynchronously, once the adapter ACKs.
        std::fs::write(&release, b"go").expect("release");
        let payload = base64::engine::general_purpose::STANDARD.encode(b"hello");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let recorded = std::fs::read_to_string(&record).unwrap_or_default();
            if recorded.contains(&payload) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "queued input was never delivered to the adapter: {recorded:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `construct send` and the MCP input tool both arrive as SESSION_INPUT.
    /// Against a PTY-backed agent TUI the message has to be *submitted*, not
    /// typed: the LF-terminated `send_input` that dispatch used to call left
    /// the text sitting in the composer, so the send looked like a no-op.
    #[tokio::test]
    async fn user_text_submits_into_an_agent_tui_instead_of_its_composer() {
        use base64::Engine as _;
        let (mgr, _storage, id) = playbook_test_mgr("# T\n").await;
        let dir = tempfile::tempdir().expect("tempdir");
        let (record, release, _adapter_rx) =
            install_blocking_pty_mock_adapter(&mgr, &id, dir.path()).await;
        std::fs::write(&release, b"go").expect("release");
        {
            let entry = mgr.get_entry(&id).await.expect("entry");
            let mut summary = entry.summary.write().await;
            summary.harness = "codex".to_string();
            summary.has_pty = true;
        }

        // Drive the real SESSION_INPUT dispatch, not the manager helper it
        // delegates to — the bug was the wiring, not the delivery.
        let mgr = Arc::new(mgr);
        let (sub_tx, _sub_rx) = mpsc::channel(8);
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let console = Arc::new(crate::console::ConsoleSlot::new());
        let response = crate::server::dispatch(
            &mgr,
            &sub_tx,
            crate::server::ClientKind::Tui,
            1,
            construct_protocol::Request {
                jsonrpc: "2.0".into(),
                id: serde_json::json!(1),
                method: construct_protocol::ipc_method::SESSION_INPUT.to_string(),
                params: Some(
                    serde_json::to_value(construct_protocol::SessionInputParams {
                        session_id: id.clone(),
                        text: "count the files".to_string(),
                    })
                    .expect("params"),
                ),
            },
            &console,
            &out_tx,
        )
        .await;
        assert!(response.error.is_none(), "dispatch failed: {response:?}");

        let b64 = base64::engine::general_purpose::STANDARD;
        // Quoted so the Enter's short encoding can't match inside the paste's.
        let paste = format!(
            "\"{}\"",
            b64.encode(playbook_bracketed_paste_bytes("count the files"))
        );
        let enter = format!("\"{}\"", b64.encode(b"\r"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let recorded = std::fs::read_to_string(&record).unwrap_or_default();
            if recorded.contains(&paste) && recorded.contains(&enter) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected a bracketed paste then a submit Enter, got {recorded:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn missing_adapter_closes_session_for_restart() {
        let (mgr, storage, id) = playbook_test_mgr("# T\n").await;
        let mut updates = mgr.subscribe();

        let err = mgr
            .pty_input(&id, b"hello".to_vec())
            .await
            .expect_err("input without an adapter must fail");
        assert!(err.to_string().contains("no live adapter"));

        let entry = mgr.get_entry(&id).await.expect("entry");
        assert_eq!(entry.summary.read().await.state, SessionState::Done);
        assert_eq!(
            storage.load_summary(&id).expect("persisted summary").state,
            SessionState::Done
        );
        assert!(matches!(
            updates.recv().await.expect("state update"),
            BroadcastMsg::State(StateNotificationPayload { session }) if session.id == id && session.state == SessionState::Done
        ));
    }

    /// Spec 0087: every input path funnels through the same per-session
    /// ordered queue — batches reach the adapter in enqueue order even
    /// while earlier batches are still awaiting their ACK — and the
    /// delivery-awaited variant really does wait for its own batch.
    #[tokio::test]
    async fn pty_input_queue_preserves_order_and_awaited_variant_waits() {
        use base64::Engine as _;
        let (mgr, _storage, id) = playbook_test_mgr("# T\n").await;
        let mgr = Arc::new(mgr);
        let dir = tempfile::tempdir().expect("tempdir");
        let (record, release, _adapter_rx) =
            install_blocking_pty_mock_adapter(&mgr, &id, dir.path()).await;

        mgr.pty_input(&id, b"one".to_vec())
            .await
            .expect("queue one");
        mgr.pty_input(&id, b"two".to_vec())
            .await
            .expect("queue two");
        let awaited = {
            let mgr = mgr.clone();
            let id = id.clone();
            tokio::spawn(async move { mgr.pty_input_without_capture(&id, b"three".to_vec()).await })
        };
        // The writer is stuck on batch "one" (ACK withheld), so the
        // delivery-awaited call cannot have completed — deterministic, not
        // a timing guess: nothing can ACK until the release file exists.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !awaited.is_finished(),
            "delivery-awaited input returned before the adapter ACKed"
        );

        std::fs::write(&release, b"go").expect("release");
        tokio::time::timeout(Duration::from_secs(5), awaited)
            .await
            .expect("awaited input should complete once the adapter ACKs")
            .expect("join")
            .expect("delivery");

        let recorded = std::fs::read_to_string(&record).expect("record");
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let pos = |needle: &str| {
            recorded
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} not delivered in order: {recorded:?}"))
        };
        assert!(pos(&b64(b"one")) < pos(&b64(b"two")));
        assert!(pos(&b64(b"two")) < pos(&b64(b"three")));
    }

    /// The live regression behind spec 0149. A cold-starting agent TUI does
    /// not draw continuously: claude's fork boot reliably emits, pauses ~750ms
    /// mid-draw, emits again, and only attaches its input handler seconds
    /// later. Treating that pause as "startup finished" pasted the Run prompt
    /// into a harness that then flushed it on entering raw mode — the fork sat
    /// idle forever and nothing was logged, because the paste write itself
    /// succeeded. Only the harness's own `AwaitingInput` may unblock delivery
    /// while the session is still drawing.
    #[test]
    fn fork_ready_outcome_rejects_a_pause_inside_the_startup_draw() {
        use construct_protocol::SessionState;
        let since = 1_000_000i64;
        let settle = PLAYBOOK_FORK_READY_SETTLE;
        let max_wait = settle + Duration::from_secs(10);
        // Drew 750ms ago and still Running: that is a gap in the boot draw,
        // not the end of it. Keep waiting.
        let paused_ms = 750;
        assert!(
            paused_ms < settle.as_millis() as i64,
            "the fallback settle must outlast a mid-draw pause, else this \
             regression returns"
        );
        assert_eq!(
            fork_ready_outcome(
                SessionState::Running,
                Some(since + 340),
                since,
                since + 340 + paused_ms,
                Duration::from_millis(paused_ms as u64),
                settle,
                max_wait,
            ),
            None
        );
        // Same pause, but the harness now says it wants input -> ready.
        assert_eq!(
            fork_ready_outcome(
                SessionState::AwaitingInput,
                Some(since + 340),
                since,
                since + 340 + paused_ms,
                Duration::from_millis(paused_ms as u64),
                settle,
                max_wait,
            ),
            Some(true)
        );
    }

    /// The fallback exists for harnesses that never report `AwaitingInput`
    /// (one that boots straight into a turn), so they don't pay the full
    /// timeout — but it must require a genuinely idle PTY, and it must not
    /// fire off a stale pre-spawn timestamp.
    #[test]
    fn fork_ready_outcome_fallback_and_liveness_guards() {
        use construct_protocol::SessionState;
        let since = 1_000_000i64;
        let settle = PLAYBOOK_FORK_READY_SETTLE;
        let max_wait = settle + Duration::from_secs(10);
        let quiet = settle.as_millis() as i64;
        // Quiet for the full settle while still Running -> fallback fires.
        assert_eq!(
            fork_ready_outcome(
                SessionState::Running,
                Some(since + 10),
                since,
                since + 10 + quiet,
                Duration::from_secs(1),
                settle,
                max_wait,
            ),
            Some(true)
        );
        // Nothing drawn yet -> not ready, regardless of state.
        assert_eq!(
            fork_ready_outcome(
                SessionState::AwaitingInput,
                None,
                since,
                since + 5_000,
                Duration::from_secs(1),
                settle,
                max_wait,
            ),
            None
        );
        // The only PTY timestamp predates the fork -> stale, not evidence
        // this fork drew anything.
        assert_eq!(
            fork_ready_outcome(
                SessionState::AwaitingInput,
                Some(since - 10_000),
                since,
                since + 5_000,
                Duration::from_secs(1),
                settle,
                max_wait,
            ),
            None
        );
        // A dead fork is never going to accept input: give up now rather
        // than burning the whole timeout on it.
        assert_eq!(
            fork_ready_outcome(
                SessionState::Errored,
                Some(since + 10),
                since,
                since + 20,
                Duration::from_millis(0),
                settle,
                max_wait,
            ),
            Some(false)
        );
        // Timeout with the harness still booting -> deliver anyway.
        assert_eq!(
            fork_ready_outcome(
                SessionState::Running,
                Some(since + 10),
                since,
                since + 20,
                max_wait,
                settle,
                max_wait,
            ),
            Some(false)
        );
    }

    // Spec 0042: starting a selection run while another run is still in flight
    // adds the selected blocks to the existing pending set. It must not replace
    // the run record, because that would lose already-pending blocks and the
    // agent-managed lifecycle bit that keeps delegated work shimmering while
    // the owning session is idle.
    #[tokio::test]
    async fn playbook_selection_run_unions_with_managed_inflight_run() {
        use construct_protocol::SessionState;

        let md = "# A\n\n# B\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None)
            .expect("start full run");
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        let get = mgr.playbook_get(&id).await.expect("get");
        let block_ref = |needle: &str| {
            get.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap_or_else(|| panic!("missing block {needle:?}"))
                .id
                .clone()
        };
        let a_ref = block_ref("# A");
        let b_ref = block_ref("# B");

        declare(
            &mgr,
            &id,
            "# A",
            vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: a_ref.clone(),
                    shimmer: true,
                    tooltip: Some("Working A".into()),
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: b_ref.clone(),
                    shimmer: false,
                    tooltip: None,
                },
            ],
        )
        .await;
        let narrowed = mgr.playbook_run_snapshot(&id).expect("narrowed run");
        assert!(narrowed.agent_managed);
        assert_eq!(narrowed.pending_block_refs, vec![a_ref.clone()]);

        let selection_run = mgr
            .start_playbook_run_with_dispatch_state(&id, "# B\n", true, Some(&[true]), true, None)
            .expect("selection run");

        assert!(
            selection_run.agent_managed,
            "selection union preserves the old run's managed lifecycle"
        );
        assert_eq!(
            selection_run.started_at_ms, narrowed.started_at_ms,
            "selection union refreshes the existing run instead of replacing it"
        );
        assert!(
            selection_run.pending_block_refs.contains(&a_ref),
            "old in-flight block A keeps shimmering"
        );
        assert!(
            selection_run.pending_block_refs.contains(&b_ref),
            "selected block B is added under its stable ref"
        );
        assert_eq!(
            selection_run
                .pending_block_tooltips
                .get(&a_ref)
                .map(String::as_str),
            Some("Working A"),
            "existing pending tooltips are preserved"
        );

        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        assert!(
            mgr.playbook_run_snapshot(&id).is_some(),
            "the unioned managed run survives the owning session going idle"
        );
    }

    #[tokio::test]
    async fn playbook_selection_run_without_active_run_starts_selection_only() {
        let md = "# A\n\n# B\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        let get = mgr.playbook_get(&id).await.expect("get");
        let b_ref = get
            .blocks
            .iter()
            .find(|b| b.text.contains("# B"))
            .expect("B block")
            .id
            .clone();

        let run = mgr
            .start_playbook_run_with_dispatch_state(&id, "# B\n", true, Some(&[true]), false, None)
            .expect("selection run");

        assert_eq!(run.pending_block_refs, vec![b_ref]);
        assert_eq!(run.total_block_count, 1);
        assert!(!run.agent_managed);
    }

    // The bug this fix addresses: selecting a strict SUBSTRING of a single
    // line/block (not the whole line) and running it must resolve to the
    // real block, not a phantom hash-of-substring id that matches nothing in
    // the document (which is why it never shimmered). Without
    // `selection_block_ids`, the daemon falls back to re-parsing the raw
    // selected text and hash-matching, which misses on a partial-line
    // selection; this asserts the improved fallback's second attempt (text
    // containment against the saved blocks) still recovers the real block
    // when exactly one candidate contains the substring.
    #[tokio::test]
    async fn playbook_partial_line_selection_without_ids_resolves_via_containment_fallback() {
        let md = "Some long text here\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        let get = mgr.playbook_get(&id).await.expect("get");
        let real_ref = get.blocks[0].id.clone();

        let run = mgr
            .start_playbook_run_with_dispatch_state(&id, "long text", true, None, false, None)
            .expect("selection run");

        assert_eq!(
            run.pending_block_refs,
            vec![real_ref],
            "a partial-line selection should resolve to the real block via \
             containment, not fabricate a phantom hash-of-substring id"
        );
    }

    // The real fix: when the client supplies `selection_block_ids` (the
    // overlap-based real block ids it computed locally), the daemon trusts
    // that identity directly instead of re-parsing/hash-matching the raw
    // selected substring at all.
    #[tokio::test]
    async fn playbook_partial_line_selection_with_explicit_ids_resolves_real_block() {
        let md = "Some long text here\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        let get = mgr.playbook_get(&id).await.expect("get");
        let real_ref = get.blocks[0].id.clone();

        let run = mgr
            .start_playbook_run_with_dispatch_state(
                &id,
                "long text",
                true,
                None,
                false,
                Some(&[real_ref.clone()]),
            )
            .expect("selection run");

        assert_eq!(run.pending_block_refs, vec![real_ref]);
        assert_eq!(run.total_block_count, 1);
    }

    // The containment fallback is a heuristic, not a guarantee: when the
    // selected substring appears in more than one real block, which real
    // block it refers to is genuinely ambiguous without the client's overlap
    // info, so this still falls back to a phantom scoped to the substring
    // alone — a known limitation of running without `selection_block_ids`.
    // Supplying the explicit id resolves it unambiguously regardless.
    #[tokio::test]
    async fn playbook_partial_line_selection_duplicate_content_ambiguous_without_ids() {
        let md = "Some long text here\n\nAnother long text passage\n";

        // Two independent sessions/managers: a selection run always adds to
        // (rather than replaces) an already in-flight run for the same
        // session (spec 0042), so the two scenarios below must not share one.
        let (mgr_a, _storage_a, id_a) = playbook_test_mgr(md).await;
        let get_a = mgr_a.playbook_get(&id_a).await.expect("get");
        let first_ref = get_a
            .blocks
            .iter()
            .find(|b| b.text.contains("Some long text here"))
            .expect("first block")
            .id
            .clone();
        let second_ref = get_a
            .blocks
            .iter()
            .find(|b| b.text.contains("Another long text passage"))
            .expect("second block")
            .id
            .clone();

        let ambiguous = mgr_a
            .start_playbook_run_with_dispatch_state(&id_a, "long text", true, None, false, None)
            .expect("selection run");
        assert!(
            !ambiguous.pending_block_refs.contains(&first_ref)
                && !ambiguous.pending_block_refs.contains(&second_ref),
            "duplicate content is ambiguous without ids, so neither real block's \
             ref should appear: {:?}",
            ambiguous.pending_block_refs
        );

        let (mgr_b, _storage_b, id_b) = playbook_test_mgr(md).await;
        let resolved = mgr_b
            .start_playbook_run_with_dispatch_state(
                &id_b,
                "long text",
                true,
                None,
                false,
                Some(&[first_ref.clone()]),
            )
            .expect("selection run");
        assert_eq!(
            resolved.pending_block_refs,
            vec![first_ref],
            "explicit selection_block_ids resolves the ambiguity unambiguously"
        );
    }

    #[tokio::test]
    async fn playbook_full_rerun_with_explicit_shimmer_includes_user_edited_block() {
        use construct_protocol::SessionState;

        let md = "# A\n\n# B\n";
        let edited = "# A\n\n# B edited\n";
        let (mgr, storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None)
            .expect("start full run");
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        let get = mgr.playbook_get(&id).await.expect("get");
        let block_ref = |needle: &str| {
            get.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap_or_else(|| panic!("missing block {needle:?}"))
                .id
                .clone()
        };
        let a_ref = block_ref("# A");
        let b_ref = block_ref("# B");

        declare(
            &mgr,
            &id,
            "# A",
            vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: a_ref.clone(),
                    shimmer: true,
                    tooltip: Some("Working A".into()),
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: b_ref,
                    shimmer: false,
                    tooltip: None,
                },
            ],
        )
        .await;
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("narrowed run")
                .pending_block_refs,
            vec![a_ref.clone()]
        );

        storage
            .update_playbook(
                &id,
                edited.to_string(),
                construct_protocol::PlaybookUpdateActor::Human,
                None,
                None,
                None,
            )
            .expect("save edited playbook");
        let edited_get = mgr.playbook_get(&id).await.expect("edited get");
        let edited_b_ref = edited_get
            .blocks
            .iter()
            .find(|b| b.text.contains("# B edited"))
            .expect("edited B block")
            .id
            .clone();

        let run = mgr
            .start_playbook_run_with_dispatch_state(
                &id,
                edited,
                false,
                Some(&[true, true]),
                true,
                None,
            )
            .expect("explicit full re-run");

        assert!(
            run.pending_block_refs.contains(&a_ref),
            "still-pending block A remains pending"
        );
        assert!(
            run.pending_block_refs.contains(&edited_b_ref),
            "user-edited block B is seeded pending by explicit shimmer"
        );
        assert_eq!(run.pending_block_refs.len(), 2);
    }

    // An edit's shimmer declaration for an id that no longer exists is dropped
    // (fail closed), and other blocks are untouched.
    #[tokio::test]
    async fn playbook_edit_shimmer_unknown_id_is_ignored() {
        let md = "# A\n\n# B\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None)
            .expect("start run");
        let a = construct_protocol::playbook_block_id("# A");
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "# A".into(),
                    new_string: "# A".into(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![
                    construct_protocol::PlaybookShimmerDecl {
                        id: "deadbeefdeadbeef".into(),
                        shimmer: false,
                        tooltip: None,
                    },
                    construct_protocol::PlaybookShimmerDecl {
                        id: a,
                        shimmer: false,
                        tooltip: None,
                    },
                ],
            })
            .await
            .expect("edit");
        let shimmering = |needle: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .unwrap()
                .shimmer
        };
        // The bogus id changed nothing; the real declaration settled "# A";
        // "# B" was never declared and keeps its run-start shimmer.
        assert!(!shimmering("# A"));
        assert!(shimmering("# B"));
    }

    // Helper: planning-pass declaration carried by a no-op anchored edit.
    async fn declare(
        mgr: &SessionManager,
        id: &str,
        anchor: &str,
        decls: Vec<construct_protocol::PlaybookShimmerDecl>,
    ) -> PlaybookUpdateResult {
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.to_string(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: anchor.to_string(),
                new_string: anchor.to_string(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: decls,
        })
        .await
        .expect("declare")
    }

    // The bug from the 'construct improvements' session: a move that changes a
    // still-pending block's text empties the pending set BEFORE the new id is
    // declared. The run must NOT be destroyed (spec 0053) — a follow-up
    // re-declaration of the new id revives the shimmer. Before this fix the run
    // was reaped on the transient empty and the re-declare was a silent no-op.
    #[tokio::test]
    async fn playbook_run_survives_transient_empty_and_revives() {
        let md = "# Tasks\n\n* do the thing\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let get = mgr.playbook_get(&id).await.expect("get");
        let id_of = |n: &str| {
            get.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .id
                .clone()
        };
        // Planning pass: only the task is pending (heading settled).
        declare(
            &mgr,
            &id,
            "# Tasks",
            vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# Tasks"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("do the thing"),
                    shimmer: true,
                    tooltip: Some("Doing the thing".into()),
                },
            ],
        )
        .await;

        // Move/annotate the task WITHOUT keep_pending and WITHOUT re-declaring:
        // its old id drops and the pending set transiently empties.
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "* do the thing".into(),
                new_string: "* do the thing — @{session:sub1}".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![],
        })
        .await
        .expect("move");

        // Nothing shimmers right now (pending emptied) ...
        let mid = mgr.playbook_get(&id).await.expect("get mid");
        assert!(
            mid.blocks.iter().all(|b| !b.shimmer),
            "transient empty: nothing pending"
        );
        // ... but the run RECORD survived. Re-declare the moved block's new id.
        let new_id = mid
            .blocks
            .iter()
            .find(|b| b.text.contains("do the thing"))
            .unwrap()
            .id
            .clone();
        let res = declare(
            &mgr,
            &id,
            "# Tasks",
            vec![construct_protocol::PlaybookShimmerDecl {
                id: new_id,
                shimmer: true,
                tooltip: Some("Reviving moved task".into()),
            }],
        )
        .await;
        assert!(
            res.blocks
                .iter()
                .find(|b| b.text.contains("do the thing"))
                .unwrap()
                .shimmer,
            "the run survived the transient empty and the re-declaration re-lit the block"
        );
    }

    // keep_pending on a text-changing edit re-adds the resulting block's new id
    // in the SAME call, so moving/annotating a still-pending block keeps it
    // shimmering atomically — the pending set never transiently empties.
    #[tokio::test]
    async fn playbook_edit_keep_pending_keeps_moved_block_shimmering() {
        let md = "# Tasks\n\n* do the thing\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let get = mgr.playbook_get(&id).await.expect("get");
        let id_of = |n: &str| {
            get.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .id
                .clone()
        };
        // Planning pass: heading settled, task pending.
        declare(
            &mgr,
            &id,
            "# Tasks",
            vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# Tasks"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("do the thing"),
                    shimmer: true,
                    tooltip: Some("Doing the thing".into()),
                },
            ],
        )
        .await;

        // Move the task WITH keep_pending — its id changes but it stays pending
        // in one call (no transient empty, no need to know the new id).
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "* do the thing".into(),
                    new_string: "* do the thing — @{session:sub1}".into(),
                    replace_all: false,
                    keep_pending: true,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![],
            })
            .await
            .expect("move keep_pending");
        let shim = |n: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .shimmer
        };
        assert!(
            shim("do the thing"),
            "keep_pending kept the moved block pending"
        );
        // keep_pending re-adds the block under its NEW id with no carried-over
        // tooltip (spec 0057): the projection reports none, so a renderer falls
        // back to the hardcoded label until the agent re-declares with a tooltip.
        assert_eq!(
            res.blocks
                .iter()
                .find(|b| b.text.contains("do the thing"))
                .unwrap()
                .tooltip,
            None,
            "moved block has no stored tooltip; renderer falls back"
        );
        assert!(!shim("# Tasks"), "the settled heading stays settled");
    }

    // keep_pending whose new_string spans MULTIPLE blocks (a heading-anchored
    // insert: "# In progress\n\n* task") must keep the inserted *item*, not just
    // the first block (the heading). And it must not re-light the re-stated,
    // unchanged heading. This is the canonical "move task into In progress"
    // orchestration step.
    #[tokio::test]
    async fn playbook_edit_keep_pending_lights_inserted_item_not_anchor_heading() {
        let md = "# Todo\n\n* ship X\n\n# In progress\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        let id_of = |n: &str| {
            g.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .id
                .clone()
        };
        // Planning pass: settle headings, keep the todo task pending.
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "# Todo".into(),
                new_string: "# Todo".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# Todo"),
                    shimmer: false,
                    tooltip: Some("Task list settled".into()),
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# In progress"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("ship X"),
                    shimmer: true,
                    tooltip: Some("Shipping X".into()),
                },
            ],
        })
        .await
        .expect("planning");
        // Remove from Todo, then insert under the In progress heading via a
        // heading-anchored edit whose new_string spans heading + new item.
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "* ship X\n".into(),
                new_string: "".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![],
        })
        .await
        .expect("remove from todo");
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "# In progress\n".into(),
                    new_string: "# In progress\n\n* ship X — @{session:s9}\n".into(),
                    replace_all: false,
                    keep_pending: true,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![],
            })
            .await
            .expect("insert under heading");
        let shim = |n: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .shimmer
        };
        assert!(
            shim("ship X — @{session:s9}"),
            "keep_pending lights the inserted item even though it isn't new_string's first block"
        );
        assert!(
            !shim("# In progress"),
            "the re-stated, unchanged heading is not re-lit"
        );
    }

    // Smart clip instance ids are UI identity metadata. The TUI can normalize a
    // missing clip_id after an agent moved a pending task into In progress; that
    // should not settle the task's shimmer by changing only clip instance metadata.
    #[tokio::test]
    async fn playbook_update_adding_smart_clip_id_preserves_pending_block() {
        let md = "# In progress\n\n* task — @{session:s1}\n";
        let normalized = "# In progress\n\n* task — @{session:s1 clip_id=clip_4}\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        let before_task = g
            .blocks
            .iter()
            .find(|b| b.text.contains("task"))
            .unwrap()
            .clone();
        let id_of = |n: &str| {
            g.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .id
                .clone()
        };
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "# In progress".into(),
                new_string: "# In progress".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# In progress"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("task"),
                    shimmer: true,
                    tooltip: Some("Still working".into()),
                },
            ],
        })
        .await
        .expect("planning");

        let res = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: normalized.to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: None,
                note: Some("Normalize smart clip ids".into()),
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await
            .expect("normalize clip id");

        let task = res.blocks.iter().find(|b| b.text.contains("task")).unwrap();
        assert!(
            task.shimmer,
            "adding only clip_id metadata must not settle pending work"
        );
        assert_eq!(
            task.id, before_task.id,
            "smart-clip ids do not change the stable ref"
        );
        assert_eq!(task.block_id, before_task.block_id);
        assert_eq!(task.content_epoch, before_task.content_epoch);
        assert_eq!(task.content_id, before_task.content_id);
        assert_eq!(task.tooltip.as_deref(), Some("Still working"));
    }

    // Duplicate blocks have the same legacy content id, so stable shimmer must
    // address block instances by daemon-owned refs. Settling one duplicate must
    // not settle or re-light its twin.
    #[tokio::test]
    async fn playbook_shimmer_stable_refs_distinguish_duplicate_blocks() {
        let md = "* duplicate\n* duplicate\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        assert_eq!(g.blocks.len(), 2);
        assert_eq!(g.blocks[0].content_id, g.blocks[1].content_id);
        assert_ne!(g.blocks[0].id, g.blocks[1].id);
        assert!(g.blocks.iter().all(|b| b.shimmer));

        let first_ref = g.blocks[0].id.clone();
        let second_ref = g.blocks[1].id.clone();
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: md.into(),
                    new_string: md.into(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                    id: first_ref,
                    shimmer: false,
                    tooltip: None,
                }],
            })
            .await
            .expect("settle first duplicate");

        assert!(!res.blocks[0].shimmer, "first duplicate settled by ref");
        assert!(res.blocks[1].shimmer, "second duplicate keeps shimmering");
        assert_eq!(
            res.blocks[1].id, second_ref,
            "unchanged duplicate keeps its ref"
        );
    }

    // A human semantic edit changes the block's content epoch. The old pending
    // ref no longer matches the new meaning, so stale shimmer drops fail-closed.
    #[tokio::test]
    async fn playbook_shimmer_semantic_edit_changes_epoch_and_drops_pending() {
        let md = "* task\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let before = mgr.playbook_get(&id).await.expect("get").blocks[0].clone();

        let res = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: "* task changed\n".into(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: None,
                note: None,
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await
            .expect("human edit");

        let after = &res.blocks[0];
        assert_eq!(
            after.block_id, before.block_id,
            "same indexed block keeps instance id"
        );
        assert_eq!(after.content_epoch, before.content_epoch + 1);
        assert_ne!(after.id, before.id);
        assert_ne!(after.content_id, before.content_id);
        assert!(
            !after.shimmer,
            "stale pending ref does not attach to changed text"
        );
    }

    // keep_pending is the explicit opt-in for semantic edits that should remain
    // in flight: the edit creates a new epoch and atomically re-adds that new ref.
    #[tokio::test]
    async fn playbook_edit_keep_pending_relights_new_epoch() {
        let md = "* task\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let before = mgr.playbook_get(&id).await.expect("get").blocks[0].clone();

        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "* task".into(),
                    new_string: "* task @{session:s1}".into(),
                    replace_all: false,
                    keep_pending: true,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![],
            })
            .await
            .expect("agent edit");

        let after = res.blocks.iter().find(|b| b.text.contains("task")).unwrap();
        assert_eq!(after.block_id, before.block_id);
        assert_eq!(after.content_epoch, before.content_epoch + 1);
        assert_ne!(after.id, before.id);
        assert!(after.shimmer, "keep_pending re-adds the new ref atomically");
    }

    // Moving unchanged text is not a semantic change: the block ref follows the
    // block to its new location and keeps shimmer without keep_pending.
    #[tokio::test]
    async fn playbook_shimmer_ref_follows_unchanged_moved_block() {
        let md = "# Todo\n\n* task\n\n# Doing\n";
        let moved = "# Todo\n\n# Doing\n\n* task\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let before = mgr
            .playbook_get(&id)
            .await
            .expect("get")
            .blocks
            .into_iter()
            .find(|b| b.text.contains("task"))
            .unwrap();

        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![
                    construct_protocol::PlaybookEdit {
                        old_string: "* task\n\n".into(),
                        new_string: "".into(),
                        replace_all: false,
                        keep_pending: false,
                    },
                    construct_protocol::PlaybookEdit {
                        old_string: "# Doing\n".into(),
                        new_string: "# Doing\n\n* task\n".into(),
                        replace_all: false,
                        keep_pending: false,
                    },
                ],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![],
            })
            .await
            .expect("move unchanged block");

        assert_eq!(res.playbook.markdown, moved);
        let after = res.blocks.iter().find(|b| b.text.contains("task")).unwrap();
        assert_eq!(
            after.id, before.id,
            "same content keeps the same ref across moves"
        );
        assert_eq!(after.content_epoch, before.content_epoch);
        assert!(after.shimmer, "pending shimmer follows the moved block");
        assert_eq!(after.start_line, 4);
    }

    // Regression: a human co-edit save (playbook_update with no shimmer decl)
    // that merely *inserts* a brand-new block ahead of untouched siblings must
    // not disturb those siblings' identity. Block-identity reconciliation used
    // to fall back to raw positional-index alignment for any block it could
    // not match by exact content, which meant one insertion cascaded into
    // every later block being treated as "semantically edited" — silently
    // dropping their shimmer even though their text never changed.
    #[tokio::test]
    async fn playbook_human_insert_before_siblings_preserves_their_shimmer() {
        let md = "* alpha\n* beta\n* gamma\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        let id_of = |blocks: &[construct_protocol::PlaybookBlockView], n: &str| {
            blocks.iter().find(|b| b.text.contains(n)).unwrap().clone()
        };
        // Settle "alpha" (as a planning pass / prior turn would) so only beta
        // and gamma are still pending — mirroring a mid-run document where
        // some work already settled before the human edits the document.
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "* alpha".into(),
                new_string: "* alpha".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                id: id_of(&g.blocks, "alpha").id,
                shimmer: false,
                tooltip: None,
            }],
        })
        .await
        .expect("settle alpha");
        let before_beta = id_of(&g.blocks, "beta");
        let before_gamma = id_of(&g.blocks, "gamma");

        // Human inserts a brand-new item at the very top and saves — no text
        // of alpha/beta/gamma changes.
        let edited = "* zero\n* alpha\n* beta\n* gamma\n";
        let res = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: edited.to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: None,
                note: None,
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await
            .expect("human insert");

        let after_beta = id_of(&res.blocks, "beta");
        let after_gamma = id_of(&res.blocks, "gamma");
        assert_eq!(
            after_beta.id, before_beta.id,
            "untouched sibling keeps its stable ref across an unrelated insert"
        );
        assert_eq!(after_beta.content_epoch, before_beta.content_epoch);
        assert!(
            after_beta.shimmer,
            "an unrelated insert must not clear a still-pending sibling's shimmer"
        );
        assert_eq!(after_gamma.id, before_gamma.id);
        assert_eq!(after_gamma.content_epoch, before_gamma.content_epoch);
        assert!(
            after_gamma.shimmer,
            "an unrelated insert must not clear a still-pending sibling's shimmer"
        );
        assert!(
            !id_of(&res.blocks, "alpha").shimmer,
            "a settled sibling must not be re-lit by an unrelated insert"
        );
    }

    // Regression companion: inserting a new block *and* editing a later block
    // in the same human save must scope the epoch bump to only the genuinely
    // edited block — an untouched block sitting between the insert and the
    // edit must keep its ref and shimmer.
    #[tokio::test]
    async fn playbook_human_insert_and_edit_scopes_epoch_bump_to_edited_block() {
        let md = "* alpha\n* beta\n* gamma\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        let before_beta = g
            .blocks
            .iter()
            .find(|b| b.text.contains("beta"))
            .unwrap()
            .clone();

        // Insert a new item before everything, and edit "gamma" -> "gamma2".
        // "beta" sits untouched between the insert and the edit.
        let edited = "* zero\n* alpha\n* beta\n* gamma2\n";
        let res = mgr
            .playbook_update(PlaybookUpdateParams {
                session_id: id.clone(),
                markdown: edited.to_string(),
                base_version: None,
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: None,
                note: None,
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await
            .expect("human insert + edit");

        let after_beta = res.blocks.iter().find(|b| b.text.contains("beta")).unwrap();
        assert_eq!(
            after_beta.id, before_beta.id,
            "a block between an insert and an unrelated edit keeps its ref"
        );
        assert_eq!(after_beta.content_epoch, before_beta.content_epoch);
        assert!(
            after_beta.shimmer,
            "a block between an insert and an unrelated edit keeps its shimmer"
        );
        let gamma2 = res
            .blocks
            .iter()
            .find(|b| b.text.contains("gamma2"))
            .unwrap();
        assert!(
            !gamma2.shimmer,
            "the genuinely edited block drops stale shimmer (fails closed)"
        );
    }

    // Finer block granularity: a section of consecutive list items (no blank
    // lines between them) is many blocks, so one item can settle while its
    // siblings keep shimmering — and the heading is its own block too.
    #[tokio::test]
    async fn playbook_consecutive_items_shimmer_independently() {
        let md = "# In progress\n* task A\n* task B\n* task C\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        let g = mgr.playbook_get(&id).await.expect("get");
        // Each line is its own block: heading + three items.
        assert_eq!(g.blocks.len(), 4, "section splits into heading + 3 items");
        let id_of = |n: &str| {
            g.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .id
                .clone()
        };
        // Planning pass: settle the heading, keep all three items pending.
        mgr.playbook_edit(PlaybookEditParams {
            session_id: id.clone(),
            edits: vec![construct_protocol::PlaybookEdit {
                old_string: "# In progress".into(),
                new_string: "# In progress".into(),
                replace_all: false,
                keep_pending: false,
            }],
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            note: None,
            shimmer: vec![
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("# In progress"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("task A"),
                    shimmer: true,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("task B"),
                    shimmer: true,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: id_of("task C"),
                    shimmer: true,
                    tooltip: None,
                },
            ],
        })
        .await
        .expect("planning");
        // Settle ONLY task B (its work finished) — A and C must keep shimmering.
        let res = mgr
            .playbook_edit(PlaybookEditParams {
                session_id: id.clone(),
                edits: vec![construct_protocol::PlaybookEdit {
                    old_string: "# In progress".into(),
                    new_string: "# In progress".into(),
                    replace_all: false,
                    keep_pending: false,
                }],
                actor: construct_protocol::PlaybookUpdateActor::Agent,
                note: None,
                shimmer: vec![construct_protocol::PlaybookShimmerDecl {
                    id: id_of("task B"),
                    shimmer: false,
                    tooltip: None,
                }],
            })
            .await
            .expect("settle B");
        let shim = |n: &str| {
            res.blocks
                .iter()
                .find(|b| b.text.contains(n))
                .unwrap()
                .shimmer
        };
        assert!(!shim("# In progress"), "heading is its own settled block");
        assert!(shim("task A"), "task A keeps shimmering");
        assert!(!shim("task B"), "only task B settled");
        assert!(shim("task C"), "task C keeps shimmering");
    }

    // An empty managed run is reaped when the owning session goes idle, so an
    // empty record does not linger indefinitely — and a later declaration no
    // longer revives it (contrast with the mid-turn survival above).
    #[tokio::test]
    async fn playbook_run_empty_clears_when_owning_session_idle() {
        use construct_protocol::SessionState;
        let md = "# A\n\n# B\n";
        let (mgr, _storage, id) = playbook_test_mgr(md).await;
        mgr.start_playbook_run(&id, md, false, None).expect("start");
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        let b = construct_protocol::playbook_block_id("# B");
        // Settle every block → pending empties; the record survives mid-turn.
        mgr.narrow_playbook_run(
            &id,
            md,
            &[
                construct_protocol::PlaybookShimmerDecl {
                    id: construct_protocol::playbook_block_id("# A"),
                    shimmer: false,
                    tooltip: None,
                },
                construct_protocol::PlaybookShimmerDecl {
                    id: b.clone(),
                    shimmer: false,
                    tooltip: None,
                },
            ],
        );
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "empty pending shows no shimmer"
        );
        // Owning session goes idle with nothing pending → the empty run is reaped.
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        // A re-declaration can no longer revive it: the record is gone.
        mgr.narrow_playbook_run(
            &id,
            md,
            &[construct_protocol::PlaybookShimmerDecl {
                id: b,
                shimmer: true,
                tooltip: None,
            }],
        );
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "reaped on idle; a re-declaration does not revive a cleared run"
        );
    }

    // A run the agent is actively managing (it has narrowed it with a
    // declaration/edit) must survive the owning session returning to idle —
    // the agent delegated work and its own turn ended while that work is still
    // pending. It clears only when its pending set empties (spec 0042).
    #[tokio::test]
    async fn playbook_run_managed_survives_idle_and_clears_on_settle() {
        use construct_protocol::SessionState;
        let body = "# Alpha\n\n# Beta\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;

        // Fresh run: unmanaged, both blocks pending.
        let run = mgr
            .start_playbook_run(&id, body, false, None)
            .expect("start_playbook_run");
        assert!(!run.agent_managed, "a fresh run is unmanaged");
        assert_eq!(run.pending_block_refs.len(), 2);

        // Owning session is seen running.
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        assert!(
            mgr.playbook_run_snapshot(&id)
                .expect("run present")
                .seen_running
        );

        // Planning-pass-style declaration narrows the run (text unchanged, so
        // both blocks stay pending) and marks it agent-managed.
        mgr.narrow_playbook_run(&id, body, &[]);
        let run = mgr.playbook_run_snapshot(&id).expect("managed run present");
        assert!(run.agent_managed, "an in-run declaration marks it managed");
        assert_eq!(run.pending_block_ids.len(), 2);

        // The agent delegates and its own turn ends → AwaitingInput. The
        // managed run must NOT clear: delegated work is still pending.
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("managed run survives the owning session going idle")
                .pending_block_ids
                .len(),
            2
        );

        // Repeated wake/idle cycles (e.g. a /loop monitor) keep it alive.
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        assert!(mgr.playbook_run_snapshot(&id).is_some(), "still pending");

        // Settle one block (its text changes, dropping its signature); the
        // other stays pending and the run lives on.
        mgr.playbook_update(PlaybookUpdateParams {
            session_id: id.clone(),
            markdown: "# Alpha done\n\n# Beta\n".into(),
            base_version: None,
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("settle alpha");
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("one block still pending")
                .pending_block_ids,
            vec![construct_protocol::playbook_block_id("# Beta")]
        );

        // Settling the last block empties the pending set → the run clears.
        mgr.playbook_update(PlaybookUpdateParams {
            session_id: id.clone(),
            markdown: "# Alpha done\n\n# Beta done\n".into(),
            base_version: None,
            actor: construct_protocol::PlaybookUpdateActor::Agent,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("settle beta");
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "an empty pending set clears the run"
        );
    }

    #[tokio::test]
    async fn playbook_run_system_status_tracks_dispatch_and_output_state() {
        use construct_protocol::{
            SessionState, PLAYBOOK_SHIMMER_STATUS_AGENT_WORKING, PLAYBOOK_SHIMMER_STATUS_DELIVERED,
            PLAYBOOK_SHIMMER_STATUS_QUEUED,
        };
        let body = "# Alpha\n\n# Beta\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;

        let run = mgr
            .start_playbook_run(&id, body, false, None)
            .expect("start idle-dispatched run");
        assert_eq!(
            run.system_status.as_deref(),
            Some(PLAYBOOK_SHIMMER_STATUS_DELIVERED)
        );

        let run = mgr
            .start_playbook_run_with_dispatch_state(&id, body, false, None, true, None)
            .expect("start queued run");
        assert_eq!(
            run.system_status.as_deref(),
            Some(PLAYBOOK_SHIMMER_STATUS_QUEUED)
        );
        // Dispatched into a session that is mid-turn: the Running already in
        // effect belongs to that turn, so it does not start this run (spec
        // 0176) — the status stays "queued" through it.
        mgr.mark_playbook_run_dispatched(&id, SessionState::Running);
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("queued snapshot")
                .system_status
                .as_deref(),
            Some(PLAYBOOK_SHIMMER_STATUS_QUEUED),
            "the turn already in flight at dispatch is not this run's turn"
        );

        // That turn ends, and the next one is ours.
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("running snapshot")
                .system_status
                .as_deref(),
            Some(PLAYBOOK_SHIMMER_STATUS_DELIVERED),
            "once this playbook turn starts it is no longer queued"
        );

        mgr.mark_playbook_run_output_seen(&id);
        assert_eq!(
            mgr.playbook_run_snapshot(&id)
                .expect("output snapshot")
                .system_status
                .as_deref(),
            Some(PLAYBOOK_SHIMMER_STATUS_AGENT_WORKING)
        );
    }

    /// Spec 0176. A Run is armed before its prompt goes out (#1122), so the
    /// transitions a session reports in that window are its own boot — a PTY
    /// harness announces `Running` on spawn and idles again at its first
    /// prompt — or the tail of the turn before. Reading them as this run's
    /// turn starting and ending settled the shimmer within milliseconds of
    /// arming it, which is what made both Playbook e2e tests flaky.
    #[tokio::test]
    async fn playbook_run_ignores_transitions_from_before_its_prompt_was_delivered() {
        use construct_protocol::SessionState;
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run_with_dispatch_state(&id, body, false, None, false, None)
            .expect("arm run");

        // The session's boot pair lands after the run was armed but before
        // its prompt was delivered.
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        assert!(
            mgr.playbook_run_snapshot(&id).is_some(),
            "a turn that ended before this run was dispatched is not this run's turn"
        );

        // Delivery, then the session's real turn: that one does stop it.
        mgr.mark_playbook_run_dispatched(&id, SessionState::AwaitingInput);
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        assert!(
            mgr.playbook_run_snapshot(&id)
                .expect("run present")
                .seen_running,
            "a Running after delivery is this run's turn"
        );
        mgr.note_session_state_for_playbook_run(&id, SessionState::AwaitingInput);
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "this run's own turn ending still stops it"
        );
    }

    /// Spec 0176. A fork Run's turn happens in the fork. The Playbook's own
    /// session is a bystander — it can boot, be typed in, and go idle any
    /// number of times while the fork works, and none of that settles the
    /// fork's blocks. Only the fork's own lifecycle does.
    #[tokio::test]
    async fn playbook_fork_run_reads_the_fork_lifecycle_not_the_owners() {
        use construct_protocol::SessionState;
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, owner) = playbook_test_mgr(body).await;
        let fork = "sfork".to_string();
        mgr.sessions.write().await.insert(
            fork.clone(),
            synthetic_entry(&fork, construct_protocol::SessionKind::User, 1),
        );
        mgr.start_playbook_run_with_dispatch_state(&owner, body, false, None, false, None)
            .expect("arm run");
        mgr.bind_playbook_run_execution(&owner, &fork);
        mgr.mark_playbook_run_dispatched(&fork, SessionState::AwaitingInput);

        // The owner runs a turn of its own and goes idle. The fork is still
        // working, so its blocks keep shimmering.
        mgr.note_session_state_for_playbook_run(&owner, SessionState::Running);
        mgr.note_session_state_for_playbook_run(&owner, SessionState::AwaitingInput);
        assert!(
            mgr.playbook_run_snapshot(&owner).is_some(),
            "the Playbook owner's own turn does not settle the fork's work"
        );

        // The fork's turn ending does.
        mgr.note_session_state_for_playbook_run(&fork, SessionState::Running);
        mgr.note_session_state_for_playbook_run(&fork, SessionState::AwaitingInput);
        assert!(
            mgr.playbook_run_snapshot(&owner).is_none(),
            "the fork finishing its turn stops the run it was dispatched for"
        );
    }

    /// Spec 0176 consequence: the Playbook's session dying still takes its
    /// run with it, even when a fork was executing it — nothing is left to
    /// render or settle those blocks (#1090).
    #[tokio::test]
    async fn playbook_fork_run_clears_when_the_playbook_session_dies() {
        use construct_protocol::SessionState;
        let body = "# T\n\n- alpha\n";
        let (mgr, _storage, owner) = playbook_test_mgr(body).await;
        mgr.start_playbook_run_with_dispatch_state(&owner, body, false, None, false, None)
            .expect("arm run");
        mgr.bind_playbook_run_execution(&owner, "sfork");
        mgr.note_session_state_for_playbook_run(&owner, SessionState::Errored);
        assert!(
            mgr.playbook_run_snapshot(&owner).is_none(),
            "a dead Playbook session clears its run whoever was executing it"
        );
    }

    // A terminal owning-session state clears even a managed run with pending
    // blocks: the agent is gone and can never settle them (spec 0042).
    #[tokio::test]
    async fn playbook_run_managed_clears_on_terminal_state() {
        use construct_protocol::SessionState;
        let body = "# Alpha\n\n# Beta\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start");
        mgr.note_session_state_for_playbook_run(&id, SessionState::Running);
        mgr.narrow_playbook_run(&id, body, &[]);
        assert!(
            mgr.playbook_run_snapshot(&id)
                .expect("managed")
                .agent_managed,
            "run is managed with pending blocks"
        );

        // Errored is terminal → clear despite still-pending blocks.
        mgr.note_session_state_for_playbook_run(&id, SessionState::Errored);
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "a terminal state clears a managed run"
        );
    }

    /// A planning declaration transfers ownership from the optimistic safety
    /// timer to explicit block/session lifecycle. Long delegated work must not
    /// lose its shimmer merely because no Playbook text changed for ten
    /// minutes (spec 0042).
    #[tokio::test]
    async fn playbook_run_managed_ignores_expired_safety_deadline() {
        let body = "# Work\n\n- delegated @{session:sworker}\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start");
        mgr.narrow_playbook_run(&id, body, &[]);
        {
            let mut runs = mgr.playbook_runs.lock().expect("runs");
            let run = runs.get_mut(&id).expect("managed run");
            assert!(run.agent_managed);
            run.expires_at_ms = 0;
        }

        let run = mgr
            .playbook_run_snapshot(&id)
            .expect("managed run survives its former deadline");
        assert_eq!(run.pending_block_count(), 2);
        let get = mgr.playbook_get(&id).await.expect("playbook get");
        assert!(
            get.blocks.iter().all(|block| block.shimmer),
            "managed pending blocks remain projected after the safety deadline"
        );
    }

    #[tokio::test]
    async fn playbook_run_unmanaged_still_expires_at_safety_deadline() {
        let body = "# Work\n\n- task\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start");
        mgr.playbook_runs
            .lock()
            .expect("runs")
            .get_mut(&id)
            .expect("unmanaged run")
            .expires_at_ms = 0;

        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "an untouched optimistic run keeps the safety backstop"
        );
    }

    /// Terminal session clips are the orphan signal for ordinary full-run
    /// delegation. Closing one worker settles only its own pending block; the
    /// remaining live worker keeps shimmering (spec 0042).
    #[tokio::test]
    async fn terminal_worker_clip_settles_only_its_pending_block() {
        use construct_protocol::SessionState;

        let body = "# Work\n\n- alpha @{session:sworker-a}\n\n- beta @{session:sworker-b}\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start");
        let blocks = mgr.playbook_get(&id).await.expect("get").blocks;
        let alpha = blocks
            .iter()
            .find(|block| block.text.contains("alpha"))
            .expect("alpha")
            .id
            .clone();
        let beta = blocks
            .iter()
            .find(|block| block.text.contains("beta"))
            .expect("beta")
            .id
            .clone();
        mgr.set_playbook_run_pending(
            &id,
            body,
            std::collections::HashMap::from([
                (alpha.clone(), None),
                (beta.clone(), None),
            ]),
        );

        mgr.note_session_state_for_playbook_run("sunrelated", SessionState::Done);
        assert!(
            mgr.playbook_run_snapshot(&id)
                .expect("unrelated close leaves run")
                .pending_block_refs
                .contains(&alpha)
        );

        mgr.note_session_state_for_playbook_run("sworker-a", SessionState::Done);
        let run = mgr
            .playbook_run_snapshot(&id)
            .expect("beta worker remains live");
        assert!(!run.pending_block_refs.contains(&alpha));
        assert_eq!(run.pending_block_refs, vec![beta]);

        mgr.note_session_state_for_playbook_run("sworker-b", SessionState::Errored);
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "all delegated blocks settle once their referenced workers terminate"
        );
        assert!(
            !mgr.playbook_runs.lock().expect("runs").contains_key(&id),
            "the terminal lifecycle signal is authoritative when it settles the last block"
        );
    }

    #[tokio::test]
    async fn archiving_and_deleting_workers_settle_their_pending_blocks() {
        let body = "# Work\n\n- alpha @{session:sworker-a}\n\n- beta @{session:sworker-b}\n";
        let (mgr, _storage, id) = playbook_test_mgr(body).await;
        {
            let mut sessions = mgr.sessions.write().await;
            sessions.insert(
                "sworker-a".into(),
                synthetic_entry("sworker-a", construct_protocol::SessionKind::User, 0),
            );
            sessions.insert(
                "sworker-b".into(),
                synthetic_entry("sworker-b", construct_protocol::SessionKind::User, 0),
            );
        }
        mgr.start_playbook_run(&id, body, false, None)
            .expect("start");
        let blocks = mgr.playbook_get(&id).await.expect("get").blocks;
        let alpha = blocks
            .iter()
            .find(|block| block.text.contains("alpha"))
            .expect("alpha")
            .id
            .clone();
        let beta = blocks
            .iter()
            .find(|block| block.text.contains("beta"))
            .expect("beta")
            .id
            .clone();
        mgr.set_playbook_run_pending(
            &id,
            body,
            std::collections::HashMap::from([
                (alpha.clone(), None),
                (beta.clone(), None),
            ]),
        );

        mgr.archive("sworker-a").await.expect("archive worker");
        let run = mgr
            .playbook_run_snapshot(&id)
            .expect("beta worker remains pending");
        assert!(!run.pending_block_refs.contains(&alpha));
        assert_eq!(run.pending_block_refs, vec![beta]);

        mgr.delete("sworker-b").await.expect("delete worker");
        assert!(
            mgr.playbook_run_snapshot(&id).is_none(),
            "archive and delete must not orphan managed shimmer"
        );
        assert!(!mgr.playbook_runs.lock().expect("runs").contains_key(&id));
    }

    #[tokio::test]
    async fn native_subagent_event_projects_read_only_child_and_transcript() {
        use construct_protocol::{MessageRole, NativeSubagentRef};
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(Config::default());
        let (manager, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("manager");
        let owner = synthetic_entry("owner", construct_protocol::SessionKind::User, 0);
        owner.summary.write().await.harness = "codex".into();
        manager
            .sessions
            .write()
            .await
            .insert(owner.id.clone(), owner.clone());

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "native-child".into(),
                    parent_id: None,
                    title: Some("Inspect parser".into()),
                    state: SessionState::Running,
                    event: Some(Box::new(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: "found it".into(),
                    })),
                    seq: None,
                },
            )
            .await;

        let projected_id = native_subagent_session_id("owner", "native-child");
        let child = manager
            .detail(&projected_id)
            .await
            .expect("projected child");
        assert_eq!(child.summary.parent_session_id.as_deref(), Some("owner"));
        assert_eq!(child.summary.title.as_deref(), Some("Inspect parser"));
        assert_eq!(child.summary.harness, "codex");
        assert_eq!(
            child.summary.native_subagent,
            Some(NativeSubagentRef {
                owner_session_id: "owner".into(),
                native_id: "native-child".into(),
                projected_seq: 0,
            })
        );
        assert!(matches!(
            child.events.as_slice(),
            [TimestampedEvent {
                event: SessionEvent::Message { text, .. },
                ..
            }] if text == "found it"
        ));
        assert!(
            child.summary.busy_running_since_ms.is_some(),
            "a Running native child opens a compute-time span (tracked \
             transition, not a bare state assignment)"
        );

        // The child exits: state Done must close the busy span and stamp
        // last_event_at — the lineage view uses that stamp to end the
        // child's lane on the timeline instead of letting it run to now.
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "native-child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Done,
                    event: None,
                    seq: None,
                },
            )
            .await;
        let child = manager.detail(&projected_id).await.expect("exited child");
        assert_eq!(child.summary.state, SessionState::Done);
        assert!(
            child.summary.archived,
            "a terminal native child is archived immediately"
        );
        assert!(
            child.summary.busy_running_since_ms.is_none(),
            "exiting banks the open compute-time span"
        );
        assert!(
            child.summary.last_event_at.is_some(),
            "the exit transition stamps last_event_at"
        );

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagentSnapshot { ids: Vec::new() },
            )
            .await;
        assert!(
            manager
                .detail(&projected_id)
                .await
                .expect("archived mirror")
                .summary
                .archived,
            "a child absent from the authoritative snapshot is archived"
        );

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagentSnapshot {
                    ids: vec!["native-child".into()],
                },
            )
            .await;
        let restored = manager
            .detail(&projected_id)
            .await
            .expect("restored mirror");
        assert!(
            restored.summary.archived,
            "retained transcript files do not resurrect an archived native child"
        );

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "native-child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Running,
                    event: None,
                    seq: None,
                },
            )
            .await;
        assert!(
            !manager
                .detail(&projected_id)
                .await
                .expect("active mirror")
                .summary
                .archived
        );

        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "native-child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Errored,
                    event: None,
                    seq: None,
                },
            )
            .await;
        let errored = manager.detail(&projected_id).await.expect("errored mirror");
        assert!(errored.summary.archived);
        assert_eq!(errored.summary.state, SessionState::Errored);

        manager
            .archive(&projected_id)
            .await
            .expect("archive mirror");
        assert!(
            manager
                .detail(&projected_id)
                .await
                .expect("removed mirror")
                .summary
                .archived
        );
    }

    /// A harness-native mirror is a read-only projection the user cannot
    /// drive, so it never raises the "needs you" dot no matter how its
    /// projected child events move it — the owning session is the row the
    /// user can actually act on. Spec 0054/0079.
    #[tokio::test]
    async fn native_mirror_never_raises_needs_attention() {
        use construct_protocol::MessageRole;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage = Arc::new(Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(Config::default());
        let (manager, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("manager");
        let owner = synthetic_entry("owner", construct_protocol::SessionKind::User, 0);
        owner.summary.write().await.harness = "claude".into();
        manager
            .sessions
            .write()
            .await
            .insert(owner.id.clone(), owner.clone());

        // The child works unwatched — genuine activity, which for an ordinary
        // session is exactly what makes the following stop "need you".
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: Some("Inspect parser".into()),
                    state: SessionState::Running,
                    event: Some(Box::new(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: "working".into(),
                    })),
                    seq: None,
                },
            )
            .await;

        let projected_id = native_subagent_session_id("owner", "child");
        let entry = manager.get_entry(&projected_id).await.expect("mirror");
        assert!(
            entry.unseen_activity.load(Ordering::Relaxed),
            "projected child output is still unseen activity — the marker is \
             suppressed by what the mirror IS, not by pretending it was idle"
        );

        // ...and then stops, unfocused. An ordinary session flags here.
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Running,
                    event: Some(Box::new(SessionEvent::Status {
                        state: SessionState::AwaitingInput,
                        detail: None,
                    })),
                    seq: None,
                },
            )
            .await;

        let mirror = manager.detail(&projected_id).await.expect("mirror").summary;
        assert_eq!(mirror.state, SessionState::AwaitingInput);
        assert!(
            !mirror.needs_attention,
            "a native mirror must not wear a dot the user has no way to act on"
        );

        // A marker persisted by an earlier build must not survive either:
        // the next projection retires it rather than letting it resurface
        // when the mirror unarchives.
        manager
            .get_entry(&projected_id)
            .await
            .expect("mirror")
            .summary
            .write()
            .await
            .needs_attention = true;
        manager
            .handle_event(
                &owner,
                SessionEvent::NativeSubagent {
                    id: "child".into(),
                    parent_id: None,
                    title: None,
                    state: SessionState::Running,
                    event: None,
                    seq: None,
                },
            )
            .await;
        assert!(
            !manager
                .detail(&projected_id)
                .await
                .expect("mirror")
                .summary
                .needs_attention,
            "a legacy stored marker is cleared on the next projection"
        );
    }
}
