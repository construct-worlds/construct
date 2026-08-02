//! The per-session proxy connection handler (spec 0113).
//!
//! Two paths, and the split between them is the whole safety argument:
//!
//! - **Pass-through** — accept the `CONNECT`, dial the host the client
//!   named, splice the sockets. No TLS termination, no parsing, no header
//!   or credential handling. It cannot alter a request because it never
//!   sees one.
//! - **Intercept** — only for the fixed model hosts of a session with a
//!   currently armed route or an enabled native model catalog.
//!   Terminates TLS with a minted leaf, resolves any request-carried
//!   Construct model id, and streams the response back.
//!
//! Everything else — the harness's auth refresh, telemetry, package
//! registries, and whatever a spawned MCP server talks to — takes the
//! blind pass-through path. A native model request inspected for catalog
//! selection is reconstructed to the same origin with its native
//! credential intact.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{translate, ArmedRoute, Dialect, Router, SessionRouting, TargetAuth, UpstreamProxy};

/// Cap on a request head. Anthropic-dialect requests carry large bodies
/// but small heads; this only bounds the header block.
const MAX_HEAD: usize = 64 * 1024;
const MAX_ERROR_BODY: usize = 64 * 1024;
const MAX_REQUEST_BODY: usize = 100 * 1024 * 1024;
const MAX_RESPONSE_BODY: usize = 100 * 1024 * 1024;
const MAX_SSE_FRAME: usize = 100 * 1024 * 1024;
const ERROR_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CONSTRUCT_SESSION_HEADER: &str = "x-construct-session";

/// Serve one client connection.
///
/// A connection is attributed to a session by the proxy credential it
/// presents. An unattributed connection — no credential, or one we don't
/// know — is tunneled: traffic we cannot place is never routed.
pub async fn serve(mut client: TcpStream, router: Arc<Router>) -> Result<()> {
    let head = read_head(&mut client).await?;
    let request = parse_request(&head)?;
    if !request.method.eq_ignore_ascii_case("CONNECT") {
        return serve_claude_gateway(client, request, router).await;
    }
    let target = parse_connect(&head)?;
    let ctx = proxy_credential(&head).and_then(|t| router.session_for_token(&t));

    let armed = ctx
        .as_ref()
        .filter(|c| c.intercepts_host(&target.host))
        .and_then(|c| c.armed_route());
    let catalog_intercept = ctx
        .as_ref()
        .is_some_and(|context| context.catalog_enabled() && context.intercepts_host(&target.host));

    // Every connection is logged with how it was classified. Without this
    // there is no way to answer "is this session actually going through
    // the router?" — a harness that quietly bypasses us looks identical to
    // one that never made a request.
    tracing::debug!(
        session = ctx.as_ref().map(|c| c.session_id.as_str()).unwrap_or("-"),
        host = %target.host,
        port = target.port,
        disposition = if armed.is_some() || catalog_intercept { "intercept" } else { "tunnel" },
        "router connection"
    );

    if armed.is_none() && !catalog_intercept {
        let upstream = ctx
            .as_ref()
            .and_then(|c| c.upstream_proxy.clone())
            .or_else(|| router.upstream_proxy().cloned());
        // A tunnel to a host this session could route decides its
        // disposition once, here. Hand it the session so it can notice a
        // later route change and step aside; tunnels to every other host
        // are untouched by routing and never need to.
        let drain = ctx
            .as_ref()
            .filter(|c| c.intercepts_host(&target.host))
            .cloned();
        return tunnel(client, &target, upstream.as_ref(), drain).await;
    }

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("ack CONNECT")?;
    intercept_tls(
        client,
        &target,
        armed,
        ctx.expect("interception implies a session"),
        router,
    )
    .await
}

/// Serve Claude Code's session-local gateway URL on the same loopback
/// listener as the HTTPS proxy.
///
/// The opaque session token in `x-construct-session` is the authority
/// boundary. Keeping it out of the base URL lets every Claude session share
/// one native gateway-model cache entry. Older adapters that carry the token
/// in the path remain supported across daemon restarts.
async fn serve_claude_gateway(
    mut client: TcpStream,
    mut request: ParsedRequest,
    router: Arc<Router>,
) -> Result<()> {
    const PREFIX: &str = "/__construct/";
    let Some(rest) = request.path.strip_prefix(PREFIX) else {
        return write_simple(&mut client, 404, r#"{"error":"not found"}"#).await;
    };
    let Some((path_authority, upstream_path)) = rest.split_once('/') else {
        return write_simple(&mut client, 404, r#"{"error":"not found"}"#).await;
    };
    let header_token = request
        .header(CONSTRUCT_SESSION_HEADER)
        .map(String::from_utf8_lossy);
    let token = if path_authority == "claude" {
        let Some(token) = header_token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return write_simple(&mut client, 403, r#"{"error":"missing session"}"#).await;
        };
        token
    } else {
        path_authority
    };
    let Some(ctx) = router.session_for_token(token) else {
        return write_simple(&mut client, 403, r#"{"error":"unknown session"}"#).await;
    };
    if ctx.harness_name != "claude" || !ctx.catalog_enabled() {
        return write_simple(&mut client, 403, r#"{"error":"catalog unavailable"}"#).await;
    }
    let construct_gateway_auth = request.header("authorization").is_some_and(|value| {
        String::from_utf8_lossy(value)
            .trim()
            .strip_prefix("Bearer ")
            .is_some_and(|credential| credential == token)
    }) || request
        .header("x-api-key")
        .is_some_and(|value| String::from_utf8_lossy(value).trim() == token);
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case(CONSTRUCT_SESSION_HEADER));
    request.path = format!("/{upstream_path}");
    let body = read_body(&mut client, &request).await?;
    let pinned_route = ctx.armed_route();
    handle_intercepted_request(
        &mut client,
        &Target {
            host: "api.anthropic.com".to_string(),
            port: 443,
        },
        pinned_route,
        ctx,
        router,
        request,
        body,
        construct_gateway_auth,
    )
    .await
}

/// Extract the session credential from `Proxy-Authorization: Basic …`.
///
/// The credential is carried as the userinfo of the injected proxy URL,
/// which proxy clients turn into this header. A client that ignores proxy
/// userinfo simply goes unattributed, and therefore untouched.
pub fn proxy_credential(head: &[u8]) -> Option<String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(head).ok()?;
    let value = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("proxy-authorization"))
        .map(|h| h.value)?;
    let value = String::from_utf8_lossy(value);
    let encoded = value.trim().strip_prefix("Basic ").or_else(|| {
        value
            .trim()
            .strip_prefix("basic ")
    })?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let token = decoded.split(':').next().unwrap_or_default().trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Read the request head (through the blank line) without consuming a byte
/// of what follows it.
async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.context("read request head")?;
        if n == 0 {
            bail!("client closed before completing the request head");
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            bail!("request head exceeded {MAX_HEAD} bytes");
        }
    }
}

/// Parse a `CONNECT host:port HTTP/1.1` request line.
///
/// A non-`CONNECT` method means the client is proxying plain HTTP; we
/// reject rather than guess, because every endpoint we care about is TLS
/// and a silent mishandling here would be indistinguishable from a
/// network fault.
pub fn parse_connect(head: &[u8]) -> Result<Target> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(head).context("parse proxy request")?;
    let method = req.method.ok_or_else(|| anyhow!("no method"))?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        bail!("unsupported proxy method {method}; only CONNECT is served");
    }
    let authority = req.path.ok_or_else(|| anyhow!("CONNECT without authority"))?;
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("CONNECT authority {authority} has no port"))?;
    Ok(Target {
        host: host.trim_matches(['[', ']']).to_string(),
        port: port.parse().with_context(|| format!("port in {authority}"))?,
    })
}

