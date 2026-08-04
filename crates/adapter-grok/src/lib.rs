//! Grok CLI adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns `grok`
//!   under a PTY, giving the user the real Grok TUI experience.
//!
//! - **headless (opt-in)** — multi-turn structured mode that spawns
//!   `grok -p <prompt> --output-format streaming-json` per turn.
//!
//! Pick mode via `--mode interactive|headless` on `construct new`, or via
//! `CONSTRUCT_GROK_MODE=interactive|headless`. Honors `CONSTRUCT_GROK_CMD` for a
//! full command prefix, falling back to `CONSTRUCT_GROK_BIN` for a binary path.

use construct_adapter_common::{
    context_breakdown::{estimate_tokens_from_chars, BreakdownGate, FixedOverheadPin},
    drive_turn, next_native_seq, spawn_stderr_log, TurnOutcome,
};
use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::{
    Capabilities, ContextSegment, InitializeResult, MessageRole, PtySize, SessionEvent,
    SessionStartParams, SessionState,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "grok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            // Real per-turn usage, read from the session's `updates.jsonl`
            // `turn_completed` records (see `grok_usage_events`).
            supports_cost: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
        match resolve_mode(&params) {
            Mode::Interactive => run_interactive(params, ctx).await,
            Mode::Headless => run_session(params, ctx).await,
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
    if let Ok(m) = std::env::var("CONSTRUCT_GROK_MODE") {
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

fn command_override() -> construct_protocol::adapter::CommandOverride {
    construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_GROK_CMD",
        "CONSTRUCT_GROK_BIN",
        "grok",
    )
}

fn session_data_dir() -> Option<PathBuf> {
    std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(PathBuf::from)
}

fn conv_id_file() -> Option<PathBuf> {
    Some(session_data_dir()?.join("grok_session_id.txt"))
}

fn read_conv_id() -> Option<String> {
    let p = conv_id_file()?;
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_conv_id(id: &str) {
    if let Some(p) = conv_id_file() {
        let _ = std::fs::write(p, id);
    }
}

fn grok_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CONSTRUCT_GROK_HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("GROK_HOME") {
        return Some(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".grok"))
}

fn url_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut encoded = String::new();
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                encoded.push(c);
            }
            '/' => {
                encoded.push_str("%2F");
            }
            _ => {
                for byte in c.to_string().bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
fn find_session_id(cwd: &Path) -> Option<String> {
    find_session_id_excluding(cwd, &HashSet::new())
}

fn find_session_id_excluding(cwd: &Path, excluded: &HashSet<String>) -> Option<String> {
    let sessions_dir = grok_home()?.join("sessions").join(url_encode_path(cwd));
    find_session_id_excluding_in(&sessions_dir, excluded)
}

fn find_session_id_excluding_in(sessions_dir: &Path, excluded: &HashSet<String>) -> Option<String> {
    if !sessions_dir.exists() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.len() == 36
                        && !excluded.contains(&name)
                        && !grok_session_is_fork(&sessions_dir.join(&name))
                    {
                        if let Ok(metadata) = entry.metadata() {
                            if let Ok(modified) = metadata.modified() {
                                if best.is_none() || modified > best.as_ref().unwrap().0 {
                                    best = Some((modified, name));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Native Grok session ids that already exist for `cwd`.
///
/// A resumed construct session is already bound to one of these ids. All of
/// the others are historical or belong to sibling construct sessions, so they
/// must never be considered evidence that this session was cleared. A real
/// `/clear` creates a fresh UUID after the adapter starts.
fn existing_session_ids(cwd: &Path) -> HashSet<String> {
    let Some(sessions_dir) =
        grok_home().map(|home| home.join("sessions").join(url_encode_path(cwd)))
    else {
        return HashSet::new();
    };
    existing_session_ids_in(&sessions_dir)
}

fn existing_session_ids_in(sessions_dir: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.len() == 36)
        .collect()
}

/// Whether a grok session dir was created by `--fork-session`
/// (`summary.json` stamps `session_kind: "fork"` plus the source's
/// `parent_session_id`). Forks are named up front and bound directly by
/// their own watcher — newest-dir DISCOVERY must never rebind another
/// session (typically the fork's parent, sharing this cwd) onto them.
fn grok_session_is_fork(session_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(session_dir.join("summary.json")) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("session_kind")
                .and_then(|k| k.as_str())
                .map(|k| k == "fork")
        })
        .unwrap_or(false)
}

/// Other construct sessions' grok native ids, for sessions that share this
/// one's `cwd` (read from each sibling's own `grok_session_id.txt`, next to
/// its `meta.json`, both written by the daemon under
/// `<data_dir>/sessions/<construct_id>/`).
///
/// Grok organizes its own on-disk sessions per-cwd
/// (`~/.grok/sessions/<cwd>/<uuid>/`), not per-construct-session, so two
/// construct sessions started in the same `cwd` share one folder there.
/// `find_session_id_excluding`'s newest-mtime discovery — meant to notice a
/// harness-native `/clear` creating a fresh dir — can't otherwise tell that
/// apart from a sibling's own routine `summary.json` rewrite (grok restamps
/// it, atomically, on every turn, which touches the containing dir's mtime).
/// Feeding every live sibling's current native id into the `excluded` set
/// closes that: a sibling merely taking a turn can no longer look like
/// *this* session's own conversation being reset and forked-and-archived.
fn sibling_native_ids(own_cwd: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Some(own_dir) = session_data_dir() else {
        return ids;
    };
    let Some(sessions_root) = own_dir.parent() else {
        return ids;
    };
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return ids;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own_dir {
            continue;
        }
        let Ok(meta_text) = std::fs::read_to_string(path.join("meta.json")) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta_text) else {
            continue;
        };
        if meta.get("harness").and_then(|h| h.as_str()) != Some("grok") {
            continue;
        }
        let same_cwd = meta
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(|c| Path::new(c) == own_cwd)
            .unwrap_or(false);
        if !same_cwd {
            continue;
        }
        if let Ok(native_id) = std::fs::read_to_string(path.join("grok_session_id.txt")) {
            let native_id = native_id.trim();
            if !native_id.is_empty() {
                ids.insert(native_id.to_string());
            }
        }
    }
    ids
}

/// Locate a known native session id anywhere under `sessions_root/*/`.
///
/// Grok keys directories by process cwd. Construct's recorded cwd can lag
/// behind when Grok chdirs into a project/git root after spawn (spec 0192),
/// so the preferred cwd-keyed path may never appear. A scan by id is the
/// only reliable way to attach the watcher to the real directory once it
/// exists. If multiple cwd encodings somehow contain the same id (should
/// not happen in practice), the newest mtime wins.
fn find_grok_session_dir_by_id_in(sessions_root: &Path, session_id: &str) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return None;
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let candidate = entry.path().join(session_id);
        if !candidate.is_dir() {
            continue;
        }
        let modified = candidate
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best
            .as_ref()
            .map(|(t, _)| modified > *t)
            .unwrap_or(true)
        {
            best = Some((modified, candidate));
        }
    }
    best.map(|(_, path)| path)
}

/// Prefer the cwd-keyed path under `sessions_root` when it exists; otherwise
/// scan by id (spec 0192). The `sessions_root` form is pure filesystem so
/// unit tests don't race on process-global `CONSTRUCT_GROK_HOME`.
fn resolve_grok_session_dir_in(
    sessions_root: &Path,
    preferred_cwd: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    let preferred = sessions_root
        .join(url_encode_path(preferred_cwd))
        .join(session_id);
    if preferred.is_dir() {
        return Some(preferred);
    }
    find_grok_session_dir_by_id_in(sessions_root, session_id)
}

/// Prefer the cwd-keyed path when it exists; otherwise scan by id (spec 0192).
fn resolve_grok_session_dir(preferred_cwd: &Path, session_id: &str) -> Option<PathBuf> {
    resolve_grok_session_dir_in(&grok_home()?.join("sessions"), preferred_cwd, session_id)
}

