//! pi coding agent adapter.
//!
//! Wraps the `pi` CLI (npm `@earendil-works/pi-coding-agent`). pi persists
//! each conversation as a JSONL session file whose records carry everything
//! construct wants to mirror: user/assistant messages with `thinking`,
//! `text`, and `toolCall` content blocks, `toolResult` messages, live
//! `model_change` / `thinking_level_change` records, and per-assistant-call
//! `usage` with a full token split *and* exact USD cost.
//!
//! Instead of tailing pi's global store (`~/.pi/agent/sessions/<cwd-slug>/`),
//! the adapter passes `--session-dir <CONSTRUCT_SESSION_DATA_DIR>/pi-sessions`
//! so every construct session owns a private store. That removes the
//! whole sibling-disambiguation problem other adapters fight (codex
//! originator tags, kimi sibling scans): the newest session file in the
//! private dir is ours by construction.
//!
//! Session files are named `<iso-timestamp>_<uuid>.jsonl`, so lexicographic
//! filename order is chronological order, and the file for a known uuid is
//! findable by suffix. The uuid from the `session` header record is the
//! native id persisted to `pi_session_id.txt` (resume via `--session
//! <path>`, native fork via `--fork <path>` — spec 0031/0078; reset
//! detection via newest-file rebinds — specs 0138/0085).
//!
//! Interactive mode runs pi's TUI under construct's PTY with the initial
//! prompt passed as a CLI argument (pi submits leading message arguments
//! itself). Headless mode spawns `pi -p --mode json` per turn and parses
//! the event stream from stdout; `message_end` records carry the same
//! message shape as the session file, so both modes share one translator.
//!
//! Honors `CONSTRUCT_PI_CMD` (full command prefix) then `CONSTRUCT_PI_BIN`,
//! defaulting to `pi` on PATH (npm installs there; pi has no fixed
//! installer home to fall back to).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use construct_adapter_common::context_breakdown::{estimate_tokens_from_chars, BreakdownGate};
use construct_adapter_common::{drive_turn, spawn_stderr_log, TurnOutcome};
use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::{
    Capabilities, ContextSegment, InitializeResult, MessageRole, PtySize, SessionEvent,
    SessionStartParams, SessionState,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const SESSION_ID_FILE: &str = "pi_session_id.txt";
const SESSIONS_SUBDIR: &str = "pi-sessions";

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "pi".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            supports_cost: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
        match resolve_mode(&params) {
            Mode::Interactive => run_interactive(params, ctx).await,
            Mode::Headless => run_headless(params, ctx).await,
        }
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Headless,
}

fn resolve_mode(params: &SessionStartParams) -> Mode {
    if let Ok(m) = std::env::var("CONSTRUCT_PI_MODE") {
        match m.as_str() {
            "interactive" => return Mode::Interactive,
            "headless" => return Mode::Headless,
            _ => {}
        }
    }
    match params.mode.as_deref() {
        Some("interactive") => Mode::Interactive,
        Some("headless") => Mode::Headless,
        _ if params.pty_size.is_some() => Mode::Interactive,
        _ => Mode::Headless,
    }
}

fn session_data_dir() -> Option<PathBuf> {
    std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// This construct session's private pi session store, created on demand.
fn pi_sessions_dir() -> Option<PathBuf> {
    let dir = session_data_dir()?.join(SESSIONS_SUBDIR);
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

fn conv_id_file() -> Option<PathBuf> {
    Some(session_data_dir()?.join(SESSION_ID_FILE))
}

fn read_conv_id() -> Option<String> {
    let path = conv_id_file()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_pi_session_id(s))
}

fn write_conv_id(id: &str) {
    if let Some(path) = conv_id_file() {
        let _ = std::fs::write(path, id);
    }
}

/// pi session ids are UUIDs (v7, lowercase). Anything else in an id file is
/// a half-write or foreign content and must not be replayed into `--session`
/// or `--fork`.
fn is_pi_session_id(value: &str) -> bool {
    value.len() == 36
        && value.chars().zip(0..).all(|(c, i)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}

/// Newest session file in `dir` by filename. pi names files
/// `<iso-timestamp>_<uuid>.jsonl` with zero-padded fields, so lexicographic
/// order is chronological order.
fn newest_session_file(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".jsonl") {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| name > b.as_str()) {
            best = Some((name.to_string(), path));
        }
    }
    best.map(|(_, path)| path)
}

/// The session file for a known uuid, by filename suffix.
fn session_file_for_id(dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("_{id}.jsonl");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
}

/// The session uuid from a session file's `{"type":"session","id":...}`
/// header record (always the first line).
fn header_session_id(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.lines().next()?;
    let v: Value = serde_json::from_str(first).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    is_pi_session_id(&id).then_some(id)
}

