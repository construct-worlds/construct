//! Agentd-control tools: thin wrappers over `agentd_client::Client`.
//! Lets a zarvis session drive the daemon (list/spawn/send-input to
//! other sessions) using natural-language tool calls — the same surface
//! the MCP server exposes.

use super::{Tool, ToolCtx, ToolOutcome};
use agentd_client::Client;
use agentd_protocol::{paths::Paths, CreateSessionParams, PtySize, ToolRisk};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;

async fn client(ctx: &ToolCtx) -> Result<Arc<Client>> {
    ctx.client
        .get_or_try_init(|| async {
            let socket = Paths::discover().socket();
            let c = Client::connect(&socket).await?;
            Ok::<Arc<Client>, anyhow::Error>(c)
        })
        .await
        .cloned()
}

fn need_str(input: &Value, k: &str) -> Result<String> {
    input
        .get(k)
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing `{k}`"))
}

// ---------- read ----------

pub struct Whoami;
#[async_trait]
impl Tool for Whoami {
    fn name(&self) -> &str { "agentd_whoami" }
    fn description(&self) -> &str {
        "Returns the session id of the agentd session this agent is running inside, \
         or null if not inside one. Use this to avoid acting on yourself."
    }
    fn schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "session_id": ctx.session_id }).to_string(),
        })
    }
}

pub struct ListSessions;
#[async_trait]
impl Tool for ListSessions {
    fn name(&self) -> &str { "agentd_list_sessions" }
    fn description(&self) -> &str {
        "List every agentd session (running and finished). Each entry includes the \
         session id, harness, state, cwd, pinned flag, automode flag, last_pty_at_ms \
         (use `now - last_pty_at_ms < 600ms` as a 'is the agent currently busy?' \
         signal), and group info when applicable."
    }
    fn schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let c = client(ctx).await?;
        let sessions = c.list().await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&sessions)?,
        })
    }
}

pub struct GetSession;
#[async_trait]
impl Tool for GetSession {
    fn name(&self) -> &str { "agentd_get_session" }
    fn description(&self) -> &str { "Fetch the full summary + structured transcript for one session." }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let det = c.get(&sid).await?;
        Ok(ToolOutcome { ok: true, output: serde_json::to_string(&det)? })
    }
}

pub struct GetTranscript;
#[async_trait]
impl Tool for GetTranscript {
    fn name(&self) -> &str { "agentd_get_transcript" }
    fn description(&self) -> &str {
        "Fetch a slice of the session's structured event log. `from` is a 1-based seq; \
         `limit` bounds the count."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "from":       { "type": "integer", "minimum": 0 },
                "limit":      { "type": "integer", "minimum": 1 }
            },
            "required": ["session_id"]
        })
    }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let from = input.get("from").and_then(|n| n.as_u64()).unwrap_or(0);
        let limit = input.get("limit").and_then(|n| n.as_u64()).map(|n| n as usize);
        let c = client(ctx).await?;
        let res = c.transcript(&sid, from, limit).await?;
        Ok(ToolOutcome { ok: true, output: serde_json::to_string(&res)? })
    }
}

pub struct GetOutput;
#[async_trait]
impl Tool for GetOutput {
    fn name(&self) -> &str { "agentd_get_output" }
    fn description(&self) -> &str {
        "Fetch the session's recent PTY scrollback as text (UTF-8 lossy). Use for \
         reading what's on the screen of a PTY-backed session."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let snap = c.pty_replay(&sid).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&snap.data)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(ToolOutcome { ok: true, output: text })
    }
}

pub struct GetDiff;
#[async_trait]
impl Tool for GetDiff {
    fn name(&self) -> &str { "agentd_get_diff" }
    fn description(&self) -> &str { "`git diff HEAD` for the session's worktree (empty if not a git repo)." }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let d = c.diff(&sid).await?;
        Ok(ToolOutcome { ok: true, output: serde_json::to_string(&d)? })
    }
}

pub struct ListHarnesses;
#[async_trait]
impl Tool for ListHarnesses {
    fn name(&self) -> &str { "agentd_list_harnesses" }
    fn description(&self) -> &str { "List available adapter harnesses (shell, claude, codex, zarvis, …)." }
    fn schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    fn risk(&self) -> ToolRisk { ToolRisk::Safe }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let c = client(ctx).await?;
        let h = c.harnesses().await?;
        Ok(ToolOutcome { ok: true, output: serde_json::to_string(&h)? })
    }
}

// ---------- write ----------

