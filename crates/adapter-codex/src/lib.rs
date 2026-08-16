//! OpenAI Codex CLI adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns `codex`
//!   under a PTY, giving the user the real Codex TUI experience.
//!
//! - **headless (opt-in)** — multi-turn structured mode that spawns
//!   `codex exec <prompt>` per turn. Best-effort: if your codex build
//!   supports session resumption, set `CONSTRUCT_CODEX_RESUME_FLAG` to the flag
//!   name (e.g. `--session-id`) and the adapter will pass any captured
//!   `session_id` back in for subsequent turns.
//!
//! Pick mode via `--mode interactive|headless` on `construct new`, or via
//! `CONSTRUCT_CODEX_MODE=interactive|headless`. Honors `CONSTRUCT_CODEX_CMD` for a
//! full command prefix, falling back to `CONSTRUCT_CODEX_BIN` for a binary path.

use std::borrow::Cow;

use construct_adapter_common::context_breakdown::{
    estimate_tokens_from_chars, BreakdownGate, FixedOverheadPin,
};
use construct_adapter_common::{
    codex_sessions_root, drive_turn, emit_launch_failure_if_silent, next_native_seq, short,
    StderrTail, TurnOutcome,
};
use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::ContextSegment;
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "codex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
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
    if let Ok(m) = std::env::var("CONSTRUCT_CODEX_MODE") {
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

fn inject_model_catalog_arg(
    args: &mut Vec<String>,
    env: &std::collections::HashMap<String, String>,
) {
    let Some(path) = env
        .get(construct_protocol::adapter::ENV_CODEX_MODEL_CATALOG)
        .filter(|path| !path.trim().is_empty())
    else {
        return;
    };
    // JSON string escaping is also valid TOML basic-string escaping, which
    // keeps spaces, quotes, and platform path separators inside the value
    // passed to Codex's `-c key=value` parser.
    let quoted = serde_json::to_string(path).unwrap_or_else(|_| format!("{path:?}"));
    args.push("-c".into());
    args.push(format!("model_catalog_json={quoted}"));
    // Codex's built-in OpenAI provider enables Responses-over-WebSocket.
    // Construct's routing proxy currently speaks HTTP/SSE, so a published
    // model selected from the native picker would first try the fixed
    // ChatGPT WebSocket endpoint and surface a 405 before falling back.
    //
    // Keep the override session-local and let Codex choose the normal
    // ChatGPT or API endpoint from its active auth mode. Requiring OpenAI
    // auth preserves the user's native credential for pass-through models;
    // routed requests still have that credential replaced by the proxy.
    args.push("-c".into());
    args.push("model_provider=\"construct_router\"".into());
    args.push("-c".into());
    args.push(
        "model_providers.construct_router={name=\"Construct router\",wire_api=\"responses\",requires_openai_auth=true,supports_websockets=false}"
            .into(),
    );
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CODEX_CMD",
        "CONSTRUCT_CODEX_BIN",
        "codex",
    );
    let mut args = command.args.clone();
    args.extend(params.args.clone());
    // The daemon's auto-approval policy (`CONSTRUCT_AUTO_APPROVE_PATHS`, see
    // `construct_protocol::adapter::policy`) is set, but the upstream codex CLI
    // does not currently expose a path-scoped allow-list flag, so there's no
    // native translation to apply here. Either upstream gains the knob or we
    // wrap codex's IO to intercept tool calls.
    // Resume support: codex doesn't let the client assign a session id, so
    // we tag each spawn with a unique `originator` (via codex's internal
    // env override) and watch its rollouts dir for one bearing that tag.
    // When we see it, we persist codex's UUID to
    // `<session-dir>/codex_session_id.txt`; on daemon-restart respawn we
    // pass it back as `codex resume <uuid>`. The explicit override
    // `CONSTRUCT_CODEX_RESUME_ID` still wins if set.
    //
    // We deliberately do NOT fall back to `codex resume --last` when no id
    // was captured: `--last` resolves globally across every codex session
    // on the machine, so two agentd codex sessions both falling through
    // would attach to the same upstream codex and from that moment paint
    // identical PTY content. Starting a fresh codex loses one session's
    // conversation but never conflates two of them.
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("codex_session_id.txt"));
    let mut captured_id: Option<String> = None;
    if resuming {
        let explicit = std::env::var("CONSTRUCT_CODEX_RESUME_ID").ok();
        let from_file = sid_file.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
        captured_id = explicit.or(from_file);
        if let Some(id) = captured_id.as_ref() {
            args.insert(0, "resume".into());
            args.insert(1, id.clone());
        } else {
            ctx.emit.log(
                "codex respawn: no captured session id (codex_session_id.txt missing); \
                 starting a fresh codex conversation to avoid `--last` conflating sessions",
            );
        }
    }
    let fork_from = (!resuming)
        .then(|| {
            std::env::var("CONSTRUCT_CODEX_FORK_FROM")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .flatten();
    if let Some(parent) = fork_from.clone() {
        // Same-harness fork: `codex fork <parent-uuid>` starts a NEW codex
        // conversation inheriting the parent's exact context (the daemon
        // read the parent's captured id — spec 0031/0078). The forked
        // rollout copies the parent's meta (originator included) and stamps
        // `forked_from_id`, which is how the watcher below identifies it.
        args.insert(0, "fork".into());
        args.insert(1, parent);
    }
    if let Some(m) = params.model.as_ref() {
        args.push("-m".into());
        args.push(m.clone());
    }
    // Auto-inject agentd MCP server via codex's `-c` override (codex has no
    // `--mcp-config` flag — MCP servers live in `[mcp_servers.<name>]`).
    // Opt out with CONSTRUCT_INJECT_MCP=0.
    for a in construct_protocol::adapter::maybe_inject_codex_mcp_args(&ctx.session_id) {
        args.push(a);
    }
    inject_model_catalog_arg(&mut args, &params.env);
    // Skip the initial prompt only when we're actually resuming an
    // existing codex session; a respawn that fell through to a fresh
    // codex should still pass the original prompt.
    let resuming_existing = resuming && captured_id.is_some();
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
    // Tag this codex's rollout with a unique originator we can grep for.
    // Codex stamps `payload.originator` in the rollout's session_meta line
    // from this internal env var (found by string-grep on the binary; not
    // a public flag but stable across recent codex releases). Without the
    // tag we'd have to guess which of several concurrent codex rollouts
    // in the same cwd belongs to which construct session.
    let originator_tag = format!("agentd:{}", ctx.session_id);
    env.push((
        "CODEX_INTERNAL_ORIGINATOR_OVERRIDE".into(),
        originator_tag.clone(),
    ));
    // Watch the native rollout JSONL for this interactive Codex TUI and
    // mirror its semantic messages/tool events into agentd's transcript.
    // The PTY remains the interactive surface; these events make web chat
    // mode readable without scraping terminal escape sequences.
    if let Some(sid_path) = sid_file.clone() {
        spawn_interactive_transcript_watcher(
            sid_path,
            originator_tag,
            params.env.clone(),
            ctx.emit.clone(),
            resuming_existing,
            captured_id.clone(),
            fork_from.clone(),
            params.model.clone(),
        );
    }
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
    let _ = run_pty(spec, ctx).await;
}

/// Watch codex's sessions directory for the rollout file tagged with our
/// originator marker, then persist its UUID to
/// `<session-dir>/codex_session_id.txt` so a future daemon restart can
/// resume the same upstream codex conversation by id.
///
/// Polls for the entire session lifetime (the spawn dies when the
/// adapter process exits). No timeout because codex flushes its rollout
/// lazily — sometimes within a second, sometimes only after the first
/// turn completes minutes later. To keep the work cheap, files that
/// don't bear our originator are remembered and not re-read.
///
/// After the first match we keep scanning: `/clear` / `/new` mint a new
/// codex session id under the same originator tag, and resume/fork must
/// follow the newest matching rollout — not the first one we ever saw.
fn spawn_interactive_transcript_watcher(
    sid_file: PathBuf,
    expected_originator: String,
    session_env: HashMap<String, String>,
    emit: EventEmitter,
    skip_existing: bool,
    expected_uuid: Option<String>,
    expected_fork_parent: Option<String>,
    initial_model: Option<String>,
) {
    let Some(sessions_root) = codex_sessions_root(&session_env) else {
        emit.log("codex: no CODEX_HOME or HOME — cannot watch native transcript");
        return;
    };
    tokio::spawn(async move {
        let mut selected: Option<(String, PathBuf)> = None;
        let mut selected_mtime: Option<std::time::SystemTime> = None;
        let mut root_cursor = JsonlCursor::default();
        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
        let mut reported_usage = UsageTotals::default();
        // Report-on-change gate for the context breakdown (spec 0156).
        let mut breakdown_gate = BreakdownGate::default();
        let mut discovery = RolloutDiscovery::new(sessions_root);
        let mut pending_meta: HashMap<String, PathBuf> = HashMap::new();
        let mut known: HashMap<String, (PathBuf, SessionMeta)> = HashMap::new();
        let mut child_cursors: HashMap<String, JsonlCursor> = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        let mut child_states: HashMap<String, SessionState> = HashMap::new();
        let mut child_usage: HashMap<String, UsageTotals> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;

            for (name, path) in discovery.poll() {
                pending_meta.insert(name, path);
            }
            // A just-created rollout may not have its first complete JSONL
            // record yet. Keep only those unresolved paths in this retry set;
            // established rollouts are never reopened for metadata again.
            let resolved: Vec<(String, SessionMeta)> = pending_meta
                .iter()
                .filter_map(|(name, path)| read_session_meta(path).map(|meta| (name.clone(), meta)))
                .collect();
            for (name, meta) in resolved {
                if let Some(path) = pending_meta.remove(&name) {
                    known.insert(name, (path, meta));
                }
            }

            // Prefer the newest matching rollout. On first attach this
            // picks the live session; after /clear|/new a fresher file
            // appears with the same originator and we rebind.
            if let Some((name, path, uuid, mtime)) = find_best_matching_rollout(
                &known,
                &expected_originator,
                expected_uuid.as_deref(),
                expected_fork_parent.as_deref(),
            ) {
                let is_new = selected
                    .as_ref()
                    .map(|(cur, _)| cur != &name)
                    .unwrap_or(true);
                let is_newer = match (selected_mtime, mtime) {
                    (Some(prev), Some(cur)) => cur > prev,
                    (None, _) => true,
                    (Some(_), None) => is_new,
                };
                if is_new && (selected.is_none() || is_newer) {
                    let first_select = selected.is_none();
                    let existing_sid = std::fs::read_to_string(&sid_file)
                        .ok()
                        .map(|s| s.trim().to_string());
                    let should_write = existing_sid.as_deref() != Some(uuid.as_str());
                    if should_write {
                        if let Err(e) = std::fs::write(&sid_file, &uuid) {
                            emit.log(format!(
                                "codex: failed to write {}: {e}",
                                sid_file.display()
                            ));
                        } else {
                            emit.log(format!(
                                "codex: captured session id {uuid} (from {})",
                                path.display()
                            ));
                        }
                    }
                    // Skip existing lines only on the initial attach of a
                    // resumed session. Mid-session rebinds (after /clear)
                    // start at line 0 so chat mode sees the new conversation.
                    // Usage reporting follows the same split: a resumed
                    // session's historical usage is already in the daemon's
                    // tally, so baseline off the file's last snapshot; a
                    // fresh conversation starts from zero.
                    if first_select && skip_existing {
                        root_cursor = JsonlCursor::at_end(&path);
                        reported_usage = last_rollout_usage_totals(&path);
                    } else {
                        root_cursor = JsonlCursor::default();
                        reported_usage = UsageTotals::default();
                    };
                    if !first_select {
                        emit.log(format!(
                            "codex: native session id rebinding to {uuid} (from {})",
                            path.display()
                        ));
                        if let Some(prior) = existing_sid.filter(|s| !s.is_empty()) {
                            emit.emit(SessionEvent::NativeIdChanged {
                                prior_native_id: prior,
                                new_native_id: uuid.clone(),
                            });
                        }
                    }
                    selected = Some((name.clone(), path.clone()));
                    selected_mtime = mtime;
                }
            }

            let Some((_, path)) = selected.as_ref() else {
                continue;
            };
            let usage_refreshed = emit_new_codex_rollout_lines(
                path,
                &mut root_cursor,
                &emit,
                &mut last_model,
                &mut last_effort,
                &mut reported_usage,
            );
            // A fresh ContextUsage gauge means the window's contents moved;
            // refresh the breakdown behind it at the same cadence (spec
            // 0156). Always a full scan of the currently bound rollout —
            // turn cadence keeps that cheap, and it self-corrects across
            // resume and /clear rebinds.
            if usage_refreshed {
                emit_codex_context_breakdown(path, &mut breakdown_gate, &emit);
            }

            let Some(root_id) = selected
                .as_ref()
                .and_then(|(name, _)| known.get(name))
                .and_then(|(_, meta)| meta.id.clone())
            else {
                continue;
            };
            let mut related = HashSet::from([root_id.clone()]);
            loop {
                let before = related.len();
                for (_, meta) in known.values() {
                    if meta
                        .parent_thread_id
                        .as_ref()
                        .is_some_and(|parent| related.contains(parent))
                    {
                        if let Some(id) = meta.id.as_ref() {
                            related.insert(id.clone());
                        }
                    }
                }
                if related.len() == before {
                    break;
                }
            }
            for (child_path, meta) in known.values() {
                let (Some(child_id), Some(parent_id)) =
                    (meta.id.as_ref(), meta.parent_thread_id.as_ref())
                else {
                    continue;
                };
                if !related.contains(child_id) || !related.contains(parent_id) {
                    continue;
                }
                let first_seen = !child_cursors.contains_key(child_id);
                // Child files are ALWAYS read from the top — pre-existing
                // history backfills into the mirror instead of being skipped
                // on resume/restart. Every emission derived from the file
                // carries a deterministic per-child ordinal; the daemon
                // drops ordinals below the mirror's high-water mark, so
                // re-scans never duplicate.
                let cursor = child_cursors.entry(child_id.clone()).or_default();
                let ord = child_seq.entry(child_id.clone()).or_insert(0);
                let mut state = child_states
                    .get(child_id)
                    .copied()
                    .unwrap_or(SessionState::Running);
                if first_seen {
                    emit.emit(SessionEvent::NativeSubagent {
                        id: child_id.clone(),
                        parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                        title: Some(format!("Codex subagent {}", short_codex_id(child_id))),
                        state,
                        event: None,
                        seq: Some(next_native_seq(ord)),
                    });
                }
                for value in read_new_codex_values(child_path, cursor, &emit) {
                    if let Some(next_state) = codex_native_state(&value) {
                        state = next_state;
                        child_states.insert(child_id.clone(), state);
                    }
                    let events = codex_child_events(
                        &value,
                        child_usage.entry(child_id.clone()).or_default(),
                    );
                    if events.is_empty() && codex_native_state(&value).is_some() {
                        emit.emit(SessionEvent::NativeSubagent {
                            id: child_id.clone(),
                            parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                            title: None,
                            state,
                            event: None,
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                    for event in events {
                        let title = match &event {
                            SessionEvent::Message {
                                role: MessageRole::User,
                                text,
                            } => Some(short_title(text)),
                            _ => None,
                        };
                        emit.emit(SessionEvent::NativeSubagent {
                            id: child_id.clone(),
                            parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                            title,
                            state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                }
            }
        }
    });
}

/// Return the best cached rollout match for this construct session:
/// originator tag match, or (on resume) an exact uuid match. Among matches,
/// the newest mtime wins so /clear's fresh rollout supersedes the pre-clear
/// one. Metadata is populated once by [`RolloutDiscovery`]; matching must not
/// reopen every historical transcript on each watcher tick.
fn find_best_matching_rollout(
    known: &HashMap<String, (PathBuf, SessionMeta)>,
    expected_originator: &str,
    expected_uuid: Option<&str>,
    expected_fork_parent: Option<&str>,
) -> Option<(String, PathBuf, String, Option<std::time::SystemTime>)> {
    let mut best: Option<(String, PathBuf, String, Option<std::time::SystemTime>)> = None;
    for (name, (path, meta)) in known {
        let uuid = meta.id.clone().or_else(|| uuid_from_rollout_name(name));
        // A fork child COPIES its parent's originator into its meta
        // (`codex fork`), so an originator hit alone isn't ours when the
        // rollout says it was forked from somewhere — otherwise a fork of
        // this session would read as our own /clear rebind and steal the
        // parent's identity.
        let originator_matches = meta.originator.as_deref() == Some(expected_originator)
            && meta.parent_thread_id.is_none()
            && meta.forked_from_id.is_none();
        let uuid_matches = expected_uuid.is_some_and(|want| uuid.as_deref() == Some(want));
        // A session spawned as `codex fork <parent>` binds to the rollout
        // that names that parent — its own originator was copied from the
        // parent, so the tag can't identify it. (Two simultaneous forks of
        // one parent are ambiguous; newest wins, same as codex's own
        // `--last`.)
        let fork_matches = expected_fork_parent
            .is_some_and(|parent| meta.forked_from_id.as_deref() == Some(parent));
        if !originator_matches && !uuid_matches && !fork_matches {
            continue;
        }
        let Some(uuid) = uuid else {
            continue;
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let take = match best.as_ref().and_then(|(_, _, _, t)| *t) {
            Some(prev) => mtime.is_some_and(|cur| cur >= prev),
            None => true,
        };
        if take {
            best = Some((name.clone(), path.clone(), uuid, mtime));
        }
    }
    best
}

/// Byte cursor for one append-only JSONL rollout.
///
/// A line-count cursor still required reading and splitting the entire file
/// before skipping old records. This cursor seeks directly to the first
/// unseen byte, so an idle transcript costs one metadata check and no content
/// read regardless of its historical size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct JsonlCursor {
    offset: u64,
}

impl JsonlCursor {
    fn at_end(path: &Path) -> Self {
        Self {
            offset: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        }
    }
}

type ParsedJsonlRecord = (u64, Result<Value, serde_json::Error>);

/// Read complete records appended after `cursor.offset`.
///
/// Codex can expose a file between writing its first and last byte. An
/// unterminated invalid JSON fragment is therefore left unread until a later
/// tick instead of being logged and permanently skipped. A valid final record
/// is accepted even when the writer omitted its trailing newline.
fn read_new_jsonl_records(
    path: &Path,
    cursor: &mut JsonlCursor,
) -> std::io::Result<Vec<ParsedJsonlRecord>> {
    let len = std::fs::metadata(path)?.len();
    if len < cursor.offset {
        // Defensive handling for an in-place truncate/rewrite. Normal Codex
        // /clear creates a new path, but resetting here avoids a stuck cursor.
        cursor.offset = 0;
    }
    if len == cursor.offset {
        return Ok(Vec::new());
    }
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(cursor.offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut records = Vec::new();
    loop {
        let record_offset = cursor.offset;
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let terminated = line.ends_with('\n');
        if line.trim().is_empty() {
            cursor.offset += read as u64;
            continue;
        }
        let parsed = serde_json::from_str(&line);
        if !terminated && parsed.is_err() {
            break;
        }
        cursor.offset += read as u64;
        records.push((record_offset, parsed));
    }
    Ok(records)
}

/// Returns true when any of the new lines refreshed the context gauge — the
/// caller refreshes the context breakdown (spec 0156) at that same cadence.
fn emit_new_codex_rollout_lines(
    path: &Path,
    cursor: &mut JsonlCursor,
    emit: &EventEmitter,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
    reported_usage: &mut UsageTotals,
) -> bool {
    let Ok(records) = read_new_jsonl_records(path, cursor) else {
        return false;
    };
    let mut usage_refreshed = false;
    for (offset, record) in records {
        match record {
            Ok(v) => {
                usage_refreshed |=
                    emit_codex_rollout_event(emit, &v, last_model, last_effort, reported_usage);
            }
            Err(e) => emit.log(format!(
                "codex transcript: failed to parse {} at byte {}: {e}",
                path.display(),
                offset
            )),
        }
    }
    usage_refreshed
}

/// Returns true when the record emitted a fresh `ContextUsage` gauge.
fn emit_codex_rollout_event(
    emit: &EventEmitter,
    v: &Value,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
    reported_usage: &mut UsageTotals,
) -> bool {
    if let Some(model) = codex_model_change(v, last_model) {
        *last_model = Some(model.clone());
        emit.emit(SessionEvent::ModelChanged { model });
    }
    if let Some(effort) = codex_effort_change(v, last_effort) {
        *last_effort = Some(effort.clone());
        emit.emit(SessionEvent::EffortChanged { effort });
    }
    let usage_events = codex_usage_events(v, reported_usage, last_model.as_deref());
    let usage_refreshed = usage_events
        .iter()
        .any(|event| matches!(event, SessionEvent::ContextUsage { .. }));
    for event in usage_events {
        emit.emit(event);
    }
    for event in codex_rollout_events(v) {
        emit.emit(event);
    }
    usage_refreshed
}

/// Cumulative token totals already reported for the bound rollout, so each
/// `token_count` record contributes only its delta (spec 0103).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTotals {
    input: u64,
    output: u64,
    cached: u64,
}

/// Token usage from a rollout's `event_msg`/`token_count` record. Codex
/// stamps a cumulative `total_token_usage` (input includes cached; a
/// snapshot may repeat unchanged), so emitting the delta against what was
/// already reported both splits the stream into per-call Cost events and
/// makes duplicates harmless. This supersedes the headless footer parse
/// for interactive sessions — the footer only exists in `codex exec`
/// output, which has no rollout watcher, so the two never overlap.
///
/// A fresh delta also refreshes the context gauge (spec 0104): the same
/// record's `last_token_usage.input_tokens` is the prompt side of the most
/// recent call — exactly what filled the window — and codex states the
/// window itself in `model_context_window`. Gated on the delta so repeated
/// identical snapshots don't respam an unchanged gauge.
fn codex_usage_events(
    v: &Value,
    reported: &mut UsageTotals,
    model: Option<&str>,
) -> Vec<SessionEvent> {
    if v.get("type").and_then(Value::as_str) != Some("event_msg") {
        return Vec::new();
    }
    let Some(payload) = v.get("payload") else {
        return Vec::new();
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return Vec::new();
    }
    let Some(total) = rollout_usage_totals(payload) else {
        return Vec::new();
    };
    let d_in = total.input.saturating_sub(reported.input);
    let d_out = total.output.saturating_sub(reported.output);
    let d_cached = total.cached.saturating_sub(reported.cached);
    if d_in == 0 && d_out == 0 && d_cached == 0 {
        return Vec::new();
    }
    reported.input = reported.input.max(total.input);
    reported.output = reported.output.max(total.output);
    reported.cached = reported.cached.max(total.cached);
    let mut out = vec![SessionEvent::Cost {
        usd: 0.0,
        tokens_in: d_in,
        tokens_out: d_out,
        tokens_cached: d_cached,
        // A `token_count` record states no model; the rollout's own model
        // records do, and the caller tracks the latest — the same string
        // this session's `ModelChanged` carries (spec 0167).
        model: model.map(str::to_string),
    }];
    let last_input = payload
        .pointer("/info/last_token_usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if last_input > 0 {
        out.push(SessionEvent::ContextUsage {
            used_tokens: last_input,
            window_tokens: payload
                .pointer("/info/model_context_window")
                .and_then(Value::as_u64)
                .filter(|w| *w > 0),
        });
    }
    out
}

/// Events one child-rollout record contributes to its subagent mirror:
/// token-usage deltas first, then transcript events — the same order the
/// root watcher emits them. Child rollouts carry their own cumulative
/// `token_count` snapshots, so every mirror keeps its own `UsageTotals`
/// baseline; without this projection a subagent's tally never leaves zero
/// and its lineage lane is stuck on the message-count fallback (spec 0103).
fn codex_child_events(v: &Value, reported: &mut UsageTotals) -> Vec<SessionEvent> {
    // A child mirror tracks no model of its own here; the daemon knows the
    // subagent session's model, so leave attribution to the client rather
    // than stamping the parent's (spec 0167).
    let mut out = codex_usage_events(v, reported, None);
    out.extend(codex_rollout_events(v));
    out
}

/// The cumulative `total_token_usage` from a `token_count` payload.
fn rollout_usage_totals(payload: &Value) -> Option<UsageTotals> {
    let total = payload.pointer("/info/total_token_usage")?;
    let field = |k: &str| total.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(UsageTotals {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cached: field("cached_input_tokens"),
    })
}

/// The last cumulative usage snapshot already present in `path` — the
/// baseline for a resumed session's watcher. Without this, the first live
/// `token_count` after a resume would report the WHOLE conversation's
/// historical usage as one giant delta on top of the totals the daemon
/// already recounted from its own transcript.
fn last_rollout_usage_totals(path: &Path) -> UsageTotals {
    let Ok(text) = std::fs::read_to_string(path) else {
        return UsageTotals::default();
    };
    let mut latest = UsageTotals::default();
    for line in text.lines() {
        if !line.contains("token_count") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        if let Some(t) = v.get("payload").and_then(rollout_usage_totals) {
            latest = t;
        }
    }
    latest
}

/// Context breakdown behind the gauge (spec 0156): a full scan of the
/// currently-bound rollout, estimating what occupies the window. Codex's
/// rollout exposes the base instructions (`session_meta`), the whole
/// conversation (`response_item` records), and the harness-reported gauge
/// (`token_count` records), so up to three estimated segments are
/// derivable — `system prompt`, the differential `fixed overhead`
/// residual, then `messages`, fixed-prefix first.
/// Gated so identical reports aren't re-emitted (report on change, spec
/// 0104's rule applied to the breakdown).
fn emit_codex_context_breakdown(path: &Path, gate: &mut BreakdownGate, emit: &EventEmitter) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let segments = codex_breakdown_segments(&text);
    if gate.changed(&segments) {
        emit.emit(SessionEvent::ContextBreakdown { segments });
    }
}

/// Record shapes verified against real rollouts under `~/.codex/sessions/`
/// on this machine: `session_meta.payload.base_instructions` is
/// `{"text": "..."}`; conversation content lives in `response_item`
/// payloads; a `compacted` record's `payload.replacement_history` is the
/// list of response-item payloads that REPLACES the prior history, so the
/// conversation sum restarts from those items (base instructions are
/// re-sent every turn and survive compaction).
fn codex_breakdown_segments(text: &str) -> Vec<ContextSegment> {
    let mut system_chars: Option<usize> = None;
    let mut convo_chars = 0usize;
    // Differential fixed-overhead pin (spec 0156): lands on the current
    // epoch's first `token_count`, where the conversation estimate (and so
    // its error) is smallest. What it measures for codex is everything the
    // rollout can't itemize — tool schemas, per-turn formatting overhead.
    let mut pin = FixedOverheadPin::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                let base = v.pointer("/payload/base_instructions");
                system_chars = match base {
                    Some(Value::String(s)) => Some(s.len()),
                    Some(obj) => obj.get("text").and_then(Value::as_str).map(str::len),
                    None => None,
                }
                .or(system_chars);
            }
            Some("compacted") => {
                convo_chars = v
                    .pointer("/payload/replacement_history")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().map(codex_item_content_chars).sum())
                    .unwrap_or(0);
                pin.reset();
            }
            Some("response_item") => {
                if let Some(payload) = v.get("payload") {
                    convo_chars += codex_item_content_chars(payload);
                }
            }
            Some("event_msg") => {
                let last_input = v
                    .pointer("/payload/info/last_token_usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if v.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
                    && last_input > 0
                {
                    let estimated = estimate_tokens_from_chars(system_chars.unwrap_or(0))
                        .saturating_add(estimate_tokens_from_chars(convo_chars));
                    pin.observe(last_input, estimated);
                }
            }
            _ => {}
        }
    }
    let mut segments = Vec::new();
    if let Some(chars) = system_chars {
        segments.push(ContextSegment::new(
            "system prompt",
            estimate_tokens_from_chars(chars),
            true,
        ));
    }
    segments.extend(pin.segment());
    segments.push(ContextSegment::new(
        "messages",
        estimate_tokens_from_chars(convo_chars),
        true,
    ));
    segments
}

/// Content chars one response-item payload contributes to the window:
/// message text (user/assistant/developer alike — developer messages ride
/// in the same request), reasoning summaries, tool-call inputs, and
/// tool-call outputs. `reasoning.encrypted_content` is an opaque blob whose
/// char length says nothing about tokens, so it's skipped.
fn codex_item_content_chars(payload: &Value) -> usize {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => codex_blocks_text_chars(payload.get("content")),
        Some("reasoning") => codex_blocks_text_chars(payload.get("summary")),
        Some("function_call") => payload
            .get("arguments")
            .and_then(Value::as_str)
            .map_or(0, str::len),
        Some("custom_tool_call") => payload
            .get("input")
            .and_then(Value::as_str)
            .map_or(0, str::len),
        Some("function_call_output" | "custom_tool_call_output") => match payload.get("output") {
            Some(Value::String(s)) => s.len(),
            output @ Some(Value::Array(_)) => codex_blocks_text_chars(output),
            _ => 0,
        },
        _ => 0,
    }
}

/// Summed `.text` of a block array (`input_text`/`output_text`/summary
/// blocks all carry their content under `text`).
fn codex_blocks_text_chars(blocks: Option<&Value>) -> usize {
    blocks
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|b| b.get("text").and_then(Value::as_str).map_or(0, str::len))
                .sum()
        })
        .unwrap_or(0)
}