/// How long a routable tunnel must be quiet, after the last bytes came
/// *from* the server, before a route change may close it.
///
/// Direction matters more than duration: a connection whose last bytes went
/// client→server has a request outstanding — the model may simply not have
/// started answering yet — and closing it would abort a turn in flight,
/// which spec 0114 forbids. A connection whose last bytes came back from
/// the server and has since gone quiet is between turns.
const DRAIN_QUIET_MS: i64 = 2_000;
const DRAIN_POLL_MS: u64 = 250;

/// Last-byte bookkeeping for a tunnel, used only to decide when a stale
/// tunnel may be closed.
struct Activity {
    last_ms: std::sync::atomic::AtomicI64,
    last_from_server: std::sync::atomic::AtomicBool,
}

impl Activity {
    fn new() -> Self {
        Self {
            last_ms: std::sync::atomic::AtomicI64::new(now_ms()),
            last_from_server: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn stamp(&self, from_server: bool) {
        use std::sync::atomic::Ordering;
        self.last_ms.store(now_ms(), Ordering::SeqCst);
        self.last_from_server.store(from_server, Ordering::SeqCst);
    }

    /// True when no request is outstanding and the line has gone quiet.
    fn safe_to_close(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.last_from_server.load(Ordering::SeqCst)
            && now_ms() - self.last_ms.load(Ordering::SeqCst) >= DRAIN_QUIET_MS
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Copy one direction, stamping activity so the drain check can tell a
/// waiting request from an idle connection.
async fn copy_tracking<R, W>(mut from: R, mut to: W, act: Arc<Activity>, from_server: bool)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if to.write_all(&buf[..n]).await.is_err() || to.flush().await.is_err() {
            break;
        }
        act.stamp(from_server);
    }
    let _ = to.shutdown().await;
}

/// Wait until this tunnel is both stale (the session's route changed since
/// it opened) and safe to close.
async fn wait_until_drainable(ctx: Arc<SessionRouting>, opened_at_epoch: u64, act: Arc<Activity>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(DRAIN_POLL_MS)).await;
        if ctx.route_epoch() != opened_at_epoch && act.safe_to_close() {
            return;
        }
    }
}

