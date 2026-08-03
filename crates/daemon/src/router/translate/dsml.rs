//! DeepSeek V4 DSML tool-call recovery.
//!
//! DeepSeek V4 sometimes emits tool intent as DSML markup inside assistant
//! *content* instead of (or in addition to) structured `tool_calls`. The
//! official shape is:
//!
//! ```text
//! <｜DSML｜tool_calls>
//! <｜DSML｜invoke name="func_name">
//! <｜DSML｜parameter name="location" string="true">杭州</｜DSML｜parameter>
//! </｜DSML｜invoke>
//! </｜DSML｜tool_calls>
//! ```
//!
//! Live Codex→DeepSeek turns have also produced a looser command form
//! (double fullwidth pipes, `_command` / `<cmd>` tags). Neither shape is
//! a structured OpenAI tool call, so without lifting it the harness sees
//! prose and never runs a tool.
//!
//! This module:
//! - parses DSML out of text into name + JSON arguments
//! - rewrites a stream of [`CanonEvent`]s so TextDelta content is cleaned
//!   and tool events are emitted with `Stop { ToolUse }` when appropriate

use serde_json::{json, Value};

use super::{CanonEvent, CanonStop};

/// Fullwidth vertical line used in DeepSeek special tokens.
const FW: char = '\u{ff5c}';

/// One tool call recovered from DSML markup.
#[derive(Debug, Clone, PartialEq)]
pub struct DsmlToolCall {
    pub name: String,
    pub arguments: Value,
}

/// Text with DSML blocks removed, plus any tool calls those blocks described.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiftedContent {
    pub text: String,
    pub tools: Vec<DsmlToolCall>,
}

impl LiftedContent {
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// True when `text` may contain a DSML open tag (including a partial prefix).
pub fn looks_like_dsml(text: &str) -> bool {
    find_dsml_open(text).is_some() || has_dsml_open_prefix(text)
}

/// Lift every complete DSML block out of `text`. Incomplete trailing markup
/// is left in `text` so a stream filter can keep buffering it.
pub fn lift_content(text: &str) -> LiftedContent {
    let mut remaining = text;
    let mut out_text = String::new();
    let mut tools = Vec::new();

    while let Some(start) = find_dsml_open(remaining) {
        out_text.push_str(&remaining[..start]);
        let from_tag = &remaining[start..];
        match parse_one_dsml_block(from_tag) {
            Some((consumed, recovered)) => {
                tools.extend(recovered);
                remaining = &from_tag[consumed..];
            }
            None => {
                // Incomplete block at the end — keep it so the caller can
                // buffer; if this is a final lift, the incomplete markup
                // stays as text (better than inventing a half tool call).
                out_text.push_str(from_tag);
                remaining = "";
                break;
            }
        }
    }
    out_text.push_str(remaining);

    LiftedContent {
        text: collapse_blank_runs(out_text.trim_end()),
        tools,
    }
}

/// Streaming filter: hold text once a DSML open appears, then emit tools on
/// [`CanonEvent::Stop`] (or when a complete block closes mid-stream).
#[derive(Debug, Default)]
pub struct StreamLift {
    /// Text held because it may still be incomplete DSML, or because a
    /// complete lift has not been flushed yet.
    held: String,
    /// Next free tool index (after any structured tool_calls already seen).
    next_index: usize,
    /// Tools emitted this turn — used to upgrade stop reason.
    tools_emitted: usize,
}

impl StreamLift {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: CanonEvent) -> Vec<CanonEvent> {
        match event {
            CanonEvent::TextDelta(delta) => self.push_text(&delta),
            CanonEvent::ToolStart { index, id, name } => {
                self.next_index = self.next_index.max(index.saturating_add(1));
                // Structured tool_calls win; flush any held plain text
                // before them so order stays text-then-tools.
                let mut out = self.flush_held_as_text();
                out.push(CanonEvent::ToolStart { index, id, name });
                out
            }
            CanonEvent::ToolArgsDelta { index, json } => {
                vec![CanonEvent::ToolArgsDelta { index, json }]
            }
            CanonEvent::Stop { reason } => {
                let mut out = self.finish_text_and_tools();
                let reason = if self.tools_emitted > 0 {
                    CanonStop::ToolUse
                } else {
                    reason
                };
                out.push(CanonEvent::Stop { reason });
                out
            }
            other => vec![other],
        }
    }