pub struct CreateSession;
#[async_trait]
impl Tool for CreateSession {
    fn name(&self) -> &str { "agentd_create_session" }
    fn description(&self) -> &str {
        "Spawn a new session. `harness` must match `agentd_list_harnesses`. `cwd` \
         defaults to the daemon process cwd. `worktree:true` starts in an isolated \
         git worktree."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "harness":  { "type": "string" },
                "cwd":      { "type": "string" },
                "prompt":   { "type": "string" },
                "title":    { "type": "string" },
                "mode":     { "type": "string", "enum": ["interactive", "headless"] },
                "worktree": { "type": "boolean" }
            },
            "required": ["harness"]
        })
    }
    fn risk(&self) -> ToolRisk { ToolRisk::Risky }
    fn args_summary(&self, input: &Value) -> String {
        let h = input.get("harness").and_then(|s| s.as_str()).unwrap_or("?");
        let p = input.get("prompt").and_then(|s| s.as_str()).unwrap_or("");
        if p.is_empty() {
            format!("harness={h}")
        } else {
            format!("harness={h} prompt={p}")
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let harness = need_str(&input, "harness")?;
        let cwd = input
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
        let params = CreateSessionParams {
            harness,
            cwd,
            prompt: input.get("prompt").and_then(|s| s.as_str()).map(|s| s.to_string()),
            model: None,
            title: input.get("title").and_then(|s| s.as_str()).map(|s| s.to_string()),
            mode: input.get("mode").and_then(|s| s.as_str()).map(|s| s.to_string()),
            pty_size: Some(PtySize { cols: 100, rows: 30 }),
            worktree: input.get("worktree").and_then(|v| v.as_bool()).unwrap_or(false),
            env: Default::default(),
            args: Vec::new(),
        };
        let c = client(ctx).await?;
        let sid = c.create(params).await?;
        Ok(ToolOutcome { ok: true, output: json!({ "session_id": sid }).to_string() })
    }
}

macro_rules! simple_write_tool {
    ($struct_name:ident, $tool_name:literal, $desc:literal, $extra_props:expr, $required:expr, $call:expr, $summary_key:literal) => {
        pub struct $struct_name;
        #[async_trait]
        impl Tool for $struct_name {
            fn name(&self) -> &str { $tool_name }
            fn description(&self) -> &str { $desc }
            fn schema(&self) -> Value {
                let mut props = serde_json::Map::new();
                props.insert("session_id".to_string(), json!({ "type": "string" }));
                for (k, v) in $extra_props {
                    props.insert(k.to_string(), v);
                }
                json!({
                    "type": "object",
                    "properties": Value::Object(props),
                    "required": $required,
                })
            }
            fn risk(&self) -> ToolRisk { ToolRisk::Risky }
            fn args_summary(&self, input: &Value) -> String {
                let sid = input.get("session_id").and_then(|s| s.as_str()).unwrap_or("?");
                if $summary_key.is_empty() {
                    sid.to_string()
                } else {
                    let extra = input.get($summary_key).and_then(|s| s.as_str()).unwrap_or("");
                    if extra.is_empty() { sid.to_string() } else { format!("{sid} {}", super::truncate_for_model(extra, 120)) }
                }
            }
            async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
                let sid = need_str(&input, "session_id")?;
                let c = client(ctx).await?;
                ($call)(&c, &sid, &input).await?;
                Ok(ToolOutcome { ok: true, output: json!({ "ok": true }).to_string() })
            }
        }
    };
}

simple_write_tool!(
    SendInput,
    "agentd_send_input",
    "Send a line of text to a session as user input (line-oriented).",
    vec![("text", json!({ "type": "string" }))],
    json!(["session_id", "text"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let text = need_str(input, "text").unwrap_or_default();
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.send_input(&sid, text).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "text"
);

simple_write_tool!(
    SendKeys,
    "agentd_send_keys",
    "Send raw bytes (base64-encoded) to a PTY-backed session. Use for control chars / arrows.",
    vec![("bytes_b64", json!({ "type": "string" }))],
    json!(["session_id", "bytes_b64"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let b64 = need_str(input, "bytes_b64").unwrap_or_default();
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())?;
            c.pty_input(&sid, bytes).await
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "bytes_b64"
);

simple_write_tool!(
    InterruptSession,
    "agentd_interrupt_session",
    "Send an interrupt (Ctrl-C semantics) to a session.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.interrupt(&sid).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    StopSession,
    "agentd_stop_session",
    "Ask a session to wind down cleanly.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.stop(&sid).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    KillSession,
    "agentd_kill_session",
    "Kill a session immediately (SIGKILL the adapter).",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.kill(&sid).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    DeleteSession,
    "agentd_delete_session",
    "Delete a session — kills it if alive, drops its transcript + worktree.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.delete(&sid).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    PinSession,
    "agentd_pin_session",
    "Toggle the pinned flag on a session (controls the TUI pin strip).",
    vec![("pinned", json!({ "type": "boolean" }))],
    json!(["session_id", "pinned"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let pinned = input.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false);
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.set_pinned(&sid, pinned).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    RenameSession,
    "agentd_rename_session",
    "Set the session's user-facing title (or clear it by omitting `title`).",
    vec![("title", json!({ "type": "string" }))],
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let title = input.get("title").and_then(|s| s.as_str()).map(|s| s.to_string());
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.set_title(&sid, title).await }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "title"
);
