//! Claude Code adapter.
//!
//! Wraps the `claude` CLI in single-shot mode with `--output-format stream-json`.
//! Best-effort: the stream-json shape may evolve; unknown event types are
//! forwarded as adapter log lines instead of dropping them silently.
//!
//! Honors `AGENTD_CLAUDE_BIN` if set to override the binary; otherwise
//! requires `claude` to be on `PATH`.

use agentd_protocol::adapter::{run, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState,
};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "claude".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: false,
            supports_interrupt: true,
            supports_diff: false,
            supports_cost: true,
            models: Vec::new(),
        },
    };
    run(metadata, |params, ctx| async move {
        let prompt = params.prompt.clone().unwrap_or_default();
        if prompt.trim().is_empty() {
            ctx.emit.emit(SessionEvent::Error {
                message: "claude adapter: no prompt provided".into(),
            });
            ctx.emit.emit(SessionEvent::Done { exit_code: 64 });
            return;
        }

        let bin = std::env::var("AGENTD_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
        let cwd = PathBuf::from(&params.cwd);
        let mut command = Command::new(&bin);
        command
            .arg("-p")
            .arg(&prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
        if let Some(model) = &params.model {
            command.arg("--model").arg(model);
        }
        for extra in &params.args {
            command.arg(extra);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &params.env {
            command.env(k, v);
        }

        ctx.emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: Some(format!("{} -p ...", bin)),
        });

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: format!("spawn {bin}: {e}"),
                });
                ctx.emit.emit(SessionEvent::Done { exit_code: 127 });
                return;
            }
        };

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let parse_task = spawn_stream_parser(stdout, ctx.emit.clone());
        let err_task = spawn_stderr_log(stderr, ctx.emit.clone());

        let mut inbox = ctx.inbox;
        let status = loop {
            tokio::select! {
                biased;
                msg = wait_stop(&mut inbox) => {
                    if msg {
                        let _ = child.start_kill();
                        break child.wait().await.ok();
                    }
                }
                s = child.wait() => {
                    break s.ok();
                }
            }
        };

        let _ = parse_task.await;
        let _ = err_task.await;

        let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);
        ctx.emit.emit(SessionEvent::Done { exit_code });
    })
    .await
}

async fn wait_stop(inbox: &mut mpsc::Receiver<AdapterInboxMsg>) -> bool {
    while let Some(msg) = inbox.recv().await {
        match msg {
            AdapterInboxMsg::Interrupt | AdapterInboxMsg::Stop => return true,
            AdapterInboxMsg::Input(_) => {
                // single-shot mode doesn't accept additional input
            }
        }
    }
    false
}

fn spawn_stream_parser<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
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
                Ok(v) => emit_event_from_json(&emit, v),
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
            let text = extract_message_text(v.get("message"));
            if !text.is_empty() {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::User,
                    text,
                });
            }
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
            if let Some(s) = v.get("result").and_then(|r| r.as_str()) {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: s.to_string(),
                });
            }
        }
        "system" => {
            emit.log(format!(
                "system: {}",
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
    let Some(m) = msg else { return String::new(); };
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
    let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) else { return; };
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
    let Some(arr) = msg.and_then(|m| m.get("content")).and_then(|c| c.as_array()) else { return; };
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