/// Blind byte splice to the destination the client named.
async fn tunnel(
    mut client: TcpStream,
    target: &Target,
    upstream_proxy: Option<&UpstreamProxy>,
    drain: Option<Arc<SessionRouting>>,
) -> Result<()> {
    let mut upstream = match upstream_proxy {
        // A pre-existing HTTPS_PROXY: chain to it rather than bypassing
        // it, so a user behind a corporate proxy keeps reaching it.
        Some(proxy) => connect_via_proxy(proxy, target).await?,
        None => TcpStream::connect((target.host.as_str(), target.port))
            .await
            .with_context(|| format!("dial {}:{}", target.host, target.port))?,
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("ack CONNECT")?;

    let Some(ctx) = drain else {
        // Not a host this session could route: nothing can make this
        // tunnel stale, so splice it and stay out of the way.
        return tokio::io::copy_bidirectional(&mut client, &mut upstream)
            .await
            .map(|_| ())
            .or_else(|e| match e.kind() {
                // Either side hanging up mid-stream is ordinary for
                // long-lived model connections, not an error worth
                // surfacing.
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => Ok(()),
                _ => Err(e).context("tunnel"),
            });
    };

    let opened_at_epoch = ctx.route_epoch();
    let act = Arc::new(Activity::new());
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    let to_upstream = copy_tracking(client_read, upstream_write, act.clone(), false);
    let to_client = copy_tracking(upstream_read, client_write, act.clone(), true);

    tokio::select! {
        _ = to_upstream => {}
        _ = to_client => {}
        // Closing here is what makes a route change take effect on a
        // running session: the client reconnects, and the fresh CONNECT is
        // classified against the route that is armed now.
        _ = wait_until_drainable(ctx.clone(), opened_at_epoch, act) => {
            tracing::debug!(
                session = %ctx.session_id,
                host = %target.host,
                "closing stale tunnel so the new route applies"
            );
        }
    }
    Ok(())
}

async fn connect_via_proxy(proxy: &UpstreamProxy, target: &Target) -> Result<TcpStream> {
    let mut up = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .with_context(|| format!("dial upstream proxy {}:{}", proxy.host, proxy.port))?;
    let mut req = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n",
        host = target.host,
        port = target.port
    );
    if let Some(auth) = proxy.authorization.as_deref() {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    up.write_all(req.as_bytes())
        .await
        .context("upstream CONNECT")?;

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = up.read(&mut byte).await.context("upstream CONNECT reply")?;
        if n == 0 {
            bail!("upstream proxy closed during CONNECT");
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD {
            bail!("upstream proxy CONNECT reply too large");
        }
    }
    let status_line = String::from_utf8_lossy(&head);
    let first = status_line.lines().next().unwrap_or_default();
    if !first.contains(" 200") {
        bail!("upstream proxy refused CONNECT: {first}");
    }
    Ok(up)
}

/// Terminate TLS in the origin's name and forward the request onto the
/// armed route's endpoint.
async fn intercept_tls(
    client: TcpStream,
    target: &Target,
    pinned_route: Option<ArmedRoute>,
    ctx: Arc<SessionRouting>,
    router: Arc<Router>,
) -> Result<()> {
    let server_config = ctx.ca.server_config(&target.host)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
    let mut tls = acceptor
        .accept(client)
        .await
        .with_context(|| format!("TLS handshake as {}", target.host))?;

    let head = read_head_tls(&mut tls).await?;
    let request = parse_request(&head)?;
    let body = read_body(&mut tls, &request).await?;
    handle_intercepted_request(
        &mut tls,
        target,
        pinned_route,
        ctx,
        router,
        request,
        body,
        false,
    )
    .await
}

async fn handle_intercepted_request<S>(
    stream: &mut S,
    target: &Target,
    pinned_route: Option<ArmedRoute>,
    ctx: Arc<SessionRouting>,
    router: Arc<Router>,
    mut request: ParsedRequest,
    body: Vec<u8>,
    construct_gateway_auth: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let body = decode_request_body(&mut request, body)?;

    // Claude Code's native gateway integration discovers picker entries
    // from this endpoint. The session-local gateway answers the model-list
    // request here; inference then follows the ordinary native-or-route
    // decision below.
    if ctx.catalog_enabled()
        && ctx.harness_name == "claude"
        && request.method.eq_ignore_ascii_case("GET")
        && request.path.split('?').next() == Some("/v1/models")
    {
        return write_simple(stream, 200, &router.claude_models_response().to_string()).await;
    }

    // A published picker id is an explicit request-scoped route and wins
    // over the session's manually pinned default. Native model ids retain
    // the pin, or pass through to the original host when no pin exists —
    // except for the harness's own internal seats, which pass through even
    // under a pin because the pin was never a statement about them
    // (spec 0166).
    let requested_model = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    let route = if ctx.catalog_enabled() {
        match requested_model.as_deref() {
            Some(model) => match router.resolve_published_model(&ctx.harness_name, model) {
                Ok(Some(route)) => Some(route),
                Ok(None) if ctx.is_role_model(model) => {
                    // An internal seat the harness filled for itself — the
                    // approval reviewer, not the model the user picked. The
                    // pin says which model does this session's work; it does
                    // not say the reviewer should stop being the reviewer.
                    // Substituting here hands a role-specific prompt to a
                    // model chosen for an unrelated job (spec 0166).
                    if let Some(route) = pinned_route.as_ref() {
                        tracing::info!(
                            session = %ctx.session_id,
                            requested_model = %model,
                            pinned_route = %route.name,
                            pinned_model = %route.model,
                            "internal-seat model passes through to its native provider \
                             instead of this session's pinned route"
                        );
                    }
                    None
                }
                Ok(None) => {
                    if pinned_route.is_some() || !construct_gateway_auth {
                        pinned_route
                    } else {
                        // Claude's gateway discovery requires an API-shaped
                        // credential. For subscription sessions the adapter
                        // presents the session capability token; exchange it
                        // here for the Claude login Construct already detected
                        // so built-in native picker rows keep working alongside
                        // the published routes.
                        match router.resolve("claude-oauth", &ctx.harness_name, Some(model)) {
                            Ok(route) => Some(route),
                            Err(error) => {
                                let payload = serde_json::json!({
                                    "type": "error",
                                    "error": {
                                        "type": "api_error",
                                        "message": format!(
                                            "construct native Claude route: {error:#}"
                                        ),
                                    },
                                })
                                .to_string();
                                write_simple(stream, 502, &payload).await.ok();
                                return Err(error);
                            }
                        }
                    }
                }
                Err(error) => {
                    let payload = serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": format!("construct router: {error:#}"),
                        },
                    })
                    .to_string();
                    write_simple(stream, 502, &payload).await.ok();
                    return Err(error);
                }
            },
            None => pinned_route,
        }
    } else {
        pinned_route
    };

    // Picker publication requires TLS inspection to read the model id. A
    // native model with no manual pin is reconstructed onto the exact host
    // named by CONNECT with the client's credential intact.
    let Some(route) = route else {
        return match forward_native(&request, body, target).await {
            Ok(response) => write_response(stream, response).await,
            Err(error) => {
                let payload = serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("construct native passthrough: {error:#}"),
                    },
                })
                .to_string();
                write_simple(stream, 502, &payload).await.ok();
                Err(error)
            }
        };
    };

    // A substituted model is invisible from inside the harness: it keeps
    // recording the model it asked for, whatever the router sent. Leave a
    // record on our side so a session running on something other than what
    // its transcript claims is diagnosable (spec 0166).
    if let Some(requested) = requested_model
        .as_deref()
        .filter(|model| *model != route.model)
    {
        tracing::info!(
            session = %ctx.session_id,
            requested_model = %requested,
            route = %route.name,
            model = %route.model,
            "request routed to a substituted model"
        );
    }

    // Same dialect → redirect the bytes. Different dialect → rebuild the
    // request and re-encode the response stream (spec 0116).
    // The dialect the harness speaks is read from the request itself, not
    // declared: a provider-agnostic harness emits whatever its configured
    // provider speaks (spec 0116).
    //
    // A shape we do not recognize is an error, never a guess. Falling back
    // to a declared dialect would translate under an assumption we just
    // failed to confirm, and a wrong translation corrupts the turn instead
    // of failing it.
    let detected = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .as_ref()
        .and_then(translate::detect_dialect);

    // Token counting has no counterpart on most targets; answering locally
    // keeps the harness's context bookkeeping working instead of failing
    // the turn. Handled before dialect detection because these bodies are
    // not always full requests.
    if request.path.contains("count_tokens") {
        ctx.mark_observed();
        return write_simple(stream, 200, &count_tokens_reply(&body)).await;
    }

    // Detection decides whether this request needs translating at all.
    // When it fails, the only safe move depends on what we would do next:
    // a same-dialect redirect never interprets the body, so an
    // unrecognized shape is harmless there; a translation would have to
    // interpret it, and doing that on a guess corrupts the turn.
    let client_dialect = match detected {
        Some(d) => d,
        None if ctx.harness.dialect == route.target_dialect => ctx.harness.dialect,
        None => {
            ctx.mark_observed();
            let payload = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": "construct router: unrecognized request dialect; refusing to translate on a guess",
                },
            })
            .to_string();
            write_simple(stream, 502, &payload).await.ok();
            bail!("unrecognized request dialect on an armed route");
        }
    };

    // A pin-chosen effort must be injected into the request body, which the
    // byte-forward path does not do — rebuild whenever the pin names one
    // (spec 0165).
    if route.needs_rebuild()
        || client_dialect != route.target_dialect
        || route.pin_effort.is_some()
    {
        ctx.mark_observed();
        let streaming = wants_stream(&body);
        return match forward_translated(body, &route, client_dialect, &ctx).await {
            Ok(forwarded) => {
                write_translated_response(
                    stream,
                    forwarded.response,
                    &route,
                    client_dialect,
                    streaming,
                    &forwarded.context,
                    &ctx,
                )
                .await
            }
            Err(e) => {
                // Keep transport diagnostics in the daemon error chain. URLs
                // and lower-level client errors are not safe to reflect into
                // a harness transcript.
                let payload = translate::error_body(
                    client_dialect,
                    "construct router: upstream request failed",
                )
                .to_string();
                write_simple(stream, 502, &payload).await.ok();
                Err(e)
            }
        };
    }

    match forward(&request, body, &route).await {
        Ok(response) => {
            ctx.mark_observed();
            write_response(stream, response).await
        }
        Err(e) => {
            // Answer in the dialect's own error shape so the harness
            // surfaces a real error instead of parsing garbage into a
            // bogus assistant turn.
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": format!("construct router: {e:#}"),
                }
            })
            .to_string();
            let head = format!(
                "HTTP/1.1 502 Bad Gateway\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.ok();
            stream.write_all(body.as_bytes()).await.ok();
            stream.shutdown().await.ok();
            Err(e)
        }
    }
}

async fn read_head_tls<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(2048);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.context("read request head")?;
        if n == 0 {
            bail!("client closed before completing the request head");
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            bail!("request head exceeded {MAX_HEAD} bytes");
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, Vec<u8>)>,
}

impl ParsedRequest {
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }
}

pub fn parse_request(head: &[u8]) -> Result<ParsedRequest> {
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(head).context("parse intercepted request")?;
    Ok(ParsedRequest {
        method: req.method.unwrap_or("POST").to_string(),
        path: req.path.unwrap_or("/").to_string(),
        headers: req
            .headers
            .iter()
            .filter(|h| !h.name.is_empty())
            .map(|h| (h.name.to_string(), h.value.to_vec()))
            .collect(),
    })
}

async fn read_body<S>(stream: &mut S, req: &ParsedRequest) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    if req
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case(b"chunked"))
    {
        return read_chunked(stream).await;
    }
    let len = match req.header("content-length") {
        Some(v) => String::from_utf8_lossy(v)
            .trim()
            .parse::<usize>()
            .context("parse content-length")?,
        None => return Ok(Vec::new()),
    };
    if len > MAX_REQUEST_BODY {
        bail!("request body exceeded {MAX_REQUEST_BODY} bytes");
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .context("read request body")?;
    Ok(body)
}

