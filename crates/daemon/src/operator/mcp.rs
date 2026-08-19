//! Minimal MCP client for channel backends.
//!
//! A slack-personal channel reaches Slack through Slack's hosted MCP server.
//! The daemon owns the fixed endpoint and OAuth proxy setup (spec 0201); the
//! user never supplies a shell command. The daemon is a classic program, not
//! an agent, so this client speaks the protocol's plain JSON-RPC framing over
//! stdio: `initialize`, `notifications/initialized`, then `tools/call`.
//! Nothing model-shaped is involved, and no tool schema discovery is needed —
//! the channel knows exactly which contract tools it calls.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const SLACK_MCP_URL: &str = "https://mcp.slack.com/mcp";
const SLACK_MCP_REMOTE_PACKAGE: &str = "mcp-remote@0.1.38";
// Slack publishes this desktop OAuth client in its official MCP plugin. The
// corresponding redirect is localhost:3118 and authorization uses PKCE.
const SLACK_MCP_CLIENT_ID: &str = "1601185624273.8899143856786";
const SLACK_MCP_CALLBACK_PORT: &str = "3118";
const SLACK_MCP_SCOPES: &str = concat!(
    "search:read.public search:read.private search:read.im search:read.mpim ",
    "channels:history channels:read groups:history groups:read ",
    "im:history im:read mpim:history mpim:read chat:write"
);

/// Ceiling on one response line. A backend that streams megabytes into a tool
/// result is misbehaving; bounding the read keeps it from ballooning memory.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// How long one request may wait for its response.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) fn slack_auth_dir() -> PathBuf {
    construct_protocol::paths::Paths::discover()
        .data_dir
        .join("operators")
        .join("slack-mcp-auth")
}

/// Whether mcp-remote has persisted a non-empty OAuth token record. This is a
/// storage fact, not a connectivity claim: the token may still need refresh or
/// workspace-admin approval when the backend next starts.
pub(crate) fn slack_oauth_credentials_saved() -> bool {
    oauth_tokens_saved_under(&slack_auth_dir(), 4)
}

fn oauth_tokens_saved_under(directory: &Path, remaining_depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(file_type) = entry.file_type() else {
            return false;
        };
        if file_type.is_dir() && remaining_depth > 0 {
            return oauth_tokens_saved_under(&entry.path(), remaining_depth - 1);
        }
        file_type.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with("_tokens.json"))
            && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
    })
}

/// Requests waiting on a response, or `None` once the backend's stream has
/// ended — after which registering a new request must fail immediately
/// instead of waiting out a timeout nobody will answer.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Value>>>>>;

