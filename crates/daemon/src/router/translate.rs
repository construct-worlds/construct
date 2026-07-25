//! Anthropic Messages ⇄ OpenAI Chat Completions translation (spec 0112).
//!
//! A route whose target speaks a different dialect than the harness cannot
//! be served by rewriting a URL: the request body, the streaming event
//! shape, and the tool-call encoding are all different. This module is the
//! adapter between the two.
//!
//! Direction is fixed: an Anthropic-dialect client (the `claude` harness)
//! talking to an OpenAI-Chat-dialect endpoint. The reverse does not exist
//! because no route-capable harness speaks OpenAI Chat.
//!
//! Everything here is a pure function over JSON, so the translation is
//! testable without a socket. The streaming side is a small state machine
//! ([`AnthropicStreamEncoder`]) fed one OpenAI chunk at a time.

use serde_json::{json, Map, Value};

/// Convert an Anthropic `/v1/messages` request body into an OpenAI
/// `/chat/completions` one.
///
/// `model` is the route's model, substituted for whatever the client
/// asked for — the client is addressing its own model name, which means
/// nothing at the target.
pub fn request_to_openai(body: &Value, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // Anthropic carries the system prompt beside the messages; OpenAI
    // carries it as the first message.
    match body.get("system") {
        Some(Value::String(s)) => messages.push(json!({"role": "system", "content": s})),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text}));
            }
        }
        _ => {}
    }

    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        match message.get("content") {
            Some(Value::String(text)) => {
                messages.push(json!({"role": role, "content": text}));
            }
            Some(Value::Array(blocks)) => {
                translate_content_blocks(role, blocks, &mut messages);
            }
            _ => {}
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), json!(messages));
    // Anthropic requires max_tokens; OpenAI treats it as optional.
    if let Some(v) = body.get("max_tokens") {
        out.insert("max_tokens".into(), v.clone());
    }
    for passthrough in ["temperature", "top_p", "stream", "stop_sequences"] {
        if let Some(v) = body.get(passthrough) {
            let key = if passthrough == "stop_sequences" {
                "stop"
            } else {
                passthrough
            };
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let translated: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?;
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": t
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    }
                }))
            })
            .collect();
        if !translated.is_empty() {
            out.insert("tools".into(), json!(translated));
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        if let Some(translated) = tool_choice_to_openai(choice) {
            out.insert("tool_choice".into(), translated);
        }
    }
    Value::Object(out)
}

fn tool_choice_to_openai(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|n| json!({"type": "function", "function": {"name": n}})),
        _ => None,
    }
}

/// Expand one Anthropic message's content blocks into OpenAI messages.
///
/// The shapes do not correspond one-to-one: a single Anthropic user
/// message can carry several `tool_result` blocks, and OpenAI requires one
/// `role: "tool"` message per result.
fn translate_content_blocks(role: &str, blocks: &[Value], out: &mut Vec<Value>) {
    let mut text_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(json!({"type": "text", "text": t}));
                }
            }
            "image" => {
                // Anthropic sends base64 source objects; OpenAI takes a
                // data: URL.
                if let Some(source) = block.get("source") {
                    let media = source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    if let Some(data) = source.get("data").and_then(Value::as_str) {
                        text_parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{media};base64,{data}")}
                        }));
                    } else if let Some(url) = source.get("url").and_then(Value::as_str) {
                        text_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                }
            }
            "tool_use" => {
                tool_calls.push(json!({
                    "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "arguments": block
                            .get("input")
                            .map(|i| serde_json::to_string(i).unwrap_or_else(|_| "{}".into()))
                            .unwrap_or_else(|| "{}".into()),
                    }
                }));
            }
            "tool_result" => {
                // Flushed immediately: a tool message must directly follow
                // the assistant turn that called it.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "content": tool_result_text(block),
                }));
            }
            // `thinking` blocks are model-private reasoning in a shape the
            // target has no equivalent for. Replaying them as ordinary
            // text would put words in the assistant's mouth, so they are
            // dropped.
            _ => {}
        }
    }

    if text_parts.is_empty() && tool_calls.is_empty() {
        return;
    }
    let mut message = Map::new();
    message.insert("role".into(), json!(role));
    // Collapse a lone text part to a plain string — the shape every
    // OpenAI-compatible vendor accepts, including the stricter ones.
    match text_parts.len() {
        0 => {
            message.insert("content".into(), Value::Null);
        }
        1 if text_parts[0].get("type").and_then(Value::as_str) == Some("text") => {
            message.insert(
                "content".into(),
                text_parts[0].get("text").cloned().unwrap_or(json!("")),
            );
        }
        _ => {
            message.insert("content".into(), json!(text_parts));
        }
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), json!(tool_calls));
    }
    out.push(Value::Object(message));
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}