/// Decode request compression before inspecting or translating JSON.
///
/// Current Codex clients zstd-compress Responses requests by default. A
/// catalog alias is inside that body, so treating the compressed bytes as
/// opaque would leak the synthetic model id to ChatGPT. Native requests are
/// also decoded and forwarded semantically unchanged; removing the encoding
/// header lets the HTTP client recalculate framing for the decoded bytes.
fn decode_request_body(request: &mut ParsedRequest, body: Vec<u8>) -> Result<Vec<u8>> {
    let Some(encoding) = request.header("content-encoding") else {
        return Ok(body);
    };
    if !String::from_utf8_lossy(encoding)
        .trim()
        .eq_ignore_ascii_case("zstd")
    {
        return Ok(body);
    }

    use std::io::Read;
    let decoder = zstd::stream::Decoder::new(body.as_slice()).context("open zstd request body")?;
    let mut decoded = Vec::new();
    decoder
        .take((MAX_REQUEST_BODY + 1) as u64)
        .read_to_end(&mut decoded)
        .context("decode zstd request body")?;
    if decoded.len() > MAX_REQUEST_BODY {
        bail!("decoded request body exceeded {MAX_REQUEST_BODY} bytes");
    }
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("content-encoding"));
    Ok(decoded)
}

async fn read_chunked<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut out = Vec::new();
    loop {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await.context("read chunk size")?;
            if n == 0 {
                bail!("closed mid chunk-size");
            }
            line.push(byte[0]);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&line);
        let size = usize::from_str_radix(text.trim().split(';').next().unwrap_or("0").trim(), 16)
            .context("parse chunk size")?;
        if size == 0 {
            // Trailer section through the final blank line.
            let mut tail = Vec::new();
            loop {
                let n = stream.read(&mut byte).await.context("read chunk trailer")?;
                if n == 0 {
                    break;
                }
                tail.push(byte[0]);
                if tail.ends_with(b"\r\n") {
                    break;
                }
            }
            return Ok(out);
        }
        let mut chunk = vec![0u8; size + 2];
        stream.read_exact(&mut chunk).await.context("read chunk")?;
        chunk.truncate(size);
        out.extend_from_slice(&chunk);
    }
}

fn is_hop_header(name: &str) -> bool {
    const DROP: &[&str] = &[
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "proxy-connection",
        "proxy-authorization",
        "keep-alive",
        "upgrade",
        "te",
        "trailer",
        "accept-encoding",
        CONSTRUCT_SESSION_HEADER,
    ];
    DROP.iter().any(|d| name.eq_ignore_ascii_case(d))
}

/// Headers we never copy onto a routed request: hop-by-hop framing and the
/// client's credential, which belongs only to its native provider.
fn is_dropped_header(name: &str) -> bool {
    is_hop_header(name)
        || name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("api-key")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("x-goog-api-key")
}

/// Substitute the route's model into an Anthropic-dialect request body.
///
/// A body that isn't JSON, or carries no `model`, is forwarded byte-for-
/// byte: the route redirects the endpoint, and inventing a field the
/// client didn't send would be a change the user never asked for.
pub fn rewrite_body(body: &[u8], model: &str) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    let Some(obj) = value.as_object_mut() else {
        return body.to_vec();
    };
    if !obj.contains_key("model") {
        return body.to_vec();
    }
    obj.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
}

/// Join a route's base URL with the intercepted request path.
pub fn target_url(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// Reconstruct a picker-enabled session's native request after inspecting
/// its model id. This path is allowed only for the harness's hard-coded
/// model host selected by CONNECT, and it deliberately preserves the
/// client's native credential.
async fn forward_native(
    req: &ParsedRequest,
    body: Vec<u8>,
    target: &Target,
) -> Result<reqwest::Response> {
    let authority = if target.port == 443 {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let url = target_url(&format!("https://{authority}"), &req.path);
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).context("method")?;
    let client = reqwest::Client::new();
    let mut out = client.request(method, &url);
    for (name, value) in &req.headers {
        if is_hop_header(name) {
            continue;
        }
        out = out.header(name, value.clone());
    }
    out.body(body)
        .send()
        .await
        .with_context(|| format!("native passthrough to {url}"))
}

/// Same-dialect redirect: keep the request as-is, change where it goes.
async fn forward(
    req: &ParsedRequest,
    body: Vec<u8>,
    route: &ArmedRoute,
) -> Result<reqwest::Response> {
    // Same JSON dialect does not imply the same path: a Responses-speaking
    // harness may call ChatGPT's `/backend-api/codex/responses` while an
    // Azure target expects `/openai/v1/responses`. The armed endpoint is
    // the target's path; carrying the client's path across providers is a
    // protocol bug even when the body can remain byte-for-byte.
    let url = route.endpoint.clone();
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).context("method")?;
    let mut out = route.client.request(method, &url);
    for (name, value) in &req.headers {
        if is_dropped_header(name) {
            continue;
        }
        out = out.header(name, value.clone());
    }
    if !route.api_key.is_empty() {
        out = match route.auth {
            TargetAuth::ApiKeyHeader => out.header("x-api-key", &route.api_key),
            TargetAuth::GoogleApiKey => out.header("x-goog-api-key", &route.api_key),
            TargetAuth::AzureApiKey => out.header("api-key", &route.api_key),
            TargetAuth::Bearer => out.header("authorization", format!("Bearer {}", route.api_key)),
        };
    }
    let body = rewrite_body(&body, &route.model);
    out = out.header("content-length", body.len().to_string()).body(body);
    out.send().await.with_context(|| format!("forward to {url}"))
}

/// Cross-dialect: rebuild the request for the target's API (spec 0116).
///
/// The client's path is deliberately not carried over — the target's
/// endpoint is a different one, not the same path on another host.
async fn forward_translated(
    body: Vec<u8>,
    route: &ArmedRoute,
    client_dialect: Dialect,
    ctx: &SessionRouting,
) -> Result<TranslatedResponse> {
    let source: serde_json::Value =
        serde_json::from_slice(&body).context("parse intercepted request body")?;
    let mut canon = translate::parse_request(client_dialect, &source);
    if route.reasoning_echo {
        restore_reasoning(&mut canon, |id| ctx.recall_reasoning(id));
    }
    // Durable pin effort overrides the harness body on pin-routed turns
    // (spec 0165). Catalog-resolved arms leave pin_effort empty so the
    // request body remains the authority.
    if let Some(effort) = route.pin_effort.as_ref() {
        canon.reasoning_effort = Some(effort.clone());
    }
    let kimi_effort = match route.effort {
        // Kimi K3 is always-thinking and accepts its scale separately from
        // the Anthropic `thinking` object. Remove the canonical value so the
        // generic Anthropic emitter does not turn it into a Claude budget.
        super::EffortSupport::Kimi => canon
            .reasoning_effort
            .take()
            .map(|effort| kimi_effort(&effort).to_string()),
        // Never send an unverified effort knob: Codex includes its default
        // on every request, so guessing could reject every routed turn.
        super::EffortSupport::Unsupported => {
            canon.reasoning_effort = None;
            None
        }
        _ => None,
    };
    // Some backends refuse a request whose system prompt does not open with
    // a specific line. Prepending rather than replacing keeps the harness's
    // own instructions intact.
    if let Some(prefix) = route.system_prefix {
        canon.system = Some(match canon.system.take() {
            Some(existing) if existing.starts_with(prefix) => existing,
            Some(existing) => format!("{prefix}\n\n{existing}"),
            None => prefix.to_string(),
        });
    }
    let emitted = translate::emit_request_with_context(route.target_dialect, &canon, &route.model);
    let mut translated = emitted.body;
    if let Some(effort) = kimi_effort {
        apply_kimi_effort(&mut translated, &effort);
    }
    // A target can refuse a parameter its own dialect defines — the Codex
    // backend 400s on `max_output_tokens`. Strip rather than refuse the
    // turn; the alternative is a request the target will never accept.
    if !route.drop_params.is_empty() {
        if let Some(obj) = translated.as_object_mut() {
            for key in route.drop_params {
                obj.remove(*key);
            }
        }
    }
    let url = if route.target_dialect == Dialect::GoogleGemini {
        translate::target_url(
            &route.base_url,
            route.target_dialect,
            &route.model,
            canon.stream,
        )
    } else {
        route.endpoint.clone()
    };
    let mut out = route
        .client
        .post(&url)
        .header("content-type", "application/json");
    // Auth scheme is a property of the target, not of its dialect: the
    // Anthropic subscription backend takes a bearer where the Anthropic API
    // takes a key header. The client's own credential is never forwarded.
    if !route.api_key.is_empty() {
        out = match route.auth {
            TargetAuth::ApiKeyHeader => out
                .header("x-api-key", &route.api_key)
                .header("anthropic-version", "2023-06-01"),
            TargetAuth::GoogleApiKey => out.header("x-goog-api-key", &route.api_key),
            TargetAuth::AzureApiKey => out.header("api-key", &route.api_key),
            TargetAuth::Bearer => out.header("authorization", format!("Bearer {}", route.api_key)),
        };
    }
    for (name, value) in &route.extra_headers {
        out = out.header(name, value);
    }
    let response = out
        .json(&translated)
        .send()
        .await
        .with_context(|| format!("forward to {url}"))?;
    Ok(TranslatedResponse {
        response,
        context: emitted.context,
    })
}

