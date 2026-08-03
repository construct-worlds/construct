//! Dialect translation (spec 0116).
//!
//! Every translation goes through a canonical form rather than being
//! written pairwise. With four dialects in play, pairwise would already
//! mean twelve converters that drift apart; parse-to-canonical plus
//! emit-from-canonical means one parser and one emitter per dialect, and a
//! new dialect costs two pieces instead of 2N.
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
pub mod dsml;
pub mod google;
pub mod openai_chat;
pub mod responses;

use std::collections::BTreeMap;

use serde_json::{json, Value};

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
    /// Requested reasoning effort (`low` / `medium` / `high` / ...).
    /// Parsed from dialects that carry it as a request knob and emitted
    /// only by dialects whose targets accept it verbatim; mapping it onto
    /// unlike knobs (e.g. Anthropic thinking budgets) is deliberately not
    /// done here.
    pub reasoning_effort: Option<String>,
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
    /// A freeform (grammar-constrained) tool: the harness takes raw text
    /// as the call input, not JSON arguments, and declares no JSON schema.
    /// Codex's `exec` is one. Carries the dialect's own `format` object so
    /// a target that understands freeform tools gets it back verbatim;
    /// targets that do not are given a synthesized schema instead. `None`
    /// for an ordinary function tool.
    pub freeform: Option<Value>,
}

/// Property name of the synthesized single argument a freeform tool is
/// given when the target speaks only JSON-schema functions. Matches the
/// field Responses itself uses on `custom_tool_call`, so the value maps
/// straight back with no renaming.
pub const FREEFORM_INPUT_PROPERTY: &str = "input";

impl CanonTool {
    /// Schema to advertise to a target that has no freeform tool concept.
    ///
    /// A freeform tool has no JSON schema, and emitting it as a function
    /// with an empty property set tells the model it takes no arguments —
    /// leaving it no way to express the call and pushing it into writing
    /// prose instead. Synthesizing a single required string restores a
    /// callable shape.
    pub fn schema_for_json_target(&self) -> Value {
        let Some(format) = self.freeform.as_ref() else {
            return self.schema.clone();
        };
        let mut description =
            "Raw text input for this tool. Send the body verbatim, not JSON.".to_string();
        if let Some(grammar) = format.get("definition").and_then(Value::as_str) {
            let syntax = format
                .get("syntax")
                .and_then(Value::as_str)
                .unwrap_or("grammar");
            description.push_str(&format!(
                " It must parse against this {syntax} grammar:\n{grammar}"
            ));
        }
        json!({
            "type": "object",
            "properties": {
                FREEFORM_INPUT_PROPERTY: {"type": "string", "description": description}
            },
            "required": [FREEFORM_INPUT_PROPERTY],
            "additionalProperties": false,
        })
    }
}

/// Request-scoped state needed to reverse a target's wire restrictions.
///
/// Most dialects need no state. Gemini, however, restricts function names
/// to 64 ASCII characters. Invalid or colliding names are encoded on the
/// request and restored from this map when the response calls the tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslationContext {
    pub(crate) tool_names: BTreeMap<String, String>,
    /// Tool names this request actually offered, in request order. DSML
    /// recovery uses them to resolve a name the model invented (it names
    /// `shell` where the harness offers `exec_command`) onto one the
    /// harness can dispatch. Empty means "no information" — never a
    /// signal that the request offered no tools.
    pub(crate) offered_tools: Vec<String>,
    /// Names of the offered tools that are freeform. A call to one of
    /// these has to go back to the harness as its dialect's freeform call
    /// item carrying raw text, not as a JSON function call.
    pub(crate) freeform_tools: Vec<String>,
}

/// A translated body plus the request-scoped state its response decoder
/// needs. Keeping the state beside the body prevents global cross-session
/// name maps and their collision/leakage failure modes.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedRequest {
    pub body: Value,
    pub context: TranslationContext,
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
    Start {
        id: String,
    },
    TextDelta(String),
    /// Model-private reasoning as the target streamed it. Not re-encoded
    /// into any client dialect — it exists so the proxy can remember what
    /// a target reasoned and hand it back on the next request (spec 0181).
    ThinkingDelta(String),
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolArgsDelta {
        index: usize,
        json: String,
    },
    Usage {
        input: u64,
        output: u64,
    },
    Stop {
        reason: CanonStop,
    },
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
/// The shapes are unambiguous on their own keys — `input` belongs
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
    if obj.contains_key("contents")
        || obj.contains_key("systemInstruction")
        || obj.contains_key("generationConfig")
    {
        return Some(Dialect::GoogleGemini);
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
        Dialect::GoogleGemini => google::parse_request(body),
        Dialect::OpenAiChat => openai_chat::parse_request(body),
        Dialect::OpenAiResponses => responses::parse_request(body),
    }
}

