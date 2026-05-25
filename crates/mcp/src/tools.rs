//! MCP tool catalog and dispatchers. Each tool wraps one or more methods
//! on `agentd_client::Client`.

use agentd_client::Client;
use agentd_protocol::{
    agent_context, CreateSessionParams, PtySize, SessionEvent, UiPanel, UiPlacement,
};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

mod browser;

/// Static tool catalog returned by `tools/list`.
pub fn catalog() -> Vec<Value> {
    let mut tools = vec![
        // ----- Read -----
        tool(
            agent_context::TOOL_NAME,
            agent_context::TOOL_DESCRIPTION,
            schema_empty(),
        ),
        tool(
            "agentd_whoami",
            "Returns the AGENTD_SESSION_ID env var visible to this MCP server, which is the agentd session id that the calling agent is running inside. Returns null if unset (the MCP server is running outside an agentd-managed session).",
            schema_empty(),
        ),
        tool(
            "agentd_list_sessions",
            "List every agentd session known to the daemon (running and finished, ungrouped and grouped). Returns an array of session summaries. Each entry includes `last_pty_at_ms` (Unix epoch ms of the latest PTY byte — use `now - last_pty_at_ms < ~600ms` to tell whether the session looks busy) and, when the session belongs to a group, `group_id` and `group_name`.",
            schema_empty(),
        ),
        tool(
            "agentd_list_harnesses",
            "List the available agent harnesses (shell, claude, codex, …). Each entry includes whether the binary was resolvable on this host.",
            schema_empty(),
        ),
        tool(
            "agentd_get_session",
            "Fetch the full detail (summary + structured transcript) for one session.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_get_transcript",
            "Fetch a slice of the session's structured event log. `from` is a 1-based sequence number; `limit` bounds the returned events.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "from":       { "type": "integer", "minimum": 0 },
                    "limit":      { "type": "integer", "minimum": 1 }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "agentd_get_output",
            "Fetch the session's recent PTY scrollback as text (UTF-8 lossy). Use this to read what's on the screen of a PTY-backed session.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_get_diff",
            "`git diff HEAD` for the session's worktree (or its cwd if it's a git repo without an isolated worktree). Empty string if not a git repo.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_get_tasks",
            "List the session's tool-call task registry: running, backgrounded, and recently-completed entries. Each entry includes call_id, tool, args_summary, state, started_at_ms, optionally backgrounded_at_ms / ended_at_ms / output_preview. Use this to discover what a session is currently working on, including long-running tools that have been auto-promoted to the background.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        // ----- Write -----
        tool(
            "agentd_create_session",
            "Spawn a new session. `harness` must match an available harness name (see agentd_list_harnesses). `cwd` defaults to the daemon's process cwd. Set `worktree:true` to start the session in an isolated git worktree.",
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
            }),
        ),
        tool(
            "agentd_send_input",
            "Send a line of text to a session as user input. For PTY sessions a trailing newline is added automatically.",
            schema_obj(&[
                ("session_id", "string", true),
                ("text",       "string", true),
            ]),
        ),
        tool(
            "agentd_send_keys",
            "Send raw bytes to a PTY-backed session (base64-encoded). Use this for control characters or arrow keys — e.g. `\\u0003` (C-c) base64 = \"Aw==\".",
            schema_obj(&[
                ("session_id", "string", true),
                ("bytes_b64",  "string", true),
            ]),
        ),
        tool(
            "agentd_ui_create",
            "Create or replace a dynamic session UI panel. The panel body is safe agentd-markdown: normal markdown plus action links like `[Run checks](agentd:action/run-checks)`. Use for compact task status, checklists, decisions, and command decks when it materially helps the user.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session. Defaults to the caller's AGENTD_SESSION_ID when omitted." },
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "placement": { "type": "string", "enum": ["sticky", "inline"] },
                    "markdown": { "type": "string" }
                },
                "required": ["id", "markdown"]
            }),
        ),
        tool(
            "agentd_ui_patch",
            "Patch a dynamic session UI panel. For the MVP this replaces `markdown` and/or `title` on an existing panel id while preserving other fields.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Target session. Defaults to the caller's AGENTD_SESSION_ID when omitted." },
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "placement": { "type": "string", "enum": ["sticky", "inline"] },
                    "markdown": { "type": "string", "description": "Optional full markdown replacement." },
                    "find": { "type": "string", "description": "Optional exact substring to replace in the current/new markdown. Must occur exactly once." },
                    "replace": { "type": "string", "description": "Replacement text for `find`; defaults to empty string when `find` is set." }
                },
                "required": ["id"]
            }),
        ),
        tool(
            "agentd_ui_delete",
            "Delete a dynamic session UI panel by id. Defaults to the caller's session when `session_id` is omitted.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "id": { "type": "string" }
                },
                "required": ["id"]
            }),
        ),
        tool(
            "agentd_interrupt_session",
            "Send an interrupt to the session (the adapter decides the exact semantic: usually SIGINT-equivalent to the running child).",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_stop_session",
            "Stop the session cleanly (asks the adapter to shut down). Use kill_session for hard kill.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_kill_session",
            "SIGKILL the adapter (and its child). The session record stays; use delete_session to also drop the record.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_delete_session",
            "Delete a session entirely: kill if running, remove transcript, worktree, and metadata from disk.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "agentd_pin_session",
            "Pin or unpin a session so it shows as a live tile in the TUI pin strip.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "pinned":     { "type": "boolean" }
                },
                "required": ["session_id", "pinned"]
            }),
        ),
        tool(
            "agentd_rename_session",
            "Set a user-facing title for the session. Empty or omitted `title` clears it.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "title":      { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "agentd_set_session_group",
            "Move a session into a group, or ungroup it. Omit `group_id` (or pass null) \
             to remove the session from its current group. `position` is `top` or `bottom` \
             of the target region (default `bottom`).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "group_id":   { "type": ["string", "null"] },
                    "position":   { "type": "string", "enum": ["top", "bottom"] }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "agentd_move_session",
            "Reorder a session within its current region — `direction` `up` swaps with the \
             session above (or moves into the previous group/ungrouped region when at the top \
             of its region); `down` is symmetric.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "direction":  { "type": "string", "enum": ["up", "down"] }
                },
                "required": ["session_id", "direction"]
            }),
        ),
        tool(
            "agentd_subagent_create",
            "Create a subagent owned by the current agentd session. The subagent is backed by any \
             registered harness and appears nested under the parent session in clients.",
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
            }),
        ),
        tool(
            "agentd_subagent_list",
            "List subagents owned by the current agentd session, including current backing \
             session summaries.",
            schema_empty(),
        ),
        tool(
            "agentd_subagent_peek",
            "Peek at a subagent's current output. PTY-backed subagents return recent scrollback \
             text; headless subagents return a tail of structured events.",
            json!({
                "type": "object",
                "properties": {
                    "subagent_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Event tail size for non-PTY subagents; default 20." }
                },
                "required": ["subagent_id"]
            }),
        ),
        tool(
            "agentd_subagent_enqueue",
            "Enqueue a user message/input line for a subagent owned by the current session.",
            schema_obj(&[
                ("subagent_id", "string", true),
                ("text",        "string", true),
            ]),
        ),
        tool(
            "agentd_subagent_cancel",
            "Interrupt a subagent's current turn/task without deleting the subagent.",
            schema_obj(&[("subagent_id", "string", true)]),
        ),
        tool(
            "agentd_subagent_delete",
            "Delete a subagent owned by the current session.",
            schema_obj(&[("subagent_id", "string", true)]),
        ),
    ];
    tools.extend(browser::catalog());
    // Dev-only: hot-reload the daemon's web UI from a worktree's assets.
    // Only advertised in debug builds so it never appears in production.
    #[cfg(debug_assertions)]
    tools.push(tool(
        "webui_hot_reload",
        "Dev-only. Point the running agentd daemon's web UI at a directory of assets \
         (typically `<worktree>/crates/daemon/assets`) so edits to `index.html` / static files \
         show up on browser refresh — a live-reload poller is injected so the page reloads on \
         save. Pass `dir: null` (or omit) to revert to the embedded assets. Lets a dev session \
         iterate on the web UI against an already-running daemon with no rebuild or restart.",
        json!({
            "type": "object",
            "properties": { "dir": { "type": "string" } }
        }),
    ));
    tools
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

