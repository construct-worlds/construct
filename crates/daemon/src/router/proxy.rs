//! The per-session proxy connection handler (spec 0109).
//!
//! Two paths, and the split between them is the whole safety argument:
//!
//! - **Pass-through** — accept the `CONNECT`, dial the host the client
//!   named, splice the sockets. No TLS termination, no parsing, no header
//!   or credential handling. It cannot alter a request because it never
//!   sees one.
//! - **Intercept** — only for the hosts a *currently armed* route covers.
//!   Terminates TLS with a minted leaf, rewrites the request onto the
//!   route's endpoint, streams the response back.
//!
//! Anything not explicitly being routed — the harness's auth refresh, its
//! telemetry, package registries, whatever a spawned MCP server talks to —
//! takes the pass-through path.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{ArmedRoute, Router, SessionRouting, UpstreamProxy};

/// Cap on a request head. Anthropic-dialect requests carry large bodies
/// but small heads; this only bounds the header block.
const MAX_HEAD: usize = 64 * 1024;

/// Serve one client connection.
///
/// A connection is attributed to a session by the proxy credential it
/// presents. An unattributed connection — no credential, or one we don't
/// know — is tunneled: traffic we cannot place is never routed.
pub async fn serve(mut client: TcpStream, router: Arc<Router>) -> Result<()> {
    let head = read_head(&mut client).await?;
    let target = parse_connect(&head)?;
    let ctx = proxy_credential(&head).and_then(|t| router.session_for_token(&t));

    let armed = ctx
        .as_ref()
        .filter(|c| c.intercepts_host(&target.host))
        .and_then(|c| c.armed_route());

    let Some(route) = armed else {
        let upstream = ctx
            .as_ref()
            .and_then(|c| c.upstream_proxy.clone())
            .or_else(|| router.upstream_proxy().cloned());
        return tunnel(client, &target, upstream.as_ref()).await;
    };

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .context("ack CONNECT")?;
    intercept_tls(client, &target, &route, ctx.expect("route implies a session")).await
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

/// Blind byte splice to the destination the client named.
async fn tunnel(
    mut client: TcpStream,
    target: &Target,
    upstream_proxy: Option<&UpstreamProxy>,
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
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map(|_| ())
        .or_else(|e| match e.kind() {
            // Either side hanging up mid-stream is ordinary for long-lived
            // model connections, not an error worth surfacing.
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => Ok(()),
            _ => Err(e).context("tunnel"),
        })
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
    route: &ArmedRoute,
    ctx: Arc<SessionRouting>,
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

    match forward(&request, body, route).await {
        Ok(response) => {
            ctx.mark_observed();
            write_response(&mut tls, response).await
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
            tls.write_all(head.as_bytes()).await.ok();
            tls.write_all(body.as_bytes()).await.ok();
            tls.shutdown().await.ok();
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
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .context("read request body")?;
    Ok(body)
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

/// Headers we never copy onto the outbound request: hop-by-hop framing,
/// and the client's credential (replaced with the route's).
fn is_dropped_header(name: &str) -> bool {
    const DROP: &[&str] = &[
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "proxy-connection",
        "keep-alive",
        "upgrade",
        "te",
        "trailer",
        "accept-encoding",
        "authorization",
        "x-api-key",
    ];
    DROP.iter().any(|d| name.eq_ignore_ascii_case(d))
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

async fn forward(
    req: &ParsedRequest,
    body: Vec<u8>,
    route: &ArmedRoute,
) -> Result<reqwest::Response> {
    let url = target_url(&route.base_url, &req.path);
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).context("method")?;
    let mut out = route.client.request(method, &url);
    for (name, value) in &req.headers {
        if is_dropped_header(name) {
            continue;
        }
        out = out.header(name, value.clone());
    }
    out = out.header("x-api-key", &route.api_key);
    let body = rewrite_body(&body, &route.model);
    out = out.header("content-length", body.len().to_string()).body(body);
    out.send().await.with_context(|| format!("forward to {url}"))
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
    fn joins_base_url_and_path_without_doubling_slashes() {
        assert_eq!(
            target_url("https://api.moonshot.ai/anthropic/", "/v1/messages"),
            "https://api.moonshot.ai/anthropic/v1/messages"
        );
        assert_eq!(
            target_url("https://api.moonshot.ai/anthropic", "v1/messages"),
            "https://api.moonshot.ai/anthropic/v1/messages"
        );
    }

    /// The client's own credential must never ride along to a different
    /// vendor's endpoint.
    #[test]
    fn drops_client_credentials_and_hop_headers() {
        assert!(is_dropped_header("x-api-key"));
        assert!(is_dropped_header("Authorization"));
        assert!(is_dropped_header("Host"));
        assert!(is_dropped_header("content-length"));
        assert!(!is_dropped_header("anthropic-version"));
        assert!(!is_dropped_header("content-type"));
    }
}
