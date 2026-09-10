//! Anthropic `/v1/messages` with SSE streaming + tool use.
//!
//! Uses the Messages API (current stable). Anthropic's tool-use loop:
//! the assistant emits `tool_use` content blocks; we respond with a
//! `tool_result` block on the next user message.
//!
//! The wire helpers ([`messages_to_anthropic`], [`tools_to_anthropic`],
//! [`read_message_stream`]) are shared with the subscription-OAuth
//! `claude-oauth` provider, which hits the same endpoint and differs only
//! in how it authenticates the request and shapes the system prompt.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

pub(crate) const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";
const CONTEXT_1M_TOKENS: u64 = 1_000_000;
const DEFAULT_CONTEXT_TOKENS: u64 = 200_000;
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Capabilities for an Anthropic endpoint. Named profiles populate these
/// fields directly; the built-in provider reads equivalent environment
/// overrides (documented in `docs/smith.md`).
#[derive(Debug, Clone, Default)]
pub struct AnthropicOptions {
    pub cache_control: Option<bool>,
    pub betas: Vec<String>,
    pub context_window_tokens: Option<u64>,
}

pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    cache_control: bool,
    betas: Vec<String>,
    context_window_tokens: Option<u64>,
    first_party: bool,
}

impl Anthropic {
    pub fn from_env() -> Result<Self> {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").map_err(|_| anyhow!("ANTHROPIC_API_KEY not set"))?;
        Self::with_options(
            std::env::var("ANTHROPIC_BASE_URL").ok(),
            options_from_env()?,
            api_key,
        )
    }

    /// Build with an explicit base URL (None → public Anthropic) and key.
    /// Used by named `[smith.models.*]` profiles.
    #[cfg(test)]
    pub fn with_config(base_url: Option<String>, api_key: String) -> Result<Self> {
        Self::with_options(base_url, AnthropicOptions::default(), api_key)
    }

    /// Build with endpoint capabilities supplied by a named model profile.
    /// Options precede the key so call sites keep credentials visually last.
    pub fn with_options(
        base_url: Option<String>,
        options: AnthropicOptions,
        api_key: String,
    ) -> Result<Self> {
        if options.context_window_tokens == Some(0) {
            anyhow::bail!("Anthropic context window must be greater than zero");
        }
        let base_url = base_url
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();
        let first_party = is_first_party_endpoint(&base_url);
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("build reqwest client")?,
            base_url,
            api_key,
            cache_control: options.cache_control.unwrap_or(first_party),
            betas: dedup_betas(options.betas),
            context_window_tokens: options.context_window_tokens,
            first_party,
        })
    }

    fn request_betas(&self, model: &str) -> Vec<String> {
        let mut betas = self.betas.clone();
        let explicitly_capped_at_default = self
            .context_window_tokens
            .is_some_and(|tokens| tokens <= DEFAULT_CONTEXT_TOKENS);
        if self.first_party
            && supports_context_1m_beta(model)
            && !explicitly_capped_at_default
            && !betas.iter().any(|beta| beta == CONTEXT_1M_BETA)
        {
            betas.push(CONTEXT_1M_BETA.to_string());
        }
        betas
    }

    fn context_window_for_model(&self, model: &str) -> Option<u64> {
        self.context_window_tokens.or_else(|| {
            self.request_betas(model)
                .iter()
                .any(|beta| beta == CONTEXT_1M_BETA)
                .then_some(CONTEXT_1M_TOKENS)
        })
    }
}

fn supports_context_1m_beta(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude-sonnet-4")
}

fn is_first_party_endpoint(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com"))
}

fn dedup_betas(betas: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for beta in betas {
        let beta = beta.trim();
        if !beta.is_empty() && !out.iter().any(|existing| existing == beta) {
            out.push(beta.to_string());
        }
    }
    out
}

fn options_from_env() -> Result<AnthropicOptions> {
    let cache_control = match std::env::var("CONSTRUCT_SMITH_ANTHROPIC_CACHE_CONTROL") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => Some(true),
            "0" | "false" | "off" | "no" => Some(false),
            other => anyhow::bail!(
                "CONSTRUCT_SMITH_ANTHROPIC_CACHE_CONTROL must be on/off (got `{other}`)"
            ),
        },
        Err(_) => None,
    };
    let betas = std::env::var("CONSTRUCT_SMITH_ANTHROPIC_BETAS")
        .ok()
        .map(|value| value.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let context_window_tokens = std::env::var("CONSTRUCT_SMITH_ANTHROPIC_CONTEXT_WINDOW_TOKENS")
        .ok()
        .map(|value| {
            value.parse::<u64>().with_context(|| {
                "CONSTRUCT_SMITH_ANTHROPIC_CONTEXT_WINDOW_TOKENS must be a positive integer"
            })
        })
        .transpose()?;
    Ok(AnthropicOptions {
        cache_control,
        betas,
        context_window_tokens,
    })
}