/// One connected MCP server. Dropping the client drops its writer, which
/// closes the server's stdin — the conventional stdio-server shutdown signal —
/// and the spawned child, if any, is killed on drop as a backstop.
pub(crate) struct McpClient {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Held only so the child dies with the client.
    _child: Option<tokio::process::Child>,
    reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        if let Some(child) = self._child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

impl McpClient {
    /// Start Construct's built-in Slack MCP backend. `mcp-remote` owns the
    /// first-run browser OAuth flow and refresh-token persistence; pinning its
    /// package and every argument here keeps channel configuration free of
    /// executable input.
    pub(crate) async fn spawn_slack() -> Result<Self> {
        let auth_dir = slack_auth_dir();
        tokio::fs::create_dir_all(&auth_dir)
            .await
            .with_context(|| format!("create Slack MCP auth directory {}", auth_dir.display()))?;
        let client_info = format!(r#"{{"client_id":"{SLACK_MCP_CLIENT_ID}"}}"#);
        let client_metadata = format!(r#"{{"scope":"{SLACK_MCP_SCOPES}"}}"#);
        let mut command = tokio::process::Command::new("npx");
        command
            .args([
                "-y",
                SLACK_MCP_REMOTE_PACKAGE,
                SLACK_MCP_URL,
                SLACK_MCP_CALLBACK_PORT,
                "--transport",
                "http-only",
                "--auth-timeout",
                "300",
                "--static-oauth-client-info",
                &client_info,
                "--static-oauth-client-metadata",
                &client_metadata,
            ])
            .env("MCP_REMOTE_CONFIG_DIR", auth_dir);
        Self::spawn_process(command, "Slack MCP backend").await
    }

    /// Spawn `command` with the user's shell and complete the MCP handshake.
    #[cfg(test)]
    pub(crate) async fn spawn(command: &str) -> Result<Self> {
        let mut process = tokio::process::Command::new("/bin/sh");
        process.arg("-c").arg(command);
        Self::spawn_process(process, "test MCP backend").await
    }

    async fn spawn_process(mut command: tokio::process::Command, label: &str) -> Result<Self> {
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Backends log to stderr; letting it flow to the daemon's own
            // stderr keeps their diagnostics findable without parsing them.
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {label}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("MCP backend has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP backend has no stdout"))?;
        Self::connect(stdout, stdin, Some(child)).await
    }

    /// Complete the handshake over an arbitrary byte stream. Tests connect an
    /// in-process fake server through a duplex pipe with exactly this entry.
    pub(crate) async fn connect<R, W>(reader: R, writer: W, child: Option<tokio::process::Child>) -> Result<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let reader_pending = pending.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::with_capacity(64 * 1024, reader).split(b'\n');
            loop {
                let line = match lines.next_segment().await {
                    Ok(Some(line)) if line.len() <= MAX_LINE_BYTES => line,
                    Ok(Some(_)) => {
                        tracing::warn!("MCP backend response exceeded the line cap; dropping it");
                        continue;
                    }
                    _ => break,
                };
                let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                    // Backends occasionally print banners to stdout before
                    // speaking JSON-RPC; skipping keeps the handshake alive.
                    continue;
                };
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue; // Notifications and server-initiated requests.
                };
                let waiter = reader_pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .and_then(|pending| pending.remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(message);
                }
            }
            // The stream is over: nobody still waiting will be answered, and
            // nobody may start waiting.
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        });
        let client = Self {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
            reader_task,
        };
        let response = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "construct", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .context("MCP initialize")?;
        if response.get("error").is_some() {
            return Err(anyhow!("MCP backend refused initialize: {}", response["error"]));
        }
        client
            .notify("notifications/initialized", json!({}))
            .await
            .context("MCP initialized notification")?;
        Ok(client)
    }

    /// Test helper for tools that return JSON encoded in a text block.
    #[cfg(test)]
    pub(crate) async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let value = self.call_tool_value(name, arguments).await?;
        match value {
            Value::String(text) => serde_json::from_str(&text)
                .with_context(|| format!("MCP tool `{name}` returned text that is not JSON")),
            value => Ok(value),
        }
    }

    /// Call a tool while accepting both modern `structuredContent` results
    /// and ordinary text blocks. Slack's hosted server currently returns JSON
    /// encoded as text for reads and plain text links for writes.
    pub(crate) async fn call_tool_value(&self, name: &str, arguments: Value) -> Result<Value> {
        let response = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
            .with_context(|| format!("call MCP tool `{name}`"))?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("MCP tool `{name}` failed: {error}"));
        }
        let result = response
            .get("result")
            .ok_or_else(|| anyhow!("MCP tool `{name}` returned no result"))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(anyhow!(
                "MCP tool `{name}` reported an error: {}",
                first_text(result).unwrap_or_default()
            ));
        }
        if let Some(structured) = result.get("structuredContent") {
            return Ok(structured.clone());
        }
        let text = first_text(result)
            .ok_or_else(|| anyhow!("MCP tool `{name}` returned no text content"))?;
        Ok(Value::String(text))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending
                .as_mut()
                .ok_or_else(|| anyhow!("MCP backend closed before `{method}` was sent"))?
                .insert(id, tx);
        }
        let unregister = || {
            if let Some(pending) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
            {
                pending.remove(&id);
            }
        };
        let sent = self
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        if let Err(error) = sent {
            unregister();
            return Err(error);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("MCP backend closed before answering `{method}`")),
            Err(_) => {
                unregister();
                Err(anyhow!("MCP backend timed out answering `{method}`"))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn send(&self, message: Value) -> Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&line).await.context("write to MCP backend")?;
        writer.flush().await.context("flush to MCP backend")
    }
}

fn first_text(result: &Value) -> Option<String> {
    result
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|block| {
            (block.get("type")?.as_str()? == "text")
                .then(|| block.get("text")?.as_str().map(str::to_string))
                .flatten()
        })
}

/// In-process MCP server shared by adapter tests that need to exercise the
/// real JSON-RPC client and inspect tool-call ordering.
#[cfg(test)]
pub(crate) fn fake_server(
    handler: impl Fn(&str, &Value) -> Value + Send + 'static,
) -> (
    impl AsyncRead + Send + Unpin,
    impl AsyncWrite + Send + Unpin,
) {
    let (client_reader, mut server_writer) = tokio::io::duplex(256 * 1024);
    let (server_reader, client_writer) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let mut lines = BufReader::new(server_reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = message["method"].as_str().unwrap_or_default().to_string();
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let result = match method.as_str() {
                "initialize" => {
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake"}})
                }
                "tools/call" => {
                    let name = message["params"]["name"].as_str().unwrap_or_default();
                    handler(name, &message["params"]["arguments"])
                }
                _ => json!({}),
            };
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let mut line = serde_json::to_vec(&response).unwrap();
            line.push(b'\n');
            let _ = server_writer.write_all(&line).await;
        }
    });
    (client_reader, client_writer)
}

