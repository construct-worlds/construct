//! Interactive (PTY) mode for zarvis.
//!
//! Zarvis doesn't spawn a child — there's no CLI to attach a real PTY
//! to. Instead we synthesize a terminal session: we emit
//! `SessionEvent::Pty` bytes that look like a chat-style REPL (banner
//! + colored prompt + streaming assistant text + inline tool blocks +
//! inline approval prompts), and we read keystrokes from
//! `AdapterInboxMsg::PtyInput` through a minimal line editor.
//!
//! The TUI's `vt100`-backed terminal pane parses these bytes the same
//! way it parses any other PTY-backed adapter's output.

use crate::agent::{ResolvedModel, SYSTEM_PROMPT};
use crate::context;
use crate::provider::{Content, Message, Role, StopReason, TextSink, ToolCall};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;

const TOOL_OUTPUT_BUDGET: usize = 8_000;

/// Wrapper around `EventEmitter` that writes raw bytes / styled text to
/// the session's PTY stream.
struct Terminal<'a> {
    emit: &'a EventEmitter,
}
impl<'a> Terminal<'a> {
    fn new(emit: &'a EventEmitter) -> Self {
        Self { emit }
    }
    fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.emit.emit(SessionEvent::pty(bytes));
    }
    fn print(&self, s: &str) {
        self.write(s.as_bytes());
    }
    fn newline(&self) {
        self.write(b"\r\n");
    }
    fn prompt(&self) {
        // Bold cyan `❯ `.
        self.write(b"\r\n\x1b[1;36m\xe2\x9d\xaf \x1b[0m");
    }
    /// Banner shown when the session starts.
    fn banner(&self, provider: &str, model: &str, automode: bool) {
        let mode_badge = if automode { "  [automode]" } else { "" };
        let banner = format!(
            "\r\n\x1b[1;35mzarvis\x1b[0m  \x1b[2m{provider}:{model}\x1b[0m{mode_badge}\r\n\
             \x1b[2mtype your prompt and press Enter. C-c interrupts a turn. \
             `/quit` or C-d to end the session.\x1b[0m\r\n",
        );
        self.write(banner.as_bytes());
    }
    fn tool_use(&self, name: &str, args_summary: &str) {
        let line = format!(
            "\r\n\x1b[1;33m→ {name}\x1b[0m\x1b[2m({args_summary})\x1b[0m\r\n"
        );
        self.write(line.as_bytes());
    }
    fn tool_result(&self, ok: bool, output: &str) {
        let glyph = if ok { "\x1b[1;32m✓\x1b[0m" } else { "\x1b[1;31m✗\x1b[0m" };
        // Print a short single-line preview of the result; full content
        // is in the transcript (we also emit ToolResult).
        let one_line: String = output.lines().next().unwrap_or("").chars().take(160).collect();
        let line = format!("  {glyph}  \x1b[2m{one_line}\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
    fn approval(&self, tool: &str, args_summary: &str, risk: ToolRisk) {
        let risk_label = match risk {
            ToolRisk::Safe => "safe",
            ToolRisk::Risky => "risky",
        };
        let line = format!(
            "\r\n\x1b[1;33m? approve [{risk_label}]\x1b[0m {tool}\x1b[2m({args_summary})\x1b[0m\
             — \x1b[1m[y]\x1b[0mes / \x1b[1m[n]\x1b[0mo / \x1b[1m[a]\x1b[0mutomode: "
        );
        self.write(line.as_bytes());
    }
    fn note(&self, msg: &str) {
        let line = format!("\r\n\x1b[2m{msg}\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
}

/// Lines whose start matches one of these labels are dimmed in the PTY
/// (the structured Message event still carries the raw text — this is
/// purely a rendering tweak). Cheap to extend; keep the entries short
/// so the at-start-of-line buffer stays tiny.
const DIM_LINE_PREFIXES: &[&str] = &["Summary:"];

/// Sink for the interactive mode: deltas go directly to the PTY (with
/// optional dim-line styling) and as Message events so the transcript
/// still has the raw text.
struct PtySink<'a> {
    emit: &'a EventEmitter,
    at_line_start: bool,
    in_dim_line: bool,
    /// Buffered chars seen at the start of the current line while we
    /// decide whether they match a `DIM_LINE_PREFIXES` entry. Bounded
    /// by the longest prefix length, so streaming UX stays snappy.
    prefix_buf: String,
}
impl<'a> PtySink<'a> {
    fn new(emit: &'a EventEmitter) -> Self {
        Self {
            emit,
            at_line_start: true,
            in_dim_line: false,
            prefix_buf: String::new(),
        }
    }
}
impl<'a> TextSink for PtySink<'a> {
    fn delta(&mut self, text: &str) {
        let mut out = String::with_capacity(text.len() + 16);
        for c in text.chars() {
            if c == '\n' {
                // End of line: flush any buffered prefix, close dim, CRLF.
                if !self.prefix_buf.is_empty() {
                    out.push_str(&self.prefix_buf);
                    self.prefix_buf.clear();
                }
                if self.in_dim_line {
                    out.push_str("\x1b[0m");
                    self.in_dim_line = false;
                }
                out.push_str("\r\n");
                self.at_line_start = true;
                continue;
            }
            if self.at_line_start && !self.in_dim_line {
                self.prefix_buf.push(c);
                // Did we just complete one of the dim labels?
                if let Some(matched) = DIM_LINE_PREFIXES
                    .iter()
                    .find(|p| **p == self.prefix_buf.as_str())
                {
                    out.push_str("\x1b[2m");
                    out.push_str(matched);
                    self.prefix_buf.clear();
                    self.in_dim_line = true;
                    self.at_line_start = false;
                    continue;
                }
                // Still a prefix of some candidate? keep buffering.
                let still_candidate = DIM_LINE_PREFIXES
                    .iter()
                    .any(|p| p.starts_with(self.prefix_buf.as_str()));
                if !still_candidate {
                    out.push_str(&self.prefix_buf);
                    self.prefix_buf.clear();
                    self.at_line_start = false;
                }
                continue;
            }
            out.push(c);
        }
        if !out.is_empty() {
            self.emit.emit(SessionEvent::pty(out.as_bytes()));
        }
        // Transcript copy stays raw.
        self.emit.emit(SessionEvent::Message {
            role: agentd_protocol::MessageRole::Assistant,
            text: text.to_string(),
        });
    }
}

/// Minimal terminal line editor — handles printable ASCII, Backspace,
/// Enter, Ctrl-C, Ctrl-D. Returns one event per fed byte.
struct LineEditor {
    buf: String,
}
impl LineEditor {
    fn new() -> Self {
        Self { buf: String::new() }
    }
    fn feed(&mut self, b: u8) -> LineEvent {
        match b {
            // Ctrl-C
            0x03 => LineEvent::Interrupt,
            // Ctrl-D — EOF if buffer empty, else nothing.
            0x04 => {
                if self.buf.is_empty() {
                    LineEvent::Eof
                } else {
                    LineEvent::None
                }
            }
            // Enter (CR or LF)
            b'\r' | b'\n' => {
                let line = std::mem::take(&mut self.buf);
                LineEvent::Submit(line)
            }
            // Backspace / DEL
            0x08 | 0x7f => {
                if self.buf.pop().is_some() {
                    LineEvent::Backspace
                } else {
                    LineEvent::None
                }
            }
            // Skip other control codes; let printable bytes through.
            b if b < 0x20 => LineEvent::None,
            b => {
                self.buf.push(b as char);
                LineEvent::Echo(b)
            }
        }
    }
}

enum LineEvent {
    Echo(u8),
    Backspace,
    Submit(String),
    Interrupt,
    Eof,
    None,
}

/// Tri-state interrupt signal used during in-flight turns.
#[derive(Default)]
struct Interrupted {
    flag: std::sync::atomic::AtomicBool,
}
impl Interrupted {
    fn set(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn take(&self) -> bool {
        self.flag.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

pub async fn run(
    params: SessionStartParams,
    ctx: AdapterContext,
    spec: ResolvedModel,
) -> Result<()> {
    let AdapterContext { session_id, emit, mut inbox } = ctx;
    let provider_name = spec.provider_name();
    let model = spec.model.clone();
    let provider = spec.provider;
    let cwd = PathBuf::from(&params.cwd);
    let registry = ToolRegistry::with_defaults();
    let specs = registry.specs();
    let mut automode = std::env::var("AGENTD_ZARVIS_AUTOMODE").as_deref() == Ok("1");

    let term = Terminal::new(&emit);
    term.banner(provider_name, &model, automode);
    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: Some(format!("{}:{}  [interactive]", provider_name, model)),
    });
    term.prompt();

    let tool_ctx = ToolCtx {
        cwd,
        session_id,
        client: tokio::sync::OnceCell::new(),
    };

    let mut editor = LineEditor::new();
    let mut messages: Vec<Message> = Vec::new();
    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = params.prompt.clone() {
        if !p.trim().is_empty() {
            pending.push_back(p);
        }
    }

    'outer: loop {
        // Wait for a user message, either from pending or by typing.
        let user_text = if let Some(t) = pending.pop_front() {
            // Echo the pre-supplied prompt as if the user typed it, so
            // the transcript is faithful.
            term.print(&t);
            term.newline();
            t
        } else {
            emit.emit(SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            });
            match read_one_line(&mut inbox, &mut editor, &term, &mut automode).await {
                ReadOutcome::Line(t) => t,
                ReadOutcome::Stop => break 'outer,
                ReadOutcome::Eof => {
                    term.note("(end of session)");
                    break 'outer;
                }
            }
        };

        // Slash-quit shortcut.
        let trimmed = user_text.trim();
        if trimmed == "/quit" || trimmed == "/exit" {
            term.note("(bye)");
            break;
        }
        if trimmed.is_empty() {
            term.prompt();
            continue;
        }

        messages.push(Message {
            role: Role::User,
            content: Content::Text(user_text.clone()),
        });
        emit.emit(SessionEvent::Message {
            role: agentd_protocol::MessageRole::User,
            text: user_text,
        });
        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });

        // Inner step loop — feed tool results back until end-of-turn.
        loop {
            let _pruned = context::prune(&mut messages, provider_name, &model);
            let mut sink = PtySink::new(&emit);
            let turn = match provider
                .complete(&model, SYSTEM_PROMPT, &messages, &specs, &mut sink)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    term.note(&format!("(provider error: {e})"));
                    emit.emit(SessionEvent::Error { message: format!("{e}") });
                    break;
                }
            };
            emit.emit(SessionEvent::Cost {
                usd: turn.usage.usd,
                tokens_in: turn.usage.input_tokens,
                tokens_out: turn.usage.output_tokens,
            });

            if turn.tool_calls.is_empty() {
                if let Some(text) = turn.text {
                    messages.push(Message {
                        role: Role::Assistant,
                        content: Content::Text(text),
                    });
                }
                break;
            }

            messages.push(Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: turn.text.clone(),
                    calls: turn.tool_calls.clone(),
                },
            });
            for call in turn.tool_calls.iter() {
                let outcome = run_one_tool(
                    call,
                    &registry,
                    &tool_ctx,
                    &emit,
                    &term,
                    &mut inbox,
                    &mut automode,
                )
                .await;
                let outcome = match outcome {
                    Ok(o) => o,
                    Err(reason) => {
                        messages.push(Message {
                            role: Role::Tool,
                            content: Content::ToolResult {
                                call_id: call.id.clone(),
                                output: format!("(turn aborted: {reason})"),
                                is_error: true,
                            },
                        });
                        if reason == "stop" {
                            return Ok(());
                        }
                        break;
                    }
                };
                let truncated = truncate_for_model(&outcome.output, TOOL_OUTPUT_BUDGET);
                messages.push(Message {
                    role: Role::Tool,
                    content: Content::ToolResult {
                        call_id: call.id.clone(),
                        output: truncated,
                        is_error: !outcome.ok,
                    },
                });
            }
            if matches!(turn.stop_reason, StopReason::MaxTokens) {
                break;
            }
        }

        term.prompt();
    }
    Ok(())
}