pub(crate) fn messages_to_anthropic(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    // Anthropic merges consecutive same-role messages on the wire.
    // We don't, but we do skip the system role (passed top-level).
    for m in messages {
        match (m.role, &m.content) {
            (Role::System, _) => {} // attached as `system` field on the request
            (_, Content::Text { text }) => {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "user", // tool_result blocks live in user messages
                    Role::System => unreachable!(),
                };
                out.push(json!({ "role": role, "content": text }));
            }
            (_, Content::UserInput { text, images }) => {
                let mut blocks = Vec::with_capacity(images.len() + 1);
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                blocks.extend(images.iter().map(|image| {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": image.media_type,
                            "data": image.data,
                        }
                    })
                }));
                out.push(json!({ "role": "user", "content": blocks }));
            }
            (_, Content::AssistantToolCalls { text, calls }) => {
                let mut blocks: Vec<Value> = Vec::with_capacity(calls.len() + 1);
                if let Some(t) = text {
                    if !t.is_empty() {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                for c in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.input,
                    }));
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            (
                _,
                Content::ToolResult {
                    call_id,
                    output,
                    is_error,
                },
            ) => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output,
                    "is_error": *is_error,
                });
                out.push(json!({ "role": "user", "content": [block] }));
            }
            (_, Content::Summary { text, .. }) => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "content": body }));
            }
            // codex-oauth-only; nothing to send to the Anthropic API.
            (_, Content::Reasoning(_)) => {}
        }
    }
    out
}

pub(crate) fn tools_to_anthropic(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            })
        })
        .collect()
}

/// Add up to Anthropic's four ephemeral cache breakpoints in stable-prefix
/// order: system, tool definitions, then the latest user-message boundaries.
/// This mutates an already valid Messages request so the same policy works for
/// API-key and Claude OAuth requests while Anthropic-compatible providers can
/// continue using the unmodified wire helpers.
pub(crate) fn apply_cache_control(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let mut remaining = MAX_CACHE_BREAKPOINTS;

    if remaining > 0 {
        if let Some(system) = object.get_mut("system") {
            if mark_content_tail(system) {
                remaining -= 1;
            }
        }
    }
    if remaining > 0 {
        if let Some(last_tool) = object
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .and_then(|tools| tools.last_mut())
        {
            if mark_object(last_tool) {
                remaining -= 1;
            }
        }
    }
    if remaining == 0 {
        return;
    }
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if message.get_mut("content").is_some_and(mark_content_tail) {
            remaining -= 1;
        }
    }
}

fn mark_content_tail(content: &mut Value) -> bool {
    if let Some(text) = content.as_str().map(str::to_owned) {
        *content = json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" },
        }]);
        return true;
    }
    content
        .as_array_mut()
        .and_then(|blocks| blocks.last_mut())
        .is_some_and(mark_object)
}

fn mark_object(value: &mut Value) -> bool {
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    object.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    true
}

/// Anthropic reports uncached, cache-write, and cache-read prompt tokens as
/// separate fields. Their sum is what occupied the model's input window; only
/// the read portion is the cached-token subset shown in cost telemetry.
fn update_input_usage(usage: &mut Usage, value: &Value) {
    let fresh = value.get("input_tokens").and_then(Value::as_u64);
    let created = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let read = value.get("cache_read_input_tokens").and_then(Value::as_u64);
    if fresh.is_some() || created.is_some() || read.is_some() {
        usage.input_tokens = Some(
            fresh
                .unwrap_or(0)
                .saturating_add(created.unwrap_or(0))
                .saturating_add(read.unwrap_or(0)),
        );
    }
    if let Some(read) = read {
        usage.cached_tokens = read;
    }
}

