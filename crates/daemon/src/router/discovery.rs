//! Dynamic model discovery for route targets (spec 0209).
//!
//! Providers that expose a model-listing endpoint get their picker rows
//! fetched live instead of relying only on the curated catalog. Discovery
//! is additive and best-effort: the curated list keeps its place at the
//! front (stable, known-good ordering), discovered ids are appended, and
//! any failure — endpoint down, key invalid, unsupported provider —
//! degrades to exactly the curated behavior. The menus never gate what a
//! user can request, so nothing here is load-bearing for routing itself.
//!
//! Fetches happen on demand when a client asks for the route menu, through
//! a TTL cache so repeated opens are instant and a dead endpoint is not
//! hammered. All fetches for one refresh run concurrently under a single
//! wall-clock budget: the menu may open a few seconds late once, never
//! hang.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// How long a successful listing stays fresh.
const OK_TTL: Duration = Duration::from_secs(300);
/// How long a failure is remembered before the endpoint is tried again.
/// Shorter than [`OK_TTL`] so a just-exported key or recovered endpoint is
/// picked up quickly, long enough that scrolling a menu doesn't retry a
/// dead host on every repaint.
const ERR_TTL: Duration = Duration::from_secs(60);
/// Per-request ceiling and the whole refresh's wall-clock budget. The
/// route menu is interactive; a slow vendor gets cut off, not waited for.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_BUDGET: Duration = Duration::from_secs(4);

/// The listing dialects we know how to speak. Deliberately fewer than the
/// routable dialects: a provider absent here simply keeps its curated
/// list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// `GET {base}/models` with a bearer token; ids at `data[].id`.
    /// OpenAI's own surface and every vendor that mimics it (Grok,
    /// DeepSeek, OpenRouter, Responses-only backends).
    OpenAiCompatible,
    /// `GET {base}/models` with `x-api-key` + `anthropic-version`; ids at
    /// `data[].id`.
    Anthropic,
    /// `GET {base}/models?key=` ; ids at `models[].name` (prefixed
    /// `models/`), filtered to those supporting `generateContent`.
    Gemini,
}

/// Which listing dialect a `[smith.models.*]` wire provider speaks, if
/// any. Azure is deliberately absent (its listing path is
/// deployment-scoped, not `{base}/models`), as is Meta (no public listing
/// surface), so both keep curated lists.
pub fn list_kind(provider: &str) -> Option<ListKind> {
    match provider.to_ascii_lowercase().as_str() {
        "openai" | "openai-responses" | "grok" | "deepseek" | "openrouter" => {
            Some(ListKind::OpenAiCompatible)
        }
        "anthropic" => Some(ListKind::Anthropic),
        "gemini" | "google" => Some(ListKind::Gemini),
        _ => None,
    }
}

/// Cache key for one endpoint. Keyed by provider + base URL (not route
/// name) so two profiles pointing at the same endpoint share one fetch.
pub fn cache_key(provider: &str, base_url: &str) -> String {
    format!("{}|{}", provider.to_ascii_lowercase(), base_url.trim_end_matches('/'))
}

/// One endpoint to (re)fetch.
#[derive(Debug, Clone)]
pub struct FetchSpec {
    pub key: String,
    pub kind: ListKind,
    pub base_url: String,
    pub api_key: String,
}

struct Entry {
    /// Empty on failure — [`DiscoveryCache::get`] hides that from callers
    /// so a failed fetch reads as "nothing discovered".
    models: Arc<Vec<String>>,
    fetched_at: Instant,
    ok: bool,
}

/// TTL cache of discovered model lists, keyed by [`cache_key`].
#[derive(Default)]
pub struct DiscoveryCache {
    entries: RwLock<HashMap<String, Entry>>,
}

