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

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use construct_protocol::{RouteOption, RouterListRoutesResult, SessionRoute};
use tokio::net::TcpListener;

use crate::config::{RouteConfig, RouterConfig};
use ca::RouterCa;

/// Env var carrying the proxy the harness should use. This is the only
/// channel Construct injects for transport; the harness's own endpoint
/// configuration is never displaced (spec 0109).
pub const PROXY_ENV: &str = "HTTPS_PROXY";
/// Lowercase spelling, honored by some clients in preference to the
/// uppercase one. Both are set to the same value.
pub const PROXY_ENV_LOWER: &str = "https_proxy";

/// How a harness can be routed.
///
/// Per spec 0111 each entry is an empirical claim about a specific
/// harness, established by a probe (see `router_probe` in the e2e suite),
/// not a reading of that harness's documentation. A harness absent from
/// this table is not route-capable, and offering to route it is a bug.
#[derive(Debug, Clone, Copy)]
pub struct HarnessRouting {
    /// Wire dialect the harness speaks to its model endpoint.
    pub dialect: &'static str,
    /// Hosts that carry model traffic and may therefore be intercepted
    /// while a route is armed. Everything else always tunnels.
    pub intercept_hosts: &'static [&'static str],
    /// Env vars through which this harness accepts an additional trusted
    /// CA. Empty means interception is impossible: the harness can be
    /// observed but not redirected.
    pub ca_env: &'static [&'static str],
}

pub fn harness_routing(harness: &str) -> Option<HarnessRouting> {
    match harness {
        // Node/undici: honors HTTPS_PROXY and NODE_EXTRA_CA_CERTS.
        "claude" => Some(HarnessRouting {
            dialect: "anthropic",
            intercept_hosts: &["api.anthropic.com"],
            ca_env: &["NODE_EXTRA_CA_CERTS"],
        }),
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
    pub client: reqwest::Client,
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
        self.observed.store(true, Ordering::Relaxed);
    }

    pub fn observed(&self) -> bool {
        self.observed.load(Ordering::Relaxed)
    }
}

pub struct Router {
    enabled: bool,
    port: u16,
    routes: BTreeMap<String, RouteConfig>,
    state_dir: PathBuf,
    ca: RwLock<Option<Arc<RouterCa>>>,
    upstream_proxy: Option<UpstreamProxy>,
    listening: AtomicBool,
    /// The port actually bound. Equals `port` in every real configuration;
    /// differs only when `port = 0` asks the OS to choose (tests).
    bound_port: std::sync::atomic::AtomicU16,
    sessions: RwLock<HashMap<String, Arc<SessionRouting>>>,
    /// Proxy credential → session. A `CONNECT` that presents no known
    /// credential is served as a plain tunnel: unattributable traffic gets
    /// the safe path, never a route.
    tokens: RwLock<HashMap<String, Arc<SessionRouting>>>,
}

impl Router {
    pub fn new(state_dir: PathBuf, cfg: &RouterConfig) -> Arc<Self> {
        let upstream_proxy = std::env::var(PROXY_ENV)
            .ok()
            .or_else(|| std::env::var(PROXY_ENV_LOWER).ok())
            .as_deref()
            .and_then(UpstreamProxy::from_env_value);
        Arc::new(Self {
            enabled: cfg.enabled,
            port: cfg.port,
            routes: cfg.routes.clone(),
            state_dir,
            ca: RwLock::new(None),
            upstream_proxy,
            listening: AtomicBool::new(false),
            bound_port: std::sync::atomic::AtomicU16::new(0),
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
        });
        self.sessions
            .write()
            .unwrap()
            .insert(session_id.to_string(), ctx.clone());
        self.tokens.write().unwrap().insert(token.clone(), ctx);

        Ok(self.session_env(&token))
    }

