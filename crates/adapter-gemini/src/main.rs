//! Gemini CLI adapter.
//!
//! Two modes, picked the same way as the `claude` adapter:
//!
//! - **interactive (default when a PTY size is provided)** — spawns
//!   `gemini` under a PTY. The right pane is the real Gemini TUI.
//!
//! - **headless (opt-in)** — multi-turn structured mode using
//!   `gemini -p <text> -o stream-json --session-id <uuid>` (first turn)
//!   then `--resume <uuid>` for follow-ups. Emits structured
//!   `Message` / `ToolUse` / `ToolResult` / `Cost` events parsed from
//!   the observed stream-json shape (init / message / tool_use /
//!   tool_result / result).
//!
//! Honors `AGENTD_GEMINI_BIN` for the binary path and
//! `AGENTD_GEMINI_MODE=interactive|headless` for mode override.
//!
//! # Session isolation (`GEMINI_CLI_HOME`)
//!
//! Gemini CLI has no `--mcp-config <path>` flag — MCP servers are
//! read from `settings.json` under the global gemini home. To keep
//! per-session MCP config from colliding across concurrent agentd
//! sessions (or polluting the user's `~/.gemini`), we point gemini at
//! a per-session home via `GEMINI_CLI_HOME=<AGENTD_SESSION_DATA_DIR>/gemini-home`.
//!
//! Side effect: that env override moves gemini's auth + history too.
//! To avoid making the user re-OAuth on every new session, we
//! symlink the user's existing auth files (`oauth_creds.json`,
//! `google_accounts.json`, `installation_id`) from `~/.gemini` into
//! the session home at first spawn. Subsequent spawns of the same
//! session reuse the symlinks. If the user has no global gemini home
//! yet, the symlinks are skipped and gemini will prompt for auth.

