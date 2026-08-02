//! Native harness model-catalog publication.
//!
//! A published model id is routing data, not a provider model name. Codex
//! carries the id on every request (including native subagent requests), so
//! the proxy can select a route per request instead of pinning the whole
//! session to one target.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::{
    effort_level_set, oauth, oauth::OauthProvider, profile_effort_support, ArmedRoute,
    EffortSupport, Router,
};

// The id codec is a wire contract shared with clients (they decode ids for
// display, spec 0158), so it lives in the protocol crate. Re-exported here
// for the router's existing use sites.
pub use construct_protocol::published_model::{
    decode_published_model_id, published_model_id_for_harness, PUBLISHED_MODEL_PREFIX,
};
#[cfg(test)]
use construct_protocol::published_model::published_model_id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedModel {
    pub id: String,
    pub route: String,
    pub model: String,
    /// Whether a reasoning-effort choice made in the harness's native
    /// picker actually reaches this route's target. True only when the
    /// target speaks a dialect that accepts the effort knob verbatim
    /// (OpenAI Responses today); targets whose reasoning controls are a
    /// different shape advertise a single provider-default level instead
    /// of a selector that would silently do nothing.
    pub effort: EffortSupport,
}

impl Router {
    /// Available route/model pairs for a harness, without credentials.
    ///
    /// Availability is checked before publication, so the native picker
    /// never advertises a target the same session could not select from the
    /// Construct route menu.
    pub fn published_models(&self, harness: &str) -> Vec<PublishedModel> {
        if !self.publish_models {
            return Vec::new();
        }
        let mut out = Vec::new();
        for provider in OauthProvider::ALL {
            if self.oauth_blocker(*provider, harness).is_some() {
                continue;
            }
            for model in self.oauth_model_list(*provider) {
                let effort = oauth::effort_support(*provider, &model);
                out.push(PublishedModel {
                    id: published_model_id_for_harness(harness, provider.name(), &model),
                    route: provider.name().to_string(),
                    model,
                    effort,
                });
            }
        }
        for (route, profile) in &self.profiles {
            if self.profile_blocker(profile, harness).is_some() {
                continue;
            }
            for model in self.profile_model_list(profile) {
                let effort = profile_effort_support(&profile.provider, &model);
                out.push(PublishedModel {
                    id: published_model_id_for_harness(harness, route, &model),
                    route: route.clone(),
                    model,
                    effort,
                });
            }
        }
        out
    }

    /// Anthropic Models API-shaped response consumed by Claude Code's
    /// gateway model discovery. Native picker rows are built from these
    /// fields; credentials and endpoint details never enter the response.
    pub fn claude_models_response(&self) -> Value {
        let data: Vec<Value> = self
            .published_models("claude")
            .into_iter()
            .map(|model| {
                json!({
                    "id": model.id,
                    // Claude Code hardcodes the secondary label for every
                    // discovered model to "From gateway" and only consumes
                    // `id` plus `display_name` from this response. Keep the
                    // owning integration explicit in the customizable field
                    // so Construct routes cannot be mistaken for rows owned
                    // by an unrelated user-configured gateway.
                    "display_name": format!("{} · {} · Construct", model.model, model.route),
                    "description": format!(
                        "Routed by Construct through {} to {}.",
                        model.route, model.model
                    ),
                    "type": "model",
                    "created_at": "1970-01-01T00:00:00Z"
                })
            })
            .collect();
        json!({
            "data": data,
            "has_more": false,
            "first_id": null,
            "last_id": null
        })
    }

    /// Resolve a model id carried by a harness request. An id outside the
    /// Construct namespace is not ours. A malformed or no-longer-published
    /// Construct id fails closed rather than leaking to the harness's native
    /// provider as an accidental paid request.
    pub fn resolve_published_model(&self, harness: &str, id: &str) -> Result<Option<ArmedRoute>> {
        let Some((route, model)) = decode_published_model_id(id)
            .with_context(|| format!("invalid Construct model id {id:?}"))?
        else {
            return Ok(None);
        };
        let allowed = self
            .published_models(harness)
            .into_iter()
            .any(|candidate| candidate.route == route && candidate.model == model);
        if !allowed {
            bail!("Construct model {id:?} is not available for the {harness} harness");
        }
        self.resolve(&route, harness, Some(&model)).map(Some)
    }