/// The root session's active model, if `v` is a `turn_context` rollout
/// record carrying `payload.model` that differs from what we last saw.
/// Scoped to `emit_codex_rollout_event` (root-only — the subagent mirroring
/// path calls `codex_rollout_events` directly, bypassing this) rather than
/// inside `codex_rollout_events` itself, which gates on `response_item` and
/// would never see a `turn_context` record anyway.
///
/// Verified against 1849 real codex rollouts on this machine: `turn_context`
/// repeats every turn (up to 737 times in one file) and 19 rollouts had more
/// than one distinct `payload.model` value within a single session,
/// confirming this field tracks a live model switch (unlike grok's
/// `model_id`, which turned out to be frozen per session).
fn codex_model_change(v: &Value, last_model: &Option<String>) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some("turn_context") {
        return None;
    }
    let model = v.pointer("/payload/model")?.as_str()?;
    (last_model.as_deref() != Some(model)).then(|| model.to_string())
}

/// Same signal as `codex_model_change`, for `payload.effort` (e.g.
/// `"high"`/`"medium"`/`"low"`) in the same `turn_context` record. Verified
/// against the same 1849 real rollouts: 14 had more than one distinct effort
/// value within a session, confirming it's live like `model`, not frozen
/// like grok's fields.
fn codex_effort_change(v: &Value, last_effort: &Option<String>) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some("turn_context") {
        return None;
    }
    let effort = v.pointer("/payload/effort")?.as_str()?;
    (last_effort.as_deref() != Some(effort)).then(|| effort.to_string())
}