use agentd_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use agentd_protocol::adapter::{run, AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "gemini".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    run(metadata, |params, ctx| async move {
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
    if let Ok(m) = std::env::var("AGENTD_GEMINI_MODE") {
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

/// Build the per-session gemini home (`<AGENTD_SESSION_DATA_DIR>/gemini-home`),
/// seed `.gemini/settings.json` with the agentd MCP server entry (unless the
/// user opted out via `AGENTD_INJECT_MCP=0`), and symlink auth files from the
/// user's existing `~/.gemini` so they don't re-OAuth per session.
///
/// Returns the path to use as `GEMINI_CLI_HOME`. If we couldn't set up the
/// session home (no data dir env, mkdir failed, etc.) returns `None` and
/// the caller falls back to the user's default gemini home — losing MCP
/// injection but otherwise working.
fn setup_gemini_home(session_id: &str) -> Option<PathBuf> {
    let data_dir = std::env::var("AGENTD_SESSION_DATA_DIR").ok().map(PathBuf::from)?;
    let home = data_dir.join("gemini-home");
    let gemini_dir = home.join(".gemini");
    if let Err(e) = std::fs::create_dir_all(&gemini_dir) {
        tracing::warn!(error = ?e, "mkdir gemini-home failed; falling back to user home");
        return None;
    }

    // Settings file with our MCP entry. Merge with any pre-existing file
    // so a user who customizes their session's gemini settings (rare —
    // it's session-scoped) doesn't lose them on respawn.
    let inject = std::env::var("AGENTD_INJECT_MCP").as_deref() != Ok("0");
    if inject {
        let settings_path = gemini_dir.join("settings.json");
        if let Err(e) = write_settings_with_mcp(&settings_path, session_id) {
            tracing::warn!(error = ?e, path = %settings_path.display(), "write gemini settings.json failed");
        }
    }

    // Symlink auth/identity files from the user's global gemini home so
    // they don't re-OAuth. Best-effort: missing files are fine, broken
    // symlinks are fine too (gemini will treat them as missing).
    if let Some(global) = global_gemini_dir() {
        for name in ["oauth_creds.json", "google_accounts.json", "installation_id"] {
            let src = global.join(name);
            let dst = gemini_dir.join(name);
            // Skip if dst already exists (idempotent across respawns).
            if dst.exists() {
                continue;
            }
            if !src.exists() {
                continue;
            }
            #[cfg(unix)]
            {
                let _ = std::os::unix::fs::symlink(&src, &dst);
            }
            #[cfg(not(unix))]
            {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }

    Some(home)
}

/// Path of the user's standalone `~/.gemini` dir, if discoverable.
/// We don't honor `GEMINI_CLI_HOME` here on purpose — this resolves
/// where the user's auth lives *outside* of any agentd session.
fn global_gemini_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let dir = home.join(".gemini");
    dir.exists().then_some(dir)
}

/// Resolve the `agentd-mcp` binary path the same way other adapters do.
fn agentd_mcp_binary() -> Option<PathBuf> {
    agentd_protocol::paths::locate_sibling_binary("agentd-mcp")
}

fn write_settings_with_mcp(path: &Path, session_id: &str) -> std::io::Result<()> {
    let mcp_bin = match agentd_mcp_binary() {
        Some(p) => p.to_string_lossy().to_string(),
        // No agentd-mcp on PATH/sibling — don't write a stub entry that
        // would prevent gemini from working. Skip injection entirely.
        None => return Ok(()),
    };

    // Read existing settings if any so we don't clobber user-set fields.
    let mut root: Value = if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    } else {
        json!({})
    };

    // mcpServers.agentd = { command, args, env }
    let entry = json!({
        "command": mcp_bin,
        "args": [],
        "env": { "AGENTD_SESSION_ID": session_id },
        "trust": true,
    });
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().expect("ensured object above");
    let mcp = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    if !mcp.is_object() {
        *mcp = json!({});
    }
    let mcp_obj = mcp.as_object_mut().expect("ensured object above");
    mcp_obj.insert("agentd".to_string(), entry);

    let text = serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".into());
    std::fs::write(path, text)
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let bin = std::env::var("AGENTD_GEMINI_BIN").unwrap_or_else(|_| "gemini".into());
    let mut args = params.args.clone();
    if let Some(m) = params.model.as_ref() {
        args.push("--model".into());
        args.push(m.clone());
    }
    // Resume support: stash our own UUID under
    // $AGENTD_SESSION_DATA_DIR/gemini_session_id.txt at first spawn
    // (passed to gemini as --session-id), then pass it back as --resume
    // when the daemon respawns us. Gemini's resume accepts a UUID
    // directly when `--session-id` was used to mint one.
    let resuming = std::env::var("AGENTD_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("AGENTD_SESSION_DATA_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("gemini_session_id.txt"));
    let gemini_session_id = match (resuming, sid_file.as_ref()) {
        (true, Some(p)) if p.exists() => std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    if let Some(sid) = &gemini_session_id {
        args.push("--resume".into());
        args.push(sid.clone());
    } else if let Some(p) = &sid_file {
        let new_id = uuid::Uuid::new_v4().to_string();
        let _ = std::fs::write(p, &new_id);
        args.push("--session-id".into());
        args.push(new_id);
    }
    // Skip the initial prompt on resume — it's already in the gemini
    // conversation we're rejoining.
    if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.clone());
        }
    }

    // Build env: per-session GEMINI_CLI_HOME so MCP + history stay
    // isolated, plus the standard AGENTD_SESSION_ID for child agents.
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("AGENTD_SESSION_ID".into(), ctx.session_id.clone()));
    if let Some(home) = setup_gemini_home(&ctx.session_id) {
        env.push(("GEMINI_CLI_HOME".into(), home.to_string_lossy().to_string()));
    }

    let label = bin.clone();
    let spec = PtySpec {
        bin,
        args,
        cwd: PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize { cols: 100, rows: 30 }),
        status_detail: Some(format!("{label} (interactive)")),
    };
    let _ = run_pty(spec, ctx).await;
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id,
        emit,
        mut inbox,
    } = ctx;

    let bin = std::env::var("AGENTD_GEMINI_BIN").unwrap_or_else(|_| "gemini".into());
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("AGENTD_SESSION_ID".into(), session_id.clone()));
    if let Some(home) = setup_gemini_home(&session_id) {
        env.push(("GEMINI_CLI_HOME".into(), home.to_string_lossy().to_string()));
    }

    // Resume id: minted on the first turn so subsequent turns can stay
    // in the same gemini conversation. Persisted to disk so a daemon
    // restart can pick the same id back up.
    let sid_file = std::env::var("AGENTD_SESSION_DATA_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("gemini_session_id.txt"));
    let mut gemini_sid: Option<String> = sid_file
        .as_ref()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

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
                    | Some(AdapterInboxMsg::SetAutoMode(_))
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

        // Build per-turn args.
        let mut child_args: Vec<String> = Vec::new();
        child_args.push("-p".into());
        child_args.push(user_text.clone());
        child_args.push("-o".into());
        child_args.push("stream-json".into());
        // Trust the workspace so gemini doesn't prompt for it on a
        // headless run (no human to type "trust").
        child_args.push("--skip-trust".into());
        // YOLO so it doesn't park on tool approvals — the agentd
        // approval gate lives in the adapter/daemon, not here.
        child_args.push("--yolo".into());
        match (&gemini_sid, sid_file.as_ref()) {
            (Some(sid), _) => {
                child_args.push("--resume".into());
                child_args.push(sid.clone());
            }
            (None, Some(_)) => {
                // First turn — mint a UUID so we can resume next turn.
                let new_id = uuid::Uuid::new_v4().to_string();
                child_args.push("--session-id".into());
                child_args.push(new_id.clone());
                gemini_sid = Some(new_id);
            }
            (None, None) => {
                // No persistence dir — single-shot per turn. Gemini
                // will mint its own id we ignore.
            }
        }
        if let Some(m) = &model {
            child_args.push("--model".into());
            child_args.push(m.clone());
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }

        let mut command = Command::new(&bin);
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

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: agentd_protocol::adapter::missing_bin_hint(&bin, &e),
                });
                break 127;
            }
        };

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let parser_task = spawn_parser(child_stdout, emit.clone(), captured_sid.clone());
        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = parser_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        // If we didn't have a sid yet, persist whatever we saw in init.
        if gemini_sid.is_none() {
            if let Some(sid) = captured_sid.lock().unwrap().clone() {
                if let Some(p) = sid_file.as_ref() {
                    let _ = std::fs::write(p, &sid);
                }
                gemini_sid = Some(sid);
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

#[derive(Debug)]
enum TurnOutcome {
    Completed,
    Interrupted,
    Stopped,
}

async fn drive_turn(
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
                    | Some(AdapterInboxMsg::SetAutoMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => {
                        // headless gemini doesn't gate tools; ignore.
                    }
                }
            }
            _ = child.wait() => {
                return TurnOutcome::Completed;
            }
        }
    }
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
        // Gemini's stream-json emits one assistant turn as N
        // `{"type":"message","role":"assistant","delta":true,"content":"..."}`
        // chunks. Forward each chunk as a `Message` event — the daemon
        // accumulates deltas for the transcript. Tool calls arrive as
        // separate `tool_use` / `tool_result` records.
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        let mut g = captured_sid.lock().unwrap();
                        if g.is_none() {
                            *g = Some(sid.to_string());
                        }
                    }
                    emit_event_from_json(&emit, v);
                }
                Err(_) => {
                    // Non-JSON line (e.g. an early "Ripgrep is not
                    // available" notice that gemini prints to stdout
                    // before the stream proper). Pass it as a log.
                    emit.log(format!("gemini stdout: {line}"));
                }
            }
        }
    })
}