enum ReadOutcome {
    Line(String),
    Stop,
    Eof,
}

async fn read_one_line(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    automode: &mut bool,
) -> ReadOutcome {
    loop {
        match inbox.recv().await {
            None => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => {
                // Discard current line, redraw prompt.
                editor.buf.clear();
                term.newline();
                term.prompt();
            }
            Some(AdapterInboxMsg::Input(t)) => {
                // External input from `agent send_input` — treat as if the
                // user typed it, echo for the transcript.
                term.print(&t);
                term.newline();
                return ReadOutcome::Line(t);
            }
            Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                for b in bytes {
                    match editor.feed(b) {
                        LineEvent::Echo(c) => term.write(&[c]),
                        LineEvent::Backspace => term.write(b"\x08 \x08"),
                        LineEvent::Submit(line) => {
                            term.newline();
                            return ReadOutcome::Line(line);
                        }
                        LineEvent::Interrupt => {
                            editor.buf.clear();
                            term.note("(C-c)");
                            term.prompt();
                        }
                        LineEvent::Eof => return ReadOutcome::Eof,
                        LineEvent::None => {}
                    }
                }
            }
            Some(AdapterInboxMsg::PtyResize { .. }) => {
                // We don't currently track size for line-wrapping; rely
                // on the terminal emulator to wrap.
            }
            Some(AdapterInboxMsg::ToolDecision { .. }) => {}
        }
    }
}