    fn push_text(&mut self, delta: &str) -> Vec<CanonEvent> {
        self.held.push_str(delta);
        // Prefer emitting safe leading text promptly when no DSML is open.
        if !looks_like_dsml(&self.held) {
            // Hold a short suffix that might still become a DSML open tag.
            let split = safe_emit_split(&self.held);
            if split == 0 {
                return Vec::new();
            }
            let emit = self.held[..split].to_string();
            self.held.drain(..split);
            return if emit.is_empty() {
                Vec::new()
            } else {
                vec![CanonEvent::TextDelta(emit)]
            };
        }

        // We have (or may have) DSML. Emit only complete lifts; keep an
        // incomplete trailer buffered.
        let lifted = lift_content(&self.held);
        if !lifted.has_tools() {
            // Still incomplete — wait for more bytes or Stop.
            return Vec::new();
        }

        // Complete block(s) recovered. Anything left in `lifted.text` that
        // still looks like open DSML stays held; the rest can stream out.
        let (emit_text, keep) = split_trailing_dsml_prefix(lifted.text);
        self.held = keep;
        let mut out = Vec::new();
        if !emit_text.is_empty() {
            out.push(CanonEvent::TextDelta(emit_text));
        }
        out.extend(self.emit_tools(lifted.tools));
        out
    }

    fn finish_text_and_tools(&mut self) -> Vec<CanonEvent> {
        let held = std::mem::take(&mut self.held);
        if held.is_empty() {
            return Vec::new();
        }
        let lifted = lift_content(&held);
        let mut out = Vec::new();
        if !lifted.text.is_empty() {
            out.push(CanonEvent::TextDelta(lifted.text));
        }
        out.extend(self.emit_tools(lifted.tools));
        out
    }

    fn flush_held_as_text(&mut self) -> Vec<CanonEvent> {
        let held = std::mem::take(&mut self.held);
        if held.is_empty() {
            Vec::new()
        } else {
            // Last chance: lift complete DSML before forced flush.
            let lifted = lift_content(&held);
            let mut out = Vec::new();
            if !lifted.text.is_empty() {
                out.push(CanonEvent::TextDelta(lifted.text));
            }
            out.extend(self.emit_tools(lifted.tools));
            out
        }
    }

    fn emit_tools(&mut self, tools: Vec<DsmlToolCall>) -> Vec<CanonEvent> {
        let mut out = Vec::new();
        for tool in tools {
            let index = self.next_index;
            self.next_index += 1;
            self.tools_emitted += 1;
            let id = format!("call_dsml_{index}");
            let args = serde_json::to_string(&tool.arguments).unwrap_or_else(|_| "{}".into());
            out.push(CanonEvent::ToolStart {
                index,
                id,
                name: tool.name,
            });
            out.push(CanonEvent::ToolArgsDelta { index, json: args });
        }
        out
    }
}

/// Rewrite a finished list of events (non-streaming decode) so DSML in
/// text becomes tool events and the stop reason is upgraded when needed.
pub fn lift_events(events: Vec<CanonEvent>) -> Vec<CanonEvent> {
    let mut lift = StreamLift::new();
    let mut out = Vec::new();
    let mut saw_stop = false;
    for event in events {
        if matches!(event, CanonEvent::Stop { .. }) {
            saw_stop = true;
        }
        out.extend(lift.push(event));
    }
    if !saw_stop {
        // Non-streaming decode always appends Stop itself; if a caller
        // passes a partial list, flush held text/tools without inventing
        // a stop.
        out.extend(lift.finish_text_and_tools());
    }
    out
}

// ---------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------

