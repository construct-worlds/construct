//! Model-route transport (spec 0113 / 0110 / 0111).
//!
//! The router owns one loopback `CONNECT` listener and a per-session
//! routing table the proxy consults on every connection. Changing a
//! session's route mutates that table — the harness process is never
//! restarted, never signalled, and never learns of the change.
//!
//! Connections are attributed to sessions by the proxy credential the
//! harness presents, not by which port it dialed. One listener per home
//! is far easier to reclaim after a daemon restart than a port per
//! session, and reclaiming it is mandatory: harness processes outlive
//! the daemon and keep dialing the port they were given at spawn. The
//! bound port is persisted under `runtime_dir/router.port` so the same
//! home comes back on the same port.
//!
//! A busy port is therefore waited for, and a stand-in bound only after
//! that — never recorded as the home's own, and given up as soon as the
//! real port frees (spec 0183). A home with no port yet has nothing
//! dialing it and simply adopts whatever it can get, which is how a
//! second home on one machine gets an identity of its own.
//!
//! With `[router] enabled = false` (the default) nothing here runs: no
//! listener is bound, no CA is generated, and no session's environment is
//! touched.

pub mod ca;
pub mod catalog;
pub mod discovery;
pub mod oauth;
pub mod proxy;
pub mod translate;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use construct_protocol::{RouteOption, RouterListRoutesResult, SessionRoute};
use tokio::net::TcpListener;

use crate::config::{ModelProfile, RouterConfig};
use ca::RouterCa;
use oauth::OauthProvider;

/// Env var carrying the proxy the harness should use. This is the only
/// channel Construct injects for transport; the harness's own endpoint
/// configuration is never displaced (spec 0113).
pub const PROXY_ENV: &str = "HTTPS_PROXY";

/// How long to keep asking for the preferred port before accepting a
/// stand-in. Sized for a restart racing its own predecessor's socket, which
/// clears in milliseconds, not for waiting out another daemon.
const PORT_CLAIM_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);
const PORT_CLAIM_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// How often to try taking the preferred port back once a stand-in is in use.
const PORT_RECLAIM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the port just bound should be recorded as this home's own.
///
/// The port file exists so a home comes back where its sessions expect it.
/// That makes a stand-in port the one thing that must never be written to it
/// by a home that already has sessions: doing so converts one busy moment into
/// a permanent move and gives up on every session still dialing the old port.
fn records_port(pinned: bool, bound: u16, claimed_preferred: bool, established: bool) -> bool {
    !pinned && bound != 0 && (claimed_preferred || !established)
}
/// Lowercase spelling, honored by some clients in preference to the
/// uppercase one. Both are set to the same value.
pub const PROXY_ENV_LOWER: &str = "https_proxy";

/// Password half of the injected proxy credential. Carries no meaning —
/// only the username identifies the session — but must be present; see
/// `session_env`.
const CREDENTIAL_FILLER: &str = "construct";

/// A wire dialect the router understands.
///
/// This is the axis that decides whether a route is a redirect or a
/// translation: same dialect on both sides means rewrite the destination
/// and forward the bytes; different dialects means the request and the
/// response stream are rebuilt (spec 0116).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Anthropic Messages (`/v1/messages`).
    AnthropicMessages,
    /// Google Gemini GenerateContent (`:generateContent` /
    /// `:streamGenerateContent`).
    GoogleGemini,
    /// OpenAI Chat Completions (`/chat/completions`).
    OpenAiChat,
    /// OpenAI Responses (`/responses`).
    OpenAiResponses,
}

impl Dialect {
    pub fn label(self) -> &'static str {
        match self {
            Dialect::AnthropicMessages => "anthropic",
            Dialect::GoogleGemini => "google-gemini",
            Dialect::OpenAiChat => "openai-chat",
            Dialect::OpenAiResponses => "openai-responses",
        }
    }
}

/// Map a `[smith.models.*]` provider onto a dialect the router can serve.
///
/// Providers absent here are declared, usable by smith, and simply not
/// routable. Claiming support without a translator would corrupt turns
/// rather than fail cleanly.
pub fn provider_dialect(provider: &str) -> Option<Dialect> {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => Some(Dialect::AnthropicMessages),
        "gemini" | "google" => Some(Dialect::GoogleGemini),
        // Grok, DeepSeek, and OpenRouter are served by smith's OpenAI client
        // and speak the same wire format.
        "openai" | "grok" | "deepseek" | "openrouter" => Some(Dialect::OpenAiChat),
        // Azure's current v1 API uses Responses on the wire; its adapter
        // difference is the `api-key` header, not a separate JSON dialect.
        // Meta serves Muse Spark over the same Responses surface — smith's
        // own Meta client posts to `/v1/responses` and decodes the standard
        // `response.*` event vocabulary, which is what this translator emits.
        "openai-responses" | "azure" | "azure-openai" | "meta" => {
            Some(Dialect::OpenAiResponses)
        }
        _ => None,
    }
}

/// Whether a target refuses an assistant turn that does not carry back the
/// reasoning it produced for that turn (spec 0181).
///
/// Measured, not documented: DeepSeek's thinking mode rejects a replayed
/// tool-calling turn whose `reasoning_content` is missing, while accepting
/// the same turn when it is present — including when it is empty.
pub fn provider_echoes_reasoning(provider: &str) -> bool {
    matches!(provider.to_ascii_lowercase().as_str(), "deepseek")
}

/// How a route's target consumes a requested reasoning effort (spec 0160).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSupport {
    Unsupported,
    Verbatim,
    Thinking,
    /// Grok's native `reasoning_effort`, with `high` as its default.
    Grok,
    /// Kimi K3's always-on thinking plus `output_config.effort`.
    Kimi,
    /// DeepSeek's `reasoning_effort` enum. Its accepted values include
    /// `medium` and `xhigh`, but only `low` / `high` / `max` were observed to
    /// grade the work monotonically, so those are the levels offered.
    DeepSeek,
}

/// How a declared or built-in profile consumes a requested effort.
///
/// Model-aware, not provider-aware: a vendor may grade effort on one model
/// and floor every level to the same value on another, and advertising a
/// scale the model does not honor is worse than advertising none.
pub fn profile_effort_support(provider: &str, model: &str) -> EffortSupport {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => EffortSupport::Thinking,
        "openai" | "openai-responses" | "azure" | "azure-openai" => EffortSupport::Verbatim,
        "deepseek" => deepseek_effort_support(model),
        _ => EffortSupport::Unsupported,
    }
}

/// DeepSeek grades effort on the flash tier only. The pro tier accepts every
/// level and floors them all to its own default, so measured reasoning length
/// is flat and non-monotonic across `low` / `high` / `max` — it gets no scale
/// rather than a picker column that changes nothing.
fn deepseek_effort_support(model: &str) -> EffortSupport {
    let m = model.to_ascii_lowercase();
    if m.contains("flash") {
        EffortSupport::DeepSeek
    } else {
        EffortSupport::Unsupported
    }
}

/// Default and selectable levels for a target's effort support (spec 0160).
///
/// A single-element list is a provider-default stub — the native catalog
/// still advertises it so Codex has a level, but Construct's pin picker
/// treats it as "no choice" and omits the third column (spec 0165).
pub fn effort_level_set(support: EffortSupport) -> (&'static str, &'static [&'static str]) {
    match support {
        EffortSupport::Verbatim => ("medium", &["low", "medium", "high"]),
        EffortSupport::Thinking => ("minimal", &["minimal", "low", "medium", "high"]),
        EffortSupport::Grok => ("high", &["low", "medium", "high"]),
        EffortSupport::Kimi => ("high", &["low", "high", "xhigh"]),
        // DeepSeek's own default effort is `high`.
        EffortSupport::DeepSeek => ("high", &["low", "high", "max"]),
        EffortSupport::Unsupported => ("medium", &["medium"]),
    }
}

/// Levels a pin-router third column may offer. Empty when the target has
/// no real selectable scale.
pub fn effort_levels_for_picker(support: EffortSupport) -> Vec<String> {
    let (_, levels) = effort_level_set(support);
    if levels.len() <= 1 {
        Vec::new()
    } else {
        levels.iter().map(|s| (*s).to_string()).collect()
    }
}

/// Per-model effort map for a route option's picker column.
fn efforts_for_models(
    models: impl IntoIterator<Item = String>,
    support_for: impl Fn(&str) -> EffortSupport,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for model in models {
        let levels = effort_levels_for_picker(support_for(&model));
        if !levels.is_empty() {
            out.insert(model, levels);
        }
    }
    out
}

/// How a harness can be routed.
///
/// Per spec 0115 each entry is an empirical claim about a specific
/// harness, established by a probe (see `router_probe` in the e2e suite),
/// not a reading of that harness's documentation. A harness absent from
/// this table is not route-capable, and offering to route it is a bug.
#[derive(Debug, Clone, Copy)]
pub struct HarnessRouting {
    /// Wire dialect the harness speaks to its model endpoint.
    pub dialect: Dialect,
    /// Hosts that carry model traffic and may therefore be intercepted
    /// while a route is armed. Everything else always tunnels.
    pub intercept_hosts: &'static [&'static str],
    /// How this harness can be made to trust the router CA. Empty means
    /// interception is impossible: the harness can be observed but not
    /// redirected.
    pub ca_env: &'static [CaChannel],
}

/// A variable through which a harness accepts a certificate authority,
/// and — critically — what that variable *does* to the existing trust.
///
/// The distinction is not cosmetic. Handing a replacing variable a file
/// containing only the router CA leaves the session unable to reach
/// anything else at all; it must be given the composed bundle instead.
/// Which mode a variable has is a per-harness, probe-established fact:
/// the same `SSL_CERT_FILE` is additive for one harness and replacing for
/// another.
#[derive(Debug, Clone, Copy)]
pub struct CaChannel {
    pub var: &'static str,
    pub mode: CaMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaMode {
    /// Adds to the system roots. Gets the bare CA file.
    Additive,
    /// Replaces the system roots. Gets the composed bundle, or the
    /// harness is not routable at all.
    Replacing,
}

pub fn harness_routing(harness: &str) -> Option<HarnessRouting> {
    match harness {
        // Node/undici: honors HTTPS_PROXY and NODE_EXTRA_CA_CERTS.
        // Probed: completes a full turn through the injected proxy.
        "claude" => Some(HarnessRouting {
            dialect: Dialect::AnthropicMessages,
            intercept_hosts: &["api.anthropic.com"],
            ca_env: &[CaChannel {
                var: "NODE_EXTRA_CA_CERTS",
                mode: CaMode::Additive,
            }],
        }),
        // Node. Probed end to end; a forged leaf signed by our CA was
        // accepted through NODE_EXTRA_CA_CERTS.
        "pi" => Some(HarnessRouting {
            dialect: Dialect::OpenAiResponses,
            intercept_hosts: &["chatgpt.com"],
            ca_env: &[CaChannel {
                var: "NODE_EXTRA_CA_CERTS",
                mode: CaMode::Additive,
            }],
        }),
        // Rust. Honors SSL_CERT_FILE, which REPLACES the system roots —
        // verified by pointing it at a bundle without them and watching
        // every TLS connection fail. It therefore gets the composed bundle,
        // and is unroutable if that bundle cannot be built. ChatGPT auth
        // uses chatgpt.com; API-key auth uses api.openai.com. Catalog-enabled
        // sessions can carry Construct ids through either native origin.
        "codex" => Some(HarnessRouting {
            dialect: Dialect::OpenAiResponses,
            intercept_hosts: &["chatgpt.com", "api.openai.com"],
            ca_env: &[CaChannel {
                var: "SSL_CERT_FILE",
                mode: CaMode::Replacing,
            }],
        }),
        // Native binary. Rejects NODE_EXTRA_CA_CERTS outright but honors
        // SSL_CERT_FILE *additively* — verified by reaching its real
        // endpoint while trusting our CA through that variable. Do not
        // copy this entry to another harness without re-probing: the same
        // variable REPLACES the system roots for codex and hermes, and
        // setting it there would break all of their TLS.
        "grok" => Some(HarnessRouting {
            dialect: Dialect::OpenAiResponses,
            intercept_hosts: &["cli-chat-proxy.grok.com"],
            ca_env: &[CaChannel {
                var: "SSL_CERT_FILE",
                mode: CaMode::Additive,
            }],
        }),
        // Python. Honors SSL_CERT_FILE, which REPLACES the system roots
        // (same class as codex), so it also gets the composed bundle.
        // Speaks plain Chat Completions to its inference host; the portal
        // host it also talks to is deliberately NOT in the intercept list.
        //
        // hermes can be pointed at another provider, and its endpoint host
        // moves with it. A session on a non-default provider simply never
        // matches this host, so it stays a pass-through — inert and
        // reported, never mis-intercepted.
        "hermes" => Some(HarnessRouting {
            dialect: Dialect::OpenAiChat,
            intercept_hosts: &["inference-api.nousresearch.com"],
            ca_env: &[CaChannel {
                var: "SSL_CERT_FILE",
                mode: CaMode::Replacing,
            }],
        }),
        // grok, opencode and pi have all been probed and
        // all honor the proxy environment — pass-through works for every
        // one of them. They are absent anyway, for two distinct reasons
        // that matter to anyone extending this:
        //
        // - `opencode` has no fixed endpoint host: it follows whatever
        //   provider the user configured, so there is nothing static to
        //   put in `intercept_hosts`. Its dialect is already handled by
        //   detection; the host is the open problem.
        // - `codex` and `hermes` take their CA through `SSL_CERT_FILE`,
        //   which *replaces* the system roots rather than adding to them;
        //   both are handed the composed bundle instead of the bare CA.
        //   `grok` also uses `SSL_CERT_FILE` but additively; `opencode`
        //   and `pi` take `NODE_EXTRA_CA_CERTS`, additive, like claude.
        // - `opencode` needs more than a table entry in any case: it was
        //   observed speaking Responses only because its configured
        //   provider is Meta. Its dialect AND its endpoint host both
        //   follow whatever provider it is pointed at, so neither can be
        //   hardcoded here — it needs dialect detected from the
        //   intercepted request instead of declared.
        //
        // Measurements live in the e2e router_probe suite. Transport
        // capability alone is never route capability.
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamProxy {
    pub host: String,
    pub port: u16,
    pub authorization: Option<String>,
}

impl UpstreamProxy {
    /// Capture a pre-existing proxy setting so we can chain to it rather
    /// than bypass it. Only `host:port` and optional basic credentials are
    /// used; anything unparseable is ignored (we would rather dial direct
    /// than dial somewhere wrong).
    pub fn from_env_value(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let rest = value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"))
            .unwrap_or(value);
        let (userinfo, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let hostport = hostport.split('/').next().unwrap_or(hostport);
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (hostport.to_string(), 80u16),
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host,
            port,
            authorization: userinfo.map(|u| {
                use base64::Engine;
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(u)
                )
            }),
        })
    }
}

/// A route resolved into everything the proxy needs to serve it.
#[derive(Clone)]
pub struct ArmedRoute {
    pub name: String,
    /// Exact URL to send to. For a profile this is its base URL plus the
    /// target dialect's path; for a subscription login it is that login's
    /// own backend, which is not relocatable.
    pub endpoint: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Auth scheme and any headers the target requires beyond the key.
    pub auth: TargetAuth,
    /// Text the target requires the system prompt to open with, if any.
    pub system_prefix: Option<&'static str>,
    /// Extra headers the target rejects requests without.
    pub extra_headers: Vec<(String, String)>,
    /// Parameters this target rejects, stripped after the request is
    /// emitted. The dialect decides the shape; the target decides which of
    /// it is accepted.
    pub drop_params: &'static [&'static str],
    /// Dialect the *target* speaks. When it differs from the harness's,
    /// the proxy translates instead of merely redirecting (spec 0116).
    pub target_dialect: Dialect,
    /// Dialect the harness speaks, i.e. what the response must look like.
    pub client_dialect: Dialect,
    /// Whether and how the target honors a requested reasoning effort.
    pub effort: EffortSupport,
    /// The target requires each assistant turn to carry back the reasoning
    /// it produced for that turn (spec 0181).
    pub reasoning_echo: bool,
    /// Pin-chosen effort applied when this arm is the session's durable
    /// pin (spec 0165). Catalog-resolved request-scoped arms leave this
    /// `None` so the harness request body remains authoritative.
    pub pin_effort: Option<String>,
    pub client: reqwest::Client,
}

