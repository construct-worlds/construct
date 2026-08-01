//! Slack Socket Mode adapter for service ingress.
//!
//! Socket Mode is outbound-only: the daemon exchanges the configured app
//! token for a short-lived WebSocket URL, acknowledges each envelope before
//! doing work, then posts the completed answer with the bot token.

use super::ingress::{IngressRequest, ServiceIngress};
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const SLACK_API: &str = "https://slack.com/api";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SlackConfig {
    pub(super) app_token: String,
    pub(super) bot_token: String,
    pub(super) allowed_workspaces: Vec<String>,
    pub(super) allowed_channels: Vec<String>,
}

#[derive(Clone)]
struct SlackApi {
    client: reqwest::Client,
    base_url: String,
}

impl Default for SlackApi {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("Slack HTTP client"),
            base_url: SLACK_API.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl SlackApi {
    async fn open_socket(&self, token: &str) -> Result<String> {
        let response = self
            .client
            .post(format!("{}/apps.connections.open", self.base_url))
            .bearer_auth(token)
            .send()
            .await
            .context("open Slack Socket Mode connection")?
            .error_for_status()
            .context("Slack Socket Mode HTTP response")?
            .json::<SlackResponse>()
            .await
            .context("decode Slack Socket Mode response")?;
        if !response.ok {
            return Err(anyhow!(
                "Slack rejected Socket Mode connection: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        response
            .url
            .ok_or_else(|| anyhow!("Slack response omitted WebSocket URL"))
    }

    async fn post_message(
        &self,
        token: &str,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<()> {
        let response = self
            .client
            .post(format!("{}/chat.postMessage", self.base_url))
            .bearer_auth(token)
            .json(&serde_json::json!({
                "channel": channel,
                "thread_ts": thread_ts,
                "text": text,
            }))
            .send()
            .await
            .context("post Slack reply")?
            .error_for_status()
            .context("Slack reply HTTP response")?
            .json::<SlackResponse>()
            .await
            .context("decode Slack reply response")?;
        if response.ok {
            Ok(())
        } else {
            Err(anyhow!(
                "Slack rejected reply: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            ))
        }
    }
}

pub(super) async fn serve(
    ingress: Arc<ServiceIngress>,
    config: SlackConfig,
    cancel: CancellationToken,
) -> Result<()> {
    serve_with_api(ingress, config, cancel, SlackApi::default()).await
}

async fn serve_with_api(
    ingress: Arc<ServiceIngress>,
    config: SlackConfig,
    cancel: CancellationToken,
    api: SlackApi,
) -> Result<()> {
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let result = run_connection(&ingress, &config, &cancel, &api).await;
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = result {
            tracing::warn!(
                service = %ingress.service_name(),
                channel = %ingress.channel_id(),
                %error,
                retry_seconds = backoff.as_secs(),
                "Slack channel disconnected; retrying"
            );
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

async fn run_connection(
    ingress: &Arc<ServiceIngress>,
    config: &SlackConfig,
    cancel: &CancellationToken,
    api: &SlackApi,
) -> Result<()> {
    let url = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = api.open_socket(&config.app_token) => result?,
    };
    let (socket, _) = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = tokio_tungstenite::connect_async(&url) => {
            result.context("connect Slack Socket Mode WebSocket")?
        }
    };
    let (mut sink, mut stream) = socket.split();
    tracing::info!(
        service = %ingress.service_name(),
        channel = %ingress.channel_id(),
        "Slack service channel connected"
    );
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.close().await;
                return Ok(());
            }
            message = stream.next() => message,
        };
        let Some(message) = message else {
            return Err(anyhow!("Slack WebSocket closed"));
        };
        let message = message.context("read Slack WebSocket")?;
        let text = match message {
            Message::Text(text) => text,
            Message::Ping(payload) => {
                sink.send(Message::Pong(payload))
                    .await
                    .context("reply to Slack WebSocket ping")?;
                continue;
            }
            Message::Close(_) => return Err(anyhow!("Slack WebSocket closed")),
            _ => continue,
        };
        let Ok(envelope) = serde_json::from_str::<SocketEnvelope>(&text) else {
            continue;
        };
        if let Some(envelope_id) = envelope.envelope_id.as_deref() {
            acknowledge(&mut sink, envelope_id).await?;
        }
        let Some(delivery) = delivery_from_envelope(envelope, config) else {
            continue;
        };
        let ingress = ingress.clone();
        let config = config.clone();
        let cancel = cancel.clone();
        let api = api.clone();
        tokio::spawn(async move {
            if let Err(error) = process_delivery(&ingress, &config, &cancel, &api, delivery).await {
                tracing::warn!(
                    service = %ingress.service_name(),
                    channel = %ingress.channel_id(),
                    %error,
                    "Slack delivery failed"
                );
            }
        });
    }
}

async fn acknowledge<S>(sink: &mut S, envelope_id: &str) -> Result<()>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    sink.send(Message::Text(
        serde_json::json!({"envelope_id": envelope_id})
            .to_string()
            .into(),
    ))
    .await
    .context("acknowledge Slack envelope")
}

async fn process_delivery(
    ingress: &ServiceIngress,
    config: &SlackConfig,
    cancel: &CancellationToken,
    api: &SlackApi,
    delivery: SlackDelivery,
) -> Result<()> {
    let receipt = ingress
        .submit_tracked(IngressRequest {
            message: delivery.text,
            session_key: Some(format!(
                "{}:{}:{}",
                delivery.team_id, delivery.channel, delivery.thread_ts
            )),
            request_id: Some(delivery.event_id),
        })
        .await?;
    let reply = ingress.wait_for_final(&receipt, cancel).await?;
    tokio::select! {
        _ = cancel.cancelled() => Err(anyhow!("channel stopped")),
        result = api.post_message(
            &config.bot_token,
            &delivery.channel,
            &delivery.thread_ts,
            &reply,
        ) => result,
    }
}

#[derive(Debug, Deserialize)]
struct SocketEnvelope {
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<EventsPayload>,
}

#[derive(Debug, Deserialize)]
struct EventsPayload {
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    channel_type: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SlackDelivery {
    event_id: String,
    team_id: String,
    channel: String,
    thread_ts: String,
    text: String,
}

fn delivery_from_envelope(envelope: SocketEnvelope, config: &SlackConfig) -> Option<SlackDelivery> {
    let payload = envelope.payload?;
    let event = payload.event?;
    let accepted_kind = event.kind == "app_mention"
        || (event.kind == "message" && event.channel_type.as_deref() == Some("im"));
    if !accepted_kind || event.user.is_none() || event.bot_id.is_some() || event.subtype.is_some() {
        return None;
    }
    let team_id = payload.team_id?;
    let channel = event.channel?;
    if !config.allowed_workspaces.is_empty()
        && !config.allowed_workspaces.iter().any(|id| id == &team_id)
    {
        return None;
    }
    if !config.allowed_channels.is_empty()
        && !config.allowed_channels.iter().any(|id| id == &channel)
    {
        return None;
    }
    let ts = event.ts?;
    let text = strip_leading_mentions(event.text?.trim());
    if text.is_empty() {
        return None;
    }
    Some(SlackDelivery {
        event_id: payload.event_id?,
        team_id,
        channel,
        thread_ts: event.thread_ts.unwrap_or(ts),
        text,
    })
}

fn strip_leading_mentions(mut text: &str) -> String {
    loop {
        let Some(rest) = text.strip_prefix("<@") else {
            break;
        };
        let Some(end) = rest.find('>') else { break };
        text = rest[end + 1..].trim_start();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config() -> SlackConfig {
        SlackConfig {
            app_token: "xapp-test".into(),
            bot_token: "xoxb-test".into(),
            allowed_workspaces: vec!["T1".into()],
            allowed_channels: vec!["C1".into()],
        }
    }

    #[test]
    fn mention_maps_to_thread_route_and_strips_address() {
        let envelope = serde_json::from_value::<SocketEnvelope>(serde_json::json!({
            "envelope_id": "E1",
            "payload": {
                "team_id": "T1",
                "event_id": "Ev1",
                "event": {
                    "type": "app_mention",
                    "channel": "C1",
                    "user": "U1",
                    "text": "<@UBOT> deploy status",
                    "ts": "123.45"
                }
            }
        }))
        .unwrap();
        assert_eq!(
            delivery_from_envelope(envelope, &config()),
            Some(SlackDelivery {
                event_id: "Ev1".into(),
                team_id: "T1".into(),
                channel: "C1".into(),
                thread_ts: "123.45".into(),
                text: "deploy status".into(),
            })
        );
    }

    #[test]
    fn direct_messages_are_accepted_but_bots_and_unlisted_channels_are_not() {
        let event = |channel: &str, bot: bool| {
            serde_json::from_value::<SocketEnvelope>(serde_json::json!({
                "payload": {
                    "team_id": "T1", "event_id": "Ev1",
                    "event": {
                        "type": "message", "channel_type": "im", "channel": channel,
                        "user": "U1", "text": "hello", "ts": "1",
                        "bot_id": bot.then_some("B1")
                    }
                }
            }))
            .unwrap()
        };
        assert!(delivery_from_envelope(event("C1", false), &config()).is_some());
        assert!(delivery_from_envelope(event("C2", false), &config()).is_none());
        assert!(delivery_from_envelope(event("C1", true), &config()).is_none());
    }

    #[tokio::test]
    async fn slack_api_uses_bearer_auth_and_thread_reply_shape() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    if request.len() >= header_end + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /chat.postMessage HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer xoxb-secret"));
            assert!(request.contains("\"channel\":\"C1\""));
            assert!(request.contains("\"thread_ts\":\"123.4\""));
            let body = br#"{"ok":true}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let api = SlackApi {
            client: reqwest::Client::new(),
            base_url: format!("http://{address}"),
        };
        api.post_message("xoxb-secret", "C1", "123.4", "done")
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socket_open_uses_the_app_token_without_putting_it_in_the_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /apps.connections.open HTTP/1.1"));
            assert!(!request.lines().next().unwrap().contains("xapp-secret"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer xapp-secret"));
            let body = br#"{"ok":true,"url":"ws://127.0.0.1/socket"}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        });
        let api = SlackApi {
            client: reqwest::Client::new(),
            base_url: format!("http://{address}"),
        };
        assert_eq!(
            api.open_socket("xapp-secret").await.unwrap(),
            "ws://127.0.0.1/socket"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socket_envelope_ack_is_sent_on_the_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(r#"{"envelope_id":"E123"}"#.into()))
                .await
                .unwrap();
            let reply = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&reply).unwrap(),
                serde_json::json!({"envelope_id":"E123"})
            );
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let (mut sink, mut stream) = socket.split();
        let envelope = stream.next().await.unwrap().unwrap().into_text().unwrap();
        let envelope: SocketEnvelope = serde_json::from_str(&envelope).unwrap();
        acknowledge(&mut sink, envelope.envelope_id.as_deref().unwrap())
            .await
            .unwrap();
        server.await.unwrap();
    }
}