/// Shared handler for an Anthropic Messages API streaming response: checks
/// the HTTP status (mapping context-overflow 400s to [`super::ContextOverflow`]
/// so the agent loop's learn-and-retry path can fire), then parses the typed
/// SSE event stream into a [`ProviderTurn`]. Both the API-key `anthropic`
/// provider and the subscription-OAuth `claude-oauth` provider feed their
/// already-sent `reqwest::Response` through here.
pub(crate) async fn read_message_stream(
    resp: reqwest::Response,
    sink: &mut dyn TextSink,
) -> Result<ProviderTurn> {
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if code.as_u16() == 400 {
            if let Some(extracted) = super::parse_overflow(&body) {
                return Err(anyhow::Error::new(super::ContextOverflow {
                    extracted,
                    raw: body,
                }));
            }
        }
        return Err(anyhow!("anthropic {code}: {body}"));
    }

    let mut stream = resp.bytes_stream().eventsource();

    // Anthropic's stream uses typed events:
    //   message_start, content_block_start (text or tool_use),
    //   content_block_delta (text_delta or input_json_delta),
    //   content_block_stop, message_delta (stop_reason + usage),
    //   message_stop.
    let mut assistant_text = String::new();
    let mut blocks: Vec<BlockAcc> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = Usage::default();

    while let Some(ev) = stream.next().await {
        let ev = ev.context("anthropic SSE stream")?;
        sink.progress();
        if ev.data.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
        match ty {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    update_input_usage(&mut usage, u);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
                while blocks.len() <= idx {
                    blocks.push(BlockAcc::default());
                }
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let bty = block.get("type").and_then(|s| s.as_str()).unwrap_or("");
                match bty {
                    "tool_use" => {
                        blocks[idx].kind = BlockKind::ToolUse;
                        blocks[idx].id = block
                            .get("id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        blocks[idx].name = block
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    "thinking" => {
                        blocks[idx].kind = BlockKind::Thinking;
                    }
                    _ => {
                        blocks[idx].kind = BlockKind::Text;
                    }
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
                if idx >= blocks.len() {
                    continue;
                }
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                let dty = delta.get("type").and_then(|s| s.as_str()).unwrap_or("");
                match dty {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                sink.delta(t);
                                blocks[idx].text.push_str(t);
                                assistant_text.push_str(t);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(j) = delta.get("partial_json").and_then(|s| s.as_str()) {
                            blocks[idx].input_json.push_str(j);
                        }
                    }
                    // Extended-thinking content: stream into the
                    // sink's reasoning channel so the TUI renders
                    // it dim/italic and the headless transcript
                    // gets a separate `SessionEvent::Reasoning`.
                    // The accompanying `signature_delta` event
                    // (Anthropic-internal signature for the
                    // thinking block; not user-visible) is
                    // ignored by the catch-all below.
                    "thinking_delta" => {
                        if let Some(t) = delta.get("thinking").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                sink.reasoning_delta(t);
                                blocks[idx].text.push_str(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    stop_reason = match reason {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    };
                }
                if let Some(u) = v.pointer("/usage") {
                    if let Some(n) = u.get("output_tokens").and_then(|n| n.as_u64()) {
                        usage.output_tokens = n;
                    }
                }
            }
            _ => {}
        }
    }

    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for b in blocks {
        if matches!(b.kind, BlockKind::ToolUse) {
            let input = if b.input_json.is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&b.input_json).unwrap_or_else(|_| json!({}))
            };
            tool_calls.push(ToolCall {
                id: b.id,
                name: b.name,
                input,
            });
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

#[async_trait]
impl LlmProvider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    async fn effective_context_window_tokens(&self, model: &str) -> Option<u64> {
        self.context_window_for_model(model)
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
            "max_tokens": 8192,
            "stream": true,
            "messages": messages_to_anthropic(messages),
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools_to_anthropic(tools));
        }
        if self.cache_control {
            apply_cache_control(&mut body);
        }

        let url = format!("{}/messages", self.base_url);
        let mut request = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01");
        let betas = self.request_betas(model);
        if !betas.is_empty() {
            request = request.header("anthropic-beta", betas.join(","));
        }
        let resp = request
            .json(&body)
            .send()
            .await
            .context("anthropic POST /messages")?;
        read_message_stream(resp, sink).await
    }
}

#[derive(Default)]
struct BlockAcc {
    kind: BlockKind,
    text: String,
    id: String,
    name: String,
    input_json: String,
}

#[derive(Default)]
enum BlockKind {
    #[default]
    Text,
    ToolUse,
    /// Extended-thinking content block from Anthropic models that
    /// support reasoning (e.g. claude-3.7-sonnet thinking mode).
    /// Streamed via `thinking_delta` content-block deltas; we route
    /// these to `TextSink::reasoning_delta` instead of `delta`.
    Thinking,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ImageInput;