/// Grok's own idea of the session cwd, stamped on `summary.json` once the
/// directory is fully initialized. Used to re-anchor `/clear` discovery
/// after a process-cwd rebinding.
fn grok_session_recorded_cwd(session_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(session_dir.join("summary.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.pointer("/info/cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Context gauge from a session dir's `signals.json` (spec 0104): grok
/// maintains `contextTokensUsed` / `contextWindowTokens` there. Returns
/// `(used, window)` when the file parses and reports non-zero usage.
///
/// This is the *gauge* only. Per-call consumption comes from `updates.jsonl`
/// instead — see `grok_usage_events`.
fn grok_context_usage_in(session_dir: &Path) -> Option<(u64, Option<u64>)> {
    let path = session_dir.join("signals.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let used = v.get("contextTokensUsed").and_then(Value::as_u64)?;
    if used == 0 {
        return None;
    }
    let window = v
        .get("contextWindowTokens")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0);
    Some((used, window))
}

/// Context-breakdown segments (spec 0156) from the same session dir as the
/// gauge: grok writes the exact system prompt it sends to
/// `system_prompt.txt` and the full conversation to `chat_history.jsonl`,
/// so both components are derivable via the char heuristic. Ordered
/// fixed-prefix first (system prompt, then messages), both estimated.
fn grok_context_breakdown_in(session_dir: &Path) -> Vec<ContextSegment> {
    let mut segments = Vec::new();
    let prompt_chars = std::fs::read_to_string(session_dir.join("system_prompt.txt"))
        .map(|s| s.chars().count())
        .unwrap_or(0);
    if prompt_chars > 0 {
        segments.push(ContextSegment::new(
            "system prompt",
            estimate_tokens_from_chars(prompt_chars),
            true,
        ));
    }
    let message_chars = std::fs::read_to_string(session_dir.join("chat_history.jsonl"))
        .map(|text| grok_chat_history_chars(&text))
        .unwrap_or(0);
    if message_chars > 0 {
        segments.push(ContextSegment::new(
            "messages",
            estimate_tokens_from_chars(message_chars),
            true,
        ));
    }
    segments
}

/// Conversation chars across a full `chat_history.jsonl` scan — deliberately
/// a full re-scan each time so resume, `/clear` rebinds, and grok's own file
/// rewrites all self-correct without incremental bookkeeping.
fn grok_chat_history_chars(text: &str) -> usize {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|v| grok_record_chars(&v))
        .sum()
}

/// Conversation chars of one `chat_history.jsonl` record. Shapes verified
/// against real sessions on this machine (2026-07-28): `user` content is a
/// list of `{type:"text",text}` items in root transcripts (a JSON-encoded
/// string of the same list in child ones); `assistant` carries a
/// plain-string `content` plus `tool_calls` whose `arguments` is a JSON
/// string; `tool_result` carries a plain-string `content`. `system` records
/// duplicate `system_prompt.txt` verbatim (they have their own segment, so
/// counting them here would double-count); `reasoning` holds only encrypted
/// content plus display summaries and `backend_tool_call` is a server-side
/// action record — neither has countable conversation chars, so the
/// client's "unaccounted" row absorbs whatever they occupy.
fn grok_record_chars(v: &Value) -> usize {
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "user" => match v.get("content") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .map(|t| t.chars().count())
                .sum(),
            Some(Value::String(_)) => grok_content_text(v).map(|t| t.chars().count()).unwrap_or(0),
            _ => 0,
        },
        "assistant" => {
            let content = v
                .get("content")
                .and_then(Value::as_str)
                .map(|t| t.chars().count())
                .unwrap_or(0);
            let calls: usize = v
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| calls.iter().map(grok_tool_call_chars).sum())
                .unwrap_or(0);
            content + calls
        }
        "tool_result" => v
            .get("content")
            .and_then(Value::as_str)
            .map(|t| t.chars().count())
            .unwrap_or(0),
        _ => 0,
    }
}

fn grok_tool_call_chars(call: &Value) -> usize {
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .map(|n| n.chars().count())
        .unwrap_or(0);
    let args = match call.get("arguments") {
        Some(Value::String(s)) => s.chars().count(),
        Some(Value::Null) | None => 0,
        Some(other) => other.to_string().chars().count(),
    };
    name + args
}

fn count_jsonl_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn read_new_grok_jsonl_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = 0usize;
    let mut values = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        seen = idx + 1;
        if idx < *next_line {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => values.push(v),
            Err(e) => emit.log(format!(
                "grok: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
    values
}

fn emit_new_grok_transcript_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
) {
    for value in read_new_grok_jsonl_lines(path, next_line, emit) {
        emit_event_from_json(emit, value, last_model, last_effort);
    }
}

/// The root session's active model, if `v` is an assistant turn carrying a
/// `model_id` that differs from what we last saw — grok stamps `model_id` on
/// every assistant line in `chat_history.jsonl`, so this establishes the
/// initial model and, in principle, catches a mid-session `/model` switch.
///
/// Verified against a real session (2026-07-12): running `/model` and
/// picking a different one did not change `model_id` on the next turn, nor
/// `summary.json`'s `current_model_id` — and grok's own status bar kept
/// showing the old model too, so the switch never actually took effect at
/// the grok CLI level. That's an external grok issue, not something this
/// diff-against-`last_model` logic can work around: there's nothing on disk
/// to observe when grok itself never persists the change. Initial-model
/// capture is unaffected and works reliably.
/// Per-turn token usage from an `updates.jsonl` `turn_completed` record
/// (spec 0103), split per model (spec 0167).
///
/// grok closes every prompt with one such record carrying a real usage
/// object — `inputTokens` is the whole prompt side (`totalTokens` is exactly
/// input + output), `cachedReadTokens` the subset of it served from cache,
/// which is the split `Cost` wants. Its `modelUsage` map keys are spelled
/// exactly as `chat_history.jsonl`'s `model_id`, which is what
/// `grok_model_change` reports, so per-model attribution here cannot
/// disagree with this session's `ModelChanged`.
///
/// One record per prompt, and the caller's line cursor only ever hands each
/// line over once, so no dedupe is needed. Records belonging to a subagent
/// carry that agent's own `sessionId` and are skipped: the root's tally must
/// not absorb a child's.
///
/// `costUsdTicks` is deliberately ignored. The figure is plainly a scaled
/// integer, but nothing states the scale, and a dollar amount that is wrong
/// by a factor of ten is worse than none — so this reports volume only, as
/// every other wrapper adapter does.
fn grok_usage_events(v: &Value, root_id: &str, fallback_model: Option<&str>) -> Vec<SessionEvent> {
    let params = match v.get("params") {
        Some(params) => params,
        None => return Vec::new(),
    };
    if params.get("sessionId").and_then(Value::as_str) != Some(root_id) {
        return Vec::new();
    }
    let update = match params.get("update") {
        Some(update) => update,
        None => return Vec::new(),
    };
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("turn_completed") {
        return Vec::new();
    }
    let usage = match update.get("usage") {
        Some(usage) => usage,
        None => return Vec::new(),
    };

    let cost_from = |split: &Value, model: Option<String>| -> Option<SessionEvent> {
        let field = |key: &str| split.get(key).and_then(Value::as_u64).unwrap_or(0);
        let input = field("inputTokens");
        let output = field("outputTokens");
        let cached = field("cachedReadTokens");
        (input > 0 || output > 0).then_some(SessionEvent::Cost {
            usd: 0.0,
            tokens_in: input,
            tokens_out: output,
            tokens_cached: cached,
            model,
        })
    };

    // Prefer the per-model split; it is the same figure broken out, so using
    // both would double-count.
    if let Some(per_model) = usage.get("modelUsage").and_then(Value::as_object) {
        let events: Vec<SessionEvent> = per_model
            .iter()
            .filter_map(|(model, split)| cost_from(split, Some(model.clone())))
            .collect();
        if !events.is_empty() {
            return events;
        }
    }
    cost_from(usage, fallback_model.map(str::to_string))
        .into_iter()
        .collect()
}

fn grok_model_change(v: &Value, last_model: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let model = v.get("model_id").and_then(|m| m.as_str())?;
    (last_model.as_deref() != Some(model)).then(|| model.to_string())
}

/// Same signal as `grok_model_change`, for `reasoning_effort` (e.g.
/// `"high"`/`"medium"`/`"low"`) on the same assistant line. Carries the same
/// caveat: scanning 32 real sessions on this machine found 0 with more than
/// one distinct value, the identical frozen-per-session pattern already
/// confirmed for `model_id` — best-effort initial capture, unreliable for a
/// live mid-session change.
fn grok_effort_change(v: &Value, last_effort: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let effort = v.get("reasoning_effort").and_then(|e| e.as_str())?;
    (last_effort.as_deref() != Some(effort)).then(|| effort.to_string())
}

fn emit_event_from_json(
    emit: &EventEmitter,
    v: Value,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
) {
    if let Some(model) = grok_model_change(&v, last_model) {
        *last_model = Some(model.clone());
        emit.emit(SessionEvent::ModelChanged { model });
    }
    if let Some(effort) = grok_effort_change(&v, last_effort) {
        *last_effort = Some(effort.clone());
        emit.emit(SessionEvent::EffortChanged { effort });
    }
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "assistant" | "tool_result" => {
            for event in grok_events_from_json(&v) {
                emit.emit(event);
            }
        }
        "reasoning" => {
            if let Some(summary) = v.get("summary").and_then(|s| s.as_array()) {
                for item in summary {
                    if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                        emit.log(format!("grok reasoning: {}", t));
                    }
                }
            }
        }
        _ => {}
    }
}

