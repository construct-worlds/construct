//! Ollama `/api/chat` — NDJSON streaming, optional tool-calling on
//! capable models. Default host `http://localhost:11434`; override with
//! `OLLAMA_HOST`.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Ollama {
    client: reqwest::Client,
    base_url: String,
}

impl Ollama {
    pub fn from_env() -> Result<Self> {
        Self::with_config(std::env::var("OLLAMA_HOST").ok())
    }

    /// Build pointed at an explicit host (None → `http://localhost:11434`).
    /// Used by named `[smith.models.*]` profiles so multiple local servers
    /// can coexist, independent of `OLLAMA_HOST`.
    pub fn with_config(base_url: Option<String>) -> Result<Self> {
        let base_url = base_url
            .unwrap_or_else(|| "http://localhost:11434".to_string())
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("build reqwest client")?,
            base_url,
        })
    }

    /// Read the allocation of an already-loaded model. Ollama's model
    /// metadata exposes the architectural maximum, but `/api/ps` is the
    /// source of truth for the context length selected by this server.
    async fn loaded_context_window_tokens(&self, model: &str) -> Option<u64> {
        let url = format!("{}/api/ps", self.base_url);
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Value>()
            .await
            .ok()?;
        context_window_from_ps(&response, model)
    }

    /// Ensure the model is loaded without running inference. This pays the
    /// same load cost the imminent chat request would pay, then lets us read
    /// the server's effective context allocation before pruning history.
    async fn load_model(&self, model: &str) -> bool {
        let url = format!("{}/api/generate", self.base_url);
        self.client
            .post(url)
            .timeout(Duration::from_secs(60))
            .json(&json!({
                "model": model,
                "stream": false,
            }))
            .send()
            .await
            .ok()
            .and_then(|response| response.error_for_status().ok())
            .is_some()
    }
}

fn normalized_model_name(model: &str) -> String {
    if model.contains(':') {
        model.to_ascii_lowercase()
    } else {
        format!("{}:latest", model.to_ascii_lowercase())
    }
}

fn context_window_from_ps(response: &Value, requested_model: &str) -> Option<u64> {
    let requested = normalized_model_name(requested_model);
    response
        .get("models")?
        .as_array()?
        .iter()
        .find(|loaded| {
            ["name", "model"].iter().any(|field| {
                loaded
                    .get(field)
                    .and_then(Value::as_str)
                    .map(normalized_model_name)
                    .as_deref()
                    == Some(requested.as_str())
            })
        })?
        .get("context_length")?
        .as_u64()
        .filter(|window| *window > 0)
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn messages_to_ollama(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    for m in messages {
        match &m.content {
            Content::Text { text } => {
                out.push(json!({ "role": role_str(m.role), "content": text }));
            }
            Content::AssistantToolCalls { text, calls } => {
                let tc: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({
                            "function": {
                                "name": c.name,
                                "arguments": c.input,
                            }
                        })
                    })
                    .collect();
                let mut entry = json!({
                    "role": "assistant",
                    "content": text.clone().unwrap_or_default(),
                });
                if !tc.is_empty() {
                    entry["tool_calls"] = Value::Array(tc);
                }
                out.push(entry);
            }
            Content::ToolResult {
                call_id: _,
                output,
                is_error: _,
            } => {
                out.push(json!({
                    "role": "tool",
                    "content": output,
                }));
            }
            Content::Summary { text, .. } => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "content": body }));
            }
            // codex-oauth-only; nothing to send to Ollama.
            Content::Reasoning(_) => {}
        }
    }
    out
}

fn tools_to_ollama(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                }
            })
        })
        .collect()
}

#[async_trait]
impl LlmProvider for Ollama {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn effective_context_window_tokens(&self, model: &str) -> Option<u64> {
        if let Some(window) = self.loaded_context_window_tokens(model).await {
            return Some(window);
        }
        if !self.load_model(model).await {
            return None;
        }
        self.loaded_context_window_tokens(model).await
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let mut body = json!({
            "model": model,
            "stream": true,
            "messages": messages_to_ollama(system, messages),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools_to_ollama(tools));
        }

