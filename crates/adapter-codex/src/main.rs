//! OpenAI Codex CLI adapter.
//!
//! Single-shot wrapper around `codex exec`. Streams stdout as assistant
//! messages; stderr is forwarded as adapter log lines. Honors
//! `AGENTD_CODEX_BIN` to override the binary path.

use agentd_protocol::adapter::{run, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState,
};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "codex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: false,
            supports_interrupt: true,
            supports_diff: false,
            supports_cost: false,
            models: Vec::new(),
        },
    };
    run(metadata, |params, ctx| async move {
        let prompt = params.prompt.clone().unwrap_or_default();
        if prompt.trim().is_empty() {
            ctx.emit.emit(SessionEvent::Error {
                message: "codex adapter: no prompt provided".into(),
            });
            ctx.emit.emit(SessionEvent::Done { exit_code: 64 });
            return;
        }

        let bin = std::env::var("AGENTD_CODEX_BIN").unwrap_or_else(|_| "codex".into());
        let cwd = PathBuf::from(&params.cwd);
        let mut command = Command::new(&bin);
        command.arg("exec").arg(&prompt);
        if let Some(model) = &params.model {
            command.arg("-m").arg(model);
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
            detail: Some(format!("{} exec ...", bin)),
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
        let out_task = spawn_lines(stdout, ctx.emit.clone(), MessageRole::Assistant);
        let err_task = spawn_stderr(stderr, ctx.emit.clone());

        let mut inbox = ctx.inbox;
        let status = loop {
            tokio::select! {
                biased;
                stop = wait_stop(&mut inbox) => {
                    if stop {
                        let _ = child.start_kill();
                        break child.wait().await.ok();
                    }
                }
                s = child.wait() => {
                    break s.ok();
                }
            }
        };

        let _ = out_task.await;
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
            AdapterInboxMsg::Input(_) => {}
        }
    }
    false
}

fn spawn_lines<R>(
    reader: R,
    emit: EventEmitter,
    role: MessageRole,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit.emit(SessionEvent::Message {
                role,
                text: line,
            });
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
