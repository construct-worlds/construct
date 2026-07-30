//! Model-route transport (spec 0113 / 0110 / 0111).
//!
//! The router owns one loopback `CONNECT` listener and a per-session
//! routing table the proxy consults on every connection. Changing a
//! session's route mutates that table — the harness process is never
//! restarted, never signalled, and never learns of the change.
//!
//! Connections are attributed to sessions by the proxy credential the
//! harness presents, not by which port it dialed. One fixed port is far
//! easier to reclaim after a daemon restart than a port per session, and
//! reclaiming it is mandatory: harness processes outlive the daemon and
//! keep dialing the port they were given at spawn.
//!
//! With `[router] enabled = false` (the default) nothing here runs: no
//! listener is bound, no CA is generated, and no session's environment is
//! touched.

pub mod ca;
pub mod catalog;
pub mod oauth;
pub mod proxy;
pub mod translate;

use std::collections::{BTreeMap, HashMap};
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
        // Grok is served by smith's OpenAI client and speaks the same wire
        // format.
        "openai" | "grok" => Some(Dialect::OpenAiChat),
        // Azure's current v1 API uses Responses on the wire; its adapter
        // difference is the `api-key` header, not a separate JSON dialect.
        "openai-responses" | "azure" | "azure-openai" => {
            Some(Dialect::OpenAiResponses)
        }
        _ => None,
    }
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
    route: RwLock<Option<ArmedRoute>>,
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