/// Find the start index of a DSML open tag, if any.
fn find_dsml_open(text: &str) -> Option<usize> {
    // Scan for '<' then a DSML marker. Both fullwidth and ASCII pipes, one
    // or two separators — V4 and leaked agent formats disagree.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(rest) = text.get(i + 1..) {
                if dsml_marker_len(rest) > 0 {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Length of a leading DSML marker (`｜DSML｜`, `||DSML||`, …), or 0.
fn dsml_marker_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    let mut pipes_before = 0;
    let mut pos_after_pipes = 0;
    while let Some((idx, ch)) = chars.next() {
        if ch == FW || ch == '|' {
            pipes_before += 1;
            pos_after_pipes = idx + ch.len_utf8();
            if pipes_before > 2 {
                return 0;
            }
            continue;
        }
        break;
    }
    if pipes_before == 0 {
        return 0;
    }
    let rest = &s[pos_after_pipes..];
    if !rest.starts_with("DSML") {
        return 0;
    }
    let after_dsml = &rest["DSML".len()..];
    let mut pipes_after = 0;
    let mut pos = 0;
    for (idx, ch) in after_dsml.char_indices() {
        if ch == FW || ch == '|' {
            pipes_after += 1;
            pos = idx + ch.len_utf8();
            if pipes_after > 2 {
                break;
            }
            continue;
        }
        break;
    }
    if pipes_after == 0 {
        return 0;
    }
    pos_after_pipes + "DSML".len() + pos
}

/// Parse one DSML open-tag block starting at `s` (which begins with `<`).
/// Returns bytes consumed and recovered tool calls.
fn parse_one_dsml_block(s: &str) -> Option<(usize, Vec<DsmlToolCall>)> {
    let (open_end, tag, attrs) = parse_open_tag(s)?;
    let close = find_matching_close(s, open_end, &tag)?;
    let inner = &s[open_end..close.start];
    let consumed = close.end;

    let tools = match tag.as_str() {
        "tool_calls" => parse_tool_calls_body(inner),
        "invoke" => {
            let name = attrs
                .get("name")
                .cloned()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "unknown".into());
            vec![DsmlToolCall {
                name,
                arguments: parameters_to_json(inner),
            }]
        }
        // Leaked agent form: <…DSML…_command> <cmd>…</…>
        "command" | "_command" => {
            let cmd = extract_cmd(inner).unwrap_or_else(|| inner.trim().to_string());
            if cmd.is_empty() {
                Vec::new()
            } else {
                vec![DsmlToolCall {
                    // Codex's function tool is named `shell` and takes `cmd`
                    // (see adapter-codex transcript fixtures).
                    name: "shell".into(),
                    arguments: json!({"cmd": cmd}),
                }]
            }
        }
        // Unknown DSML tag — still consume the block so it does not leak
        // as assistant text, but do not invent a tool call.
        _ => Vec::new(),
    };

    Some((consumed, tools))
}

struct CloseSpan {
    start: usize,
    end: usize,
}

fn parse_open_tag(s: &str) -> Option<(usize, String, std::collections::BTreeMap<String, String>)> {
    if !s.starts_with('<') {
        return None;
    }
    let marker = dsml_marker_len(&s[1..]);
    if marker == 0 {
        return None;
    }
    let after_marker = &s[1 + marker..];
    // Tag name: until whitespace, `>`, or `/`.
    let name_len = after_marker
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
        .map(char::len_utf8)
        .sum::<usize>();
    if name_len == 0 {
        return None;
    }
    let tag = after_marker[..name_len].to_string();
    let mut rest = &after_marker[name_len..];
    let mut attrs = std::collections::BTreeMap::new();
    // Attributes: name="value"
    while let Some(stripped) = rest.strip_prefix(|c: char| c.is_whitespace()) {
        rest = stripped;
        if rest.starts_with('>') || rest.starts_with("/>") {
            break;
        }
        let key_len = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .map(char::len_utf8)
            .sum::<usize>();
        if key_len == 0 {
            break;
        }
        let key = rest[..key_len].to_string();
        rest = &rest[key_len..];
        rest = rest.trim_start();
        if !rest.starts_with('=') {
            attrs.insert(key, String::new());
            continue;
        }
        rest = rest[1..].trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        rest = &rest[quote.len_utf8()..];
        let end = rest.find(quote)?;
        let value = rest[..end].to_string();
        rest = &rest[end + quote.len_utf8()..];
        attrs.insert(key, value);
    }
    rest = rest.trim_start();
    let close_rel = rest.find('>')?;
    let open_end = (rest.as_ptr() as usize - s.as_ptr() as usize) + close_rel + 1;
    Some((open_end, tag, attrs))
}