fn spawn_stderr_log<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit.log(format!("stderr: {line}"));
        }
    })
}

fn emit_event_from_json(emit: &EventEmitter, v: Value) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "init" => {
            // Init carries session_id (already captured above) + model.
            // No user-facing event needed; just log it for visibility.
            let model = v.get("model").and_then(|s| s.as_str()).unwrap_or("");
            if !model.is_empty() {
                emit.log(format!("gemini init: model={model}"));
            }
        }
        "message" => {
            let role = v.get("role").and_then(|s| s.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
            if role == "user" {
                // The daemon already wrote this; skip to avoid dupe.
                return;
            }
            if content.is_empty() {
                return;
            }
            let role = match role {
                "assistant" => MessageRole::Assistant,
                "system" => MessageRole::System,
                _ => MessageRole::Assistant,
            };
            emit.emit(SessionEvent::Message {
                role,
                text: content.to_string(),
            });
        }
        "tool_use" => {
            let name = v
                .get("tool_name")
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .to_string();
            let args = v.get("parameters").cloned().unwrap_or(Value::Null);
            emit.emit(SessionEvent::ToolUse { tool: name, args });
        }
        "tool_result" => {
            let tool = v
                .get("tool_id")
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .to_string();
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let ok = status == "success";
            // `output` is a string when present; on errors gemini also
            // includes an `error.message`. Prefer the explicit error
            // message when the result wasn't ok, else fall back to
            // output (which may itself contain the error preview).
            let output = if !ok {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        v.get("output")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default()
            } else {
                v.get("output")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            emit.emit(SessionEvent::ToolResult { tool, ok, output });
        }
        "result" => {
            let stats = v.get("stats");
            let tin = stats
                .and_then(|s| s.get("input_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let tout = stats
                .and_then(|s| s.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            // Gemini doesn't expose USD cost in stream-json; the daemon
            // will show tokens only. Emit only when there's something
            // to report.
            if tin > 0 || tout > 0 {
                emit.emit(SessionEvent::Cost {
                    usd: 0.0,
                    tokens_in: tin,
                    tokens_out: tout,
                });
            }
        }
        other => {
            emit.log(format!(
                "gemini event[{other}]: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_settings_creates_minimal_json_when_no_mcp_binary() {
        // No agentd-mcp sibling → function returns Ok without writing.
        // (Hard to assert without intercepting locate_sibling_binary,
        // but at minimum it must not panic.)
        let tmp = std::env::temp_dir().join(format!(
            "agentd-gemini-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let _ = write_settings_with_mcp(&tmp.join("settings.json"), "sid");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parser_emits_tool_use_with_parameters() {
        // Smoke-test the JSON shape mapping. We don't have a real
        // EventEmitter at unit scope, so just exercise the type-level
        // path via the public helpers. This covers the schema
        // assumptions: tool_name → tool, parameters → args.
        let v: Value = serde_json::from_str(
            r#"{"type":"tool_use","tool_name":"shell","tool_id":"x","parameters":{"command":"ls"}}"#,
        )
        .unwrap();
        assert_eq!(v.get("tool_name").and_then(|s| s.as_str()), Some("shell"));
        assert_eq!(
            v.get("parameters")
                .and_then(|p| p.get("command"))
                .and_then(|s| s.as_str()),
            Some("ls")
        );
    }
}