impl SessionRouting {
    pub fn armed_route(&self) -> Option<ArmedRoute> {
        self.route.read().unwrap().clone()
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

pub struct Router {
    enabled: bool,
    /// Native picker publication is automatic but independently
    /// configurable, so users may retain manual routing with an otherwise
    /// blind unarmed path.
    publish_models: bool,
    featured_models: Vec<String>,
    port: u16,
    /// Route targets: the `[smith.models.*]` profiles, so an endpoint is
    /// declared once and reachable from both smith and a routed session.
    profiles: BTreeMap<String, ModelProfile>,
    /// `[router.oauth]` model overrides, keyed by provider name.
    oauth_models: BTreeMap<String, crate::config::OauthModels>,
    state_dir: PathBuf,
    ca: RwLock<Option<Arc<RouterCa>>>,
    upstream_proxy: Option<UpstreamProxy>,
    listening: AtomicBool,
    /// The port actually bound. Equals `port` in every real configuration;
    /// differs only when `port = 0` asks the OS to choose (tests).
    bound_port: std::sync::atomic::AtomicU16,
    observed_tx: tokio::sync::mpsc::UnboundedSender<String>,
    observed_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>,
    sessions: RwLock<HashMap<String, Arc<SessionRouting>>>,
    /// Proxy credential → session. A `CONNECT` that presents no known
    /// credential is served as a plain tunnel: unattributable traffic gets
    /// the safe path, never a route.
    tokens: RwLock<HashMap<String, Arc<SessionRouting>>>,
}

impl Router {
    pub fn new(
        state_dir: PathBuf,
        cfg: &RouterConfig,
        profiles: BTreeMap<String, ModelProfile>,
    ) -> Arc<Self> {
        let upstream_proxy = std::env::var(PROXY_ENV)
            .ok()
            .or_else(|| std::env::var(PROXY_ENV_LOWER).ok())
            .as_deref()
            .and_then(UpstreamProxy::from_env_value);
        let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self {
            enabled: cfg.enabled,
            publish_models: cfg.publish_models,
            featured_models: cfg.featured_models.clone(),
            port: cfg.port,
            profiles,
            oauth_models: cfg.oauth.clone(),
            state_dir,
            ca: RwLock::new(None),
            upstream_proxy,
            listening: AtomicBool::new(false),
            bound_port: std::sync::atomic::AtomicU16::new(0),
            observed_tx,
            observed_rx: std::sync::Mutex::new(Some(observed_rx)),
            sessions: RwLock::new(HashMap::new()),
            tokens: RwLock::new(HashMap::new()),
        })
    }

    /// The port harness processes are told to use.
    pub fn port(&self) -> u16 {
        match self.bound_port.load(Ordering::SeqCst) {
            0 => self.port,
            p => p,
        }
    }

    /// Bind the router's single loopback listener. Idempotent.
    ///
    /// Failing to bind is not fatal to the daemon: new sessions simply
    /// come up without routing transport. It *is* loud, because any
    /// session spawned by a previous daemon on this port can no longer
    /// reach us.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if !self.enabled || self.listening.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port));
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                self.listening.store(false, Ordering::SeqCst);
                return Err(e).with_context(|| {
                    format!(
                        "bind router listener on 127.0.0.1:{}; sessions spawned by a \
                         previous daemon can only reach the router on that port",
                        self.port
                    )
                });
            }
        };
        self.bound_port
            .store(listener.local_addr()?.port(), Ordering::SeqCst);
        // Generate the CA up front: a harness reads its trust env once, at
        // spawn, so the file has to exist before the first session starts.
        self.ca()?;
        let router = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let router = router.clone();
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
        tracing::info!(port = self.port(), "router listening");
        Ok(())
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
        self.enabled && harness_routing(harness).is_some()
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
        if !self.enabled {
            return Err(anyhow!("router is disabled"));
        }
        if !self.listening.load(Ordering::SeqCst) {
            return Err(anyhow!("router listener is not bound"));
        }
        let ca = self.ca()?;
        let token = existing_token.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let catalog_path = if self.publish_models && harness == "codex" {
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
            || (self.publish_models
                && harness == "claude"
                && !self.published_models("claude").is_empty());

        let ctx = Arc::new(SessionRouting {
            session_id: session_id.to_string(),
            harness_name: harness.to_string(),
            harness: routing,
            ca,
            upstream_proxy: self.upstream_proxy.clone(),
            catalog_enabled: AtomicBool::new(catalog_enabled),
            route: RwLock::new(None),
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
        Ok(ArmedRoute {
            name: provider.name().to_string(),
            endpoint: provider.endpoint().to_string(),
            base_url: provider.endpoint().to_string(),
            model: model
                .map(str::to_string)
                .unwrap_or_else(|| self.oauth_model(provider)),
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
            client: reqwest::Client::new(),
        })
    }

    /// Models a subscription target offers, configured or built-in.
    fn oauth_model_list(&self, provider: OauthProvider) -> Vec<String> {
        let configured: Vec<String> = self
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
        models
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
        let profile = self.profiles.get(name).ok_or_else(|| {
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
        Ok(ArmedRoute {
            name: name.to_string(),
            endpoint: translate::target_url(
                &base_url,
                target_dialect,
                model.or(profile.model.as_deref()).unwrap_or_default(),
                true,
            ),
            base_url,
            model: model
                .map(str::to_string)
                .or_else(|| profile.model.clone())
                .unwrap_or_default(),
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
        let armed = self.resolve(name, harness, model)?;
        let summary = SessionRoute {
            name: armed.name.clone(),
            model: armed.model.clone(),
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
        let unavailable_reason = if !self.enabled {
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
                RouteOption {
                    name: p.name().to_string(),
                    dialect: p.dialect().label().to_string(),
                    model: self.oauth_model(*p),
                    models: self.oauth_model_list(*p),
                    base_url: p.endpoint().to_string(),
                    unavailable_reason,
                    login_command,
                }
            })
            .collect();
        routes.extend(self
            .profiles
            .iter()
            .map(|(name, profile)| RouteOption {
                name: name.clone(),
                dialect: provider_dialect(&profile.provider)
                    .map(|d| d.label().to_string())
                    .unwrap_or_else(|| profile.provider.clone()),
                model: profile.model.clone().unwrap_or_default(),
                models: self.profile_model_list(profile),
                base_url: profile.resolved_base_url().unwrap_or_default(),
                unavailable_reason: routing
                    .and_then(|_| self.profile_blocker(profile, harness)),
                // A profile's blocker is a missing key or dialect, never a
                // login someone can click through.
                login_command: None,
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
            port: 0,
            oauth: BTreeMap::new(),
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
        let r = Router::new(dir.path().to_path_buf(), &cfg, profiles);
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
        let err = r.set_route("s1", "claude", Some("kimi"), None, None).unwrap_err();
        assert!(err.to_string().contains("NOT_SET_ANYWHERE"), "{err}");
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
            .set_route("s1", "claude", Some("kimi"), None, Some("claude-opus-5".into()))
            .unwrap()
            .unwrap();
        assert_eq!(armed.name, "kimi");
        assert_eq!(armed.model, "kimi-k2.5");
        assert_eq!(armed.origin_model.as_deref(), Some("claude-opus-5"));
        assert!(!armed.observed, "nothing has been proxied yet");

        // Clearing always succeeds (spec 0114).
        assert!(r.set_route("s1", "claude", None, None, None).unwrap().is_none());
    }

    /// A provider with no translator is offered but not selectable, with
    /// the reason attached rather than hidden (spec 0115).
    #[tokio::test]
    async fn untranslatable_providers_are_listed_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![("meta-model", profile("meta", Some("X")))]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None, false);
        assert!(listed.unavailable_reason.is_none());
        let reason = route_named(&listed, "meta-model")
            .unavailable_reason
            .as_deref()
            .unwrap();
        assert!(reason.contains("no translator"), "{reason}");
        assert!(r.set_route("s1", "claude", Some("meta-model"), None, None).is_err());
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

        r.set_route("s1", "claude", Some("gemini-pro"), None, None)
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
        r.set_route("s1", "claude", Some("azure"), None, None)
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
            .set_route("s1", "claude", Some("gpt"), None, None)
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
            .set_route("never-attached", "claude", Some("kimi"), None, None)
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
        r.set_route("s1", "claude", Some("kimi"), None, Some("claude-opus-5".into()))
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
        r.set_route("s1", "claude", Some("gpt"), None, Some("claude-opus-5".into()))
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
            .set_route("s1", "pi", Some("claude-oauth"), None, Some("gpt-5.6-sol".into()))
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
        r.set_route("s1", "claude", Some("codex-oauth"), None, None)
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