impl DiscoveryCache {
    /// Discovered ids for an endpoint, or `None` when nothing (usable) is
    /// cached. Staleness is not checked here: a stale success keeps
    /// serving until the next refresh replaces it, so the menu never
    /// regresses to curated-only while a refetch is in flight.
    pub fn get(&self, key: &str) -> Option<Arc<Vec<String>>> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        let entry = entries.get(key)?;
        if entry.models.is_empty() {
            return None;
        }
        Some(entry.models.clone())
    }

    fn stale(&self, key: &str) -> bool {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        match entries.get(key) {
            None => true,
            Some(e) => e.fetched_at.elapsed() > if e.ok { OK_TTL } else { ERR_TTL },
        }
    }

    pub(crate) fn insert(&self, key: &str, models: Vec<String>, ok: bool) {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        entries.insert(
            key.to_string(),
            Entry {
                models: Arc::new(models),
                fetched_at: Instant::now(),
                ok,
            },
        );
    }

    /// Fetch every stale spec concurrently, within [`REFRESH_BUDGET`].
    /// Errors are recorded (empty entry, short TTL) rather than surfaced:
    /// discovery failing must look exactly like discovery not existing.
    pub async fn refresh(&self, specs: Vec<FetchSpec>) {
        let stale: Vec<FetchSpec> = specs.into_iter().filter(|s| self.stale(&s.key)).collect();
        if stale.is_empty() {
            return;
        }
        let client = match reqwest::Client::builder()
            .connect_timeout(FETCH_TIMEOUT)
            .timeout(FETCH_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let fetches = stale.into_iter().map(|spec| {
            let client = client.clone();
            async move {
                let fetched = fetch_models(&client, &spec).await;
                (spec.key, fetched)
            }
        });
        let joined = tokio::time::timeout(REFRESH_BUDGET, futures::future::join_all(fetches));
        let Ok(results) = joined.await else {
            // Budget exhausted: whatever resolved in time was returned by
            // join_all only as a whole, so nothing landed. The stale
            // entries stay; the next open retries.
            return;
        };
        for (key, fetched) in results {
            match fetched {
                Ok(models) => self.insert(&key, models, true),
                Err(e) => {
                    tracing::debug!(target: "router", key, error = %e, "model discovery failed");
                    self.insert(&key, Vec::new(), false);
                }
            }
        }
    }
}

async fn fetch_models(client: &reqwest::Client, spec: &FetchSpec) -> anyhow::Result<Vec<String>> {
    let base = spec.base_url.trim_end_matches('/');
    let req = match spec.kind {
        ListKind::OpenAiCompatible => client
            .get(format!("{base}/models"))
            .bearer_auth(&spec.api_key),
        ListKind::Anthropic => client
            .get(format!("{base}/models?limit=1000"))
            .header("x-api-key", &spec.api_key)
            .header("anthropic-version", "2023-06-01"),
        ListKind::Gemini => client.get(format!(
            "{base}/models?pageSize=1000&key={}",
            spec.api_key
        )),
    };
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{} from {base}/models", resp.status());
    }
    let body = resp.text().await?;
    Ok(parse_models(spec.kind, &body))
}