fn schema_empty() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn schema_obj(fields: &[(&str, &str, bool)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (n, ty, req) in fields {
        props.insert(n.to_string(), json!({ "type": ty }));
        if *req {
            required.push(n.to_string());
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// Dispatch a `tools/call` to the right method. Returns the full
/// `tools/call` response payload (a `{content: [...], isError?}` object).
pub async fn call(client: &Arc<Client>, session_id: Option<&str>, params: Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing tool name"))?
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    if matches!(
        name.as_str(),
        "browser_open" | "browser_inspect" | "browser_screenshot" | "browser_eval"
    ) {
        let result_json = browser::call(client.clone(), session_id, name.as_str(), args).await?;
        let text = serde_json::to_string_pretty(&result_json)?;
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }));
    }

    let result_json: Value = match name.as_str() {
        // ----- Read -----
        agent_context::TOOL_NAME => serde_json::to_value(agent_context::build_from_env())?,
        "agentd_whoami" => json!({ "session_id": session_id }),
        "agentd_list_sessions" => {
            // Enrich each summary with its group name so callers don't need
            // a separate list_groups round-trip. `last_pty_at_ms` is already
            // part of SessionSummary.
            let sessions = client.list().await?;
            let groups = client.list_groups().await.unwrap_or_default();
            let group_name_by_id: HashMap<&str, &str> = groups
                .iter()
                .map(|g| (g.id.as_str(), g.name.as_str()))
                .collect();
            let enriched: Vec<Value> = sessions
                .iter()
                .map(|s| {
                    let mut v = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
                    if let (Some(gid), Value::Object(map)) = (s.group_id.as_deref(), &mut v) {
                        if let Some(name) = group_name_by_id.get(gid) {
                            map.insert("group_name".into(), json!(name));
                        }
                    }
                    v
                })
                .collect();
            Value::Array(enriched)
        }
        "agentd_list_harnesses" => serde_json::to_value(client.harnesses().await?)?,
        "agentd_get_session" => {
            let sid = arg_str(&args, "session_id")?;
            serde_json::to_value(client.get(&sid).await?)?
        }
        "agentd_get_transcript" => {
            let sid = arg_str(&args, "session_id")?;
            let from = arg_u64(&args, "from").unwrap_or(0);
            let limit = arg_usize(&args, "limit");
            serde_json::to_value(client.transcript(&sid, from, limit).await?)?
        }
        "agentd_get_output" => {
            let sid = arg_str(&args, "session_id")?;
            let snap = client.pty_replay(&sid).await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&snap.data)
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            json!({ "text": text, "size": snap.size })
        }
        "agentd_get_diff" => {
            let sid = arg_str(&args, "session_id")?;
            serde_json::to_value(client.diff(&sid).await?)?
        }
        "agentd_get_tasks" => {
            let sid = arg_str(&args, "session_id")?;
            let tasks = client.list_tasks(&sid).await?;
            json!({ "tasks": tasks })
        }
        // ----- Write -----
        "agentd_create_session" => {
            let harness = arg_str(&args, "harness")?;
            let cwd = arg_str(&args, "cwd").unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            let params = CreateSessionParams {
                harness,
                cwd,
                prompt: arg_str(&args, "prompt").ok(),
                model: None,
                title: arg_str(&args, "title").ok(),
                mode: arg_str(&args, "mode").ok(),
                pty_size: Some(PtySize {
                    cols: 100,
                    rows: 30,
                }),
                worktree: args
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                env: Default::default(),
                args: Vec::new(),
                kind: agentd_protocol::SessionKind::User,
                parent_session_id: None,
                group_id: arg_str(&args, "group_id").ok(),
            };
            let sid = client.create(params).await?;
            json!({ "session_id": sid })
        }
        "agentd_send_input" => {
            let sid = arg_str(&args, "session_id")?;
            let text = arg_str(&args, "text")?;
            client.send_input(&sid, text).await?;
            json!({ "ok": true })
        }
        "agentd_send_keys" => {
            let sid = arg_str(&args, "session_id")?;
            let b64 = arg_str(&args, "bytes_b64")?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())?;
            client.pty_input(&sid, bytes).await?;
            json!({ "ok": true })
        }
        "agentd_ui_create" => {
            let sid = target_session_id(session_id, &args)?;
            let panel = ui_panel_from_args(&args, None)?;
            client
                .emit_event(&sid, SessionEvent::UiPanel(panel.clone()))
                .await?;
            json!({ "ok": true, "session_id": sid, "panel": panel })
        }
        "agentd_ui_patch" => {
            let sid = target_session_id(session_id, &args)?;
            let panel = ui_panel_from_args(&args, Some(client.get(&sid).await?))?;
            client
                .emit_event(&sid, SessionEvent::UiPanel(panel.clone()))
                .await?;
            json!({ "ok": true, "session_id": sid, "panel": panel })
        }
        "agentd_ui_delete" => {
            let sid = target_session_id(session_id, &args)?;
            let id = arg_str(&args, "id")?;
            client
                .emit_event(&sid, SessionEvent::UiDelete { id: id.clone() })
                .await?;
            json!({ "ok": true, "session_id": sid, "id": id })
        }
        "agentd_interrupt_session" => {
            client.interrupt(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "agentd_stop_session" => {
            client.stop(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "agentd_kill_session" => {
            client.kill(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "agentd_delete_session" => {
            client.delete(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "agentd_pin_session" => {
            let sid = arg_str(&args, "session_id")?;
            let pinned = args
                .get("pinned")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| anyhow!("missing or non-bool `pinned`"))?;
            client.set_pinned(&sid, pinned).await?;
            json!({ "ok": true })
        }
        "agentd_rename_session" => {
            let sid = arg_str(&args, "session_id")?;
            let title = arg_str(&args, "title")
                .ok()
                .filter(|s| !s.trim().is_empty());
            client.set_title(&sid, title).await?;
            json!({ "ok": true })
        }
        "agentd_set_session_group" => {
            let sid = arg_str(&args, "session_id")?;
            let group_id = match args.get("group_id") {
                Some(serde_json::Value::Null) | None => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(anyhow!("`group_id` must be a string or null")),
            };
            let position = match args
                .get("position")
                .and_then(|v| v.as_str())
                .unwrap_or("bottom")
            {
                "top" => agentd_protocol::SessionGroupPosition::Top,
                "bottom" => agentd_protocol::SessionGroupPosition::Bottom,
                other => {
                    return Err(anyhow!(
                        "`position` must be \"top\" or \"bottom\", got {other:?}"
                    ))
                }
            };
            client.set_session_group(&sid, group_id, position).await?;
            json!({ "ok": true })
        }
        "agentd_move_session" => {
            let sid = arg_str(&args, "session_id")?;
            let direction = match arg_str(&args, "direction")?.as_str() {
                "up" => agentd_protocol::MoveDirection::Up,
                "down" => agentd_protocol::MoveDirection::Down,
                other => {
                    return Err(anyhow!(
                        "`direction` must be \"up\" or \"down\", got {other:?}"
                    ))
                }
            };
            client.move_session(&sid, direction).await?;
            json!({ "ok": true })
        }
        "agentd_subagent_create" => {
            let parent_id = require_session_id(session_id)?;
            let harness = arg_str(&args, "harness")?;
            let cwd = arg_str(&args, "cwd").unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            let mut env = HashMap::new();
            env.insert("AGENTD_PARENT_SESSION_ID".to_string(), parent_id.clone());
            let params = CreateSessionParams {
                title: arg_str(&args, "title")
                    .ok()
                    .or_else(|| Some(format!("subagent:{harness}"))),
                harness,
                cwd,
                prompt: arg_str(&args, "prompt").ok(),
                model: None,
                mode: Some(arg_str(&args, "mode").unwrap_or_else(|_| "headless".to_string())),
                pty_size: Some(PtySize {
                    cols: 100,
                    rows: 30,
                }),
                worktree: args
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                env,
                args: Vec::new(),
                kind: agentd_protocol::SessionKind::Subagent,
                parent_session_id: Some(parent_id),
                group_id: None,
            };
            let sid = client.create(params).await?;
            json!({ "subagent_id": sid })
        }
        "agentd_subagent_list" => {
            let parent_id = require_session_id(session_id)?;
            let subagents: Vec<_> = client
                .list()
                .await?
                .into_iter()
                .filter(|s| {
                    s.kind == agentd_protocol::SessionKind::Subagent
                        && s.parent_session_id.as_deref() == Some(parent_id.as_str())
                })
                .collect();
            json!({ "subagents": subagents })
        }
        "agentd_subagent_peek" => {
            let sid = arg_str(&args, "subagent_id")?;
            let detail = owned_subagent_detail(client, session_id, &sid).await?;
            if detail.summary.has_pty {
                let snap = client.pty_replay(&sid).await?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&snap.data)
                    .unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes).to_string();
                json!({
                    "summary": detail.summary,
                    "output": text,
                })
            } else {
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let start = detail.events.len().saturating_sub(limit);
                let events = detail.events[start..].to_vec();
                json!({
                    "summary": detail.summary,
                    "events": events,
                })
            }
        }
        "agentd_subagent_enqueue" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.send_input(&sid, arg_str(&args, "text")?).await?;
            json!({ "ok": true })
        }
        "agentd_subagent_cancel" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.interrupt(&sid).await?;
            json!({ "ok": true })
        }
        "agentd_subagent_delete" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.delete(&sid).await?;
            json!({ "ok": true })
        }
        "webui_hot_reload" => {
            let dir = args.get("dir").and_then(|v| v.as_str()).map(String::from);
            let res = client.dev_set_assets(dir).await?;
            json!({ "dir": res.dir, "embedded": res.dir.is_none() })
        }
        other => return Err(anyhow!("unknown tool: {other}")),
    };

    // Per MCP, `tools/call` returns a `content` array. Surface the JSON
    // result as a single text block — the LLM parses it.
    let text = serde_json::to_string_pretty(&result_json)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

fn arg_str(args: &Value, name: &str) -> Result<String> {
    args.get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing or non-string `{name}`"))
}

fn arg_u64(args: &Value, name: &str) -> Result<u64> {
    args.get(name)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("missing or non-integer `{name}`"))
}

fn arg_usize(args: &Value, name: &str) -> Option<usize> {
    args.get(name).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn target_session_id(session_id: Option<&str>, args: &Value) -> Result<String> {
    arg_str(args, "session_id").or_else(|_| {
        session_id
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("missing `session_id` and AGENTD_SESSION_ID is unset"))
    })
}

fn parse_ui_placement(args: &Value, default: UiPlacement) -> UiPlacement {
    match args.get("placement").and_then(|v| v.as_str()).unwrap_or("") {
        "inline" => UiPlacement::Inline,
        "sticky" => UiPlacement::Sticky,
        _ => default,
    }
}

fn ui_panel_from_args(
    args: &Value,
    existing_detail: Option<agentd_protocol::SessionDetail>,
) -> Result<UiPanel> {
    let id = arg_str(args, "id")?;
    let existing = existing_detail.and_then(|detail| {
        detail
            .events
            .into_iter()
            .fold(None, |current, ev| match ev.event {
                SessionEvent::UiPanel(panel) if panel.id == id => Some(panel),
                SessionEvent::UiDelete { id: deleted } if deleted == id => None,
                _ => current,
            })
    });
    let title = arg_str(args, "title")
        .ok()
        .or_else(|| existing.as_ref().and_then(|p| p.title.clone()));
    let placement = parse_ui_placement(
        args,
        existing.as_ref().map(|p| p.placement).unwrap_or_default(),
    );
    let mut markdown = arg_str(args, "markdown")
        .ok()
        .or_else(|| existing.as_ref().map(|p| p.markdown.clone()))
        .ok_or_else(|| anyhow!("missing `markdown` and no existing panel `{id}` to patch"))?;
    apply_search_replace_patch(args, &mut markdown)?;
    Ok(UiPanel {
        id,
        source: None,
        title,
        placement,
        markdown,
    })
}

fn apply_search_replace_patch(args: &Value, markdown: &mut String) -> Result<()> {
    let Some(find) = args.get("find").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let replace = args.get("replace").and_then(|v| v.as_str()).unwrap_or("");
    let count = markdown.matches(find).count();
    match count {
        0 => Err(anyhow!("`find` did not match panel markdown")),
        1 => {
            *markdown = markdown.replacen(find, replace, 1);
            Ok(())
        }
        _ => Err(anyhow!(
            "`find` matched panel markdown {count} times; refine the search text"
        )),
    }
}

fn require_session_id(session_id: Option<&str>) -> Result<String> {
    session_id
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("subagent tools require AGENTD_SESSION_ID"))
}

