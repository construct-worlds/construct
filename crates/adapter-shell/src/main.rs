//! Generic shell adapter.
//!
//! The session's prompt is interpreted as a shell command. Stdout is streamed
//! as assistant messages; stderr as system messages. While the command runs,
//! `session.input` writes lines to the child's stdin — handy for commands
//! that read from stdin (e.g. `python -i`, `cat`, etc.).

use agentd_protocol::adapter::{run, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "shell".into(),
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
        let cmd = params.prompt.clone().unwrap_or_default();
        if cmd.trim().is_empty() {
            ctx.emit.emit(SessionEvent::Error {
                message: "shell adapter: no command provided".into(),
            });
            ctx.emit.emit(SessionEvent::Done { exit_code: 64 });
            return;
        }

        ctx.emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: Some(format!("sh -c {cmd:?}")),
        });

        let cwd = PathBuf::from(&params.cwd);
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &params.env {
            command.env(k, v);
        }

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: format!("spawn failed: {e}"),
                });
                ctx.emit.emit(SessionEvent::Done { exit_code: 127 });
                return;
            }
        };

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let mut stdin = child.stdin.take();

        let out_task = spawn_drain(stdout, ctx.emit.clone(), MessageRole::Assistant);
        let err_task = spawn_drain(stderr, ctx.emit.clone(), MessageRole::System);

        let mut inbox = ctx.inbox;
        let emit = ctx.emit.clone();
        let status = loop {
            tokio::select! {
                biased;
                msg = inbox.recv() => {
                    match msg {
                        None => continue,
                        Some(AdapterInboxMsg::Input(t)) => {
                            if let Some(s) = stdin.as_mut() {
                                let _ = s.write_all(t.as_bytes()).await;
                                if !t.ends_with('\n') {
                                    let _ = s.write_all(b"\n").await;
                                }
                                let _ = s.flush().await;
                            } else {
                                emit.log("ignored input: child stdin already closed");
                            }
                        }
                        Some(AdapterInboxMsg::Interrupt) => {
                            // Drop stdin to send EOF, then kill the child.
                            let _ = stdin.take();
                            let _ = child.start_kill();
                        }
                        Some(AdapterInboxMsg::Stop) => {
                            let _ = stdin.take();
                            let _ = child.start_kill();
                            break child.wait().await.ok();
                        }
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

fn spawn_drain<R>(reader: R, emit: EventEmitter, role: MessageRole) -> tokio::task::JoinHandle<()>
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