/// Emit a canonical request in `dialect`, substituting `model`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn emit_request(dialect: Dialect, req: &CanonRequest, model: &str) -> Value {
    emit_request_with_context(dialect, req, model).body
}

/// Emit a canonical request and retain any request-scoped decoder state.
pub fn emit_request_with_context(
    dialect: Dialect,
    req: &CanonRequest,
    model: &str,
) -> EmittedRequest {
    let (body, context) = match dialect {
        Dialect::GoogleGemini => google::emit_request(req, model),
        Dialect::AnthropicMessages => (
            anthropic::emit_request(req, model),
            TranslationContext::default(),
        ),
        Dialect::OpenAiChat => (
            openai_chat::emit_request(req, model),
            TranslationContext::default(),
        ),
        Dialect::OpenAiResponses => (
            responses::emit_request(req, model),
            TranslationContext::default(),
        ),
    };
    let mut context = context;
    // Recorded for every dialect: the decoder cannot see the request, and
    // a name the model invented is only checkable against what was offered.
    context.offered_tools = req.tools.iter().map(|t| t.name.clone()).collect();
    context.freeform_tools = req
        .tools
        .iter()
        .filter(|t| t.freeform.is_some())
        .map(|t| t.name.clone())
        .collect();
    EmittedRequest { body, context }
}

/// Full target URL for a dialect. Gemini selects its RPC method from
/// streaming mode and carries the model in the path; the other APIs have a
/// stable path independent of the request body.
pub fn target_url(base_url: &str, dialect: Dialect, model: &str, stream: bool) -> String {
    match dialect {
        Dialect::GoogleGemini => {
            let method = if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            let suffix = if stream { "?alt=sse" } else { "" };
            // AI Studio's native API root is commonly configured either as
            // the bare host (OpenCodex convention) or with `/v1beta`
            // already present (Construct's historical example). Accept
            // both, and preserve an explicit Google API version.
            let base = base_url.trim_end_matches('/');
            let last_segment = base.rsplit('/').next().unwrap_or_default();
            let versioned_base = if matches!(last_segment, "v1" | "v1beta" | "v1alpha") {
                base.to_string()
            } else {
                format!("{base}/v1beta")
            };
            format!(
                "{}/models/{}:{method}{suffix}",
                versioned_base,
                encode_path_segment(model)
            )
        }
        Dialect::OpenAiResponses => {
            let base = base_url.trim_end_matches('/');
            if base.ends_with("/responses") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/responses")
            } else {
                format!("{base}/v1/responses")
            }
        }
        _ => format!("{}{}", base_url.trim_end_matches('/'), target_path(dialect)),
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Path suffix to append to a profile's base URL for `dialect`.
pub fn target_path(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::AnthropicMessages => "/messages",
        // Gemini has no fixed path: see target_url.
        Dialect::GoogleGemini => "/models",
        Dialect::OpenAiChat => "/chat/completions",
        Dialect::OpenAiResponses => "/responses",
    }
}

/// Decode one SSE `data:` payload from a target speaking `dialect`.
pub fn decode_target_event_with_context(
    dialect: Dialect,
    data: &Value,
    context: &TranslationContext,
) -> Vec<CanonEvent> {
    match dialect {
        Dialect::AnthropicMessages => anthropic::decode_event(data),
        Dialect::GoogleGemini => google::decode_event(data, context),
        Dialect::OpenAiChat => openai_chat::decode_event(data),
        Dialect::OpenAiResponses => responses::decode_event(data),
    }
}

/// Whether a target's SSE frame terminates the stream.
pub fn is_done_sentinel(data: &str) -> bool {
    data.trim() == "[DONE]"
}

/// Pull a client-safe message from a provider error envelope.
///
/// Raw non-JSON bodies are intentionally not reflected: upstream proxies
/// sometimes include credentials, request paths, or HTML diagnostics.
pub fn upstream_error_message(data: &Value) -> Option<String> {
    fn message(value: &Value) -> Option<&str> {
        value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.as_str())
    }

    let raw = data
        .get("error")
        .and_then(message)
        .or_else(|| data.get("last_error").and_then(message))
        .or_else(|| data.get("detail").and_then(message))
        .or_else(|| {
            data.get("response")
                .and_then(|r| r.get("error"))
                .and_then(message)
        })?;
    Some(sanitize_error_message(raw))
}