/// How a target authenticates. Not cosmetic: sending an OAuth bearer in
/// an `x-api-key` header (or the reverse) is rejected outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAuth {
    /// Anthropic API key header.
    ApiKeyHeader,
    /// Google Generative Language API-key header.
    GoogleApiKey,
    /// Azure OpenAI API-key header.
    AzureApiKey,
    /// `Authorization: Bearer …`.
    Bearer,
}

impl ArmedRoute {
    pub fn translates(&self) -> bool {
        self.target_dialect != self.client_dialect
    }

    /// Subscription targets always go through the translating path, even
    /// when the dialects match: their backends need required headers and a
    /// required system prefix that byte-forwarding would not apply.
    pub fn needs_rebuild(&self) -> bool {
        self.translates()
            || self.system_prefix.is_some()
            || !self.extra_headers.is_empty()
            || !self.drop_params.is_empty()
            // Carrying reasoning back is a rewrite of the request body,
            // which byte-forwarding never performs (spec 0181).
            || self.reasoning_echo
    }
}

/// Per-session routing state, looked up from the proxy credential the
/// harness presents on `CONNECT`.
pub struct SessionRouting {
    pub session_id: String,
    harness_name: String,
    pub harness: HarnessRouting,
    pub ca: Arc<RouterCa>,
    pub upstream_proxy: Option<UpstreamProxy>,
    /// This session was launched with a Construct-generated native model
    /// catalog. Its model host must be inspected even without a manually
    /// pinned route so a request-carried Construct alias can select its own
    /// target.
    catalog_enabled: AtomicBool,
    /// Native models the harness fills its own internal seats with (spec
    /// 0166). Captured at attach because the set is a property of the
    /// harness's catalog, and the proxy cannot afford a disk read per
    /// request.
    role_models: HashSet<String>,
    route: RwLock<Option<ArmedRoute>>,
    /// Reasoning the session's target has produced, kept so a later request
    /// can hand each assistant turn its own back (spec 0181).
    reasoning: RwLock<ReasoningMemo>,
    /// Bumped on every route change. Open pass-through tunnels compare it
    /// against the value they started with to notice they are stale: a
    /// tunnel decides tunnel-vs-intercept once, at CONNECT, and a harness
    /// that keeps its connection alive would otherwise keep using the old
    /// disposition indefinitely.
    route_epoch: std::sync::atomic::AtomicU64,
    observed: AtomicBool,
    /// Fired once, the first time interception actually serves a request,
    /// so the session record can stop reporting the route as unproven.
    observed_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

/// Reasoning a target produced for one of its own tool-calling turns,
/// keyed by the tool-call id that turn carried (spec 0181).
///
/// Bounded rather than complete: an entry that has aged out degrades to an
/// empty echo, which the target accepts, instead of growing a session's
/// memory with every turn it has ever taken.
#[derive(Default)]
pub struct ReasoningMemo {
    entries: VecDeque<(String, String)>,
    bytes: usize,
}

impl ReasoningMemo {
    /// Reasoning kept per session before the oldest turns are forgotten.
    const MAX_BYTES: usize = 1 << 20;

    fn remember(&mut self, id: String, reasoning: String) {
        if id.is_empty() || self.entries.iter().any(|(known, _)| *known == id) {
            return;
        }
        self.bytes += id.len() + reasoning.len();
        self.entries.push_back((id, reasoning));
        while self.bytes > Self::MAX_BYTES {
            let Some((id, reasoning)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= id.len() + reasoning.len();
        }
    }

    fn recall(&self, id: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, reasoning)| reasoning.clone())
    }
}

impl SessionRouting {
    pub fn armed_route(&self) -> Option<ArmedRoute> {
        self.route.read().unwrap().clone()
    }

    /// Record the reasoning a target produced for the turn that made these
    /// tool calls. Each call id resolves to it, since the harness may
    /// replay the turn's calls in any order.
    pub fn remember_reasoning(&self, tool_call_ids: &[String], reasoning: &str) {
        let mut memo = self.reasoning.write().unwrap();
        for id in tool_call_ids {
            memo.remember(id.clone(), reasoning.to_string());
        }
    }

    /// The reasoning that accompanied `tool_call_id`, if still remembered.
    pub fn recall_reasoning(&self, tool_call_id: &str) -> Option<String> {
        self.reasoning.read().unwrap().recall(tool_call_id)
    }

    /// Whether a request's model names an internal seat the harness chose
    /// for itself rather than the model the session runs on (spec 0166).
    pub fn is_role_model(&self, model: &str) -> bool {
        self.role_models.contains(model)
    }

    pub fn route_epoch(&self) -> u64 {
        self.route_epoch.load(Ordering::SeqCst)
    }

    fn bump_route_epoch(&self) {
        self.route_epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub fn intercepts_host(&self, host: &str) -> bool {
        self.harness
            .intercept_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }

    pub fn catalog_enabled(&self) -> bool {
        self.catalog_enabled.load(Ordering::Relaxed)
    }

    /// Record that interception actually served a request. Until this
    /// flips, an armed route is unproven: the harness may resolve its
    /// endpoint through a channel that ignores our injection (spec 0115).
    pub fn mark_observed(&self) {
        if !self.observed.swap(true, Ordering::Relaxed) {
            let _ = self.observed_tx.send(self.session_id.clone());
        }
    }

    pub fn observed(&self) -> bool {
        self.observed.load(Ordering::Relaxed)
    }
}

/// The parts of `[router]` a config reload can replace (spec 0190).
///
/// Everything here is consulted while serving — resolving a route, publishing
/// a catalog — so swapping it is enough for the change to take effect. The
/// port deliberately is *not* here: see [`Router::apply_config`].
pub(crate) struct RouterSettings {
    enabled: bool,
    /// Native picker publication is automatic but independently
    /// configurable, so users may retain manual routing with an otherwise
    /// blind unarmed path.
    publish_models: bool,
    featured_models: Vec<String>,
    /// Route targets: the `[smith.models.*]` profiles, so an endpoint is
    /// declared once and reachable from both smith and a routed session.
    profiles: BTreeMap<String, ModelProfile>,
    /// `[router.oauth]` model overrides, keyed by provider name.
    oauth_models: BTreeMap<String, crate::config::OauthModels>,
    /// Whether route targets with a listing endpoint get their picker
    /// models fetched live (spec 0209). On by default; the off switch
    /// exists for locked-down networks where the fetch itself is unwanted.
    discover_models: bool,
}

pub struct Router {
    /// Config-derived state that can be exchanged while running. Read through
    /// [`Router::settings`], which clones the `Arc` out of the lock — a
    /// `std::sync::RwLockReadGuard` held across an await would make the
    /// enclosing future `!Send`.
    settings: RwLock<Arc<RouterSettings>>,
    /// Preferred port before bind. `0` asks the OS (tests / fallback).
    ///
    /// Not swappable, and deliberately so (spec 0183, spec 0190): harness
    /// processes are told this port once, in their environment at spawn, and
    /// have no way to be told it moved. Changing it in config is recorded as
    /// needing a restart rather than applied.
    preferred_port: u16,
    /// When true the user pinned `[router] port` — do not auto-fallback
    /// on EADDRINUSE and do not overwrite the persisted port file.
    port_pinned: bool,
    state_dir: PathBuf,
    /// Runtime dir of this home — owns `router.port` so the bound port
    /// is reclaimed on the next start of the same home.
    runtime_dir: PathBuf,
    ca: RwLock<Option<Arc<RouterCa>>>,
    upstream_proxy: Option<UpstreamProxy>,
    listening: AtomicBool,
    /// The port actually bound after [`Self::start`].
    bound_port: std::sync::atomic::AtomicU16,
    observed_tx: tokio::sync::mpsc::UnboundedSender<String>,
    observed_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
    sessions: RwLock<HashMap<String, Arc<SessionRouting>>>,
    /// Proxy credential → session. A `CONNECT` that presents no known
    /// credential is served as a plain tunnel: unattributable traffic gets
    /// the safe path, never a route.
    tokens: RwLock<HashMap<String, Arc<SessionRouting>>>,
    /// TTL cache of live-fetched model listings, keyed by endpoint
    /// (spec 0209). Refreshed on route-menu opens; consulted by
    /// [`Self::profile_model_list`].
    discovered: discovery::DiscoveryCache,
}

impl Router {
    pub fn new(
        state_dir: PathBuf,
        runtime_dir: PathBuf,
        cfg: &RouterConfig,
        profiles: BTreeMap<String, ModelProfile>,
    ) -> Arc<Self> {
        let upstream_proxy = std::env::var(PROXY_ENV)
            .ok()
            .or_else(|| std::env::var(PROXY_ENV_LOWER).ok())
            .as_deref()
            .and_then(UpstreamProxy::from_env_value);
        let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let paths = construct_protocol::paths::Paths {
            config_dir: runtime_dir.clone(),
            state_dir: state_dir.clone(),
            data_dir: state_dir.clone(),
            runtime_dir: runtime_dir.clone(),
        };
        let preferred_port = construct_protocol::paths::preferred_router_port(&paths, cfg.port);
        Arc::new(Self {
            settings: RwLock::new(Arc::new(RouterSettings {
                enabled: cfg.enabled,
                publish_models: cfg.publish_models,
                featured_models: cfg.featured_models.clone(),
                profiles,
                oauth_models: cfg.oauth.clone(),
                discover_models: cfg.discover_models,
            })),
            preferred_port,
            port_pinned: cfg.port.is_some(),
            state_dir,
            runtime_dir,
            ca: RwLock::new(None),
            upstream_proxy,
            listening: AtomicBool::new(false),
            bound_port: std::sync::atomic::AtomicU16::new(0),
            observed_tx,
            observed_rx: std::sync::Mutex::new(Some(observed_rx)),
            sessions: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
            discovered: discovery::DiscoveryCache::default(),
        })
    }

    /// The settings in force, cloned out of the lock before the caller does
    /// anything with them.
    pub(crate) fn settings(&self) -> Arc<RouterSettings> {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Adopt a reloaded `[router]` / `[smith.models.*]` (spec 0190), and
    /// report what could not be applied.
    ///
    /// Deliberately synchronous, and deliberately does not call
    /// [`Self::start`]: the caller does that afterwards, outside the lock.
    /// Awaiting the bind while holding the settings write lock would deadlock
    /// against any concurrent `session_env`.
    ///
    /// Two things are recorded rather than applied, both for the reason spec
    /// 0183 gives — a harness process is told the router's port once, in its
    /// environment at spawn, and cannot be told it moved:
    ///
    /// * A **port change** never moves the bound socket.
    /// * A **listening router is never switched off**, because withdrawing
    ///   the port would strand every session already dialing it. Switching a
    ///   *stopped* router on is applied, since nothing depends on its absence.
    pub fn apply_config(
        &self,
        cfg: &RouterConfig,
        profiles: BTreeMap<String, ModelProfile>,
    ) -> Vec<String> {
        let listening = self.listening.load(Ordering::SeqCst);
        let mut restart_required = Vec::new();

        if !cfg.enabled && listening {
            restart_required.push(
                "[router] enabled = false (the router is serving sessions that are still dialing it)"
                    .to_string(),
            );
        }
        // A pin that names a different port is a move. `Some(0)` is not a
        // port but a request for any port, which whatever we already bound
        // already satisfies — reporting it would make a restart look
        // necessary to reach a state we are in.
        let pinned_elsewhere = cfg
            .port
            .is_some_and(|port| port != 0 && port != self.port());
        // A pin appearing or disappearing is also a change, since it decides
        // whether a busy port may be fallen back from. Compared exactly as
        // `Router::new` derives `port_pinned`, so re-applying the running
        // config can never read as a toggle.
        let pin_toggled = cfg.port.is_some() != self.port_pinned;
        if pinned_elsewhere || pin_toggled {
            restart_required.push(format!("[router] port (still serving {})", self.port()));
        }

        let mut slot = self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Arc::new(RouterSettings {
            // Never `false` while listening — see above.
            enabled: cfg.enabled || listening,
            publish_models: cfg.publish_models,
            featured_models: cfg.featured_models.clone(),
            profiles,
            oauth_models: cfg.oauth.clone(),
            discover_models: cfg.discover_models,
        });
        restart_required
    }

    /// Whether the router should be serving but is not yet — the off→on
    /// transition a reload can apply. The caller follows a `true` with
    /// [`Self::start`].
    pub fn wants_start(&self) -> bool {
        self.settings().enabled && !self.listening.load(Ordering::SeqCst)
    }

    /// The port harness processes are told to use.
    pub fn port(&self) -> u16 {
        match self.bound_port.load(Ordering::SeqCst) {
            0 => self.preferred_port,
            p => p,
        }
    }

    fn router_port_file(&self) -> PathBuf {
        self.runtime_dir.join("router.port")
    }

    /// Bind the router's single loopback listener. Idempotent.
    ///
    /// Failing to bind is not fatal to the daemon: new sessions simply
    /// come up without routing transport. It *is* loud, because any
    /// session spawned by a previous daemon on this port can no longer
    /// reach us.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        // The `||` short-circuit is load-bearing, not incidental: when the
        // router is disabled the `swap` is never evaluated, so `listening`
        // stays false and the latch is left unarmed. That is what lets a
        // config reload switch a stopped router on (spec 0190) and have this
        // bind normally. Splitting it into two `if`s would silently break
        // hot-enable -- see `enabling_a_stopped_router_arms_the_listener`.
        if !self.settings().enabled || self.listening.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Whether this home already has a port of its own. A home that does
        // has live sessions dialing it; a first boot has none, so a fallback
        // costs nothing and becomes this home's port instead.
        let established = self.router_port_file().exists();
        let listener = match self.claim_listener().await {
            Ok(l) => l,
            Err(e) => {
                self.listening.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let bound = listener.local_addr()?.port();
        let claimed_preferred = self.preferred_port == 0 || bound == self.preferred_port;
        self.bound_port.store(bound, Ordering::SeqCst);
        // Persist only when the port was auto-selected for this home. A
        // pinned `[router] port = N` is minibuffer intent and must not be
        // overwritten by whatever happened to bind; `port = 0` is the
        // test "ask the OS" path and has no reclaim story.
        //
        // A fallback is explicitly NOT recorded for a home that already has a
        // port. Recording it would turn one busy moment into a permanent move
        // and give up on every session still dialing the old one — the exact
        // opposite of why the port is persisted at all.
        if records_port(self.port_pinned, bound, claimed_preferred, established) {
            if let Err(e) =
                construct_protocol::paths::write_persisted_port(&self.router_port_file(), bound)
            {
                tracing::warn!(
                    path = %self.router_port_file().display(),
                    error = %e,
                    "could not persist router port; next restart may pick a different one"
                );
            }
        }
        // Generate the CA up front: a harness reads its trust env once, at
        // spawn, so the file has to exist before the first session starts.
        self.ca()?;
        self.clone().serve_on(listener);
        tracing::info!(port = self.port(), "router listening");
        if !claimed_preferred {
            self.report_fallback(bound, established);
        }
        // Warm the discovered-model cache in the background (spec 0209) so
        // catalogs published to native harnesses at attach carry live
        // listings without waiting for a route-menu open. Off the start
        // path: startup must not block on vendor endpoints.
        let warm = self.clone();
        tokio::spawn(async move { warm.refresh_discovered_models().await });
        Ok(())
    }

    /// Accept and serve router connections on `listener` until it fails.
    ///
    /// Separate from [`Self::start`] because the router may end up serving on
    /// more than one port: a reclaimed preferred port is served alongside the
    /// fallback that stood in for it, so neither generation of sessions is cut
    /// off.
    fn serve_on(self: Arc<Self>, listener: TcpListener) {
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let router = self.clone();
                        tokio::spawn(async move {
                            if let Err(e) = proxy::serve(stream, router).await {
                                tracing::debug!(
                                    error = %format!("{e:#}"),
                                    "router connection ended"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "router listener accept failed");
                        return;
                    }
                }
            }
        });
    }

