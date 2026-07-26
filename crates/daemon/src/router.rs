//! Model-route transport (specs 0109 / 0110 / 0111).
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

/// Env var carrying the proxy the harness should use. This is the only
/// channel Construct injects for transport; the harness's own endpoint
/// configuration is never displaced (spec 0109).
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
/// response stream are rebuilt (spec 0112).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Anthropic Messages (`/v1/messages`).
    AnthropicMessages,
    /// OpenAI Chat Completions (`/chat/completions`).
    OpenAiChat,
    /// OpenAI Responses (`/responses`). A *client* dialect only: four
    /// harnesses speak it, no configurable route target does.
    OpenAiResponses,
}

impl Dialect {
    pub fn label(self) -> &'static str {
        match self {
            Dialect::AnthropicMessages => "anthropic",
            Dialect::OpenAiChat => "openai-chat",
            Dialect::OpenAiResponses => "openai-responses",
        }
    }
}

/// Map a `[smith.models.*]` provider onto a dialect the router can serve.
///
/// Providers absent here are declared, usable by smith, and simply not
/// routable: `gemini`, `meta` (Responses API) and `ollama` (`/api/chat`
/// NDJSON) each speak a third shape, and claiming support without a
/// translator would corrupt turns rather than fail cleanly.
pub fn provider_dialect(provider: &str) -> Option<Dialect> {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => Some(Dialect::AnthropicMessages),
        // Grok is served by smith's OpenAI client and speaks the same wire
        // format.
        "openai" | "grok" => Some(Dialect::OpenAiChat),
        _ => None,
    }
}

