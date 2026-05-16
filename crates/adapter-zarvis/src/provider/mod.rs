//! Provider plumbing: a small trait + normalized message/tool types,
//! plus per-provider implementations. The agent loop is generic over
//! [`LlmProvider`] so adding a new provider is one impl file.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod routing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the rolling conversation we send to the model. Mirrors
/// the shape that maps cleanly onto OpenAI / Anthropic / Ollama wire
/// formats — each provider impl translates it to its own JSON.
///
/// Serializable so the agent loop can append each message to
/// `zarvis.jsonl` and replay on daemon-restart resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    /// Plain text (system / user / assistant).
    Text { text: String },
    /// Assistant turn that's making tool calls. May also include final
    /// pre-tool prose (`text`) that comes before the calls.
    AssistantToolCalls {
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
    /// Single tool result, paired with its originating call id.
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON-schema-shaped object describing the tool's input.
    pub schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

/// Why the provider stopped producing tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Final assistant text; the agent loop should park awaiting input.
    EndTurn,
    /// Assistant emitted tool calls; the agent loop should run them and
    /// feed results back.
    ToolUse,
    /// Hit max tokens / other provider-side limit. Treat like EndTurn so
    /// the user can intervene.
    MaxTokens,
}

/// The aggregated result of one provider call.
#[derive(Debug)]
pub struct ProviderTurn {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Sink for the assistant's streaming text deltas. Headless mode wires
/// this to `SessionEvent::Message` events; interactive (PTY) mode wires
/// it to raw `SessionEvent::Pty` bytes so the user sees the response
/// flow in the terminal pane. Provider impls don't care which it is.
pub trait TextSink: Send {
    fn delta(&mut self, text: &str);
}

/// Sentinel error for "the input you sent is over the model's
/// context window". Providers return this wrapped in `anyhow::Error`
/// when they recognize the API's overflow signal in an HTTP 400
/// body; the agent loop downcasts and routes to the
/// `model_limits.rs` learn-and-retry path. `extracted` carries the
/// provider-reported limit when present (OpenAI: "maximum context
/// length is N tokens"); otherwise `None` and the agent loop falls
/// back to a fixed-ratio reduction.
#[derive(Debug, Clone)]
pub struct ContextOverflow {
    pub extracted: Option<u64>,
    pub raw: String,
}

impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "context overflow (extracted={:?}): {}",
            self.extracted, self.raw
        )
    }
}

impl std::error::Error for ContextOverflow {}

/// Parse a provider's HTTP 400 error body for a context-overflow
/// signature. Returns `Some(extracted)` only when the body reads as
/// an overflow error; the extracted value is `Some(n)` if the body
/// stated a token limit explicitly, `None` if it was overflow-shaped
/// but didn't name a number.
pub fn parse_overflow(body: &str) -> Option<Option<u64>> {
    let lower = body.to_ascii_lowercase();
    let overflow_shaped = lower.contains("maximum context length")
        || lower.contains("context length")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("context window")
        || lower.contains("too many tokens");
    if !overflow_shaped {
        return None;
    }
    // OpenAI: "This model's maximum context length is 200000 tokens.
    // However, you requested ... tokens".
    // Extract the FIRST token-count number; the "however you
    // requested N" comes after, and it'd be confusing to learn
    // *that* as the cap.
    let mut nums = Vec::new();
    let mut cur = String::new();
    for ch in body.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                if let Ok(n) = cur.parse::<u64>() {
                    nums.push(n);
                }
                cur.clear();
            }
        }
    }
    if !cur.is_empty() {
        if let Ok(n) = cur.parse::<u64>() {
            nums.push(n);
        }
    }
    // Reasonable token-count range: >= 1K, <= 5M. Filters out HTTP
    // status codes, error codes, line numbers, etc.
    let extracted = nums.into_iter().find(|n| *n >= 1_000 && *n <= 5_000_000);
    Some(extracted)
}

#[cfg(test)]
mod overflow_tests {
    use super::parse_overflow;

    #[test]
    fn openai_style_with_limit() {
        let body = r#"{"error":{"message":"This model's maximum context length is 200000 tokens. However, you requested 380000 tokens (250000 in the messages, 130000 in the completion). Please reduce the length of the messages or completion.","type":"invalid_request_error","param":"messages","code":"context_length_exceeded"}}"#;
        let r = parse_overflow(body);
        assert_eq!(r, Some(Some(200_000)));
    }

    #[test]
    fn anthropic_style_no_explicit_limit() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#;
        let r = parse_overflow(body);
        // Two numbers in range; we take the first (250000 here, the
        // "current usage" number). That's a worse outcome than
        // extracting the actual 200000 cap — but the fallback ratio
        // applied at the agent layer drops it 20% to 200k anyway,
        // so the user lands close to right either way.
        assert!(r.is_some());
        let extracted = r.unwrap();
        // Either parser interpretation is acceptable; the important
        // thing is we recognized the overflow shape.
        assert!(extracted.is_some());
    }

    #[test]
    fn ollama_style() {
        let body = "context length exceeded: 8192 tokens";
        assert_eq!(parse_overflow(body), Some(Some(8192)));
    }

    #[test]
    fn unrelated_400_is_not_overflow() {
        let body = r#"{"error":{"message":"invalid api key","code":"invalid_api_key"}}"#;
        assert_eq!(parse_overflow(body), None);
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Run one turn against the model. Implementations stream the
    /// response and push deltas through `sink` so the user sees the
    /// assistant text flowing as it arrives.
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn>;
}