        let url = format!("{}/api/chat", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("ollama POST /api/chat")?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // Ollama returns 4xx for context overflow but body shape
            // varies by model. Run the parser; it only matches when
            // the body is actually overflow-shaped.
            if code.is_client_error() {
                if let Some(extracted) = super::parse_overflow(&body) {
                    return Err(anyhow::Error::new(super::ContextOverflow {
                        extracted,
                        raw: body,
                    }));
                }
            }
            return Err(anyhow!("ollama {code}: {body}"));
        }

        // NDJSON: one JSON object per line. `done: true` terminates.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        let mut assistant_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = Usage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("ollama NDJSON stream")?;
            sink.progress();
            buf.extend_from_slice(&chunk);
            // Process complete lines.
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let trimmed = match std::str::from_utf8(&line) {
                    Ok(s) => s.trim(),
                    Err(_) => continue,
                };
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(msg) = v.get("message") {
                    if let Some(t) = msg.get("content").and_then(|s| s.as_str()) {
                        if !t.is_empty() {
                            sink.delta(t);
                            assistant_text.push_str(t);
                        }
                    }
                    if let Some(calls) = msg.get("tool_calls").and_then(|a| a.as_array()) {
                        for c in calls {
                            let name = c
                                .pointer("/function/name")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = c
                                .pointer("/function/arguments")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            if !name.is_empty() {
                                tool_calls.push(ToolCall {
                                    id: format!("tool_{}", tool_calls.len()),
                                    name,
                                    input: args,
                                });
                            }
                        }
                    }
                }
                if v.get("done").and_then(|b| b.as_bool()).unwrap_or(false) {
                    if let Some(n) = v.get("prompt_eval_count").and_then(|n| n.as_u64()) {
                        usage.input_tokens = n;
                    }
                    if let Some(n) = v.get("eval_count").and_then(|n| n.as_u64()) {
                        usage.output_tokens = n;
                    }
                    if !tool_calls.is_empty() {
                        stop_reason = StopReason::ToolUse;
                    }
                }
            }
        }

        Ok(ProviderTurn {
            text: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls,
            stop_reason,
            usage,
            reasoning_items: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_context_matches_implicit_latest_name() {
        let response = json!({
            "models": [{
                "name": "qwen3.8:latest",
                "model": "qwen3.8:latest",
                "context_length": 32_768
            }]
        });
        assert_eq!(context_window_from_ps(&response, "qwen3.8"), Some(32_768));
    }

    #[test]
    fn ps_context_selects_requested_model_and_rejects_zero() {
        let response = json!({
            "models": [
                {"name": "other:latest", "context_length": 65_536},
                {"model": "qwen3.8:custom", "context_length": 0}
            ]
        });
        assert_eq!(context_window_from_ps(&response, "qwen3.8:custom"), None);
        assert_eq!(context_window_from_ps(&response, "missing"), None);
    }

    #[tokio::test]
    async fn effective_context_loads_model_then_reads_runtime_allocation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let responses = [
                ("GET /api/ps ", r#"{"models":[]}"#),
                (
                    "POST /api/generate ",
                    r#"{"model":"qwen3.8","response":"","done":true,"done_reason":"load"}"#,
                ),
                (
                    "GET /api/ps ",
                    r#"{"models":[{"name":"qwen3.8:latest","context_length":32768}]}"#,
                ),
            ];
            for (expected_request, body) in responses {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8_192];
                let read = tcp.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with(expected_request), "{request}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                tcp.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let provider = Ollama::with_config(Some(format!("http://{addr}"))).unwrap();
        assert_eq!(
            provider.effective_context_window_tokens("qwen3.8").await,
            Some(32_768)
        );
        server.await.unwrap();
    }
}
