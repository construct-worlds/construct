//! Kimi Code CLI adapter.
//!
//! Runs Kimi Code's native TUI under construct's PTY. Kimi appends every
//! session it creates to an index file under its home directory
//! (`session_index.jsonl`: one `{sessionId, sessionDir, workDir}` object per
//! line), so the adapter tails that file to learn the native session id of
//! the conversation it spawned. The id lets a construct session resume the
//! same Kimi conversation after a daemon restart via `--session <id>`; the
//! recorded session directory also locates the session's wire log, whose
//! `config.update` lines carry the model alias and thinking effort the
//! session is actually answering with.
//!
//! Kimi's `--prompt` flag is headless-only (it suppresses the TUI), so an
//! initial prompt is instead typed into the PTY once the index shows the
//! native session exists — the closest signal Kimi gives that its TUI is up.
//!
//! Honors `CONSTRUCT_KIMI_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_KIMI_BIN`, then `kimi` on `PATH`, then the standard installer
//! location at `~/.kimi-code/bin/kimi`. The Kimi home directory watched for
//! session data follows `CONSTRUCT_KIMI_HOME`, then Kimi's own
//! `KIMI_CODE_HOME`, then `~/.kimi-code`.

use construct_adapter_common::context_breakdown::{
    estimate_tokens_from_chars, BreakdownGate, FixedOverheadPin,
};
use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::{
    Capabilities, ContextSegment, InitializeResult, PtySize, SessionEvent, SessionStartParams,
};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

const SESSION_ID_FILE: &str = "kimi_session_id.txt";

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "kimi".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, run_interactive).await
}

fn kimi_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CONSTRUCT_KIMI_HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("KIMI_CODE_HOME") {
        return Some(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".kimi-code"))
}

fn session_data_dir() -> Option<PathBuf> {
    std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn conv_id_file() -> Option<PathBuf> {
    Some(session_data_dir()?.join(SESSION_ID_FILE))
}

fn read_conv_id() -> Option<String> {
    let path = conv_id_file()?;
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_kimi_session_id(s))
}

fn write_conv_id(id: &str) {
    if let Some(path) = conv_id_file() {
        let _ = std::fs::write(path, id);
    }
}

/// Kimi names sessions `session_<uuid>`; anything else in an id file is a
/// half-write or foreign content and must not be replayed into `--session`.
fn is_kimi_session_id(value: &str) -> bool {
    value.len() > "session_".len() && value.starts_with("session_")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    session_id: String,
    session_dir: PathBuf,
    work_dir: PathBuf,
}

fn parse_index_entry(line: &str) -> Option<IndexEntry> {
    let v: Value = serde_json::from_str(line).ok()?;
    let session_id = v.get("sessionId")?.as_str()?.to_string();
    if !is_kimi_session_id(&session_id) {
        return None;
    }
    Some(IndexEntry {
        session_id,
        session_dir: PathBuf::from(v.get("sessionDir")?.as_str()?),
        work_dir: PathBuf::from(v.get("workDir")?.as_str()?),
    })
}

/// Parse index lines past `next_line`, advancing the cursor. The index is
/// append-only, so a line-count cursor is a stable position in it.
fn read_new_index_entries(path: &Path, next_line: &mut usize) -> Vec<IndexEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = 0usize;
    let mut entries = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        seen = idx + 1;
        if idx < *next_line || line.trim().is_empty() {
            continue;
        }
        if let Some(entry) = parse_index_entry(line) {
            entries.push(entry);
        }
    }
    *next_line = seen;
    entries
}

/// Other construct sessions' kimi native ids, for sessions sharing this
/// one's `cwd` (read from each sibling's own `kimi_session_id.txt` next to
/// its `meta.json`, both written under `<data_dir>/sessions/<construct_id>/`).
///
/// Kimi's index is global, so a sibling construct session started in the
/// same cwd appends an entry indistinguishable from ours by content alone.
/// Excluding every id a sibling has already claimed keeps a sibling's fresh
/// session from being bound (or rebound) onto this one.
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
        if meta.get("harness").and_then(|h| h.as_str()) != Some("kimi") {
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
        if let Ok(native_id) = std::fs::read_to_string(path.join(SESSION_ID_FILE)) {
            let native_id = native_id.trim();
            if !native_id.is_empty() {
                ids.insert(native_id.to_string());
            }
        }
    }
    ids
}