fn grok_events_from_json(v: &Value) -> Vec<SessionEvent> {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "user" => grok_content_text(v)
            .filter(|content| {
                !content.is_empty() && !content.trim_start().starts_with("<system-reminder>")
            })
            .map(|text| {
                vec![SessionEvent::Message {
                    role: MessageRole::User,
                    text,
                }]
            })
            .unwrap_or_default(),
        "assistant" => {
            let mut out = Vec::new();
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    out.push(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: content.to_string(),
                    });
                }
            }
            if let Some(tool_calls) = v.get("tool_calls").and_then(|tc| tc.as_array()) {
                for call in tool_calls {
                    let name = call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = call
                        .get("arguments")
                        .and_then(|a| {
                            if let Some(s) = a.as_str() {
                                serde_json::from_str::<Value>(s).ok()
                            } else {
                                Some(a.clone())
                            }
                        })
                        .unwrap_or(Value::Null);
                    let call_id = call.get("id").and_then(|i| i.as_str()).map(String::from);
                    out.push(SessionEvent::ToolUse {
                        tool: name,
                        args,
                        call_id,
                    });
                }
            }
            out
        }
        "tool_result" => {
            let call_id = v
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .map(String::from);
            let output = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let is_failed = output.contains("User cancelled") || output.contains("failed");
            vec![SessionEvent::ToolResult {
                tool: "".to_string(),
                ok: !is_failed,
                output,
                call_id,
            }]
        }
        _ => Vec::new(),
    }
}

fn grok_content_text(v: &Value) -> Option<String> {
    let content = v.get("content")?.as_str()?;
    let Ok(items) = serde_json::from_str::<Value>(content) else {
        return Some(content.to_string());
    };
    let text = items
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrokNativeSubagentUpdate {
    Spawned {
        id: String,
        parent_id: Option<String>,
        title: Option<String>,
    },
    Finished {
        id: String,
        state: SessionState,
    },
}

fn grok_native_subagent_update(
    value: &Value,
    owner_native_id: &str,
) -> Option<GrokNativeSubagentUpdate> {
    if value.get("method").and_then(Value::as_str) != Some("_x.ai/session/update") {
        return None;
    }
    let update = value.pointer("/params/update")?;
    match update.get("sessionUpdate").and_then(Value::as_str)? {
        "subagent_spawned" => {
            let id = update
                .get("child_session_id")
                .or_else(|| update.get("subagent_id"))
                .and_then(Value::as_str)?
                .to_string();
            let parent_id = update
                .get("parent_session_id")
                .and_then(Value::as_str)
                .filter(|parent| *parent != owner_native_id)
                .map(str::to_string);
            let title = update
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.trim().is_empty())
                .map(str::to_string);
            Some(GrokNativeSubagentUpdate::Spawned {
                id,
                parent_id,
                title,
            })
        }
        "subagent_finished" => {
            let id = update
                .get("child_session_id")
                .or_else(|| update.get("subagent_id"))
                .and_then(Value::as_str)?
                .to_string();
            let state = match update.get("status").and_then(Value::as_str).unwrap_or("") {
                "failed" | "error" | "errored" | "cancelled" => SessionState::Errored,
                "completed" | "done" | "success" => SessionState::Done,
                _ => SessionState::Running,
            };
            Some(GrokNativeSubagentUpdate::Finished { id, state })
        }
        _ => None,
    }
}

#[derive(Debug)]
struct GrokNativeChild {
    parent_id: Option<String>,
    state: SessionState,
    next_transcript_line: usize,
}

fn apply_grok_native_update(
    update: GrokNativeSubagentUpdate,
    children: &mut HashMap<String, GrokNativeChild>,
    emit: Option<&EventEmitter>,
) {
    match update {
        GrokNativeSubagentUpdate::Spawned {
            id,
            parent_id,
            title,
        } => {
            let next_transcript_line = children
                .get(&id)
                .map(|child| child.next_transcript_line)
                .unwrap_or(0);
            children.insert(
                id.clone(),
                GrokNativeChild {
                    parent_id: parent_id.clone(),
                    state: SessionState::Running,
                    next_transcript_line,
                },
            );
            if let Some(emit) = emit {
                emit.emit(SessionEvent::NativeSubagent {
                    id,
                    parent_id,
                    title,
                    state: SessionState::Running,
                    event: None,
                    seq: None,
                });
            }
        }
        GrokNativeSubagentUpdate::Finished { id, state } => {
            let child = children
                .entry(id.clone())
                .or_insert_with(|| GrokNativeChild {
                    parent_id: None,
                    state,
                    next_transcript_line: 0,
                });
            child.state = state;
            if let Some(emit) = emit {
                emit.emit(SessionEvent::NativeSubagent {
                    id,
                    parent_id: child.parent_id.clone(),
                    title: None,
                    state,
                    event: None,
                    seq: None,
                });
            }
        }
    }
}

fn grok_allow_args() -> Vec<String> {
    grok_allow_args_for(&construct_protocol::adapter::policy::AutoApprovePolicy::from_env())
}

fn grok_allow_args_for(
    policy: &construct_protocol::adapter::policy::AutoApprovePolicy,
) -> Vec<String> {
    let mut out = Vec::new();
    for root in policy.allow_paths() {
        let glob = format!("{}/**", root.display());
        // Grok's CLI only knows `Write` and `Edit`; `MultiEdit` is a Claude
        // tool prefix and is rejected as an unknown prefix at spawn time.
        for tool in ["Write", "Edit"] {
            out.push("--allow".into());
            out.push(format!("{tool}({glob})"));
        }
    }
    out
}

/// Apply skip-existing cursors and subagent seed state once the real on-disk
/// directory for `root_id` is attached. Returns the line cursors to use.
fn attach_existing_native_session(
    root_id: &str,
    session_dir: &Path,
    emit: &EventEmitter,
    children: &mut HashMap<String, GrokNativeChild>,
) -> (usize, usize) {
    let transcript = session_dir.join("chat_history.jsonl");
    let updates = session_dir.join("updates.jsonl");
    let next_line = count_jsonl_lines(&transcript);
    let next_update_line = count_jsonl_lines(&updates);
    let mut replay_line = 0;
    for value in read_new_grok_jsonl_lines(&updates, &mut replay_line, emit) {
        if let Some(update) = grok_native_subagent_update(&value, root_id) {
            apply_grok_native_update(update, children, None);
        }
    }
    (next_line, next_update_line)
}

