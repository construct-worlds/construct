//! Anthropic Messages dialect: parse, emit, decode, encode.

use serde_json::{json, Map, Value};

use super::{
    sse, CanonBlock, CanonEvent, CanonMessage, CanonRequest, CanonRole, CanonStop, CanonTool,
    CanonToolChoice,
};

pub fn parse_request(body: &Value) -> CanonRequest {
    let system = match body.get("system") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    };

    let mut messages = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("assistant") => CanonRole::Assistant,
            _ => CanonRole::User,
        };
        let blocks = match message.get("content") {
            Some(Value::String(text)) => vec![CanonBlock::Text(text.clone())],
            Some(Value::Array(items)) => items.iter().filter_map(parse_block).collect(),
            _ => Vec::new(),
        };
        if !blocks.is_empty() {
            messages.push(CanonMessage { role, blocks });
        }
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
                        Some(CanonTool {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            schema: t
                                .get("input_schema")
                                .cloned()
                                .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                            freeform: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tool_choice: body.get("tool_choice").and_then(|c| {
            Some(match c.get("type")?.as_str()? {
                "auto" => CanonToolChoice::Auto,
                "any" => CanonToolChoice::Required,
                "none" => CanonToolChoice::None,
                "tool" => CanonToolChoice::Named(c.get("name")?.as_str()?.to_string()),
                _ => return None,
            })
        }),
        max_tokens: body.get("max_tokens").and_then(Value::as_u64),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        stream: body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stop: body
            .get("stop_sequences")
            .and_then(Value::as_array)
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        // Anthropic carries no request-level effort knob; its `thinking`
        // budget is a different control and is not mapped here.
        reasoning_effort: None,
    }
}

fn parse_block(block: &Value) -> Option<CanonBlock> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(CanonBlock::Text(
            block.get("text").and_then(Value::as_str)?.to_string(),
        )),
        "thinking" => Some(CanonBlock::Thinking(
            block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        "image" => {
            let source = block.get("source")?;
            if let Some(url) = source.get("url").and_then(Value::as_str) {
                return Some(CanonBlock::Image(url.to_string()));
            }
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source.get("data").and_then(Value::as_str)?;
            Some(CanonBlock::Image(format!("data:{media};base64,{data}")))
        }
        "tool_use" => Some(CanonBlock::ToolUse {
            id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: block.get("input").cloned().unwrap_or_else(|| json!({})),
        }),
        "tool_result" => Some(CanonBlock::ToolResult {
            id: block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            text: match block.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            },
            is_error: block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

fn thinking_budget(effort: &str) -> Option<u64> {
    match effort {
        "low" => Some(4_096),
        "medium" => Some(12_288),
        "high" | "xhigh" => Some(24_576),
        _ => None,
    }
}

pub fn emit_request(req: &CanonRequest, model: &str) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    let forces_tool = !req.tools.is_empty()
        && matches!(
            req.tool_choice,
            Some(CanonToolChoice::Required) | Some(CanonToolChoice::Named(_))
        );
    let budget = req.reasoning_effort.as_deref()
        .and_then(thinking_budget)
        .filter(|_| !forces_tool);
    // Anthropic requires max_tokens; a canonical request that lost it
    // (Responses makes it optional) still has to carry one. Thinking
    // requires max_tokens to exceed its budget.
    let mut max_tokens = req.max_tokens.unwrap_or(4096);
    if let Some(budget) = budget {
        max_tokens = max_tokens.max(budget + 8_192);
        out.insert("thinking".into(), json!({
            "type": "enabled",
            "budget_tokens": budget
        }));
    }
    out.insert("max_tokens".into(), json!(max_tokens));
    if let Some(system) = &req.system {
        out.insert("system".into(), json!(system));
    }
    let mut messages = Vec::new();
    for message in &req.messages {
        let role = match message.role {
            CanonRole::Assistant => "assistant",
            // Anthropic has no tool role: results ride in a user turn.
            _ => "user",
        };
        let mut content = Vec::new();
        for block in &message.blocks {
            match block {
                CanonBlock::Text(t) => content.push(json!({"type":"text","text":t})),
                CanonBlock::Image(url) => {
                    if let Some(rest) = url.strip_prefix("data:") {
                        if let Some((media, data)) = rest.split_once(";base64,") {
                            content.push(json!({
                                "type":"image",
                                "source":{"type":"base64","media_type":media,"data":data}
                            }));
                        }
                    } else {
                        content.push(json!({
                            "type":"image","source":{"type":"url","url":url}
                        }));
                    }
                }
                CanonBlock::ToolUse { id, name, input } => content.push(json!({
                    "type":"tool_use","id":id,"name":name,"input":input
                })),
                CanonBlock::ToolResult { id, text, is_error } => content.push(json!({
                    "type":"tool_result","tool_use_id":id,"content":text,"is_error":is_error
                })),
                // Anthropic accepts thinking blocks only with matching
                // signatures it issued itself; replaying a foreign one is
                // rejected, so it is dropped rather than forged.
                CanonBlock::Thinking(_) => {}
            }
        }
        if !content.is_empty() {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    out.insert("messages".into(), json!(messages));
    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema_for_json_target(),
                }))
                .collect::<Vec<_>>()),
        );
    }
    if let Some(choice) = req.tool_choice.as_ref().filter(|_| !req.tools.is_empty()) {
        let v = match choice {
            CanonToolChoice::Auto => json!({"type":"auto"}),
            CanonToolChoice::Required => json!({"type":"any"}),
            CanonToolChoice::None => json!({"type":"none"}),
            CanonToolChoice::Named(n) => json!({"type":"tool","name":n}),
        };
        out.insert("tool_choice".into(), v);
    }
    // Extended thinking is incompatible with custom sampling controls.
    if budget.is_none() {
        if let Some(t) = req.temperature {
            out.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.top_p {
            out.insert("top_p".into(), json!(p));
        }
    }
    if !req.stop.is_empty() {
        out.insert("stop_sequences".into(), json!(req.stop));
    }
    out.insert("stream".into(), json!(req.stream));
    Value::Object(out)
}