fn wire_log_path(session_dir: &Path) -> PathBuf {
    session_dir.join("agents").join("main").join("wire.jsonl")
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// The session's model alias, if `v` is a `config.update` wire line carrying
/// a `modelAlias` different from what we last saw (e.g.
/// `kimi-code/kimi-for-coding`). Kimi stamps one of these at session start
/// and again on an in-TUI model switch.
fn wire_model_change(v: &Value, last_model: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("config.update") {
        return None;
    }
    let model = v.get("modelAlias").and_then(|m| m.as_str())?;
    (last_model.as_deref() != Some(model)).then(|| model.to_string())
}

/// Same signal for `thinkingEffort` on the same `config.update` lines.
fn wire_effort_change(v: &Value, last_effort: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("config.update") {
        return None;
    }
    let effort = v.get("thinkingEffort").and_then(|e| e.as_str())?;
    (last_effort.as_deref() != Some(effort)).then(|| effort.to_string())
}

/// Token usage from a `context.append_loop_event`/`step.end` wire line
/// (spec 0103) plus the context gauge it implies (spec 0104). Kimi stamps
/// one per LLM call:
/// `{"usage":{"inputOther","output","inputCacheRead","inputCacheCreation"}}`.
/// `tokens_in` (and the gauge's `used_tokens` — the prompt side that
/// actually filled the window on this call) covers other + cache reads +
/// cache creation, keeping `tokens_cached ⊆ tokens_in` per the Cost
/// contract. Kimi states no window size, so the gauge has no denominator.
/// No dedupe needed — each step lands exactly once and the watcher's line
/// cursor already skips history on resume.
fn wire_usage_events(v: &Value) -> Vec<SessionEvent> {
    if v.get("type").and_then(|t| t.as_str()) != Some("context.append_loop_event") {
        return Vec::new();
    }
    let Some(event) = v.get("event") else {
        return Vec::new();
    };
    if event.get("type").and_then(|t| t.as_str()) != Some("step.end") {
        return Vec::new();
    }
    let Some(usage) = event.get("usage") else {
        return Vec::new();
    };
    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let other = field("inputOther");
    let output = field("output");
    let cache_read = field("inputCacheRead");
    let cache_creation = field("inputCacheCreation");
    if other == 0 && output == 0 && cache_read == 0 && cache_creation == 0 {
        return Vec::new();
    }
    let prompt_side = other
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
    vec![
        SessionEvent::Cost {
            usd: 0.0,
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

/// Conversation chars across a full `wire.jsonl` scan, feeding the
/// `messages` breakdown segment (spec 0156). Everything kimi appends to
/// the model context is journaled as a `context.append_*` wire line;
/// shapes verified against real sessions on this machine (2026-07-28):
///
/// - `context.append_message`: `message.content` is a list of
///   `{type:"text",text}` items (user turns; `toolCalls` observed empty).
/// - `context.append_loop_event` `content.part`: one `part.text` or
///   `part.think` string (assistant text / thinking).
/// - `context.append_loop_event` `tool.call`: `name` plus structured
///   `args`, counted as serialized JSON.
/// - `context.append_loop_event` `tool.result`: `result.output` string.
///
/// `turn.prompt` mirrors the user text `context.append_message` already
/// carries (counting both would double-count), and `step.begin`/`step.end`
/// /`config.update`/`llm.request` etc. are bookkeeping — no conversation
/// chars. The full re-scan at turn cadence is deliberate: resume and
/// native-id rebinds self-correct without incremental bookkeeping.
fn wire_line_chars(v: &Value) -> usize {
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "context.append_message" => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .map(|t| t.chars().count())
                    .sum()
            })
            .unwrap_or(0),
        "context.append_loop_event" => {
            let Some(event) = v.get("event") else {
                return 0;
            };
            match event.get("type").and_then(Value::as_str).unwrap_or("") {
                "content.part" => event
                    .get("part")
                    .and_then(|p| p.get("text").or_else(|| p.get("think")))
                    .and_then(Value::as_str)
                    .map(|t| t.chars().count())
                    .unwrap_or(0),
                "tool.call" => {
                    let name = event
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| n.chars().count())
                        .unwrap_or(0);
                    let args = event
                        .get("args")
                        .filter(|a| !a.is_null())
                        .map(|a| a.to_string().chars().count())
                        .unwrap_or(0);
                    name + args
                }
                "tool.result" => event
                    .pointer("/result/output")
                    .and_then(Value::as_str)
                    .map(|t| t.chars().count())
                    .unwrap_or(0),
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// The breakdown segments implied by a full wire-log scan, fixed-prefix
/// first: an estimated `tools` segment from the latest `llm.tools_snapshot`
/// (skipped when the log has none), the differential `fixed overhead`
/// residual (spec 0156) pinned on the log's first `step.end` usage, then
/// the estimated `messages` conversation segment. Kimi's system prompt
/// never lands on disk (`llm.request` records only a `systemPromptHash`),
/// so it has no segment of its own — it is exactly what the pinned
/// residual measures.
fn wire_breakdown_segments(text: &str) -> Vec<ContextSegment> {
    let mut tools_chars = 0usize;
    let mut convo_chars = 0usize;
    let mut pin = FixedOverheadPin::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Kimi journals the tool schemas verbatim as `llm.tools_snapshot`
        // lines (shape verified against real sessions on this machine,
        // 2026-07-28), re-stamping a new snapshot whenever the active
        // toolset changes — the last one seen describes the live prefix.
        // Counted as the serialized `tools` array.
        if v.get("type").and_then(Value::as_str) == Some("llm.tools_snapshot") {
            tools_chars = v
                .get("tools")
                .map(|t| t.to_string().chars().count())
                .unwrap_or(tools_chars);
        }
        if let Some(prompt_side) = wire_step_prompt_side(&v) {
            pin.observe(
                prompt_side,
                estimate_tokens_from_chars(tools_chars)
                    .saturating_add(estimate_tokens_from_chars(convo_chars)),
            );
        }
        convo_chars += wire_line_chars(&v);
    }
    let mut segments = Vec::new();
    if tools_chars > 0 {
        segments.push(ContextSegment::new(
            "tools",
            estimate_tokens_from_chars(tools_chars),
            true,
        ));
    }
    segments.extend(pin.segment());
    if convo_chars > 0 {
        segments.push(ContextSegment::new(
            "messages",
            estimate_tokens_from_chars(convo_chars),
            true,
        ));
    }
    segments
}

/// Prompt side of a `step.end` wire record's usage — the same arithmetic
/// `wire_usage_events` feeds the gauge, minus the event plumbing. `None`
/// for every other record and for all-zero usage.
fn wire_step_prompt_side(v: &Value) -> Option<u64> {
    if v.get("type").and_then(Value::as_str) != Some("context.append_loop_event") {
        return None;
    }
    let event = v.get("event")?;
    if event.get("type").and_then(Value::as_str) != Some("step.end") {
        return None;
    }
    let usage = event.get("usage")?;
    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let prompt_side = field("inputOther")
        .saturating_add(field("inputCacheRead"))
        .saturating_add(field("inputCacheCreation"));
    (prompt_side > 0).then_some(prompt_side)
}

fn append_launch_args(args: &mut Vec<String>, model: Option<&str>, native_id: Option<&str>) {
    if let Some(model) = model {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(id) = native_id {
        args.extend(["--session".into(), id.into()]);
    }
    // No prompt arg on purpose: Kimi's `--prompt` runs headless and never
    // opens the TUI, so the initial prompt is typed into the PTY instead
    // (see `spawn_session_watcher`).
}

/// The prompt must be typed and submitted as two separate PTY writes: the
/// TUI runs with bracketed paste on, so a single chunk containing the text
/// AND a trailing carriage return is treated as one paste — the return
/// becomes a newline inside the composer instead of a submit (verified live
/// against kimi 0.27.0). A lone carriage return sent afterwards is a real
/// Enter keypress.
fn prompt_text_bytes(prompt: &str) -> Vec<u8> {
    prompt.as_bytes().to_vec()
}

struct WatcherSetup {
    home: PathBuf,
    cwd: PathBuf,
    /// The persisted native id when resuming; `None` on first spawn.
    initial_id: Option<String>,
    /// Model the daemon asked for at launch; seeds change detection so a
    /// resume on the same model stays quiet.
    initial_model: Option<String>,
    /// Initial prompt to type into the PTY once the native session exists,
    /// with the sender feeding the PTY inbox. `None` when resuming or when
    /// no prompt was given.
    type_prompt: Option<(String, mpsc::Sender<AdapterInboxMsg>)>,
}

fn spawn_session_watcher(setup: WatcherSetup, emit: EventEmitter) {
    tokio::spawn(async move {
        let WatcherSetup {
            home,
            cwd,
            initial_id,
            initial_model,
            mut type_prompt,
        } = setup;
        let index_path = home.join("session_index.jsonl");

        // Baseline: entries already in the index predate this adapter, so
        // they are historical or belong to sibling construct sessions —
        // never bindable. On resume, the one describing our own persisted id
        // is looked up (for its session dir) but the cursor still starts at
        // the end.
        let mut cursor = 0usize;
        let existing = read_new_index_entries(&index_path, &mut cursor);
        let mut current: Option<IndexEntry> = initial_id.as_ref().and_then(|id| {
            existing
                .iter()
                .find(|entry| &entry.session_id == id)
                .cloned()
        });
        if initial_id.is_some() && current.is_none() {
            emit.log(
                "kimi respawn: persisted session id not found in Kimi's session index; \
                 transcript metadata (model, effort) will not be tracked",
            );
        }

        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
        // A resumed session's wire log already holds the whole conversation;
        // only new lines matter. A freshly bound session starts at the top.
        let mut wire_cursor = current
            .as_ref()
            .map(|entry| count_lines(&wire_log_path(&entry.session_dir)))
            .unwrap_or(0);

        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Sibling composition changes rarely; refresh their claimed ids
        // every 10 ticks (5s) rather than re-scanning every pass.
        let mut sibling_ids = sibling_native_ids(&cwd);
        let mut ticks_since_sibling_refresh = 0u32;
        // Breakdown (spec 0156): recomputed by full wire-log scan at
        // `step.end` cadence, reported only when it changes.
        let mut breakdown_gate = BreakdownGate::default();
        loop {
            tick.tick().await;
            ticks_since_sibling_refresh += 1;
            if ticks_since_sibling_refresh >= 10 {
                ticks_since_sibling_refresh = 0;
                sibling_ids = sibling_native_ids(&cwd);
            }

            for entry in read_new_index_entries(&index_path, &mut cursor) {
                if entry.work_dir != cwd
                    || sibling_ids.contains(&entry.session_id)
                    || current
                        .as_ref()
                        .is_some_and(|c| c.session_id == entry.session_id)
                {
                    continue;
                }
                if let Some(prior) = current.as_ref() {
                    // A newer entry for our cwd that no sibling claims is
                    // this TUI starting a fresh conversation (Kimi's own
                    // new-session flow): follow it.
                    emit.log(format!(
                        "kimi: native session id changed {} -> {}; rebinding",
                        prior.session_id, entry.session_id
                    ));
                    emit.emit(SessionEvent::NativeIdChanged {
                        prior_native_id: prior.session_id.clone(),
                        new_native_id: entry.session_id.clone(),
                    });
                }
                write_conv_id(&entry.session_id);
                wire_cursor = 0;
                current = Some(entry);
                if let Some((prompt, tx)) = type_prompt.take() {
                    // The index entry appears while the TUI finishes
                    // booting; a short grace keeps the keystrokes from
                    // landing before the input box exists.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        if tx
                            .send(AdapterInboxMsg::PtyInput(prompt_text_bytes(&prompt)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = tx.send(AdapterInboxMsg::PtyInput(vec![b'\r'])).await;
                    });
                }
            }

            let Some(entry) = current.as_ref() else {
                continue;
            };
            let wire_path = wire_log_path(&entry.session_dir);
            if !wire_path.exists() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&wire_path) else {
                continue;
            };
            let mut saw_step_usage = false;
            for (idx, line) in text.lines().enumerate() {
                if idx < wire_cursor || line.trim().is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(model) = wire_model_change(&v, &last_model) {
                    last_model = Some(model.clone());
                    emit.emit(SessionEvent::ModelChanged { model });
                }
                if let Some(effort) = wire_effort_change(&v, &last_effort) {
                    last_effort = Some(effort.clone());
                    emit.emit(SessionEvent::EffortChanged { effort });
                }
                let usage_events = wire_usage_events(&v);
                saw_step_usage |= !usage_events.is_empty();
                for event in usage_events {
                    emit.emit(event);
                }
            }
            wire_cursor = text.lines().count();
            if saw_step_usage {
                // Same cadence as the gauge: a `step.end` with usage just
                // landed, so re-derive the messages estimate from the whole
                // log (`text` is already the full file).
                let segments = wire_breakdown_segments(&text);
                if !segments.is_empty() && breakdown_gate.changed(&segments) {
                    emit.emit(SessionEvent::ContextBreakdown { segments });
                }
            }
        }
    });
}

async fn run_interactive(params: SessionStartParams, mut ctx: AdapterContext) {
    let default_bin = construct_protocol::adapter::default_cli_bin_with_home_fallback(
        "kimi",
        Path::new(".kimi-code/bin/kimi"),
    );
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_KIMI_CMD",
        "CONSTRUCT_KIMI_BIN",
        &default_bin,
    );
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let native_id = resuming.then(read_conv_id).flatten();
    if resuming && native_id.is_none() {
        ctx.emit
            .log("kimi respawn: no captured native session id; starting a fresh conversation");
    }

    let mut args = command.args.clone();
    args.extend(params.args.clone());
    append_launch_args(&mut args, params.model.as_deref(), native_id.as_deref());

    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), ctx.session_id.clone()));

    let home = kimi_home();
    if let Some(home) = home.as_ref() {
        // Keep the spawned CLI and our watcher pointed at the same home even
        // when the override came from `CONSTRUCT_KIMI_HOME`.
        env.retain(|(key, _)| key != "KIMI_CODE_HOME");
        env.push(("KIMI_CODE_HOME".into(), home.to_string_lossy().into_owned()));
    }

    let prompt = (!resuming)
        .then(|| params.prompt.clone().filter(|s| !s.trim().is_empty()))
        .flatten();
    let type_prompt = prompt.map(|prompt| {
        // Interpose on the PTY inbox so the watcher can type the initial
        // prompt once Kimi's session exists; everything the daemon sends
        // flows through unchanged.
        let (tx, rx) = mpsc::channel(64);
        let mut real_inbox = std::mem::replace(&mut ctx.inbox, rx);
        let forward = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = real_inbox.recv().await {
                if forward.send(msg).await.is_err() {
                    break;
                }
            }
        });
        (prompt, tx)
    });

    match home {
        Some(home) => spawn_session_watcher(
            WatcherSetup {
                home,
                cwd: PathBuf::from(&params.cwd),
                initial_id: native_id,
                initial_model: params.model.clone(),
                type_prompt,
            },
            ctx.emit.clone(),
        ),
        None => ctx
            .emit
            .log("kimi: no CONSTRUCT_KIMI_HOME/KIMI_CODE_HOME/HOME — cannot track native session"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_entries_parse_and_respect_the_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("session_index.jsonl");
        std::fs::write(
            &index,
            concat!(
                r#"{"sessionId":"session_aaa","sessionDir":"/h/s/session_aaa","workDir":"/w1"}"#,
                "\n",
                "not json\n",
                r#"{"sessionId":"session_bbb","sessionDir":"/h/s/session_bbb","workDir":"/w2"}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut cursor = 0;
        let entries = read_new_index_entries(&index, &mut cursor);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, "session_aaa");
        assert_eq!(entries[1].work_dir, PathBuf::from("/w2"));
        assert_eq!(cursor, 3);

        // Appending one more line yields only that line.
        let mut text = std::fs::read_to_string(&index).unwrap();
        text.push_str(
            r#"{"sessionId":"session_ccc","sessionDir":"/h/s/session_ccc","workDir":"/w1"}"#,
        );
        text.push('\n');
        std::fs::write(&index, text).unwrap();
        let entries = read_new_index_entries(&index, &mut cursor);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "session_ccc");
    }

    #[test]
    fn index_entries_reject_foreign_session_id_shapes() {
        assert_eq!(
            parse_index_entry(r#"{"sessionId":"ses_abc","sessionDir":"/d","workDir":"/w"}"#),
            None
        );
        assert_eq!(
            parse_index_entry(r#"{"sessionId":"session_","sessionDir":"/d","workDir":"/w"}"#),
            None
        );
        assert!(parse_index_entry(
            r#"{"sessionId":"session_b877a11f","sessionDir":"/d","workDir":"/w"}"#
        )
        .is_some());
    }

    #[test]
    fn model_change_fires_on_config_update_only() {
        let update = serde_json::json!({
            "type": "config.update",
            "modelAlias": "kimi-code/kimi-for-coding",
            "thinkingEffort": "high",
        });
        assert_eq!(
            wire_model_change(&update, &None).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        assert_eq!(
            wire_model_change(&update, &Some("kimi-code/kimi-for-coding".into())),
            None
        );
        assert_eq!(wire_effort_change(&update, &None).as_deref(), Some("high"));
        assert_eq!(wire_effort_change(&update, &Some("high".into())), None);

        let other = serde_json::json!({"type": "metadata", "modelAlias": "x"});
        assert_eq!(wire_model_change(&other, &None), None);
    }

    #[test]
    fn step_end_usage_emits_cost_with_full_prompt_side() {
        // Real wire.jsonl shape: one usage per LLM step. `tokens_in` must
        // cover other + cache reads + cache creation, with the reads
        // broken out in `tokens_cached`.
        let v = serde_json::json!({
            "type": "context.append_loop_event",
            "event": {
                "type": "step.end",
                "usage": {
                    "inputOther": 1029,
                    "output": 196,
                    "inputCacheRead": 68_864,
                    "inputCacheCreation": 512
                }
            }
        });
        match wire_usage_events(&v).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            }] => {
                assert_eq!(*tokens_in, 1029 + 68_864 + 512);
                assert_eq!(*tokens_out, 196);
                assert_eq!(*tokens_cached, 68_864);
                // Context gauge (spec 0104): same prompt side, no window —
                // kimi never states one.
                assert_eq!(*used_tokens, 1029 + 68_864 + 512);
                assert_eq!(*window_tokens, None);
            }
            other => panic!("expected Cost + ContextUsage: {other:?}"),
        }
        // Non-step lines and steps without usage contribute nothing.
        let other = serde_json::json!({
            "type": "context.append_loop_event",
            "event": { "type": "step.begin" }
        });
        assert!(wire_usage_events(&other).is_empty());
    }

    #[test]
    fn resume_uses_native_id_and_model_without_prompt_args() {
        let mut args = Vec::new();
        append_launch_args(&mut args, Some("kimi-for-coding"), Some("session_abc"));
        assert_eq!(
            args,
            ["--model", "kimi-for-coding", "--session", "session_abc"]
        );

        let mut fresh = Vec::new();
        append_launch_args(&mut fresh, None, None);
        assert!(fresh.is_empty());
    }

    #[test]
    fn prompt_text_bytes_carry_no_submit_key() {
        // The submitting carriage return is a separate PTY write; bundling
        // it here would be swallowed by the TUI's bracketed paste handling.
        assert_eq!(prompt_text_bytes("fix the test"), b"fix the test");
    }

    #[test]
    fn native_id_file_requires_kimi_session_shape() {
        assert!(is_kimi_session_id(
            "session_b877a11f-adde-4b0f-8825-6b4329c62203"
        ));
        assert!(!is_kimi_session_id("session_"));
        assert!(!is_kimi_session_id("ses_abc123"));
    }

    /// Line shapes verified against real `wire.jsonl` files on this
    /// machine (2026-07-28): user turns land as `context.append_message`
    /// with a `{type:"text",text}` item list (and again as `turn.prompt`,
    /// which must NOT be double-counted), assistant output as
    /// `content.part` think/text loop events, tool traffic as `tool.call`
    /// (structured `args`) and `tool.result` (`result.output` string),
    /// and the full active tool schemas as `llm.tools_snapshot` lines —
    /// re-stamped on toolset change, so only the latest one is live.
    fn wire_breakdown_fixture() -> String {
        concat!(
            r#"{"type":"metadata","protocol_version":"1.4"}"#,
            "\n",
            r#"{"type":"llm.tools_snapshot","hash":"aaa","tools":[{"name":"Old"}]}"#,
            "\n",
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"fix the icon"}],"origin":{"kind":"user"}}"#,
            "\n",
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"fix the icon"}],"toolCalls":[],"origin":{"kind":"user"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"u1","turnId":"0","step":1}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"Find the button."}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t1","name":"Grep","args":{"pattern":"expand"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"t1","result":{"output":"no matches","isError":false}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"Done."}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0}}}"#,
            "\n",
            r#"{"type":"llm.tools_snapshot","hash":"bbb","tools":[{"name":"Grep","description":"Search files."}]}"#,
            "\n",
            r#"{"type":"config.update","modelAlias":"kimi-code/k3","thinkingEffort":"low"}"#,
            "\n",
        )
        .to_string()
    }

    #[test]
    fn wire_scan_counts_context_appends_once() {
        // context.append_message 12 ("fix the icon") + think 16
        // ("Find the button.") + tool.call 4 ("Grep") + 20 (serialized
        // {"pattern":"expand"}) + tool.result 10 ("no matches") + text 5
        // ("Done.") = 67. turn.prompt mirrors the user text and counts 0;
        // metadata/llm.tools_snapshot/step.begin/step.end/config.update
        // count 0 here (the snapshot feeds the `tools` segment instead).
        let chars: usize = wire_breakdown_fixture()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|v| wire_line_chars(&v))
            .sum();
        assert_eq!(chars, 67);
    }

    #[test]
    fn tools_segment_uses_only_the_latest_snapshot() {
        // Two snapshots in the fixture; only the later ("bbb") one is the
        // live toolset. Its serialized `tools` array,
        // `[{"name":"Grep","description":"Search files."}]`, is 47 chars
        // (key order cannot change the length) -> ~13 tokens; the earlier
        // "Old" snapshot (16 chars -> ~4) must not leak through.
        let segments = wire_breakdown_segments(&wire_breakdown_fixture());
        assert_eq!(segments[0], ContextSegment::new("tools", 13, true));
        assert!(wire_breakdown_segments("").is_empty());
    }

    #[test]
    fn breakdown_pins_fixed_overhead_on_first_step_end_usage() {
        use construct_adapter_common::context_breakdown::FIXED_OVERHEAD_LABEL;
        // First step.end arrives with tools (16 chars -> ~4 tokens) and one
        // user message (12 chars -> ~3 tokens) on the books; its 5000-token
        // prompt side pins the residual at 4993 — kimi's on-disk-invisible
        // system prompt. The later, larger step.end must not move the pin.
        let wire = concat!(
            r#"{"type":"llm.tools_snapshot","hash":"aaa","tools":[{"name":"Old"}]}"#,
            "\n",
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"fix the icon"}]}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":5000,"output":5,"inputCacheRead":0,"inputCacheCreation":0}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"Done."}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":9000,"output":5,"inputCacheRead":0,"inputCacheCreation":0}}}"#,
            "\n",
        );
        let segments = wire_breakdown_segments(wire);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].label, "tools");
        assert_eq!(
            segments[1],
            ContextSegment::new(FIXED_OVERHEAD_LABEL, 4993, true)
        );
        assert_eq!(segments[2].label, "messages");
    }

    #[test]
    fn breakdown_orders_tools_before_messages_and_gates_repeats() {
        let text = wire_breakdown_fixture();
        let segments = wire_breakdown_segments(&text);
        // Fixed-prefix first: tools 47 chars / 3.5 -> ~13 tokens, then
        // messages 67 chars / 3.5 -> ~19, both estimated by contract
        // (spec 0156).
        assert_eq!(
            segments,
            vec![
                ContextSegment::new("tools", 13, true),
                ContextSegment::new("messages", 19, true),
            ]
        );

        // A log with no tools snapshot skips the segment, not zeroes it.
        let no_snapshot = r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"Done."}}}"#;
        assert_eq!(
            wire_breakdown_segments(no_snapshot),
            vec![ContextSegment::new("messages", 1, true)]
        );

        // Report-on-change: recomputing over an unchanged log stays quiet;
        // a grown log fires again.
        let mut gate = BreakdownGate::default();
        assert!(gate.changed(&segments));
        assert!(!gate.changed(&wire_breakdown_segments(&text)));
        let mut grown = text.clone();
        grown.push_str(concat!(
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"And pushed the fix."}}}"#,
            "\n",
        ));
        assert!(gate.changed(&wire_breakdown_segments(&grown)));

        // An empty or content-free log yields no segments at all.
        assert!(wire_breakdown_segments("").is_empty());
        assert!(
            wire_breakdown_segments(r#"{"type":"metadata","protocol_version":"1.4"}"#).is_empty()
        );
    }

    #[test]
    fn wire_log_path_is_the_main_agents_log() {
        assert_eq!(
            wire_log_path(Path::new("/h/sessions/wd_x/session_a")),
            PathBuf::from("/h/sessions/wd_x/session_a/agents/main/wire.jsonl")
        );
    }
}