fn codex_rollout_events(v: &Value) -> Vec<SessionEvent> {
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = v.get("payload") else {
        return Vec::new();
    };
    match payload.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message" => {
            let role = match payload.get("role").and_then(|r| r.as_str()) {
                Some("user") => MessageRole::User,
                _ => MessageRole::Assistant,
            };
            if let Some(text) = extract_text_from_blocks(payload.get("content")) {
                if !text.trim().is_empty() {
                    return vec![SessionEvent::Message { role, text }];
                }
            }
            Vec::new()
        }
        "function_call" => {
            let tool = payload
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let args = payload
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .or_else(|| payload.get("arguments").cloned())
                .unwrap_or(Value::Null);
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            vec![SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }]
        }
        "function_call_output" => {
            let tool = payload
                .get("call_id")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let output = match payload.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                None => String::new(),
            };
            // No tool name is available here; `tool` keeps the call_id and
            // `call_id` carries the explicit correlation key.
            let call_id = Some(tool.clone());
            vec![SessionEvent::ToolResult {
                tool,
                ok: true,
                output,
                call_id,
            }]
        }
        _ => Vec::new(),
    }
}

/// Incremental discovery index for Codex's date-partitioned rollout tree.
///
/// Appending to a rollout changes the file but not its parent directory.
/// Creating a new root or native-child rollout changes exactly one directory
/// mtime. Remembering directory mtimes lets the watcher stat the small set of
/// known directories and enumerate only those whose entries changed, instead
/// of walking thousands of historical files twice every 500 ms. A periodic
/// full enumeration is a backstop for filesystems with coarse or coalesced
/// directory timestamps.
struct RolloutDiscovery {
    root: PathBuf,
    directories: HashMap<PathBuf, Option<std::time::SystemTime>>,
    files: HashSet<PathBuf>,
    polls_since_full_scan: u16,
}

