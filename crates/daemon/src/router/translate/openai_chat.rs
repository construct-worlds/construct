//! OpenAI Chat Completions dialect: parse, emit, decode, and encode.
//!
//! Hermes is probe-confirmed to speak this dialect, so both request and
//! response directions are part of the router contract.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::{
    CanonBlock, CanonEvent, CanonMessage, CanonRequest, CanonRole, CanonStop, CanonTool,
    CanonToolChoice,
};

pub fn parse_request(body: &Value) -> CanonRequest {
    let mut system = None;
    let mut messages = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "system" || role == "developer" {
            if let Some(t) = message.get("content").and_then(Value::as_str) {
                system = Some(t.to_string());
            }
            continue;
        }
        let mut blocks = Vec::new();
        // Reasoning-model dialects carry the model's own thinking beside
        // the content. It leads the turn so it stays ahead of the text it
        // produced when the blocks are emitted again.
        if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str) {
            blocks.push(CanonBlock::Thinking(reasoning.to_string()));
        }
        match message.get("content") {
            Some(Value::String(t)) if !t.is_empty() => blocks.push(CanonBlock::Text(t.clone())),
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                blocks.push(CanonBlock::Text(t.to_string()));
                            }
                        }
                        Some("image_url") => {
                            if let Some(u) = part
                                .get("image_url")
                                .and_then(|i| i.get("url"))
                                .and_then(Value::as_str)
                            {
                                blocks.push(CanonBlock::Image(u.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        for call in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            blocks.push(CanonBlock::ToolUse {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input: call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or_else(|| json!({})),
            });
        }
        if role == "tool" {
            blocks = vec![CanonBlock::ToolResult {
                id: message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: message
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_error: false,
            }];
        }
        if blocks.is_empty() {
            continue;
        }
        messages.push(CanonMessage {
            role: match role {
                "assistant" => CanonRole::Assistant,
                "tool" => CanonRole::Tool,
                _ => CanonRole::User,
            },
            blocks,
        });
    }

    CanonRequest {
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        system,
        messages,
        tools: body
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| {
                        let f = t.get("function")?;
                        Some(CanonTool {
                            name: f.get("name")?.as_str()?.to_string(),
                            description: f
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            schema: f
                                .get("parameters")
                                .cloned()
                                .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tool_choice: body.get("tool_choice").and_then(|c| match c {
            Value::String(s) => Some(match s.as_str() {
                "required" => CanonToolChoice::Required,
                "none" => CanonToolChoice::None,
                _ => CanonToolChoice::Auto,
            }),
            Value::Object(_) => c
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(|n| CanonToolChoice::Named(n.to_string())),
            _ => None,
        }),
        max_tokens: body
            .get("max_tokens")
            .or_else(|| body.get("max_completion_tokens"))
            .and_then(Value::as_u64),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        stream: body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stop: body
            .get("stop")
            .and_then(Value::as_array)
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        reasoning_effort: body
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub fn emit_request(req: &CanonRequest, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({"role":"system","content":system}));
    }
    for message in &req.messages {
        // A tool result must be its own message, immediately after the
        // assistant turn that called it — the target rejects the
        // conversation otherwise.
        let mut parts: Vec<Value> = Vec::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut reasoning: Option<String> = None;
        for block in &message.blocks {
            match block {
                CanonBlock::Text(t) => parts.push(json!({"type":"text","text":t})),
                CanonBlock::Image(url) => {
                    parts.push(json!({"type":"image_url","image_url":{"url":url}}))
                }
                CanonBlock::ToolUse { id, name, input } => calls.push(json!({
                    "id":id,"type":"function",
                    "function":{"name":name,"arguments":serde_json::to_string(input).unwrap_or_else(|_| "{}".into())}
                })),
                CanonBlock::ToolResult { id, text, .. } => {
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":text}));
                }
                // A thinking target rejects a turn whose reasoning it
                // issued and cannot see again, so it is carried rather
                // than dropped (spec 0181).
                CanonBlock::Thinking(t) => {
                    reasoning.get_or_insert_with(String::new).push_str(t);
                }
            }
        }
        if parts.is_empty() && calls.is_empty() {
            continue;
        }
        let mut m = Map::new();
        m.insert(
            "role".into(),
            json!(match message.role {
                CanonRole::Assistant => "assistant",
                _ => "user",
            }),
        );
        // Collapse a lone text part to a plain string — the shape every
        // OpenAI-compatible vendor accepts, including the stricter ones.
        match parts.len() {
            0 => {
                m.insert("content".into(), Value::Null);
            }
            1 if parts[0].get("type").and_then(Value::as_str) == Some("text") => {
                m.insert("content".into(), parts[0]["text"].clone());
            }
            _ => {
                m.insert("content".into(), json!(parts));
            }
        }
        if !calls.is_empty() {
            m.insert("tool_calls".into(), json!(calls));
        }
        if let Some(reasoning) = reasoning.filter(|_| message.role == CanonRole::Assistant) {
            m.insert("reasoning_content".into(), json!(reasoning));
        }
        messages.push(Value::Object(m));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), json!(messages));
    if let Some(m) = req.max_tokens {
        out.insert("max_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        out.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        out.insert("stop".into(), json!(req.stop));
    }
    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type":"function",
                    "function":{"name":t.name,"description":t.description,"parameters":t.schema}
                }))
                .collect::<Vec<_>>()),
        );
    }
    if let Some(choice) = req.tool_choice.as_ref().filter(|_| !req.tools.is_empty()) {
        out.insert(
            "tool_choice".into(),
            match choice {
                CanonToolChoice::Auto => json!("auto"),
                CanonToolChoice::Required => json!("required"),
                CanonToolChoice::None => json!("none"),
                CanonToolChoice::Named(n) => json!({"type":"function","function":{"name":n}}),
            },
        );
    }
    if let Some(effort) = &req.reasoning_effort {
        out.insert("reasoning_effort".into(), json!(effort));
    }
    out.insert("stream".into(), json!(req.stream));
    Value::Object(out)
}