fn sanitize_error_message(raw: &str) -> String {
    let mut out = Vec::new();
    let mut redact_next = false;
    for token in raw.split_whitespace().take(100) {
        if redact_next {
            out.push("[REDACTED]");
            redact_next = false;
            continue;
        }
        if token.eq_ignore_ascii_case("bearer")
            || token.eq_ignore_ascii_case("x-api-key:")
            || token.eq_ignore_ascii_case("api-key:")
        {
            out.push(token);
            redact_next = true;
            continue;
        }
        if token.starts_with("sk-")
            || token.starts_with("AIza")
            || token.starts_with("ghp_")
            || token.starts_with("/Users/")
            || token.starts_with("/home/")
            || token.starts_with("/root/")
            || token.to_ascii_lowercase().contains(":\\users\\")
        {
            out.push("[REDACTED]");
        } else {
            out.push(token);
        }
    }
    let message = out.join(" ");
    if message.is_empty() {
        "upstream error".to_string()
    } else {
        message
    }
}

/// Reject response shapes that otherwise decode into a bogus empty turn.
pub fn invalid_response_message(dialect: Dialect, data: &Value) -> Option<&'static str> {
    match dialect {
        Dialect::GoogleGemini
            if !data
                .get("candidates")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()) =>
        {
            Some("google response contained no candidates")
        }
        _ => None,
    }
}

/// A non-streaming error envelope in the dialect the harness expects.
pub fn error_body(dialect: Dialect, message: &str) -> Value {
    match dialect {
        Dialect::AnthropicMessages => serde_json::json!({
            "type":"error",
            "error":{"type":"api_error","message":message}
        }),
        Dialect::GoogleGemini => serde_json::json!({
            "error":{"code":502,"message":message,"status":"INTERNAL"}
        }),
        Dialect::OpenAiChat | Dialect::OpenAiResponses => serde_json::json!({
            "error":{"type":"api_error","message":message}
        }),
    }
}

/// Re-encodes canonical events into the dialect the harness speaks.
pub enum ClientEncoder {
    Anthropic(anthropic::StreamEncoder),
    Chat(openai_chat::StreamEncoder),
    Responses(responses::StreamEncoder),
}

impl ClientEncoder {
    pub fn new(dialect: Dialect, model: &str) -> Self {
        Self::with_context(dialect, model, &TranslationContext::default())
    }

    /// Encoder that knows the request's freeform tools, so a call to one
    /// goes back in the item shape the harness declared it with.
    pub fn with_context(dialect: Dialect, model: &str, context: &TranslationContext) -> Self {
        match dialect {
            Dialect::OpenAiChat => ClientEncoder::Chat(openai_chat::StreamEncoder::new(model)),
            Dialect::OpenAiResponses => ClientEncoder::Responses(
                responses::StreamEncoder::with_freeform_tools(model, &context.freeform_tools),
            ),
            // No route-capable harness currently speaks Gemini. Anthropic
            // remains the established client framing for Claude.
            _ => ClientEncoder::Anthropic(anthropic::StreamEncoder::new(model)),
        }
    }

    pub fn push(&mut self, event: &CanonEvent) -> String {
        match self {
            ClientEncoder::Anthropic(e) => e.push(event),
            ClientEncoder::Chat(e) => e.push(event),
            ClientEncoder::Responses(e) => e.push(event),
        }
    }

    /// Close the stream as a complete turn. Must be called even when the
    /// target produced nothing: a truncated stream leaves the harness
    /// waiting forever.
    pub fn finish(&mut self) -> String {
        match self {
            ClientEncoder::Anthropic(e) => e.finish(),
            ClientEncoder::Chat(e) => e.finish(),
            ClientEncoder::Responses(e) => e.finish(),
        }
    }

    /// An error the harness will surface, in its own dialect.
    pub fn error(&self, message: &str) -> String {
        match self {
            ClientEncoder::Anthropic(_) => anthropic::error_event(message),
            ClientEncoder::Chat(_) => openai_chat::error_event(message),
            ClientEncoder::Responses(_) => responses::error_event(message),
        }
    }
}

/// Decode a whole non-streaming response into canonical events.
pub fn decode_full_response_with_context(
    target: Dialect,
    body: &Value,
    context: &TranslationContext,
) -> Vec<CanonEvent> {
    match target {
        Dialect::AnthropicMessages => anthropic::decode_full_response(body),
        Dialect::GoogleGemini => google::decode_full_response(body, context),
        Dialect::OpenAiResponses => responses::decode_full_response(body),
        Dialect::OpenAiChat => openai_chat::decode_full_response(body, context),
    }
}

/// Re-encode decoded events as a non-streaming body in `dialect`.
pub fn encode_full_response(dialect: Dialect, events: &[CanonEvent], model: &str) -> Value {
    encode_full_response_with_context(dialect, events, model, &TranslationContext::default())
}