/// Run one tool with approval gating + interrupt support. Mirrors the
/// headless version but renders into the PTY and reads y/n/a from
/// PtyInput when prompting.
async fn run_one_tool(
    call: &ToolCall,
    registry: &ToolRegistry,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    term: &Terminal<'_>,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    automode: &mut bool,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            term.tool_use(&call.name, &serde_json::to_string(&call.input).unwrap_or_default());
            term.tool_result(false, &format!("unknown tool: {}", call.name));
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
        }
    };

    let args_summary = tool.args_summary(&call.input);
    term.tool_use(&call.name, &args_summary);
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
    });

    let needs_approval = !*automode && matches!(tool.risk(), ToolRisk::Risky);
    if needs_approval {
        term.approval(&call.name, &args_summary, tool.risk());
        emit.emit(SessionEvent::ToolApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_summary: args_summary.clone(),
            risk: tool.risk(),
        });
        let decision = wait_for_approval(inbox, &call.id, automode).await;
        match decision {
            ApprovalOutcome::Stop => return Err("stop".into()),
            ApprovalOutcome::Interrupt => return Err("interrupt".into()),
            ApprovalOutcome::Deny => {
                term.print("n\r\n");
                let msg = "user denied this action".to_string();
                emit.emit(SessionEvent::ToolResult {
                    tool: call.id.clone(),
                    ok: false,
                    output: msg.clone(),
                });
                term.tool_result(false, &msg);
                return Ok(ToolOutcome { ok: false, output: msg });
            }
            ApprovalOutcome::Approve => term.print("y\r\n"),
            ApprovalOutcome::Automode => {
                term.print("a\r\n");
                *automode = true;
            }
        }
    }

    let outcome = run_with_interrupt(tool, call.input.clone(), tool_ctx, inbox).await;
    match &outcome {
        Ok(o) => {
            term.tool_result(o.ok, &o.output);
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: o.ok,
                output: o.output.clone(),
            });
        }
        Err(reason) => {
            term.tool_result(false, &format!("({reason})"));
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("({reason})"),
            });
        }
    }
    outcome
}