fn find_matching_close(s: &str, open_end: usize, tag: &str) -> Option<CloseSpan> {
    // Closing forms:
    //   </｜DSML｜tag>
    //   </||DSML||tag>
    // Prefer an exact tag match so a nested `</…_param>` inside a
    // `_command` block does not prematurely end the outer block (live
    // session s8e4420fd3).
    let body = &s[open_end..];
    let tag_norm = tag.trim_start_matches('_');
    let mut search_from = 0;
    let mut first_dsml_close: Option<CloseSpan> = None;
    while let Some(rel) = body[search_from..].find("</") {
        let abs = open_end + search_from + rel;
        let close = &s[abs..];
        if !close.starts_with("</") {
            search_from += rel + 2;
            continue;
        }
        let after = &close[2..];
        let marker = dsml_marker_len(after);
        if marker == 0 {
            // Plain HTML-ish close such as `</cmd>` — not a DSML close.
            search_from += rel + 2;
            continue;
        }
        let name_start = &after[marker..];
        let name_len = name_start
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>')
            .map(char::len_utf8)
            .sum::<usize>();
        let name = &name_start[..name_len];
        let after_name = &name_start[name_len..];
        let Some(gt) = after_name.find('>') else {
            search_from += rel + 2;
            continue;
        };
        let end = abs + 2 + marker + name_len + gt + 1;
        let span = CloseSpan { start: abs, end };
        if name == tag || name.trim_start_matches('_') == tag_norm {
            return Some(span);
        }
        if first_dsml_close.is_none() {
            first_dsml_close = Some(span);
        }
        search_from += rel + 2;
    }
    // No exact match: for loose command markup, fall back to the last
    // DSML close in range so `_param` + trailing `_command` still get
    // consumed together. Prefer the furthest close.
    if matches!(tag_norm, "command" | "param") {
        // Rescan for the last DSML close — consumes through `</…_command>`.
        let mut last = first_dsml_close;
        let mut search_from = 0;
        while let Some(rel) = body[search_from..].find("</") {
            let abs = open_end + search_from + rel;
            let close = &s[abs..];
            if !close.starts_with("</") {
                search_from += rel + 2;
                continue;
            }
            let after = &close[2..];
            let marker = dsml_marker_len(after);
            if marker == 0 {
                search_from += rel + 2;
                continue;
            }
            let name_start = &after[marker..];
            let name_len = name_start
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>')
                .map(char::len_utf8)
                .sum::<usize>();
            let after_name = &name_start[name_len..];
            let Some(gt) = after_name.find('>') else {
                search_from += rel + 2;
                continue;
            };
            last = Some(CloseSpan {
                start: abs,
                end: abs + 2 + marker + name_len + gt + 1,
            });
            search_from += rel + 2;
        }
        return last;
    }
    None
}

fn parse_tool_calls_body(inner: &str) -> Vec<DsmlToolCall> {
    let mut tools = Vec::new();
    let mut rest = inner;
    while let Some(start) = find_dsml_open(rest) {
        let from = &rest[start..];
        match parse_one_dsml_block(from) {
            Some((consumed, recovered)) => {
                tools.extend(recovered);
                rest = &from[consumed..];
            }
            None => break,
        }
    }
    tools
}

fn parameters_to_json(inner: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut rest = inner;
    while let Some(start) = find_dsml_open(rest) {
        let from = &rest[start..];
        let Some((open_end, tag, attrs)) = parse_open_tag(from) else {
            break;
        };
        if tag != "parameter" {
            // Nested non-parameter: try to skip one block or abort.
            if let Some((consumed, _)) = parse_one_dsml_block(from) {
                rest = &from[consumed..];
                continue;
            }
            break;
        }
        let Some(close) = find_matching_close(from, open_end, "parameter") else {
            break;
        };
        let raw = from[open_end..close.start].trim();
        let name = attrs
            .get("name")
            .cloned()
            .unwrap_or_else(|| format!("arg{}", map.len()));
        let as_string = attrs
            .get("string")
            .map(|v| v == "true")
            .unwrap_or(true);
        let value = if as_string {
            Value::String(raw.to_string())
        } else {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
        };
        map.insert(name, value);
        rest = &from[close.end..];
    }
    Value::Object(map)
}