/// Give each replayed assistant turn back the reasoning the target
/// produced for it (spec 0181).
///
/// A harness that speaks a dialect without a reasoning field cannot carry
/// it, so the proxy remembers it instead — keyed by the turn's tool-call
/// ids, which the harness does replay. A turn whose reasoning is no longer
/// remembered gets an empty one: the target accepts that, and an empty
/// echo is the honest statement that we no longer have it. Nothing is
/// invented.
fn restore_reasoning(
    canon: &mut translate::CanonRequest,
    recall: impl Fn(&str) -> Option<String>,
) {
    for message in &mut canon.messages {
        if message.role != translate::CanonRole::Assistant {
            continue;
        }
        // The harness carried it itself — leave its own account alone.
        if message
            .blocks
            .iter()
            .any(|block| matches!(block, translate::CanonBlock::Thinking(_)))
        {
            continue;
        }
        let called = message.blocks.iter().find_map(|block| match block {
            translate::CanonBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        });
        // Only tool-calling turns are refused without it; a plain answer
        // needs no echo and gains nothing from an empty one.
        let Some(id) = called else { continue };
        let reasoning = recall(&id).unwrap_or_default();
        message
            .blocks
            .insert(0, translate::CanonBlock::Thinking(reasoning));
    }
}

/// Map Codex's effort vocabulary onto the K3 scale.
fn kimi_effort(effort: &str) -> &'static str {
    match effort {
        "low" | "minimal" => "low",
        "xhigh" | "max" => "max",
        // Codex can carry a global `medium` even though the K3 catalog does
        // not advertise it; K3's nearest valid level is its default `high`.
        _ => "high",
    }
}

fn apply_kimi_effort(body: &mut serde_json::Value, effort: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("thinking".into(), serde_json::json!({"type":"enabled"}));
        obj.insert(
            "output_config".into(),
            serde_json::json!({"effort":effort}),
        );
    }
}

struct TranslatedResponse {
    response: reqwest::Response,
    context: translate::TranslationContext,
}

/// Does this request want a streamed response?
fn wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Answer a token-counting endpoint locally when the target has no
/// equivalent. Refusing would break the harness's own context
/// bookkeeping; the estimate is explicitly approximate (spec 0116).
fn count_tokens_reply(body: &[u8]) -> String {
    let parsed = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    serde_json::json!({"input_tokens": translate::estimate_tokens(&parsed)}).to_string()
}

/// Stream a translated response back in the client's dialect.
async fn write_translated_response<S>(
    stream: &mut S,
    response: reqwest::Response,
    route: &ArmedRoute,
    client_dialect: Dialect,
    streaming: bool,
    context: &translate::TranslationContext,
    ctx: &SessionRouting,
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use futures::StreamExt;

    let mut capture = ReasoningCapture::new(route.reasoning_echo);

    let status = response.status();
    if !status.is_success() {
        let bytes = tokio::time::timeout(
            ERROR_BODY_TIMEOUT,
            read_bounded_response(response, MAX_ERROR_BODY),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let message = parsed
            .as_ref()
            .and_then(translate::upstream_error_message)
            .unwrap_or_else(|| format!("upstream HTTP {}", status.as_u16()));
        let body = translate::error_body(client_dialect, &message).to_string();
        return write_simple(stream, status.as_u16(), &body).await;
    }

    if !streaming {
        let bytes = match read_bounded_response(response, MAX_RESPONSE_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => {
                let body = translate::error_body(
                    client_dialect,
                    "failed to read bounded upstream response",
                )
                .to_string();
                return write_simple(stream, 502, &body).await;
            }
        };
        let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(parsed) => parsed,
            Err(_) => {
                let body =
                    translate::error_body(client_dialect, "upstream response was not valid JSON")
                        .to_string();
                return write_simple(stream, 502, &body).await;
            }
        };
        if let Some(message) = translate::upstream_error_message(&parsed) {
            let body = translate::error_body(client_dialect, &message).to_string();
            return write_simple(stream, 502, &body).await;
        }
        if let Some(message) = translate::invalid_response_message(route.target_dialect, &parsed) {
            let body = translate::error_body(client_dialect, message).to_string();
            return write_simple(stream, 502, &body).await;
        }
        let events =
            translate::decode_full_response_with_context(route.target_dialect, &parsed, context);
        for event in &events {
            capture.observe(event);
        }
        if let Some((ids, reasoning)) = capture.take() {
            ctx.remember_reasoning(&ids, &reasoning);
        }
        let body =
            translate::encode_full_response(client_dialect, &events, &route.model).to_string();
        return write_simple(stream, 200, &body).await;
    }

    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await
        .context("write response head")?;

    let mut encoder = translate::ClientEncoder::new(client_dialect, &route.model);
    let mut upstream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut saw_frame = false;
    let mut saw_terminal = false;
    let mut failed = false;
    'upstream: while let Some(chunk) = upstream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                // Mid-stream failure: report it inside the stream the
                // client is already reading, then close it cleanly so the
                // turn still terminates.
                let msg = encoder.error(&format!("upstream stream failed: {e}"));
                write_chunk(stream, msg.as_bytes()).await?;
                failed = true;
                break;
            }
        };
        pending.extend_from_slice(&chunk);
        if pending.len() > MAX_SSE_FRAME {
            let msg = encoder.error("upstream SSE data frame exceeded the size limit");
            write_chunk(stream, msg.as_bytes()).await?;
            failed = true;
            break;
        }
        // SSE frames are newline-delimited; hold partial lines back.
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            match process_target_sse_data(
                stream,
                &mut encoder,
                route.target_dialect,
                data.trim(),
                context,
                &mut capture,
            )
            .await?
            {
                SseOutcome::Continue => {}
                SseOutcome::Frame { terminal } => {
                    saw_frame = true;
                    saw_terminal |= terminal;
                }
                SseOutcome::Failed => {
                    failed = true;
                    break 'upstream;
                }
            }
        }
    }

    if !failed && !pending.iter().all(u8::is_ascii_whitespace) {
        let residual = String::from_utf8_lossy(&pending);
        if let Some(data) = residual.trim().strip_prefix("data:") {
            match process_target_sse_data(
                stream,
                &mut encoder,
                route.target_dialect,
                data.trim(),
                context,
                &mut capture,
            )
            .await?
            {
                SseOutcome::Continue => {}
                SseOutcome::Frame { terminal } => {
                    saw_frame = true;
                    saw_terminal |= terminal;
                }
                SseOutcome::Failed => failed = true,
            }
        } else {
            let msg = encoder
                .error("upstream stream ended with an incomplete SSE frame — possible truncation");
            write_chunk(stream, msg.as_bytes()).await?;
            failed = true;
        }
    }

    if !failed && (!saw_frame || !saw_terminal) {
        let msg =
            encoder.error("upstream stream ended without a terminal signal — possible truncation");
        write_chunk(stream, msg.as_bytes()).await?;
        failed = true;
    }
    if !failed {
        // Only a turn that arrived whole is worth remembering: a truncated
        // one is not the reasoning the target will expect back.
        if let Some((ids, reasoning)) = capture.take() {
            ctx.remember_reasoning(&ids, &reasoning);
        }
        let tail = encoder.finish();
        write_chunk(stream, tail.as_bytes()).await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await?;
    stream.shutdown().await.ok();
    Ok(())
}

