//! Google Gemini GenerateContent dialect.
//!
//! The compatibility guards in this adapter are derived from OpenCodex's
//! MIT-licensed Google adapter at commit
//! bc811e77d040bb780e2287610bfc56b0b6a4fc1d. See THIRD_PARTY_NOTICES.md.
//! They are expressed against Construct's canonical translation boundary,
//! not copied as a second routing architecture.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::{
    CanonBlock, CanonEvent, CanonMessage, CanonRequest, CanonRole, CanonStop, CanonTool,
    CanonToolChoice, TranslationContext,
};

const EMPTY_CONTENT: &str = "(empty)";
const EMPTY_TOOL_OUTPUT: &str = "(empty tool output)";
const MAX_SCHEMA_DEPTH: usize = 24;
const MAX_REF_DEPTH: usize = 16;

pub fn parse_request(body: &Value) -> CanonRequest {
    let system = body
        .get("systemInstruction")
        .and_then(|s| s.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());

    let mut messages = Vec::new();
    for content in body
        .get("contents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => CanonRole::Assistant,
            _ => CanonRole::User,
        };
        let mut blocks = Vec::new();
        for part in content
            .get("parts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                blocks.push(CanonBlock::Text(text.to_string()));
                continue;
            }
            if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                if let (Some(media), Some(data)) = (
                    inline
                        .get("mimeType")
                        .or_else(|| inline.get("mime_type"))
                        .and_then(Value::as_str),
                    inline.get("data").and_then(Value::as_str),
                ) {
                    blocks.push(CanonBlock::Image(format!("data:{media};base64,{data}")));
                }
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                blocks.push(CanonBlock::ToolUse {
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: call.get("args").cloned().unwrap_or_else(|| json!({})),
                });
                continue;
            }
            if let Some(response) = part.get("functionResponse") {
                let result = response
                    .get("response")
                    .and_then(|r| r.get("result"))
                    .cloned()
                    .unwrap_or(Value::Null);
                blocks.push(CanonBlock::ToolResult {
                    id: response
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    text: match result {
                        Value::String(s) => s,
                        Value::Null => String::new(),
                        other => serde_json::to_string(&other).unwrap_or_default(),
                    },
                    is_error: false,
                });
            }
        }
        if !blocks.is_empty() {
            messages.push(CanonMessage { role, blocks });
        }
    }

    let generation = body.get("generationConfig");
    CanonRequest {
        model: String::new(),
        system,
        messages,
        tools: body
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|group| {
                group
                    .get("functionDeclarations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|tool| {
                Some(CanonTool {
                    name: tool.get("name")?.as_str()?.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    schema: tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(empty_object_schema),
                })
            })
            .collect(),
        tool_choice: parse_tool_choice(body.get("toolConfig")),
        max_tokens: generation
            .and_then(|g| g.get("maxOutputTokens"))
            .and_then(Value::as_u64),
        temperature: generation
            .and_then(|g| g.get("temperature"))
            .and_then(Value::as_f64),
        top_p: generation
            .and_then(|g| g.get("topP"))
            .and_then(Value::as_f64),
        stream: false,
        stop: generation
            .and_then(|g| g.get("stopSequences"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        reasoning_effort: None,
    }
}

fn parse_tool_choice(config: Option<&Value>) -> Option<CanonToolChoice> {
    let choice = config?.get("functionCallingConfig")?;
    let mode = choice.get("mode").and_then(Value::as_str)?;
    Some(match mode.to_ascii_uppercase().as_str() {
        "ANY" | "VALIDATED" => {
            let name = choice
                .get("allowedFunctionNames")
                .and_then(Value::as_array)
                .and_then(|n| n.first())
                .and_then(Value::as_str);
            name.map_or(CanonToolChoice::Required, |n| {
                CanonToolChoice::Named(n.to_string())
            })
        }
        "NONE" => CanonToolChoice::None,
        _ => CanonToolChoice::Auto,
    })
}

pub fn emit_request(req: &CanonRequest, _model: &str) -> (Value, TranslationContext) {
    let codec = ToolNameCodec::new(req);
    let mut contents = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();

    for message in &req.messages {
        let mut parts = Vec::new();
        for block in &message.blocks {
            match block {
                CanonBlock::Text(text) if !text.is_empty() => {
                    parts.push(json!({"text": text}));
                }
                CanonBlock::Text(_) => {}
                CanonBlock::Image(url) => {
                    if let Some((media, data)) = data_url(url) {
                        parts.push(json!({
                            "inline_data": {"mime_type": media, "data": data}
                        }));
                    } else {
                        parts.push(json!({"text": format!("[image: {url}]")}));
                    }
                }
                CanonBlock::ToolUse { id, name, input } => {
                    call_names.insert(id.clone(), name.clone());
                    let mut call = Map::new();
                    call.insert("name".into(), json!(codec.encode(name)));
                    call.insert("args".into(), input.clone());
                    if let Some(id) = normalize_tool_id(id) {
                        call.insert("id".into(), json!(id));
                    }
                    parts.push(json!({"functionCall": call}));
                }
                CanonBlock::ToolResult { id, text, .. } => {
                    let name = call_names.get(id).map(String::as_str).unwrap_or("tool");
                    let mut response = Map::new();
                    response.insert("name".into(), json!(codec.encode(name)));
                    response.insert(
                        "response".into(),
                        json!({"result": if text.is_empty() { EMPTY_TOOL_OUTPUT } else { text }}),
                    );
                    if let Some(id) = normalize_tool_id(id) {
                        response.insert("id".into(), json!(id));
                    }
                    parts.push(json!({"functionResponse": response}));
                }
                CanonBlock::Thinking(_) => {}
            }
        }

        let role = match message.role {
            CanonRole::Assistant => "model",
            _ => "user",
        };
        if parts.is_empty() {
            if role == "model" {
                // Thinking-only assistant turns cannot be represented and
                // an empty model turn is rejected by Gemini.
                continue;
            }
            parts.push(json!({"text": EMPTY_CONTENT}));
        }
        contents.push(json!({"role": role, "parts": parts}));
    }

    let mut body = Map::new();
    body.insert("contents".into(), json!(contents));
    if let Some(system) = req.system.as_deref().filter(|s| !s.is_empty()) {
        body.insert(
            "systemInstruction".into(),
            json!({"parts":[{"text":system}]}),
        );
    }
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!([{
                "functionDeclarations": req.tools.iter().map(|tool| json!({
                    "name": codec.encode(&tool.name),
                    "description": tool.description,
                    "parameters": sanitize_tool_schema(&tool.schema),
                })).collect::<Vec<_>>()
            }]),
        );
    }

    let mut generation = Map::new();
    if let Some(tokens) = req.max_tokens.filter(|n| *n > 0) {
        generation.insert("maxOutputTokens".into(), json!(tokens));
    }
    if let Some(temperature) = req.temperature.filter(|n| n.is_finite() && *n >= 0.0) {
        generation.insert("temperature".into(), json!(temperature.min(2.0)));
    }
    if let Some(top_p) = req.top_p.filter(|n| n.is_finite() && *n >= 0.0) {
        generation.insert("topP".into(), json!(top_p.min(1.0)));
    }
    let stops: Vec<&str> = req
        .stop
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(5)
        .collect();
    if !stops.is_empty() {
        generation.insert("stopSequences".into(), json!(stops));
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation));
    }

    if let Some(choice) = req.tool_choice.as_ref().filter(|_| !req.tools.is_empty()) {
        let mut function = Map::new();
        match choice {
            CanonToolChoice::Auto => {
                function.insert("mode".into(), json!("AUTO"));
            }
            CanonToolChoice::Required => {
                function.insert("mode".into(), json!("ANY"));
            }
            CanonToolChoice::None => {
                function.insert("mode".into(), json!("NONE"));
            }
            CanonToolChoice::Named(name) => {
                function.insert("mode".into(), json!("ANY"));
                function.insert("allowedFunctionNames".into(), json!([codec.encode(name)]));
            }
        }
        body.insert(
            "toolConfig".into(),
            json!({"functionCallingConfig": function}),
        );
    }

    (
        Value::Object(body),
        TranslationContext {
            tool_names: codec.reverse,
        },
    )
}