const ROLLOUT_FULL_SCAN_TICKS: u16 = 20;

impl RolloutDiscovery {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            directories: HashMap::new(),
            files: HashSet::new(),
            polls_since_full_scan: 0,
        }
    }

    fn poll(&mut self) -> Vec<(String, PathBuf)> {
        let mut discovered = Vec::new();
        if self.directories.is_empty() {
            let root = self.root.clone();
            self.scan_new_directory(root, &mut discovered);
            return discovered;
        }

        self.polls_since_full_scan += 1;
        let force_full_scan = self.polls_since_full_scan >= ROLLOUT_FULL_SCAN_TICKS;
        if force_full_scan {
            self.polls_since_full_scan = 0;
        }
        let directories: Vec<PathBuf> = self.directories.keys().cloned().collect();
        for directory in directories {
            let modified = directory_mtime(&directory);
            if !force_full_scan && self.directories.get(&directory) == Some(&modified) {
                continue;
            }
            // Record the timestamp observed before enumerating. If another
            // entry appears during read_dir, its later timestamp remains
            // different and guarantees a follow-up scan on the next tick.
            self.directories.insert(directory.clone(), modified);
            self.scan_directory_entries(&directory, &mut discovered);
        }
        discovered
    }

    fn scan_new_directory(&mut self, directory: PathBuf, discovered: &mut Vec<(String, PathBuf)>) {
        if self.directories.contains_key(&directory) {
            return;
        }
        self.directories
            .insert(directory.clone(), directory_mtime(&directory));
        self.scan_directory_entries(&directory, discovered);
    }

    fn scan_directory_entries(
        &mut self,
        directory: &Path,
        discovered: &mut Vec<(String, PathBuf)>,
    ) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                self.scan_new_directory(path, discovered);
                continue;
            }
            if !file_type.is_file() || !self.files.insert(path.clone()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                discovered.push((name.to_string(), path));
            }
        }
    }
}

fn directory_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Subset of fields we care about from codex's `session_meta` JSONL line.
#[derive(Default)]
struct SessionMeta {
    id: Option<String>,
    originator: Option<String>,
    parent_thread_id: Option<String>,
    /// `codex fork` stamps the source session's uuid here — the
    /// discriminator between "my own /clear rebind" and "a fork of me"
    /// (forks COPY the parent's originator into their meta).
    forked_from_id: Option<String>,
}

/// Read the rollout's first JSONL line and pull `payload.id` and
/// `payload.originator`. Returns `None` if the file is empty / mid-write
/// / not parseable — caller should re-check on a later tick.
fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    if first.is_empty() || !first.ends_with('\n') {
        return None;
    }
    let v: Value = serde_json::from_str(&first).ok()?;
    let payload = v.get("payload")?;
    Some(SessionMeta {
        id: payload.get("id").and_then(|s| s.as_str()).map(String::from),
        originator: payload
            .get("originator")
            .and_then(|s| s.as_str())
            .map(String::from),
        parent_thread_id: payload
            .get("parent_thread_id")
            .and_then(|s| s.as_str())
            .map(String::from),
        forked_from_id: payload
            .get("forked_from_id")
            .and_then(|s| s.as_str())
            .map(String::from),
    })
}

fn read_new_codex_values(path: &Path, cursor: &mut JsonlCursor, emit: &EventEmitter) -> Vec<Value> {
    let Ok(records) = read_new_jsonl_records(path, cursor) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for (offset, record) in records {
        match record {
            Ok(value) => values.push(value),
            Err(error) => emit.log(format!(
                "codex subagent transcript: failed to parse {} at byte {}: {error}",
                path.display(),
                offset
            )),
        }
    }
    values
}

fn codex_native_state(value: &Value) -> Option<SessionState> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    match value.pointer("/payload/type").and_then(Value::as_str) {
        Some("task_started") => Some(SessionState::Running),
        Some("task_complete") => Some(SessionState::Done),
        Some("task_failed") => Some(SessionState::Errored),
        _ => None,
    }
}

fn short_codex_id(id: &str) -> &str {
    id.get(..id.len().min(8)).unwrap_or(id)
}

fn short_title(text: &str) -> String {
    let title: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() <= 72 {
        title
    } else {
        format!("{}…", title.chars().take(71).collect::<String>())
    }
}

/// Extract the trailing UUID from a `rollout-<ts>-<uuid>.jsonl` filename.
/// Returns `None` if the trailing 36 chars don't look UUID-shaped.
fn uuid_from_rollout_name(name: &str) -> Option<String> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let uuid = &stem[stem.len() - 36..];
    // 8-4-4-4-12 hex digits
    let parts: Vec<&str> = uuid.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let lens = [8usize, 4, 4, 4, 12];
    for (p, want) in parts.iter().zip(lens.iter()) {
        if p.len() != *want || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(uuid.to_string())
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CODEX_CMD",
        "CONSTRUCT_CODEX_BIN",
        "codex",
    );
    let resume_flag = std::env::var("CONSTRUCT_CODEX_RESUME_FLAG").ok();
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut codex_session_id: Option<String> = None;
    let mut pending: VecDeque<String> = VecDeque::new();
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

        let mut child_args: Vec<String> = command_override.args.clone();
        child_args.push("exec".into());
        if let (Some(flag), Some(sid)) = (resume_flag.as_ref(), codex_session_id.as_ref()) {
            child_args.push(flag.clone());
            child_args.push(sid.clone());
        }
        if let Some(m) = &model {
            child_args.push("-m".into());
            child_args.push(m.clone());
        }
        for a in construct_protocol::adapter::maybe_inject_codex_mcp_args(&agentd_session_id) {
            child_args.push(a);
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }
        inject_model_catalog_arg(&mut child_args, &env);
        child_args.push(user_text.clone());
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

        let events_before_spawn = emit.events_emitted();
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
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));
        let stderr_tail = StderrTail::default();
        let stdout_task = spawn_stdout(child_stdout, emit.clone(), diagnostics.clone());
        let stderr_task = spawn_headless_stderr(
            child_stderr,
            emit.clone(),
            diagnostics.clone(),
            stderr_tail.clone(),
        );

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = stdout_task.await;
        let stderr_error = stderr_task.await.ok().flatten();
        let exit_status = child.wait().await.ok();

        let diagnostics = diagnostics.lock().unwrap().clone();

        if let Some(message) = stderr_error {
            emit.emit(SessionEvent::Error { message });
            break 1;
        }

        if matches!(outcome, TurnOutcome::Completed) {
            emit_launch_failure_if_silent(
                &emit,
                events_before_spawn,
                exit_status.as_ref(),
                &stderr_tail.snapshot(),
            );
        }

        // Always adopt the latest native id so a mid-run reset is honored
        // on subsequent turns (and written for daemon resume).
        if let Some(sid) = diagnostics.session_id.clone() {
            if codex_session_id.as_ref() != Some(&sid) {
                if let Ok(dir) = std::env::var("CONSTRUCT_SESSION_DATA_DIR") {
                    let p = PathBuf::from(dir).join("codex_session_id.txt");
                    let _ = std::fs::write(p, &sid);
                }
                codex_session_id = Some(sid);
            }
        }

        match outcome {
            TurnOutcome::Completed => {
                if let Some(reason) = diagnostics.blocked_write.as_deref() {
                    emit.emit(SessionEvent::Error {
                        message: blocked_write_error(reason),
                    });
                }
                continue;
            }
            TurnOutcome::Interrupted => {
                emit.log("turn interrupted; awaiting next input");
                continue;
            }
            TurnOutcome::Stopped => break 0,
        }
    };

    emit.emit(SessionEvent::Done { exit_code });
}