    /// Native models the harness selects for itself rather than offering to
    /// the user: the approval reviewer, and anything else the vendor ships
    /// hidden from the picker. Read from the harness's own catalog, so the
    /// set follows the vendor instead of a slug list we would have to chase.
    pub fn native_role_models(&self, harness: &str) -> Result<HashSet<String>> {
        if harness != "codex" {
            return Ok(HashSet::new());
        }
        let source = active_codex_catalog_source()?;
        let raw = std::fs::read(&source)
            .with_context(|| format!("read Codex model catalog {}", source.display()))?;
        let baseline: Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parse Codex model catalog {}", source.display()))?;
        Ok(role_model_slugs(&baseline))
    }

    pub fn write_codex_catalog(&self, session_id: &str) -> Result<PathBuf> {
        let source = active_codex_catalog_source()?;
        let raw = std::fs::read(&source)
            .with_context(|| format!("read Codex model catalog {}", source.display()))?;
        let baseline: Value = serde_json::from_slice(&raw)
            .with_context(|| format!("parse Codex model catalog {}", source.display()))?;
        let published = self.published_models("codex");
        if published.is_empty() {
            bail!("no available routes can be published to Codex");
        }
        let catalog = build_codex_catalog(baseline, &published, &self.featured_models)?;

        let dir = self.state_dir.join("router").join("catalogs");
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let safe_id: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("{safe_id}.json"));
        let temp = dir.join(format!(".{safe_id}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let bytes = serde_json::to_vec_pretty(&catalog).context("encode Codex model catalog")?;
        std::fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("install {}", path.display()))?;
        Ok(path)
    }
}

fn codex_home() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CODEX_HOME") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Honor a user-selected catalog as the native baseline; otherwise use the
/// cache Codex itself maintains. The generated session catalog never
/// modifies either source.
fn active_codex_catalog_source() -> Result<PathBuf> {
    let home = codex_home()?;
    let config_path = home.join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = raw.parse::<toml::Value>() {
            if let Some(value) = config
                .get("model_catalog_json")
                .and_then(toml::Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                let path = Path::new(value);
                return Ok(if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    home.join(path)
                });
            }
        }
    }
    Ok(home.join("models_cache.json"))
}

fn selector(route: &str, model: &str) -> String {
    format!("{route}/{model}")
}

/// Slugs a Codex catalog marks as not picker-visible. `visibility` is the
/// vendor's own statement about whether a user can choose the model, which
/// is exactly the line between a model the session runs on and a model the
/// harness fills an internal seat with. An entry that omits the field is
/// treated as selectable: over-applying the pin is the current behavior,
/// and a missing field is not evidence of an internal seat.
fn role_model_slugs(catalog: &Value) -> HashSet<String> {
    let Some(models) = catalog.get("models").and_then(Value::as_array) else {
        return HashSet::new();
    };
    models
        .iter()
        .filter(|entry| {
            entry
                .get("visibility")
                .and_then(Value::as_str)
                .is_some_and(|visibility| !visibility.eq_ignore_ascii_case("list"))
        })
        .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn feature_ranks(models: &[PublishedModel], configured: &[String]) -> BTreeMap<String, usize> {
    let available: std::collections::HashSet<String> = models
        .iter()
        .map(|model| selector(&model.route, &model.model))
        .collect();
    let mut ranks = BTreeMap::new();
    for value in configured {
        if ranks.len() == 5 {
            break;
        }
        if available.contains(value) && !ranks.contains_key(value) {
            ranks.insert(value.clone(), ranks.len());
        }
    }
    ranks
}

fn replace_identity(entry: &mut Value, model: &PublishedModel) {
    const NATIVE_IDENTITY: &str = "You are Codex, an agent based on GPT-5.";
    let replacement = format!(
        "You are Codex, a coding agent powered by {} through Construct.",
        model.model
    );
    if let Some(text) = entry
        .get("base_instructions")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        entry["base_instructions"] = Value::String(text.replacen(NATIVE_IDENTITY, &replacement, 1));
    }
    if let Some(text) = entry
        .pointer("/model_messages/instructions_template")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        if let Some(slot) = entry.pointer_mut("/model_messages/instructions_template") {
            *slot = Value::String(text.replacen(NATIVE_IDENTITY, &replacement, 1));
        }
    }
}