pub fn decode_event(data: &Value, context: &TranslationContext) -> Vec<CanonEvent> {
    let mut out = Vec::new();
    if let Some(usage) = data.get("usageMetadata") {
        out.push(CanonEvent::Usage {
            input: usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    let Some(candidate) = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    else {
        return out;
    };

    let mut called_tool = false;
    for (index, part) in candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        if let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            out.push(CanonEvent::TextDelta(text.to_string()));
        }
        if let Some(call) = part.get("functionCall") {
            called_tool = true;
            let wire_name = call.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            out.push(CanonEvent::ToolStart {
                index,
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| stable_call_id(wire_name, &args)),
                name: context
                    .tool_names
                    .get(wire_name)
                    .cloned()
                    .unwrap_or_else(|| wire_name.to_string()),
            });
            out.push(CanonEvent::ToolArgsDelta {
                index,
                json: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
            });
        }
    }
    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
        out.push(CanonEvent::Stop {
            reason: match reason {
                "STOP" if called_tool => CanonStop::ToolUse,
                "STOP" => CanonStop::EndTurn,
                "MAX_TOKENS" => CanonStop::MaxTokens,
                other => CanonStop::Other(other.to_string()),
            },
        });
    }
    out
}

pub fn decode_full_response(body: &Value, context: &TranslationContext) -> Vec<CanonEvent> {
    let mut events = vec![CanonEvent::Start {
        id: "gemini_router".into(),
    }];
    events.extend(decode_event(body, context));
    if !events
        .iter()
        .any(|event| matches!(event, CanonEvent::Stop { .. }))
    {
        events.push(CanonEvent::Stop {
            reason: CanonStop::EndTurn,
        });
    }
    events
}