fn headless_error_message(line: &str) -> Option<String> {
    line.strip_prefix("ERROR:")
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned)
}

fn spawn_headless_stderr<R>(
    reader: R,
    emit: EventEmitter,
    diagnostics: Arc<StdMutex<HeadlessTurnDiagnostics>>,
    tail: StderrTail,
) -> tokio::task::JoinHandle<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut error = None;
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(message) = headless_error_message(&line) {
                error = Some(message);
            }
            if let Some(reason) = blocked_write_reason_from_log(&line) {
                let mut state = diagnostics.lock().unwrap();
                // Keep the first refusal: it is closest to the action that was
                // denied, and later ones may only repeat the same cause.
                if state.blocked_write.is_none() {
                    state.blocked_write = Some(reason);
                }
            }
            emit.log(format!("stderr: {line}"));
            tail.push(line);
        }
        error
    })
}

#[derive(Debug, Clone, Default)]
struct HeadlessTurnDiagnostics {
    session_id: Option<String>,
    blocked_write: Option<String>,
}

fn spawn_stdout<R>(
    reader: R,
    emit: EventEmitter,
    diagnostics: Arc<StdMutex<HeadlessTurnDiagnostics>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        // codex CLI prints a per-turn footer like:
        //   tokens used
        //   2,280
        // on two consecutive lines. Track whether we just saw the header.
        let mut expecting_token_count = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Stateful token-footer parse, BEFORE any emit, so the footer
            // never leaks to the transcript as assistant prose.
            if expecting_token_count {
                expecting_token_count = false;
                if let Some(n) = parse_token_count(&line) {
                    emit.emit(SessionEvent::Cost {
                        usd: 0.0,
                        // codex reports a single "total tokens used" per turn
                        // without splitting in/out. Stored in tokens_in as a
                        // conservative proxy (the prompt/context dominates).
                        tokens_in: n,
                        tokens_out: 0,
                        tokens_cached: 0,
                        // The PTY footer states no model, and this path
                        // tracks none — the client attributes it to the
                        // session's model (spec 0167).
                        model: None,
                    });
                    continue;
                }
                // Fall through if the line wasn't a number — treat it normally.
            }
            if line.trim().eq_ignore_ascii_case("tokens used") {
                expecting_token_count = true;
                continue;
            }
            // Best-effort JSON parse; if not JSON, emit as plain assistant text.
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    let mut g = diagnostics.lock().unwrap();
                    // Keep the most recently observed id (not only the first).
                    g.session_id = Some(sid.to_string());
                }
                if !try_emit_structured(&emit, &v) {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: line,
                    });
                }
            } else {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: line,
                });
            }
        }
    })
}

/// When the sandbox or the approval policy refuses an action, codex logs the
/// refusal itself on stderr, e.g.
///
/// ```text
/// 2026-08-04T12:57:08Z ERROR codex_core::tools::router: error=patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings
/// ```
///
/// The cause is worded differently per refusal, but the tracing envelope is
/// machine-emitted and stable, so anchor on the envelope and pass the cause
/// through verbatim. The agent's own prose about the refusal is not a usable
/// signal: it is free-form ("I cannot create `notes.txt` because...",
/// "Cannot: `$HOME` resolves to..."), and a turn that merely quotes a refusal
/// reads identically to one that suffered it.
///
/// Both halves of the match are deliberately conservative. codex colors this
/// line when its stderr is a terminal — headless spawns a pipe, so we receive
/// it plain, but the coloring is one flag away and invisible when it breaks, so
/// strip the escapes rather than depend on the stream shape. The cause must
/// then name *both* the refusal and the policy that refused: unrelated
/// `codex_core` errors borrow the same verbs ("request rejected by server", a
/// path that happens to contain `blocked/`) and reporting those as permissions
/// problems would send the user to the wrong knob. A refusal worded outside
/// this vocabulary is missed instead, which degrades to silence — the safe
/// direction for a signal read out of a tool's log.
fn blocked_write_reason_from_log(line: &str) -> Option<String> {
    let line = strip_ansi(line);
    let (envelope, cause) = line.split_once(": error=")?;
    if !envelope.contains(" ERROR ") || !envelope.contains("codex_core::") {
        return None;
    }
    let cause = cause.trim();
    let refused = ["rejected", "blocked", "denied"]
        .iter()
        .any(|word| cause.contains(word));
    let by_policy = ["sandbox", "approval"]
        .iter()
        .any(|word| cause.contains(word));
    if !refused || !by_policy {
        return None;
    }
    Some(short(cause, 600))
}

/// Drop ANSI escape sequences so a colored log line parses like a plain one.
fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI (`ESC [ params… final`) is the only form codex emits; consume it
        // through its final byte. Any other escape loses just its introducer.
        if chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if matches!(c, '\u{40}'..='\u{7e}') {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

/// Reported when a turn completes but codex was refused along the way. The
/// refusal is action-level, not turn-level: codex may have routed around it and
/// finished, so this names what was denied without claiming the turn failed.
fn blocked_write_error(reason: &str) -> String {
    format!(
        "Codex was refused an action by its sandbox or approval policy during this turn: {reason}. The turn may have completed anyway by working around the refusal. If that was not intended, configure Codex's headless permissions; Construct cannot resolve this approval interactively."
    )
}

/// Parse codex's "2,280" style total-tokens line. Strips commas/whitespace.
/// Returns None if the line isn't a pure integer (modulo separators).
fn parse_token_count(line: &str) -> Option<u64> {
    let cleaned: String = line
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != '_')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<u64>().ok()
}

/// Try to pull structured fields out of a codex JSON event. Returns `true` if
/// the value was recognized; otherwise the caller falls back to emitting raw.
fn try_emit_structured(emit: &EventEmitter, v: &Value) -> bool {
    let item = structured_item(v);
    let ty = match item.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    match ty {
        "message" | "assistant" => {
            if let Some((role, text)) = structured_message(item) {
                if !text.is_empty() {
                    emit.emit(SessionEvent::Message { role, text });
                }
                return true;
            }
            false
        }
        "tool_use" => {
            let name = item
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let args = item.get("input").cloned().unwrap_or(Value::Null);
            let call_id = item.get("id").and_then(|n| n.as_str()).map(str::to_string);
            emit.emit(SessionEvent::ToolUse {
                tool: name,
                args,
                call_id,
            });
            true
        }
        "tool_result" => {
            let tool = item
                .get("tool_use_id")
                .or_else(|| item.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !item
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let output = match item.get("output").or_else(|| item.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            // No tool name is available here; `tool` keeps the id and `call_id`
            // carries the explicit correlation key.
            let call_id = Some(tool.clone());
            emit.emit(SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            });
            true
        }
        _ => false,
    }
}

fn structured_item(v: &Value) -> &Value {
    if v.get("type").and_then(Value::as_str) == Some("response_item") {
        v.get("payload").unwrap_or(v)
    } else {
        v
    }
}

fn structured_message(v: &Value) -> Option<(MessageRole, String)> {
    let ty = v.get("type").and_then(Value::as_str)?;
    let role = match ty {
        "assistant" => MessageRole::Assistant,
        "message" => match v.get("role").and_then(Value::as_str) {
            None | Some("assistant") => MessageRole::Assistant,
            Some("user") => MessageRole::User,
            Some("system" | "developer") => MessageRole::System,
            Some("tool") => MessageRole::Tool,
            Some(_) => return None,
        },
        _ => return None,
    };
    let text = v
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| extract_text_from_blocks(v.get("content")))?;
    Some((role, text))
}

