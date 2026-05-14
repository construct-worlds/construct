//! Translate a model spec string into a (provider, bare model name).
//!
//! Explicit prefixes (`openai:`, `anthropic:`, `ollama:`) always win.
//! Otherwise we sniff the bare name:
//!   - starts with `gpt-` or `o[1-5]` → OpenAI
//!   - starts with `claude-` → Anthropic
//!   - anything else → Ollama (local fallback)
//!
//! Returning an enum keeps the dispatch table small and testable.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
    Ollama,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: Provider,
    pub model: String,
}

pub fn parse_model_spec(s: &str) -> Result<ModelSpec, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty model spec".into());
    }
    if let Some(rest) = s.strip_prefix("openai:") {
        return Ok(ModelSpec {
            provider: Provider::OpenAI,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("anthropic:") {
        return Ok(ModelSpec {
            provider: Provider::Anthropic,
            model: rest.to_string(),
        });
    }
    if let Some(rest) = s.strip_prefix("ollama:") {
        return Ok(ModelSpec {
            provider: Provider::Ollama,
            model: rest.to_string(),
        });
    }
    if let Some(prefix) = s.split(':').next() {
        // Reject unknown explicit prefixes so typos don't silently fall through.
        if s.contains(':') && !matches!(prefix, "openai" | "anthropic" | "ollama") {
            return Err(format!(
                "unknown provider prefix `{prefix}:` (expected one of openai:, anthropic:, ollama:)"
            ));
        }
    }
    let provider = if s.starts_with("gpt-") || is_o_series(s) {
        Provider::OpenAI
    } else if s.starts_with("claude-") {
        Provider::Anthropic
    } else {
        Provider::Ollama
    };
    Ok(ModelSpec {
        provider,
        model: s.to_string(),
    })
}

fn is_o_series(s: &str) -> bool {
    // o1, o3, o4, o5, plus their dashed variants (o1-mini, o3-pro, …).
    let mut chars = s.chars();
    let Some(c0) = chars.next() else { return false };
    if c0 != 'o' {
        return false;
    }
    let Some(c1) = chars.next() else { return false };
    if !matches!(c1, '1' | '3' | '4' | '5') {
        return false;
    }
    // Boundary: end-of-string, dash, dot, or digit cont.
    match chars.next() {
        None | Some('-') | Some('.') => true,
        Some(c) if c.is_ascii_digit() => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ModelSpec {
        parse_model_spec(s).unwrap()
    }

    #[test]
    fn gpt_4o_is_openai() {
        assert_eq!(parse("gpt-4o").provider, Provider::OpenAI);
        assert_eq!(parse("gpt-4o").model, "gpt-4o");
    }

    #[test]
    fn o_series_is_openai() {
        assert_eq!(parse("o1").provider, Provider::OpenAI);
        assert_eq!(parse("o3-pro").provider, Provider::OpenAI);
        assert_eq!(parse("o5").provider, Provider::OpenAI);
    }

    #[test]
    fn claude_haiku_is_anthropic() {
        assert_eq!(parse("claude-haiku-4-5").provider, Provider::Anthropic);
        assert_eq!(parse("claude-haiku-4-5").model, "claude-haiku-4-5");
    }

    #[test]
    fn explicit_prefix_overrides_heuristic() {
        let s = parse("anthropic:something-new");
        assert_eq!(s.provider, Provider::Anthropic);
        assert_eq!(s.model, "something-new");

        let s = parse("ollama:llama3.1");
        assert_eq!(s.provider, Provider::Ollama);
        assert_eq!(s.model, "llama3.1");

        let s = parse("openai:gpt-5-mini");
        assert_eq!(s.provider, Provider::OpenAI);
        assert_eq!(s.model, "gpt-5-mini");
    }

    #[test]
    fn bare_unknown_falls_back_to_ollama() {
        let s = parse("llama3.1");
        assert_eq!(s.provider, Provider::Ollama);
        assert_eq!(s.model, "llama3.1");

        let s = parse("mistral");
        assert_eq!(s.provider, Provider::Ollama);
    }

    #[test]
    fn unknown_prefix_errors() {
        assert!(parse_model_spec("bogus:foo").is_err());
    }

    #[test]
    fn empty_errors() {
        assert!(parse_model_spec("").is_err());
        assert!(parse_model_spec("   ").is_err());
    }
}