/// Decode one Anthropic SSE payload into canonical events.
pub fn decode_event(data: &Value) -> Vec<CanonEvent> {
    let Some(kind) = data.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match kind {
        "message_start" => {
            let id = data
                .get("message")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("msg_router")
                .to_string();
            vec![CanonEvent::Start { id }]
        }
        "content_block_start" => {
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = data.get("content_block");
            match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                Some("tool_use") => vec![CanonEvent::ToolStart {
                    index,
                    id: block
                        .and_then(|b| b.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .and_then(|b| b.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }],
                _ => Vec::new(),
            }
        }
        "content_block_delta" => {
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = data.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                Some("text_delta") => delta
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .map(|t| vec![CanonEvent::TextDelta(t.to_string())])
                    .unwrap_or_default(),
                Some("input_json_delta") => delta
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str)
                    .map(|j| {
                        vec![CanonEvent::ToolArgsDelta {
                            index,
                            json: j.to_string(),
                        }]
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        "message_delta" => {
            let mut out = Vec::new();
            if let Some(usage) = data.get("usage") {
                out.push(CanonEvent::Usage {
                    input: usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                });
            }
            let reason = data
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str);
            out.push(CanonEvent::Stop {
                reason: match reason {
                    Some("max_tokens") => CanonStop::MaxTokens,
                    Some("tool_use") => CanonStop::ToolUse,
                    Some("end_turn") | None => CanonStop::EndTurn,
                    Some(other) => CanonStop::Other(other.to_string()),
                },
            });
            out
        }
        _ => Vec::new(),
    }
}

/// Decode a non-streaming Anthropic response into canonical events.
pub fn decode_full_response(body: &Value) -> Vec<CanonEvent> {
    let mut events = vec![CanonEvent::Start {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg_router")
            .to_string(),
    }];
    let mut tool_index = 0usize;
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    events.push(CanonEvent::TextDelta(t.to_string()));
                }
            }
            Some("tool_use") => {
                events.push(CanonEvent::ToolStart {
                    index: tool_index,
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
                events.push(CanonEvent::ToolArgsDelta {
                    index: tool_index,
                    json: block
                        .get("input")
                        .map(|i| serde_json::to_string(i).unwrap_or_else(|_| "{}".into()))
                        .unwrap_or_else(|| "{}".into()),
                });
                tool_index += 1;
            }
            _ => {}
        }
    }
    if let Some(usage) = body.get("usage") {
        events.push(CanonEvent::Usage {
            input: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    events.push(CanonEvent::Stop {
        reason: match body.get("stop_reason").and_then(Value::as_str) {
            Some("max_tokens") => CanonStop::MaxTokens,
            Some("tool_use") => CanonStop::ToolUse,
            _ => CanonStop::EndTurn,
        },
    });
    events
}

/// Build a non-streaming Anthropic message from canonical events.
pub fn encode_full(events: &[CanonEvent], model: &str) -> Value {
    let mut content = Vec::new();
    let mut text = String::new();
    let mut tools: Vec<(String, String, String)> = Vec::new();
    let mut usage = (0u64, 0u64);
    let mut stop = CanonStop::EndTurn;
    for event in events {
        match event {
            CanonEvent::TextDelta(t) => text.push_str(t),
            CanonEvent::ThinkingDelta(_) => {}
            CanonEvent::ToolStart { id, name, .. } => {
                tools.push((id.clone(), name.clone(), String::new()))
            }
            CanonEvent::ToolArgsDelta { json, .. } => {
                if let Some(last) = tools.last_mut() {
                    last.2.push_str(json);
                }
            }
            CanonEvent::Usage { input, output } => usage = (*input, *output),
            CanonEvent::Stop { reason } => stop = reason.clone(),
            CanonEvent::Start { .. } => {}
        }
    }
    if !text.is_empty() {
        content.push(json!({"type":"text","text":text}));
    }
    for (id, name, args) in tools {
        content.push(json!({
            "type":"tool_use","id":id,"name":name,
            "input": serde_json::from_str::<Value>(&args).unwrap_or_else(|_| json!({})),
        }));
    }
    json!({
        "id":"msg_router","type":"message","role":"assistant","model":model,
        "content":content,
        "stop_reason": stop_reason_str(&stop),
        "stop_sequence": Value::Null,
        "usage": {"input_tokens":usage.0,"output_tokens":usage.1},
    })
}

fn stop_reason_str(stop: &CanonStop) -> Value {
    match stop {
        CanonStop::EndTurn => json!("end_turn"),
        CanonStop::MaxTokens => json!("max_tokens"),
        CanonStop::ToolUse => json!("tool_use"),
        CanonStop::Other(o) => json!(o),
    }
}

/// Anthropic brackets every content block with explicit start/stop events
/// and numbers them. A client fed an unbracketed or misnumbered stream
/// renders nothing at all, so the bracketing — not the text — is the part
/// that has to be right.
pub struct StreamEncoder {
    model: String,
    started: bool,
    open: Option<Open>,
    index: usize,
    tools: Vec<usize>,
    stop: CanonStop,
    output_tokens: u64,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Open {
    Text,
    Tool,
}

impl StreamEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            open: None,
            index: 0,
            tools: Vec::new(),
            stop: CanonStop::EndTurn,
            output_tokens: 0,
        }
    }

    pub fn push(&mut self, event: &CanonEvent) -> String {
        let mut out = String::new();
        if !self.started {
            self.started = true;
            out.push_str(&sse(
                "message_start",
                &json!({
                    "type":"message_start",
                    "message":{
                        "id":"msg_router","type":"message","role":"assistant",
                        "model":self.model,"content":[],
                        "stop_reason":Value::Null,"stop_sequence":Value::Null,
                        "usage":{"input_tokens":0,"output_tokens":0}
                    }
                }),
            ));
        }
        match event {
            CanonEvent::Start { .. } => {}
            CanonEvent::TextDelta(text) => {
                if self.open != Some(Open::Text) {
                    out.push_str(&self.close_open());
                    out.push_str(&sse(
                        "content_block_start",
                        &json!({
                            "type":"content_block_start","index":self.index,
                            "content_block":{"type":"text","text":""}
                        }),
                    ));
                    self.open = Some(Open::Text);
                }
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({
                        "type":"content_block_delta","index":self.index,
                        "delta":{"type":"text_delta","text":text}
                    }),
                ));
            }
            CanonEvent::ThinkingDelta(_) => {}
            CanonEvent::ToolStart { index, id, name } => {
                if !self.tools.contains(index) {
                    out.push_str(&self.close_open());
                    out.push_str(&sse(
                        "content_block_start",
                        &json!({
                            "type":"content_block_start","index":self.index,
                            "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}
                        }),
                    ));
                    self.open = Some(Open::Tool);
                    self.tools.push(*index);
                }
            }
            CanonEvent::ToolArgsDelta { json: args, .. } => {
                out.push_str(&sse(
                    "content_block_delta",
                    &json!({
                        "type":"content_block_delta","index":self.index,
                        "delta":{"type":"input_json_delta","partial_json":args}
                    }),
                ));
            }
            CanonEvent::Usage { output, .. } => self.output_tokens = *output,
            CanonEvent::Stop { reason } => self.stop = reason.clone(),
        }
        out
    }

    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.push(&CanonEvent::Start {
                id: "msg_router".into(),
            }));
        }
        out.push_str(&self.close_open());
        out.push_str(&sse(
            "message_delta",
            &json!({
                "type":"message_delta",
                "delta":{"stop_reason":stop_reason_str(&self.stop),"stop_sequence":Value::Null},
                "usage":{"output_tokens":self.output_tokens}
            }),
        ));
        out.push_str(&sse("message_stop", &json!({"type":"message_stop"})));
        out
    }

    fn close_open(&mut self) -> String {
        if self.open.take().is_none() {
            return String::new();
        }
        let out = sse(
            "content_block_stop",
            &json!({"type":"content_block_stop","index":self.index}),
        );
        self.index += 1;
        out
    }
}