enum ApprovalOutcome {
    Approve,
    Deny,
    Automode,
    Stop,
    Interrupt,
}

async fn wait_for_approval(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    call_id: &str,
    automode: &mut bool,
) -> ApprovalOutcome {
    loop {
        match inbox.recv().await {
            None => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => return ApprovalOutcome::Interrupt,
            Some(AdapterInboxMsg::SetAutoMode(on)) => {
                *automode = on;
                if on {
                    return ApprovalOutcome::Automode;
                }
            }
            Some(AdapterInboxMsg::ToolDecision { call_id: cid, decision })
                if cid == call_id =>
            {
                return match decision.as_str() {
                    "approve" => ApprovalOutcome::Approve,
                    "automode" => {
                        *automode = true;
                        ApprovalOutcome::Automode
                    }
                    _ => ApprovalOutcome::Deny,
                };
            }
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                // Single-key approval from the PTY.
                for b in bytes {
                    match b {
                        b'y' | b'Y' | b'\r' | b'\n' => return ApprovalOutcome::Approve,
                        b'n' | b'N' | 0x1b | 0x07 => return ApprovalOutcome::Deny,
                        b'a' | b'A' => {
                            *automode = true;
                            return ApprovalOutcome::Automode;
                        }
                        0x03 => return ApprovalOutcome::Deny,
                        _ => {}
                    }
                }
            }
            Some(_) => {}
        }
    }
}

async fn run_with_interrupt(
    tool: &dyn crate::tools::Tool,
    input: serde_json::Value,
    ctx: &ToolCtx,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> std::result::Result<ToolOutcome, String> {
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let client_cell = std::sync::Mutex::new(ctx.client.get().cloned());
    let tool_fut = async {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
        };
        if let Some(c) = client_cell.lock().unwrap().clone() {
            let _ = local_ctx.client.set(c);
        }
        tool.run(input, &local_ctx).await
    };
    tokio::select! {
        biased;
        kind = wait_for_interrupt(inbox) => {
            match kind {
                InterruptKind::Stop => Err("stop".into()),
                _ => Err("interrupt".into()),
            }
        }
        res = tool_fut => res.map_err(|e| format!("tool error: {e}")),
    }
}

enum InterruptKind {
    Stop,
    Interrupt,
    Channel,
}

async fn wait_for_interrupt(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> InterruptKind {
    loop {
        match inbox.recv().await {
            None => return InterruptKind::Channel,
            Some(AdapterInboxMsg::Stop) => return InterruptKind::Stop,
            Some(AdapterInboxMsg::Interrupt) => return InterruptKind::Interrupt,
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                if bytes.contains(&0x03) {
                    return InterruptKind::Interrupt;
                }
            }
            Some(_) => {}
        }
    }
}