/// Rebind transcript/updates paths onto the directory Grok actually wrote
/// for `session_id`, following a process-cwd rebinding (spec 0192).
///
/// Returns `true` when this call first attaches to a real directory (caller
/// may then apply skip-existing). Updates `watch_cwd` and the discovery
/// exclusion set when Grok's recorded cwd differs from construct's.
fn rebind_watcher_to_resolved_session(
    session_id: &str,
    construct_cwd: &Path,
    watch_cwd: &mut PathBuf,
    path: &mut Option<PathBuf>,
    updates_path: &mut Option<PathBuf>,
    session_dir_out: &mut Option<PathBuf>,
    never_rebind_onto: &mut HashSet<String>,
    emit: &EventEmitter,
) -> bool {
    let Some(dir) = resolve_grok_session_dir(watch_cwd, session_id)
        .or_else(|| resolve_grok_session_dir(construct_cwd, session_id))
    else {
        return false;
    };
    let new_path = dir.join("chat_history.jsonl");
    let new_updates = dir.join("updates.jsonl");
    if path.as_ref() == Some(&new_path)
        && updates_path.as_ref() == Some(&new_updates)
        && session_dir_out.as_ref() == Some(&dir)
    {
        return false;
    }
    let first_attach = path.is_none()
        || path
            .as_ref()
            .map(|p| !p.exists())
            .unwrap_or(true);
    if let Some(recorded) = grok_session_recorded_cwd(&dir) {
        if &recorded != watch_cwd {
            emit.log(format!(
                "grok: native session cwd rebased {} -> {}; \
                 rebinding transcript watcher paths",
                watch_cwd.display(),
                recorded.display()
            ));
            // Dirs that already lived under the rebased cwd cannot be our
            // future /clear — snapshot them into the permanent exclusion set.
            never_rebind_onto.extend(existing_session_ids(&recorded));
            never_rebind_onto.remove(session_id);
            *watch_cwd = recorded;
        }
    }
    *path = Some(new_path);
    *updates_path = Some(new_updates);
    *session_dir_out = Some(dir);
    first_attach
}