pub fn decode_event(data: &Value) -> Vec<CanonEvent> {
    let mut out = Vec::new();
    if let Some(usage) = data.get("usage").filter(|u| !u.is_null()) {
        out.push(CanonEvent::Usage {
            input: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    let Some(choice) = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    else {
        return out;
    };
    if let Some(delta) = choice.get("delta") {
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            out.push(CanonEvent::ThinkingDelta(reasoning.to_string()));
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            out.push(CanonEvent::TextDelta(text.to_string()));
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(name) = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                out.push(CanonEvent::ToolStart {
                    index,
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("toolu_router")
                        .to_string(),
                    name: name.to_string(),
                });
            }
            if let Some(args) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                out.push(CanonEvent::ToolArgsDelta {
                    index,
                    json: args.to_string(),
                });
            }
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        if let Some(stop) = CanonStop::from_openai_finish(Some(reason)) {
            out.push(CanonEvent::Stop { reason: stop });
        }
    }
    out
}

pub fn decode_full_response(body: &Value) -> Vec<CanonEvent> {
    let mut events = vec![CanonEvent::Start {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("cmpl_router")
            .to_string(),
    }];
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));
    if let Some(reasoning) = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        events.push(CanonEvent::ThinkingDelta(reasoning.to_string()));
    }
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        events.push(CanonEvent::TextDelta(text.to_string()));
    }
    for (index, call) in message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        events.push(CanonEvent::ToolStart {
            index,
            id: call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
        events.push(CanonEvent::ToolArgsDelta {
            index,
            json: call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string(),
        });
    }
    if let Some(usage) = body.get("usage") {
        events.push(CanonEvent::Usage {
            input: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    events.push(CanonEvent::Stop {
        reason: CanonStop::from_openai_finish(
            choice
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str),
        )
        .unwrap_or(CanonStop::EndTurn),
    });
    // DeepSeek V4 may put tool intent in content as DSML markup rather
    // than structured tool_calls — lift it so Codex (and any other
    // Responses harness) sees real function_call items (session s8e4420fd3).
    super::dsml::lift_events(events)
}

/// Re-encode canonical events as a Chat Completions SSE stream.
pub struct StreamEncoder {
    model: String,
    id: String,
    started: bool,
    tool_args: BTreeMap<usize, String>,
    usage: Option<(u64, u64)>,
    stop: CanonStop,
}

impl StreamEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            id: "chatcmpl_construct_router".to_string(),
            started: false,
            tool_args: BTreeMap::new(),
            usage: None,
            stop: CanonStop::EndTurn,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Value, usage: Value) -> String {
        sse(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
            "usage": usage,
        }))
    }

    fn start(&mut self) -> String {
        self.started = true;
        self.chunk(
            json!({"role":"assistant","content":""}),
            Value::Null,
            Value::Null,
        )
    }

    pub fn push(&mut self, event: &CanonEvent) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.start());
        }
        match event {
            CanonEvent::Start { id } => {
                if !id.is_empty() {
                    self.id = id.clone();
                }
            }
            CanonEvent::TextDelta(text) => {
                out.push_str(&self.chunk(json!({"content":text}), Value::Null, Value::Null));
            }
            // Reasoning is remembered by the proxy, not replayed to the
            // harness: no client dialect asked for it (spec 0181).
            CanonEvent::ThinkingDelta(_) => {}
            CanonEvent::ToolStart { index, id, name } => {
                if self.tool_args.contains_key(index) {
                    return out;
                }
                self.tool_args.insert(*index, String::new());
                out.push_str(&self.chunk(
                    json!({"tool_calls":[{
                        "index":index,
                        "id":id,
                        "type":"function",
                        "function":{"name":name,"arguments":""},
                    }]}),
                    Value::Null,
                    Value::Null,
                ));
            }
            CanonEvent::ToolArgsDelta { index, json: args } => {
                let Some(buffer) = self.tool_args.get_mut(index) else {
                    return out;
                };
                buffer.push_str(args);
                out.push_str(&self.chunk(
                    json!({"tool_calls":[{
                        "index":index,
                        "function":{"arguments":args},
                    }]}),
                    Value::Null,
                    Value::Null,
                ));
            }
            CanonEvent::Usage { input, output } => self.usage = Some((*input, *output)),
            CanonEvent::Stop { reason } => self.stop = reason.clone(),
        }
        out
    }

    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.start());
        }
        let usage = self
            .usage
            .map(|(input, output)| {
                json!({
                    "prompt_tokens":input,
                    "completion_tokens":output,
                    "total_tokens":input.saturating_add(output),
                })
            })
            .unwrap_or(Value::Null);
        out.push_str(&self.chunk(json!({}), json!(chat_stop_reason(&self.stop)), usage));
        out.push_str("data: [DONE]\n\n");
        out
    }
}

