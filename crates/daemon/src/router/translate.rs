//! Dialect translation (spec 0112).
//!
//! Every translation goes through a canonical form rather than being
//! written pairwise. With three dialects in play, pairwise would mean six
//! converters that drift apart; parse-to-canonical plus emit-from-canonical
//! means one parser and one emitter per dialect, and a new dialect costs
//! two pieces instead of 2N.
//!
//! Two directions, and they are not symmetric:
//!
//! - **Request**: parsed from the dialect the *harness* speaks, emitted in
//!   the dialect the *target* speaks.
//! - **Response stream**: decoded from the target's event vocabulary,
//!   re-encoded into the harness's. This is the half that must preserve
//!   framing, not merely content — see the encoders.
//!
//! The dialect a harness speaks is recognized from its request
//! ([`detect_dialect`]), not declared per harness: provider-agnostic
//! harnesses emit whatever their configured provider speaks, so a
//! declaration would be wrong for them by construction.

pub mod anthropic;
pub mod openai_chat;
pub mod responses;

use serde_json::Value;

use super::Dialect;

/// A request in neutral form.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CanonRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<CanonMessage>,
    pub tools: Vec<CanonTool>,
    pub tool_choice: Option<CanonToolChoice>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stream: bool,
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanonMessage {
    pub role: CanonRole,
    pub blocks: Vec<CanonBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CanonBlock {
    Text(String),
    /// Already a URL the target can take verbatim (`data:` or remote).
    Image(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        text: String,
        is_error: bool,
    },
    /// Model-private reasoning. Carried through the canonical form so a
    /// dialect that supports it can keep it, and dropped — never rendered
    /// as assistant text — by dialects that cannot.
    Thinking(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanonTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonToolChoice {
    Auto,
    Required,
    None,
    Named(String),
}

/// One step of a response stream, dialect-neutral.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonEvent {
    Start { id: String },
    TextDelta(String),
    ToolStart { index: usize, id: String, name: String },
    ToolArgsDelta { index: usize, json: String },
    Usage { input: u64, output: u64 },
    Stop { reason: CanonStop },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonStop {
    EndTurn,
    MaxTokens,
    ToolUse,
    Other(String),
}

impl CanonStop {
    pub fn from_openai_finish(reason: Option<&str>) -> Option<Self> {
        Some(match reason? {
            "stop" => CanonStop::EndTurn,
            "length" => CanonStop::MaxTokens,
            "tool_calls" | "function_call" => CanonStop::ToolUse,
            other => CanonStop::Other(other.to_string()),
        })
    }
}

/// Recognize the dialect of an intercepted request from its body.
///
/// The three shapes are unambiguous on their own keys — `input` belongs
/// only to Responses, a top-level `system` beside `messages` only to
/// Anthropic — so this needs no help from the URL path and works for a
/// harness whose endpoint moves with its configured provider.
pub fn detect_dialect(body: &Value) -> Option<Dialect> {
    let obj = body.as_object()?;
    if obj.contains_key("input")
        || obj.contains_key("instructions")
        || obj.contains_key("max_output_tokens")
    {
        return Some(Dialect::OpenAiResponses);
    }
    if obj.contains_key("messages") {
        // Anthropic requires max_tokens and carries the system prompt
        // beside the messages; Chat Completions has neither.
        if obj.contains_key("system") || obj.contains_key("max_tokens") {
            return Some(Dialect::AnthropicMessages);
        }
        return Some(Dialect::OpenAiChat);
    }
    None
}

/// Parse a request written in `dialect` into canonical form.
pub fn parse_request(dialect: Dialect, body: &Value) -> CanonRequest {
    match dialect {
        Dialect::AnthropicMessages => anthropic::parse_request(body),
        Dialect::OpenAiChat => openai_chat::parse_request(body),
        Dialect::OpenAiResponses => responses::parse_request(body),
    }
}

/// Emit a canonical request in `dialect`, substituting `model`.
pub fn emit_request(dialect: Dialect, req: &CanonRequest, model: &str) -> Value {
    match dialect {
        Dialect::AnthropicMessages => anthropic::emit_request(req, model),
        Dialect::OpenAiChat => openai_chat::emit_request(req, model),
        // No configurable profile targets Responses; a route never emits
        // it. Falling back to Chat keeps this total rather than panicking
        // on a shape that cannot currently be constructed.
        Dialect::OpenAiResponses => openai_chat::emit_request(req, model),
    }
}

/// Path suffix to append to a profile's base URL for `dialect`.
pub fn target_path(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::AnthropicMessages => "/messages",
        Dialect::OpenAiChat | Dialect::OpenAiResponses => "/chat/completions",
    }
}

/// Decode one SSE `data:` payload from a target speaking `dialect`.
pub fn decode_target_event(dialect: Dialect, data: &Value) -> Vec<CanonEvent> {
    match dialect {
        Dialect::AnthropicMessages => anthropic::decode_event(data),
        Dialect::OpenAiChat | Dialect::OpenAiResponses => openai_chat::decode_event(data),
    }
}

/// Whether a target's SSE frame terminates the stream.
pub fn is_done_sentinel(data: &str) -> bool {
    data.trim() == "[DONE]"
}

/// Re-encodes canonical events into the dialect the harness speaks.
pub enum ClientEncoder {
    Anthropic(anthropic::StreamEncoder),
    Responses(responses::StreamEncoder),
}

impl ClientEncoder {
    pub fn new(dialect: Dialect, model: &str) -> Self {
        match dialect {
            Dialect::OpenAiResponses => {
                ClientEncoder::Responses(responses::StreamEncoder::new(model))
            }
            // Chat Completions is not spoken by any route-capable harness;
            // Anthropic framing is the safe default for the remaining case.
            _ => ClientEncoder::Anthropic(anthropic::StreamEncoder::new(model)),
        }
    }

    pub fn push(&mut self, event: &CanonEvent) -> String {
        match self {
            ClientEncoder::Anthropic(e) => e.push(event),
            ClientEncoder::Responses(e) => e.push(event),
        }
    }

    /// Close the stream as a complete turn. Must be called even when the
    /// target produced nothing: a truncated stream leaves the harness
    /// waiting forever.
    pub fn finish(&mut self) -> String {
        match self {
            ClientEncoder::Anthropic(e) => e.finish(),
            ClientEncoder::Responses(e) => e.finish(),
        }
    }

    /// An error the harness will surface, in its own dialect.
    pub fn error(&self, message: &str) -> String {
        match self {
            ClientEncoder::Anthropic(_) => anthropic::error_event(message),
            ClientEncoder::Responses(_) => responses::error_event(message),
        }
    }
}

/// Non-streaming response body in the client's dialect.
pub fn encode_response(
    dialect: Dialect,
    target: Dialect,
    body: &Value,
    model: &str,
) -> Value {
    let events = match target {
        Dialect::AnthropicMessages => anthropic::decode_full_response(body),
        _ => openai_chat::decode_full_response(body),
    };
    match dialect {
        Dialect::OpenAiResponses => responses::encode_full(&events, model),
        _ => anthropic::encode_full(&events, model),
    }
}

/// Rough token estimate for endpoints a target cannot answer.
///
/// Deliberately approximate and deliberately not silent: refusing would
/// break the harness's own context bookkeeping. Four characters per token
/// is the usual English approximation.
pub fn estimate_tokens(body: &Value) -> u64 {
    fn walk(v: &Value, acc: &mut usize) {
        match v {
            Value::String(s) => *acc += s.len(),
            Value::Array(items) => items.iter().for_each(|i| walk(i, acc)),
            Value::Object(map) => map.values().for_each(|i| walk(i, acc)),
            _ => {}
        }
    }
    let mut chars = 0usize;
    walk(body, &mut chars);
    (chars / 4).max(1) as u64
}

/// Shared SSE frame helper.
pub(crate) fn sse(name: &str, payload: &Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
}

#[cfg(test)]
pub(crate) mod tests_support {
    use serde_json::Value;

    /// Split an SSE body into (event name, payload) pairs.
    pub fn sse_events(raw: &str) -> Vec<(String, Value)> {
        raw.split("\n\n")
            .filter(|b| !b.trim().is_empty())
            .map(|block| {
                let mut name = String::new();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        name = v.to_string();
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data = v.to_string();
                    }
                }
                (name, serde_json::from_str(&data).unwrap_or(Value::Null))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_responses_by_its_own_keys() {
        assert_eq!(
            detect_dialect(&json!({"model":"m","input":[],"store":false})),
            Some(Dialect::OpenAiResponses)
        );
        assert_eq!(
            detect_dialect(&json!({"model":"m","instructions":"be terse"})),
            Some(Dialect::OpenAiResponses)
        );
    }

    #[test]
    fn recognizes_anthropic_by_system_or_max_tokens() {
        assert_eq!(
            detect_dialect(&json!({"messages":[],"system":"x"})),
            Some(Dialect::AnthropicMessages)
        );
        assert_eq!(
            detect_dialect(&json!({"messages":[],"max_tokens":16})),
            Some(Dialect::AnthropicMessages)
        );
    }

    #[test]
    fn recognizes_chat_completions_by_elimination() {
        assert_eq!(
            detect_dialect(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]})),
            Some(Dialect::OpenAiChat)
        );
        assert_eq!(detect_dialect(&json!({"unrelated": true})), None);
    }

    /// The captured shapes from the real harnesses must classify
    /// correctly — this is the check that keeps detection honest against
    /// what was actually observed on the wire.
    #[test]
    fn classifies_captured_harness_requests() {
        // codex / pi (chatgpt.com /backend-api/codex/responses)
        let codex = json!({
            "model": "gpt-5.6-sol", "stream": true, "store": false,
            "instructions": "You are an expert coding assistant",
            "input": [{"role":"user","content":[{"type":"input_text","text":"say pong"}]}],
            "tools": [{"type":"function","name":"read"}],
            "reasoning": {}, "parallel_tool_calls": true
        });
        assert_eq!(detect_dialect(&codex), Some(Dialect::OpenAiResponses));

        // grok / opencode (/v1/responses)
        let grok = json!({
            "model": "grok-4.5", "stream": true, "store": false,
            "input": [{"type":"message","role":"system","content":"..."}],
            "max_output_tokens": 100, "reasoning": {}
        });
        assert_eq!(detect_dialect(&grok), Some(Dialect::OpenAiResponses));

        // hermes (inference-api.nousresearch.com /v1/chat/completions):
        // messages with no top-level system and no max_tokens.
        let hermes = json!({
            "model": "tencent/hy3:free", "stream": true,
            "messages": [{"role":"system","content":"..."},{"role":"user","content":"say pong"}],
            "tools": [{"type":"function","function":{"name":"browser_back"}}],
            "reasoning": {}, "stream_options": {}, "tags": []
        });
        assert_eq!(detect_dialect(&hermes), Some(Dialect::OpenAiChat));

        // claude
        let claude = json!({
            "model": "claude-opus-5", "max_tokens": 32, "stream": true,
            "system": "be terse", "messages": [{"role":"user","content":"hi"}]
        });
        assert_eq!(detect_dialect(&claude), Some(Dialect::AnthropicMessages));
    }

    #[test]
    fn estimates_tokens_from_every_string_in_the_body() {
        let body = json!({"messages": [{"role": "user", "content": "12345678"}]});
        assert_eq!(estimate_tokens(&body), 3);
        assert_eq!(estimate_tokens(&json!({})), 1, "never zero");
    }

    /// A canonical request must survive a round trip through the dialect
    /// it came from — otherwise a same-dialect translation would silently
    /// mutate the conversation.
    #[test]
    fn anthropic_round_trips_through_canonical_form() {
        let original = json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "system": "be terse",
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_1","name":"ls","input":{"path":"/"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":"a"}
                ]}
            ]
        });
        let canon = parse_request(Dialect::AnthropicMessages, &original);
        let back = emit_request(Dialect::AnthropicMessages, &canon, "claude-opus-5");
        let recanon = parse_request(Dialect::AnthropicMessages, &back);
        assert_eq!(canon, recanon);
    }

    #[test]
    fn responses_round_trips_through_canonical_form() {
        let original = json!({
            "model": "gpt-5.6",
            "instructions": "be terse",
            "input": [
                {"role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call","call_id":"call_1","name":"ls","arguments":"{\"p\":\"/\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"a"}
            ]
        });
        let canon = parse_request(Dialect::OpenAiResponses, &original);
        assert_eq!(canon.system.as_deref(), Some("be terse"));
        assert_eq!(canon.messages.len(), 3);
        assert!(matches!(
            canon.messages[1].blocks[0],
            CanonBlock::ToolUse { .. }
        ));
        assert!(matches!(
            canon.messages[2].blocks[0],
            CanonBlock::ToolResult { .. }
        ));
    }
}