fn spawn_interactive_transcript_watcher(
    initial_id: Option<String>,
    cwd: PathBuf,
    emit: EventEmitter,
    skip_existing: bool,
    never_rebind_onto: HashSet<String>,
    initial_model: Option<String>,
) {
    if grok_home().is_none() {
        emit.log("grok: no GROK_HOME or HOME — cannot watch native transcript");
        return;
    }
    tokio::spawn(async move {
        // `cwd` is construct's recorded session cwd (sibling filter key).
        // `watch_cwd` tracks where Grok actually stores files and may rebind
        // after a process chdir (spec 0192).
        let construct_cwd = cwd;
        let mut watch_cwd = construct_cwd.clone();
        let mut never_rebind_onto = never_rebind_onto;
        let mut current_id = initial_id;
        let mut path: Option<PathBuf> = None;
        let mut updates_path: Option<PathBuf> = None;
        let mut session_dir: Option<PathBuf> = None;
        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
        // Resume attaches skip prior history only after the real directory
        // is found — a delayed cwd rebase must not re-project native history.
        let mut pending_skip_existing = skip_existing;
        let mut next_line = 0usize;
        let mut next_update_line = 0usize;
        let mut children = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        if let Some(id) = current_id.as_deref() {
            if rebind_watcher_to_resolved_session(
                id,
                &construct_cwd,
                &mut watch_cwd,
                &mut path,
                &mut updates_path,
                &mut session_dir,
                &mut never_rebind_onto,
                &emit,
            ) && pending_skip_existing
            {
                if let Some(dir) = session_dir.as_ref() {
                    let (nl, nul) =
                        attach_existing_native_session(id, dir, &emit, &mut children);
                    next_line = nl;
                    next_update_line = nul;
                }
                pending_skip_existing = false;
            } else if path.is_some() {
                // Attached without skip (fresh/fork): leave cursors at 0.
                pending_skip_existing = false;
            }
        }
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Re-scanning every sibling's meta.json on every 500ms tick is
        // needless I/O churn — sibling composition and cwd are static for a
        // session's lifetime, only their native ids rotate occasionally.
        // Refreshing every 5s (10 ticks) still closes the false-rebind
        // window far faster than that window can recur in practice.
        // Sibling filter always uses construct's recorded cwd (spec 0088 /
        // 0192): it matches construct metadata, not Grok's process cwd.
        let mut ticks_since_sibling_refresh: u32 = 0;
        let mut sibling_ids: HashSet<String> = sibling_native_ids(&construct_cwd);
        // Last context gauge reported (spec 0104) — poll `signals.json`
        // each tick but only emit when the numbers actually move.
        let mut last_context: Option<(u64, Option<u64>)> = None;
        // Breakdown (spec 0156): recomputed from the session dir on the
        // same poll as the gauge, reported only when it changes.
        let mut breakdown_gate = BreakdownGate::default();
        let mut overhead_pin = FixedOverheadPin::default();
        loop {
            tick.tick().await;
            ticks_since_sibling_refresh += 1;
            if ticks_since_sibling_refresh >= 10 {
                ticks_since_sibling_refresh = 0;
                sibling_ids = sibling_native_ids(&construct_cwd);
            }
            // Known id: keep paths pointed at the real on-disk directory,
            // which may appear under a different cwd encoding after Grok
            // chdirs (spec 0192).
            if let Some(id) = current_id.as_deref() {
                if rebind_watcher_to_resolved_session(
                    id,
                    &construct_cwd,
                    &mut watch_cwd,
                    &mut path,
                    &mut updates_path,
                    &mut session_dir,
                    &mut never_rebind_onto,
                    &emit,
                ) && pending_skip_existing
                {
                    if let Some(dir) = session_dir.as_ref() {
                        let (nl, nul) =
                            attach_existing_native_session(id, dir, &emit, &mut children);
                        next_line = nl;
                        next_update_line = nul;
                    }
                    pending_skip_existing = false;
                } else if path
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false)
                {
                    pending_skip_existing = false;
                }
            }
            if let Some(path) = path.as_deref().filter(|path| path.exists()) {
                emit_new_grok_transcript_lines(
                    path,
                    &mut next_line,
                    &emit,
                    &mut last_model,
                    &mut last_effort,
                );
            }
            if let (Some(root_id), Some(updates_path)) =
                (current_id.as_deref(), updates_path.as_deref())
            {
                for value in read_new_grok_jsonl_lines(updates_path, &mut next_update_line, &emit) {
                    for event in grok_usage_events(&value, root_id, last_model.as_deref()) {
                        emit.emit(event);
                    }
                    if let Some(update) = grok_native_subagent_update(&value, root_id) {
                        apply_grok_native_update(update, &mut children, Some(&emit));
                    }
                }
            }
            if let Some(dir) = session_dir.as_ref() {
                let observed = grok_context_usage_in(dir);
                if let Some((used, window)) = observed {
                    if last_context != Some((used, window)) {
                        last_context = Some((used, window));
                        emit.emit(SessionEvent::ContextUsage {
                            used_tokens: used,
                            window_tokens: window,
                        });
                    }
                }
                let mut segments = grok_context_breakdown_in(dir);
                // Differential fixed-overhead pin (spec 0156): grok's gauge
                // is a live snapshot with no on-disk history, so the pin is
                // held across polls and lands on the first poll where both
                // the gauge and a conversation estimate exist — requiring
                // the `messages` segment keeps a not-yet-flushed
                // chat_history.jsonl from inflating the residual. What it
                // measures is the tool schemas (the system prompt has its
                // own file, and so its own segment).
                if let Some((used, _)) = observed {
                    if segments.iter().any(|s| s.label == "messages") {
                        let estimated = segments.iter().map(|s| s.tokens).sum();
                        overhead_pin.observe(used, estimated);
                    }
                }
                if let Some(seg) = overhead_pin.segment() {
                    let at = segments
                        .iter()
                        .position(|s| s.label == "messages")
                        .unwrap_or(segments.len());
                    segments.insert(at, seg);
                }
                if !segments.is_empty() && breakdown_gate.changed(&segments) {
                    emit.emit(SessionEvent::ContextBreakdown { segments });
                }
            }
            for (id, child) in &mut children {
                // Children live under the same cwd encoding as the root
                // after any rebase; also accept a scan-by-id hit.
                let child_path = resolve_grok_session_dir(&watch_cwd, id)
                    .or_else(|| resolve_grok_session_dir(&construct_cwd, id))
                    .map(|d| d.join("chat_history.jsonl"));
                let Some(child_path) = child_path else {
                    continue;
                };
                for value in
                    read_new_grok_jsonl_lines(&child_path, &mut child.next_transcript_line, &emit)
                {
                    for event in grok_events_from_json(&value) {
                        // File-derived: ordinal-tagged so the daemon drops
                        // replays of already-projected history.
                        let ord = child_seq.entry(id.clone()).or_insert(0);
                        emit.emit(SessionEvent::NativeSubagent {
                            id: id.clone(),
                            parent_id: child.parent_id.clone(),
                            title: None,
                            state: child.state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                }
            }

            // Prefer the newest non-child, non-sibling session dir under the
            // watch cwd (Grok's effective storage cwd). First spawn discovers
            // the id; after /clear a fresher root dir appears and we rebind.
            let mut excluded: HashSet<String> = children.keys().cloned().collect();
            excluded.extend(never_rebind_onto.iter().cloned());
            excluded.extend(sibling_ids.iter().cloned());
            if let Some(id) = find_session_id_excluding(&watch_cwd, &excluded) {
                if current_id.as_ref() != Some(&id) {
                    if let Some(dir) = resolve_grok_session_dir(&watch_cwd, &id) {
                        if let Some(prior) = current_id.as_ref() {
                            emit.log(format!(
                                "grok: native session id changed {:?} -> {id}; \
                                 rebinding transcript watcher",
                                current_id
                            ));
                            emit.emit(SessionEvent::NativeIdChanged {
                                prior_native_id: prior.clone(),
                                new_native_id: id.clone(),
                            });
                        }
                        write_conv_id(&id);
                        current_id = Some(id);
                        path = Some(dir.join("chat_history.jsonl"));
                        updates_path = Some(dir.join("updates.jsonl"));
                        session_dir = Some(dir);
                        next_line = 0;
                        next_update_line = 0;
                        pending_skip_existing = false;
                        last_context = None;
                        breakdown_gate = BreakdownGate::default();
                        // New native session, new context epoch: re-measure.
                        overhead_pin.reset();
                    }
                }
            }
        }
    });
}

/// How the interactive Grok process is bound to a native session id.
///
/// Grok has no originator tag (unlike Codex), so without an explicit
/// `--session-id` / `-r` the adapter used to discover "whichever session dir
/// under this cwd is newest." That heuristic is fine for mid-session
/// `/clear` detection once we already know our id, but it is fatal on first
/// spawn in a shared cwd: an older orphan conversation (e.g. a short
/// `echo "yes"` test from weeks ago) can win the mtime race, get written to
/// `grok_session_id.txt`, and then poison every later same-harness fork —
/// the client skips the portable transcript seed for natively-forking
/// harnesses (spec 0031), so the fork inherits only that orphan's history.
///
/// Pre-minting a UUID with `--session-id` on first spawn (matching Claude)
/// makes the binding authoritative from process start. Discovery then only
/// rebinds onto directories that appear *after* spawn (`/clear`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveIdentity {
    /// Args that select/create the native session (`-r`, `--fork-session`,
    /// `--session-id`).
    args: Vec<String>,
    /// Native id this construct session owns (pre-minted, resumed, or forked).
    session_id: Option<String>,
    /// Parent native id when this launch is a same-harness fork.
    fork_parent_id: Option<String>,
    /// Skip already-projected transcript lines. Only true on daemon resume —
    /// a native fork must project the inherited parent history into the new
    /// construct transcript, and a fresh pre-mint has nothing to skip.
    skip_existing: bool,
}

fn plan_interactive_identity(
    resuming: bool,
    persisted_id: Option<String>,
    fork_from: Option<String>,
    mint_id: impl FnOnce() -> String,
) -> InteractiveIdentity {
    if resuming {
        let mut args = Vec::new();
        if let Some(sid) = persisted_id.as_ref() {
            args.push("-r".into());
            args.push(sid.clone());
        }
        return InteractiveIdentity {
            args,
            session_id: persisted_id,
            fork_parent_id: None,
            skip_existing: true,
        };
    }

    if let Some(parent) = fork_from.filter(|s| !s.is_empty()) {
        // Same-harness fork: resume the parent's native session AS A NEW one
        // (`--fork-session`), named up front (`--session-id`) so this
        // session's own id file is correct immediately (the daemon read the
        // parent's id — spec 0031/0078).
        let new_id = mint_id();
        return InteractiveIdentity {
            args: vec![
                "-r".into(),
                parent.clone(),
                "--fork-session".into(),
                "--session-id".into(),
                new_id.clone(),
            ],
            session_id: Some(new_id),
            fork_parent_id: Some(parent),
            // Forked history lives in the new native session file and must
            // be projected — this is a brand-new construct session.
            skip_existing: false,
        };
    }

    // Fresh interactive spawn: pre-mint so we never bind to an unrelated
    // pre-existing session dir under this cwd via newest-mtime discovery.
    let new_id = mint_id();
    InteractiveIdentity {
        args: vec!["--session-id".into(), new_id.clone()],
        session_id: Some(new_id),
        fork_parent_id: None,
        skip_existing: false,
    }
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = command_override();
    let mut args = command.args.clone();
    args.extend(params.args.clone());

    if let Some(m) = params.model.as_ref() {
        args.push("--model".into());
        args.push(m.clone());
    }

    args.extend(grok_allow_args());

    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    // Fork hint is first-launch only. The daemon persists start.json env
    // across resume; re-honoring it would fork the parent again instead of
    // resuming this session's own captured native id (same as Claude).
    let fork_from = (!resuming)
        .then(|| {
            std::env::var("CONSTRUCT_GROK_FORK_FROM")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .flatten();
    let identity = plan_interactive_identity(
        resuming,
        if resuming { read_conv_id() } else { None },
        fork_from,
        || uuid::Uuid::new_v4().to_string(),
    );
    args.extend(identity.args.iter().cloned());
    if let Some(id) = identity.session_id.as_ref() {
        // Persist before spawn so a crash mid-start still leaves a usable id,
        // and so sibling exclusion sees us immediately.
        write_conv_id(id);
    }

    if !resuming {
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

    let label = command.argv_preview();
    let bin = command.bin;
    let spec = PtySpec {
        bin,
        args,
        cwd: std::path::PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
        // Full-screen TUI: holds the foreground group; use daemon quiescence.
        detect_prompt_via_pgroup: false,
    };

    let cwd = PathBuf::from(&params.cwd);
    // Continuous discovery covers mid-session /clear (a fresh dir with a
    // newer mtime). Pre-mint / resume / fork all give us a known id up front,
    // so we always snapshot pre-existing dirs into the exclusion set: none of
    // them can be our future `/clear`, and none of them may steal our binding.
    let mut never_rebind_onto: HashSet<String> =
        identity.fork_parent_id.into_iter().collect();
    never_rebind_onto.extend(existing_session_ids(&cwd));
    if let Some(own_id) = identity.session_id.as_ref() {
        never_rebind_onto.remove(own_id);
    }
    spawn_interactive_transcript_watcher(
        identity.session_id,
        cwd,
        ctx.emit.clone(),
        identity.skip_existing,
        never_rebind_onto,
        params.model.clone(),
    );

    let _ = run_pty(spec, ctx).await;
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = command_override();
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut session_id = read_conv_id();
    let mut pending = VecDeque::new();
    if let Some(p) = params.prompt.clone() {
        if !p.trim().is_empty() {
            pending.push_back(p);
        }
    }

    let exit_code = loop {
        let user_text = match pending.pop_front() {
            Some(t) => t,
            None => {
                emit.emit(SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                });
                match inbox.recv().await {
                    None => break 0,
                    Some(AdapterInboxMsg::Input(t)) => t,
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::Stop) => break 0,
                    _ => continue,
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

        let mut child_args = command_override.args.clone();
        child_args.push("-p".into());
        child_args.push(user_text.clone());
        child_args.push("--output-format".into());
        child_args.push("streaming-json".into());

        child_args.extend(grok_allow_args());

        if let Some(sid) = &session_id {
            child_args.push("-r".into());
            child_args.push(sid.clone());
        }
        if let Some(m) = &model {
            child_args.push("--model".into());
            child_args.push(m.clone());
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }

        let mut command = Command::new(&command_override.bin);
        for a in &child_args {
            command.arg(a);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        for (k, v) in &env {
            command.env(k, v);
        }
        command.env("CONSTRUCT_SESSION_ID", &agentd_session_id);

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: construct_protocol::adapter::missing_bin_hint(
                        &command_override.argv_preview(),
                        &e,
                    ),
                });
                break 127;
            }
        };

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let parser_task = spawn_parser(child_stdout, emit.clone(), captured_sid.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = parser_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        // Always adopt the latest native id so a mid-run reset is honored
        // on subsequent turns (and written for daemon resume).
        if let Some(sid) = captured_sid.lock().unwrap().clone() {
            if session_id.as_ref() != Some(&sid) {
                write_conv_id(&sid);
                session_id = Some(sid);
            }
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

fn spawn_parser<R>(
    reader: R,
    emit: EventEmitter,
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
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                emit.emit(SessionEvent::Message {
                                    role: MessageRole::Assistant,
                                    text: data.to_string(),
                                });
                            }
                        }
                        "end" => {
                            if let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) {
                                let mut g = captured_sid.lock().unwrap();
                                // Keep the most recently observed id (not only the first).
                                *g = Some(sid.to_string());
                            }
                        }
                        "thought" => {
                            if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                emit.log(format!("thought: {}", data));
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: line,
                    });
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn fresh_interactive_launch_premints_session_id() {
        // First spawn must pin a UUID with `--session-id` rather than
        // discovering "newest under cwd" — that discovery is what bound
        // live construct sessions onto orphan `echo "yes"` conversations
        // and made same-harness forks inherit only that orphan history.
        let planned = plan_interactive_identity(false, None, None, || {
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()
        });
        assert_eq!(
            planned.args,
            vec![
                "--session-id".to_string(),
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            ]
        );
        assert_eq!(
            planned.session_id.as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert!(planned.fork_parent_id.is_none());
        assert!(
            !planned.skip_existing,
            "fresh sessions have no prior construct transcript to skip"
        );
    }

    #[test]
    fn native_fork_launch_keeps_parent_and_projects_history() {
        let planned = plan_interactive_identity(
            false,
            None,
            Some("parent-native-id".into()),
            || "fork-native-id".into(),
        );
        assert_eq!(
            planned.args,
            vec![
                "-r".to_string(),
                "parent-native-id".into(),
                "--fork-session".into(),
                "--session-id".into(),
                "fork-native-id".into(),
            ]
        );
        assert_eq!(planned.session_id.as_deref(), Some("fork-native-id"));
        assert_eq!(planned.fork_parent_id.as_deref(), Some("parent-native-id"));
        assert!(
            !planned.skip_existing,
            "a native fork is a new construct session: inherited parent \
             history must be projected into its transcript (skip_existing \
             was previously true because the pre-minted id made the adapter \
             look 'attached', which dropped forked history from chat view)"
        );
    }

    #[test]
    fn resume_launch_skips_existing_and_ignores_fork_hint() {
        // plan_interactive_identity itself does not see the fork env; the
        // caller clears fork_from when resuming. Assert the resume shape.
        let planned = plan_interactive_identity(
            true,
            Some("persisted-native-id".into()),
            None, // caller must pass None on resume
            || panic!("resume must not mint a new id"),
        );
        assert_eq!(
            planned.args,
            vec!["-r".to_string(), "persisted-native-id".into()]
        );
        assert_eq!(planned.session_id.as_deref(), Some("persisted-native-id"));
        assert!(planned.skip_existing);
        assert!(planned.fork_parent_id.is_none());
    }

    #[test]
    fn newest_dir_discovery_skips_fork_sessions() {
        // A fork (`--fork-session`) creates a NEW session dir in the same
        // cwd, stamped `session_kind: "fork"` in its summary. The
        // newest-dir discovery another session's watcher runs (typically
        // the fork's parent) must never rebind onto it — forks are named
        // up front and bound directly by their own watcher.
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-fork-skip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = std::path::Path::new("/tmp/proj");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        let parent = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        let fork = "019e32bb-014a-7ff0-9a3f-7ae773961a99";
        std::fs::create_dir_all(sessions.join(parent)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(fork)).unwrap();
        std::fs::write(
            sessions.join(fork).join("summary.json"),
            format!(
                "{{\"info\":{{\"id\":\"{fork}\"}},\
                 \"session_kind\":\"fork\",\
                 \"parent_session_id\":\"{parent}\"}}"
            ),
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        let found = find_session_id_excluding(cwd, &HashSet::new());
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        assert_eq!(
            found.as_deref(),
            Some(parent),
            "the newer fork dir must be invisible to discovery"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The other direction of the same race: a FORK's own watcher, in the
    /// window before grok has created the fork's own session dir, must
    /// never rebind onto its PARENT's dir just because the parent's is
    /// the only (and therefore "newest") one present. Passing the parent's
    /// id in the exclusion set — as `run_interactive` now does whenever
    /// `CONSTRUCT_GROK_FORK_FROM` is set — closes this regardless of
    /// timing, rather than relying on the fork's own dir winning a race.
    #[test]
    fn newest_dir_discovery_excludes_the_forks_own_parent() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-fork-parent-exclude-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = std::path::Path::new("/tmp/proj");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        let parent = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        // Only the parent's dir exists — simulating the window right after
        // the fork process is spawned but before grok has created its own
        // session directory.
        std::fs::create_dir_all(sessions.join(parent)).unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        let mut excluded = HashSet::new();
        excluded.insert(parent.to_string());
        let found = find_session_id_excluding(cwd, &excluded);
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        assert_eq!(
            found, None,
            "with the parent excluded, discovery must find nothing rather \
             than silently rebinding the fork's persisted id onto its \
             parent's conversation"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    use super::*;

    #[test]
    fn parses_assistant_message() {
        let v: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"Creating file","tool_calls":[]}"#,
        )
        .unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::Message { role, text } => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "Creating file");
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn parses_assistant_tool_calls() {
        let v: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-1","name":"Write","arguments":"{\"path\":\"hello.txt\"}"}]}"#
        ).unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            } => {
                assert_eq!(tool, "Write");
                assert_eq!(args["path"], "hello.txt");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool use event"),
        }
    }

    #[test]
    fn parses_tool_result() {
        let v: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"file written"}"#,
        )
        .unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            } => {
                assert_eq!(tool, "");
                assert!(*ok);
                assert_eq!(output, "file written");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool result event"),
        }
    }

    #[test]
    fn parses_cancelled_tool_result() {
        let v: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"User cancelled the execution"}"#
        ).unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            } => {
                assert_eq!(tool, "");
                assert!(!*ok);
                assert_eq!(output, "User cancelled the execution");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool result event"),
        }
    }

    #[test]
    fn parses_native_subagent_spawn_and_finish_updates() {
        let owner = "019f4ae5-dc29-7142-9c7c-34dac1017cbc";
        let child = "019f4ae5-f3f4-7550-8182-39671f0959af";
        let spawned = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_spawned",
                "subagent_id": child,
                "parent_session_id": owner,
                "child_session_id": child,
                "description": "Print hello world"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&spawned, owner),
            Some(GrokNativeSubagentUpdate::Spawned {
                id: child.into(),
                parent_id: None,
                title: Some("Print hello world".into()),
            })
        );

        let finished = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_finished",
                "child_session_id": child,
                "status": "completed",
                "output": "hello world"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&finished, owner),
            Some(GrokNativeSubagentUpdate::Finished {
                id: child.into(),
                state: SessionState::Done,
            })
        );
    }

    #[test]
    fn native_subagent_spawn_preserves_nested_parent() {
        let spawned = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_spawned",
                "child_session_id": "grandchild",
                "parent_session_id": "child"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&spawned, "owner"),
            Some(GrokNativeSubagentUpdate::Spawned {
                id: "grandchild".into(),
                parent_id: Some("child".into()),
                title: None,
            })
        );
    }

    #[test]
    fn parses_json_encoded_user_content_for_child_transcript() {
        let value = serde_json::json!({
            "type": "user",
            "content": r#"[{"type":"text","text":"Print hello world"}]"#
        });
        match grok_events_from_json(&value).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::User));
                assert_eq!(text, "Print hello world");
            }
            other => panic!("unexpected child user events: {other:?}"),
        }
    }

    #[test]
    fn omits_internal_reminders_from_child_transcript() {
        let value = serde_json::json!({
            "type": "user",
            "content": r#"[{"type":"text","text":"\n<system-reminder>internal context</system-reminder>"}]"#
        });
        assert!(grok_events_from_json(&value).is_empty());
    }

    #[test]
    fn url_encodes_paths_correctly() {
        let path = Path::new("/Users/moon/agentd");
        assert_eq!(url_encode_path(path), "%2FUsers%2Fmoon%2Fagentd");
    }

    /// Record shapes verified against real `chat_history.jsonl` files on
    /// this machine (2026-07-28): root `user` content is a real JSON list,
    /// `assistant` a plain string plus `tool_calls` with JSON-string
    /// `arguments`, `tool_result` a plain string; `system` duplicates
    /// `system_prompt.txt` verbatim and `reasoning` carries encrypted
    /// content only — both must not count toward the messages estimate.
    fn breakdown_fixture() -> String {
        concat!(
            r#"{"type":"system","content":"You are Grok."}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"fix the bug"}]}"#,
            "\n",
            r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"display only"}],"encrypted_content":"AAAA","status":"completed"}"#,
            "\n",
            r#"{"type":"assistant","content":"Looking.","tool_calls":[{"id":"call-1","name":"Write","arguments":"{\"path\":\"hello.txt\"}"}],"model_id":"grok-4.5","reasoning_effort":"high"}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"file written"}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search"}}"#,
            "\n",
        )
        .to_string()
    }

    #[test]
    fn chat_history_chars_count_conversation_content_only() {
        // user 11 ("fix the bug") + assistant 8 ("Looking.") + tool call
        // 5 ("Write") + 20 (arguments JSON string) + tool result 12
        // ("file written") = 56; system/reasoning/backend_tool_call add 0.
        assert_eq!(grok_chat_history_chars(&breakdown_fixture()), 56);

        // Child transcripts JSON-encode the user item list into a string
        // (same shape `grok_events_from_json` already parses).
        let child = serde_json::json!({
            "type": "user",
            "content": r#"[{"type":"text","text":"Print hello world"}]"#
        });
        assert_eq!(
            grok_record_chars(&child),
            "Print hello world".chars().count()
        );
    }

    #[test]
    fn breakdown_orders_system_prompt_before_messages_and_gates_repeats() {
        let dir = std::env::temp_dir().join(format!(
            "agentd-grok-breakdown-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 35 chars -> ~10 tokens at the shared chars/3.5 heuristic.
        std::fs::write(
            dir.join("system_prompt.txt"),
            "You are Grok, a CLI coding agent!!!",
        )
        .unwrap();
        std::fs::write(dir.join("chat_history.jsonl"), breakdown_fixture()).unwrap();

        let segments = grok_context_breakdown_in(&dir);
        assert_eq!(
            segments,
            vec![
                ContextSegment::new("system prompt", 10, true),
                ContextSegment::new("messages", 16, true), // 56 chars / 3.5
            ]
        );

        // Report-on-change: an unchanged recompute must be suppressed.
        let mut gate = BreakdownGate::default();
        assert!(gate.changed(&segments));
        assert!(!gate.changed(&grok_context_breakdown_in(&dir)));

        // No system_prompt.txt -> the segment is skipped, not zeroed.
        std::fs::remove_file(dir.join("system_prompt.txt")).unwrap();
        let segments = grok_context_breakdown_in(&dir);
        assert_eq!(segments, vec![ContextSegment::new("messages", 16, true)]);
        assert!(gate.changed(&segments));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_session_dir_prefers_cwd_keyed_path() {
        let root = std::env::temp_dir().join(format!(
            "agentd-grok-resolve-prefer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let spawn_cwd = Path::new("/Users/moon");
        let other_cwd = Path::new("/Users/moon/construct");
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let preferred = root.join(url_encode_path(spawn_cwd)).join(id);
        let other = root.join(url_encode_path(other_cwd)).join(id);
        std::fs::create_dir_all(&preferred).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        // Pure filesystem helper — no process-global GROK_HOME (CI runs
        // these tests in parallel with other env-mutating cases).
        let got = resolve_grok_session_dir_in(&root, spawn_cwd, id);
        assert_eq!(got.as_deref(), Some(preferred.as_path()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// When Grok chdirs after spawn, the cwd-keyed path under construct's
    /// recorded cwd never appears — the scan-by-id fallback must find the
    /// real directory under the rebased cwd encoding (spec 0192).
    #[test]
    fn resolve_session_dir_scans_by_id_when_cwd_path_missing() {
        let root = std::env::temp_dir().join(format!(
            "agentd-grok-resolve-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let spawn_cwd = Path::new("/Users/moon");
        let rebased_cwd = Path::new("/Users/moon/construct");
        let id = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
        let real = root.join(url_encode_path(rebased_cwd)).join(id);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("summary.json"),
            r#"{"info":{"id":"bbbbbbbb-cccc-dddd-eeee-ffffffffffff","cwd":"/Users/moon/construct"}}"#,
        )
        .unwrap();

        // Preferred path under spawn cwd does not exist under `root`.
        assert!(!root.join(url_encode_path(spawn_cwd)).join(id).is_dir());
        let got = resolve_grok_session_dir_in(&root, spawn_cwd, id).expect("scan by id");
        assert_eq!(got, real);
        assert_eq!(
            grok_session_recorded_cwd(&got).as_deref(),
            Some(Path::new("/Users/moon/construct"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_session_id_prefers_newest_mtime() {
        // Simulate /clear: two UUID session dirs under the same project
        // path; the newer mtime must win so resume tracks the active id.
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = Path::new("/tmp/agentd-grok-clear-test");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        std::fs::create_dir_all(&sessions).unwrap();

        let old_id = "aaaaaaaa-bbbb-cccc-dddd-000000000001";
        let new_id = "aaaaaaaa-bbbb-cccc-dddd-000000000002";
        std::fs::create_dir_all(sessions.join(old_id)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(new_id)).unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        assert_eq!(find_session_id(cwd).as_deref(), Some(new_id));
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resumed_session_excludes_every_preexisting_native_sibling() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-resume-baseline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = Path::new("/tmp/agentd-grok-resume-baseline-test");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        std::fs::create_dir_all(&sessions).unwrap();

        let own_id = "aaaaaaaa-bbbb-cccc-dddd-000000000001";
        let sibling_id = "aaaaaaaa-bbbb-cccc-dddd-000000000002";
        let cleared_id = "aaaaaaaa-bbbb-cccc-dddd-000000000003";
        std::fs::create_dir_all(sessions.join(own_id)).unwrap();
        std::fs::create_dir_all(sessions.join(sibling_id)).unwrap();

        let mut baseline = existing_session_ids_in(&sessions);
        baseline.remove(own_id);
        assert_eq!(
            find_session_id_excluding_in(&sessions, &baseline).as_deref(),
            Some(own_id)
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(cleared_id)).unwrap();
        assert_eq!(
            find_session_id_excluding_in(&sessions, &baseline).as_deref(),
            Some(cleared_id)
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sibling_native_ids_reads_only_grok_siblings_in_the_same_cwd() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-siblings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions_root = home.join("sessions");
        let own_dir = sessions_root.join("sOWN");
        std::fs::create_dir_all(&own_dir).unwrap();

        let cwd = Path::new("/tmp/agentd-shared-cwd");

        // A real sibling: same cwd, same harness — must be picked up.
        let sibling_dir = sessions_root.join("sSIBLING");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::write(
            sibling_dir.join("meta.json"),
            format!(r#"{{"harness":"grok","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();
        std::fs::write(sibling_dir.join("grok_session_id.txt"), "sibling-native-id").unwrap();

        // A different-cwd grok session — must be excluded from the result.
        let other_cwd_dir = sessions_root.join("sOTHERCWD");
        std::fs::create_dir_all(&other_cwd_dir).unwrap();
        std::fs::write(
            other_cwd_dir.join("meta.json"),
            r#"{"harness":"grok","cwd":"/tmp/somewhere-else"}"#,
        )
        .unwrap();
        std::fs::write(
            other_cwd_dir.join("grok_session_id.txt"),
            "other-cwd-native-id",
        )
        .unwrap();

        // A same-cwd, different-harness session — must be excluded.
        let other_harness_dir = sessions_root.join("sOTHERHARNESS");
        std::fs::create_dir_all(&other_harness_dir).unwrap();
        std::fs::write(
            other_harness_dir.join("meta.json"),
            format!(r#"{{"harness":"claude","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();
        std::fs::write(
            other_harness_dir.join("grok_session_id.txt"),
            "wrong-harness-id",
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_SESSION_DATA_DIR", &own_dir);
        let ids = sibling_native_ids(cwd);
        std::env::remove_var("CONSTRUCT_SESSION_DATA_DIR");

        assert_eq!(ids, HashSet::from(["sibling-native-id".to_string()]));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sibling_native_ids_skips_dirs_missing_meta_or_native_id() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-siblings-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions_root = home.join("sessions");
        let own_dir = sessions_root.join("sOWN");
        std::fs::create_dir_all(&own_dir).unwrap();

        let cwd = Path::new("/tmp/agentd-shared-cwd");

        // No meta.json at all (e.g. a session mid-creation) — skipped, not a panic.
        let no_meta_dir = sessions_root.join("sNOMETA");
        std::fs::create_dir_all(&no_meta_dir).unwrap();

        // meta.json present but no grok_session_id.txt yet — skipped.
        let no_native_id_dir = sessions_root.join("sNONATIVEID");
        std::fs::create_dir_all(&no_native_id_dir).unwrap();
        std::fs::write(
            no_native_id_dir.join("meta.json"),
            format!(r#"{{"harness":"grok","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_SESSION_DATA_DIR", &own_dir);
        let ids = sibling_native_ids(cwd);
        std::env::remove_var("CONSTRUCT_SESSION_DATA_DIR");

        assert!(ids.is_empty());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn model_change_ignored_for_non_assistant_lines() {
        let v: Value = serde_json::json!({"type": "user", "model_id": "grok-4.5"});
        assert_eq!(grok_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_ignored_when_field_absent() {
        let v: Value = serde_json::json!({"type": "assistant", "content": "hi"});
        assert_eq!(grok_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_fires_on_first_observation() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5"});
        assert_eq!(grok_model_change(&v, &None).as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn model_change_silent_when_unchanged() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5"});
        assert_eq!(grok_model_change(&v, &Some("grok-4.5".to_string())), None);
    }

    #[test]
    fn model_change_fires_on_switch() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5-fast"});
        assert_eq!(
            grok_model_change(&v, &Some("grok-4.5".to_string())).as_deref(),
            Some("grok-4.5-fast")
        );
    }

    #[test]
    fn effort_change_ignored_for_non_assistant_lines() {
        let v: Value = serde_json::json!({"type": "user", "reasoning_effort": "high"});
        assert_eq!(grok_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_ignored_when_field_absent() {
        let v: Value = serde_json::json!({"type": "assistant", "content": "hi"});
        assert_eq!(grok_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_fires_on_first_observation() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "high"});
        assert_eq!(grok_effort_change(&v, &None).as_deref(), Some("high"));
    }

    #[test]
    fn effort_change_silent_when_unchanged() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "high"});
        assert_eq!(grok_effort_change(&v, &Some("high".to_string())), None);
    }

    #[test]
    fn effort_change_fires_on_switch() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "low"});
        assert_eq!(
            grok_effort_change(&v, &Some("high".to_string())).as_deref(),
            Some("low")
        );
    }

    /// A real `turn_completed` record, verbatim from a live session's
    /// `updates.jsonl` (values only; nothing about the shape is invented).
    fn turn_completed(session_id: &str) -> Value {
        serde_json::json!({
            "timestamp": 1785549199,
            "method": "_x.ai/session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "turn_completed",
                    "prompt_id": "f9839542-1988-4880-9616-37aa4212d460",
                    "stop_reason": "end_turn",
                    "usage": {
                        "inputTokens": 98232,
                        "outputTokens": 2785,
                        "totalTokens": 101017,
                        "cachedReadTokens": 65792,
                        "cacheCreationTokens": 0,
                        "reasoningTokens": 924,
                        "modelCalls": 3,
                        "apiDurationMs": 46219,
                        "costUsdTicks": 1013276000,
                        "modelUsage": {
                            "grok-4.5-build": {
                                "inputTokens": 98232,
                                "outputTokens": 2785,
                                "totalTokens": 101017,
                                "cachedReadTokens": 65792,
                                "cacheCreationTokens": 0,
                                "reasoningTokens": 924,
                                "modelCalls": 3
                            }
                        },
                        "numTurns": 3
                    }
                }
            }
        })
    }

    /// The prompt side is `inputTokens` and the cached subset is
    /// `cachedReadTokens` — `totalTokens` is exactly input + output, which is
    /// what establishes that input already covers the cache reads.
    #[test]
    fn turn_completed_reports_the_real_usage_split() {
        let v = turn_completed("root");
        match grok_usage_events(&v, "root", None).as_slice() {
            [SessionEvent::Cost {
                usd,
                tokens_in,
                tokens_out,
                tokens_cached,
                model,
            }] => {
                assert_eq!(*tokens_in, 98232);
                assert_eq!(*tokens_out, 2785);
                assert_eq!(*tokens_cached, 65792);
                assert!(tokens_cached < tokens_in, "cached must be a subset");
                // costUsdTicks has no documented scale; volume only.
                assert_eq!(*usd, 0.0);
                // Spelled as `chat_history.jsonl`'s `model_id`, which is what
                // `ModelChanged` carries — see `grok_model_change`.
                assert_eq!(model.as_deref(), Some("grok-4.5-build"));
            }
            other => panic!("expected one Cost, got {other:?}"),
        }
    }

    /// A subagent's own `turn_completed` must not land on the root's tally.
    #[test]
    fn another_sessions_usage_is_not_absorbed() {
        let v = turn_completed("some-subagent");
        assert!(grok_usage_events(&v, "root", None).is_empty());
    }

    /// Without a per-model split, the adapter's tracked model stands in
    /// rather than the sample going unattributed.
    #[test]
    fn falls_back_to_the_tracked_model_without_a_split() {
        let mut v = turn_completed("root");
        v["params"]["update"]["usage"]
            .as_object_mut()
            .expect("usage object")
            .remove("modelUsage");
        match grok_usage_events(&v, "root", Some("grok-4.5-build")).as_slice() {
            [SessionEvent::Cost {
                tokens_in, model, ..
            }] => {
                assert_eq!(*tokens_in, 98232);
                assert_eq!(model.as_deref(), Some("grok-4.5-build"));
            }
            other => panic!("expected one Cost, got {other:?}"),
        }
    }

    /// Every other update on the stream is chatter — only turn completions
    /// carry usage, and a chunk's running `totalTokens` must not be mistaken
    /// for one.
    #[test]
    fn ordinary_updates_carry_no_usage() {
        let chunk = serde_json::json!({
            "method": "session/update",
            "params": {
                "sessionId": "root",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hi"}
                },
                "_meta": {"totalTokens": 5731}
            }
        });
        assert!(grok_usage_events(&chunk, "root", None).is_empty());
        assert!(grok_usage_events(&Value::Null, "root", None).is_empty());
    }

    /// A turn that consumed nothing produces no event, so an empty tail
    /// record can't stamp a zero sample onto the meter.
    #[test]
    fn a_zero_usage_turn_reports_nothing() {
        let mut v = turn_completed("root");
        v["params"]["update"]["usage"] = serde_json::json!({
            "inputTokens": 0, "outputTokens": 0, "cachedReadTokens": 0
        });
        assert!(grok_usage_events(&v, "root", None).is_empty());
    }

    /// Several models inside one turn each get their own sample, so a turn
    /// that switched models is not credited entirely to one of them.
    #[test]
    fn each_model_in_a_turn_gets_its_own_sample() {
        let mut v = turn_completed("root");
        v["params"]["update"]["usage"]["modelUsage"] = serde_json::json!({
            "grok-4.5-build": {"inputTokens": 100, "outputTokens": 10, "cachedReadTokens": 0},
            "grok-4.5-fast": {"inputTokens": 200, "outputTokens": 20, "cachedReadTokens": 5}
        });
        let events = grok_usage_events(&v, "root", None);
        assert_eq!(events.len(), 2);
        let total: u64 = events
            .iter()
            .map(|e| match e {
                SessionEvent::Cost { tokens_in, .. } => *tokens_in,
                _ => 0,
            })
            .sum();
        assert_eq!(total, 300, "the split is used instead of the aggregate");
    }

    #[test]
    fn grok_allow_args_omits_multiedit() {
        let policy =
            construct_protocol::adapter::policy::AutoApprovePolicy::new(vec![PathBuf::from(
                "/var/agentd/widgets",
            )]);
        let args = grok_allow_args_for(&policy);
        assert_eq!(
            args,
            vec![
                "--allow".to_string(),
                "Write(/var/agentd/widgets/**)".to_string(),
                "--allow".to_string(),
                "Edit(/var/agentd/widgets/**)".to_string(),
            ]
        );
    }
}
