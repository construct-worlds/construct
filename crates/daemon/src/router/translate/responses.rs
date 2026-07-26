//! OpenAI Responses dialect: parse (request) and encode (response stream).
//!
//! This is the dialect four harnesses speak — codex and pi to
//! `chatgpt.com/backend-api/codex/responses`, grok and opencode to
//! `/v1/responses` on their own hosts — so it is a *client* dialect here.
//! No configurable route target speaks it, which is why there is no
//! request emitter.
//!
//! The event vocabulary below was captured from a real harness turn rather
//! than written from memory: names and payload shapes come from an
//! intercepted, forwarded stream. Responses is far more structured than
//! Chat Completions — output items are opened and closed, content parts
//! within them are opened and closed, and both carry indices — so an
//! encoder that emits deltas without the surrounding items produces a
//! stream the harness renders as nothing.

use serde_json::{json, Map, Value};

use super::{
    sse, CanonBlock, CanonEvent, CanonMessage, CanonRequest, CanonRole, CanonStop, CanonTool,
    CanonToolChoice,
};

pub fn parse_request(body: &Value) -> CanonRequest {
    let system = body
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut messages = Vec::new();
    for item in body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        // Input items are a union: messages, function calls, and function
        // call outputs sit side by side in one list.
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                messages.push(CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![CanonBlock::ToolUse {
                        id: item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        input: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .and_then(|a| serde_json::from_str(a).ok())
                            .unwrap_or_else(|| json!({})),
                    }],
                });
                continue;
            }
            Some("function_call_output") => {
                messages.push(CanonMessage {
                    role: CanonRole::Tool,
                    blocks: vec![CanonBlock::ToolResult {
                        id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        text: match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => serde_json::to_string(other).unwrap_or_default(),
                            None => String::new(),
                        },
                        is_error: false,
                    }],
                });
                continue;
            }
            // `reasoning` items carry the model's own encrypted summary and
            // have no counterpart anywhere else; replaying them would put
            // words in the assistant's mouth.
            Some("reasoning") => continue,
            _ => {}
        }

        let role = match item.get("role").and_then(Value::as_str) {
            Some("assistant") => CanonRole::Assistant,
            // Responses uses `developer` where Chat uses `system`.
            Some("system") | Some("developer") => CanonRole::System,
            _ => CanonRole::User,
        };
        let mut blocks = Vec::new();
        match item.get("content") {
            Some(Value::String(t)) => blocks.push(CanonBlock::Text(t.clone())),
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("input_text") | Some("output_text") | Some("text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                blocks.push(CanonBlock::Text(t.to_string()));
                            }
                        }
                        Some("input_image") => {
                            if let Some(u) = part.get("image_url").and_then(Value::as_str) {
                                blocks.push(CanonBlock::Image(u.to_string()));
                            } else if let Some(u) = part
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
        if blocks.is_empty() {
            continue;
        }
        if role == CanonRole::System {
            // Fold a system item into the canonical system prompt rather
            // than carrying a role no target reliably accepts mid-list.
            let text = blocks
                .iter()
                .filter_map(|b| match b {
                    CanonBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            messages.insert(
                0,
                CanonMessage {
                    role: CanonRole::System,
                    blocks: vec![CanonBlock::Text(text)],
                },
            );
            continue;
        }
        messages.push(CanonMessage { role, blocks });
    }

    // A `system` role item and `instructions` can both be present; the
    // instructions win and any system item is appended after it.
    let mut system_text = system;
    messages.retain(|m| {
        if m.role != CanonRole::System {
            return true;
        }
        if let Some(CanonBlock::Text(t)) = m.blocks.first() {
            system_text = Some(match system_text.take() {
                Some(existing) => format!("{existing}\n{t}"),
                None => t.clone(),
            });
        }
        false
    });

    CanonRequest {
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        system: system_text,
        messages,
        tools: body
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| {
                        // Responses puts the function fields flat on the
                        // tool, unlike Chat's nested `function` object.
                        Some(CanonTool {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            schema: t
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
                .get("name")
                .and_then(Value::as_str)
                .map(|n| CanonToolChoice::Named(n.to_string())),
            _ => None,
        }),
        max_tokens: body.get("max_output_tokens").and_then(Value::as_u64),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        stream: body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stop: Vec::new(),
    }
}

/// Emit a canonical request as OpenAI Responses.
///
/// Needed because a subscription login can be a *target*: the Codex
/// backend speaks Responses and nothing else (spec 0117).
pub fn emit_request(req: &CanonRequest, model: &str) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for message in &req.messages {
        let mut parts: Vec<Value> = Vec::new();
        for block in &message.blocks {
            match block {
                CanonBlock::Text(t) => {
                    // The text part type differs by who is speaking, and
                    // the backend rejects the wrong one.
                    let kind = match message.role {
                        CanonRole::Assistant => "output_text",
                        _ => "input_text",
                    };
                    parts.push(json!({"type": kind, "text": t}));
                }
                CanonBlock::Image(url) => {
                    parts.push(json!({"type":"input_image","image_url":url}))
                }
                CanonBlock::ToolUse { id, name, input: args } => {
                    // Function calls are their own input items, not content
                    // parts inside a message.
                    input.push(json!({
                        "type":"function_call","call_id":id,"name":name,
                        "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                    }));
                }
                CanonBlock::ToolResult { id, text, .. } => {
                    input.push(json!({
                        "type":"function_call_output","call_id":id,"output":text,
                    }));
                }
                // Reasoning items are encrypted and bound to the response
                // that produced them; a foreign one is rejected.
                CanonBlock::Thinking(_) => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        input.push(json!({
            "type":"message",
            "role": match message.role {
                CanonRole::Assistant => "assistant",
                CanonRole::System => "developer",
                _ => "user",
            },
            "content": parts,
        }));
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    if let Some(system) = &req.system {
        out.insert("instructions".into(), json!(system));
    }
    out.insert("input".into(), json!(input));
    if let Some(m) = req.max_tokens {
        out.insert("max_output_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        out.insert("temperature".into(), json!(t));
    }
    if !req.tools.is_empty() {
        out.insert(
            "tools".into(),
            json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type":"function","name":t.name,
                    "description":t.description,"parameters":t.schema
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
                CanonToolChoice::Named(n) => json!({"type":"function","name":n}),
            },
        );
    }
    out.insert("stream".into(), json!(req.stream));
    // The router keeps no server-side conversation state: every turn is
    // sent whole, so storing it would leave orphaned state behind.
    out.insert("store".into(), json!(false));
    Value::Object(out)
}

/// Decode one Responses SSE payload into canonical events.
///
/// Dispatches on the payload's own `type`, not the SSE `event:` line, so it
/// works on a stream where only `data:` frames are forwarded.
pub fn decode_event(data: &Value) -> Vec<CanonEvent> {
    let Some(kind) = data.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let output_index = data
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    match kind {
        "response.created" => vec![CanonEvent::Start {
            id: data
                .get("response")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("resp_router")
                .to_string(),
        }],
        "response.output_text.delta" => data
            .get("delta")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(|d| vec![CanonEvent::TextDelta(d.to_string())])
            .unwrap_or_default(),
        "response.output_item.added" => {
            let item = data.get("item");
            match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                Some("function_call") => vec![CanonEvent::ToolStart {
                    index: output_index,
                    id: item
                        .and_then(|i| i.get("call_id"))
                        .or_else(|| item.and_then(|i| i.get("id")))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .and_then(|i| i.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }],
                _ => Vec::new(),
            }
        }
        "response.function_call_arguments.delta" => data
            .get("delta")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(|d| {
                vec![CanonEvent::ToolArgsDelta {
                    index: output_index,
                    json: d.to_string(),
                }]
            })
            .unwrap_or_default(),
        "response.completed" | "response.incomplete" | "response.failed" => {
            let mut out = Vec::new();
            let response = data.get("response");
            if let Some(usage) = response.and_then(|r| r.get("usage")).filter(|u| !u.is_null()) {
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
            let status = response
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed");
            out.push(CanonEvent::Stop {
                reason: match status {
                    "incomplete" => CanonStop::MaxTokens,
                    "completed" => CanonStop::EndTurn,
                    other => CanonStop::Other(other.to_string()),
                },
            });
            out
        }
        _ => Vec::new(),
    }
}

/// Decode a non-streaming Responses body into canonical events.
pub fn decode_full_response(body: &Value) -> Vec<CanonEvent> {
    let mut events = vec![CanonEvent::Start {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp_router")
            .to_string(),
    }];
    let mut tool_index = 0usize;
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        events.push(CanonEvent::TextDelta(t.to_string()));
                    }
                }
            }
            Some("function_call") => {
                events.push(CanonEvent::ToolStart {
                    index: tool_index,
                    id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
                events.push(CanonEvent::ToolArgsDelta {
                    index: tool_index,
                    json: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string(),
                });
                tool_index += 1;
            }
            _ => {}
        }
    }
    if let Some(usage) = body.get("usage").filter(|u| !u.is_null()) {
        events.push(CanonEvent::Usage {
            input: usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            output: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    events.push(CanonEvent::Stop {
        reason: match body.get("status").and_then(Value::as_str) {
            Some("incomplete") => CanonStop::MaxTokens,
            _ => CanonStop::EndTurn,
        },
    });
    events
}

/// Builds a Responses event stream from canonical events.
///
/// Responses nests two levels: output *items* (a message, a function
/// call) and, inside a message item, content *parts*. Both are explicitly
/// opened and closed and both are indexed, and `sequence_number` runs
/// across the whole stream. Emitting a text delta without its enclosing
/// item and part is the classic way to produce a stream that parses but
/// displays nothing.
pub struct StreamEncoder {
    model: String,
    response_id: String,
    seq: u64,
    started: bool,
    output_index: usize,
    message_open: bool,
    /// Canonical tool index → output index it was opened at.
    tools: Vec<(usize, String)>,
    /// Accumulated assistant text — Responses repeats the full text on
    /// `output_text.done` and on the closing item, not just the deltas.
    text: String,
    /// Accumulated arguments per open function call, for the same reason.
    tool_args: Vec<String>,
    /// Items already closed, replayed on `response.completed`. A client
    /// that reconstructs the turn from the terminal event needs them.
    completed_items: Vec<Value>,
    /// Unix seconds. Part of the response object every client deserializes.
    created_at: i64,
    stop: CanonStop,
    usage: Option<(u64, u64)>,
}

impl StreamEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            response_id: "resp_construct_router".to_string(),
            seq: 0,
            started: false,
            output_index: 0,
            message_open: false,
            tools: Vec::new(),
            text: String::new(),
            tool_args: Vec::new(),
            completed_items: Vec::new(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            stop: CanonStop::EndTurn,
            usage: None,
        }
    }

    fn next(&mut self) -> u64 {
        let n = self.seq;
        self.seq += 1;
        n
    }

    /// The `response` object carried by `response.created`,
    /// `response.in_progress` and `response.completed`.
    ///
    /// Clients deserialize this into a struct with required fields, so a
    /// minimal object is rejected outright — a real harness failed with
    /// `missing field created_at` against an earlier version of this. The
    /// shape below is a **superset of a captured response from a live
    /// endpoint**: every field the real backend sends is emitted here, so
    /// no client struct can find one absent. Fields are present even when
    /// null, because absent and null are not the same thing to a strict
    /// deserializer. Extra fields are ignored by every client; missing ones
    /// fail the whole turn.
    fn response_object(&self, status: &str) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": if status == "in_progress" {
                Value::Null
            } else {
                json!(self.created_at)
            },
            "status": status,
            "model": self.model,
            "output": self.completed_items,
            "error": Value::Null,
            "incomplete_details": Value::Null,
            "instructions": Value::Null,
            "max_output_tokens": Value::Null,
            "previous_response_id": Value::Null,
            "parallel_tool_calls": true,
            "reasoning": {"effort": Value::Null, "summary": Value::Null},
            "store": false,
            "temperature": 1.0,
            "top_p": 1.0,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "truncation": "disabled",
            "metadata": {},
            "user": Value::Null,
            "background": false,
            "service_tier": "default",
            "top_logprobs": 0,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "prompt_cache_key": Value::Null,
            "usage": match self.usage {
                Some((i, o)) => json!({
                    "input_tokens": i,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": o,
                    "output_tokens_details": {"reasoning_tokens": 0},
                    "total_tokens": i + o,
                }),
                None => Value::Null,
            },
        })
    }

    fn start(&mut self) -> String {
        let mut out = String::new();
        let seq = self.next();
        out.push_str(&sse(
            "response.created",
            &json!({
                "type":"response.created",
                "sequence_number": seq,
                "response": self.response_object("in_progress"),
            }),
        ));
        let seq = self.next();
        out.push_str(&sse(
            "response.in_progress",
            &json!({
                "type":"response.in_progress",
                "sequence_number": seq,
                "response": self.response_object("in_progress"),
            }),
        ));
        self.started = true;
        out
    }

    fn open_message(&mut self) -> String {
        let mut out = String::new();
        let item_id = self.message_item_id();
        let seq = self.next();
        out.push_str(&sse(
            "response.output_item.added",
            &json!({
                "type":"response.output_item.added",
                "sequence_number": seq,
                "output_index": self.output_index,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ));
        let seq = self.next();
        out.push_str(&sse(
            "response.content_part.added",
            &json!({
                "type":"response.content_part.added",
                "sequence_number": seq,
                "output_index": self.output_index,
                "content_index": 0,
                "item_id": item_id,
                "part": {"type":"output_text","text":"","annotations":[],"logprobs":[]},
            }),
        ));
        self.message_open = true;
        out
    }

    fn close_message(&mut self, text: &str) -> String {
        if !self.message_open {
            return String::new();
        }
        let item_id = self.message_item_id();
        let mut out = String::new();
        let seq = self.next();
        out.push_str(&sse(
            "response.output_text.done",
            &json!({
                "type":"response.output_text.done",
                "sequence_number": seq,
                "output_index": self.output_index,
                "content_index": 0,
                "item_id": item_id,
                "text": text,
            }),
        ));
        let seq = self.next();
        out.push_str(&sse(
            "response.content_part.done",
            &json!({
                "type":"response.content_part.done",
                "sequence_number": seq,
                "output_index": self.output_index,
                "content_index": 0,
                "item_id": item_id,
                "part": {"type":"output_text","text":text,"annotations":[],"logprobs":[]},
            }),
        ));
        let seq = self.next();
        out.push_str(&sse(
            "response.output_item.done",
            &json!({
                "type":"response.output_item.done",
                "sequence_number": seq,
                "output_index": self.output_index,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type":"output_text","text":text,"annotations":[],"logprobs":[]}],
                },
            }),
        ));
        self.completed_items.push(json!({
            "id": item_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type":"output_text","text":text,"annotations":[],"logprobs":[]}],
        }));
        self.message_open = false;
        self.output_index += 1;
        out
    }

    fn message_item_id(&self) -> String {
        format!("msg_{}_{}", self.response_id, self.output_index)
    }

    pub fn push(&mut self, event: &CanonEvent) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.start());
        }
        match event {
            CanonEvent::Start { .. } => {}
            CanonEvent::TextDelta(text) => {
                if !self.message_open {
                    out.push_str(&self.open_message());
                }
                self.text.push_str(text);
                let item_id = self.message_item_id();
                let seq = self.next();
                out.push_str(&sse(
                    "response.output_text.delta",
                    &json!({
                        "type":"response.output_text.delta",
                        "sequence_number": seq,
                        "output_index": self.output_index,
                        "content_index": 0,
                        "item_id": item_id,
                        "delta": text,
                        "logprobs": [],
                    }),
                ));
            }
            CanonEvent::ToolStart { index, id, name } => {
                if self.tools.iter().any(|(i, _)| i == index) {
                    return out;
                }
                let text = std::mem::take(&mut self.text);
                out.push_str(&self.close_message(&text));
                let item_id = format!("fc_{}_{}", self.response_id, self.output_index);
                let seq = self.next();
                out.push_str(&sse(
                    "response.output_item.added",
                    &json!({
                        "type":"response.output_item.added",
                        "sequence_number": seq,
                        "output_index": self.output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": id,
                            "name": name,
                            "arguments": "",
                        },
                    }),
                ));
                self.tools.push((*index, item_id));
                self.tool_args.push(String::new());
            }
            CanonEvent::ToolArgsDelta { index, json: args } => {
                let Some(slot) = self.tools.iter().position(|(i, _)| i == index) else {
                    return out;
                };
                if let Some(buf) = self.tool_args.get_mut(slot) {
                    buf.push_str(args);
                }
                let item_id = self.tools[slot].1.clone();
                let seq = self.next();
                out.push_str(&sse(
                    "response.function_call_arguments.delta",
                    &json!({
                        "type":"response.function_call_arguments.delta",
                        "sequence_number": seq,
                        "output_index": self.output_index,
                        "item_id": item_id,
                        "delta": args,
                    }),
                ));
            }
            CanonEvent::Usage { input, output } => self.usage = Some((*input, *output)),
            CanonEvent::Stop { reason } => self.stop = reason.clone(),
        }
        out
    }

    /// Close every open item and the response itself. Emitted even when
    /// the target produced nothing, so the harness sees a finished turn
    /// rather than a stream that simply stops.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        if !self.started {
            out.push_str(&self.start());
        }
        let text = std::mem::take(&mut self.text);
        out.push_str(&self.close_message(&text));
        // Close any open function-call items, in the order they opened.
        let tools = std::mem::take(&mut self.tools);
        for (slot, (_, item_id)) in tools.iter().enumerate() {
            let args = self.tool_args.get(slot).cloned().unwrap_or_default();
            let seq = self.next();
            out.push_str(&sse(
                "response.function_call_arguments.done",
                &json!({
                    "type":"response.function_call_arguments.done",
                    "sequence_number": seq,
                    "output_index": self.output_index,
                    "item_id": item_id,
                    "arguments": args,
                }),
            ));
            let seq = self.next();
            out.push_str(&sse(
                "response.output_item.done",
                &json!({
                    "type":"response.output_item.done",
                    "sequence_number": seq,
                    "output_index": self.output_index,
                    "item": {
                        "id": item_id,
                        "type": "function_call",
                        "status": "completed",
                        "arguments": args,
                    },
                }),
            ));
            self.completed_items.push(json!({
                "id": item_id,
                "type": "function_call",
                "status": "completed",
                "arguments": args,
            }));
            self.output_index += 1;
        }
        let status = match self.stop {
            CanonStop::MaxTokens => "incomplete",
            _ => "completed",
        };
        let seq = self.next();
        let response = self.response_object(status);
        out.push_str(&sse(
            "response.completed",
            &json!({
                "type":"response.completed",
                "sequence_number": seq,
                "response": response,
            }),
        ));
        out
    }
}