/// The reasoning and tool-call ids of one response, held until the turn
/// completes and then recorded against the session (spec 0181).
#[derive(Default)]
struct ReasoningCapture {
    armed: bool,
    reasoning: String,
    tool_call_ids: Vec<String>,
}

impl ReasoningCapture {
    fn new(armed: bool) -> Self {
        Self {
            armed,
            ..Self::default()
        }
    }

    fn observe(&mut self, event: &translate::CanonEvent) {
        if !self.armed {
            return;
        }
        match event {
            translate::CanonEvent::ThinkingDelta(delta) => self.reasoning.push_str(delta),
            translate::CanonEvent::ToolStart { id, .. } if !id.is_empty() => {
                if !self.tool_call_ids.iter().any(|known| known == id) {
                    self.tool_call_ids.push(id.clone());
                }
            }
            _ => {}
        }
    }

    /// The completed turn, if it is one worth remembering. A turn with no
    /// tool calls is never replayed as one, and a turn with no reasoning
    /// has nothing to hand back that the empty default does not already
    /// cover.
    fn take(&mut self) -> Option<(Vec<String>, String)> {
        if self.reasoning.is_empty() || self.tool_call_ids.is_empty() {
            return None;
        }
        Some((
            std::mem::take(&mut self.tool_call_ids),
            std::mem::take(&mut self.reasoning),
        ))
    }
}

enum SseOutcome {
    Continue,
    Frame { terminal: bool },
    Failed,
}

async fn process_target_sse_data<S>(
    stream: &mut S,
    encoder: &mut translate::ClientEncoder,
    dialect: Dialect,
    data: &str,
    context: &translate::TranslationContext,
    capture: &mut ReasoningCapture,
) -> Result<SseOutcome>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    if data.is_empty() {
        return Ok(SseOutcome::Continue);
    }
    if translate::is_done_sentinel(data) {
        return Ok(SseOutcome::Frame { terminal: true });
    }
    if data.len() > MAX_SSE_FRAME {
        let msg = encoder.error("upstream SSE data frame exceeded the size limit");
        write_chunk(stream, msg.as_bytes()).await?;
        return Ok(SseOutcome::Failed);
    }
    let value = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => value,
        Err(_) => {
            let msg = encoder.error("malformed upstream SSE data frame");
            write_chunk(stream, msg.as_bytes()).await?;
            return Ok(SseOutcome::Failed);
        }
    };
    if let Some(message) = translate::upstream_error_message(&value) {
        let msg = encoder.error(&message);
        write_chunk(stream, msg.as_bytes()).await?;
        return Ok(SseOutcome::Failed);
    }
    let events = translate::decode_target_event_with_context(dialect, &value, context);
    let terminal = events.iter().any(|event| {
        matches!(event, translate::CanonEvent::Stop { .. })
            || (dialect == Dialect::GoogleGemini
                && matches!(event, translate::CanonEvent::Usage { .. }))
    });
    for event in events {
        capture.observe(&event);
        let out = encoder.push(&event);
        if !out.is_empty() {
            write_chunk(stream, out.as_bytes()).await?;
        }
    }
    Ok(SseOutcome::Frame { terminal })
}