#[cfg(test)]
pub(crate) fn text_result(body: Value) -> Value {
    json!({"content": [{"type": "text", "text": body.to_string()}]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, BufReader};

    #[test]
    fn oauth_readiness_requires_a_nonempty_persisted_token_record() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!oauth_tokens_saved_under(directory.path(), 4));

        let version = directory.path().join("0.1.38");
        std::fs::create_dir_all(&version).unwrap();
        std::fs::write(version.join("server_tokens.json"), "").unwrap();
        assert!(!oauth_tokens_saved_under(directory.path(), 4));

        std::fs::write(version.join("server_tokens.json"), r#"{"access_token":"saved"}"#)
            .unwrap();
        assert!(oauth_tokens_saved_under(directory.path(), 4));
    }

    #[test]
    fn unrelated_auth_files_do_not_report_oauth_readiness() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("client_info.json"), "{}").unwrap();
        assert!(!oauth_tokens_saved_under(directory.path(), 4));
    }

    #[tokio::test]
    async fn handshake_then_tool_call_round_trips_json() {
        let (reader, writer) = fake_server(|name, args| {
            assert_eq!(name, "slack_search_public_and_private");
            assert_eq!(args["query"], "after:2026-08-18");
            text_result(json!({"results": "none", "pagination_info": "none"}))
        });
        let client = McpClient::connect(reader, writer, None).await.expect("handshake");
        let result = client
            .call_tool(
                "slack_search_public_and_private",
                json!({"query": "after:2026-08-18"}),
            )
            .await
            .expect("tool call");
        assert_eq!(result["results"], "none");
    }

    #[tokio::test]
    async fn structured_content_is_returned_without_text_coercion() {
        let (reader, writer) = fake_server(|_, _| {
            json!({"structuredContent": {"message_link": "https://example.slack.com/archives/C1/p1234567890123456"}})
        });
        let client = McpClient::connect(reader, writer, None).await.expect("handshake");
        let result = client
            .call_tool_value("slack_send_message", json!({}))
            .await
            .expect("structured result");
        assert_eq!(result["message_link"], "https://example.slack.com/archives/C1/p1234567890123456");
    }

    #[tokio::test]
    async fn a_tool_error_is_an_error_not_a_value() {
        let (reader, writer) = fake_server(|_, _| {
            json!({"isError": true, "content": [{"type": "text", "text": "no such channel"}]})
        });
        let client = McpClient::connect(reader, writer, None).await.expect("handshake");
        let error = client
            .call_tool("slack_send_message", json!({}))
            .await
            .expect_err("isError must surface");
        assert!(error.to_string().contains("no such channel"));
    }

    #[tokio::test]
    async fn non_json_tool_text_is_rejected() {
        // The contract is JSON-in-text. A backend that answers with markdown
        // prose is not conforming, and parsing prose would invent messages.
        let (reader, writer) = fake_server(|_, _| {
            json!({"content": [{"type": "text", "text": "# Search Results\nnothing"}]})
        });
        let client = McpClient::connect(reader, writer, None).await.expect("handshake");
        assert!(client
            .call_tool("slack_search_public_and_private", json!({}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn stdout_banners_before_json_are_skipped() {
        let (client_reader, mut server_writer) = duplex(64 * 1024);
        let (server_reader, client_writer) = duplex(64 * 1024);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_reader).lines();
            // A banner the way real servers print one: before any response.
            let _ = server_writer.write_all(b"starting fake backend...\n").await;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}});
                    let mut line = serde_json::to_vec(&response).unwrap();
                    line.push(b'\n');
                    let _ = server_writer.write_all(&line).await;
                }
            }
        });
        McpClient::connect(client_reader, client_writer, None)
            .await
            .expect("handshake survives the banner");
    }

    #[tokio::test]
    async fn spawn_runs_a_real_subprocess_backend() {
        // A shell implementation of just enough of the protocol: request ids
        // are deterministic (1 = initialize, 2 = the first tool call), so a
        // line-matching loop can answer them without parsing JSON.
        let script = r#"while read line; do
            case "$line" in
              *'"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}';;
              *'"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"pong\":true}"}]}}';;
            esac
          done"#;
        let client = McpClient::spawn(script).await.expect("spawn shell backend");
        let result = client
            .call_tool("slack_search_public_and_private", json!({"query": "test"}))
            .await
            .expect("tool call over a real pipe");
        assert_eq!(result["pong"], true);
    }

    #[tokio::test]
    async fn a_backend_that_dies_fails_the_call_instead_of_hanging() {
        let (client_reader, server_writer) = duplex(1024);
        let (server_reader, client_writer) = duplex(1024);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_reader).lines();
            // Answer initialize, then close the response stream — while still
            // reading, the way a process whose stdout died but stdin lives
            // would look.
            if let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                let id = message["id"].as_u64().unwrap();
                let response = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                let mut server_writer = server_writer;
                let mut line = serde_json::to_vec(&response).unwrap();
                line.push(b'\n');
                let _ = server_writer.write_all(&line).await;
                drop(server_writer);
            }
            while let Ok(Some(_)) = lines.next_line().await {}
        });
        let client = McpClient::connect(client_reader, client_writer, None)
            .await
            .expect("handshake");
        let error = client
            .call_tool("slack_search_public_and_private", json!({}))
            .await
            .expect_err("a closed stream must fail the pending call");
        // The failure must be the fast "closed" path, never the 60s timeout.
        assert!(format!("{error:#}").contains("closed"), "{error:#}");
    }
}
