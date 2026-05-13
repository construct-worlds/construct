//! OpenAI Codex CLI adapter — multi-turn (best-effort).
//!
//! The protocol surface is the same as the claude adapter: each turn spawns a
//! fresh `codex exec <prompt>` process, stdout streams as assistant messages,
//! stderr is forwarded as adapter log lines. When the child exits the session
//! becomes `awaiting_input`; the next `session.input` starts the next turn.
//!
//! **Caveat:** unlike `claude --resume`, this adapter does not currently pass
//! a session id between turns, so each turn starts a fresh codex context.
//! If your codex build supports session resumption, set
//! `AGENTD_CODEX_RESUME_FLAG` to the flag name (e.g. `--session-id`) and the
//! adapter will pass `--<flag> <captured-id>` on subsequent turns; the
//! adapter captures any `session_id` field it sees in JSON output.
//!
//! Honors `AGENTD_CODEX_BIN` for the binary path.

use agentd_protocol::adapter::{run, AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, SessionEvent, SessionStartParams, SessionState,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "codex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_diff: false,
            supports_cost: false,
            models: Vec::new(),
        },
    };
    run(metadata, |params, ctx| async move {
        run_session(params, ctx).await;
    })
    .await
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: _,
        emit,
        mut inbox,
    } = ctx;

    let bin = std::env::var("AGENTD_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let resume_flag = std::env::var("AGENTD_CODEX_RESUME_FLAG").ok();
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

        let mut command = Command::new(&bin);
        command.arg("exec");
        if let (Some(flag), Some(sid)) = (resume_flag.as_ref(), codex_session_id.as_ref()) {
            command.arg(flag).arg(sid);
        }
        if let Some(m) = &model {
            command.arg("-m").arg(m);
        }
        for a in &extra_args {
            command.arg(a);
        }
        command.arg(&user_text);
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
                    message: format!("spawn {bin}: {e}"),
                });
                break 127;
            }
        };

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let stdout_task = spawn_stdout(child_stdout, emit.clone(), captured_sid.clone());
        let stderr_task = spawn_stderr(child_stderr, emit.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        if codex_session_id.is_none() {
            codex_session_id = captured_sid.lock().unwrap().clone();
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
                }
            }
            _ = child.wait() => {
                return TurnOutcome::Completed;
            }
        }
    }
}

fn spawn_stdout<R>(
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
            // Best-effort JSON parse; if not JSON, emit as plain assistant text.
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    let mut g = captured_sid.lock().unwrap();
                    if g.is_none() {
                        *g = Some(sid.to_string());
                    }
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

fn spawn_stderr<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
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

/// Try to pull structured fields out of a codex JSON event. Returns `true` if
/// the value was recognized; otherwise the caller falls back to emitting raw.
fn try_emit_structured(emit: &EventEmitter, v: &Value) -> bool {
    let ty = match v.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    match ty {
        "message" | "assistant" => {
            if let Some(text) = v
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
                .or_else(|| extract_text_from_blocks(v.get("content")))
            {
                if !text.is_empty() {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text,
                    });
                    return true;
                }
            }
            false
        }
        "tool_use" => {
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let args = v.get("input").cloned().unwrap_or(Value::Null);
            emit.emit(SessionEvent::ToolUse { tool: name, args });
            true
        }
        "tool_result" => {
            let tool = v
                .get("tool_use_id")
                .or_else(|| v.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !v
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let output = match v.get("output").or_else(|| v.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            emit.emit(SessionEvent::ToolResult { tool, ok, output });
            true
        }
        _ => false,
    }
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

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}