pub fn build_codex_catalog(
    mut baseline: Value,
    published: &[PublishedModel],
    configured_featured: &[String],
) -> Result<Value> {
    let models = baseline
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("Codex catalog has no models array"))?;
    models.retain(|entry| {
        !entry
            .get("slug")
            .and_then(Value::as_str)
            .is_some_and(|slug| slug.starts_with(PUBLISHED_MODEL_PREFIX))
    });
    let template = models
        .iter()
        .find(|entry| {
            entry
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| !slug.contains('/'))
                && entry.get("base_instructions").is_some()
        })
        .cloned()
        .ok_or_else(|| anyhow!("Codex catalog has no native model template"))?;
    let ranks = feature_ranks(published, configured_featured);
    // Codex's v2 agent-message path may encrypt a child task for the
    // ChatGPT backend. A routed provider cannot decrypt that payload. The
    // v1 surface carries the task as ordinary Responses input, so
    // published catalogs pin the whole session to v1 and keep native/routed
    // delegation interoperable.
    for entry in models.iter_mut() {
        entry["multi_agent_version"] = Value::String("v1".to_string());
    }
    if !ranks.is_empty() {
        // Codex sorts the native picker and the spawn_agent override enum by
        // ascending priority. An explicit featured roster must therefore
        // move every non-featured native below that bounded block.
        for entry in models.iter_mut() {
            let priority = entry.get("priority").and_then(Value::as_u64).unwrap_or(100);
            entry["priority"] = json!(priority.max((ranks.len() + 100) as u64));
        }
    }

    for (index, model) in published.iter().enumerate() {
        let mut entry = template.clone();
        entry["slug"] = Value::String(model.id.clone());
        entry["display_name"] = Value::String(format!("{} · {}", model.model, model.route));
        entry["description"] = Value::String(format!(
            "Routed by Construct through {} to {}.",
            model.route, model.model
        ));
        entry["visibility"] = Value::String("list".to_string());
        entry["priority"] = json!(if ranks.is_empty() {
            // Match Codex's native ordering convention: routed models are
            // picker-visible and some enter the five-model subagent enum,
            // while the priority-1 native default remains unchanged.
            5
        } else {
            ranks
                .get(&selector(&model.route, &model.model))
                .copied()
                .unwrap_or(100 + ranks.len() + index)
        });
        let (default_level, level_names) = effort_level_set(model.effort);
        let levels = match model.effort {
            EffortSupport::Verbatim => json!([
                {"effort": "low", "description": "Fastest, minimal reasoning"},
                {"effort": "medium", "description": "Balanced reasoning"},
                {"effort": "high", "description": "Deepest reasoning, slower"}
            ]),
            EffortSupport::Thinking => json!([
                {"effort": "minimal", "description": "No extended thinking"},
                {"effort": "low", "description": "Extended thinking, 4k token budget"},
                {"effort": "medium", "description": "Extended thinking, 12k token budget"},
                {"effort": "high", "description": "Extended thinking, 24k token budget"}
            ]),
            EffortSupport::Grok => json!([
                {"effort": "low", "description": "Quick, fast implementation"},
                {"effort": "medium", "description": "Balanced implementation and testing"},
                {"effort": "high", "description": "Highest quality with extensive reasoning"}
            ]),
            EffortSupport::Kimi => json!([
                {"effort": "low", "description": "Lower Kimi thinking effort"},
                {"effort": "high", "description": "High Kimi thinking effort"},
                {"effort": "xhigh", "description": "Maximum Kimi thinking effort"}
            ]),
            EffortSupport::DeepSeek => json!([
                {"effort": "low", "description": "Brief reasoning, fastest"},
                {"effort": "high", "description": "DeepSeek default reasoning depth"},
                {"effort": "max", "description": "Longest reasoning, slowest"}
            ]),
            EffortSupport::Unsupported => json!([{
                "effort": "medium",
                "description": "Provider-default reasoning through Construct"
            }]),
        };
        debug_assert_eq!(
            level_names.len(),
            levels.as_array().map(|a| a.len()).unwrap_or(0),
            "catalog descriptions must track effort_level_set"
        );
        entry["default_reasoning_level"] = Value::String(default_level.to_string());
        entry["supported_reasoning_levels"] = levels;
        // Route profiles currently carry no capability metadata. Advertise a
        // conservative text/tool surface until the shared registry grows
        // explicit per-model capabilities.
        entry["context_window"] = json!(64_000);
        entry["max_context_window"] = json!(64_000);
        entry["effective_context_window_percent"] = json!(90);
        entry["input_modalities"] = json!(["text"]);
        entry["supports_image_detail_original"] = Value::Bool(false);
        entry["supports_search_tool"] = Value::Bool(false);
        entry["multi_agent_version"] = Value::String("v1".to_string());
        if let Some(object) = entry.as_object_mut() {
            object.remove("web_search_tool_type");
            object.remove("service_tiers");
            object.remove("additional_speed_tiers");
            object.remove("supports_websockets");
            object.remove("prefer_websockets");
            object.remove("availability_nux");
            object.insert("upgrade".to_string(), Value::Null);
        }
        replace_identity(&mut entry, model);
        models.push(entry);
    }
    Ok(baseline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Value {
        json!({
            "client_version": "test",
            "models": [{
                "slug": "gpt-native",
                "display_name": "Native",
                "description": "native",
                "priority": 1,
                "visibility": "list",
                "base_instructions": "You are Codex, an agent based on GPT-5. Keep working.",
                "model_messages": {
                    "instructions_template": "You are Codex, an agent based on GPT-5. Use tools."
                },
                "supported_reasoning_levels": [{"effort":"high"}],
                "context_window": 200000,
                "service_tiers": [{"id":"priority"}],
                "web_search_tool_type": "text_and_image"
            }]
        })
    }

    /// The reviewer seat is the case this exists for: Codex ships it
    /// hidden, so `visibility` is the vendor telling us the user never
    /// chose it and a session pin was never a statement about it.
    #[test]
    fn hidden_catalog_entries_are_role_models_and_listed_ones_are_not() {
        let catalog = json!({
            "models": [
                {"slug": "gpt-5.6-sol", "visibility": "list"},
                {"slug": "codex-auto-review", "visibility": "hide"},
                {"slug": "no-visibility-field"},
            ]
        });
        let roles = role_model_slugs(&catalog);
        assert!(roles.contains("codex-auto-review"));
        assert!(!roles.contains("gpt-5.6-sol"));
        // Absent evidence is not evidence of an internal seat: an entry
        // without the field keeps following the pin, as it does today.
        assert!(!roles.contains("no-visibility-field"));
    }

    #[test]
    fn a_catalog_without_models_yields_no_role_models() {
        assert!(role_model_slugs(&json!({})).is_empty());
    }

    /// Routed entries are published for the picker, so they must never be
    /// mistaken for internal seats and skip their own route.
    #[test]
    fn published_routed_entries_are_never_role_models() {
        let route = PublishedModel {
            id: published_model_id("claude-oauth", "opus"),
            route: "claude-oauth".into(),
            model: "opus".into(),
            effort: EffortSupport::Unsupported,
        };
        let catalog = build_codex_catalog(baseline(), &[route.clone()], &[]).unwrap();
        assert!(!role_model_slugs(&catalog).contains(&route.id));
    }

    #[test]
    fn generated_catalog_keeps_native_and_adds_routed_entries() {
        let route = PublishedModel {
            id: published_model_id("claude-oauth", "opus"),
            route: "claude-oauth".into(),
            model: "opus".into(),
            effort: EffortSupport::Unsupported,
        };
        let catalog = build_codex_catalog(baseline(), &[route.clone()], &[]).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "gpt-native");
        assert_eq!(models[0]["multi_agent_version"], "v1");
        assert_eq!(models[1]["slug"], route.id);
        assert_eq!(models[1]["multi_agent_version"], "v1");
        assert_eq!(models[1]["priority"], 5);
        assert_eq!(models[1]["context_window"], 64_000);
        assert!(!models[1]["base_instructions"]
            .as_str()
            .unwrap()
            .contains("based on GPT-5"));
        assert!(models[1].get("service_tiers").is_none());
        // An effort choice cannot reach an Anthropic-dialect target yet, so
        // the picker must not offer one.
        assert_eq!(
            models[1]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn effort_selectable_route_advertises_real_levels() {
        let route = PublishedModel {
            id: published_model_id("codex-oauth", "gpt-5.2-codex"),
            route: "codex-oauth".into(),
            model: "gpt-5.2-codex".into(),
            effort: EffortSupport::Verbatim,
        };
        let catalog = build_codex_catalog(baseline(), &[route], &[]).unwrap();
        let levels = catalog["models"][1]["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(levels, vec!["low", "medium", "high"]);
        assert_eq!(catalog["models"][1]["default_reasoning_level"], "medium");
    }

    #[test]
    fn thinking_route_advertises_off_and_budget_levels() {
        let route = PublishedModel {
            id: published_model_id("claude-oauth", "opus"),
            route: "claude-oauth".into(),
            model: "opus".into(),
            effort: EffortSupport::Thinking,
        };
        let catalog = build_codex_catalog(baseline(), &[route], &[]).unwrap();
        let levels = catalog["models"][1]["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(levels, vec!["minimal", "low", "medium", "high"]);
        assert_eq!(catalog["models"][1]["default_reasoning_level"], "minimal");
    }

    #[test]
    fn grok_and_kimi_advertise_their_native_scales() {
        let grok = PublishedModel {
            id: published_model_id("grok-oauth", "grok-4.5"),
            route: "grok-oauth".into(),
            model: "grok-4.5".into(),
            effort: EffortSupport::Grok,
        };
        let kimi = PublishedModel {
            id: published_model_id("kimi-oauth", "k3"),
            route: "kimi-oauth".into(),
            model: "k3".into(),
            effort: EffortSupport::Kimi,
        };
        let catalog = build_codex_catalog(baseline(), &[grok, kimi], &[]).unwrap();
        let efforts = |index: usize| {
            catalog["models"][index]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|level| level["effort"].as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(efforts(1), vec!["low", "medium", "high"]);
        assert_eq!(catalog["models"][1]["default_reasoning_level"], "high");
        assert_eq!(efforts(2), vec!["low", "high", "xhigh"]);
        assert_eq!(catalog["models"][2]["default_reasoning_level"], "high");
    }

    #[test]
    fn configured_feature_order_controls_priorities() {
        let a = PublishedModel {
            id: published_model_id("a", "one"),
            route: "a".into(),
            model: "one".into(),
            effort: EffortSupport::Verbatim,
        };
        let b = PublishedModel {
            id: published_model_id("b", "two"),
            route: "b".into(),
            model: "two".into(),
            effort: EffortSupport::Unsupported,
        };
        let catalog =
            build_codex_catalog(baseline(), &[a, b], &["b/two".into(), "a/one".into()]).unwrap();
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models[0]["priority"], 102);
        assert_eq!(models[1]["priority"], 1);
        assert_eq!(models[2]["priority"], 0);
    }
}