    fn session_env(&self, token: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        // The credential rides in the proxy URL's userinfo, which is how
        // proxy clients carry `Proxy-Authorization`. One listener with
        // per-session credentials beats one listener per session: only a
        // single port has to be reclaimed after a daemon restart.
        let url = format!("http://{token}@127.0.0.1:{}", self.port());
        env.insert(PROXY_ENV.to_string(), url.clone());
        env.insert(PROXY_ENV_LOWER.to_string(), url);
        if let Ok(ca) = self.ca() {
            let path = ca.cert_path().to_string_lossy().to_string();
            // Trusting the router CA changes nothing until a route is
            // armed — no interception happens without one — but it has to
            // be present at spawn, because the harness reads it once.
            for var in ["NODE_EXTRA_CA_CERTS"] {
                env.insert(var.to_string(), path.clone());
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

    pub fn observed(&self, session_id: &str) -> bool {
        self.sessions
            .read()
            .unwrap()
            .get(session_id)
            .is_some_and(|s| s.observed())
    }

    fn resolve(&self, name: &str, harness: &str) -> Result<ArmedRoute> {
        let cfg = self
            .routes
            .get(name)
            .ok_or_else(|| anyhow!("no route named \"{name}\" is configured"))?;
        let routing = harness_routing(harness)
            .ok_or_else(|| anyhow!("harness {harness} is not route-capable"))?;
        if !cfg.dialect.eq_ignore_ascii_case(routing.dialect) {
            return Err(anyhow!(
                "route \"{name}\" speaks {} but {harness} speaks {}; \
                 a route redirects an endpoint, it does not translate between dialects",
                cfg.dialect,
                routing.dialect
            ));
        }
        if routing.ca_env.is_empty() {
            return Err(anyhow!(
                "{harness} exposes no way to trust the router CA, so it can be \
                 observed but not redirected"
            ));
        }
        let api_key = cfg.resolve_api_key().map_err(|e| anyhow!(e))?;
        Ok(ArmedRoute {
            name: name.to_string(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            model: cfg.model.clone(),
            api_key,
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

    /// Routes offered for a session's picker, each with the reason it
    /// cannot be selected when that applies (spec 0111: render the reason,
    /// never an empty list).
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
        } else {
            None
        };

        let routes = self
            .routes
            .iter()
            .map(|(name, cfg)| {
                let reason = if let Some(r) = routing {
                    if !cfg.dialect.eq_ignore_ascii_case(r.dialect) {
                        Some(format!("speaks {}, {harness} speaks {}", cfg.dialect, r.dialect))
                    } else if r.ca_env.is_empty() {
                        Some(format!("{harness} cannot trust the router CA"))
                    } else {
                        cfg.resolve_api_key().err()
                    }
                } else {
                    None
                };
                RouteOption {
                    name: name.clone(),
                    dialect: cfg.dialect.clone(),
                    model: cfg.model.clone(),
                    base_url: cfg.base_url.clone(),
                    unavailable_reason: reason,
                }
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

    fn cfg_with(enabled: bool, routes: Vec<(&str, RouteConfig)>) -> RouterConfig {
        RouterConfig {
            enabled,
            // Let the OS pick, so concurrent tests don't collide on the
            // fixed production port.
            port: 0,
            routes: routes
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn route(dialect: &str, key_env: Option<&str>) -> RouteConfig {
        RouteConfig {
            dialect: dialect.to_string(),
            base_url: "https://api.moonshot.ai/anthropic".to_string(),
            model: "kimi-k2.5".to_string(),
            api_key_env: key_env.map(str::to_string),
            api_key: None,
        }
    }

    async fn started(dir: &tempfile::TempDir, cfg: RouterConfig) -> Arc<Router> {
        let r = Router::new(dir.path().to_path_buf(), &cfg);
        r.start().await.unwrap();
        r
    }

    /// A disabled router must be inert: no listener, no CA, no env.
    #[tokio::test]
    async fn disabled_router_is_completely_inert() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(false, vec![])).await;
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
        let r = started(&dir, cfg_with(true, vec![])).await;
        let env = r.attach_session("s1", "claude", None).unwrap();
        let proxy = env.get(PROXY_ENV).unwrap();
        assert!(
            proxy.starts_with("http://") && proxy.contains(&format!("@127.0.0.1:{}", r.port())),
            "{proxy}"
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
        let first = started(&dir, cfg_with(true, vec![])).await;
        let env = first.attach_session("s1", "claude", None).unwrap();
        let token = Router::token_from_env(&env).unwrap();

        let second = started(&dir, cfg_with(true, vec![])).await;
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
        let r = started(&dir, cfg_with(true, vec![])).await;
        assert!(r.attach_session("s1", "codex", None).is_err());
        let listed = r.list_routes("codex", false, None);
        assert!(listed
            .unavailable_reason
            .unwrap()
            .contains("not route-capable"));
    }

    #[tokio::test]
    async fn arming_requires_a_resolvable_key() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(
            &dir,
            cfg_with(
                true,
                vec![("kimi", route("anthropic", Some("NOT_SET_ANYWHERE")))],
            ),
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
        let r = started(
            &dir,
            cfg_with(
                true,
                vec![("kimi", route("anthropic", Some("CONSTRUCT_TEST_ROUTE_KEY")))],
            ),
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

    /// A cross-dialect route is offered but not selectable, with the
    /// reason attached rather than hidden (spec 0111).
    #[tokio::test]
    async fn cross_dialect_routes_are_listed_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true, vec![("gpt", route("openai", Some("X")))])).await;
        r.attach_session("s1", "claude", None).unwrap();
        let listed = r.list_routes("claude", true, None);
        assert!(listed.unavailable_reason.is_none());
        let reason = listed.routes[0].unavailable_reason.as_deref().unwrap();
        assert!(reason.contains("openai"), "{reason}");
        assert!(r.set_route("s1", "claude", Some("gpt"), None).is_err());
    }

    #[tokio::test]
    async fn a_session_without_transport_cannot_be_armed() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true, vec![("kimi", route("anthropic", None))])).await;
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
    }

    /// The whole safety claim in one test: an unrouted session's bytes
    /// reach the destination it named, unmodified, and the router never
    /// terminates TLS on that path.
    #[tokio::test]
    async fn passes_through_unrouted_traffic_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let r = started(&dir, cfg_with(true, vec![])).await;
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
        let r = started(&dir, cfg_with(true, vec![])).await;

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
        let mut cfg = cfg_with(true, vec![]);
        cfg.routes.insert(
            "kimi".to_string(),
            RouteConfig {
                dialect: "anthropic".to_string(),
                base_url: format!("http://127.0.0.1:{upstream_port}"),
                model: "kimi-k2.5".to_string(),
                api_key_env: None,
                api_key: Some("sk-route".to_string()),
            },
        );
        let r = started(&dir, cfg).await;
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