async fn owned_subagent_detail(
    client: &Arc<Client>,
    session_id: Option<&str>,
    subagent_id: &str,
) -> Result<agentd_protocol::SessionDetail> {
    let parent_id = require_session_id(session_id)?;
    let detail = client.get(subagent_id).await?;
    if detail.summary.kind != agentd_protocol::SessionKind::Subagent {
        return Err(anyhow!("{subagent_id} is not a subagent"));
    }
    if detail.summary.parent_session_id.as_deref() != Some(parent_id.as_str()) {
        return Err(anyhow!("{subagent_id} is not owned by this session"));
    }
    Ok(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{ipc_method, SessionKind, SessionState};
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn catalog_includes_browser_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in [
            "browser_open",
            "browser_inspect",
            "browser_screenshot",
            "browser_eval",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn catalog_includes_agentd_context_tool() {
        let tools = catalog();
        let context = tools
            .iter()
            .find(|tool| tool.get("name").and_then(|name| name.as_str()) == Some("agentd_context"))
            .expect("missing agentd_context");

        assert!(context
            .get("description")
            .and_then(|description| description.as_str())
            .unwrap_or_default()
            .contains("Call this before starting any user task"));
    }

    #[test]
    fn catalog_includes_ui_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in ["agentd_ui_create", "agentd_ui_patch", "agentd_ui_delete"] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn catalog_includes_subagent_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in [
            "agentd_subagent_create",
            "agentd_subagent_list",
            "agentd_subagent_peek",
            "agentd_subagent_enqueue",
            "agentd_subagent_cancel",
            "agentd_subagent_delete",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn webui_hot_reload_tool_is_debug_only() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        // Tests run in debug, so the dev-only tool is advertised here;
        // it's `#[cfg(debug_assertions)]`-gated out of release builds.
        assert_eq!(names.contains("webui_hot_reload"), cfg!(debug_assertions));
    }

    #[test]
    fn ui_panel_from_args_applies_search_replace_patch() {
        let detail = agentd_protocol::SessionDetail {
            summary: subagent_summary("s1", "parent", false),
            events: vec![agentd_protocol::TimestampedEvent {
                seq: 1,
                at: "2026-05-24T00:00:00Z".parse().expect("timestamp"),
                event: SessionEvent::UiPanel(UiPanel {
                    id: "task".into(),
                    source: None,
                    title: Some("Task".into()),
                    placement: UiPlacement::Sticky,
                    markdown: "- [~] Implement\n- [ ] Test".into(),
                }),
            }],
            ui_panels: Vec::new(),
        };
        let panel = ui_panel_from_args(
            &json!({
                "id": "task",
                "find": "- [~] Implement\n- [ ] Test",
                "replace": "- [x] Implement\n- [~] Test"
            }),
            Some(detail),
        )
        .expect("patch panel");
        assert_eq!(panel.markdown, "- [x] Implement\n- [~] Test");
    }

    #[test]
    fn ui_panel_from_args_patches_existing_markdown() {
        let detail = agentd_protocol::SessionDetail {
            summary: subagent_summary("s1", "parent", false),
            events: vec![agentd_protocol::TimestampedEvent {
                seq: 1,
                at: "2026-05-24T00:00:00Z".parse().expect("timestamp"),
                event: SessionEvent::UiPanel(UiPanel {
                    id: "task".into(),
                    source: None,
                    title: Some("Old".into()),
                    placement: UiPlacement::Sticky,
                    markdown: "old".into(),
                }),
            }],
            ui_panels: Vec::new(),
        };
        let panel = ui_panel_from_args(&json!({ "id": "task", "markdown": "new" }), Some(detail))
            .expect("patch panel");
        assert_eq!(panel.id, "task");
        assert_eq!(panel.title.as_deref(), Some("Old"));
        assert_eq!(panel.markdown, "new");
    }

    #[tokio::test]
    async fn subagent_tools_flow_through_mcp_with_parent_scope() {
        let dir =
            std::env::temp_dir().join(format!("agentd-mcp-subagent-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        let sock = dir.join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut created = subagent_summary("ssub", "sparent", false);
            loop {
                let raw = match agentd_protocol::transport::read_message(&mut reader).await {
                    Ok(Some(raw)) => raw,
                    _ => break,
                };
                let id = raw.get("id").cloned().unwrap_or_else(|| json!(0));
                let method = raw.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
                let result = match method {
                    ipc_method::SESSION_CREATE => {
                        let p: CreateSessionParams =
                            serde_json::from_value(params).expect("create params");
                        assert_eq!(p.kind, SessionKind::Subagent);
                        assert_eq!(p.parent_session_id.as_deref(), Some("sparent"));
                        assert_eq!(
                            p.env.get("AGENTD_PARENT_SESSION_ID").map(String::as_str),
                            Some("sparent")
                        );
                        assert_eq!(p.mode.as_deref(), Some("headless"));
                        assert_eq!(p.harness, "codex");
                        json!({ "session_id": "ssub" })
                    }
                    ipc_method::SESSION_LIST => json!([
                        created.clone(),
                        subagent_summary("sother", "other-parent", false),
                    ]),
                    ipc_method::SESSION_GET => {
                        let sid = params
                            .get("session_id")
                            .and_then(|s| s.as_str())
                            .expect("session_id");
                        let summary = match sid {
                            "ssub" => created.clone(),
                            "sother" => subagent_summary("sother", "other-parent", false),
                            other => panic!("unexpected get {other}"),
                        };
                        json!({
                            "summary": summary,
                            "events": [
                                {
                                    "seq": 1,
                                    "at": "2026-05-24T00:00:00Z",
                                    "event": {
                                        "type": "message",
                                        "role": "assistant",
                                        "text": "done"
                                    }
                                }
                            ]
                        })
                    }
                    ipc_method::SESSION_INPUT => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        assert_eq!(
                            params.get("text").and_then(|s| s.as_str()),
                            Some("continue")
                        );
                        json!(null)
                    }
                    ipc_method::SESSION_INTERRUPT => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        json!(null)
                    }
                    ipc_method::SESSION_DELETE => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        created.state = SessionState::Done;
                        json!(null)
                    }
                    other => panic!("unexpected method {other}"),
                };
                let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let line = resp.to_string() + "\n";
                writer.write_all(line.as_bytes()).await.expect("write resp");
            }
        });

        let client = Client::connect(&sock).await.expect("connect mock daemon");
        let created = call_tool(
            &client,
            Some("sparent"),
            "agentd_subagent_create",
            json!({
                "harness": "codex",
                "prompt": "summarize",
            }),
        )
        .await;
        assert_eq!(created["subagent_id"], "ssub");

        let listed = call_tool(&client, Some("sparent"), "agentd_subagent_list", json!({})).await;
        assert_eq!(listed["subagents"].as_array().expect("subagents").len(), 1);
        assert_eq!(listed["subagents"][0]["id"], "ssub");

        let peeked = call_tool(
            &client,
            Some("sparent"),
            "agentd_subagent_peek",
            json!({ "subagent_id": "ssub" }),
        )
        .await;
        assert_eq!(peeked["events"][0]["event"]["text"], "done");

        call_tool(
            &client,
            Some("sparent"),
            "agentd_subagent_enqueue",
            json!({ "subagent_id": "ssub", "text": "continue" }),
        )
        .await;
        call_tool(
            &client,
            Some("sparent"),
            "agentd_subagent_cancel",
            json!({ "subagent_id": "ssub" }),
        )
        .await;
        call_tool(
            &client,
            Some("sparent"),
            "agentd_subagent_delete",
            json!({ "subagent_id": "ssub" }),
        )
        .await;

        let blocked = call(
            &client,
            Some("sparent"),
            json!({
                "name": "agentd_subagent_peek",
                "arguments": { "subagent_id": "sother" }
            }),
        )
        .await
        .expect_err("ownership error should fail direct dispatcher calls");
        assert!(
            blocked.to_string().contains("not owned by this session"),
            "unexpected error: {blocked}"
        );

        drop(client);
        server.await.expect("server task");
    }

    async fn call_tool(
        client: &Arc<Client>,
        session_id: Option<&str>,
        name: &str,
        arguments: Value,
    ) -> Value {
        let response = call(
            client,
            session_id,
            json!({ "name": name, "arguments": arguments }),
        )
        .await
        .expect("tool call");
        assert_eq!(response["isError"], false, "{response:?}");
        let text = response["content"][0]["text"]
            .as_str()
            .expect("text result");
        serde_json::from_str(text).expect("json tool result")
    }

    fn subagent_summary(id: &str, parent: &str, has_pty: bool) -> agentd_protocol::SessionSummary {
        agentd_protocol::SessionSummary {
            id: id.to_string(),
            harness: "codex".to_string(),
            cwd: "/tmp".to_string(),
            title: None,
            state: SessionState::Running,
            created_at: "2026-05-24T00:00:00Z".parse().expect("timestamp"),
            last_event_at: None,
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 1,
            has_pty,
            mode: Some("headless".to_string()),
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: Some(parent.to_string()),
            last_pty_at_ms: None,
            automode: false,
            kind: SessionKind::Subagent,
        }
    }
}
