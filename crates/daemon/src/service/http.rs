//! Loopback HTTP adapter for service ingress.

use super::ingress::{IngressRequest, ServiceIngress};
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const MAX_HTTP_BYTES: usize = 1024 * 1024;

struct HttpChannel {
    ingress: Arc<ServiceIngress>,
}

pub(super) async fn serve(
    ingress: Arc<ServiceIngress>,
    listener: TcpListener,
    cancel: CancellationToken,
) -> Result<()> {
    let runtime = Arc::new(HttpChannel { ingress });
    let port = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(0);
    tracing::info!(
        service = %runtime.ingress.service_name(),
        channel = %runtime.ingress.channel_id(),
        port,
        "service http endpoint ready (loopback only)"
    );
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!(
                    service = %runtime.ingress.service_name(),
                    channel = %runtime.ingress.channel_id(),
                    port,
                    "service http endpoint released"
                );
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle(stream, runtime).await {
                        tracing::debug!(%error, "service request failed");
                    }
                });
            }
        }
    }
}

/// The credential this HTTP channel currently accepts. Read per request so a
/// rotation takes effect without rebinding its listener.
pub(super) fn token(ingress: &ServiceIngress) -> Option<String> {
    ingress
        .current_config()
        .channels
        .get(ingress.channel_id())
        .and_then(|channel| channel.token.clone())
        .filter(|token| !token.is_empty())
}

/// Recheck pause/enable state per request to cover the interval before the
/// supervisor releases a listener after a definition reload.
pub(super) fn serving(ingress: &ServiceIngress) -> bool {
    let config = ingress.current_config();
    !config.paused
        && config
            .channels
            .get(ingress.channel_id())
            .is_some_and(|channel| channel.enabled)
}

async fn handle(mut stream: TcpStream, runtime: Arc<HttpChannel>) -> Result<()> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > MAX_HTTP_BYTES {
            return respond(&mut stream, 413, "request too large").await;
        }
        if let Some(end) = find_headers_end(&bytes) {
            let length = content_length(&bytes[..end])?;
            while bytes.len() < end + length {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return respond(&mut stream, 400, "truncated request").await;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            break;
        }
    }
    let end = find_headers_end(&bytes).unwrap();
    let headers = std::str::from_utf8(&bytes[..end]).map_err(|_| anyhow!("invalid headers"))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let route = match parse_http_route(runtime.ingress.service_name(), request_line) {
        Ok(route) => route,
        Err((status, message)) => return respond(&mut stream, status, message).await,
    };
    let expected = token(&runtime.ingress);
    let authorized = expected.is_some_and(|token| {
        lines
            .filter_map(|line| line.split_once(':'))
            .any(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    && value.trim() == format!("Bearer {token}")
            })
    });
    if !authorized {
        return respond(&mut stream, 401, "unauthorized").await;
    }
    // Check this after authentication so callers cannot use 401 versus 503 to
    // discover which services exist.
    if !serving(&runtime.ingress) {
        return respond(&mut stream, 503, "service paused").await;
    }
    match route {
        HttpRoute::Submit => {
            let result = match serde_json::from_slice::<IngressRequest>(&bytes[end..]) {
                Ok(request) => runtime.ingress.submit(request).await,
                Err(_) => Err(anyhow!("invalid JSON")),
            };
            match result {
                Ok(session) => {
                    json_response(
                        &mut stream,
                        202,
                        &serde_json::json!({
                            "accepted": true,
                            "service": runtime.ingress.service_name(),
                            "channel": runtime.ingress.channel_id(),
                            "session": session,
                        }),
                    )
                    .await
                }
                Err(error) => respond(&mut stream, 400, &error.to_string()).await,
            }
        }
        HttpRoute::Session(session_id) => {
            match runtime.ingress.session_result(&session_id).await? {
                Some(result) => json_response(&mut stream, 200, &result).await,
                None => respond(&mut stream, 404, "session not found").await,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HttpRoute {
    Submit,
    Session(String),
}

pub(super) fn parse_http_route(
    service_name: &str,
    request_line: &str,
) -> std::result::Result<HttpRoute, (u16, &'static str)> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if parts.next().is_some() || !version.starts_with("HTTP/") {
        return Err((400, "invalid request line"));
    }
    let submit = format!("/svc/{service_name}");
    if target == submit {
        return if method == "POST" {
            Ok(HttpRoute::Submit)
        } else {
            Err((405, "POST required"))
        };
    }
    let session_prefix = format!("{submit}/sessions/");
    if let Some(session_id) = target.strip_prefix(&session_prefix) {
        if session_id.is_empty() || session_id.contains('/') {
            return Err((404, "not found"));
        }
        return if method == "GET" {
            Ok(HttpRoute::Session(session_id.to_string()))
        } else {
            Err((405, "GET required"))
        };
    }
    Err((404, "not found"))
}

pub(super) fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|x| x == b"\r\n\r\n")
        .map(|i| i + 4)
}

pub(super) fn content_length(headers: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(headers)?;
    Ok(text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
        })
        .unwrap_or(0))
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    json_response(stream, status, &serde_json::json!({"error": message})).await
}

async fn json_response(stream: &mut TcpStream, status: u16, value: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.write_all(&body).await?;
    Ok(())
}