/// Convert a non-streaming OpenAI completion into an Anthropic message.
pub fn response_to_anthropic(body: &Value, model: &str) -> Value {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));
    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        content.push(json!({"type": "text", "text": text}));
    }
    for call in message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        content.push(json!({
            "type": "tool_use",
            "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
            "name": call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "input": call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .unwrap_or_else(|| json!({})),
        }));
    }
    let usage = body.get("usage");
    json!({
        "id": body.get("id").and_then(Value::as_str).unwrap_or("msg_router"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(choice.and_then(|c| c.get("finish_reason")).and_then(Value::as_str)),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0),
            "output_tokens": usage.and_then(|u| u.get("completion_tokens")).and_then(Value::as_u64).unwrap_or(0),
        }
    })
}

fn stop_reason(finish: Option<&str>) -> Value {
    match finish {
        Some("length") => json!("max_tokens"),
        Some("tool_calls") | Some("function_call") => json!("tool_use"),
        Some("stop") => json!("end_turn"),
        Some(other) => json!(other),
        None => Value::Null,
    }
}

/// One in-flight content block on the Anthropic side.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum OpenBlock {
    Text,
    ToolUse,
}

/// Turns a stream of OpenAI chunks into an Anthropic SSE event stream.
///
/// The two streaming protocols disagree about framing, not just naming:
/// OpenAI emits flat deltas with an implicit single content stream, while
/// Anthropic brackets every content block with explicit
/// `content_block_start` / `content_block_stop` events and numbers them.
/// This tracks which block is open so the brackets land in the right
/// places — a client that gets them wrong renders nothing at all.
pub struct AnthropicStreamEncoder {
    model: String,
    started: bool,
    open: Option<OpenBlock>,
    index: usize,
    /// OpenAI tool-call index → the Anthropic block index it was opened as.
    tool_blocks: Vec<usize>,
    finish_reason: Option<String>,
    output_tokens: u64,
    input_tokens: u64,
}

impl AnthropicStreamEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            open: None,
            index: 0,
            tool_blocks: Vec::new(),
            finish_reason: None,
            output_tokens: 0,
            input_tokens: 0,
        }
    }

    /// Feed one decoded OpenAI SSE `data:` payload; returns the Anthropic
    /// SSE bytes to forward (possibly empty).
    pub fn push_chunk(&mut self, chunk: &Value) -> String {
        let mut out = String::new();
        if !self.started {
            self.started = true;
            out.push_str(&event(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": chunk.get("id").and_then(Value::as_str).unwrap_or("msg_router"),
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }
        if let Some(usage) = chunk.get("usage") {
            if let Some(v) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.input_tokens = v;
            }
            if let Some(v) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.output_tokens = v;
            }
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return out;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return out;
        };

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if self.open != Some(OpenBlock::Text) {
                out.push_str(&self.close_open_block());
                out.push_str(&event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.index,
                        "content_block": {"type": "text", "text": ""},
                    }),
                ));
                self.open = Some(OpenBlock::Text);
            }
            out.push_str(&event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.index,
                    "delta": {"type": "text_delta", "text": text},
                }),
            ));
        }

        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let slot = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let is_new = slot >= self.tool_blocks.len();
            if is_new {
                out.push_str(&self.close_open_block());
                out.push_str(&event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.index,
                        "content_block": {
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or("toolu_router"),
                            "name": call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            "input": {},
                        },
                    }),
                ));
                self.open = Some(OpenBlock::ToolUse);
                self.tool_blocks.push(self.index);
            }
            if let Some(args) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                out.push_str(&event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.index,
                        "delta": {"type": "input_json_delta", "partial_json": args},
                    }),
                ));
            }
        }
        out
    }

    /// Close the stream. Emitted once, after the upstream ends or sends
    /// `[DONE]`.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started {
            // Upstream produced nothing at all; still emit a well-formed
            // empty message so the client sees a complete turn rather than
            // a truncated stream.
            out.push_str(&self.push_chunk(&json!({})));
        }
        out.push_str(&self.close_open_block());
        out.push_str(&event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason(self.finish_reason.as_deref()),
                    "stop_sequence": Value::Null,
                },
                "usage": {"output_tokens": self.output_tokens},
            }),
        ));
        out.push_str(&event("message_stop", &json!({"type": "message_stop"})));
        out
    }

    fn close_open_block(&mut self) -> String {
        let Some(_) = self.open.take() else {
            return String::new();
        };
        let out = event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.index}),
        );
        self.index += 1;
        out
    }
}

/// An Anthropic-shaped error event, for failures that happen once the
/// stream is already open.
pub fn error_event(message: &str) -> String {
    event(
        "error",
        &json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        }),
    )
}

fn event(name: &str, payload: &Value) -> String {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".into())
    )
}