fn chat_stop_reason(stop: &CanonStop) -> &str {
    match stop {
        CanonStop::EndTurn => "stop",
        CanonStop::MaxTokens => "length",
        CanonStop::ToolUse => "tool_calls",
        CanonStop::Other(reason) => reason,
    }
}

fn sse(value: &Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
    )
}

pub fn error_event(message: &str) -> String {
    sse(&json!({
        "error":{
            "message":message,
            "type":"server_error",
            "code":"upstream_error",
        }
    }))
}

/// Build a non-streaming Chat Completions response from canonical events.
pub fn encode_full(events: &[CanonEvent], model: &str) -> Value {
    let mut id = "chatcmpl_construct_router".to_string();
    let mut text = String::new();
    let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
    let mut usage = None;
    let mut stop = CanonStop::EndTurn;
    for event in events {
        match event {
            CanonEvent::Start { id: event_id } if !event_id.is_empty() => id = event_id.clone(),
            CanonEvent::Start { .. } => {}
            CanonEvent::TextDelta(delta) => text.push_str(delta),
            CanonEvent::ThinkingDelta(_) => {}
            CanonEvent::ToolStart { index, id, name } => {
                tools
                    .entry(*index)
                    .or_insert_with(|| (id.clone(), name.clone(), String::new()));
            }
            CanonEvent::ToolArgsDelta { index, json } => {
                if let Some((_, _, args)) = tools.get_mut(index) {
                    args.push_str(json);
                }
            }
            CanonEvent::Usage { input, output } => usage = Some((*input, *output)),
            CanonEvent::Stop { reason } => stop = reason.clone(),
        }
    }

    let tool_calls: Vec<Value> = tools
        .into_iter()
        .map(|(index, (id, name, arguments))| {
            json!({
                "index":index,
                "id":id,
                "type":"function",
                "function":{"name":name,"arguments":arguments},
            })
        })
        .collect();
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), json!(tool_calls));
    }

    json!({
        "id":id,
        "object":"chat.completion",
        "created":0,
        "model":model,
        "choices":[{
            "index":0,
            "message":message,
            "finish_reason":chat_stop_reason(&stop),
        }],
        "usage":match usage {
            Some((input, output)) => json!({
                "prompt_tokens":input,
                "completion_tokens":output,
                "total_tokens":input.saturating_add(output),
            }),
            None => Value::Null,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_becomes_its_own_tool_role_message() {
        let req = CanonRequest {
            messages: vec![
                CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![CanonBlock::ToolUse {
                        id: "t1".into(),
                        name: "ls".into(),
                        input: json!({"p":"/"}),
                    }],
                },
                CanonMessage {
                    role: CanonRole::User,
                    blocks: vec![CanonBlock::ToolResult {
                        id: "t1".into(),
                        text: "a".into(),
                        is_error: false,
                    }],
                },
            ],
            ..Default::default()
        };
        let out = emit_request(&req, "gpt-5.5");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "ls");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "t1");
    }

    /// A thinking target refuses a replayed tool-calling turn whose
    /// reasoning it cannot see again, so the turn has to carry it (spec
    /// 0181) — including the empty reasoning that stands for "no longer
    /// remembered".
    #[test]
    fn an_assistant_turn_carries_its_reasoning_back() {
        let req = CanonRequest {
            messages: vec![
                CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![
                        CanonBlock::Thinking("the repo root is the place to look".into()),
                        CanonBlock::Text("Looking.".into()),
                        CanonBlock::ToolUse {
                            id: "call_1".into(),
                            name: "ls".into(),
                            input: json!({"p": "/"}),
                        },
                    ],
                },
                CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![
                        CanonBlock::Thinking(String::new()),
                        CanonBlock::ToolUse {
                            id: "call_2".into(),
                            name: "ls".into(),
                            input: json!({"p": "/src"}),
                        },
                    ],
                },
            ],
            ..Default::default()
        };
        let out = emit_request(&req, "deepseek-v4-flash");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(
            messages[0]["reasoning_content"],
            "the repo root is the place to look"
        );
        assert_eq!(messages[1]["reasoning_content"], "");
    }

    /// Reasoning must survive a round trip, or replaying a conversation
    /// this dialect produced would strip what the target demands back.
    #[test]
    fn reasoning_round_trips_through_canonical_form() {
        let original = json!({
            "model": "deepseek-v4-flash",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "Looking.",
                 "reasoning_content": "they want a listing",
                 "tool_calls": [{"id":"call_1","type":"function",
                                 "function":{"name":"ls","arguments":"{}"}}]},
            ]
        });
        let canon = parse_request(&original);
        assert_eq!(
            canon.messages[1].blocks[0],
            CanonBlock::Thinking("they want a listing".into())
        );
        let back = emit_request(&canon, "deepseek-v4-flash");
        assert_eq!(back["messages"][1]["reasoning_content"], "they want a listing");
    }

    /// A user turn never carries reasoning, whatever a client put on it.
    #[test]
    fn only_assistant_turns_carry_reasoning() {
        let req = CanonRequest {
            messages: vec![CanonMessage {
                role: CanonRole::User,
                blocks: vec![
                    CanonBlock::Thinking("stray".into()),
                    CanonBlock::Text("hi".into()),
                ],
            }],
            ..Default::default()
        };
        let out = emit_request(&req, "deepseek-v4-flash");
        assert!(out["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn decodes_streamed_and_whole_reasoning() {
        assert_eq!(
            decode_event(&json!({"choices":[{"delta":{"reasoning_content":"weighing"}}]})),
            vec![CanonEvent::ThinkingDelta("weighing".into())]
        );
        let whole = decode_full_response(&json!({
            "id":"cmpl_1",
            "choices":[{"message":{"role":"assistant","reasoning_content":"weighing","content":"ok"},
                        "finish_reason":"stop"}]
        }));
        assert!(whole.contains(&CanonEvent::ThinkingDelta("weighing".into())));
        assert!(whole.contains(&CanonEvent::TextDelta("ok".into())));
    }

    #[test]
    fn omits_tool_choice_when_no_translatable_tools_remain() {
        let req = CanonRequest {
            tool_choice: Some(CanonToolChoice::Auto),
            ..Default::default()
        };
        let out = emit_request(&req, "grok-4.5");
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn decodes_streamed_tool_calls() {
        let chunk = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"ls","arguments":"{\"p\":"}}
        ]}}]});
        assert_eq!(
            decode_event(&chunk),
            vec![
                CanonEvent::ToolStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "ls".into()
                },
                CanonEvent::ToolArgsDelta {
                    index: 0,
                    json: "{\"p\":".into()
                },
            ]
        );
    }

    #[test]
    fn maps_finish_reasons() {
        assert_eq!(
            CanonStop::from_openai_finish(Some("tool_calls")),
            Some(CanonStop::ToolUse)
        );
        assert_eq!(
            CanonStop::from_openai_finish(Some("length")),
            Some(CanonStop::MaxTokens)
        );
        assert_eq!(CanonStop::from_openai_finish(None), None);
    }

    #[test]
    fn encodes_chat_stream_with_tools_usage_and_terminal_sentinel() {
        let mut encoder = StreamEncoder::new("hermes-model");
        let events = [
            CanonEvent::TextDelta("checking".into()),
            CanonEvent::ToolStart {
                index: 0,
                id: "call_1".into(),
                name: "read".into(),
            },
            CanonEvent::ToolArgsDelta {
                index: 0,
                json: "{\"path\":\"/tmp\"}".into(),
            },
            CanonEvent::Usage {
                input: 7,
                output: 3,
            },
            CanonEvent::Stop {
                reason: CanonStop::ToolUse,
            },
        ];
        let mut wire = String::new();
        for event in &events {
            wire.push_str(&encoder.push(event));
        }
        wire.push_str(&encoder.finish());
        assert!(wire.contains("\"object\":\"chat.completion.chunk\""));
        assert!(wire.contains("\"name\":\"read\""));
        assert!(wire.contains("\"finish_reason\":\"tool_calls\""));
        assert!(wire.contains("\"total_tokens\":10"));
        assert!(wire.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn encodes_nonstreaming_chat_completion() {
        let body = encode_full(
            &[
                CanonEvent::TextDelta("done".into()),
                CanonEvent::Usage {
                    input: 2,
                    output: 1,
                },
                CanonEvent::Stop {
                    reason: CanonStop::EndTurn,
                },
            ],
            "hermes-model",
        );
        assert_eq!(body["choices"][0]["message"]["content"], "done");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["total_tokens"], 3);
    }

    /// Live Codex→DeepSeek failure mode: DSML command markup in content
    /// must become a structured shell tool call, not assistant prose.
    #[test]
    fn lifts_deepseek_dsml_command_markup_into_tool_calls() {
        let fw = '\u{ff5c}';
        let content = format!(
            "Let me look.\n\n<{fw}{fw}DSML{fw}{fw}_command>\n  <cmd>ls -la /Users/moon</{fw}{fw}DSML{fw}{fw}_param>\n</{fw}{fw}DSML{fw}{fw}_command>"
        );
        let events = decode_full_response(&json!({
            "id": "cmpl_dsml",
            "choices": [{
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20}
        }));
        assert!(
            events.iter().any(|e| matches!(
                e,
                CanonEvent::ToolStart { name, .. } if name == "shell"
            )),
            "expected shell tool, got {events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::ToolArgsDelta { json, .. } if json.contains("ls -la /Users/moon")
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::Stop {
                reason: CanonStop::ToolUse
            }
        )));
        assert!(
            !events.iter().any(|e| matches!(
                e,
                CanonEvent::TextDelta(t) if t.contains("DSML")
            )),
            "DSML markup must not remain as text: {events:?}"
        );
    }
}