    /// Say what a fallback actually costs, and start trying to undo it.
    fn report_fallback(self: &Arc<Self>, bound: u16, established: bool) {
        if !established {
            tracing::warn!(
                preferred = self.preferred_port,
                bound,
                "another home holds the default router port; this home has taken its own"
            );
            return;
        }
        // Not a warning. Every session this home spawned before now is dialing
        // a port nothing is listening on, and will fail its next model call
        // with a connection error that says nothing about why.
        tracing::error!(
            preferred = self.preferred_port,
            bound,
            "router could not take this home's port; sessions spawned by a previous daemon \
             can only reach the router on the preferred port and cannot route until it is \
             reclaimed. Retrying in the background"
        );
        let router = self.clone();
        tokio::spawn(async move {
            router.reclaim_preferred().await;
        });
    }

    /// Keep trying to take the preferred port back, and serve on it when it
    /// comes free.
    ///
    /// Whatever holds the port usually lets go — a second daemon exits, a
    /// racing predecessor finishes shutting down. Until then every session
    /// from a previous daemon of this home is unroutable, so this does not
    /// give up: one bind attempt per interval is nothing next to leaving them
    /// stranded for the life of the daemon.
    async fn reclaim_preferred(self: Arc<Self>) {
        loop {
            tokio::time::sleep(PORT_RECLAIM_INTERVAL).await;
            match self.bind_preferred().await {
                Ok(listener) => {
                    // Serve on both: this port rescues the sessions already
                    // dialing it, and the fallback stays up for the ones
                    // spawned while it was all this home had.
                    self.clone().serve_on(listener);
                    // New sessions get the stable port from here on, so the
                    // home converges back to one port instead of accumulating.
                    self.bound_port.store(self.preferred_port, Ordering::SeqCst);
                    tracing::info!(
                        port = self.preferred_port,
                        "router reclaimed this home's port; sessions from a previous daemon \
                         can reach it again"
                    );
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => {
                    tracing::warn!(
                        preferred = self.preferred_port,
                        error = %e,
                        "router cannot reclaim this home's port; giving up"
                    );
                    return;
                }
            }
        }
    }

    async fn bind_preferred(&self) -> std::io::Result<TcpListener> {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.preferred_port));
        TcpListener::bind(addr).await
    }

