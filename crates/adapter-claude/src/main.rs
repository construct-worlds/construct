//! Claude Code adapter — multi-turn.
//!
//! Each conversational turn spawns a fresh `claude -p` process with
//! `--input-format stream-json --output-format stream-json --verbose`. The
//! initial turn has no session id; subsequent turns pass `--resume <id>`
//! so the underlying CLI threads the conversation together.
//!
//! Honors `AGENTD_CLAUDE_BIN` for the binary path.
//!
//! Adapter inbox semantics while a turn is running:
//!   - `Input` → queued for the next turn (echoed as a log line)
//!   - `Interrupt` → kill current child; loop back to await/run next input
//!   - `Stop` → kill current child; emit Done and exit
//!
//! `Input` arriving while we're already awaiting input is consumed immediately
//! and dispatched as the next turn's prompt.

use agentd_protocol::adapter::{run, AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, SessionEvent, SessionStartParams, SessionState,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "claude".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_diff: false,
            supports_cost: true,
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

    let bin = std::env::var("AGENTD_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut session_id: Option<String> = None;
    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = params.prompt.clone() {
        if !p.trim().is_empty() {
            pending.push_back(p);
        }
    }

    let exit_code = loop {
        // Pick next user message, or wait for one.
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

        // Build the per-turn child command.
        let mut command = Command::new(&bin);
        command
            .arg("-p")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
        if let Some(sid) = &session_id {
            command.arg("--resume").arg(sid);
        }
        if let Some(m) = &model {
            command.arg("--model").arg(m);
        }
        for a in &extra_args {
            command.arg(a);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
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

        let child_stdin = child.stdin.take().expect("piped");
        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        // Write the user message, then close stdin so claude knows we're done.
        let writer_task = spawn_writer(child_stdin, user_text.clone());
        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let parser_task =
            spawn_parser(child_stdout, emit.clone(), captured_sid.clone());

        // Drive the child: queue mid-turn inputs, honor stop/interrupt.
        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = writer_task.await;
        let _ = parser_task.await;
        let _ = stderr_task.await;
        // Make sure the child is fully reaped.
        let _ = child.wait().await;

        if session_id.is_none() {
            session_id = captured_sid.lock().unwrap().clone();
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
                        // daemon channel closed
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

fn spawn_writer(mut stdin: tokio::process::ChildStdin, user_text: String) -> tokio::task::JoinHandle<()> {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": user_text }]
        }
    });
    tokio::spawn(async move {
        let line = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = stdin.write_all(line.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
        let _ = stdin.shutdown().await;
    })
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
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        let mut g = captured_sid.lock().unwrap();
                        if g.is_none() {
                            *g = Some(sid.to_string());
                        }
                    }
                    emit_event_from_json(&emit, v);
                }
                Err(_) => emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: line,
                }),
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
        "assistant" => {
            let text = extract_message_text(v.get("message"));
            if !text.is_empty() {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text,
                });
            }
            forward_tool_uses(emit, v.get("message"));
        }
        "user" => {
            // The CLI echoes tool_result blocks here. The actual user text is
            // already in the transcript (daemon emits it when sending input).
            forward_tool_results(emit, v.get("message"));
        }
        "result" => {
            let usd = v
                .get("total_cost_usd")
                .and_then(|n| n.as_f64())
                .unwrap_or(0.0);
            let tin = v
                .get("usage")
                .and_then(|u| u.get("input_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let tout = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            if usd > 0.0 || tin > 0 || tout > 0 {
                emit.emit(SessionEvent::Cost {
                    usd,
                    tokens_in: tin,
                    tokens_out: tout,
                });
            }
            // The `result` text duplicates the assistant's final message; skip it.
        }
        "system" => {
            emit.log(format!(
                "system: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
        "rate_limit_event" => {
            emit.log(format!(
                "rate_limit: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
        other => {
            emit.log(format!(
                "claude event[{other}]: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
    }
}

fn extract_message_text(msg: Option<&Value>) -> String {
    let Some(m) = msg else {
        return String::new();
    };
    if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        return out;
    }
    String::new()
}

fn forward_tool_uses(emit: &EventEmitter, msg: Option<&Value>) {
    let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
        return;
    };
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            emit.emit(SessionEvent::ToolUse {
                tool: name,
                args: input,
            });
        }
    }
}

fn forward_tool_results(emit: &EventEmitter, msg: Option<&Value>) {
    let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
        return;
    };
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            let tool = block
                .get("tool_use_id")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let output = match block.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                None => String::new(),
            };
            emit.emit(SessionEvent::ToolResult { tool, ok, output });
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
