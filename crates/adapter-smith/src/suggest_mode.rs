//! One-shot next-prompt suggestion generation (spec 0109).
//!
//! For smith sessions the "same harness" generator is simply another
//! smith completion, so the daemon shells out to
//! `construct-adapter-smith --suggest-mode`, writing a rendered
//! transcript tail to stdin. Same model-resolution ladder as
//! title-mode, one tools-disabled completion, then the shared
//! [`SuggestionHand::parse_loose`] contract from the protocol crate.
//! The normalized hand is printed to stdout as JSON for the daemon to
//! broadcast verbatim.

use crate::provider::{Content, Message, Role, TextSink, ToolSpec};
use crate::title_mode::{pick_default_spec_str, provider_for};
use anyhow::{anyhow, Result};
use construct_protocol::SuggestionHand;

/// Sink that captures streamed deltas in memory.
#[derive(Default)]
struct CaptureSink {
    text: String,
}
impl TextSink for CaptureSink {
    fn delta(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

const MAX_CONTEXT_CHARS: usize = 24_000;

/// Run one suggestion completion over the rendered transcript tail and
/// return the clamped hand. Fails fast on missing credentials / network
/// errors / unparseable output so the daemon can silently drop the
/// attempt — suggestions are best-effort by contract.
pub async fn suggest_hand(context: &str) -> Result<SuggestionHand> {
    let context = tail_chars(context, MAX_CONTEXT_CHARS);
    if context.trim().is_empty() {
        return Err(anyhow!("empty transcript context"));
    }
    let spec = crate::provider::routing::parse_model_spec(&pick_default_spec_str()?)
        .map_err(|e| anyhow!("model-spec parse: {e}"))?;
    let provider = provider_for(spec.provider)?;
    let messages = vec![Message {
        role: Role::User,
        content: Content::Text {
            text: format!("Transcript tail:\n\n{context}\n\nJSON:"),
        },
    }];
    let tools: Vec<ToolSpec> = Vec::new();
    let mut sink = CaptureSink::default();
    let _turn = provider
        .complete(
            &spec.model,
            SuggestionHand::PROMPT_INSTRUCTIONS,
            &messages,
            &tools,
            &mut sink,
        )
        .await?;
    SuggestionHand::parse_loose(&sink.text).map_err(|e| anyhow!(e))
}

/// Last `max` chars of `s`, on a char boundary.
fn tail_chars(s: &str, max: usize) -> &str {
    let count = s.chars().count();
    if count <= max {
        return s;
    }
    let skip = count - max;
    let (idx, _) = s.char_indices().nth(skip).unwrap_or((0, ' '));
    &s[idx..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_chars_keeps_suffix() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("abc", 10), "abc");
    }

    // parse_loose behavior (fences, clamps, empty-top rejection) is
    // covered in the protocol crate where the function lives.
}