/// Resolve a native id to its session file for `--fork`: our own private
/// store first, then every sibling construct session's store under the same
/// sessions root. The sibling walk is what makes forking from a reset
/// snapshot work — the snapshot's data dir holds the retired id, but the
/// file itself still lives in the original session's store.
fn resolve_session_file(id: &str) -> Option<PathBuf> {
    let own_data_dir = session_data_dir()?;
    if let Some(path) = session_file_for_id(&own_data_dir.join(SESSIONS_SUBDIR), id) {
        return Some(path);
    }
    let sessions_root = own_data_dir.parent()?;
    for entry in std::fs::read_dir(sessions_root).ok()?.flatten() {
        let candidate = entry.path().join(SESSIONS_SUBDIR);
        if candidate == own_data_dir.join(SESSIONS_SUBDIR) {
            continue;
        }
        if let Some(path) = session_file_for_id(&candidate, id) {
            return Some(path);
        }
    }
    None
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Model/effort change-detection state shared by both modes (and, in
/// headless mode, across turns).
#[derive(Default)]
struct MetaState {
    last_model: Option<String>,
    last_effort: Option<String>,
}

/// `provider/modelId` when both are present — the shape pi's own `--model`
/// flag accepts, so the daemon can round-trip it into a respawn.
fn model_label(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    match (
        provider.filter(|s| !s.is_empty()),
        model.filter(|s| !s.is_empty()),
    ) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (None, Some(m)) => Some(m.to_string()),
        _ => None,
    }
}

/// Cost + context gauge from an assistant message's `usage` object
/// (specs 0103/0104). pi's `input` EXCLUDES cache reads/writes — verified
/// live: `totalTokens = input + cacheRead + output` (348 + 1024 + 5 = 1377)
/// — so the prompt side is input + cacheRead + cacheWrite, keeping
/// `tokens_cached ⊆ tokens_in` per the Cost contract. `reasoning` is a
/// subset of `output` (output 36 ⊇ reasoning 20 in the same live check),
/// so it is not added separately. pi prices every call itself
/// (`usage.cost.total`), and states no context window, so the gauge has no
/// denominator.
fn usage_events(usage: &Value) -> Vec<SessionEvent> {
    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let input = field("input");
    let output = field("output");
    let cache_read = field("cacheRead");
    let cache_write = field("cacheWrite");
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return Vec::new();
    }
    let prompt_side = input.saturating_add(cache_read).saturating_add(cache_write);
    let usd = usage
        .pointer("/cost/total")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    vec![
        SessionEvent::Cost {
            usd,
            tokens_in: prompt_side,
            tokens_out: output,
            tokens_cached: cache_read,
        },
        SessionEvent::ContextUsage {
            used_tokens: prompt_side,
            window_tokens: None,
        },
    ]
}

/// Conversation-content chars of one pi `message` object: `thinking`/`text`/
/// `toolCall` blocks, whatever the role (user and toolResult content is text
/// blocks; assistant adds thinking and tool calls). Signature blobs
/// (`thinkingSignature`, `textSignature`) are bookkeeping, not conversation
/// content, and are excluded.
fn message_content_chars(message: &Value) -> usize {
    let mut chars = 0usize;
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => {
                chars += block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
            }
            Some("text") => {
                chars += block
                    .get("text")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
            }
            Some("toolCall") => {
                chars += block
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                if let Some(args) = block.get("arguments") {
                    chars += args.to_string().len();
                }
            }
            _ => {}
        }
    }
    chars
}

/// Char sum over every `message` record in a session file. Deliberately a
/// full scan at gauge cadence (not an incremental tally): the figure
/// self-corrects across resume, rebinds, and anything pi rewrites behind us.
fn conversation_chars(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|v| v.get("message").map(message_content_chars))
        .sum()
}

/// Context breakdown behind the gauge (spec 0156): the session file IS the
/// conversation pi resends on the next call, so a char-heuristic sum over
/// its content is a usable `messages` estimate. One segment only — the fixed
/// prefix (system prompt, tools) never appears on pi's data surface, so it
/// lands in the client's synthetic `unaccounted` row. `None` while the file
/// has no conversation content yet.
fn breakdown_segments(path: &Path) -> Option<Vec<ContextSegment>> {
    let chars = conversation_chars(path);
    (chars > 0).then(|| {
        vec![ContextSegment::new(
            "messages",
            estimate_tokens_from_chars(chars),
            true,
        )]
    })
}

/// Emit the breakdown at the same cadence as the context gauge, gated on
/// change so identical rescans stay out of the transcript.
fn emit_breakdown(path: &Path, gate: &mut BreakdownGate, emit: &EventEmitter) {
    if let Some(segments) = breakdown_segments(path) {
        if gate.changed(&segments) {
            emit.emit(SessionEvent::ContextBreakdown { segments });
        }
    }
}