fn data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    rest.split_once(";base64,")
}

fn normalize_tool_id(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned == raw {
        Some(cleaned)
    } else {
        Some(format!("{cleaned}_{:08x}", fnv1a64(raw.as_bytes()) as u32))
    }
}

fn stable_call_id(name: &str, args: &Value) -> String {
    let source = format!(
        "{name}\0{}",
        serde_json::to_string(args).unwrap_or_default()
    );
    format!("call_{:016x}", fnv1a64(source.as_bytes()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

struct ToolNameCodec {
    forward: BTreeMap<String, String>,
    reverse: BTreeMap<String, String>,
}

impl ToolNameCodec {
    fn new(req: &CanonRequest) -> Self {
        let mut names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        for message in &req.messages {
            for block in &message.blocks {
                if let CanonBlock::ToolUse { name, .. } = block {
                    names.push(name.clone());
                }
            }
        }
        let mut forward = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        let mut used = HashSet::new();
        for name in names {
            if forward.contains_key(&name) {
                continue;
            }
            let wire = if valid_tool_name(&name) && used.insert(name.clone()) {
                name.clone()
            } else {
                let mut cleaned: String = name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                if !cleaned
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                {
                    cleaned.insert(0, '_');
                }
                let prefix: String = cleaned.chars().take(55).collect();
                let mut salt = 0u64;
                loop {
                    let hash_input = if salt == 0 {
                        name.clone()
                    } else {
                        format!("{name}#{salt}")
                    };
                    let candidate =
                        format!("{prefix}_{:08x}", fnv1a64(hash_input.as_bytes()) as u32);
                    if used.insert(candidate.clone()) {
                        break candidate;
                    }
                    salt += 1;
                }
            };
            forward.insert(name.clone(), wire.clone());
            reverse.insert(wire, name);
        }
        Self { forward, reverse }
    }

    fn encode<'a>(&'a self, name: &'a str) -> &'a str {
        self.forward.get(name).map(String::as_str).unwrap_or(name)
    }
}

fn valid_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !name.is_ascii() {
        return false;
    }
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn empty_object_schema() -> Value {
    json!({"type":"object","properties":{}})
}

fn sanitize_tool_schema(schema: &Value) -> Value {
    let mut defs = BTreeMap::new();
    collect_defs(schema, &mut defs);
    let mut root = sanitize_schema(schema, &defs, 0, 0, false);
    let Some(root_obj) = root.as_object_mut() else {
        return empty_object_schema();
    };
    root_obj.insert("type".into(), json!("object"));
    if !root_obj.get("properties").is_some_and(Value::is_object) {
        root_obj.insert("properties".into(), json!({}));
    }
    root
}

fn collect_defs(value: &Value, defs: &mut BTreeMap<String, Value>) {
    let Some(obj) = value.as_object() else {
        return;
    };
    for bag in ["$defs", "definitions"] {
        if let Some(group) = obj.get(bag).and_then(Value::as_object) {
            for (name, value) in group {
                defs.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }
}

fn sanitize_schema(
    value: &Value,
    defs: &BTreeMap<String, Value>,
    depth: usize,
    ref_depth: usize,
    preserve_null: bool,
) -> Value {
    if depth >= MAX_SCHEMA_DEPTH {
        return json!({});
    }
    let Some(source) = value.as_object() else {
        return json!({});
    };
    if ref_depth < MAX_REF_DEPTH {
        if let Some(reference) = source.get("$ref").and_then(Value::as_str) {
            if let Some(name) = reference
                .strip_prefix("#/$defs/")
                .or_else(|| reference.strip_prefix("#/definitions/"))
            {
                if let Some(target) = defs.get(name) {
                    let mut merged = target.as_object().cloned().unwrap_or_default();
                    for (key, value) in source {
                        if key != "$ref" {
                            merged.insert(key.clone(), value.clone());
                        }
                    }
                    return sanitize_schema(
                        &Value::Object(merged),
                        defs,
                        depth,
                        ref_depth + 1,
                        preserve_null,
                    );
                }
            }
        }
    }

    let mut out = Map::new();
    normalize_type(source.get("type"), &mut out, preserve_null);
    for key in ["description", "format"] {
        if let Some(value) = source.get(key).and_then(Value::as_str) {
            out.insert(key.into(), json!(value));
        }
    }
    if let Some(nullable) = source.get("nullable").and_then(Value::as_bool) {
        out.insert("nullable".into(), json!(nullable));
    }
    if let Some(values) = string_enum(source.get("enum").or_else(|| source.get("const"))) {
        out.insert("enum".into(), json!(values));
    }
    if let Some(properties) = source.get("properties").and_then(Value::as_object) {
        out.insert(
            "properties".into(),
            Value::Object(
                properties
                    .iter()
                    .map(|(name, schema)| {
                        (
                            name.clone(),
                            sanitize_schema(schema, defs, depth + 1, ref_depth, false),
                        )
                    })
                    .collect(),
            ),
        );
    }
    if let Some(items) = source.get("items").filter(|v| v.is_object()) {
        out.insert(
            "items".into(),
            sanitize_schema(items, defs, depth + 1, ref_depth, false),
        );
    }
    if let Some(required) = source.get("required").and_then(Value::as_array) {
        let values: BTreeSet<&str> = required.iter().filter_map(Value::as_str).collect();
        out.insert("required".into(), json!(values));
    }
    if let Some(any_of) = source.get("anyOf").and_then(Value::as_array) {
        let branches: Vec<Value> = any_of
            .iter()
            .map(|v| sanitize_schema(v, defs, depth + 1, ref_depth, true))
            .collect();
        let non_null: Vec<&Value> = branches
            .iter()
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        if non_null.len() == 1 && non_null.len() < branches.len() {
            if let Some(branch) = non_null[0].as_object() {
                out.extend(branch.clone());
                out.insert("nullable".into(), json!(true));
            }
        } else if let Some(kind) = branches
            .first()
            .and_then(|branch| branch.get("type"))
            .and_then(Value::as_str)
            .filter(|kind| {
                branches.iter().all(|branch| {
                    branch.get("type").and_then(Value::as_str) == Some(*kind)
                        && branch.get("enum").is_some_and(Value::is_array)
                        && branch
                            .as_object()
                            .is_some_and(|obj| obj.keys().all(|key| key == "type" || key == "enum"))
                })
            })
        {
            let values: BTreeSet<&str> = branches
                .iter()
                .flat_map(|branch| {
                    branch
                        .get("enum")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                })
                .collect();
            if !values.is_empty() {
                out.insert("type".into(), json!(kind));
                out.insert("enum".into(), json!(values));
            }
        }
    }
    Value::Object(out)
}

fn normalize_type(value: Option<&Value>, out: &mut Map<String, Value>, preserve_null: bool) {
    let candidates: Vec<&str> = match value {
        Some(Value::String(value)) => vec![value],
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut saw_null = false;
    for candidate in candidates {
        let candidate = candidate.to_ascii_lowercase();
        if candidate == "null" {
            saw_null = true;
        } else if out.get("type").is_none()
            && matches!(
                candidate.as_str(),
                "string" | "integer" | "number" | "boolean" | "array" | "object"
            )
        {
            out.insert("type".into(), json!(candidate));
        }
    }
    if saw_null {
        if out.contains_key("type") {
            out.insert("nullable".into(), json!(true));
        } else if preserve_null {
            out.insert("type".into(), json!("null"));
        } else {
            out.insert("nullable".into(), json!(true));
        }
    }
}

fn string_enum(value: Option<&Value>) -> Option<Vec<String>> {
    let mut values = BTreeSet::new();
    match value {
        Some(Value::String(value)) => {
            values.insert(value.clone());
        }
        Some(Value::Array(items)) => {
            values.extend(items.iter().filter_map(Value::as_str).map(str::to_string));
        }
        _ => {}
    }
    (!values.is_empty()).then(|| values.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> CanonRequest {
        CanonRequest {
            system: Some("be exact".into()),
            messages: vec![
                CanonMessage {
                    role: CanonRole::User,
                    blocks: vec![CanonBlock::Text("use it".into())],
                },
                CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![CanonBlock::ToolUse {
                        id: "call:1".into(),
                        name: "9 invalid.tool name".into(),
                        input: json!({"path":"/"}),
                    }],
                },
                CanonMessage {
                    role: CanonRole::Tool,
                    blocks: vec![CanonBlock::ToolResult {
                        id: "call:1".into(),
                        text: String::new(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![CanonTool {
                name: "9 invalid.tool name".into(),
                description: "read".into(),
                schema: json!({
                    "type":"object",
                    "properties":{"path":{"type":"string","x-mcp-header":"secret"}},
                    "additionalProperties":false
                }),
            }],
            tool_choice: Some(CanonToolChoice::Named("9 invalid.tool name".into())),
            max_tokens: Some(100),
            temperature: Some(99.0),
            top_p: Some(3.0),
            stream: true,
            ..CanonRequest::default()
        }
    }

    #[test]
    fn emits_gemini_and_hardens_names_ids_schemas_and_ranges() {
        let (body, context) = emit_request(&sample_request(), "gemini-test");
        let wire_name = body["tools"][0]["functionDeclarations"][0]["name"]
            .as_str()
            .unwrap();
        assert!(valid_tool_name(wire_name));
        assert_eq!(
            context.tool_names.get(wire_name).map(String::as_str),
            Some("9 invalid.tool name")
        );
        assert_eq!(body["generationConfig"]["temperature"], 2.0);
        assert_eq!(body["generationConfig"]["topP"], 1.0);
        assert!(body.to_string().find("x-mcp-header").is_none());
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["id"]
                .as_str()
                .unwrap(),
            contents[2]["parts"][0]["functionResponse"]["id"]
                .as_str()
                .unwrap()
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            EMPTY_TOOL_OUTPUT
        );
    }

    #[test]
    fn decodes_text_tools_usage_and_finish_reason() {
        let (body, context) = emit_request(&sample_request(), "gemini-test");
        let wire_name = body["tools"][0]["functionDeclarations"][0]["name"]
            .as_str()
            .unwrap();
        let events = decode_event(
            &json!({
                "candidates":[{
                    "content":{"parts":[
                        {"text":"hi"},
                        {"functionCall":{"name":wire_name,"args":{"x":1}}}
                    ]},
                    "finishReason":"STOP"
                }],
                "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}
            }),
            &context,
        );
        assert!(events.contains(&CanonEvent::TextDelta("hi".into())));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonEvent::ToolStart { name, .. } if name == "9 invalid.tool name"
        )));
        assert!(events.contains(&CanonEvent::Usage {
            input: 3,
            output: 2
        }));
        assert!(events.contains(&CanonEvent::Stop {
            reason: CanonStop::ToolUse
        }));
    }

    #[test]
    fn empty_user_turn_gets_placeholder_but_thinking_only_assistant_is_dropped() {
        let req = CanonRequest {
            messages: vec![
                CanonMessage {
                    role: CanonRole::User,
                    blocks: vec![CanonBlock::Text(String::new())],
                },
                CanonMessage {
                    role: CanonRole::Assistant,
                    blocks: vec![CanonBlock::Thinking("private".into())],
                },
            ],
            ..CanonRequest::default()
        };
        let (body, _) = emit_request(&req, "m");
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        assert_eq!(body["contents"][0]["parts"][0]["text"], EMPTY_CONTENT);
    }
}