pub fn error_event(message: &str) -> String {
    sse(
        "error",
        &json!({
            "type":"error",
            "code":"api_error",
            "message":message,
            "sequence_number":0,
        }),
    )
}

/// Build a non-streaming Responses body from canonical events.
pub fn encode_full(events: &[CanonEvent], model: &str) -> Value {
    let mut text = String::new();
    let mut output = Vec::new();
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let mut usage = None;
    let mut stop = CanonStop::EndTurn;
    for event in events {
        match event {
            CanonEvent::TextDelta(t) => text.push_str(t),
            CanonEvent::ToolStart { id, name, .. } => {
                calls.push((id.clone(), name.clone(), String::new()))
            }
            CanonEvent::ToolArgsDelta { json: args, .. } => {
                if let Some(last) = calls.last_mut() {
                    last.2.push_str(args);
                }
            }
            CanonEvent::Usage { input, output: o } => usage = Some((*input, *o)),
            CanonEvent::Stop { reason } => stop = reason.clone(),
            CanonEvent::Start { .. } => {}
        }
    }
    if !text.is_empty() {
        output.push(json!({
            "id":"msg_construct_router","type":"message","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":text,"annotations":[]}]
        }));
    }
    for (id, name, args) in calls {
        output.push(json!({
            "id": format!("fc_{id}"), "type":"function_call","status":"completed",
            "call_id": id, "name": name, "arguments": args,
        }));
    }
    json!({
        "id":"resp_construct_router","object":"response","model":model,
        "status": match stop { CanonStop::MaxTokens => "incomplete", _ => "completed" },
        "output": output,
        "usage": match usage {
            Some((i,o)) => json!({"input_tokens":i,"output_tokens":o,"total_tokens":i+o}),
            None => Value::Null,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::translate::tests_support::sse_events;

    #[test]
    fn folds_instructions_into_the_system_prompt() {
        let req = parse_request(&json!({
            "model":"gpt-5.6","instructions":"be terse",
            "input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]
        }));
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].blocks[0], CanonBlock::Text("hi".into()));
    }

    /// grok and opencode send a `system`/`developer` role item instead of
    /// (or as well as) `instructions`.
    #[test]
    fn folds_a_system_role_item_into_the_system_prompt() {
        let req = parse_request(&json!({
            "input":[
                {"type":"message","role":"system","content":"you are a title generator"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]
        }));
        assert_eq!(req.system.as_deref(), Some("you are a title generator"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, CanonRole::User);
    }

    #[test]
    fn reads_flat_tool_definitions() {
        let req = parse_request(&json!({
            "input":[],
            "tools":[{"type":"function","name":"read","description":"d",
                      "parameters":{"type":"object","properties":{}}}]
        }));
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "read");
        assert_eq!(req.tools[0].schema["type"], "object");
    }

    #[test]
    fn reads_function_calls_and_their_outputs() {
        let req = parse_request(&json!({
            "input":[
                {"type":"function_call","call_id":"c1","name":"ls","arguments":"{\"p\":\"/\"}"},
                {"type":"function_call_output","call_id":"c1","output":"a"}
            ]
        }));
        assert_eq!(req.messages.len(), 2);
        match &req.messages[0].blocks[0] {
            CanonBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "ls");
                assert_eq!(input["p"], "/");
            }
            other => panic!("expected tool use, got {other:?}"),
        }
        assert!(matches!(
            &req.messages[1].blocks[0],
            CanonBlock::ToolResult { text, .. } if text == "a"
        ));
    }

    /// Reasoning items carry encrypted, model-private content; replaying
    /// them would fabricate assistant speech.
    #[test]
    fn drops_reasoning_items() {
        let req = parse_request(&json!({
            "input":[
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"hmm"}]},
                {"role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]
        }));
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn maps_max_output_tokens_onto_max_tokens() {
        let req = parse_request(&json!({"input":[],"max_output_tokens":100}));
        assert_eq!(req.max_tokens, Some(100));
    }

    /// The framing check: a text turn must be wrapped in an output item
    /// and a content part, both opened and closed, or it renders as
    /// nothing.
    #[test]
    fn wraps_text_in_item_and_part_brackets() {
        let mut enc = StreamEncoder::new("kimi-k2.5");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::TextDelta("Hi ".into())));
        raw.push_str(&enc.push(&CanonEvent::TextDelta("there".into())));
        raw.push_str(&enc.push(&CanonEvent::Stop {
            reason: CanonStop::EndTurn,
        }));
        raw.push_str(&enc.finish());

        let names: Vec<String> = sse_events(&raw).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        let events = sse_events(&raw);
        let done = events
            .iter()
            .find(|(n, _)| n == "response.output_text.done")
            .unwrap();
        assert_eq!(done.1["text"], "Hi there", "done carries the full text");
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let mut enc = StreamEncoder::new("m");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::TextDelta("a".into())));
        raw.push_str(&enc.finish());
        let seqs: Vec<u64> = sse_events(&raw)
            .iter()
            .filter_map(|(_, v)| v["sequence_number"].as_u64())
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "sequence numbers must run consecutively: {seqs:?}"
        );
    }

    #[test]
    fn emits_function_call_items_with_argument_deltas() {
        let mut enc = StreamEncoder::new("m");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::TextDelta("ok".into())));
        raw.push_str(&enc.push(&CanonEvent::ToolStart {
            index: 0,
            id: "call_1".into(),
            name: "ls".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::ToolArgsDelta {
            index: 0,
            json: "{\"p\":".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::ToolArgsDelta {
            index: 0,
            json: "\"/\"}".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::Stop {
            reason: CanonStop::ToolUse,
        }));
        raw.push_str(&enc.finish());

        let events = sse_events(&raw);
        // The message item must be closed before the function call opens.
        let order: Vec<&str> = events
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| n.ends_with("output_item.added") || n.ends_with("output_item.done"))
            .collect();
        assert_eq!(
            order,
            vec![
                "response.output_item.added",
                "response.output_item.done",
                "response.output_item.added",
                "response.output_item.done"
            ]
        );
        let added: Vec<&serde_json::Value> = events
            .iter()
            .filter(|(n, _)| n == "response.output_item.added")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(added[1]["item"]["type"], "function_call");
        assert_eq!(added[1]["item"]["call_id"], "call_1");
        assert_eq!(added[1]["item"]["name"], "ls");

        let done = events
            .iter()
            .find(|(n, _)| n == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(done.1["arguments"], "{\"p\":\"/\"}");
    }

    #[test]
    fn emits_a_complete_turn_with_no_content() {
        let mut enc = StreamEncoder::new("m");
        let names: Vec<String> = sse_events(&enc.finish())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names,
            vec!["response.created", "response.in_progress", "response.completed"]
        );
    }

    #[test]
    fn reports_usage_and_incomplete_status() {
        let mut enc = StreamEncoder::new("m");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::Usage {
            input: 10,
            output: 3,
        }));
        raw.push_str(&enc.push(&CanonEvent::Stop {
            reason: CanonStop::MaxTokens,
        }));
        raw.push_str(&enc.finish());
        let events = sse_events(&raw);
        let (_, completed) = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        assert_eq!(completed["response"]["status"], "incomplete");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 10);
        assert_eq!(completed["response"]["usage"]["total_tokens"], 13);
    }

    /// REGRESSION: a strict client deserializes the `response` object into
    /// a struct with required fields and rejects the whole turn when one is
    /// missing. A real harness failed with `missing field created_at`
    /// against an encoder that emitted only id/object/model/status/output.
    ///
    /// Event names and ordering were already asserted and were correct the
    /// whole time — the payloads were not. This checks the fields.
    #[test]
    fn the_response_object_carries_what_a_strict_client_requires() {
        let mut enc = StreamEncoder::new("gpt-5.5");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::TextDelta("hi".into())));
        raw.push_str(&enc.push(&CanonEvent::Usage { input: 4, output: 1 }));
        raw.push_str(&enc.push(&CanonEvent::Stop {
            reason: CanonStop::EndTurn,
        }));
        raw.push_str(&enc.finish());

        let events = sse_events(&raw);
        for name in [
            "response.created",
            "response.in_progress",
            "response.completed",
        ] {
            let (_, payload) = events
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no {name}"));
            let response = &payload["response"];
            for field in [
                "id",
                "object",
                "created_at",
                "status",
                "model",
                "output",
                "error",
                "incomplete_details",
                "parallel_tool_calls",
                "tools",
                "tool_choice",
                "text",
                "metadata",
                "usage",
            ] {
                assert!(
                    response.get(field).is_some(),
                    "{name}: response object is missing `{field}`; a strict \
                     client rejects the turn"
                );
            }
            assert!(
                response["created_at"].as_i64().unwrap_or(0) > 0,
                "{name}: created_at must be a real timestamp"
            );
        }

        // The terminal event replays what was produced, so a client that
        // reconstructs the turn from it sees the text.
        let (_, completed) = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        assert_eq!(completed["response"]["output"][0]["type"], "message");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "hi"
        );
        assert_eq!(completed["response"]["usage"]["total_tokens"], 5);
    }

    /// A tool call must also appear in the terminal event's output.
    #[test]
    fn completed_replays_function_call_items() {
        let mut enc = StreamEncoder::new("m");
        let mut raw = String::new();
        raw.push_str(&enc.push(&CanonEvent::ToolStart {
            index: 0,
            id: "call_1".into(),
            name: "ls".into(),
        }));
        raw.push_str(&enc.push(&CanonEvent::ToolArgsDelta {
            index: 0,
            json: "{}".into(),
        }));
        raw.push_str(&enc.finish());
        let events = sse_events(&raw);
        let (_, completed) = events
            .iter()
            .find(|(n, _)| n == "response.completed")
            .unwrap();
        let out = &completed["response"]["output"][0];
        assert_eq!(out["type"], "function_call");
        assert_eq!(out["call_id"].as_str().or(out["id"].as_str()).is_some(), true);
        assert_eq!(out["arguments"], "{}");
    }


    /// The encoder's response object must stay a superset of what the real
    /// endpoint sends. A client struct is written against the real payload,
    /// so any field the backend emits and we omit is a `missing field`
    /// failure for the entire turn — which is exactly how this was found.
    #[test]
    fn the_response_object_is_a_superset_of_the_real_endpoints() {
        // Observed on a live Responses endpoint by interception.
        const OBSERVED: &[&str] = &[
            "created_at", "completed_at", "id", "max_output_tokens", "model",
            "object", "output", "parallel_tool_calls", "previous_response_id",
            "reasoning", "temperature", "text", "tool_choice", "tools",
            "usage", "user", "incomplete_details", "status", "store",
            "metadata", "background", "service_tier", "truncation",
            "top_logprobs", "presence_penalty", "frequency_penalty",
            "prompt_cache_key",
        ];
        let mut enc = StreamEncoder::new("m");
        let events = sse_events(&enc.finish());
        let (_, created) = events
            .iter()
            .find(|(n, _)| n == "response.created")
            .unwrap();
        let response = created["response"].as_object().unwrap();
        let missing: Vec<&str> = OBSERVED
            .iter()
            .copied()
            .filter(|f| !response.contains_key(*f))
            .collect();
        assert!(
            missing.is_empty(),
            "the real endpoint sends these and we do not: {missing:?}"
        );
    }

}