fn text_of_blocks(content: Option<&Value>) -> String {
    let Some(arr) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// Translate one pi `message` object (identical shape in the session file's
/// `message` records and headless `message_end` stdout records) into
/// session events. User records are skipped entirely: the daemon already
/// records the user text when it forwards the create prompt or a
/// `session.input` (same convention as the claude adapter), so mirroring
/// pi's copy would double every user turn.
fn message_events(message: &Value, meta: &mut MetaState) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            // The per-message `model` field is live (pi stamps the model
            // each call actually used), so it backs up the explicit
            // `model_change` records against mid-session switches.
            let label = model_label(
                message.get("provider").and_then(Value::as_str),
                message.get("model").and_then(Value::as_str),
            );
            if let Some(model) = label {
                if meta.last_model.as_deref() != Some(model.as_str()) {
                    meta.last_model = Some(model.clone());
                    events.push(SessionEvent::ModelChanged { model });
                }
            }
            for block in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match block.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                events.push(SessionEvent::Reasoning {
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                events.push(SessionEvent::Message {
                                    role: MessageRole::Assistant,
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                    Some("toolCall") => {
                        events.push(SessionEvent::ToolUse {
                            tool: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_string(),
                            args: block.get("arguments").cloned().unwrap_or(Value::Null),
                            call_id: block.get("id").and_then(Value::as_str).map(str::to_string),
                        });
                    }
                    _ => {}
                }
            }
            if let Some(usage) = message.get("usage") {
                events.extend(usage_events(usage));
            }
        }
        Some("toolResult") => {
            events.push(SessionEvent::ToolResult {
                tool: message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                ok: !message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                output: text_of_blocks(message.get("content")),
                call_id: message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        _ => {}
    }
    events
}

/// Translate one session-file record into session events.
fn record_events(v: &Value, meta: &mut MetaState) -> Vec<SessionEvent> {
    match v.get("type").and_then(Value::as_str) {
        Some("model_change") => {
            let label = model_label(
                v.get("provider").and_then(Value::as_str),
                v.get("modelId").and_then(Value::as_str),
            );
            let Some(model) = label else {
                return Vec::new();
            };
            if meta.last_model.as_deref() == Some(model.as_str()) {
                return Vec::new();
            }
            meta.last_model = Some(model.clone());
            vec![SessionEvent::ModelChanged { model }]
        }
        Some("thinking_level_change") => {
            let Some(effort) = v.get("thinkingLevel").and_then(Value::as_str) else {
                return Vec::new();
            };
            if meta.last_effort.as_deref() == Some(effort) {
                return Vec::new();
            }
            meta.last_effort = Some(effort.to_string());
            vec![SessionEvent::EffortChanged {
                effort: effort.to_string(),
            }]
        }
        Some("message") => match v.get("message") {
            Some(message) => message_events(message, meta),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Re-frame line-oriented `Input` as two PTY writes — the text, then a lone
/// carriage return after a grace delay — forwarding every other inbox
/// message unchanged. The delay lets pi's paste heuristics settle so the CR
/// registers as a real Enter keypress.
fn interpose_typed_input(
    mut real: mpsc::Receiver<AdapterInboxMsg>,
) -> mpsc::Receiver<AdapterInboxMsg> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        while let Some(msg) = real.recv().await {
            match msg {
                AdapterInboxMsg::Input(text) => {
                    if tx
                        .send(AdapterInboxMsg::PtyInput(text.into_bytes()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if tx
                        .send(AdapterInboxMsg::PtyInput(vec![b'\r']))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                other => {
                    if tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

async fn run_interactive(params: SessionStartParams, mut ctx: AdapterContext) {
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_PI_CMD",
        "CONSTRUCT_PI_BIN",
        "pi",
    );
    let mut args = command.args.clone();
    args.extend(params.args.clone());

    let store = pi_sessions_dir();
    if let Some(dir) = store.as_ref() {
        args.extend(["--session-dir".into(), dir.to_string_lossy().into_owned()]);
    } else {
        ctx.emit.log(
            "pi: no CONSTRUCT_SESSION_DATA_DIR — falling back to pi's global session store; \
             resume and native-id tracking are disabled",
        );
    }

    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    // Resume the exact captured conversation by file path. No fallback to
    // `--continue`: the private store makes "newest file" unambiguous, but a
    // missing/half-written id file usually means the previous spawn never
    // got far enough to converse — a fresh conversation is less surprising
    // than silently re-entering something we can't name.
    let resume_path = resuming
        .then(|| {
            let id = read_conv_id()?;
            let path = store.as_ref().and_then(|d| session_file_for_id(d, &id));
            if path.is_none() {
                ctx.emit.log(format!(
                    "pi respawn: captured session {id} has no file in the private store; \
                     starting a fresh conversation"
                ));
            }
            path
        })
        .flatten();
    if resuming && resume_path.is_none() && read_conv_id().is_none() {
        ctx.emit
            .log("pi respawn: no captured native session id; starting a fresh conversation");
    }
    if let Some(path) = resume_path.as_ref() {
        args.extend(["--session".into(), path.to_string_lossy().into_owned()]);
    }

    // Same-harness fork (spec 0031/0078): the daemon passes the parent's
    // captured uuid; pi forks a session file into OUR private store.
    let fork_path = (!resuming)
        .then(|| {
            let parent = std::env::var("CONSTRUCT_PI_FORK_FROM")
                .ok()
                .filter(|s| is_pi_session_id(s))?;
            let path = resolve_session_file(&parent);
            if path.is_none() {
                ctx.emit.log(format!(
                    "pi fork: parent session {parent} not found in any construct pi store; \
                     starting fresh without parent context"
                ));
            }
            path
        })
        .flatten();
    if let Some(path) = fork_path.as_ref() {
        args.extend(["--fork".into(), path.to_string_lossy().into_owned()]);
    }

    if let Some(model) = params.model.as_ref() {
        args.extend(["--model".into(), model.clone()]);
    }

    // pi submits leading message arguments itself in interactive mode, so
    // the initial prompt rides the command line — no PTY typing dance.
    let resuming_existing = resume_path.is_some();
    if !resuming_existing {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.clone());
        }
    }

    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), ctx.session_id.clone()));

    // Deliver `session.input` text as two separate PTY writes: the text,
    // then a lone carriage return after a short grace. pi's composer treats
    // a single chunk arriving with its terminator as one paste (bracketed
    // paste), leaving the text sitting unsubmitted — the same behavior kimi
    // documented and solved this way. The generic PTY runner's one-write
    // `text\n` framing never submits (verified live against pi 0.82.0).
    ctx.inbox = interpose_typed_input(std::mem::replace(&mut ctx.inbox, mpsc::channel(1).1));

    if let Some(dir) = store {
        spawn_session_watcher(
            WatcherSetup {
                dir,
                resume_path,
                initial_model: params.model.clone(),
            },
            ctx.emit.clone(),
        );
    }

    let label = command.argv_preview();
    let spec = PtySpec {
        bin: command.bin,
        args,
        cwd: PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
        // Full-screen TUI: holds the foreground group; use daemon quiescence.
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty(spec, ctx).await;
}

struct WatcherSetup {
    dir: PathBuf,
    /// The session file we relaunched with on resume; its history is
    /// already in the daemon's transcript, so the cursor starts at its end.
    /// `None` on fresh spawns AND forks — a forked file's copied parent
    /// history deliberately backfills into the child construct session's
    /// transcript from line 0 (same behavior as the codex adapter).
    resume_path: Option<PathBuf>,
    /// Model the daemon asked for at launch; seeds change detection so a
    /// spawn on the requested model stays quiet.
    initial_model: Option<String>,
}

/// Watch the private session store: bind to the newest session file, mirror
/// its records into session events, follow rebinds (pi's `/new` mints a new
/// file — spec 0138/0085), and keep `pi_session_id.txt` pointing at the
/// live conversation.
fn spawn_session_watcher(setup: WatcherSetup, emit: EventEmitter) {
    tokio::spawn(async move {
        let WatcherSetup {
            dir,
            resume_path,
            initial_model,
        } = setup;
        let mut meta = MetaState {
            last_model: initial_model,
            last_effort: None,
        };
        let mut current: Option<(PathBuf, String)> = resume_path.and_then(|path| {
            let id = header_session_id(&path)?;
            Some((path, id))
        });
        let mut cursor = current
            .as_ref()
            .map(|(path, _)| count_lines(path))
            .unwrap_or(0);
        let mut breakdown_gate = BreakdownGate::default();

        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;

            if let Some(newest) = newest_session_file(&dir) {
                let is_new = current.as_ref().map(|(p, _)| p != &newest).unwrap_or(true);
                if is_new {
                    // The header is the file's first write; until it parses,
                    // leave the binding alone and retry next tick.
                    if let Some(id) = header_session_id(&newest) {
                        if let Some((_, prior_id)) = current.as_ref() {
                            emit.log(format!(
                                "pi: native session id changed {prior_id} -> {id}; rebinding"
                            ));
                            emit.emit(SessionEvent::NativeIdChanged {
                                prior_native_id: prior_id.clone(),
                                new_native_id: id.clone(),
                            });
                        }
                        write_conv_id(&id);
                        cursor = 0;
                        current = Some((newest, id));
                    }
                } else if current
                    .as_ref()
                    .is_some_and(|(_, id)| header_session_id(&newest).is_some_and(|h| &h != id))
                {
                    // Same path, rewritten header (observed once live: a
                    // `--continue` rewrote the file under a fresh uuid while
                    // keeping the history). Follow the id; history already
                    // mirrored stays valid, so the cursor holds.
                    if let (Some(new_id), Some((path, prior_id))) =
                        (header_session_id(&newest), current.take())
                    {
                        emit.emit(SessionEvent::NativeIdChanged {
                            prior_native_id: prior_id,
                            new_native_id: new_id.clone(),
                        });
                        write_conv_id(&new_id);
                        current = Some((path, new_id));
                    }
                }
            }

            let Some((path, _)) = current.as_ref() else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut saw_context_usage = false;
            for (idx, line) in text.lines().enumerate() {
                if idx < cursor || line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                for event in record_events(&v, &mut meta) {
                    saw_context_usage |= matches!(event, SessionEvent::ContextUsage { .. });
                    emit.emit(event);
                }
            }
            cursor = text.lines().count();
            if saw_context_usage {
                emit_breakdown(path, &mut breakdown_gate, &emit);
            }
        }
    });
}

async fn run_headless(params: SessionStartParams, mut ctx: AdapterContext) {
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_PI_CMD",
        "CONSTRUCT_PI_BIN",
        "pi",
    );
    let emit = ctx.emit.clone();
    let store = pi_sessions_dir();

    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let mut session_path: Option<PathBuf> = resuming
        .then(|| {
            let id = read_conv_id()?;
            store.as_ref().and_then(|d| session_file_for_id(d, &id))
        })
        .flatten();
    let mut fork_path = (!resuming)
        .then(|| {
            let parent = std::env::var("CONSTRUCT_PI_FORK_FROM")
                .ok()
                .filter(|s| is_pi_session_id(s))?;
            resolve_session_file(&parent)
        })
        .flatten();

    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(prompt) = params.prompt.as_ref() {
        // On resume the prior conversation continues; the original seed
        // prompt was already consumed by the first spawn.
        if !resuming && !prompt.trim().is_empty() {
            pending.push_back(prompt.clone());
        }
    }

    let meta = Arc::new(StdMutex::new(MetaState {
        last_model: params.model.clone(),
        last_effort: None,
    }));
    let mut breakdown_gate = BreakdownGate::default();

    let exit_code = loop {
        let user_text = match pending.pop_front() {
            Some(t) => t,
            None => {
                emit.emit(SessionEvent::AwaitingInput { prompt: None });
                match ctx.inbox.recv().await {
                    None => break 0,
                    Some(AdapterInboxMsg::Input(t)) => t,
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::Stop) => break 0,
                    Some(AdapterInboxMsg::PtyInput(_))
                    | Some(AdapterInboxMsg::PtyResize { .. })
                    | Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => continue,
                }
            }
        };
        if user_text.trim().is_empty() {
            continue;
        }

        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });

        let mut child_args: Vec<String> = command.args.clone();
        child_args.extend(["-p".into(), "--mode".into(), "json".into()]);
        if let Some(dir) = store.as_ref() {
            child_args.extend(["--session-dir".into(), dir.to_string_lossy().into_owned()]);
        }
        if let Some(path) = session_path.as_ref() {
            child_args.extend(["--session".into(), path.to_string_lossy().into_owned()]);
        } else if let Some(path) = fork_path.take() {
            child_args.extend(["--fork".into(), path.to_string_lossy().into_owned()]);
        }
        if let Some(model) = params.model.as_ref() {
            child_args.extend(["--model".into(), model.clone()]);
        }
        for a in &params.args {
            child_args.push(a.clone());
        }
        child_args.push(user_text.clone());

        let mut cmd = Command::new(&command.bin);
        for a in &child_args {
            cmd.arg(a);
        }
        cmd.current_dir(&params.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &params.env {
            cmd.env(k, v);
        }
        cmd.env("CONSTRUCT_SESSION_ID", &ctx.session_id);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: construct_protocol::adapter::missing_bin_hint(
                        &command.argv_preview(),
                        &e,
                    ),
                });
                break 127;
            }
        };

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let stdout_task = spawn_stdout(
            child_stdout,
            emit.clone(),
            meta.clone(),
            captured_sid.clone(),
        );
        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());

        let outcome = drive_turn(&mut child, &mut ctx.inbox, &emit, &mut pending).await;

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        // Adopt the session the turn actually ran as (fresh spawn, fork, or
        // a continue that re-minted the uuid) so the next turn and a daemon
        // respawn both target it.
        if let Some(sid) = captured_sid.lock().unwrap().clone() {
            write_conv_id(&sid);
            if let Some(dir) = store.as_ref() {
                // The file lands on child exit, which has already happened.
                if let Some(path) = session_file_for_id(dir, &sid) {
                    session_path = Some(path);
                }
            }
        }

        // Per-turn is the gauge's headless cadence (usage arrives once per
        // `message_end`), and by now the session file has landed, so the
        // full rescan sees the whole conversation the turn produced.
        if let Some(path) = session_path.as_ref() {
            emit_breakdown(path, &mut breakdown_gate, &emit);
        }

        match outcome {
            TurnOutcome::Completed => continue,
            TurnOutcome::Interrupted => {
                emit.log("turn interrupted; awaiting next input");
                continue;
            }
            TurnOutcome::Stopped => break 0,
        }
    };

    emit.emit(SessionEvent::Done { exit_code });
}