    #[test]
    fn user_images_use_anthropic_source_blocks() {
        let wire = messages_to_anthropic(&[Message {
            role: Role::User,
            content: Content::UserInput {
                text: "inspect".into(),
                images: vec![ImageInput {
                    media_type: "image/jpeg".into(),
                    data: "YWJj".into(),
                    source: None,
                }],
            },
        }]);
        assert_eq!(wire[0]["content"][0]["type"], "text");
        assert_eq!(wire[0]["content"][1]["type"], "image");
        assert_eq!(wire[0]["content"][1]["source"]["type"], "base64");
        assert_eq!(wire[0]["content"][1]["source"]["media_type"], "image/jpeg");
        assert_eq!(wire[0]["content"][1]["source"]["data"], "YWJj");
    }

    fn test_message(role: Role, text: &str) -> Message {
        Message {
            role,
            content: Content::Text {
                text: text.to_string(),
            },
        }
    }

    #[test]
    fn usage_sums_fresh_cache_write_and_cache_read_tokens() {
        let mut usage = Usage::default();
        update_input_usage(
            &mut usage,
            &json!({
                "input_tokens": 101,
                "cache_creation_input_tokens": 2_000,
                "cache_read_input_tokens": 30_000,
            }),
        );
        assert_eq!(usage.input_tokens, Some(32_101));
        assert_eq!(usage.cached_tokens, 30_000);
    }

    #[test]
    fn cache_control_marks_four_stable_prefix_breakpoints() {
        let messages = vec![
            test_message(Role::User, "one"),
            test_message(Role::Assistant, "answer"),
            test_message(Role::User, "two"),
            test_message(Role::Assistant, "answer"),
            test_message(Role::User, "three"),
        ];
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "run".into(),
            schema: json!({"type": "object"}),
        }];
        let mut body = json!({
            "system": "stable system",
            "tools": tools_to_anthropic(&tools),
            "messages": messages_to_anthropic(&messages),
        });
        apply_cache_control(&mut body);

        fn count(value: &Value) -> usize {
            match value {
                Value::Array(values) => values.iter().map(count).sum(),
                Value::Object(map) => {
                    usize::from(map.contains_key("cache_control"))
                        + map.values().map(count).sum::<usize>()
                }
                _ => 0,
            }
        }
        assert_eq!(count(&body), MAX_CACHE_BREAKPOINTS);
        assert_eq!(
            body.pointer("/system/0/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
        assert_eq!(
            body.pointer("/tools/0/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
        // With system + tools consuming two slots, the newest two user
        // boundaries are cached and the oldest remains untouched.
        assert!(body.pointer("/messages/0/content").unwrap().is_string());
        assert_eq!(
            body.pointer("/messages/2/content/0/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
        assert_eq!(
            body.pointer("/messages/4/content/0/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
    }

    #[tokio::test]
    async fn first_party_sonnet_enables_1m_beta_and_window() {
        let provider = Anthropic::with_config(None, "test".into()).unwrap();
        assert_eq!(
            provider
                .effective_context_window_tokens("claude-sonnet-4-6")
                .await,
            Some(CONTEXT_1M_TOKENS)
        );
        assert_eq!(
            provider.request_betas("claude-sonnet-4-6"),
            [CONTEXT_1M_BETA]
        );
    }

    #[tokio::test]
    async fn compatible_endpoint_requires_explicit_capabilities() {
        let provider =
            Anthropic::with_config(Some("https://gateway.example/v1".into()), "test".into())
                .unwrap();
        assert!(!provider.cache_control);
        assert!(provider.request_betas("claude-sonnet-4-6").is_empty());
        assert_eq!(
            provider
                .effective_context_window_tokens("claude-sonnet-4-6")
                .await,
            None
        );
    }

    #[tokio::test]
    async fn compatible_endpoint_accepts_declared_capabilities() {
        let provider = Anthropic::with_options(
            Some("https://gateway.example/v1".into()),
            AnthropicOptions {
                cache_control: Some(true),
                betas: vec!["gateway-long-context".into(), "gateway-long-context".into()],
                context_window_tokens: Some(750_000),
            },
            "test".into(),
        )
        .unwrap();
        assert!(provider.cache_control);
        assert_eq!(
            provider.request_betas("vendor-model"),
            ["gateway-long-context"]
        );
        assert_eq!(
            provider
                .effective_context_window_tokens("vendor-model")
                .await,
            Some(750_000)
        );
    }
}