/// Rough token estimate for `/v1/messages/count_tokens` when the target
/// cannot answer it.
///
/// Deliberately approximate and deliberately *not* silent: an
/// OpenAI-dialect endpoint has no equivalent endpoint, and refusing the
/// request outright makes the harness's context bookkeeping fail. Four
/// characters per token is the usual English approximation.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_events(raw: &str) -> Vec<(String, Value)> {
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

    #[test]
    fn moves_the_system_prompt_into_the_message_list() {
        let req = json!({
            "model": "claude-opus-5",
            "system": "be terse",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let out = request_to_openai(&req, "gpt-5.5");
        assert_eq!(out["model"], "gpt-5.5");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be terse");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["max_tokens"], 64);
    }

    #[test]
    fn accepts_a_block_array_system_prompt() {
        let req = json!({
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": [],
        });
        let out = request_to_openai(&req, "m");
        assert_eq!(out["messages"][0]["content"], "a\nb");
    }

    #[test]
    fn translates_tools_to_function_definitions() {
        let req = json!({
            "messages": [],
            "tools": [{
                "name": "read_file",
                "description": "read it",
                "input_schema": {"type": "object", "properties": {"p": {"type": "string"}}},
            }],
            "tool_choice": {"type": "any"},
        });
        let out = request_to_openai(&req, "m");
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "read_file");
        assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(out["tool_choice"], "required");
    }

    /// An assistant `tool_use` and the user's `tool_result` have to land as
    /// an assistant `tool_calls` message followed by a `role: "tool"` one,
    /// or the target rejects the conversation outright.
    #[test]
    fn round_trips_a_tool_call_exchange() {
        let req = json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "ls", "input": {"path": "/"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb"}
                ]},
            ]
        });
        let out = request_to_openai(&req, "m");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "ls");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"/\"}"
        );
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "toolu_1");
        assert_eq!(messages[1]["content"], "a\nb");
    }

    #[test]
    fn encodes_images_as_data_urls() {
        let req = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}},
            ]}]
        });
        let out = request_to_openai(&req, "m");
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAA");
    }

    /// Reasoning blocks have no target equivalent; replaying them as text
    /// would fabricate assistant speech.
    #[test]
    fn drops_thinking_blocks() {
        let req = json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "answer"},
            ]}]
        });
        let out = request_to_openai(&req, "m");
        assert_eq!(out["messages"][0]["content"], "answer");
    }

    #[test]
    fn streams_text_with_correctly_bracketed_blocks() {
        let mut enc = AnthropicStreamEncoder::new("kimi-k2.5");
        let mut raw = String::new();
        raw.push_str(&enc.push_chunk(&json!({"id": "cmpl_1", "choices": [{"delta": {"role": "assistant"}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {"content": "He"}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {"content": "llo"}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})));
        raw.push_str(&enc.finish());

        let events = sse_events(&raw);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["model"], "kimi-k2.5");
        assert_eq!(events[2].1["delta"]["text"], "He");
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn streams_tool_calls_as_input_json_deltas() {
        let mut enc = AnthropicStreamEncoder::new("m");
        let mut raw = String::new();
        raw.push_str(&enc.push_chunk(&json!({"id": "c", "choices": [{"delta": {"content": "ok"}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call_1", "function": {"name": "ls", "arguments": "{\"p\":"}
        }]}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "function": {"arguments": "\"/\"}"}
        }]}}]})));
        raw.push_str(&enc.push_chunk(&json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})));
        raw.push_str(&enc.finish());

        let events = sse_events(&raw);
        // The text block must be closed before the tool block opens, and
        // the two must carry different indices.
        let starts: Vec<&Value> = events
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["content_block"]["type"], "text");
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[1]["content_block"]["type"], "tool_use");
        assert_eq!(starts[1]["content_block"]["name"], "ls");
        assert_eq!(starts[1]["index"], 1);

        let partials: String = events
            .iter()
            .filter(|(n, v)| n == "content_block_delta" && v["delta"]["type"] == "input_json_delta")
            .map(|(_, v)| v["delta"]["partial_json"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(partials, "{\"p\":\"/\"}");

        let (_, last_delta) = events.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(last_delta["delta"]["stop_reason"], "tool_use");
    }

    /// A stream that produces nothing must still be a complete, parseable
    /// turn — a truncated SSE stream hangs the client.
    #[test]
    fn emits_a_well_formed_empty_turn() {
        let mut enc = AnthropicStreamEncoder::new("m");
        let events = sse_events(&enc.finish());
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn converts_a_non_streaming_completion() {
        let body = json!({
            "id": "cmpl_9",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "sure",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {"name": "ls", "arguments": "{\"p\":\"/\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        });
        let out = response_to_anthropic(&body, "kimi");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "kimi");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["input"]["p"], "/");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 10);
    }

    #[test]
    fn maps_finish_reasons_onto_stop_reasons() {
        assert_eq!(stop_reason(Some("stop")), json!("end_turn"));
        assert_eq!(stop_reason(Some("length")), json!("max_tokens"));
        assert_eq!(stop_reason(Some("tool_calls")), json!("tool_use"));
        assert_eq!(stop_reason(None), Value::Null);
    }

    #[test]
    fn estimates_tokens_from_every_string_in_the_body() {
        let body = json!({"messages": [{"role": "user", "content": "12345678"}]});
        // "user" + "12345678" = 12 chars → 3
        assert_eq!(estimate_tokens(&body), 3);
        assert_eq!(estimate_tokens(&json!({})), 1, "never zero");
    }
}