/// Parse pi's `--mode json` stdout stream. `message_end` records carry the
/// final message (same shape as session-file records — one per API call,
/// with final usage), which is everything construct mirrors; deltas and
/// lifecycle records are skipped. The `session` header record carries the
/// uuid for resume. Non-JSON lines are surfaced as adapter logs, never as
/// assistant prose.
fn spawn_stdout<R>(
    reader: R,
    emit: EventEmitter,
    meta: Arc<StdMutex<MetaState>>,
    captured_sid: Arc<StdMutex<Option<String>>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                emit.log(format!("pi stdout: {line}"));
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("session") => {
                    if let Some(id) = v.get("id").and_then(Value::as_str) {
                        if is_pi_session_id(id) {
                            *captured_sid.lock().unwrap() = Some(id.to_string());
                        }
                    }
                }
                Some("message_end") => {
                    if let Some(message) = v.get("message") {
                        let mut meta = meta.lock().unwrap();
                        for event in message_events(message, &mut meta) {
                            emit.emit(event);
                        }
                    }
                }
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // All record literals below are real lines captured from pi 0.82.0
    // session files / `--mode json` output on this machine (2026-07-24),
    // trimmed only of the opaque signature blobs.

    fn meta() -> MetaState {
        MetaState::default()
    }

    #[test]
    fn header_id_parses_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("2026-07-24T16-13-57-159Z_019f94e7-a627-75fa-8509-c8d85654e609.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"019f94e7-a627-75fa-8509-c8d85654e609","timestamp":"2026-07-24T16:13:57.159Z","cwd":"/w"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            header_session_id(&path).as_deref(),
            Some("019f94e7-a627-75fa-8509-c8d85654e609")
        );

        assert!(is_pi_session_id("019f94e7-a627-75fa-8509-c8d85654e609"));
        assert!(!is_pi_session_id("019F94E7-A627-75FA-8509-C8D85654E609"));
        assert!(!is_pi_session_id("session_019f94e7"));
        assert!(!is_pi_session_id(""));
    }

    #[test]
    fn newest_session_file_is_lexicographically_last() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            "2026-07-24T16-12-49-586Z_019f94e6-9e32-75b8-99c4-150adee35c8a.jsonl",
            "2026-07-24T16-13-57-159Z_019f94e7-a627-75fa-8509-c8d85654e609.jsonl",
            "notes.txt",
        ] {
            std::fs::write(tmp.path().join(name), "").unwrap();
        }
        assert_eq!(
            newest_session_file(tmp.path())
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            Some("2026-07-24T16-13-57-159Z_019f94e7-a627-75fa-8509-c8d85654e609.jsonl".into())
        );
        assert_eq!(
            session_file_for_id(tmp.path(), "019f94e6-9e32-75b8-99c4-150adee35c8a")
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            Some("2026-07-24T16-12-49-586Z_019f94e6-9e32-75b8-99c4-150adee35c8a.jsonl".into())
        );
        assert_eq!(session_file_for_id(tmp.path(), "missing-id"), None);
    }

    #[test]
    fn model_and_effort_changes_dedupe() {
        let mut meta = meta();
        let model = serde_json::json!({
            "type":"model_change","id":"635a776d","parentId":null,
            "timestamp":"2026-07-24T16:08:07.023Z","provider":"openai","modelId":"gpt-5.5"
        });
        match record_events(&model, &mut meta).as_slice() {
            [SessionEvent::ModelChanged { model }] => assert_eq!(model, "openai/gpt-5.5"),
            other => panic!("expected ModelChanged: {other:?}"),
        }
        assert!(record_events(&model, &mut meta).is_empty());

        let effort = serde_json::json!({
            "type":"thinking_level_change","id":"17fa169f","parentId":"635a776d",
            "timestamp":"2026-07-24T16:08:07.023Z","thinkingLevel":"medium"
        });
        match record_events(&effort, &mut meta).as_slice() {
            [SessionEvent::EffortChanged { effort }] => assert_eq!(effort, "medium"),
            other => panic!("expected EffortChanged: {other:?}"),
        }
        assert!(record_events(&effort, &mut meta).is_empty());
    }

    #[test]
    fn assistant_message_emits_tool_use_and_split_usage() {
        // Real assistant record (write tool call), usage from the real "OK"
        // turn: totalTokens 1377 = input 348 + cacheRead 1024 + output 5,
        // proving `input` excludes cache reads.
        let mut meta = meta();
        let v = serde_json::json!({
            "type":"message","id":"aaef1d8c","parentId":null,"timestamp":"2026-07-24T16:12:51.0Z",
            "message":{
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":""},
                    {"type":"toolCall","id":"call_T27eYYfD60ur79KfMdy2SPnG|fc_0bee","name":"write",
                     "arguments":{"path":"hello.txt","content":"hello"}}
                ],
                "api":"openai-responses","provider":"openai","model":"gpt-5.5",
                "usage":{"input":348,"output":5,"cacheRead":1024,"cacheWrite":0,"reasoning":0,
                          "totalTokens":1377,
                          "cost":{"input":0.00174,"output":0.00015,"cacheRead":0.000512,
                                   "cacheWrite":0,"total":0.002402}},
                "stopReason":"toolUse","timestamp":1784909571000i64
            }
        });
        let events = record_events(&v, &mut meta);
        match events.as_slice() {
            [SessionEvent::ModelChanged { model }, SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }, SessionEvent::Cost {
                usd,
                tokens_in,
                tokens_out,
                tokens_cached,
            }, SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            }] => {
                assert_eq!(model, "openai/gpt-5.5");
                assert_eq!(tool, "write");
                assert_eq!(
                    args.pointer("/path").and_then(Value::as_str),
                    Some("hello.txt")
                );
                assert_eq!(
                    call_id.as_deref(),
                    Some("call_T27eYYfD60ur79KfMdy2SPnG|fc_0bee")
                );
                assert_eq!(*tokens_in, 348 + 1024);
                assert_eq!(*tokens_out, 5);
                assert_eq!(*tokens_cached, 1024);
                assert!((usd - 0.002402).abs() < 1e-9);
                assert_eq!(*used_tokens, 348 + 1024);
                assert_eq!(*window_tokens, None);
            }
            other => panic!("expected model+tool+cost+context: {other:?}"),
        }
        // Empty thinking blocks never become Reasoning events.
        assert!(!events
            .iter()
            .any(|e| matches!(e, SessionEvent::Reasoning { .. })));
        // Same model on the next message stays quiet.
        assert!(record_events(&v, &mut meta)
            .iter()
            .all(|e| !matches!(e, SessionEvent::ModelChanged { .. })));
    }

    #[test]
    fn tool_result_maps_name_ok_and_call_id() {
        let mut meta = meta();
        let v = serde_json::json!({
            "type":"message","id":"f6fd5d17","parentId":"27248213","timestamp":"2026-07-24T16:12:51.308Z",
            "message":{
                "role":"toolResult",
                "toolCallId":"call_T27eYYfD60ur79KfMdy2SPnG|fc_0bee",
                "toolName":"write",
                "content":[{"type":"text","text":"Successfully wrote 5 bytes to hello.txt"}],
                "isError":false,"timestamp":1784909571308i64
            }
        });
        match record_events(&v, &mut meta).as_slice() {
            [SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            }] => {
                assert_eq!(tool, "write");
                assert!(*ok);
                assert_eq!(output, "Successfully wrote 5 bytes to hello.txt");
                assert_eq!(
                    call_id.as_deref(),
                    Some("call_T27eYYfD60ur79KfMdy2SPnG|fc_0bee")
                );
            }
            other => panic!("expected ToolResult: {other:?}"),
        }
    }

    #[test]
    fn user_messages_are_never_mirrored() {
        // The daemon records user text itself when forwarding the create
        // prompt or a `session.input` (claude-adapter convention); mirroring
        // pi's copy would double every user turn — verified live before the
        // fix.
        let mut meta = meta();
        let v = serde_json::json!({
            "type":"message","id":"ee76398c","parentId":"17fa169f","timestamp":"2026-07-24T16:08:09.403Z",
            "message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1784909289402i64}
        });
        assert!(record_events(&v, &mut meta).is_empty());
    }

    #[test]
    fn usage_with_all_zero_tokens_is_dropped() {
        // Real `message_start` partials carry an all-zero usage stub; only
        // the final message's real usage may become a Cost event.
        let usage = serde_json::json!({
            "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}
        });
        assert!(usage_events(&usage).is_empty());
    }

    #[test]
    fn resume_cursor_skips_persisted_history() {
        // Replaying a file from a cursor must yield only the appended
        // records — the exactly-once contract for resume.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let line1 = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}"#;
        let line2 = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#;
        std::fs::write(&path, format!("{line1}\n")).unwrap();
        let cursor = count_lines(&path);
        std::fs::write(&path, format!("{line1}\n{line2}\n")).unwrap();

        let mut meta = meta();
        let mut seen = Vec::new();
        let text = std::fs::read_to_string(&path).unwrap();
        for (idx, line) in text.lines().enumerate() {
            if idx < cursor {
                continue;
            }
            let v: Value = serde_json::from_str(line).unwrap();
            seen.extend(record_events(&v, &mut meta));
        }
        match seen.as_slice() {
            [SessionEvent::Message { text, .. }] => assert_eq!(text, "second"),
            other => panic!("expected only the appended record: {other:?}"),
        }
    }

    /// A representative session file mirroring the real pi 0.82.0 records
    /// inspected on this machine (2026-07-28): header, bookkeeping records,
    /// then a user text block, an assistant thinking+toolCall message with
    /// signature blobs, a toolResult message, and a final assistant text
    /// with its signature.
    fn write_breakdown_fixture(path: &Path) -> usize {
        let user_text = "run date";
        let thinking = "The user wants the current date; run the bash tool.";
        let tool_name = "bash";
        let tool_args = r#"{"command":"date"}"#;
        let tool_output = "Mon Jul 28 10:00:00 KST 2026";
        let reply = "It is Mon Jul 28 10:00:00 KST 2026.";
        let lines = [
            r#"{"type":"session","version":3,"id":"019f9bc4-ca98-7527-9f5d-7ffa067500e2","timestamp":"2026-07-26T00:13:13.240Z","cwd":"/w"}"#.to_string(),
            r#"{"type":"model_change","id":"85c02dea","parentId":null,"timestamp":"2026-07-26T00:13:13.252Z","provider":"openai-codex","modelId":"gpt-5.6-sol"}"#.to_string(),
            format!(
                r#"{{"type":"message","id":"a1","message":{{"role":"user","content":[{{"type":"text","text":"{user_text}"}}],"timestamp":1785000000000}}}}"#
            ),
            format!(
                r#"{{"type":"message","id":"a2","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{thinking}","thinkingSignature":"OPAQUE-SIGNATURE-BLOB-MUST-NOT-COUNT"}},{{"type":"toolCall","id":"call_1|fc_1","name":"{tool_name}","arguments":{tool_args}}}],"provider":"openai-codex","model":"gpt-5.6-sol","usage":{{"input":100,"output":10,"cacheRead":0,"cacheWrite":0,"totalTokens":110,"cost":{{"total":0.001}}}},"stopReason":"toolUse"}}}}"#
            ),
            format!(
                r#"{{"type":"message","id":"a3","message":{{"role":"toolResult","toolCallId":"call_1|fc_1","toolName":"bash","content":[{{"type":"text","text":"{tool_output}"}}],"isError":false}}}}"#
            ),
            format!(
                r#"{{"type":"message","id":"a4","message":{{"role":"assistant","content":[{{"type":"text","text":"{reply}","textSignature":"OPAQUE-TEXT-SIGNATURE"}}],"provider":"openai-codex","model":"gpt-5.6-sol","usage":{{"input":160,"output":12,"cacheRead":0,"cacheWrite":0,"totalTokens":172,"cost":{{"total":0.002}}}},"stopReason":"stop"}}}}"#
            ),
        ];
        std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
        user_text.len()
            + thinking.len()
            + tool_name.len()
            + tool_args.len()
            + tool_output.len()
            + reply.len()
    }

    #[test]
    fn conversation_chars_sums_content_and_skips_signatures() {
        // Char sum covers thinking/text/toolCall blocks of every message
        // record (user, assistant, toolResult) and nothing else — signature
        // blobs, headers, and model_change bookkeeping stay out.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("2026-07-26T00-13-13-240Z_019f9bc4-ca98-7527-9f5d-7ffa067500e2.jsonl");
        let expected = write_breakdown_fixture(&path);
        assert_eq!(conversation_chars(&path), expected);
        assert_eq!(conversation_chars(&tmp.path().join("missing.jsonl")), 0);
    }

    #[test]
    fn breakdown_reports_single_estimated_segment_and_gates_repeats() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        let chars = write_breakdown_fixture(&path);

        let segments = breakdown_segments(&path).expect("content present");
        match segments.as_slice() {
            [ContextSegment {
                label,
                tokens,
                estimated,
            }] => {
                assert_eq!(label, "messages");
                assert_eq!(*tokens, estimate_tokens_from_chars(chars));
                assert!(*estimated, "char heuristic must be marked estimated");
            }
            other => panic!("expected one messages segment: {other:?}"),
        }

        // Report-on-change: an identical rescan stays quiet; growth reports.
        let mut gate = BreakdownGate::default();
        assert!(gate.changed(&segments));
        assert!(!gate.changed(&breakdown_segments(&path).unwrap()));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            r#"{"type":"message","id":"a5","message":{"role":"user","content":[{"type":"text","text":"more"}]}}"#,
        );
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        assert!(gate.changed(&breakdown_segments(&path).unwrap()));

        // A file with no conversation content yet reports nothing at all
        // (spec 0156: only what the data surface makes derivable).
        let empty = tmp.path().join("empty.jsonl");
        std::fs::write(
            &empty,
            concat!(
                r#"{"type":"session","version":3,"id":"019f94e7-a627-75fa-8509-c8d85654e609","timestamp":"2026-07-24T16:13:57.159Z","cwd":"/w"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(breakdown_segments(&empty), None);
    }

    #[test]
    fn model_label_prefers_provider_qualified_form() {
        assert_eq!(
            model_label(Some("openai"), Some("gpt-5.5")).as_deref(),
            Some("openai/gpt-5.5")
        );
        assert_eq!(
            model_label(None, Some("gpt-5.5")).as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(model_label(Some("openai"), None), None);
        assert_eq!(model_label(Some(""), Some("")), None);
    }
}