fn extract_cmd(inner: &str) -> Option<String> {
    // Prefer <cmd>...</cmd> or <cmd>...</DSML…_param> style.
    let lower = inner;
    if let Some(start) = lower.find("<cmd>") {
        let after = &lower[start + 5..];
        // Close at </cmd> or the next DSML close or </…
        let end = after
            .find("</cmd>")
            .or_else(|| after.find("</"))
            .unwrap_or(after.len());
        let cmd = after[..end].trim();
        if !cmd.is_empty() {
            return Some(cmd.to_string());
        }
    }
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

/// How much of `held` is safe to emit when no DSML open has been seen yet.
/// Keeps a short suffix that could still grow into a DSML open tag.
fn safe_emit_split(held: &str) -> usize {
    // Keep from the last '<' if the suffix after it could be a DSML prefix.
    if let Some(idx) = held.rfind('<') {
        let suffix = &held[idx + 1..];
        if suffix.is_empty() || could_be_dsml_prefix(suffix) {
            return idx;
        }
    }
    held.len()
}

fn could_be_dsml_prefix(s: &str) -> bool {
    // Empty, pipes only, pipes+partial "DSML", or full marker without tag yet.
    if s.is_empty() {
        return true;
    }
    let mut pipes = 0;
    let mut rest = s;
    for (i, ch) in s.char_indices() {
        if ch == FW || ch == '|' {
            pipes += 1;
            if pipes > 2 {
                return false;
            }
            rest = &s[i + ch.len_utf8()..];
            continue;
        }
        rest = &s[i..];
        break;
    }
    if pipes == 0 {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    const NEEDLE: &str = "DSML";
    if NEEDLE.starts_with(rest) {
        return true;
    }
    if !rest.starts_with(NEEDLE) {
        return false;
    }
    let after = &rest[NEEDLE.len()..];
    let mut pipes_after = 0;
    for ch in after.chars() {
        if ch == FW || ch == '|' {
            pipes_after += 1;
            if pipes_after > 2 {
                return false;
            }
            continue;
        }
        // After the marker, any tag name is a real open — not a prefix.
        return false;
    }
    true
}

fn has_dsml_open_prefix(text: &str) -> bool {
    if let Some(idx) = text.rfind('<') {
        could_be_dsml_prefix(&text[idx + 1..])
    } else {
        false
    }
}

fn split_trailing_dsml_prefix(text: String) -> (String, String) {
    if let Some(idx) = text.rfind('<') {
        if could_be_dsml_prefix(&text[idx + 1..]) || find_dsml_open(&text[idx..]).is_some() {
            // If a full open sits at idx but is incomplete (no close), keep it.
            if find_dsml_open(&text[idx..]).is_some()
                && parse_one_dsml_block(&text[idx..]).is_none()
            {
                return (text[..idx].to_string(), text[idx..].to_string());
            }
            if could_be_dsml_prefix(&text[idx + 1..]) {
                return (text[..idx].to_string(), text[idx..].to_string());
            }
        }
    }
    (text, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fw_tag(inner: &str) -> String {
        // <｜DSML｜inner>
        format!("<{FW}DSML{FW}{inner}>")
    }
    fn fw_close(inner: &str) -> String {
        format!("</{FW}DSML{FW}{inner}>")
    }

    #[test]
    fn lifts_official_invoke_parameters() {
        let text = format!(
            "I will check.\n{open_calls}\n{open_invoke}\n{p1}杭州{p1c}\n{p2}5{p2c}\n{close_invoke}\n{close_calls}\n",
            open_calls = fw_tag("tool_calls"),
            open_invoke = format!("<{FW}DSML{FW}invoke name=\"get_weather\">"),
            p1 = format!("<{FW}DSML{FW}parameter name=\"location\" string=\"true\">"),
            p1c = fw_close("parameter"),
            p2 = format!("<{FW}DSML{FW}parameter name=\"count\" string=\"false\">"),
            p2c = fw_close("parameter"),
            close_invoke = fw_close("invoke"),
            close_calls = fw_close("tool_calls"),
        );
        let lifted = lift_content(&text);
        assert_eq!(lifted.text.trim(), "I will check.");
        assert_eq!(lifted.tools.len(), 1);
        assert_eq!(lifted.tools[0].name, "get_weather");
        assert_eq!(lifted.tools[0].arguments["location"], "杭州");
        assert_eq!(lifted.tools[0].arguments["count"], 5);
    }

    #[test]
    fn lifts_double_pipe_command_form_from_live_session() {
        // Session s8e4420fd3: double fullwidth pipes + _command / <cmd>.
        let open = format!("<{FW}{FW}DSML{FW}{FW}_command>");
        let close = format!("</{FW}{FW}DSML{FW}{FW}_command>");
        let text = format!(
            "Let me look at the project.\n\n{open}\n  <cmd>ls -la /Users/moon</{FW}{FW}DSML{FW}{FW}_param>\n{close}\n"
        );
        let lifted = lift_content(&text);
        assert!(
            !lifted.text.contains("DSML"),
            "DSML must not leak into text: {:?}",
            lifted.text
        );
        assert_eq!(lifted.tools.len(), 1);
        assert_eq!(lifted.tools[0].name, "shell");
        assert_eq!(lifted.tools[0].arguments["cmd"], "ls -la /Users/moon");
    }

    #[test]
    fn lifts_ascii_pipe_variant() {
        let text = r#"searching
<||DSML||tool_calls>
<||DSML||invoke name="doc_search">
<||DSML||parameter name="query" string="true">bond detail</||DSML||parameter>
</||DSML||invoke>
</||DSML||tool_calls>
"#;
        let lifted = lift_content(text);
        assert_eq!(lifted.tools[0].name, "doc_search");
        assert_eq!(lifted.tools[0].arguments["query"], "bond detail");
        assert_eq!(lifted.text.trim(), "searching");
    }

    #[test]
    fn stream_hold_then_emits_tools_on_stop() {
        let mut lift = StreamLift::new();
        let close = fw_close("invoke");

        // Leading prose streams promptly.
        let lead = lift.push(CanonEvent::TextDelta("Looking.\n".into()));
        assert!(
            lead.iter()
                .any(|e| matches!(e, CanonEvent::TextDelta(t) if t.contains("Looking"))),
            "leading text should stream: {lead:?}"
        );

        // Partial open is held.
        assert!(
            lift.push(CanonEvent::TextDelta("<".into())).is_empty(),
            "partial DSML open must not leak"
        );

        // Complete the block across chunks.
        let rest = format!("{FW}DSML{FW}invoke name=\"ls\">\n{close}");
        let mid = lift.push(CanonEvent::TextDelta(rest));
        let stop = lift.push(CanonEvent::Stop {
            reason: CanonStop::EndTurn,
        });
        let all: Vec<_> = mid.into_iter().chain(stop).collect();
        assert!(
            all.iter().any(|e| matches!(
                e,
                CanonEvent::ToolStart { name, .. } if name == "ls"
            )),
            "expected tool start, got {all:?}"
        );
        assert!(all.iter().any(|e| matches!(
            e,
            CanonEvent::Stop {
                reason: CanonStop::ToolUse
            }
        )));
    }

    #[test]
    fn plain_text_passes_through() {
        let mut lift = StreamLift::new();
        let out = lift.push(CanonEvent::TextDelta("hello world".into()));
        assert_eq!(out, vec![CanonEvent::TextDelta("hello world".into())]);
        let stop = lift.push(CanonEvent::Stop {
            reason: CanonStop::EndTurn,
        });
        assert_eq!(
            stop,
            vec![CanonEvent::Stop {
                reason: CanonStop::EndTurn
            }]
        );
    }

    #[test]
    fn lift_events_rewrites_a_full_turn() {
        let open = format!("<{FW}{FW}DSML{FW}{FW}_command>");
        let close = format!("</{FW}{FW}DSML{FW}{FW}_command>");
        let text = format!("Checking.\n{open}\n<cmd>rg DEEPSEEK</cmd>\n{close}");
        let events = lift_events(vec![
            CanonEvent::Start {
                id: "cmpl_1".into(),
            },
            CanonEvent::TextDelta(text),
            CanonEvent::Stop {
                reason: CanonStop::EndTurn,
            },
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::ToolStart { name, .. } if name == "shell"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::ToolArgsDelta { json, .. } if json.contains("rg DEEPSEEK")
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::Stop {
                reason: CanonStop::ToolUse
            }
        )));
        // Clean text still present.
        assert!(events.iter().any(|e| matches!(
            e,
            CanonEvent::TextDelta(t) if t.contains("Checking") && !t.contains("DSML")
        )));
    }
}