/// As [`encode_full_response`], but aware of the request's freeform tools.
pub fn encode_full_response_with_context(
    dialect: Dialect,
    events: &[CanonEvent],
    model: &str,
    context: &TranslationContext,
) -> Value {
    match dialect {
        Dialect::OpenAiChat => openai_chat::encode_full(events, model),
        Dialect::OpenAiResponses => {
            responses::encode_full_with_freeform(events, model, &context.freeform_tools)
        }
        _ => anthropic::encode_full(events, model),
    }
}

/// Non-streaming response body in the client's dialect.
#[cfg_attr(not(test), allow(dead_code))]
pub fn encode_response_with_context(
    dialect: Dialect,
    target: Dialect,
    body: &Value,
    model: &str,
    context: &TranslationContext,
) -> Value {
    let events = decode_full_response_with_context(target, body, context);
    encode_full_response_with_context(dialect, &events, model, context)
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

    #[test]
    fn recognizes_gemini_by_its_own_keys() {
        assert_eq!(
            detect_dialect(&json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]})),
            Some(Dialect::GoogleGemini)
        );
    }

    #[test]
    fn gemini_target_url_selects_rpc_and_escapes_model() {
        assert_eq!(
            target_url(
                "https://generativelanguage.googleapis.com/v1beta/",
                Dialect::GoogleGemini,
                "publishers/acme model",
                true,
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/publishers%2Facme%20model:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            target_url(
                "https://generativelanguage.googleapis.com",
                Dialect::GoogleGemini,
                "gemini-2.5-pro",
                false,
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn responses_target_url_normalizes_the_v1_segment() {
        assert_eq!(
            target_url(
                "https://resource.openai.azure.com/openai",
                Dialect::OpenAiResponses,
                "deployment",
                true,
            ),
            "https://resource.openai.azure.com/openai/v1/responses"
        );
        assert_eq!(
            target_url(
                "https://api.openai.com/v1/",
                Dialect::OpenAiResponses,
                "gpt",
                false,
            ),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            target_url(
                "https://gateway.example/v1/responses",
                Dialect::OpenAiResponses,
                "gpt",
                false,
            ),
            "https://gateway.example/v1/responses"
        );
    }

    /// The captured shapes from the real harnesses must classify
    /// correctly — this is the check that keeps detection honest against
    /// what was actually observed on the wire.
    /// The live Codex→DeepSeek failure, end to end. Codex's only execution
    /// tool is `exec`, a `custom` grammar tool with no `parameters`. Routed
    /// to a chat-completions target it used to arrive as a function taking
    /// no arguments, leaving the model no way to express a call — so it
    /// wrote DSML prose instead and no tool ever ran, every single turn.
    #[test]
    fn codex_exec_tool_reaches_a_chat_target_callable() {
        let codex = json!({
            "model": "deepseek-v4", "stream": true,
            "instructions": "You are a coding assistant",
            "input": [{"role":"user","content":[{"type":"input_text","text":"count the files"}]}],
            "tools": [
                {"type":"custom","name":"exec","description":"Run JavaScript",
                 "format":{"type":"grammar","syntax":"lark","definition":"start: SOURCE\n"}},
                {"type":"function","name":"wait","description":"wait",
                 "parameters":{"type":"object","properties":{"ms":{"type":"number"}}}}
            ]
        });
        assert_eq!(detect_dialect(&codex), Some(Dialect::OpenAiResponses));
        let req = parse_request(Dialect::OpenAiResponses, &codex);
        let emitted = emit_request_with_context(Dialect::OpenAiChat, &req, "deepseek-v4");

        let tools = emitted.body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2, "no tool may be dropped: {tools:?}");
        let exec = tools
            .iter()
            .find(|t| t["function"]["name"] == "exec")
            .expect("exec must survive");
        let params = &exec["function"]["parameters"];
        assert_eq!(params["required"], json!([FREEFORM_INPUT_PROPERTY]));
        assert_eq!(
            params["properties"][FREEFORM_INPUT_PROPERTY]["type"],
            "string"
        );

        // And the response side knows to send a call back as freeform.
        assert_eq!(emitted.context.freeform_tools, vec!["exec".to_string()]);

        // The ordinary function tool keeps its own schema.
        let wait = tools
            .iter()
            .find(|t| t["function"]["name"] == "wait")
            .expect("wait");
        assert_eq!(
            wait["function"]["parameters"]["properties"]["ms"]["type"],
            "number"
        );
    }

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

    #[test]
    fn extracts_and_redacts_structured_upstream_errors() {
        assert_eq!(
            upstream_error_message(&json!({
                "error":{"message":"Authorization: Bearer secret-token at /Users/me/key.json"}
            }))
            .as_deref(),
            Some("Authorization: Bearer [REDACTED] at [REDACTED]")
        );
        assert_eq!(upstream_error_message(&json!("raw secret")), None);
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