    /// Prefer the configured / persisted port; on `EADDRINUSE` with no pin,
    /// fall back to an OS-assigned free port so a second home still boots.
    ///
    /// The preferred port is not given up on the first refusal. The likeliest
    /// reason it is busy during a restart is the daemon that just exec'd away
    /// still holding it, which clears in milliseconds — and the cost of not
    /// waiting is every live session of this home losing its route.
    async fn claim_listener(&self) -> Result<TcpListener> {
        let preferred = self.preferred_port;
        let deadline = tokio::time::Instant::now() + PORT_CLAIM_WINDOW;
        let last = loop {
            match self.bind_preferred().await {
                Ok(l) => return Ok(l),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    if tokio::time::Instant::now() >= deadline {
                        break e;
                    }
                    tokio::time::sleep(PORT_CLAIM_RETRY).await;
                }
                Err(e) => break e,
            }
        };
        if last.kind() != std::io::ErrorKind::AddrInUse || self.port_pinned {
            return Err(last).with_context(|| {
                format!(
                    "bind router listener on 127.0.0.1:{preferred}; sessions spawned by a \
                     previous daemon can only reach the router on that port"
                )
            });
        }
        let fallback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        TcpListener::bind(fallback)
            .await
            .with_context(|| {
                format!("bind router listener on 127.0.0.1:0 after 127.0.0.1:{preferred} was busy")
            })
    }

    /// Take the stream of sessions whose route has just been proven to be
    /// carrying traffic. Yields its receiver once; the session manager owns
    /// it and persists the flag.
    pub fn take_observed_stream(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<String>> {
        self.observed_rx.lock().unwrap().take()
    }

    /// Resolve a proxy credential to its session. `None` → tunnel.
    pub fn session_for_token(&self, token: &str) -> Option<Arc<SessionRouting>> {
        self.tokens.read().unwrap().get(token).cloned()
    }

    pub fn upstream_proxy(&self) -> Option<&UpstreamProxy> {
        self.upstream_proxy.as_ref()
    }

    /// Whether a session on `harness` would get routing transport.
    pub fn can_route_harness(&self, harness: &str) -> bool {
        self.settings().enabled && harness_routing(harness).is_some()
    }

    /// The CA, generated on first actual use so a disabled or unused
    /// router leaves no artifacts on disk.
    fn ca(&self) -> Result<Arc<RouterCa>> {
        if let Some(ca) = self.ca.read().unwrap().clone() {
            return Ok(ca);
        }
        let mut slot = self.ca.write().unwrap();
        if let Some(ca) = slot.clone() {
            return Ok(ca);
        }
        let ca = Arc::new(RouterCa::load_or_create(&self.state_dir.join("router"))?);
        *slot = Some(ca.clone());
        Ok(ca)
    }

    /// Register a session with the router and return the environment to
    /// inject into its harness process.
    ///
    /// `existing_token` re-adopts the credential a previously-spawned
    /// adapter was given. Adapters outlive the daemon
    /// (`Adapter::spawn_reconnectable`), so a live harness keeps
    /// presenting the credential it was handed at spawn; re-adopting it
    /// is what lets that session stay attributable across a restart.
    pub fn attach_session(
        &self,
        session_id: &str,
        harness: &str,
        existing_token: Option<String>,
    ) -> Result<HashMap<String, String>> {
        let routing = harness_routing(harness)
            .ok_or_else(|| anyhow!("harness {harness} is not route-capable"))?;
        let settings = self.settings();
        if !settings.enabled {
            return Err(anyhow!("router is disabled"));
        }
        if !self.listening.load(Ordering::SeqCst) {
            return Err(anyhow!("router listener is not bound"));
        }
        let ca = self.ca()?;
        let token = existing_token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let catalog_path = if settings.publish_models && harness == "codex" {
            match self.write_codex_catalog(session_id) {
                Ok(path) => Some(path),
                Err(error) => {
                    tracing::warn!(
                        session = %session_id,
                        error = %format!("{error:#}"),
                        "could not publish Construct routes to the Codex model picker"
                    );
                    None
                }
            }
        } else {
            None
        };
        let catalog_enabled = catalog_path.is_some()
            || (settings.publish_models
                && harness == "claude"
                && !self.published_models("claude").is_empty());
        // Failing to read the catalog leaves the set empty, which restores
        // the pre-0166 behavior of pinning every model. Say so rather than
        // letting a silent fallback re-hide the substitution.
        let role_models = match self.native_role_models(harness) {
            Ok(models) => models,
            Err(error) => {
                tracing::warn!(
                    session = %session_id,
                    %harness,
                    error = %format!("{error:#}"),
                    "could not read the harness catalog; internal-seat models will follow \
                     this session's pinned route"
                );
                HashSet::new()
            }
        };

        let ctx = Arc::new(SessionRouting {
            session_id: session_id.to_string(),
            harness_name: harness.to_string(),
            harness: routing,
            ca,
            upstream_proxy: self.upstream_proxy.clone(),
            catalog_enabled: AtomicBool::new(catalog_enabled),
            role_models,
            route: RwLock::new(None),
            reasoning: RwLock::new(ReasoningMemo::default()),
            route_epoch: std::sync::atomic::AtomicU64::new(0),
            observed: AtomicBool::new(false),
            observed_tx: self.observed_tx.clone(),
        });
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), ctx.clone());
        self.tokens.write().unwrap().insert(token.clone(), ctx);

        Ok(self.session_env(
            &token,
            harness,
            routing,
            catalog_path.as_deref(),
            catalog_enabled,
        ))
    }

    fn session_env(
        &self,
        token: &str,
        harness: &str,
        routing: HarnessRouting,
        catalog_path: Option<&std::path::Path>,
        catalog_enabled: bool,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();
        // The credential rides in the proxy URL's userinfo, which is how
        // proxy clients carry `Proxy-Authorization`. One listener with
        // per-session credentials beats one listener per session: only a
        // single port has to be reclaimed after a daemon restart.
        //
        // The password half is NOT optional, even though nothing reads it.
        // A username-only userinfo (`http://token@host`) is accepted by the
        // URL grammar but breaks at least one real harness: the CONNECT
        // still arrives, so the proxy looks healthy, and the client then
        // fails the request with a DNS-shaped error. Always emitting
        // `token:<filler>` keeps us on the form clients actually handle.
        let url = format!("http://{token}:{CREDENTIAL_FILLER}@127.0.0.1:{}", self.port());
        env.insert(PROXY_ENV.to_string(), url.clone());
        env.insert(PROXY_ENV_LOWER.to_string(), url);
        if let Some(path) = catalog_path {
            env.insert(
                construct_protocol::adapter::ENV_CODEX_MODEL_CATALOG.to_string(),
                path.to_string_lossy().to_string(),
            );
        }
        if harness == "claude" && catalog_enabled {
            env.insert(
                construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG.to_string(),
                format!("http://127.0.0.1:{}/__construct/claude", self.port()),
            );
            env.insert(
                construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG_TOKEN.to_string(),
                token.to_string(),
            );
            env.insert(
                construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG_DATA.to_string(),
                self.claude_models_response().to_string(),
            );
        }
        if let Ok(ca) = self.ca() {
            let path = ca.cert_path().to_string_lossy().to_string();
            // Trusting the router CA changes nothing until a route is
            // armed — no interception happens without one — but it has to
            // be present at spawn, because the harness reads it once.
            //
            // Which variable is per-harness and probe-established: some
            // harnesses take an ADDITIONAL ca here, others would treat the
            // same variable as a REPLACEMENT for the system roots and lose
            // every other endpoint. Only harnesses whose variable is
            // additive appear in `harness_routing` at all.
            for channel in routing.ca_env {
                let value = match channel.mode {
                    CaMode::Additive => Some(path.clone()),
                    // A replacing variable must never receive the bare CA:
                    // that would narrow the session's trust to our CA
                    // alone and break every endpoint but the routed one.
                    CaMode::Replacing => ca
                        .bundle_path()
                        .map(|p| p.to_string_lossy().to_string()),
                };
                if let Some(value) = value {
                    env.insert(channel.var.to_string(), value);
                }
            }
        }
        env
    }

    /// Recover the credential a session was previously issued, from its
    /// persisted start-params env.
    pub fn token_from_env(env: &HashMap<String, String>) -> Option<String> {
        let url = env.get(PROXY_ENV).or_else(|| env.get(PROXY_ENV_LOWER))?;
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or(url);
        let (userinfo, _) = rest.rsplit_once('@')?;
        let token = userinfo.split(':').next().unwrap_or(userinfo).trim();
        (!token.is_empty()).then(|| token.to_string())
    }

    pub fn detach_session(&self, session_id: &str) {
        if self.sessions.write().unwrap().remove(session_id).is_some() {
            self.tokens
                .write()
                .unwrap()
                .retain(|_, ctx| ctx.session_id != session_id);
        }
    }

    pub fn is_attached(&self, session_id: &str) -> bool {
        self.sessions.read().unwrap().contains_key(session_id)
    }

    /// Whether this session was launched with routes published into its
    /// harness's native model picker (spec 0157). Clients use this to show
    /// that a pin and a native selection can coexist (spec 0158).
    pub fn session_native_catalog(&self, session_id: &str) -> bool {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .is_some_and(|ctx| ctx.catalog_enabled())
    }

    /// Whether the router has actually served an intercepted request for
    /// this session — the difference between a route that is armed and one
    /// that is working (spec 0115).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn observed(&self, session_id: &str) -> bool {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .is_some_and(|s| s.observed())
    }

    /// Why a profile cannot serve as a route for `harness`, if it cannot.
    fn profile_blocker(&self, profile: &ModelProfile, harness: &str) -> Option<String> {
        let routing = harness_routing(harness)?;
        if routing.ca_env.is_empty() {
            return Some(format!("{harness} cannot trust the router CA"));
        }
        // A harness whose only CA channel replaces the system roots needs
        // the composed bundle. Without it, arming a route would cut the
        // session off from every endpoint it is not being routed to.
        let needs_bundle = routing
            .ca_env
            .iter()
            .all(|c| c.mode == CaMode::Replacing);
        if needs_bundle && self.ca().ok().and_then(|ca| ca.bundle_path()).is_none() {
            return Some(format!(
                "{harness} needs the system trust store composed with the router CA, \
                 and the platform trust store could not be read"
            ));
        }
        let Some(dialect) = provider_dialect(&profile.provider) else {
            return Some(format!(
                "no translator for provider \"{}\"",
                profile.provider
            ));
        };
        let Some(base_url) = profile.resolved_base_url() else {
            return Some(format!("provider \"{}\" has no base_url", profile.provider));
        };
        if matches!(
            profile.provider.to_ascii_lowercase().as_str(),
            "azure" | "azure-openai"
        ) && (base_url.contains('{') || base_url.contains('}'))
        {
            return Some(
                "azure-openai base_url contains an unresolved placeholder".to_string(),
            );
        }
        if profile.model.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Some("profile sets no model".to_string());
        }
        let _ = dialect;
        profile.resolve_api_key().err()
    }

    /// Resolve a subscription login into an armed route.
    fn resolve_oauth(
        &self,
        provider: OauthProvider,
        harness: &str,
        model: Option<&str>,
    ) -> Result<ArmedRoute> {
        let routing = harness_routing(harness)
            .ok_or_else(|| anyhow!("harness {harness} is not route-capable"))?;
        if let Some(blocker) = self.oauth_blocker(provider, harness) {
            return Err(anyhow!("route \"{}\": {}", provider.name(), blocker.reason));
        }
        let cred = oauth::read_credential(provider).map_err(|e| anyhow!(e))?;
        let model = model
            .map(str::to_string)
            .unwrap_or_else(|| self.oauth_model(provider));
        // Claude's catalog uses short aliases (`sonnet`, `opus`, `fable`) for
        // the picker; Anthropic rejects those labels. Expand before arming so
        // a native Codex/Claude pick of `construct-claude-oauth/sonnet` does
        // not 404 with `model: sonnet` against the Messages API.
        let model = if provider == OauthProvider::Claude {
            construct_protocol::slash::resolve_claude_oauth_model(&model)
        } else {
            model
        };
        Ok(ArmedRoute {
            name: provider.name().to_string(),
            endpoint: provider.endpoint().to_string(),
            base_url: provider.endpoint().to_string(),
            model: model.clone(),
            api_key: cred.access_token.clone(),
            // Every subscription backend here takes a bearer; none accepts
            // the Anthropic key header, even the Anthropic one.
            auth: TargetAuth::Bearer,
            system_prefix: oauth::required_system_prefix(provider),
            drop_params: oauth::unsupported_params(provider),
            extra_headers: oauth::extra_headers(provider, &cred)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            target_dialect: provider.dialect(),
            client_dialect: routing.dialect,
            effort: oauth::effort_support(provider, &model),
            reasoning_echo: false,
            pin_effort: None,
            client: reqwest::Client::new(),
        })
    }

    /// Models a subscription target offers, configured or built-in.
    fn oauth_model_list(&self, provider: OauthProvider) -> Vec<String> {
        let configured: Vec<String> = self
            .settings()
            .oauth_models
            .get(provider.name())
            .map(|m| m.to_vec())
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !m.trim().is_empty())
            .collect();
        if !configured.is_empty() {
            return configured;
        }
        provider.seed_models()
    }

    /// Models a profile offers: the one it declares first, then the rest of
    /// its provider's catalog. A profile pins one model, but the endpoint
    /// behind it serves the whole family, so the picker offers them.
    fn profile_model_list(&self, profile: &ModelProfile) -> Vec<String> {
        let mut models: Vec<String> = profile
            .model
            .clone()
            .into_iter()
            .filter(|m| !m.trim().is_empty())
            .collect();
        for model in construct_protocol::slash::models_for_provider(&profile.provider) {
            if !models.contains(&model) {
                models.push(model);
            }
        }
        // Live-discovered ids come after the declared model and the curated
        // catalog (spec 0209): curation keeps its known-good ordering at the
        // front, discovery contributes the endpoint's real tail. An empty or
        // failed discovery leaves exactly the curated behavior.
        if let Some(base_url) = profile.resolved_base_url() {
            let key = discovery::cache_key(&profile.provider, &base_url);
            if let Some(discovered) = self.discovered.get(&key) {
                for model in discovered.iter() {
                    if !models.contains(model) {
                        models.push(model.clone());
                    }
                }
            }
        }
        models
    }

    /// Refresh the discovered-model cache for every route target whose
    /// provider has a listing endpoint and whose credential resolves
    /// (spec 0209). Called on route-menu opens; fresh entries make this a
    /// no-op, so the common cost is zero and the worst case is one bounded
    /// wait. Best-effort by design — the caller proceeds with whatever the
    /// cache then holds.
    pub async fn refresh_discovered_models(&self) {
        let settings = self.settings();
        if !settings.discover_models {
            return;
        }
        let specs: Vec<discovery::FetchSpec> = settings
            .profiles
            .values()
            .filter_map(|profile| {
                let kind = discovery::list_kind(&profile.provider)?;
                let base_url = profile.resolved_base_url()?;
                let api_key = profile.resolve_api_key().ok()?;
                Some(discovery::FetchSpec {
                    key: discovery::cache_key(&profile.provider, &base_url),
                    kind,
                    base_url,
                    api_key,
                })
            })
            .collect();
        self.discovered.refresh(specs).await;
    }

    /// Model a subscription target sends when none is chosen explicitly.
    fn oauth_model(&self, provider: OauthProvider) -> String {
        self.oauth_model_list(provider)
            .first()
            .cloned()
            .unwrap_or_else(|| provider.default_model().to_string())
    }

    /// Why a subscription login cannot serve as a route for `harness`.
    /// Login problems keep the owning CLI's sign-in command attached so a
    /// client can offer to run it (spec 0117: the fix is always the owning
    /// tool; Construct only makes reaching for it cheaper).
    fn oauth_blocker(&self, provider: OauthProvider, harness: &str) -> Option<oauth::LoginBlocker> {
        let routing = harness_routing(harness)?;
        if routing.ca_env.is_empty() {
            return Some(oauth::LoginBlocker {
                reason: format!("{harness} cannot trust the router CA"),
                login_command: None,
            });
        }
        let needs_bundle = routing.ca_env.iter().all(|c| c.mode == CaMode::Replacing);
        if needs_bundle && self.ca().ok().and_then(|ca| ca.bundle_path()).is_none() {
            return Some(oauth::LoginBlocker {
                reason: format!(
                    "{harness} needs the system trust store composed with the router CA, \
                     and the platform trust store could not be read"
                ),
                login_command: None,
            });
        }
        oauth::check_login(provider).err()
    }

    fn resolve(&self, name: &str, harness: &str, model: Option<&str>) -> Result<ArmedRoute> {
        if let Some(provider) = OauthProvider::ALL.iter().find(|p| p.name() == name) {
            return self.resolve_oauth(*provider, harness, model);
        }
        let settings = self.settings();
        let profile = settings.profiles.get(name).ok_or_else(|| {
            anyhow!(
                "no route named \"{name}\": it is neither a [smith.models] profile \
                 nor a known subscription login"
            )
        })?;
        let routing = harness_routing(harness)
            .ok_or_else(|| anyhow!("harness {harness} is not route-capable"))?;
        if let Some(reason) = self.profile_blocker(profile, harness) {
            return Err(anyhow!("route \"{name}\": {reason}"));
        }
        let target_dialect = provider_dialect(&profile.provider)
            .ok_or_else(|| anyhow!("route \"{name}\": unsupported provider"))?;
        let base_url = profile
            .resolved_base_url()
            .ok_or_else(|| anyhow!("route \"{name}\": no base_url"))?;
        if matches!(
            profile.provider.to_ascii_lowercase().as_str(),
            "azure" | "azure-openai"
        ) && (base_url.contains('{') || base_url.contains('}'))
        {
            return Err(anyhow!(
                "route \"{name}\": azure-openai base_url contains an unresolved placeholder"
            ));
        }
        // Resolve the model once: the endpoint, the armed model, and the
        // effort scale must all describe the same model, since effort support
        // varies per model within a provider.
        let resolved_model = model
            .map(str::to_string)
            .or_else(|| profile.model.clone())
            .unwrap_or_default();
        Ok(ArmedRoute {
            name: name.to_string(),
            endpoint: translate::target_url(&base_url, target_dialect, &resolved_model, true),
            base_url,
            api_key: profile.resolve_api_key().map_err(|e| anyhow!(e))?,
            auth: match profile.provider.to_ascii_lowercase().as_str() {
                "anthropic" => TargetAuth::ApiKeyHeader,
                "gemini" | "google" => TargetAuth::GoogleApiKey,
                "azure" | "azure-openai" => TargetAuth::AzureApiKey,
                _ => TargetAuth::Bearer,
            },
            system_prefix: None,
            extra_headers: Vec::new(),
            drop_params: &[],
            target_dialect,
            client_dialect: routing.dialect,
            effort: profile_effort_support(&profile.provider, &resolved_model),
            reasoning_echo: provider_echoes_reasoning(&profile.provider),
            model: resolved_model,
            pin_effort: None,
            client: reqwest::Client::new(),
        })
    }

    /// Arm, change, or clear a session's route. Takes effect on the next
    /// request the harness makes; a request already in flight completes on
    /// the route it started with.
    pub fn set_route(
        &self,
        session_id: &str,
        harness: &str,
        name: Option<&str>,
        model: Option<&str>,
        origin_model: Option<String>,
        effort: Option<String>,
    ) -> Result<Option<SessionRoute>> {
        let ctx = self
            .sessions
            .read()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "session {session_id} has no routing transport; it was started \
                     before routing was enabled, or its harness is not route-capable"
                )
            })?;
        let Some(name) = name else {
            // Clearing returns the session to pass-through, which is
            // always reachable and therefore cannot fail (spec 0114).
            *ctx.route.write().unwrap() = None;
            ctx.bump_route_epoch();
            return Ok(None);
        };
        let mut armed = self.resolve(name, harness, model)?;
        if let Some(chosen) = effort {
            let levels = effort_levels_for_picker(armed.effort);
            if levels.is_empty() {
                // Target has no real scale: drop the pin effort rather than
                // advertise a choice the proxy cannot honor.
                armed.pin_effort = None;
            } else if levels.iter().any(|l| l == &chosen) {
                armed.pin_effort = Some(chosen);
            } else {
                return Err(anyhow!(
                    "route \"{name}\": effort \"{chosen}\" is not supported \
                     (supported: {})",
                    levels.join(", ")
                ));
            }
        }
        let summary = SessionRoute {
            name: armed.name.clone(),
            model: armed.model.clone(),
            effort: armed.pin_effort.clone(),
            origin_model,
            observed: ctx.observed(),
        };
        *ctx.route.write().unwrap() = Some(armed);
        ctx.bump_route_epoch();
        Ok(Some(summary))
    }

    /// Routes offered for a session's picker (spec 0115: render the
    /// reason, never an empty list).
    pub fn list_routes(
        &self,
        harness: &str,
        attached: bool,
        active: Option<String>,
        native_catalog: bool,
    ) -> RouterListRoutesResult {
        let routing = harness_routing(harness);
        let settings = self.settings();
        let unavailable_reason = if !settings.enabled {
            Some("routing is disabled; set [router] enabled = true in config.toml".to_string())
        } else if routing.is_none() {
            Some(format!(
                "{harness} is not route-capable: no probe has shown it honors {PROXY_ENV}"
            ))
        } else if !attached {
            Some(
                "this session started before routing was enabled; new sessions can be routed"
                    .to_string(),
            )
        } else {
            None
        };

        // Subscription logins first: they need no configuration, so a
        // machine with a login and no profiles still has something to pick.
        let mut routes: Vec<RouteOption> = OauthProvider::ALL
            .iter()
            .map(|p| {
                let blocker = routing.and_then(|_| self.oauth_blocker(*p, harness));
                let (unavailable_reason, login_command) = match blocker {
                    Some(b) => (Some(b.reason), b.login_command),
                    None => (None, None),
                };
                let models = self.oauth_model_list(*p);
                let efforts = efforts_for_models(models.iter().cloned(), |m| {
                    oauth::effort_support(*p, m)
                });
                RouteOption {
                    name: p.name().to_string(),
                    dialect: p.dialect().label().to_string(),
                    model: self.oauth_model(*p),
                    models,
                    efforts,
                    base_url: p.endpoint().to_string(),
                    unavailable_reason,
                    login_command,
                }
            })
            .collect();
        routes.extend(settings
            .profiles
            .iter()
            .map(|(name, profile)| {
                let models = self.profile_model_list(profile);
                let efforts = efforts_for_models(models.iter().cloned(), |m| {
                    profile_effort_support(&profile.provider, m)
                });
                RouteOption {
                    name: name.clone(),
                    dialect: provider_dialect(&profile.provider)
                        .map(|d| d.label().to_string())
                        .unwrap_or_else(|| profile.provider.clone()),
                    model: profile.model.clone().unwrap_or_default(),
                    models,
                    efforts,
                    base_url: profile.resolved_base_url().unwrap_or_default(),
                    unavailable_reason: routing
                        .and_then(|_| self.profile_blocker(profile, harness)),
                    // A profile's blocker is a missing key or dialect, never a
                    // login someone can click through.
                    login_command: None,
                }
            }));

        RouterListRoutesResult {
            routes,
            unavailable_reason,
            active,
            native_catalog,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Router config: the OS picks the port so concurrent tests don't
    /// collide on the fixed production one.
    fn cfg_with(enabled: bool) -> RouterConfig {
        RouterConfig {
            enabled,
            publish_models: false,
            featured_models: Vec::new(),
            port: Some(0),
            oauth: BTreeMap::new(),
            // Tests never fetch listings; discovery is exercised by its
            // own module tests.
            discover_models: false,
        }
    }

    fn profiles(entries: Vec<(&str, ModelProfile)>) -> BTreeMap<String, ModelProfile> {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    fn profile(provider: &str, key_env: Option<&str>) -> ModelProfile {
        ModelProfile {
            provider: provider.to_string(),
            base_url: Some("https://api.moonshot.ai/anthropic".to_string()),
            api_key_env: key_env.map(str::to_string),
            api_key: None,
            model: Some("kimi-k2.5".to_string()),
        }
    }

    /// Discovered ids append after the declared model and the curated
    /// catalog, deduped; an empty discovery leaves exactly the curated
    /// behavior (spec 0209).
    #[tokio::test]
    async fn discovered_models_append_after_curated_in_route_menu() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = profile("openrouter", None);
        p.base_url = Some("https://openrouter.ai/api/v1".to_string());
        p.model = Some("openrouter/auto".to_string());
        let r = Router::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            &cfg_with(true),
            profiles(vec![("openrouter", p.clone())]),
        );
        let curated: Vec<String> = r.profile_model_list(&p);
        assert!(curated.contains(&"openrouter/auto".to_string()));

        let key = discovery::cache_key("openrouter", "https://openrouter.ai/api/v1");
        r.discovered.insert(
            &key,
            vec![
                "stealth/newer-alpha".to_string(),
                // Already curated/declared: must not duplicate.
                "openrouter/auto".to_string(),
            ],
            true,
        );
        let merged = r.profile_model_list(&p);
        assert_eq!(&merged[..curated.len()], &curated[..], "curated order keeps the front");
        assert_eq!(merged.len(), curated.len() + 1);
        assert_eq!(merged.last().map(String::as_str), Some("stealth/newer-alpha"));
    }

    /// REGRESSION: pins the `||` short-circuit in `Router::start`. When the
    /// router is disabled, `enabled` is false so `listening.swap(true)` is
    /// never evaluated and the latch stays unarmed — which is the only reason
    /// a config reload can switch a stopped router on (spec 0190) without an
    /// unbind path. Rewriting that guard as two separate `if`s would leave
    /// the latch armed, make this `start()` a silent no-op, and break
    /// hot-enable with nothing else failing.
    #[tokio::test]
    async fn enabling_a_stopped_router_arms_the_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = Router::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            &cfg_with(false),
            BTreeMap::new(),
        );
        router.start().await.expect("start on a disabled router");
        assert!(
            !router.listening.load(Ordering::SeqCst),
            "a disabled router must not bind"
        );

        let restart_required = router.apply_config(&cfg_with(true), BTreeMap::new());
        assert!(
            restart_required.is_empty(),
            "switching a stopped router on needs no restart: {restart_required:?}"
        );
        assert!(router.wants_start(), "the router should now want to serve");
        router.start().await.expect("start after hot-enable");
        assert!(
            router.listening.load(Ordering::SeqCst),
            "hot-enable must actually bind the listener"
        );
        assert!(router.port() > 0, "a bound router reports its port");
    }

    /// Spec 0183: harness processes were told this port at spawn and cannot
    /// be told it moved, so withdrawing it is recorded, never performed.
    #[tokio::test]
    async fn disabling_a_listening_router_is_recorded_not_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = started(&dir, cfg_with(true)).await;
        let bound = router.port();

        let restart_required = router.apply_config(&cfg_with(false), BTreeMap::new());

        assert!(
            restart_required.iter().any(|r| r.contains("[router] enabled")),
            "disabling a serving router must be reported as needing a restart: {restart_required:?}"
        );
        assert!(
            router.can_route_harness("claude"),
            "the router keeps serving until the restart it asked for"
        );
        assert_eq!(router.port(), bound, "the bound port never moves");
    }

    #[tokio::test]
    async fn a_port_change_is_recorded_and_the_bound_port_stays_put() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = started(&dir, cfg_with(true)).await;
        let bound = router.port();

        let moved = RouterConfig {
            port: Some(bound.wrapping_add(1).max(1)),
            ..cfg_with(true)
        };
        let restart_required = router.apply_config(&moved, BTreeMap::new());

        assert!(
            restart_required.iter().any(|r| r.contains("[router] port")),
            "a port change must be reported: {restart_required:?}"
        );
        assert_eq!(
            router.port(),
            bound,
            "sessions are still dialing the port we bound"
        );
    }

    /// The other half of the split: what the router reads per request does
    /// apply, with no restart and no rebind.
    #[tokio::test]
    async fn swapped_profiles_are_visible_to_the_next_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = started(&dir, cfg_with(true)).await;
        assert!(
            router.resolve("kimi", "claude", None).is_err(),
            "the profile does not exist yet"
        );

        let restart_required = router.apply_config(
            &cfg_with(true),
            profiles(vec![("kimi", profile("anthropic", None))]),
        );

        assert!(
            restart_required.is_empty(),
            "a model profile applies live: {restart_required:?}"
        );
        assert!(
            router.settings().profiles.contains_key("kimi"),
            "the new profile is in force"
        );
    }

    #[tokio::test]
    async fn an_unchanged_router_config_asks_for_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let router = started(&dir, cfg_with(true)).await;
        // `cfg_with` pins port 0, which the OS resolved to the bound port;
        // re-applying the same config must not read as a move.
        let same = RouterConfig {
            port: Some(router.port()),
            ..cfg_with(true)
        };
        assert!(
            router.apply_config(&same, BTreeMap::new()).is_empty(),
            "re-applying the running config must not ask for a restart"
        );
    }

    #[test]
    fn codex_catalog_routes_both_native_openai_origins() {
        let routing = harness_routing("codex").expect("Codex routing");
        assert!(routing.intercept_hosts.contains(&"chatgpt.com"));
        assert!(routing.intercept_hosts.contains(&"api.openai.com"));
    }

    async fn started_with(
        dir: &tempfile::TempDir,
        cfg: RouterConfig,
        profiles: BTreeMap<String, ModelProfile>,
    ) -> Arc<Router> {
        let r = Router::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            &cfg,
            profiles,
        );
        r.start().await.unwrap();
        r
    }

    /// Look a route up by name. Index-based lookup is brittle now that
    /// subscription logins are listed alongside profiles.
    fn route_named<'a>(
        listed: &'a RouterListRoutesResult,
        name: &str,
    ) -> &'a construct_protocol::RouteOption {
        listed
            .routes
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no route named {name} in {:?}", listed.routes.iter().map(|r| &r.name).collect::<Vec<_>>()))
    }

    async fn started(dir: &tempfile::TempDir, cfg: RouterConfig) -> Arc<Router> {
        started_with(dir, cfg, BTreeMap::new()).await
    }

    /// Auto path, home that already has a port: bind a stand-in so the daemon
    /// still boots, but keep preferring the port this home's live sessions are
    /// dialing. Recording the stand-in instead would make one busy moment
    /// permanent and strand every one of them for good.
    #[tokio::test]
    async fn a_stand_in_port_does_not_replace_this_homes_own() {
        let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = holder.local_addr().unwrap().port();

        let dir = tempfile::tempdir().unwrap();
        // Seed the preferred port so start() tries the busy one first.
        std::fs::write(dir.path().join("router.port"), format!("{busy}\n")).unwrap();

        let cfg = RouterConfig {
            enabled: true,
            publish_models: false,
            featured_models: Vec::new(),
            port: None, // auto
            oauth: BTreeMap::new(),
            discover_models: false,
        };
        let r = started(&dir, cfg).await;
        let bound = r.port();
        assert_ne!(bound, busy, "must not steal the held port");
        assert_ne!(bound, 0);
        let persisted = std::fs::read_to_string(dir.path().join("router.port")).unwrap();
        assert_eq!(
            persisted.trim().parse::<u16>().unwrap(),
            busy,
            "the home keeps preferring the port its sessions were given"
        );
        // Keep `r` and `holder` alive so nothing else steals the ports mid-assert.
        let _keep = (r, holder);
    }

    /// The rule deciding whether a bound port becomes this home's own. Stated
    /// directly because the interesting case — a first boot whose default port
    /// is already taken — cannot be staged against a real socket without
    /// binding the machine's actual default port.
    #[test]
    fn only_a_port_this_home_can_keep_becomes_its_own() {
        // Got what it asked for: record it, whatever the history.
        assert!(records_port(false, 5000, true, true));
        assert!(records_port(false, 5000, true, false));

        // Fell back, and this home already has a port: keep preferring the
        // old one. Sessions from a previous daemon are still dialing it, and
        // recording the stand-in would abandon them permanently.
        assert!(!records_port(false, 5000, false, true));

        // Fell back on a first boot: nothing is dialing anything yet, so this
        // becomes the home's port. A second home on one machine lands here.
        assert!(records_port(false, 5000, false, false));

        // A pinned port is minibuffer intent; the file is never rewritten.
        assert!(!records_port(true, 5000, true, true));
        assert!(!records_port(true, 5000, false, false));

        // `port = 0` is the tests' ask-the-OS path and has no reclaim story.
        assert!(!records_port(false, 0, true, false));
    }

    /// A predecessor that has not let go yet must not cost this home its port.
    /// This is the restart race: the daemon exec's, the new image binds before
    /// the old socket is released, and giving up immediately would move the
    /// whole home off the port every live session is dialing.
    #[tokio::test]
    async fn the_preferred_port_is_waited_for_not_abandoned_on_first_refusal() {
        let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = holder.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("router.port"), format!("{busy}\n")).unwrap();

        // Let go while the router is still inside its claim window.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(holder);
        });

        let cfg = RouterConfig {
            enabled: true,
            publish_models: false,
            featured_models: Vec::new(),
            port: None,
            oauth: BTreeMap::new(),
            discover_models: false,
        };
        let r = started(&dir, cfg).await;
        assert_eq!(
            r.port(),
            busy,
            "the port came free during the window and was taken"
        );
    }

    /// A pinned `[router] port = N` must not auto-fallback or rewrite the
    /// port file when N is busy.
    #[tokio::test]
    async fn pinned_port_does_not_fallback_or_persist() {
        let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = holder.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("router.port"), "12345\n").unwrap();

        let cfg = RouterConfig {
            enabled: true,
            publish_models: false,
            featured_models: Vec::new(),
            port: Some(busy),
            oauth: BTreeMap::new(),
            discover_models: false,
        };
        let r = Router::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            &cfg,
            BTreeMap::new(),
        );
        let err = r.start().await.expect_err("pinned busy port must fail");
        assert!(
            format!("{err:#}").contains(&format!("127.0.0.1:{busy}")),
            "{err:#}"
        );
        // Prior persisted value left alone.
        let persisted = std::fs::read_to_string(dir.path().join("router.port")).unwrap();
        assert_eq!(persisted.trim(), "12345");
    }

    /// A disabled router must be inert: no listener, no CA, no env.
    #[tokio::test]
    async fn disabled_router_is_completely_inert() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(false)).await;
        assert!(!r.can_route_harness("claude"));
        assert!(r.attach_session("s1", "claude", None).is_err());
        assert!(
            !dir.path().join("router").exists(),
            "a disabled router must not generate a CA"
        );
    }

    #[tokio::test]
    async fn attach_injects_proxy_env_and_ca() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let proxy = env.get(PROXY_ENV).unwrap();
        assert!(
            proxy.starts_with("http://") && proxy.contains(&format!("@127.0.0.1:{}", r.port())),
            "{proxy}"
        );
        assert!(
            proxy_userinfo(proxy).contains(':'),
            "the credential must carry a password half; see \
             injected_credential_is_never_username_only: {proxy}"
        );
        assert_eq!(env.get(PROXY_ENV_LOWER), Some(proxy));
        assert!(env.contains_key("NODE_EXTRA_CA_CERTS"));

        // The credential round-trips out of the persisted env, which is
        // how a session stays attributable across a daemon restart.
        let token = Router::token_from_env(&env).unwrap();
        assert!(r.session_for_token(&token).is_some());
    }

    #[tokio::test]
    async fn published_model_id_resolves_to_its_request_scoped_route() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut request = vec![0u8; 16384];
            let count = socket.read(&mut request).await.unwrap();
            request.truncate(count);
            *seen_w.lock().unwrap() = String::from_utf8_lossy(&request).to_string();
            let body = serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "model-one",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "routed"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2
                }
            })
            .to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with(true);
        cfg.publish_models = true;
        let r = started_with(
            &dir,
            cfg,
            profiles(vec![(
                "fast",
                ModelProfile {
                    provider: "openai".to_string(),
                    base_url: Some(format!("http://127.0.0.1:{upstream_port}/v1")),
                    api_key_env: None,
                    api_key: Some("sk-test".to_string()),
                    model: Some("model-one".to_string()),
                },
            )]),
        )
        .await;
        let published = r
            .published_models("claude")
            .into_iter()
            .find(|model| model.route == "fast" && model.model == "model-one")
            .unwrap();
        assert!(published.id.starts_with("claude-construct-"));
        let resolved = r
            .resolve_published_model("claude", &published.id)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.name, "fast");
        assert_eq!(resolved.model, "model-one");
        assert_eq!(resolved.target_dialect, Dialect::OpenAiChat);
        let claude_response = r.claude_models_response();
        let picker_entry = claude_response["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == published.id)
            .unwrap();
        assert_eq!(
            picker_entry["display_name"],
            "model-one · fast · Construct"
        );
        assert_eq!(
            picker_entry["description"],
            "Routed by Construct through fast to model-one."
        );

        let env = r.attach_session("s-claude", "claude", None).unwrap();
        assert_eq!(
            env[construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG],
            format!("http://127.0.0.1:{}/__construct/claude", r.port())
        );
        assert!(r.sessions.read().unwrap()["s-claude"].catalog_enabled());

        let token = Router::token_from_env(&env).unwrap();
        assert_eq!(
            env[construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG_TOKEN],
            token
        );
        assert!(
            env[construct_protocol::adapter::ENV_CLAUDE_MODEL_CATALOG_DATA]
                .contains(&published.id)
        );
        let mut denied_gateway =
            tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
                .await
                .unwrap();
        denied_gateway
            .write_all(
                b"GET /__construct/claude/v1/models?limit=1000 HTTP/1.1\r\n\
                  Host: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut denied_response = String::new();
        denied_gateway
            .read_to_string(&mut denied_response)
            .await
            .unwrap();
        assert!(denied_response.starts_with("HTTP/1.1 403"), "{denied_response}");

        let mut gateway = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        gateway
            .write_all(
                format!(
                    "GET /__construct/claude/v1/models?limit=1000 HTTP/1.1\r\n\
                     Host: 127.0.0.1\r\nX-Construct-Session: {token}\r\n\
                     Connection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        gateway.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains(&published.id), "{response}");

        let request_body = serde_json::json!({
            "model": published.id,
            "max_tokens": 16,
            "stream": false,
            "messages": [{"role": "user", "content": "ping"}]
        })
        .to_string();
        let mut inference = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        inference
            .write_all(
                format!(
                    "POST /__construct/claude/v1/messages HTTP/1.1\r\n\
                     Host: 127.0.0.1\r\nX-Construct-Session: {token}\r\n\
                     Authorization: Bearer user-oauth-token\r\n\
                     Content-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{request_body}",
                    request_body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut inference_response = String::new();
        inference
            .read_to_string(&mut inference_response)
            .await
            .unwrap();
        assert!(
            inference_response.starts_with("HTTP/1.1 200"),
            "{inference_response}"
        );
        assert!(
            inference_response.contains("routed"),
            "{inference_response}"
        );

        let forwarded = seen.lock().unwrap().clone();
        assert!(forwarded.contains("\"model\":\"model-one\""), "{forwarded}");
        assert!(
            forwarded.contains("authorization: Bearer sk-test"),
            "{forwarded}"
        );
        assert!(!forwarded.contains(&published.id), "{forwarded}");
    }

    /// A live harness keeps presenting the credential it got at spawn, so
    /// re-adopting it must reattach the same session.
    #[tokio::test]
    async fn readopts_a_persisted_credential_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = started(&dir, cfg_with(true)).await;
        let env = first.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();

        let second = started(&dir, cfg_with(true)).await;
        assert!(second.session_for_token(&token).is_none());
        let env2 = second
            .attach_session("s1", "claude", Some(token.clone()))
            .unwrap();
        assert_eq!(Router::token_from_env(&env2).as_deref(), Some(token.as_str()));
        assert!(second.session_for_token(&token).is_some());
    }

    #[tokio::test]
    async fn unroutable_harness_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        // `shell` runs commands and makes no model calls at all; it is
        // the permanent example of a harness with nothing to route.
        assert!(r.attach_session("s1", "shell", None).is_err());
        let listed = r.list_routes("shell", false, None, false);
        assert!(listed
            .unavailable_reason
            .unwrap()
            .contains("not route-capable"));
    }

    #[tokio::test]
    async fn arming_requires_a_resolvable_key() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![("kimi", profile("anthropic", Some("NOT_SET_ANYWHERE")))]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let err = r.set_route("s1", "claude", Some("kimi"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("NOT_SET_ANYWHERE"), "{err}");
    }

    /// A Codex session learns its harness's internal seats at attach, so
    /// the proxy can tell "the model this session runs on" apart from "the
    /// model Codex picked for its own reviewer" without a disk read per
    /// request (spec 0166).
    #[tokio::test]
    async fn attach_captures_the_harnesss_hidden_models_as_role_models() {
        let _env = oauth::test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("models_cache.json"),
            serde_json::json!({
                "models": [
                    {"slug": "gpt-5.6-sol", "visibility": "list"},
                    {"slug": "codex-auto-review", "visibility": "hide"},
                ]
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);

        let r = started(&dir, cfg_with(true)).await;
        r.attach_session("s-codex", "codex", None).unwrap();
        let ctx = r.sessions.read().unwrap()["s-codex"].clone();

        assert!(
            ctx.is_role_model("codex-auto-review"),
            "the approval reviewer is a seat Codex fills for itself"
        );
        assert!(
            !ctx.is_role_model("gpt-5.6-sol"),
            "a picker-visible model is the session's own work and follows the pin"
        );

        std::env::remove_var("CODEX_HOME");
    }

    /// Only Codex publishes a catalog we can read seats from. Every other
    /// harness keeps today's behavior rather than guessing.
    #[tokio::test]
    async fn a_harness_without_a_readable_catalog_has_no_role_models() {
        let _env = oauth::test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        r.attach_session("s-claude", "claude", None).unwrap();
        let ctx = r.sessions.read().unwrap()["s-claude"].clone();
        assert!(!ctx.is_role_model("codex-auto-review"));
    }

    #[tokio::test]
    async fn arming_and_clearing_a_route() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_ROUTE_KEY", "sk-test");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "kimi",
                profile("anthropic", Some("CONSTRUCT_TEST_ROUTE_KEY")),
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let armed = r
            .set_route("s1", "claude", Some("kimi"), None, Some("claude-opus-5".into()), None)
            .unwrap()
            .unwrap();
        assert_eq!(armed.name, "kimi");
        assert_eq!(armed.model, "kimi-k2.5");
        assert_eq!(armed.origin_model.as_deref(), Some("claude-opus-5"));
        assert!(!armed.observed, "nothing has been proxied yet");

        // Clearing always succeeds (spec 0114).
        assert!(r.set_route("s1", "claude", None, None, None, None).unwrap().is_none());
    }

    /// A pin may record a reasoning-effort level when the target advertises
    /// a real scale (spec 0165). Unsupported scales drop the value; invalid
    /// levels are rejected.
    #[tokio::test]
    async fn pin_effort_is_recorded_when_the_target_supports_it() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_ROUTE_KEY", "sk-test");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![
                (
                    "gpt",
                    ModelProfile {
                        provider: "openai".to_string(),
                        base_url: Some("https://api.openai.com/v1".to_string()),
                        api_key_env: Some("CONSTRUCT_TEST_ROUTE_KEY".to_string()),
                        api_key: None,
                        model: Some("gpt-5".to_string()),
                    },
                ),
                (
                    "gemini-pro",
                    ModelProfile {
                        provider: "gemini".to_string(),
                        base_url: Some("https://generativelanguage.googleapis.com".to_string()),
                        api_key_env: Some("CONSTRUCT_TEST_ROUTE_KEY".to_string()),
                        api_key: None,
                        model: Some("gemini-2.5-pro".to_string()),
                    },
                ),
            ]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let gpt = route_named(&listed, "gpt");
        assert!(
            gpt.efforts
                .get("gpt-5")
                .is_some_and(|levels| levels == &["low", "medium", "high"]),
            "openai profiles expose a selectable scale: {:?}",
            gpt.efforts
        );
        let gemini = route_named(&listed, "gemini-pro");
        assert!(
            gemini.efforts.is_empty(),
            "unsupported targets omit the third column: {:?}",
            gemini.efforts
        );

        let armed = r
            .set_route(
                "s1",
                "claude",
                Some("gpt"),
                Some("gpt-5"),
                None,
                Some("high".into()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(armed.effort.as_deref(), Some("high"));
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        assert_eq!(
            ctx.armed_route().unwrap().pin_effort.as_deref(),
            Some("high")
        );

        let err = r
            .set_route(
                "s1",
                "claude",
                Some("gpt"),
                None,
                None,
                Some("ludicrous".into()),
            )
            .unwrap_err();
        assert!(err.to_string().contains("ludicrous"), "{err}");

        // Gemini has no selectable scale: effort is dropped, not rejected.
        let armed = r
            .set_route(
                "s1",
                "claude",
                Some("gemini-pro"),
                None,
                None,
                Some("high".into()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(armed.effort, None);
    }

    /// A provider with no translator is offered but not selectable, with
    /// the reason attached rather than hidden (spec 0115). Ollama's native
    /// API is the example: its `/api/chat` shape is nobody else's, and the
    /// way to route to it is to declare its OpenAI-compatible `/v1` endpoint
    /// as `provider = "openai"` instead.
    #[tokio::test]
    async fn untranslatable_providers_are_listed_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![("local", profile("ollama", None))]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None, false);
        assert!(listed.unavailable_reason.is_none());
        let reason = route_named(&listed, "local")
            .unavailable_reason
            .as_deref()
            .unwrap();
        assert!(reason.contains("no translator"), "{reason}");
        assert!(r.set_route("s1", "claude", Some("local"), None, None, None).is_err());
    }

    /// Meta's Model API is the Responses wire format, not a dialect of its
    /// own — smith's own Meta client posts to `/v1/responses` and reads the
    /// standard `response.*` events. Before this was mapped, Meta was the
    /// one built-in-eligible provider that would have been listed as
    /// permanently unusable.
    #[tokio::test]
    async fn meta_profiles_speak_responses_and_are_selectable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "meta",
                ModelProfile {
                    provider: "meta".to_string(),
                    base_url: None,
                    api_key_env: None,
                    api_key: Some("meta-key".to_string()),
                    model: Some("muse-spark-1.1".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let meta = route_named(&listed, "meta");
        assert_eq!(meta.unavailable_reason, None);
        assert_eq!(meta.dialect, "openai-responses");
        assert_eq!(meta.base_url, "https://api.meta.ai/v1");

        r.set_route("s1", "claude", Some("meta"), None, None, None)
            .unwrap();
        let armed = r.sessions.read().unwrap()["s1"].armed_route().unwrap();
        assert_eq!(armed.target_dialect, Dialect::OpenAiResponses);
        // Meta takes a bearer, not the Anthropic key header, even though the
        // harness on this side of the route is an Anthropic one.
        assert_eq!(armed.auth, TargetAuth::Bearer);
        assert_eq!(armed.endpoint, "https://api.meta.ai/v1/responses");
    }

    #[tokio::test]
    async fn gemini_profiles_are_selectable_with_native_url_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "gemini-pro",
                ModelProfile {
                    provider: "gemini".to_string(),
                    base_url: None,
                    api_key_env: None,
                    api_key: Some("google-key".to_string()),
                    model: Some("gemini-2.5-pro".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let gemini = route_named(&listed, "gemini-pro");
        assert_eq!(gemini.unavailable_reason, None);
        assert_eq!(gemini.dialect, "google-gemini");
        assert_eq!(
            gemini.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );

        r.set_route("s1", "claude", Some("gemini-pro"), None, None, None)
            .unwrap();
        let armed = r.sessions.read().unwrap()["s1"].armed_route().unwrap();
        assert_eq!(armed.auth, TargetAuth::GoogleApiKey);
        assert_eq!(
            armed.endpoint,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[tokio::test]
    async fn azure_profiles_use_responses_with_api_key_auth() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "azure",
                ModelProfile {
                    provider: "azure-openai".to_string(),
                    base_url: Some("https://resource.openai.azure.com/openai".to_string()),
                    api_key_env: None,
                    api_key: Some("azure-key".to_string()),
                    model: Some("deployment".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        r.set_route("s1", "claude", Some("azure"), None, None, None)
            .unwrap();
        let armed = r.sessions.read().unwrap()["s1"].armed_route().unwrap();
        assert_eq!(armed.target_dialect, Dialect::OpenAiResponses);
        assert_eq!(armed.auth, TargetAuth::AzureApiKey);
        assert_eq!(
            armed.endpoint,
            "https://resource.openai.azure.com/openai/v1/responses"
        );
    }

    /// An OpenAI-dialect profile IS selectable from an Anthropic-dialect
    /// harness — that is what the translator is for (spec 0116).
    #[tokio::test]
    async fn openai_profiles_are_selectable_and_marked_as_translating() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_OPENAI_KEY", "sk-openai");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "gpt",
                ModelProfile {
                    provider: "openai".to_string(),
                    base_url: None,
                    api_key_env: Some("CONSTRUCT_TEST_OPENAI_KEY".to_string()),
                    api_key: None,
                    model: Some("gpt-5.5".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let gpt = route_named(&listed, "gpt");
        assert_eq!(gpt.unavailable_reason, None);
        assert_eq!(gpt.dialect, "openai-chat");
        // The provider's own default endpoint is resolved, exactly as
        // smith would resolve it.
        assert_eq!(gpt.base_url, "https://api.openai.com/v1");

        let armed = r
            .set_route("s1", "claude", Some("gpt"), None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(armed.model, "gpt-5.5");
        let route = r.session_for_token("nope").is_none();
        assert!(route);
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        let armed = ctx.armed_route().unwrap();
        assert!(
            armed.translates(),
            "an openai target from an anthropic harness must translate"
        );
    }

    /// A session remembers the reasoning its target produced, and forgets
    /// the oldest of it rather than growing without bound. A forgotten
    /// turn is a miss, which the caller answers with an empty echo — never
    /// with another turn's reasoning (spec 0181).
    #[test]
    fn a_session_remembers_reasoning_per_tool_call_and_then_forgets_it() {
        let mut memo = ReasoningMemo::default();
        memo.remember("call_1".into(), "first".into());
        memo.remember("call_2".into(), "second".into());
        memo.remember("call_1".into(), "a later turn must not overwrite".into());
        assert_eq!(memo.recall("call_1").as_deref(), Some("first"));
        assert_eq!(memo.recall("call_2").as_deref(), Some("second"));
        assert_eq!(memo.recall("call_unknown"), None);

        memo.remember(String::new(), "an id-less turn is not addressable".into());
        assert_eq!(memo.recall(""), None);

        let bulk = "x".repeat(64 * 1024);
        for i in 0..24 {
            memo.remember(format!("call_bulk_{i}"), bulk.clone());
        }
        assert_eq!(
            memo.recall("call_1"),
            None,
            "the oldest reasoning is dropped once the budget is spent"
        );
        assert_eq!(memo.recall("call_bulk_23").as_deref(), Some(bulk.as_str()));
        assert!(memo.bytes <= ReasoningMemo::MAX_BYTES);
    }

    /// DeepSeek refuses a replayed tool-calling turn whose reasoning is
    /// missing, so its arms must rebuild the request even when the harness
    /// already speaks the target's dialect (spec 0181).
    #[test]
    fn a_deepseek_arm_carries_reasoning_and_always_rebuilds() {
        assert!(provider_echoes_reasoning("deepseek"));
        assert!(provider_echoes_reasoning("DeepSeek"));
        assert!(!provider_echoes_reasoning("openai"));
        assert!(!provider_echoes_reasoning("anthropic"));

        let mut route = ArmedRoute {
            name: "deepseek".into(),
            endpoint: "https://api.deepseek.com/v1/chat/completions".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-v4-flash".into(),
            api_key: "sk-test".into(),
            auth: TargetAuth::Bearer,
            system_prefix: None,
            extra_headers: Vec::new(),
            drop_params: &[],
            target_dialect: Dialect::OpenAiChat,
            client_dialect: Dialect::OpenAiChat,
            effort: EffortSupport::DeepSeek,
            reasoning_echo: provider_echoes_reasoning("deepseek"),
            pin_effort: None,
            client: reqwest::Client::new(),
        };
        assert!(!route.translates(), "same dialect on both ends");
        assert!(
            route.needs_rebuild(),
            "byte-forwarding would ship the turn without its reasoning"
        );
        route.reasoning_echo = false;
        assert!(!route.needs_rebuild());
    }

    /// The built-in DeepSeek target (spec 0179) reaches the router as an
    /// ordinary profile, so it must be selectable from an Anthropic harness
    /// and carry every model the shared catalog lists for the provider —
    /// not just the one the built-in names as its default.
    #[tokio::test]
    async fn deepseek_target_is_selectable_and_offers_the_catalog_models() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_DEEPSEEK_KEY", "sk-deepseek");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "deepseek",
                ModelProfile {
                    provider: "deepseek".to_string(),
                    base_url: None,
                    api_key_env: Some("CONSTRUCT_TEST_DEEPSEEK_KEY".to_string()),
                    api_key: None,
                    model: Some("deepseek-v4-pro".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let deepseek = route_named(&listed, "deepseek");
        assert_eq!(deepseek.unavailable_reason, None);
        assert_eq!(deepseek.dialect, "openai-chat");
        assert_eq!(deepseek.base_url, "https://api.deepseek.com/v1");
        assert!(
            deepseek.models.iter().any(|m| m == "deepseek-v4-flash"),
            "the shared catalog's models must reach the picker: {:?}",
            deepseek.models
        );

        let armed = r
            .set_route("s1", "claude", Some("deepseek"), None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(armed.model, "deepseek-v4-pro");
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        assert!(
            ctx.armed_route().unwrap().reasoning_echo,
            "a DeepSeek arm must carry reasoning back (spec 0181)"
        );
        assert!(
            ctx.armed_route().unwrap().translates(),
            "a chat-completions target from an anthropic harness must translate"
        );
    }

    /// Effort support is per model, not per provider. Measured against the
    /// live API: on flash, `low` / `high` / `max` produce cleanly separated
    /// reasoning lengths; on pro every level floors to the same default, so
    /// pro advertises no scale rather than a control that does nothing.
    #[test]
    fn deepseek_effort_scale_is_flash_only() {
        assert_eq!(
            profile_effort_support("deepseek", "deepseek-v4-flash"),
            EffortSupport::DeepSeek
        );
        assert_eq!(
            effort_level_set(EffortSupport::DeepSeek),
            ("high", &["low", "high", "max"][..]),
            "DeepSeek's own default effort is high"
        );
        assert_eq!(
            profile_effort_support("deepseek", "deepseek-v4-pro"),
            EffortSupport::Unsupported
        );
        assert!(
            effort_levels_for_picker(profile_effort_support("deepseek", "deepseek-v4-pro"))
                .is_empty(),
            "pro must not render a picker column it cannot honor"
        );
        // Other providers keep provider-wide behavior regardless of model.
        assert_eq!(
            profile_effort_support("openai", "gpt-5"),
            EffortSupport::Verbatim
        );
    }

    /// `Unsupported` strips the effort before it reaches the wire, so pro
    /// never receives a level the picker did not offer. `DeepSeek` must not
    /// be caught by that arm — flash's levels have to survive to the emitter.
    #[test]
    fn deepseek_flash_effort_is_not_stripped_as_unsupported() {
        assert_ne!(EffortSupport::DeepSeek, EffortSupport::Unsupported);
    }

    /// The per-model split has to reach the picker, not just the helper.
    #[tokio::test]
    async fn deepseek_route_offers_effort_on_flash_but_not_pro() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_DEEPSEEK_EFFORT_KEY", "sk-deepseek");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "deepseek",
                ModelProfile {
                    provider: "deepseek".to_string(),
                    base_url: None,
                    api_key_env: Some("CONSTRUCT_TEST_DEEPSEEK_EFFORT_KEY".to_string()),
                    api_key: None,
                    model: Some("deepseek-v4-pro".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();

        let listed = r.list_routes("claude", true, None, false);
        let deepseek = route_named(&listed, "deepseek");
        assert_eq!(
            deepseek.efforts.get("deepseek-v4-flash").map(Vec::as_slice),
            Some(&["low".to_string(), "high".to_string(), "max".to_string()][..]),
            "flash grades effort: {:?}",
            deepseek.efforts
        );
        assert!(
            !deepseek.efforts.contains_key("deepseek-v4-pro"),
            "pro floors every level, so it offers none: {:?}",
            deepseek.efforts
        );

        // An armed route carries the effort scale of the model it resolved.
        let armed = r
            .set_route(
                "s1",
                "claude",
                Some("deepseek"),
                Some("deepseek-v4-flash"),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(armed.model, "deepseek-v4-flash");
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        assert_eq!(
            ctx.armed_route().unwrap().effort,
            EffortSupport::DeepSeek,
            "arming flash must carry flash's scale, not the provider default"
        );
    }

    /// A profile that sets no model cannot be a route: there would be
    /// nothing to substitute.
    #[tokio::test]
    async fn profiles_without_a_model_are_not_selectable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "bare",
                ModelProfile {
                    provider: "anthropic".to_string(),
                    base_url: None,
                    api_key_env: None,
                    api_key: Some("k".to_string()),
                    model: None,
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None, false);
        assert!(route_named(&listed, "bare")
            .unavailable_reason
            .as_deref()
            .unwrap()
            .contains("no model"));
    }

    #[tokio::test]
    async fn a_session_without_transport_cannot_be_armed() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![("kimi", profile("anthropic", None))]),
        )
        .await;
        let err = r
            .set_route("never-attached", "claude", Some("kimi"), None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("no routing transport"), "{err}");
    }

    #[test]
    fn captures_an_existing_proxy_for_chaining() {
        let p = UpstreamProxy::from_env_value("http://proxy.corp:3128").unwrap();
        assert_eq!(p.host, "proxy.corp");
        assert_eq!(p.port, 3128);
        assert!(p.authorization.is_none());

        let p = UpstreamProxy::from_env_value("http://user:pw@proxy.corp:8080").unwrap();
        assert_eq!(p.host, "proxy.corp");
        assert!(p.authorization.unwrap().starts_with("Basic "));

        assert!(UpstreamProxy::from_env_value("").is_none());
        assert!(UpstreamProxy::from_env_value("   ").is_none());
    }

    fn proxy_userinfo(url: &str) -> String {
        let rest = url.strip_prefix("http://").unwrap_or(url);
        rest.rsplit_once('@').map(|(u, _)| u.to_string()).unwrap_or_default()
    }

    /// REGRESSION: the injected proxy URL must never be `token@host`.
    ///
    /// A username with no password is legal in the URL grammar and looks
    /// fine end to end — the `CONNECT` still arrives and the proxy tunnels
    /// it — but the claude harness then fails every request with a
    /// DNS-shaped error (`ENOTFOUND`). Verified by hand against both this
    /// router and an unrelated third-party proxy: `token@host` fails,
    /// `token:anything@host` succeeds. Nothing reads the password; it
    /// exists only to keep us on the form clients handle.
    #[tokio::test]
    async fn injected_credential_is_never_username_only() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        for var in [PROXY_ENV, PROXY_ENV_LOWER] {
            let userinfo = proxy_userinfo(env.get(var).unwrap());
            let (user, pass) = userinfo.split_once(':').unwrap_or((&userinfo, ""));
            assert!(!user.is_empty(), "{var}: no session credential");
            assert!(
                !pass.is_empty(),
                "{var}: username-only userinfo breaks the claude harness"
            );
        }
        // The session must still be resolvable from the credential.
        let token = Router::token_from_env(&env).unwrap();
        assert!(r.session_for_token(&token).is_some());
    }

    #[test]
    fn reads_back_a_persisted_credential() {
        let env: HashMap<String, String> = [(
            PROXY_ENV.to_string(),
            "http://abc123@127.0.0.1:8917".to_string(),
        )]
        .into_iter()
        .collect();
        assert_eq!(Router::token_from_env(&env).as_deref(), Some("abc123"));
        assert_eq!(Router::token_from_env(&HashMap::new()), None);

        // The password half is ignored on the way back in.
        let with_pass: HashMap<String, String> = [(
            PROXY_ENV.to_string(),
            "http://abc123:construct@127.0.0.1:8917".to_string(),
        )]
        .into_iter()
        .collect();
        assert_eq!(Router::token_from_env(&with_pass).as_deref(), Some("abc123"));
    }

    /// The whole safety claim in one test: an unrouted session's bytes
    /// reach the destination it named, unmodified, and the router never
    /// terminates TLS on that path.
    #[tokio::test]
    async fn passes_through_unrouted_traffic_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();

        // Stand in for the origin: echo whatever arrives.
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = vec![0u8; 11];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
        client
            .write_all(
                format!(
                    "CONNECT 127.0.0.1:{} HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n",
                    origin_addr.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut head = [0u8; 39];
        client.read_exact(&mut head).await.unwrap();
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&head)
        );

        client.write_all(b"hello world").await.unwrap();
        let mut echoed = [0u8; 11];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello world");
    }

    /// Unattributable traffic takes the safe path, never a route.
    #[tokio::test]
    async fn tunnels_connections_with_no_credential() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;

        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            s.write_all(b"ok").await.unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", origin_addr.port()).as_bytes(),
            )
            .await
            .unwrap();
        let mut head = [0u8; 39];
        client.read_exact(&mut head).await.unwrap();
        let mut body = [0u8; 2];
        client.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"ok");
    }

    /// End-to-end interception: an armed route redirects the request to the
    /// route's endpoint, substitutes the model, swaps in the route's
    /// credential, drops the client's, and streams the response back.
    #[tokio::test]
    async fn intercepts_and_redirects_an_armed_route() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Stand-in upstream endpoint the route points at.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            buf.truncate(n);
            *seen_w.lock().unwrap() = buf;
            s.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 17\r\n\r\n{\"ok\":\"routed\"}\r\n",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "kimi",
                ModelProfile {
                    provider: "anthropic".to_string(),
                    base_url: Some(format!("http://127.0.0.1:{upstream_port}")),
                    api_key_env: None,
                    api_key: Some("sk-route".to_string()),
                    model: Some("kimi-k2.5".to_string()),
                },
            )]),
        )
        .await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        r.set_route("s1", "claude", Some("kimi"), None, Some("claude-opus-5".into()), None)
            .unwrap();

        // Client: CONNECT through the router, then TLS with the router CA
        // as its only trust root — exactly what the injected
        // NODE_EXTRA_CA_CERTS gives the harness.
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
        sock.write_all(
            format!(
                "CONNECT api.anthropic.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut ack = [0u8; 39];
        sock.read_exact(&mut ack).await.unwrap();

        let ca_pem = std::fs::read_to_string(env.get("NODE_EXTRA_CA_CERTS").unwrap()).unwrap();
        let ca_der = pem_to_der(&ca_pem);
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(ca_der))
            .unwrap();
        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let domain =
            tokio_rustls::rustls::pki_types::ServerName::try_from("api.anthropic.com").unwrap();
        let mut tls = connector.connect(domain, sock).await.unwrap();

        let body = br#"{"model":"claude-opus-5","max_tokens":8}"#;
        tls.write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: api.anthropic.com\r\nx-api-key: sk-users-own-key\r\nanthropic-version: 2023-06-01\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        tls.write_all(body).await.unwrap();
        tls.flush().await.unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("routed"), "{response}");

        let forwarded = String::from_utf8_lossy(&seen.lock().unwrap().clone()).to_string();
        assert!(
            forwarded.contains("\"model\":\"kimi-k2.5\""),
            "route model must be substituted: {forwarded}"
        );
        assert!(
            forwarded.contains("sk-route"),
            "route credential must be used: {forwarded}"
        );
        assert!(
            !forwarded.contains("sk-users-own-key"),
            "the client's own credential must never reach another vendor: {forwarded}"
        );
        assert!(
            forwarded.contains("anthropic-version"),
            "dialect headers must be preserved: {forwarded}"
        );
        assert!(r.observed("s1"), "a served request marks the route observed");
    }

    /// End-to-end cross-dialect routing (spec 0116): an Anthropic-dialect
    /// client reaches an OpenAI-dialect endpoint. The request arrives
    /// translated and bearer-authenticated; the response comes back as a
    /// well-formed Anthropic SSE stream.
    #[tokio::test]
    async fn translates_a_streaming_turn_between_dialects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).await.unwrap();
            buf.truncate(n);
            *seen_w.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            let sse = concat!(
                "data: {\"id\":\"cmpl_1\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hi \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"there\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
                    sse.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "gpt",
                ModelProfile {
                    provider: "openai".to_string(),
                    base_url: Some(format!("http://127.0.0.1:{upstream_port}")),
                    api_key_env: None,
                    api_key: Some("sk-openai".to_string()),
                    model: Some("gpt-5.5".to_string()),
                },
            )]),
        )
        .await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        r.set_route("s1", "claude", Some("gpt"), None, Some("claude-opus-5".into()), None)
            .unwrap();

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:"));
        sock.write_all(
            format!(
                "CONNECT api.anthropic.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut ack = [0u8; 39];
        sock.read_exact(&mut ack).await.unwrap();

        let ca_pem = std::fs::read_to_string(env.get("NODE_EXTRA_CA_CERTS").unwrap()).unwrap();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                pem_to_der(&ca_pem),
            ))
            .unwrap();
        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let domain =
            tokio_rustls::rustls::pki_types::ServerName::try_from("api.anthropic.com").unwrap();
        let mut tls = connector.connect(domain, sock).await.unwrap();

        let body = br#"{"model":"claude-opus-5","max_tokens":32,"stream":true,"system":"be terse","messages":[{"role":"user","content":"hi"}]}"#;
        tls.write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: api.anthropic.com\r\nx-api-key: sk-users-own-key\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        tls.write_all(body).await.unwrap();
        tls.flush().await.unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response).to_string();

        // The client sees its own dialect, correctly bracketed.
        assert!(response.contains("event: message_start"), "{response}");
        assert!(response.contains("event: content_block_start"), "{response}");
        assert!(
            response.contains("\"text\":\"Hi \""),
            "text deltas must survive translation: {response}"
        );
        assert!(response.contains("event: message_stop"), "{response}");
        assert!(
            response.contains("gpt-5.5"),
            "the stream reports the model that actually ran: {response}"
        );

        // The upstream saw OpenAI shape, bearer auth, and never the
        // client's own credential.
        let forwarded = seen.lock().unwrap().clone();
        assert!(
            forwarded.starts_with("POST /chat/completions"),
            "must hit the OpenAI endpoint: {forwarded}"
        );
        assert!(
            forwarded.contains("authorization: Bearer sk-openai"),
            "must use bearer auth: {forwarded}"
        );
        assert!(
            !forwarded.contains("sk-users-own-key"),
            "the client's credential must never reach another vendor: {forwarded}"
        );
        assert!(
            forwarded.contains("\"role\":\"system\""),
            "the anthropic system prompt must become a system message: {forwarded}"
        );
        assert!(
            forwarded.contains("\"model\":\"gpt-5.5\""),
            "the route's model must be substituted: {forwarded}"
        );
    }

    /// Gemini is a full target dialect, not an OpenAI-compatible alias:
    /// its model/RPC live in the URL, its key uses x-goog-api-key, and its
    /// contents/candidates stream is translated in both directions.
    #[tokio::test]
    async fn translates_an_anthropic_turn_onto_gemini() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).await.unwrap();
            buf.truncate(n);
            *seen_w.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            let sse = concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"gem\"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ini\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2}}\n\n",
            );
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
                    sse.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "gemini",
                ModelProfile {
                    provider: "gemini".to_string(),
                    base_url: Some(format!("http://127.0.0.1:{upstream_port}/v1beta")),
                    api_key_env: None,
                    api_key: Some("google-route-key".to_string()),
                    model: Some("gemini-2.5-pro".to_string()),
                },
            )]),
        )
        .await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        r.set_route(
            "s1",
            "claude",
            Some("gemini"),
            None,
            Some("claude-opus-5".into()),
            None,
        )
        .unwrap();

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:construct"));
        sock.write_all(
            format!(
                "CONNECT api.anthropic.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut ack = [0u8; 39];
        sock.read_exact(&mut ack).await.unwrap();

        let ca_pem = std::fs::read_to_string(env.get("NODE_EXTRA_CA_CERTS").unwrap()).unwrap();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                pem_to_der(&ca_pem),
            ))
            .unwrap();
        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let domain =
            tokio_rustls::rustls::pki_types::ServerName::try_from("api.anthropic.com").unwrap();
        let mut tls = connector.connect(domain, sock).await.unwrap();

        let body = br#"{"model":"claude-opus-5","max_tokens":32,"stream":true,"system":"be terse","messages":[{"role":"user","content":"hi"}]}"#;
        tls.write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: api.anthropic.com\r\nx-api-key: users-own-key\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        tls.write_all(body).await.unwrap();
        tls.flush().await.unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("\"text\":\"gem\""), "{response}");
        assert!(response.contains("\"text\":\"ini\""), "{response}");
        assert!(response.contains("event: message_stop"), "{response}");

        let forwarded = seen.lock().unwrap().clone();
        assert!(
            forwarded.starts_with(
                "POST /v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
            ),
            "{forwarded}"
        );
        assert!(
            forwarded.contains("x-goog-api-key: google-route-key"),
            "{forwarded}"
        );
        assert!(!forwarded.contains("users-own-key"), "{forwarded}");
        assert!(forwarded.contains("\"systemInstruction\""), "{forwarded}");
        assert!(forwarded.contains("\"contents\""), "{forwarded}");
        assert!(forwarded.contains("\"maxOutputTokens\":32"), "{forwarded}");
    }


    /// End-to-end request-scoped selection from a native picker: a
    /// Responses-speaking session carries a published Construct model id,
    /// the router resolves that id without a session pin, translates onto
    /// the target, and re-encodes the reply as a Responses event stream.
    #[tokio::test]
    async fn a_published_model_selects_its_route_per_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).await.unwrap();
            buf.truncate(n);
            *seen_w.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            let sse = concat!(
                "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"po\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ng\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
                    sse.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with(true);
        cfg.publish_models = true;
        let r = started_with(
            &dir,
            cfg,
            profiles(vec![(
                "gpt",
                ModelProfile {
                    provider: "openai".to_string(),
                    base_url: Some(format!("http://127.0.0.1:{upstream_port}")),
                    api_key_env: None,
                    api_key: Some("sk-openai".to_string()),
                    model: Some("gpt-5.5".to_string()),
                },
            )]),
        )
        .await;
        let env = r.attach_session("s1", "pi", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        ctx.catalog_enabled.store(true, Ordering::Relaxed);
        assert!(
            env.contains_key("NODE_EXTRA_CA_CERTS"),
            "pi takes its CA through the Node variable"
        );
        assert!(
            ctx.armed_route().is_none(),
            "the model id, not a session pin, must select the route"
        );
        let published = r
            .published_models("pi")
            .into_iter()
            .find(|model| model.route == "gpt")
            .unwrap();

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:construct"));
        sock.write_all(
            format!("CONNECT chatgpt.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut ack = [0u8; 39];
        sock.read_exact(&mut ack).await.unwrap();

        let ca_pem = std::fs::read_to_string(env.get("NODE_EXTRA_CA_CERTS").unwrap()).unwrap();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                pem_to_der(&ca_pem),
            ))
            .unwrap();
        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("chatgpt.com").unwrap();
        let mut tls = connector.connect(domain, sock).await.unwrap();

        // A real Responses request, in the shape captured from pi.
        let body = serde_json::to_vec(&serde_json::json!({
            "model": published.id,
            "stream": true,
            "store": false,
            "instructions": "be terse",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "say pong"}]
            }],
            "tools": [{
                "type": "function",
                "name": "read",
                "description": "read a file",
                "parameters": {"type": "object", "properties": {}}
            }]
        }))
        .unwrap();
        tls.write_all(
            format!(
                "POST /backend-api/codex/responses HTTP/1.1\r\nhost: chatgpt.com\r\nauthorization: Bearer users-own-token\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        tls.write_all(&body).await.unwrap();
        tls.flush().await.unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response).to_string();

        // The harness gets Responses framing back — items and parts, not
        // bare deltas.
        for expected in [
            "event: response.created",
            "event: response.output_item.added",
            "event: response.content_part.added",
            "event: response.output_text.delta",
            "event: response.output_item.done",
            "event: response.completed",
        ] {
            assert!(response.contains(expected), "missing {expected}: {response}");
        }
        assert!(response.contains("\"delta\":\"po\""), "{response}");

        // The target saw Chat Completions, translated from Responses.
        let forwarded = seen.lock().unwrap().clone();
        assert!(
            forwarded.starts_with("POST /chat/completions"),
            "must hit the chat endpoint: {forwarded}"
        );
        assert!(
            forwarded.contains("authorization: Bearer sk-openai"),
            "route credential must be used: {forwarded}"
        );
        assert!(
            !forwarded.contains("users-own-token"),
            "the harness credential must never reach another vendor: {forwarded}"
        );
        assert!(
            forwarded.contains("\"role\":\"system\"") && forwarded.contains("be terse"),
            "instructions must become a system message: {forwarded}"
        );
        assert!(
            forwarded.contains("\"model\":\"gpt-5.5\""),
            "route model must be substituted: {forwarded}"
        );
        assert!(
            forwarded.contains("\"function\""),
            "flat Responses tools must become nested Chat functions: {forwarded}"
        );
    }

    /// A harness whose CA variable REPLACES the system roots must be handed
    /// the composed bundle, never the bare CA — the bare CA would leave the
    /// session trusting nothing but us.
    #[tokio::test]
    async fn replacing_ca_channels_receive_the_composed_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;

        let codex = r.attach_session("s-codex", "codex", None).unwrap();
        let bundle = codex.get("SSL_CERT_FILE").expect("codex needs a CA file");
        let text = std::fs::read_to_string(bundle).unwrap();
        let count = text.matches("-----BEGIN CERTIFICATE-----").count();
        assert!(
            count > 1,
            "the bundle must carry the system roots as well as our CA, saw {count}"
        );
        let ours = std::fs::read_to_string(dir.path().join("router/ca.pem")).unwrap();
        assert!(
            text.contains(ours.trim()),
            "the bundle must contain the router CA"
        );

        // An additive channel keeps getting the bare CA.
        let claude = r.attach_session("s-claude", "claude", None).unwrap();
        let bare = claude.get("NODE_EXTRA_CA_CERTS").unwrap();
        assert!(bare.ends_with("ca.pem"), "{bare}");
        assert_ne!(bare, bundle);
    }

    /// grok and codex both use SSL_CERT_FILE, but only one of them
    /// additively. Mixing them up would break every other endpoint the
    /// session talks to, so the modes must stay distinct.
    #[tokio::test]
    async fn the_same_variable_gets_different_files_per_harness() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        let grok = r.attach_session("s-grok", "grok", None).unwrap();
        let codex = r.attach_session("s-codex", "codex", None).unwrap();
        let grok_ca = grok.get("SSL_CERT_FILE").unwrap();
        let codex_ca = codex.get("SSL_CERT_FILE").unwrap();
        assert!(grok_ca.ends_with("ca.pem"), "grok is additive: {grok_ca}");
        assert!(
            codex_ca.ends_with("ca-bundle.pem"),
            "codex is replacing: {codex_ca}"
        );
    }

    /// End-to-end for a subscription login as a target (spec 0117): a
    /// Responses-speaking harness routed onto the Claude subscription. The
    /// request must arrive as Anthropic Messages, bearer-authenticated,
    /// carrying the beta header and the identity line that backend
    /// requires — and never the harness's own credential.
    #[tokio::test]
    async fn routes_a_harness_onto_a_subscription_login() {
        let _env = oauth::test_env_guard();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        // A live Claude login, read-only.
        let creds = dir.path().join("creds.json");
        let future = (chrono::Utc::now().timestamp() + 3600) * 1000;
        std::fs::write(
            &creds,
            serde_json::json!({"claudeAiOauth":{"accessToken":"sub-token","expiresAt":future}})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE", &creds);

        let r = started(&dir, cfg_with(true)).await;
        let listed = r.list_routes("pi", true, None, false);
        let option = route_named(&listed, "claude-oauth");
        assert_eq!(
            option.unavailable_reason, None,
            "a live login must be selectable with no config at all"
        );
        assert_eq!(option.dialect, "anthropic");

        let env = r.attach_session("s1", "pi", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        let armed = r
            .set_route("s1", "pi", Some("claude-oauth"), None, Some("gpt-5.6-sol".into()), None)
            .unwrap()
            .unwrap();
        assert_eq!(armed.name, "claude-oauth");

        // Point the armed route's endpoint at a local stand-in.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_w = seen.clone();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).await.unwrap();
            buf.truncate(n);
            *seen_w.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            let sse = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"pong\"}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            );
            s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
                    sse.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
        });
        {
            let ctx = r.sessions.read().unwrap()["s1"].clone();
            let mut slot = ctx.route.write().unwrap();
            let route = slot.as_mut().unwrap();
            route.endpoint = format!("http://127.0.0.1:{upstream_port}/v1/messages");
            assert_eq!(route.auth, TargetAuth::Bearer);
            assert!(route.needs_rebuild(), "a login target always rebuilds");
        }

        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", r.port()))
            .await
            .unwrap();
        use base64::Engine;
        let cred = base64::engine::general_purpose::STANDARD.encode(format!("{token}:construct"));
        sock.write_all(
            format!("CONNECT chatgpt.com:443 HTTP/1.1\r\nProxy-Authorization: Basic {cred}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut ack = [0u8; 39];
        sock.read_exact(&mut ack).await.unwrap();

        let ca_pem = std::fs::read_to_string(env.get("NODE_EXTRA_CA_CERTS").unwrap()).unwrap();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots
            .add(tokio_rustls::rustls::pki_types::CertificateDer::from(
                pem_to_der(&ca_pem),
            ))
            .unwrap();
        let provider = tokio_rustls::rustls::crypto::ring::default_provider();
        let client_cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let domain = tokio_rustls::rustls::pki_types::ServerName::try_from("chatgpt.com").unwrap();
        let mut tls = connector.connect(domain, sock).await.unwrap();

        let body = br#"{"model":"gpt-5.6-sol","stream":true,"instructions":"be terse","input":[{"role":"user","content":[{"type":"input_text","text":"say pong"}]}]}"#;
        tls.write_all(
            format!(
                "POST /backend-api/codex/responses HTTP/1.1\r\nhost: chatgpt.com\r\nauthorization: Bearer harness-own-token\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        tls.write_all(body).await.unwrap();
        tls.flush().await.unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response).to_string();
        // The harness still receives its own dialect.
        assert!(response.contains("event: response.created"), "{response}");
        assert!(response.contains("\"delta\":\"pong\""), "{response}");
        assert!(response.contains("event: response.completed"), "{response}");

        let forwarded = seen.lock().unwrap().clone();
        assert!(
            forwarded.contains("authorization: Bearer sub-token"),
            "the subscription token must be used as a bearer: {forwarded}"
        );
        assert!(
            !forwarded.contains("harness-own-token"),
            "the harness's own credential must never be forwarded: {forwarded}"
        );
        assert!(
            forwarded.contains("anthropic-beta: oauth-2025-04-20"),
            "the backend requires its beta header: {forwarded}"
        );
        assert!(
            forwarded.contains("Claude Code"),
            "the backend requires its identity line in the system prompt: {forwarded}"
        );
        assert!(
            forwarded.contains("be terse"),
            "the harness's own instructions must survive the prefix: {forwarded}"
        );
        assert!(
            forwarded.contains("\"messages\""),
            "Responses must be translated to Anthropic Messages: {forwarded}"
        );
        std::env::remove_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE");
    }

    /// REGRESSION: claude routed to codex-oauth failed with
    /// `400 Unsupported parameter: max_output_tokens`.
    ///
    /// Anthropic requires `max_tokens`, so every claude request carries one
    /// and every translation to Responses produced the parameter the Codex
    /// backend refuses. The dialect defines it; that target does not accept
    /// it, and those are different questions.
    #[tokio::test]
    async fn a_targets_rejected_parameters_are_stripped() {
        let _env = oauth::test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let codex_home = dir.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::json!({"tokens":{"access_token":"at","account_id":"acct"}}).to_string(),
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);

        let r = started(&dir, cfg_with(true)).await;
        r.attach_session("s1", "claude", None).unwrap();
        r.set_route("s1", "claude", Some("codex-oauth"), None, None, None)
            .unwrap();
        let ctx = r.sessions.read().unwrap()["s1"].clone();
        let route = ctx.armed_route().unwrap();
        assert!(route.drop_params.contains(&"max_output_tokens"));

        // A claude request always carries max_tokens; after translation the
        // parameter must be gone, while everything else survives.
        let claude_request = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 32000,
            "temperature": 0.7,
            "system": "be terse",
            "messages": [{"role":"user","content":"hi"}],
        });
        let canon = translate::parse_request(Dialect::AnthropicMessages, &claude_request);
        let mut emitted = translate::emit_request(route.target_dialect, &canon, &route.model);
        assert!(
            emitted.get("max_output_tokens").is_some(),
            "the dialect does define it — the target is what refuses it"
        );
        if let Some(obj) = emitted.as_object_mut() {
            for key in route.drop_params {
                obj.remove(*key);
            }
        }
        assert!(emitted.get("max_output_tokens").is_none(), "{emitted}");
        assert!(emitted.get("temperature").is_none(), "{emitted}");
        assert_eq!(emitted["instructions"], "be terse");
        assert_eq!(emitted["model"], route.model);
        assert!(emitted["input"].as_array().is_some_and(|i| !i.is_empty()));

        // grok-oauth speaks the same dialect and accepts both, so the
        // stripping must not be applied dialect-wide.
        std::env::remove_var("CODEX_HOME");
    }

    /// A profile pins one model but its endpoint serves the family, so the
    /// picker offers the provider's catalog behind the declared default.
    #[tokio::test]
    async fn a_profile_offers_its_declared_model_first_then_its_provider_catalog() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CONSTRUCT_TEST_CATALOG_KEY", "sk");
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![(
                "gpt",
                ModelProfile {
                    provider: "openai".to_string(),
                    base_url: None,
                    api_key_env: Some("CONSTRUCT_TEST_CATALOG_KEY".to_string()),
                    api_key: None,
                    model: Some("gpt-5.5".to_string()),
                },
            )]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None, false);
        let gpt = route_named(&listed, "gpt");
        assert_eq!(
            gpt.models.first().map(String::as_str),
            Some("gpt-5.5"),
            "the declared model leads"
        );
        assert!(
            gpt.models.len() > 1,
            "the provider catalog follows it: {:?}",
            gpt.models
        );
        assert_eq!(
            gpt.models.iter().filter(|m| *m == "gpt-5.5").count(),
            1,
            "no duplicate when the declared model is also in the catalog"
        );
    }

    /// A subscription target's models likewise come from the shared
    /// catalog, so the picker and smith's `/model` agree.
    #[tokio::test]
    async fn subscription_targets_offer_the_shared_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true)).await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None, false);
        let claude = route_named(&listed, "claude-oauth");
        assert!(
            claude.models.iter().any(|m| m == "sonnet"),
            "the subscription path takes short aliases: {:?}",
            claude.models
        );
        assert_eq!(claude.model, claude.models[0]);
    }

    /// Catalog entries keep the short alias (`sonnet`) for display, but the
    /// armed route must expand it before the request leaves Construct —
    /// Anthropic returns `404 model: sonnet` for the label itself.
    #[tokio::test]
    async fn claude_oauth_published_aliases_expand_to_concrete_models() {
        let _env = oauth::test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("creds.json");
        let future = (chrono::Utc::now().timestamp() + 3600) * 1000;
        std::fs::write(
            &creds,
            serde_json::json!({"claudeAiOauth":{"accessToken":"sub-token","expiresAt":future}})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE", &creds);

        let mut cfg = cfg_with(true);
        cfg.publish_models = true;
        let r = started(&dir, cfg).await;
        r.attach_session("s1", "codex", None).unwrap();

        let cases = [
            ("sonnet", "claude-sonnet-4-6"),
            ("opus", "claude-opus-4-8"),
            ("fable", "claude-fable-5"),
        ];
        for (alias, concrete) in cases {
            let published = r
                .published_models("codex")
                .into_iter()
                .find(|model| model.route == "claude-oauth" && model.model == alias)
                .unwrap_or_else(|| panic!("missing published claude-oauth/{alias}"));
            assert_eq!(
                published.model, alias,
                "picker label stays short; expansion is resolve-time only"
            );
            let resolved = r
                .resolve_published_model("codex", &published.id)
                .unwrap()
                .expect("published id must resolve");
            assert_eq!(resolved.name, "claude-oauth");
            assert_eq!(
                resolved.model, concrete,
                "alias {alias} must expand before the Messages request"
            );
        }

        // Concrete ids already in the catalog pass through untouched.
        let concrete = r
            .resolve("claude-oauth", "codex", Some("claude-sonnet-4-6"))
            .unwrap();
        assert_eq!(concrete.model, "claude-sonnet-4-6");

        std::env::remove_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE");
    }

    /// Minimal PEM → DER, so the test trusts exactly the CA the router
    /// handed the session.
    fn pem_to_der(pem: &str) -> Vec<u8> {
        use base64::Engine;
        let body: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap()
    }
}
