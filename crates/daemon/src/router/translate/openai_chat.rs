//! OpenAI Chat Completions dialect: parse, emit, decode.
//!
//! No route-capable harness speaks Chat Completions, so this is primarily
//! a *target* dialect. The parser exists so a canonical round trip is
//! testable and so a Chat-speaking harness would need nothing new here.

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
                CanonBlock::Thinking(_) => {}
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
    if let Some(choice) = &req.tool_choice {
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
    events
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
}