fn extract_text_from_blocks(v: Option<&Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let mut out = String::new();
    for block in arr {
        if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_test_rollout_metadata(root: &Path) -> HashMap<String, (PathBuf, SessionMeta)> {
        let mut discovery = RolloutDiscovery::new(root.to_path_buf());
        discovery
            .poll()
            .into_iter()
            .filter_map(|(name, path)| read_session_meta(&path).map(|meta| (name, (path, meta))))
            .collect()
    }

    #[test]
    fn parse_token_count_handles_codex_formats() {
        // Plain integer, comma-separated, underscore-separated, with whitespace.
        assert_eq!(parse_token_count("2280"), Some(2280));
        assert_eq!(parse_token_count("2,280"), Some(2280));
        assert_eq!(parse_token_count("  4,448 "), Some(4448));
        assert_eq!(parse_token_count("1_234_567"), Some(1234567));
        // Non-numbers reject.
        assert_eq!(parse_token_count("hello"), None);
        assert_eq!(parse_token_count(""), None);
        assert_eq!(parse_token_count("12abc"), None);
    }

    // Verbatim stderr from `codex exec --sandbox read-only -c
    // approval_policy='"never"'` asked to create a file (codex-cli 0.146.0).
    const REAL_REFUSAL_STDERR: &str = r#"Reading prompt from stdin...
OpenAI Codex v0.146.0
--------
workdir: /private/tmp/cxrA
approval: never
sandbox: read-only
session id: 019fccd9-4fcb-7213-afbb-f485134e0886
--------
user
Create a file named index.html in the current directory containing <h1>hi</h1>. If you cannot, say exactly why.
codex
I'll create `index.html` in the current directory with the exact requested content.
2026-08-04T12:57:08.883766Z ERROR codex_core::tools::router: error=patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings
codex
I cannot create `index.html` because the filesystem sandbox is read-only, and approval settings prohibit write access.
tokens used
5,690
"#;

    #[test]
    fn codex_refusal_log_lines_are_classified() {
        // Read-only sandbox, and writing outside the workspace: two real
        // refusals, worded differently by codex itself.
        assert_eq!(
            blocked_write_reason_from_log(
                "2026-08-04T12:57:08.883766Z ERROR codex_core::tools::router: error=patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings"
            )
            .as_deref(),
            Some("patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings")
        );
        assert_eq!(
            blocked_write_reason_from_log(
                "2026-08-04T12:59:11.867428Z ERROR codex_core::tools::router: error=patch rejected: writing outside of the project; rejected by user approval settings"
            )
            .as_deref(),
            Some("patch rejected: writing outside of the project; rejected by user approval settings")
        );
    }

    #[test]
    fn ansi_colored_refusal_logs_are_classified() {
        // Verbatim bytes from the same refusal captured with codex's stderr on
        // a terminal (`script -q`), where tracing colors the envelope and
        // splits both `ERROR` and `error=` with SGR sequences. Headless spawns
        // a pipe and gets the plain form, but the parse must not depend on it.
        // The trailing CR is the PTY's, and must not survive into the reason.
        const COLORED: &str = "\u{1b}[2m2026-08-04T13:29:34.381807Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m \u{1b}[2mcodex_core::tools::router\u{1b}[0m\u{1b}[2m:\u{1b}[0m \u{1b}[3merror\u{1b}[0m\u{1b}[2m=\u{1b}[0mpatch rejected: writing is blocked by read-only sandbox; rejected by user approval settings\r";
        assert_eq!(
            blocked_write_reason_from_log(COLORED).as_deref(),
            Some("patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings")
        );
    }

    #[test]
    fn unrelated_stderr_lines_are_not_classified_as_refusals() {
        // Session preamble.
        assert!(blocked_write_reason_from_log("sandbox: read-only").is_none());
        // The agent's own prose, which is echoed on stderr too. It describes a
        // real refusal here, but it is not a signal we can anchor on.
        assert!(blocked_write_reason_from_log(
            "I cannot create `index.html` because the filesystem sandbox is read-only, and approval settings prohibit write access."
        )
        .is_none());
        // A command that simply failed.
        assert!(blocked_write_reason_from_log(
            "mkdir: /private/tmp/cxrE/newdir: Operation not permitted"
        )
        .is_none());
        // A codex_core error that is not a refusal.
        assert!(blocked_write_reason_from_log(
            "2026-08-04T12:57:08.883766Z ERROR codex_core::client: error=stream disconnected before completion"
        )
        .is_none());
    }

    #[test]
    fn core_errors_that_merely_borrow_the_refusal_vocabulary_are_not_classified() {
        // Each of these would reach a user as "your sandbox or approval
        // policy refused this", sending them to a setting that is not the
        // problem. Refusal verbs alone are not enough.
        for line in [
            "2026-08-04T12:57:08.883766Z ERROR codex_core::client: error=request rejected by server: rate limit exceeded",
            "2026-08-04T12:57:08.883766Z ERROR codex_core::auth: error=token rejected: invalid credentials",
            "2026-08-04T12:57:08.883766Z ERROR codex_core::client: error=request blocked by content policy",
            "2026-08-04T12:57:08.883766Z ERROR codex_core::tools::router: error=failed to open /tmp/blocked/fixture.txt",
            // `unblocked` contains `blocked`, and this one is good news.
            "2026-08-04T12:57:08.883766Z ERROR codex_core::client: error=stream unblocked after retry, then failed",
        ] {
            assert!(
                blocked_write_reason_from_log(line).is_none(),
                "should not be classified as a refusal: {line}"
            );
        }
    }

    #[test]
    fn blocked_write_error_preserves_the_actionable_block_reason() {
        let message = blocked_write_error(
            "patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings",
        );
        assert!(message.contains("refused an action by its sandbox or approval policy"));
        assert!(message.contains("read-only sandbox"));
        assert!(message.contains("cannot resolve this approval interactively"));
        // The refusal is action-level: the turn itself may still have finished,
        // so the message must not assert that it failed.
        assert!(message.contains("may have completed anyway"));
    }

    #[tokio::test]
    async fn spawn_headless_stderr_records_a_real_refusal() {
        let (emit, _rx) = EventEmitter::channel("session");
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));

        spawn_headless_stderr(
            REAL_REFUSAL_STDERR.as_bytes(),
            emit,
            diagnostics.clone(),
            StderrTail::default(),
        )
            .await
            .expect("stderr task should finish");

        assert_eq!(
            diagnostics.lock().unwrap().blocked_write.as_deref(),
            Some("patch rejected: writing is blocked by read-only sandbox; rejected by user approval settings")
        );
    }

    #[tokio::test]
    async fn spawn_headless_stderr_ignores_a_command_that_merely_failed() {
        // Verbatim from a run whose command was denied by the sandbox but was
        // never refused by codex: it ran, and exited non-zero like any other
        // failing command. Nothing here is an approval problem the user
        // can fix by changing permissions.
        const OUTPUT: &str = "codex\nI'll run the exact command.\nexec\n/bin/zsh -lc 'mkdir /private/tmp/cxrE/newdir'\n exited 1 in 0ms:\nmkdir: /private/tmp/cxrE/newdir: Operation not permitted\n";
        let (emit, _rx) = EventEmitter::channel("session");
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));

        spawn_headless_stderr(
            OUTPUT.as_bytes(),
            emit,
            diagnostics.clone(),
            StderrTail::default(),
        )
            .await
            .expect("stderr task should finish");

        assert_eq!(diagnostics.lock().unwrap().blocked_write, None);
    }

    #[tokio::test]
    async fn spawn_stdout_does_not_classify_the_agents_own_prose() {
        // Real stdout carries only the final agent message. None of these are
        // a refusal signal — the middle one is a turn that succeeded and
        // quoted its command's output.
        const OUTPUT: &[u8] = b"I cannot create `index.html` because the filesystem sandbox is read-only.\n```text\nBlocked: synthetic tool output line\n```\nBlocked: the workspace is read-only\n";
        let (emit, _rx) = EventEmitter::channel("session");
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));

        spawn_stdout(OUTPUT, emit, diagnostics.clone())
            .await
            .expect("stdout task should finish");

        assert_eq!(diagnostics.lock().unwrap().blocked_write, None);
    }

    #[tokio::test]
    async fn spawn_stdout_keeps_a_final_message_that_is_only_the_word_codex() {
        // `codex` is a section marker, but real stdout carries only the final
        // agent message and no markers — so a lone `codex` there is the answer,
        // and dropping it would silently delete the whole turn's output.
        let (emit, mut rx) = EventEmitter::channel("session");
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));

        spawn_stdout(&b"codex\n"[..], emit, diagnostics)
            .await
            .expect("stdout task should finish");

        let events: Vec<Value> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|notification| notification.pointer("/params/event").cloned())
            .collect();
        assert!(events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("message")
                && event.get("text").and_then(Value::as_str) == Some("codex")
        }));
    }

    #[tokio::test]
    async fn spawn_stdout_unwraps_response_item_and_preserves_message_roles() {
        const OUTPUT: &[u8] = b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Blocked: prompt asks for a write\"}]}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Blocked: assistant could not write index.html\"}]}}\n";
        let (emit, mut rx) = EventEmitter::channel("session");
        let diagnostics = Arc::new(StdMutex::new(HeadlessTurnDiagnostics::default()));

        spawn_stdout(OUTPUT, emit, diagnostics.clone())
            .await
            .expect("stdout task should finish");

        let events: Vec<Value> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|notification| notification.pointer("/params/event").cloned())
            .collect();
        assert!(events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("message")
                && event.get("role").and_then(Value::as_str) == Some("user")
                && event.get("text").and_then(Value::as_str)
                    == Some("Blocked: prompt asks for a write")
        }));
        assert!(events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("message")
                && event.get("role").and_then(Value::as_str) == Some("assistant")
                && event.get("text").and_then(Value::as_str)
                    == Some("Blocked: assistant could not write index.html")
        }));
        assert!(events.iter().all(|event| {
            !event
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("response_item"))
        }));
    }

    #[test]
    fn token_count_records_emit_delta_costs_and_context_gauge() {
        // Real rollout shape: cumulative `total_token_usage` where `input_
        // tokens` already includes the cached reads, plus the most recent
        // call's own usage and the model window. Two snapshots → first
        // emits its full totals, second emits only the delta, a repeated
        // (unchanged) snapshot emits nothing — and each fresh delta rides
        // with a ContextUsage gauge from `last_token_usage` (spec 0104).
        let snapshot = |input: u64, cached: u64, output: u64, last_input: u64| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": input,
                            "cached_input_tokens": cached,
                            "output_tokens": output,
                            "reasoning_output_tokens": 3,
                            "total_tokens": input + output
                        },
                        "last_token_usage": { "input_tokens": last_input },
                        "model_context_window": 258_400
                    }
                }
            })
        };
        let mut reported = UsageTotals::default();
        match codex_usage_events(
            &snapshot(19_094, 9_984, 184, 19_094),
            &mut reported,
            Some("gpt-5.5-codex"),
        )
        .as_slice()
        {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            }] => {
                assert_eq!(*tokens_in, 19_094);
                assert_eq!(*tokens_out, 184);
                assert_eq!(*tokens_cached, 9_984);
                assert_eq!(*used_tokens, 19_094);
                assert_eq!(*window_tokens, Some(258_400));
            }
            other => panic!("expected Cost + ContextUsage: {other:?}"),
        }
        match codex_usage_events(&snapshot(48_890, 19_968, 257, 29_796), &mut reported, None)
            .as_slice()
        {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage { used_tokens, .. }] => {
                assert_eq!(*tokens_in, 29_796);
                assert_eq!(*tokens_out, 73);
                assert_eq!(*tokens_cached, 9_984);
                assert_eq!(*used_tokens, 29_796);
            }
            other => panic!("expected delta Cost + ContextUsage: {other:?}"),
        }
        assert!(
            codex_usage_events(&snapshot(48_890, 19_968, 257, 29_796), &mut reported, None)
                .is_empty(),
            "an unchanged snapshot must not re-report usage or respam the gauge"
        );
    }

    #[test]
    fn child_rollout_token_counts_reach_the_subagent_mirror() {
        // A subagent's rollout stamps the same cumulative `token_count`
        // records as the root's, against its own running total — the child
        // projection must turn them into per-call Cost deltas off the
        // child's own baseline (spec 0103), and still pass transcript
        // events through untouched.
        let snapshot = |input: u64, output: u64| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": input,
                            "cached_input_tokens": 0,
                            "output_tokens": output,
                        },
                        "last_token_usage": { "input_tokens": 0 },
                    }
                }
            })
        };
        let mut reported = UsageTotals::default();
        match codex_child_events(&snapshot(1_000, 50), &mut reported).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                ..
            }] => {
                assert_eq!(*tokens_in, 1_000);
                assert_eq!(*tokens_out, 50);
            }
            other => panic!("expected the child's Cost delta: {other:?}"),
        }
        match codex_child_events(&snapshot(1_600, 80), &mut reported).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                ..
            }] => {
                assert_eq!(*tokens_in, 600);
                assert_eq!(*tokens_out, 30);
            }
            other => panic!("expected only the delta on the second snapshot: {other:?}"),
        }
        let message = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "done" }]
            }
        });
        let events = codex_child_events(&message, &mut reported);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Message { .. })),
            "transcript events must still project: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Cost { .. })),
            "a non-usage record must not fabricate a Cost: {events:?}"
        );
    }

    #[test]
    fn uuid_from_rollout_name_parses_real_codex_filename() {
        let name = "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl";
        assert_eq!(
            uuid_from_rollout_name(name).as_deref(),
            Some("019e32aa-014a-7ff0-9a3f-7ae773961a37"),
        );
    }

    #[test]
    fn headless_codex_errors_are_promoted_out_of_stderr() {
        assert_eq!(
            headless_error_message(r#"ERROR: {"detail":"The 'sonnet' model is not supported."}"#)
                .as_deref(),
            Some(r#"{"detail":"The 'sonnet' model is not supported."}"#)
        );
        assert_eq!(headless_error_message("warning: retrying"), None);
        assert_eq!(headless_error_message("ERROR:   "), None);
    }

    #[test]
    fn uuid_from_rollout_name_rejects_garbage() {
        assert!(uuid_from_rollout_name("rollout-foo.jsonl").is_none());
        assert!(uuid_from_rollout_name("not-a-rollout.jsonl").is_none());
        assert!(uuid_from_rollout_name(
            "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.txt"
        )
        .is_none());
        // Right length, non-hex characters.
        assert!(
            uuid_from_rollout_name("rollout-zzz-zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz.jsonl")
                .is_none()
        );
    }

    #[test]
    fn read_session_meta_extracts_id_and_originator() {
        let tmp =
            std::env::temp_dir().join(format!("agentd-codex-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mine = tmp.join("rollout-mine.jsonl");
        std::fs::write(
            &mine,
            "{\"timestamp\":\"x\",\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"019e32aa-014a-7ff0-9a3f-7ae773961a37\",\
             \"cwd\":\"/work/me\",\"originator\":\"agentd:sess-abc\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&mine).unwrap();
        assert_eq!(
            meta.id.as_deref(),
            Some("019e32aa-014a-7ff0-9a3f-7ae773961a37")
        );
        assert_eq!(meta.originator.as_deref(), Some("agentd:sess-abc"));
        assert_eq!(meta.parent_thread_id, None);

        // Metadata parsing reads only the first record. Invalid bytes deep in
        // a large transcript must not force a whole-file UTF-8 decode or make
        // the otherwise valid session metadata disappear.
        {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&mine)
                .unwrap()
                .write_all(&[0xff])
                .unwrap();
        }
        assert_eq!(
            read_session_meta(&mine).unwrap().id.as_deref(),
            Some("019e32aa-014a-7ff0-9a3f-7ae773961a37")
        );

        // Default codex originator stays distinct.
        let other = tmp.join("rollout-other.jsonl");
        std::fs::write(
            &other,
            "{\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"u\",\"originator\":\"codex-tui\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&other).unwrap();
        assert_eq!(meta.originator.as_deref(), Some("codex-tui"));

        // Empty / mid-write file: caller will re-check later.
        let blank = tmp.join("rollout-blank.jsonl");
        std::fs::write(&blank, "").unwrap();
        assert!(read_session_meta(&blank).is_none());

        let partial = tmp.join("rollout-partial.jsonl");
        std::fs::write(
            &partial,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"partial\"}}",
        )
        .unwrap();
        assert!(read_session_meta(&partial).is_none());
        {
            use std::io::Write as _;
            writeln!(std::fs::OpenOptions::new()
                .append(true)
                .open(&partial)
                .unwrap())
            .unwrap();
        }
        assert_eq!(
            read_session_meta(&partial).unwrap().id.as_deref(),
            Some("partial")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn jsonl_cursor_reads_only_appended_complete_records() {
        let tmp = std::env::temp_dir().join(format!(
            "construct-codex-jsonl-cursor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, "{\"n\":1}\n{\"n\":2}\n").unwrap();

        let mut cursor = JsonlCursor::default();
        let first = read_new_jsonl_records(&tmp, &mut cursor).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].1.as_ref().unwrap()["n"], 1);
        assert_eq!(first[1].1.as_ref().unwrap()["n"], 2);
        let offset_after_first = cursor.offset;
        assert!(read_new_jsonl_records(&tmp, &mut cursor)
            .unwrap()
            .is_empty());
        assert_eq!(cursor.offset, offset_after_first);

        {
            use std::io::Write as _;
            write!(
                std::fs::OpenOptions::new().append(true).open(&tmp).unwrap(),
                "{{\"n\":3"
            )
            .unwrap();
        }
        assert!(read_new_jsonl_records(&tmp, &mut cursor)
            .unwrap()
            .is_empty());
        assert_eq!(cursor.offset, offset_after_first);

        {
            use std::io::Write as _;
            write!(
                std::fs::OpenOptions::new().append(true).open(&tmp).unwrap(),
                "}}\n{{\"n\":4}}\n"
            )
            .unwrap();
        }
        let appended = read_new_jsonl_records(&tmp, &mut cursor).unwrap();
        assert_eq!(appended.len(), 2);
        assert_eq!(appended[0].1.as_ref().unwrap()["n"], 3);
        assert_eq!(appended[1].1.as_ref().unwrap()["n"], 4);

        std::fs::write(&tmp, "{\"n\":5}\n").unwrap();
        let rewritten = read_new_jsonl_records(&tmp, &mut cursor).unwrap();
        assert_eq!(rewritten.len(), 1);
        assert_eq!(rewritten[0].1.as_ref().unwrap()["n"], 5);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rollout_discovery_enumerates_only_new_directory_entries() {
        let tmp = std::env::temp_dir().join(format!(
            "construct-codex-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let day = tmp.join("2026/08/03");
        std::fs::create_dir_all(&day).unwrap();
        let first = day.join("rollout-first.jsonl");
        std::fs::write(&first, "{}\n").unwrap();

        let mut discovery = RolloutDiscovery::new(tmp.clone());
        assert_eq!(discovery.poll().len(), 1);
        assert!(discovery.poll().is_empty());

        {
            use std::io::Write as _;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&first)
                    .unwrap(),
                "{{}}"
            )
            .unwrap();
        }
        assert!(discovery.poll().is_empty());

        let second = day.join("rollout-second.jsonl");
        std::fs::write(&second, "{}\n").unwrap();
        let found = discovery.poll();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "rollout-second.jsonl");

        let next_day = tmp.join("2026/08/04");
        std::fs::create_dir_all(&next_day).unwrap();
        std::fs::write(next_day.join("rollout-third.jsonl"), "{}\n").unwrap();
        let found = discovery.poll();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "rollout-third.jsonl");

        // The periodic backstop finds an entry even when a filesystem reports
        // the same/coalesced directory timestamp across its creation.
        let hidden = next_day.join("rollout-hidden.jsonl");
        std::fs::write(&hidden, "{}\n").unwrap();
        let current_mtime = directory_mtime(&next_day);
        discovery
            .directories
            .insert(next_day.clone(), current_mtime);
        discovery.polls_since_full_scan = ROLLOUT_FULL_SCAN_TICKS - 1;
        let found = discovery.poll();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "rollout-hidden.jsonl");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_best_matching_rollout_prefers_newest_after_clear() {
        // Simulate /clear: two rollouts share our originator; the newer
        // mtime (post-clear) must win so resume/fork follow the active id.
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-best-match-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let old_name = "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl";
        let new_name = "rollout-2026-05-16T15-00-00-019e32bb-014a-7ff0-9a3f-7ae773961a99.jsonl";
        let old_path = tmp.join(old_name);
        let new_path = tmp.join(new_name);
        let originator = "agentd:sess-clear-test";
        let meta = |id: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{id}\",\"originator\":\"{originator}\"}}}}\n"
            )
        };
        std::fs::write(&old_path, meta("019e32aa-014a-7ff0-9a3f-7ae773961a37")).unwrap();
        // Ensure distinct mtimes so "newest" is well-defined.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new_path, meta("019e32bb-014a-7ff0-9a3f-7ae773961a99")).unwrap();

        // Unrelated originator must be ignored.
        std::fs::write(
            tmp.join("rollout-2026-05-16T16-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\",\
             \"originator\":\"codex-tui\"}}\n",
        )
        .unwrap();

        let known = load_test_rollout_metadata(&tmp);
        let best = find_best_matching_rollout(&known, originator, None, None)
            .expect("should find a match");
        assert_eq!(best.0, new_name);
        assert_eq!(best.2, "019e32bb-014a-7ff0-9a3f-7ae773961a99");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fork_children_never_steal_the_parents_originator_match() {
        // `codex fork` COPIES the parent's meta — originator included — and
        // stamps `forked_from_id`. The parent's watcher must not treat the
        // fork's (newer) rollout as its own /clear rebind, and the fork's
        // watcher finds its rollout via the parent linkage instead.
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-fork-match-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let originator = "agentd:parent-sess";
        let parent_uuid = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        let fork_uuid = "019e32bb-014a-7ff0-9a3f-7ae773961a99";
        std::fs::write(
            tmp.join("rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{parent_uuid}\",\"originator\":\"{originator}\"}}}}\n"
            ),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The fork's rollout: newer, same originator, forked_from_id set.
        std::fs::write(
            tmp.join("rollout-2026-05-16T15-00-00-019e32bb-014a-7ff0-9a3f-7ae773961a99.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{fork_uuid}\",\"originator\":\"{originator}\",\
                 \"forked_from_id\":\"{parent_uuid}\"}}}}\n"
            ),
        )
        .unwrap();

        // Parent's view: the fork rollout is newer but must NOT win.
        let known = load_test_rollout_metadata(&tmp);
        let best = find_best_matching_rollout(&known, originator, None, None)
            .expect("parent still matches its own rollout");
        assert_eq!(best.2, parent_uuid);

        // Fork's view: its originator tag was copied from the parent, so it
        // identifies its rollout by the fork linkage.
        let best = find_best_matching_rollout(&known, "agentd:fork-sess", None, Some(parent_uuid))
            .expect("fork matches via forked_from_id");
        assert_eq!(best.2, fork_uuid);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_session_meta_extracts_native_parent() {
        let tmp = std::env::temp_dir().join(format!(
            "construct-codex-native-parent-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let rollout = tmp.join("rollout-child.jsonl");
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"parent_thread_id\":\"parent\",\"thread_source\":\"subagent\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&rollout).unwrap();
        assert_eq!(meta.id.as_deref(), Some("child"));
        assert_eq!(meta.parent_thread_id.as_deref(), Some("parent"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn native_state_uses_child_task_lifecycle() {
        assert_eq!(
            codex_native_state(&serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started"}
            })),
            Some(SessionState::Running)
        );
        assert_eq!(
            codex_native_state(&serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            })),
            Some(SessionState::Done)
        );
    }

    #[test]
    fn rollout_message_records_become_chat_messages() {
        let user = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            }
        });
        let assistant = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "output_text", "text": "one" },
                    { "type": "output_text", "text": "two" }
                ]
            }
        });

        match codex_rollout_events(&user).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::User));
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected user events: {other:?}"),
        }
        match codex_rollout_events(&assistant).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "one\ntwo");
            }
            other => panic!("unexpected assistant events: {other:?}"),
        }
    }

    #[test]
    fn rollout_function_records_become_tool_events() {
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo test\"}",
                "call_id": "call_1"
            }
        });
        let output = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }
        });

        match codex_rollout_events(&call).as_slice() {
            [SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }] => {
                assert_eq!(tool, "exec_command");
                assert_eq!(args["cmd"], "cargo test");
                assert_eq!(call_id.as_deref(), Some("call_1"));
            }
            other => panic!("unexpected tool-use events: {other:?}"),
        }
        match codex_rollout_events(&output).as_slice() {
            [SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            }] => {
                assert_eq!(tool, "call_1");
                assert!(*ok);
                assert_eq!(output, "ok");
                assert_eq!(call_id.as_deref(), Some("call_1"));
            }
            other => panic!("unexpected tool-result events: {other:?}"),
        }
    }

    #[test]
    fn model_change_ignored_for_non_turn_context_records() {
        let v = serde_json::json!({"type": "response_item", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(codex_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_ignored_when_payload_model_absent() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(codex_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_fires_on_first_observation() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(
            codex_model_change(&v, &None).as_deref(),
            Some("gpt-5.6-terra")
        );
    }

    #[test]
    fn model_change_silent_when_unchanged() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(
            codex_model_change(&v, &Some("gpt-5.6-terra".to_string())),
            None
        );
    }

    #[test]
    fn model_change_fires_on_switch() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.3-codex-spark"}});
        assert_eq!(
            codex_model_change(&v, &Some("gpt-5.6-terra".to_string())).as_deref(),
            Some("gpt-5.3-codex-spark")
        );
    }

    #[test]
    fn effort_change_ignored_for_non_turn_context_records() {
        let v = serde_json::json!({"type": "response_item", "payload": {"effort": "high"}});
        assert_eq!(codex_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_ignored_when_payload_effort_absent() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(codex_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_fires_on_first_observation() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(codex_effort_change(&v, &None).as_deref(), Some("medium"));
    }

    #[test]
    fn effort_change_silent_when_unchanged() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(codex_effort_change(&v, &Some("medium".to_string())), None);
    }

    #[test]
    fn effort_change_fires_on_switch() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "high"}});
        assert_eq!(
            codex_effort_change(&v, &Some("medium".to_string())).as_deref(),
            Some("high")
        );
    }

    // Record shapes below mirror real rollouts inspected under
    // `~/.codex/sessions/` on this machine: `session_meta` carries
    // `base_instructions: {"text": ...}`; `response_item` payloads are
    // `message` (content blocks of `input_text`/`output_text`),
    // `reasoning` (summary blocks + opaque `encrypted_content`),
    // `function_call`/`custom_tool_call` (string `arguments`/`input`), and
    // their `*_output` twins (`output` either a string or a block list).
    #[test]
    fn context_breakdown_sums_system_prompt_and_conversation_chars() {
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"019f-1","originator":"agentd:s1","base_instructions":{"text":"You are Codex."}}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"model":"gpt-5.2-codex","effort":"medium"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi there"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"gAAAAABqVtwq"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"call_1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"done"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"text(1);","call_id":"call_2"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_2","output":[{"type":"input_text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"output_tokens":5,"cached_input_tokens":0}}}}"#,
            "\n",
        );
        // system: "You are Codex." (14). conversation: "hi there" (8) +
        // "plan" (4) + arguments (12) + "done" (4) + input (8) + "ok" (2)
        // = 38; encrypted_content contributes nothing.
        assert_eq!(
            codex_breakdown_segments(rollout),
            vec![
                ContextSegment::new("system prompt", estimate_tokens_from_chars(14), true),
                ContextSegment::new("messages", estimate_tokens_from_chars(38), true),
            ]
        );
    }

    #[test]
    fn context_breakdown_omits_system_prompt_when_meta_lacks_instructions() {
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"019f-1","originator":"agentd:s1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            "\n",
        );
        // Never guess a segment the data surface doesn't carry (spec 0156).
        assert_eq!(
            codex_breakdown_segments(rollout),
            vec![ContextSegment::new(
                "messages",
                estimate_tokens_from_chars(5),
                true
            )]
        );
    }

    #[test]
    fn context_breakdown_restarts_conversation_at_compacted_record() {
        // Real `compacted` records carry `replacement_history`: the
        // response-item payloads that REPLACE everything before them.
        let rollout = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"0123456789"}]}}"#,
            "\n",
            r#"{"type":"compacted","payload":{"message":"","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"sum"}]}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ab"}]}}"#,
            "\n",
        );
        assert_eq!(
            codex_breakdown_segments(rollout),
            vec![ContextSegment::new(
                "messages",
                estimate_tokens_from_chars(5),
                true
            )]
        );
    }

    #[test]
    fn context_breakdown_pins_fixed_overhead_on_epoch_first_token_count() {
        use construct_adapter_common::context_breakdown::FIXED_OVERHEAD_LABEL;
        // At the first token_count: system 14 chars (~4 tokens) + convo
        // 8 chars (~2 tokens) against a 500-token prompt side → 494 pinned.
        // The later 900-token snapshot must not move the pin.
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"019f-1","base_instructions":{"text":"You are Codex."}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi there"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"output_tokens":5,"cached_input_tokens":0},"last_token_usage":{"input_tokens":500}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"output_tokens":9,"cached_input_tokens":0},"last_token_usage":{"input_tokens":900}}}}"#,
            "\n",
        );
        let segments = codex_breakdown_segments(rollout);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].label, "system prompt");
        assert_eq!(
            segments[1],
            ContextSegment::new(FIXED_OVERHEAD_LABEL, 494, true)
        );
        assert_eq!(segments[2].label, "messages");

        // Compaction starts a new epoch: replacement "sum" (3 chars, ~0
        // tokens) + system (~4) against the next 600-token prompt → 596.
        let compacted = format!(
            "{rollout}{}",
            concat!(
                r#"{"type":"compacted","payload":{"message":"","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"sum"}]}]}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"output_tokens":9,"cached_input_tokens":0},"last_token_usage":{"input_tokens":600}}}}"#,
                "\n",
            )
        );
        let segments = codex_breakdown_segments(&compacted);
        assert_eq!(
            segments[1],
            ContextSegment::new(FIXED_OVERHEAD_LABEL, 596, true)
        );
    }

    #[test]
    fn context_breakdown_gate_suppresses_identical_reports() {
        let rollout = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;
        let mut gate = BreakdownGate::default();
        assert!(gate.changed(&codex_breakdown_segments(rollout)));
        // Re-scanning an unchanged rollout must not re-emit.
        assert!(!gate.changed(&codex_breakdown_segments(rollout)));
        let grown = format!(
            "{rollout}\n{}",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a much longer reply that moves the estimate"}]}}"#
        );
        assert!(gate.changed(&codex_breakdown_segments(&grown)));
    }

    #[test]
    fn model_catalog_uses_an_https_only_session_provider() {
        let mut args = vec!["exec".to_string()];
        let env = std::collections::HashMap::from([(
            construct_protocol::adapter::ENV_CODEX_MODEL_CATALOG.to_string(),
            "/tmp/construct catalog.json".to_string(),
        )]);
        inject_model_catalog_arg(&mut args, &env);
        assert_eq!(
            args,
            vec![
                "exec",
                "-c",
                "model_catalog_json=\"/tmp/construct catalog.json\"",
                "-c",
                "model_provider=\"construct_router\"",
                "-c",
                "model_providers.construct_router={name=\"Construct router\",wire_api=\"responses\",requires_openai_auth=true,supports_websockets=false}"
            ]
        );
    }
}