async fn read_bounded_response(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    use futures::StreamExt;

    if response.content_length().is_some_and(|n| n > max_bytes as u64) {
        bail!("upstream response exceeded {max_bytes} bytes");
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.context("read upstream response")?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            bail!("upstream response exceeded {max_bytes} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn write_chunk<S>(stream: &mut S, data: &[u8]) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }
    stream
        .write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    stream.write_all(data).await?;
    stream.write_all(b"\r\n").await?;
    // Model responses stream; holding bytes back stalls the harness's
    // incremental rendering.
    stream.flush().await?;
    Ok(())
}

async fn write_simple<S>(stream: &mut S, status: u16, body: &str) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let head = format!(
        "HTTP/1.1 {status} \r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await.ok();
    Ok(())
}

/// Stream the upstream response back to the client, chunk-framed.
///
/// The connection is closed after the response rather than kept alive:
/// one response per connection removes an entire class of framing bugs,
/// and it is the natural shape for a streamed model turn anyway.
async fn write_response<S>(stream: &mut S, response: reqwest::Response) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use futures::StreamExt;

    let status = response.status();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (name, value) in response.headers() {
        let n = name.as_str();
        if is_dropped_response_header(n) {
            continue;
        }
        head.push_str(&format!(
            "{n}: {}\r\n",
            String::from_utf8_lossy(value.as_bytes())
        ));
    }
    head.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .context("write response head")?;

    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("read upstream body")?;
        if chunk.is_empty() {
            continue;
        }
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        stream.write_all(&chunk).await?;
        stream.write_all(b"\r\n").await?;
        // Model responses stream; holding bytes back would stall the
        // harness's incremental rendering.
        stream.flush().await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await?;
    stream.shutdown().await.ok();
    Ok(())
}

fn is_dropped_response_header(name: &str) -> bool {
    const DROP: &[&str] = &[
        "transfer-encoding",
        "content-length",
        "connection",
        "keep-alive",
        "content-encoding",
    ];
    DROP.iter().any(|d| name.eq_ignore_ascii_case(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_effort_maps_onto_kimi_k3_vocabulary() {
        assert_eq!(kimi_effort("minimal"), "low");
        assert_eq!(kimi_effort("low"), "low");
        assert_eq!(kimi_effort("medium"), "high");
        assert_eq!(kimi_effort("high"), "high");
        assert_eq!(kimi_effort("xhigh"), "max");

        let mut body = serde_json::json!({"model":"k3","messages":[]});
        apply_kimi_effort(&mut body, kimi_effort("xhigh"));
        assert_eq!(body["thinking"], serde_json::json!({"type":"enabled"}));
        assert_eq!(body["output_config"], serde_json::json!({"effort":"max"}));
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn parses_a_connect_authority() {
        let t = parse_connect(b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(t.host, "api.anthropic.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn rejects_non_connect_methods() {
        let err = parse_connect(b"GET http://example.com/ HTTP/1.1\r\n\r\n").unwrap_err();
        assert!(err.to_string().contains("only CONNECT"), "{err}");
    }

    #[test]
    fn substitutes_the_model_field() {
        let body = br#"{"model":"claude-opus-5","max_tokens":16}"#;
        let out = rewrite_body(body, "kimi-k2.5");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "kimi-k2.5");
        assert_eq!(v["max_tokens"], 16);
    }

    /// A body we don't understand is forwarded untouched — a route
    /// redirects an endpoint, it does not edit requests it can't read.
    #[test]
    fn leaves_unparseable_bodies_alone() {
        assert_eq!(rewrite_body(b"not json", "x"), b"not json".to_vec());
        assert_eq!(rewrite_body(b"[1,2]", "x"), b"[1,2]".to_vec());
    }

    /// Adding a `model` the client never sent would change semantics.
    #[test]
    fn does_not_invent_a_model_field() {
        assert_eq!(
            rewrite_body(br#"{"beta":true}"#, "x"),
            br#"{"beta":true}"#.to_vec()
        );
    }

    #[test]
    fn decodes_codex_zstd_requests_before_model_inspection() {
        let body = br#"{"model":"construct-grok-oauth/grok-4.5","input":"ping"}"#;
        let compressed = zstd::stream::encode_all(body.as_slice(), 0).unwrap();
        let mut request = ParsedRequest {
            method: "POST".to_string(),
            path: "/codex/responses".to_string(),
            headers: vec![
                ("content-encoding".to_string(), b"zstd".to_vec()),
                (
                    "content-length".to_string(),
                    compressed.len().to_string().into_bytes(),
                ),
            ],
        };

        let decoded = decode_request_body(&mut request, compressed).unwrap();
        assert_eq!(decoded, body);
        assert!(request.header("content-encoding").is_none());
    }

    /// The client's own credential must never ride along to a different
    /// vendor's endpoint.
    /// A turn whose reasoning is still remembered gets it back verbatim;
    /// one that aged out gets an empty reasoning rather than none, because
    /// a thinking target refuses the turn outright when the field is
    /// missing and nothing may be invented in its place (spec 0181).
    #[test]
    fn a_replayed_tool_turn_gets_its_reasoning_back() {
        let mut canon = translate::CanonRequest {
            messages: vec![
                translate::CanonMessage {
                    role: translate::CanonRole::Assistant,
                    blocks: vec![translate::CanonBlock::ToolUse {
                        id: "call_known".into(),
                        name: "ls".into(),
                        input: serde_json::json!({}),
                    }],
                },
                translate::CanonMessage {
                    role: translate::CanonRole::Assistant,
                    blocks: vec![translate::CanonBlock::ToolUse {
                        id: "call_forgotten".into(),
                        name: "ls".into(),
                        input: serde_json::json!({}),
                    }],
                },
                translate::CanonMessage {
                    role: translate::CanonRole::Assistant,
                    blocks: vec![translate::CanonBlock::Text("done".into())],
                },
            ],
            ..Default::default()
        };
        restore_reasoning(&mut canon, |id| {
            (id == "call_known").then(|| "the root listing first".to_string())
        });
        assert_eq!(
            canon.messages[0].blocks[0],
            translate::CanonBlock::Thinking("the root listing first".into())
        );
        assert_eq!(
            canon.messages[1].blocks[0],
            translate::CanonBlock::Thinking(String::new())
        );
        assert!(
            !canon.messages[2]
                .blocks
                .iter()
                .any(|b| matches!(b, translate::CanonBlock::Thinking(_))),
            "a turn that called no tool is not refused without reasoning"
        );
    }

    /// A harness that carries reasoning itself is the authority on its own
    /// turn — the proxy's memory must not overwrite it.
    #[test]
    fn a_turn_that_carries_its_own_reasoning_is_left_alone() {
        let mut canon = translate::CanonRequest {
            messages: vec![translate::CanonMessage {
                role: translate::CanonRole::Assistant,
                blocks: vec![
                    translate::CanonBlock::Thinking("what the harness kept".into()),
                    translate::CanonBlock::ToolUse {
                        id: "call_known".into(),
                        name: "ls".into(),
                        input: serde_json::json!({}),
                    },
                ],
            }],
            ..Default::default()
        };
        restore_reasoning(&mut canon, |_| Some("what the proxy kept".to_string()));
        assert_eq!(
            canon.messages[0].blocks[0],
            translate::CanonBlock::Thinking("what the harness kept".into())
        );
        assert_eq!(canon.messages[0].blocks.len(), 2);
    }

    #[test]
    fn a_capture_keeps_only_a_reasoned_tool_turn() {
        let mut capture = ReasoningCapture::new(true);
        capture.observe(&translate::CanonEvent::ThinkingDelta("weigh".into()));
        capture.observe(&translate::CanonEvent::ThinkingDelta("ing".into()));
        capture.observe(&translate::CanonEvent::ToolStart {
            index: 0,
            id: "call_1".into(),
            name: "ls".into(),
        });
        capture.observe(&translate::CanonEvent::ToolStart {
            index: 1,
            id: "call_2".into(),
            name: "grep".into(),
        });
        let (ids, reasoning) = capture.take().expect("a reasoned tool turn is kept");
        assert_eq!(ids, vec!["call_1".to_string(), "call_2".to_string()]);
        assert_eq!(reasoning, "weighing");
        assert!(capture.take().is_none(), "a turn is recorded once");

        // Text-only turns are never replayed as tool calls, and a target
        // that reasoned nothing has nothing to hand back.
        let mut text_only = ReasoningCapture::new(true);
        text_only.observe(&translate::CanonEvent::ThinkingDelta("weighing".into()));
        assert!(text_only.take().is_none());

        // A target the flag was never raised for costs nothing to stream.
        let mut disarmed = ReasoningCapture::new(false);
        disarmed.observe(&translate::CanonEvent::ThinkingDelta("weighing".into()));
        disarmed.observe(&translate::CanonEvent::ToolStart {
            index: 0,
            id: "call_1".into(),
            name: "ls".into(),
        });
        assert!(disarmed.take().is_none());
    }

    /// Build a session whose routable host is loopback, so the drain path
    /// can be exercised without DNS.
    fn drainable_ctx(dir: &tempfile::TempDir) -> Arc<SessionRouting> {
        use crate::router::{ca::RouterCa, CaChannel, CaMode, Dialect, HarnessRouting};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(SessionRouting {
            session_id: "s-drain".into(),
            harness_name: "test".into(),
            harness: HarnessRouting {
                dialect: Dialect::AnthropicMessages,
                intercept_hosts: &["127.0.0.1"],
                ca_env: &[CaChannel {
                    var: "NODE_EXTRA_CA_CERTS",
                    mode: CaMode::Additive,
                }],
            },
            ca: Arc::new(RouterCa::load_or_create(&dir.path().join("router")).unwrap()),
            upstream_proxy: None,
            catalog_enabled: std::sync::atomic::AtomicBool::new(false),
            role_models: std::collections::HashSet::new(),
            route: std::sync::RwLock::new(None),
            reasoning: std::sync::RwLock::new(Default::default()),
            route_epoch: std::sync::atomic::AtomicU64::new(0),
            observed: std::sync::atomic::AtomicBool::new(false),
            observed_tx: tx,
        })
    }

    /// Accept one connection and hand back the server side, plus a client
    /// socket already connected to it.
    async fn socket_pair() -> (TcpStream, TcpStream) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let connect = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = l.accept().await.unwrap();
        (connect.await.unwrap(), server)
    }

    /// REGRESSION: arming a route must take effect on a session that is
    /// already running.
    ///
    /// A tunnel decides tunnel-vs-intercept once, at CONNECT. A harness
    /// holding a keep-alive connection would otherwise keep using the old
    /// disposition indefinitely, and the route switch looked like it did
    /// nothing at all.
    #[tokio::test]
    async fn a_route_change_closes_a_quiet_stale_tunnel() {
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"ok").await;
            // Keep-alive: the server holds the connection open.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let ctx = drainable_ctx(&dir);
        let (mut client, server) = socket_pair().await;
        let target = Target {
            host: "127.0.0.1".into(),
            port: origin_port,
        };
        let ctx_bg = ctx.clone();
        tokio::spawn(async move {
            let _ = tunnel(server, &target, None, Some(ctx_bg)).await;
        });

        let mut ack = [0u8; 39];
        client.read_exact(&mut ack).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 2];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ok");

        // Nothing has changed yet, so the tunnel stays up.
        let still_open = tokio::time::timeout(
            std::time::Duration::from_millis(DRAIN_QUIET_MS as u64 + 800),
            async {
                let mut sink = [0u8; 8];
                client.read(&mut sink).await
            },
        )
        .await;
        assert!(still_open.is_err(), "an unchanged route must not close anything");

        // Arming a route makes this tunnel stale.
        ctx.bump_route_epoch();

        let closed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let mut sink = [0u8; 8];
            loop {
                match client.read(&mut sink).await {
                    Ok(0) | Err(_) => return true,
                    Ok(_) => continue,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            closed,
            "a route change must close the stale tunnel, or the switch never \
             applies to a running session"
        );
    }

    /// The other half of spec 0114: a route change must NOT abort a request
    /// already in flight. A connection whose last bytes went client→server
    /// is waiting on an answer — the model may simply be slow to start —
    /// and must stay open.
    #[tokio::test]
    async fn a_route_change_does_not_cut_a_request_in_flight() {
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = origin.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).await;
            // A model that takes a long time to produce its first token.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let ctx = drainable_ctx(&dir);
        let (mut client, server) = socket_pair().await;
        let target = Target {
            host: "127.0.0.1".into(),
            port: origin_port,
        };
        let ctx_bg = ctx.clone();
        tokio::spawn(async move {
            let _ = tunnel(server, &target, None, Some(ctx_bg)).await;
        });

        let mut ack = [0u8; 39];
        client.read_exact(&mut ack).await.unwrap();
        // Send a request and leave it outstanding.
        client.write_all(b"request").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        ctx.bump_route_epoch();

        let cut = tokio::time::timeout(
            std::time::Duration::from_millis(DRAIN_QUIET_MS as u64 + 2_000),
            async {
                let mut sink = [0u8; 8];
                matches!(client.read(&mut sink).await, Ok(0) | Err(_))
            },
        )
        .await;
        assert!(
            cut.is_err(),
            "a request in flight must survive a route change (spec 0114)"
        );
    }

    /// A tunnel to a host this session could never route is not drain
    /// eligible: telemetry and auth connections must not be disturbed by a
    /// route change.
    #[test]
    fn only_routable_hosts_are_drain_eligible() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = drainable_ctx(&dir);
        assert!(ctx.intercepts_host("127.0.0.1"));
        assert!(!ctx.intercepts_host("http-intake.logs.us5.datadoghq.com"));
    }

    #[test]
    fn a_waiting_request_is_never_considered_safe_to_close() {
        let act = Activity::new();
        act.stamp(false); // last bytes went client -> server
        assert!(
            !act.safe_to_close(),
            "an outstanding request must never look closeable, however long it waits"
        );
        act.stamp(true); // server answered
        assert!(!act.safe_to_close(), "still within the quiet window");
    }

    #[test]
    fn drops_client_credentials_and_hop_headers() {
        assert!(is_dropped_header("x-api-key"));
        assert!(is_dropped_header("x-goog-api-key"));
        assert!(is_dropped_header("api-key"));
        assert!(is_dropped_header("Authorization"));
        assert!(is_dropped_header("Host"));
        assert!(is_dropped_header("content-length"));
        assert!(is_dropped_header("proxy-authorization"));
        assert!(is_dropped_header("x-construct-session"));
        assert!(!is_dropped_header("anthropic-version"));
        assert!(!is_dropped_header("content-type"));
    }

    /// Picker inspection is not routing by itself. A native model request
    /// goes back to its named origin with the harness's own provider
    /// credential, while proxy credentials and framing remain local.
    #[test]
    fn native_passthrough_preserves_end_to_end_credentials_only() {
        assert!(!is_hop_header("authorization"));
        assert!(!is_hop_header("x-api-key"));
        assert!(is_hop_header("proxy-authorization"));
        assert!(is_hop_header("x-construct-session"));
        assert!(is_hop_header("connection"));
        assert!(is_hop_header("content-length"));
    }

    #[tokio::test]
    async fn malformed_and_inline_error_frames_fail_closed() {
        use tokio::io::AsyncReadExt;

        for (payload, expected) in [
            ("{not-json", "malformed upstream SSE data frame"),
            (
                r#"{"error":{"message":"Authorization: Bearer secret-token"}}"#,
                "Authorization: Bearer [REDACTED]",
            ),
        ] {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let mut encoder =
                translate::ClientEncoder::new(Dialect::AnthropicMessages, "routed-model");
            let outcome = process_target_sse_data(
                &mut server,
                &mut encoder,
                Dialect::GoogleGemini,
                payload,
                &translate::TranslationContext::default(),
                &mut ReasoningCapture::default(),
            )
            .await
            .unwrap();
            assert!(matches!(outcome, SseOutcome::Failed));
            drop(server);
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            let response = String::from_utf8_lossy(&bytes);
            assert!(response.contains(expected), "{response}");
            assert!(!response.contains("secret-token"), "{response}");
        }
    }

    /// The drain rule in isolation: direction decides safety, not just
    /// elapsed time.
    #[test]
    fn a_connection_awaiting_a_response_is_never_safe_to_close() {
        let act = Activity::new();
        // Client sent a request; the model may take many seconds to start
        // answering. Quiet does NOT mean idle here.
        act.stamp(false);
        act.last_ms.store(now_ms() - 60_000, std::sync::atomic::Ordering::SeqCst);
        assert!(
            !act.safe_to_close(),
            "a request in flight must never be cut (spec 0114)"
        );
    }

    #[test]
    fn a_quiet_connection_after_a_response_is_safe_to_close() {
        let act = Activity::new();
        act.stamp(true);
        act.last_ms.store(
            now_ms() - DRAIN_QUIET_MS - 1,
            std::sync::atomic::Ordering::SeqCst,
        );
        assert!(act.safe_to_close());
    }

    #[test]
    fn a_response_that_just_arrived_is_not_yet_safe_to_close() {
        let act = Activity::new();
        act.stamp(true);
        assert!(
            !act.safe_to_close(),
            "the client may be about to send the next request on this connection"
        );
    }
}
