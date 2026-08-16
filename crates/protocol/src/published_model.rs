//! Codec for Construct-published native-catalog model ids (spec 0157).
//!
//! When Construct publishes its routes into a harness's native model
//! picker, each entry gets a stable id in Construct's namespace that
//! reversibly encodes the route and target model. The id is a wire
//! contract shared by the daemon (which mints it and resolves it per
//! request) and by clients (which decode it for display so the user sees
//! `model · route` instead of the raw encoded id — spec 0158). Typical
//! ids stay readable (`construct-review/kimi-k2.5`); separators and other
//! unsafe bytes are percent-encoded so the mapping remains collision-safe.

use anyhow::{anyhow, bail, Context, Result};

pub const PUBLISHED_MODEL_PREFIX: &str = "construct-";
/// Claude Code filters gateway-discovered model ids to values that begin
/// with `claude` or `anthropic`, so ids published to it carry this prefix.
pub const CLAUDE_PUBLISHED_MODEL_PREFIX: &str = "claude-construct-";

fn encode_part(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn decode_part(value: &str) -> Result<String> {
    let input = value.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            bytes.push(input[index]);
            index += 1;
            continue;
        }
        let hex = input
            .get(index + 1..index + 3)
            .ok_or_else(|| anyhow!("truncated percent escape"))?;
        let hex = std::str::from_utf8(hex).context("percent escape")?;
        bytes.push(u8::from_str_radix(hex, 16).context("percent escape")?);
        index += 3;
    }
    String::from_utf8(bytes).context("utf-8")
}

/// Stable, human-readable, one-slash id accepted by a harness's catalog
/// and model override surfaces.
pub fn published_model_id(route: &str, model: &str) -> String {
    published_model_id_with_prefix(PUBLISHED_MODEL_PREFIX, route, model)
}

fn published_model_id_with_prefix(prefix: &str, route: &str, model: &str) -> String {
    format!("{prefix}{}/{}", encode_part(route), encode_part(model))
}

pub fn published_model_id_for_harness(harness: &str, route: &str, model: &str) -> String {
    let prefix = if harness == "claude" {
        CLAUDE_PUBLISHED_MODEL_PREFIX
    } else {
        PUBLISHED_MODEL_PREFIX
    };
    published_model_id_with_prefix(prefix, route, model)
}

/// Translate a canonical or harness-specific Construct model id to the form
/// accepted by `harness`.
///
/// Durable configuration stores the ordinary `construct-` form so changing a
/// operator's harness does not leave a Claude-only prefix behind. Session
/// creation materializes that stable selection for the actual harness. Native
/// model ids return `Ok(None)` and must pass through unchanged.
pub fn published_model_id_for_harness_from_id(harness: &str, id: &str) -> Result<Option<String>> {
    Ok(decode_published_model_id(id)?
        .map(|(route, model)| published_model_id_for_harness(harness, &route, &model)))
}

/// Decode an id in Construct's namespace back to `(route, model)`.
///
/// `Ok(None)` means the id is not ours at all (a native model id).
/// `Err` means the id claims our namespace but is malformed — the router
/// fails such requests closed rather than leaking them to the native
/// provider; display callers fall back to showing the raw value.
pub fn decode_published_model_id(id: &str) -> Result<Option<(String, String)>> {
    let encoded = if let Some(encoded) = id.strip_prefix(CLAUDE_PUBLISHED_MODEL_PREFIX) {
        encoded
    } else if let Some(encoded) = id.strip_prefix(PUBLISHED_MODEL_PREFIX) {
        encoded
    } else {
        return Ok(None);
    };
    let (route, model) = encoded
        .split_once('/')
        .ok_or_else(|| anyhow!("published model id has no route/model separator"))?;
    if route.is_empty() || model.is_empty() || model.contains('/') {
        bail!("published model id has an empty route or model");
    }
    Ok(Some((decode_part(route)?, decode_part(model)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_ids_round_trip_arbitrary_parts() {
        let id = published_model_id("route/with space", "vendor/model--1");
        assert_eq!(id, "construct-route%2Fwith%20space/vendor%2Fmodel--1");
        assert_eq!(
            decode_published_model_id(&id).unwrap(),
            Some(("route/with space".into(), "vendor/model--1".into()))
        );
        assert_eq!(decode_published_model_id("gpt-native").unwrap(), None);
    }

    #[test]
    fn claude_ids_pass_gateway_filter_and_round_trip() {
        let id = published_model_id_for_harness("claude", "review", "vendor/model");
        assert_eq!(id, "claude-construct-review/vendor%2Fmodel");
        assert_eq!(
            decode_published_model_id(&id).unwrap(),
            Some(("review".into(), "vendor/model".into()))
        );
    }

    #[test]
    fn durable_ids_are_materialized_for_the_selected_harness() {
        let canonical = published_model_id("claude-oauth", "sonnet");
        assert_eq!(
            published_model_id_for_harness_from_id("codex", &canonical).unwrap(),
            Some("construct-claude-oauth/sonnet".into())
        );
        assert_eq!(
            published_model_id_for_harness_from_id("claude", &canonical).unwrap(),
            Some("claude-construct-claude-oauth/sonnet".into())
        );
        assert_eq!(
            published_model_id_for_harness_from_id("codex", "claude-construct-claude-oauth/sonnet")
                .unwrap(),
            Some("construct-claude-oauth/sonnet".into())
        );
        assert_eq!(
            published_model_id_for_harness_from_id("codex", "gpt-native").unwrap(),
            None
        );
    }

    #[test]
    fn malformed_construct_ids_fail_closed() {
        assert!(decode_published_model_id("construct-route/not%XXvalid").is_err());
        assert!(decode_published_model_id("construct-no-separator").is_err());
        assert!(decode_published_model_id("construct-/model").is_err());
    }
}