pub fn error_event(message: &str) -> String {
    sse(
        "error",
        &json!({"type":"error","error":{"type":"api_error","message":message}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::translate::tests_support::sse_events;

    #[test]
    fn brackets_text_and_tool_blocks_with_distinct_indices() {
        let mut enc = StreamEncoder::new("kimi");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::TextDelta("Hi".into())));
        raw.push_str(&enc.push(&CanonEvent::ToolStart {
            index: 0,
            id: "call_1".into(),
            name: "ls".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::ToolArgsDelta {
            index: 0,
            json: "{}".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::Stop {
            reason: CanonStop::ToolUse,
        }));
        raw.push_str(&enc.finish());

        let events = sse_events(&raw);
        let starts: Vec<&serde_json::Value> = events
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[1]["index"], 1);
        assert_eq!(starts[1]["content_block"]["type"], "tool_use");
        let (_, delta) = events.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn emits_a_complete_turn_even_with_no_content() {
        let mut enc = StreamEncoder::new("m");
        let names: Vec<String> = sse_events(&enc.finish())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn max_tokens_is_supplied_when_the_source_dialect_omits_it() {
        let req = CanonRequest {
            max_tokens: None,
            ..Default::default()
        };
        let out = emit_request(&req, "claude-opus-5");
        assert_eq!(out["max_tokens"], 4096, "Anthropic rejects a missing max_tokens");
    }

    #[test]
    fn reasoning_effort_maps_to_extended_thinking_budget() {
        let req = CanonRequest {
            reasoning_effort: Some("medium".into()),
            max_tokens: Some(1024),
            temperature: Some(0.2),
            top_p: Some(0.9),
            ..Default::default()
        };
        let out = emit_request(&req, "claude-opus-5");
        assert_eq!(out["thinking"], serde_json::json!({
            "type":"enabled","budget_tokens":12_288
        }));
        assert_eq!(out["max_tokens"], 20_480);
        assert!(out.get("temperature").is_none());
        assert!(out.get("top_p").is_none());
    }

    #[test]
    fn minimal_effort_leaves_thinking_off() {
        let out = emit_request(&CanonRequest {
            reasoning_effort: Some("minimal".into()),
            ..Default::default()
        }, "claude-opus-5");
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn decodes_its_own_stream_back_into_canonical_events() {
        let start = serde_json::json!({"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"t1","name":"ls"}});
        assert_eq!(
            decode_event(&start),
            vec![CanonEvent::ToolStart {
                index: 1,
                id: "t1".into(),
                name: "ls".into()
            }]
        );
        let delta = serde_json::json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"hi"}});
        assert_eq!(decode_event(&delta), vec![CanonEvent::TextDelta("hi".into())]);
    }
}