/// How a harness can be routed.
///
/// Per spec 0111 each entry is an empirical claim about a specific
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
        // every TLS connection fail. It therefore gets the composed
        // bundle, and is unroutable if that bundle cannot be built.
        "codex" => Some(HarnessRouting {
            dialect: Dialect::OpenAiResponses,
            intercept_hosts: &["chatgpt.com"],
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
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Dialect the *target* speaks. When it differs from the harness's,
    /// the proxy translates instead of merely redirecting (spec 0112).
    pub target_dialect: Dialect,
    /// Dialect the harness speaks, i.e. what the response must look like.
    pub client_dialect: Dialect,
    pub client: reqwest::Client,
}

impl ArmedRoute {
    pub fn translates(&self) -> bool {
        self.target_dialect != self.client_dialect
    }
}

/// Per-session routing state, looked up from the proxy credential the
/// harness presents on `CONNECT`.
pub struct SessionRouting {
    pub session_id: String,
    pub harness: HarnessRouting,
    pub ca: Arc<RouterCa>,
    pub upstream_proxy: Option<UpstreamProxy>,
    route: RwLock<Option<ArmedRoute>>,
    observed: AtomicBool,
    /// Fired once, the first time interception actually serves a request,
    /// so the session record can stop reporting the route as unproven.
    observed_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl SessionRouting {
    pub fn armed_route(&self) -> Option<ArmedRoute> {
        self.route.read().unwrap().clone()
    }

    pub fn intercepts_host(&self, host: &str) -> bool {
        self.harness
            .intercept_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }

    /// Record that interception actually served a request. Until this
    /// flips, an armed route is unproven: the harness may resolve its
    /// endpoint through a channel that ignores our injection (spec 0111).
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
    port: u16,
    /// Route targets: the `[smith.models.*]` profiles, so an endpoint is
    /// declared once and reachable from both smith and a routed session.
    profiles: BTreeMap<String, ModelProfile>,
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
            port: cfg.port,
            profiles,
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

        let ctx = Arc::new(SessionRouting {
            session_id: session_id.to_string(),
            harness: routing,
            ca,
            upstream_proxy: self.upstream_proxy.clone(),
            route: RwLock::new(None),
            observed: AtomicBool::new(false),
            observed_tx: self.observed_tx.clone(),
        });
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), ctx.clone());
        self.tokens.write().unwrap().insert(token.clone(), ctx);

        Ok(self.session_env(&token, routing))
    }

    fn session_env(&self, token: &str, routing: HarnessRouting) -> HashMap<String, String> {
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

    /// Whether the router has actually served an intercepted request for
    /// this session — the difference between a route that is armed and one
    /// that is working (spec 0111).
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
        if profile.resolved_base_url().is_none() {
            return Some(format!("provider \"{}\" has no base_url", profile.provider));
        }
        if profile.model.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Some("profile sets no model".to_string());
        }
        let _ = dialect;
        profile.resolve_api_key().err()
    }

    fn resolve(&self, name: &str, harness: &str) -> Result<ArmedRoute> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            anyhow!("no model profile named \"{name}\" is configured under [smith.models]")
        })?;
        let routing = harness_routing(harness)
            .ok_or_else(|| anyhow!("harness {harness} is not route-capable"))?;
        if let Some(reason) = self.profile_blocker(profile, harness) {
            return Err(anyhow!("route \"{name}\": {reason}"));
        }
        let target_dialect = provider_dialect(&profile.provider)
            .ok_or_else(|| anyhow!("route \"{name}\": unsupported provider"))?;
        Ok(ArmedRoute {
            name: name.to_string(),
            base_url: profile
                .resolved_base_url()
                .ok_or_else(|| anyhow!("route \"{name}\": no base_url"))?,
            model: profile.model.clone().unwrap_or_default(),
            api_key: profile.resolve_api_key().map_err(|e| anyhow!(e))?,
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
            // always reachable and therefore cannot fail (spec 0110).
            *ctx.route.write().unwrap() = None;
            return Ok(None);
        };
        let armed = self.resolve(name, harness)?;
        let summary = SessionRoute {
            name: armed.name.clone(),
            model: armed.model.clone(),
            origin_model,
            observed: ctx.observed(),
        };
        *ctx.route.write().unwrap() = Some(armed);
        Ok(Some(summary))
    }

    /// Routes offered for a session's picker (spec 0111: render the
    /// reason, never an empty list).
    pub fn list_routes(
        &self,
        harness: &str,
        attached: bool,
        active: Option<String>,
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
        } else if self.profiles.is_empty() {
            Some("no model profiles configured; add a [smith.models.<name>] block".to_string())
        } else {
            None
        };

        let routes = self
            .profiles
            .iter()
            .map(|(name, profile)| RouteOption {
                name: name.clone(),
                dialect: provider_dialect(&profile.provider)
                    .map(|d| d.label().to_string())
                    .unwrap_or_else(|| profile.provider.clone()),
                model: profile.model.clone().unwrap_or_default(),
                base_url: profile.resolved_base_url().unwrap_or_default(),
                unavailable_reason: routing
                    .and_then(|_| self.profile_blocker(profile, harness)),
            })
            .collect();

        RouterListRoutesResult {
            routes,
            unavailable_reason,
            active,
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
        RouterConfig { enabled, port: 0 }
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

    async fn started_with(
        dir: &tempfile::TempDir,
        cfg: RouterConfig,
        profiles: BTreeMap<String, ModelProfile>,
    ) -> Arc<Router> {
        let r = Router::new(dir.path().to_path_buf(), &cfg, profiles);
        r.start().await.unwrap();
        r
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
        let listed = r.list_routes("shell", false, None);
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
        let err = r.set_route("s1", "claude", Some("kimi"), None).unwrap_err();
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
            .set_route("s1", "claude", Some("kimi"), Some("claude-opus-5".into()))
            .unwrap()
            .unwrap();
        assert_eq!(armed.name, "kimi");
        assert_eq!(armed.model, "kimi-k2.5");
        assert_eq!(armed.origin_model.as_deref(), Some("claude-opus-5"));
        assert!(!armed.observed, "nothing has been proxied yet");

        // Clearing always succeeds (spec 0110).
        assert!(r.set_route("s1", "claude", None, None).unwrap().is_none());
    }

    /// A provider with no translator is offered but not selectable, with
    /// the reason attached rather than hidden (spec 0111).
    #[tokio::test]
    async fn untranslatable_providers_are_listed_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started_with(
            &dir,
            cfg_with(true),
            profiles(vec![("gemini-pro", profile("gemini", Some("X")))]),
        )
        .await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None);
        assert!(listed.unavailable_reason.is_none());
        let reason = listed.routes[0].unavailable_reason.as_deref().unwrap();
        assert!(reason.contains("no translator"), "{reason}");
        assert!(r.set_route("s1", "claude", Some("gemini-pro"), None).is_err());
    }

    /// An OpenAI-dialect profile IS selectable from an Anthropic-dialect
    /// harness — that is what the translator is for (spec 0112).
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

        let listed = r.list_routes("claude", true, None);
        assert_eq!(listed.routes[0].unavailable_reason, None);
        assert_eq!(listed.routes[0].dialect, "openai-chat");
        // The provider's own default endpoint is resolved, exactly as
        // smith would resolve it.
        assert_eq!(listed.routes[0].base_url, "https://api.openai.com/v1");

        let armed = r
            .set_route("s1", "claude", Some("gpt"), None)
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
        let listed = r.list_routes("claude", true, None);
        assert!(listed.routes[0]
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
            .set_route("never-attached", "claude", Some("kimi"), None)
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
        r.set_route("s1", "claude", Some("kimi"), Some("claude-opus-5".into()))
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


    /// End-to-end cross-dialect routing (spec 0112): an Anthropic-dialect
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
        r.set_route("s1", "claude", Some("gpt"), Some("claude-opus-5".into()))
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


    /// End-to-end for a Responses-speaking harness (spec 0112): a `pi`
    /// session sends an OpenAI Responses request, the router translates it
    /// to Chat Completions for the target, and re-encodes the reply as a
    /// Responses event stream the harness can render.
    #[tokio::test]
    async fn translates_a_responses_harness_onto_a_chat_target() {
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
        let env = r.attach_session("s1", "pi", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();
        assert!(
            env.contains_key("NODE_EXTRA_CA_CERTS"),
            "pi takes its CA through the Node variable"
        );
        r.set_route("s1", "pi", Some("gpt"), Some("gpt-5.6-sol".into()))
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
        let body = br#"{"model":"gpt-5.6-sol","stream":true,"store":false,"instructions":"be terse","input":[{"role":"user","content":[{"type":"input_text","text":"say pong"}]}],"tools":[{"type":"function","name":"read","description":"read a file","parameters":{"type":"object","properties":{}}}]}"#;
        tls.write_all(
            format!(
                "POST /backend-api/codex/responses HTTP/1.1\r\nhost: chatgpt.com\r\nauthorization: Bearer users-own-token\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
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