/// Extract usable model ids from a listing response body. Unknown shapes
/// parse to empty (treated as a failed fetch upstream via `ok=true` with
/// no rows — which [`DiscoveryCache::get`] already hides).
pub fn parse_models(kind: ListKind, body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match kind {
        ListKind::OpenAiCompatible | ListKind::Anthropic => {
            for item in v.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
                let Some(id) = item.get("id").and_then(|s| s.as_str()) else {
                    continue;
                };
                if kind == ListKind::OpenAiCompatible && !is_chatlike_id(id) {
                    continue;
                }
                if !out.iter().any(|m| m == id) {
                    out.push(id.to_string());
                }
            }
        }
        ListKind::Gemini => {
            for item in v
                .get("models")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
            {
                let supports_generate = item
                    .get("supportedGenerationMethods")
                    .and_then(|m| m.as_array())
                    .map(|m| m.iter().any(|s| s.as_str() == Some("generateContent")))
                    .unwrap_or(false);
                if !supports_generate {
                    continue;
                }
                let Some(name) = item.get("name").and_then(|s| s.as_str()) else {
                    continue;
                };
                let id = name.strip_prefix("models/").unwrap_or(name);
                if !out.iter().any(|m| m == id) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

/// Heuristic filter for OpenAI-compatible listings, which mix chat models
/// with embeddings, speech, image, and moderation ids. Excluding a usable
/// model here only hides a menu row — the id still works typed — so the
/// denylist stays small and obviously non-chat.
fn is_chatlike_id(id: &str) -> bool {
    const NON_CHAT: &[&str] = &[
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "davinci",
        "babbage",
        "audio",
        "realtime",
        "transcribe",
        "image",
    ];
    let lower = id.to_ascii_lowercase();
    !NON_CHAT.iter().any(|t| lower.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_listing_parses_and_filters_non_chat_ids() {
        let body = r#"{"object":"list","data":[
            {"id":"gpt-5","object":"model"},
            {"id":"text-embedding-3-small","object":"model"},
            {"id":"whisper-1","object":"model"},
            {"id":"gpt-4o-realtime-preview","object":"model"},
            {"id":"dall-e-3","object":"model"},
            {"id":"gpt-5-mini","object":"model"}
        ]}"#;
        assert_eq!(
            parse_models(ListKind::OpenAiCompatible, body),
            vec!["gpt-5".to_string(), "gpt-5-mini".to_string()]
        );
    }

    /// OpenRouter's listing uses the same shape with `vendor/model` ids;
    /// server order (recency) is preserved so new/stealth ids surface near
    /// the top of the discovered tail.
    #[test]
    fn openrouter_slash_ids_pass_through_in_order() {
        let body = r#"{"data":[
            {"id":"stealth/ox-alpha"},
            {"id":"anthropic/claude-sonnet-4.6"},
            {"id":"google/gemini-2.5-flash-image"}
        ]}"#;
        assert_eq!(
            parse_models(ListKind::OpenAiCompatible, body),
            vec![
                "stealth/ox-alpha".to_string(),
                "anthropic/claude-sonnet-4.6".to_string()
            ]
        );
    }

    #[test]
    fn anthropic_listing_is_not_chat_filtered() {
        let body = r#"{"data":[
            {"id":"claude-opus-4-8","display_name":"Claude Opus 4.8"},
            {"id":"claude-haiku-4-5","display_name":"Claude Haiku 4.5"}
        ],"has_more":false}"#;
        assert_eq!(
            parse_models(ListKind::Anthropic, body),
            vec!["claude-opus-4-8".to_string(), "claude-haiku-4-5".to_string()]
        );
    }

    #[test]
    fn gemini_listing_strips_prefix_and_requires_generate_content() {
        let body = r#"{"models":[
            {"name":"models/gemini-2.5-pro","supportedGenerationMethods":["generateContent","countTokens"]},
            {"name":"models/text-embedding-004","supportedGenerationMethods":["embedContent"]},
            {"name":"models/gemini-2.5-flash","supportedGenerationMethods":["generateContent"]}
        ]}"#;
        assert_eq!(
            parse_models(ListKind::Gemini, body),
            vec!["gemini-2.5-pro".to_string(), "gemini-2.5-flash".to_string()]
        );
    }

    #[test]
    fn malformed_bodies_parse_to_empty() {
        assert!(parse_models(ListKind::OpenAiCompatible, "not json").is_empty());
        assert!(parse_models(ListKind::Anthropic, r#"{"data":"nope"}"#).is_empty());
        assert!(parse_models(ListKind::Gemini, "{}").is_empty());
    }

    #[test]
    fn list_kind_covers_routable_listing_providers_only() {
        assert_eq!(list_kind("openai"), Some(ListKind::OpenAiCompatible));
        assert_eq!(list_kind("openrouter"), Some(ListKind::OpenAiCompatible));
        assert_eq!(list_kind("grok"), Some(ListKind::OpenAiCompatible));
        assert_eq!(list_kind("deepseek"), Some(ListKind::OpenAiCompatible));
        assert_eq!(list_kind("anthropic"), Some(ListKind::Anthropic));
        assert_eq!(list_kind("gemini"), Some(ListKind::Gemini));
        assert_eq!(list_kind("meta"), None);
        assert_eq!(list_kind("azure-openai"), None);
        assert_eq!(list_kind("ollama"), None);
    }

    #[test]
    fn cache_serves_successes_and_hides_failures() {
        let cache = DiscoveryCache::default();
        assert!(cache.get("k").is_none());
        assert!(cache.stale("k"));
        cache.insert("k", vec!["m1".into()], true);
        assert_eq!(cache.get("k").unwrap().as_slice(), ["m1".to_string()]);
        assert!(!cache.stale("k"));
        cache.insert("k", Vec::new(), false);
        assert!(cache.get("k").is_none(), "failed fetches read as nothing discovered");
    }

    /// A local stub server end-to-end: refresh fetches, caches, and a
    /// second refresh within TTL does not re-hit the endpoint.
    #[tokio::test]
    async fn refresh_fetches_once_within_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_hits = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                server_hits.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"data":[{"id":"vendor/model-a"},{"id":"vendor/model-b"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let cache = DiscoveryCache::default();
        let spec = FetchSpec {
            key: cache_key("openrouter", &format!("http://{addr}")),
            kind: ListKind::OpenAiCompatible,
            base_url: format!("http://{addr}"),
            api_key: "sk-test".into(),
        };
        cache.refresh(vec![spec.clone()]).await;
        assert_eq!(
            cache.get(&spec.key).unwrap().as_slice(),
            ["vendor/model-a".to_string(), "vendor/model-b".to_string()]
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        cache.refresh(vec![spec.clone()]).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "fresh entry must not refetch");
    }
}
