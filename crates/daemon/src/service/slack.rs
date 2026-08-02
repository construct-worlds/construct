//! Slack Socket Mode adapter for service ingress.
//!
//! Socket Mode is outbound-only: the daemon exchanges the configured app
//! token for a short-lived WebSocket URL, acknowledges each envelope before
//! doing work, then posts the completed answer with the bot token.

use super::ingress::{IngressProgress, IngressRequest, ServiceIngress};
use super::SlackProgress;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const SLACK_API: &str = "https://slack.com/api";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SlackConfig {
    pub(super) app_token: String,
    pub(super) bot_token: String,
    pub(super) allowed_workspaces: Vec<String>,
    pub(super) allowed_channels: Vec<String>,
    pub(super) progress: SlackProgress,
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
    ts: Option<String>,
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

    /// One Slack Web API call, returning the `ts` of whatever it addressed.
    async fn call(&self, token: &str, method: &str, body: serde_json::Value) -> Result<Option<String>> {
        let response = self
            .client
            .post(format!("{}/{method}", self.base_url))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("call Slack {method}"))?
            .error_for_status()
            .with_context(|| format!("Slack {method} HTTP response"))?
            .json::<SlackResponse>()
            .await
            .with_context(|| format!("decode Slack {method} response"))?;
        if response.ok {
            Ok(response.ts)
        } else {
            Err(anyhow!(
                "Slack rejected {method}: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            ))
        }
    }

    async fn post_message(
        &self,
        token: &str,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<Option<String>> {
        self.call(
            token,
            "chat.postMessage",
            serde_json::json!({
                "channel": channel,
                "thread_ts": thread_ts,
                "text": text,
            }),
        )
        .await
    }

    async fn update_message(
        &self,
        token: &str,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<Option<String>> {
        self.call(
            token,
            "chat.update",
            serde_json::json!({ "channel": channel, "ts": ts, "text": text }),
        )
        .await
    }

    /// Reactions need the `reactions:write` scope. An app that was installed
    /// before the operator selected the progress affordance will not have it,
    /// so callers treat a failure here as cosmetic and answer anyway.
    async fn set_reaction(
        &self,
        token: &str,
        method: &str,
        channel: &str,
        ts: &str,
        name: &str,
    ) -> Result<Option<String>> {
        self.call(
            token,
            method,
            serde_json::json!({ "channel": channel, "timestamp": ts, "name": name }),
        )
        .await
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

/// How long a turn may run before the channel admits it is still working.
///
/// Most turns answer well inside this, and announcing those would put a
/// placeholder on every message for no benefit. The affordance is for the wait
/// that has already become long enough to look like a dropped request.
const PROGRESS_AFTER: std::time::Duration = std::time::Duration::from_secs(8);

const WORKING_EMOJI: &str = "eyes";
const ANSWERED_EMOJI: &str = "white_check_mark";
const FAILED_EMOJI: &str = "warning";

fn progress_text(progress: &IngressProgress) -> String {
    match progress {
        IngressProgress::Working => "_Working on it…_".to_string(),
        // Say who has to act. A turn stopped here will not move on its own,
        // and the person waiting in Slack cannot see the approval prompt.
        IngressProgress::AwaitingApproval { tool, summary } if summary.is_empty() => {
            format!("_Waiting for an operator to approve `{tool}`._")
        }
        IngressProgress::AwaitingApproval { tool, summary } => {
            format!("_Waiting for an operator to approve `{tool}`: {summary}_")
        }
    }
}

/// What the affordance left behind in Slack, so the answer can replace it.
#[derive(Default)]
struct Affordance {
    placeholder_ts: Option<String>,
    reacted: bool,
}

/// Show, and keep current, the "still working" affordance for one delivery.
///
/// Returns once cancelled — which the caller does as soon as the turn
/// resolves — handing back whatever it put in the channel.
async fn run_affordance(
    api: SlackApi,
    config: SlackConfig,
    channel: String,
    thread_ts: String,
    message_ts: String,
    after: std::time::Duration,
    mut progress: watch::Receiver<IngressProgress>,
    cancel: CancellationToken,
) -> Affordance {
    let mut state = Affordance::default();
    if config.progress == SlackProgress::Off {
        return state;
    }
    tokio::select! {
        _ = cancel.cancelled() => return state,
        _ = tokio::time::sleep(after) => {}
    }
    if config.progress.reacts() {
        match api
            .set_reaction(
                &config.bot_token,
                "reactions.add",
                &channel,
                &message_ts,
                WORKING_EMOJI,
            )
            .await
        {
            Ok(_) => state.reacted = true,
            Err(error) => tracing::warn!(
                %error,
                "Slack progress reaction failed; the answer is unaffected \
                 (does the app have the reactions:write scope?)"
            ),
        }
    }
    if config.progress.posts_placeholder() {
        let text = progress_text(&progress.borrow_and_update().clone());
        match api
            .post_message(&config.bot_token, &channel, &thread_ts, &text)
            .await
        {
            Ok(ts) => state.placeholder_ts = ts,
            Err(error) => tracing::warn!(%error, "Slack progress placeholder failed"),
        }
    }
    // Keep the placeholder honest: a turn that stops at an approval must stop
    // claiming it is working.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return state,
            changed = progress.changed() => {
                if changed.is_err() {
                    return state;
                }
                let text = progress_text(&progress.borrow_and_update().clone());
                let Some(ts) = state.placeholder_ts.as_deref() else {
                    continue;
                };
                if let Err(error) =
                    api.update_message(&config.bot_token, &channel, ts, &text).await
                {
                    tracing::warn!(%error, "Slack progress update failed");
                }
            }
        }
    }
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
            message: delivery.text.clone(),
            session_key: Some(format!(
                "{}:{}:{}",
                delivery.team_id, delivery.channel, delivery.thread_ts
            )),
            request_id: Some(delivery.event_id.clone()),
        })
        .await?;
    let (progress_tx, progress_rx) = watch::channel(IngressProgress::default());
    let affordance_cancel = CancellationToken::new();
    let affordance = tokio::spawn(run_affordance(
        api.clone(),
        config.clone(),
        delivery.channel.clone(),
        delivery.thread_ts.clone(),
        delivery.message_ts.clone(),
        PROGRESS_AFTER,
        progress_rx,
        affordance_cancel.clone(),
    ));

    let reply = ingress.wait_for_final(&receipt, cancel, &progress_tx).await;
    affordance_cancel.cancel();
    let affordance = affordance.await.unwrap_or_default();

    // A turn that failed used to leave the thread silent forever. Now that
    // something in the channel says "working", it must not keep saying that.
    let text = match &reply {
        Ok(reply) => reply.clone(),
        Err(error) => format!("_The turn ended without an answer: {error}_"),
    };
    if cancel.is_cancelled() {
        return Err(anyhow!("channel stopped"));
    }
    match affordance.placeholder_ts.as_deref() {
        Some(ts) => {
            api.update_message(&config.bot_token, &delivery.channel, ts, &text)
                .await?;
        }
        None => {
            api.post_message(
                &config.bot_token,
                &delivery.channel,
                &delivery.thread_ts,
                &text,
            )
            .await?;
        }
    }
    if affordance.reacted {
        settle_reaction(api, config, &delivery, reply.is_ok()).await;
    }
    reply.map(|_| ())
}

/// Swap the working reaction for the outcome. Cosmetic: a workspace that
/// denies the scope mid-turn should not turn a delivered answer into a failure.
async fn settle_reaction(
    api: &SlackApi,
    config: &SlackConfig,
    delivery: &SlackDelivery,
    answered: bool,
) {
    let _ = api
        .set_reaction(
            &config.bot_token,
            "reactions.remove",
            &delivery.channel,
            &delivery.message_ts,
            WORKING_EMOJI,
        )
        .await;
    let settled = if answered {
        ANSWERED_EMOJI
    } else {
        FAILED_EMOJI
    };
    if let Err(error) = api
        .set_reaction(
            &config.bot_token,
            "reactions.add",
            &delivery.channel,
            &delivery.message_ts,
            settled,
        )
        .await
    {
        tracing::warn!(%error, "Slack outcome reaction failed");
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
    /// The message that triggered this turn. Distinct from `thread_ts` for a
    /// reply inside a thread, and it is this one a reaction belongs on.
    message_ts: String,
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
        thread_ts: event.thread_ts.unwrap_or_else(|| ts.clone()),
        message_ts: ts,
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
            progress: SlackProgress::default(),
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
                message_ts: "123.45".into(),
                text: "deploy status".into(),
            })
        );
    }

    #[test]
    fn a_thread_reply_reacts_on_itself_not_on_the_thread_root() {
        // thread_ts routes the conversation; message_ts is the message a
        // reaction belongs on. Conflating them would stack every reaction of a
        // long thread onto its first message.
        let envelope = serde_json::from_value::<SocketEnvelope>(serde_json::json!({
            "payload": {
                "team_id": "T1", "event_id": "Ev2",
                "event": {
                    "type": "app_mention", "channel": "C1", "user": "U1",
                    "text": "<@UBOT> and now?", "ts": "222.22", "thread_ts": "111.11"
                }
            }
        }))
        .unwrap();
        let delivery = delivery_from_envelope(envelope, &config()).unwrap();
        assert_eq!(delivery.thread_ts, "111.11");
        assert_eq!(delivery.message_ts, "222.22");
    }

    #[test]
    fn progress_says_who_has_to_act_when_a_turn_stops_at_an_approval() {
        // "Working on it" is a lie once the turn is parked on an approval:
        // nothing moves until a human at the TUI acts, and the person waiting
        // in Slack cannot see that prompt.
        assert_eq!(
            progress_text(&IngressProgress::Working),
            "_Working on it…_"
        );
        assert_eq!(
            progress_text(&IngressProgress::AwaitingApproval {
                tool: "bash".into(),
                summary: "cargo test".into(),
            }),
            "_Waiting for an operator to approve `bash`: cargo test_"
        );
        assert_eq!(
            progress_text(&IngressProgress::AwaitingApproval {
                tool: "bash".into(),
                summary: String::new(),
            }),
            "_Waiting for an operator to approve `bash`._"
        );
    }

    #[test]
    fn progress_modes_select_their_affordances() {
        assert!(!SlackProgress::Off.posts_placeholder() && !SlackProgress::Off.reacts());
        assert!(SlackProgress::Placeholder.posts_placeholder());
        assert!(!SlackProgress::Placeholder.reacts());
        assert!(SlackProgress::Reaction.reacts());
        assert!(!SlackProgress::Reaction.posts_placeholder());
        assert!(SlackProgress::Both.posts_placeholder() && SlackProgress::Both.reacts());
    }

    #[tokio::test]
    async fn a_turn_that_answers_quickly_leaves_no_progress_message() {
        // The affordance is for a wait long enough to look dropped. Cancelling
        // before the delay elapses — which is what a fast turn does — must
        // leave the thread untouched, with no Slack call attempted at all.
        let api = SlackApi {
            client: reqwest::Client::new(),
            // Any call would try to reach this and fail the test by erroring.
            base_url: "http://127.0.0.1:1".to_string(),
        };
        let (_tx, rx) = watch::channel(IngressProgress::default());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let state = run_affordance(
            api,
            config(),
            "C1".into(),
            "1.1".into(),
            "1.1".into(),
            PROGRESS_AFTER,
            rx,
            cancel,
        )
        .await;

        assert!(state.placeholder_ts.is_none());
        assert!(!state.reacted);
    }

    #[tokio::test]
    async fn the_off_mode_never_touches_the_channel() {
        let api = SlackApi {
            client: reqwest::Client::new(),
            base_url: "http://127.0.0.1:1".to_string(),
        };
        let mut config = config();
        config.progress = SlackProgress::Off;
        let (_tx, rx) = watch::channel(IngressProgress::default());

        // Not cancelled: Off must return immediately on its own, without even
        // waiting out the delay.
        let state = run_affordance(
            api,
            config,
            "C1".into(),
            "1.1".into(),
            "1.1".into(),
            PROGRESS_AFTER,
            rx,
            CancellationToken::new(),
        )
        .await;

        assert!(state.placeholder_ts.is_none());
        assert!(!state.reacted);
    }

    /// Stub Slack Web API: answers every method `ok` and records the calls.
    async fn stub_slack() -> (SlackApi, Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = calls.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        let Some(end) = request
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|index| index + 4)
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() < end + length {
                            continue;
                        }
                        let method = head
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .trim_start_matches('/')
                            .to_string();
                        let body: serde_json::Value =
                            serde_json::from_slice(&request[end..end + length])
                                .unwrap_or(serde_json::Value::Null);
                        recorder.lock().unwrap().push((method, body));
                        let payload = br#"{"ok":true,"ts":"P1"}"#;
                        let _ = stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                    payload.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = stream.write_all(payload).await;
                        return;
                    }
                });
            }
        });
        (
            SlackApi {
                client: reqwest::Client::new(),
                base_url: format!("http://{address}"),
            },
            calls,
        )
    }

    #[tokio::test]
    async fn a_slow_turn_announces_itself_then_says_what_it_is_blocked_on() {
        let (api, calls) = stub_slack().await;
        let mut config = config();
        config.progress = SlackProgress::Both;
        let (tx, rx) = watch::channel(IngressProgress::default());
        let cancel = CancellationToken::new();

        let affordance = tokio::spawn(run_affordance(
            api,
            config,
            "C1".into(),
            "111.11".into(),
            "222.22".into(),
            std::time::Duration::from_millis(10),
            rx,
            cancel.clone(),
        ));

        // Wait for the placeholder, then park the turn on an approval.
        let posted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(method, _)| method == "chat.postMessage")
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(posted.is_ok(), "the placeholder was never posted");

        tx.send(IngressProgress::AwaitingApproval {
            tool: "bash".into(),
            summary: "rm -rf build".into(),
        })
        .unwrap();
        let updated = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(method, _)| method == "chat.update")
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(updated.is_ok(), "the approval was never surfaced");

        cancel.cancel();
        let state = affordance.await.unwrap();
        assert_eq!(state.placeholder_ts.as_deref(), Some("P1"));
        assert!(state.reacted);

        let calls = calls.lock().unwrap();
        let by = |name: &str| {
            calls
                .iter()
                .find(|(method, _)| method == name)
                .map(|(_, body)| body.clone())
                .unwrap_or(serde_json::Value::Null)
        };
        // The reaction goes on the triggering message, the placeholder into
        // the thread — two different timestamps.
        assert_eq!(by("reactions.add")["timestamp"], "222.22");
        assert_eq!(by("reactions.add")["name"], WORKING_EMOJI);
        assert_eq!(by("chat.postMessage")["thread_ts"], "111.11");
        assert_eq!(by("chat.postMessage")["text"], "_Working on it…_");
        // And the placeholder stops claiming to be working once it isn't.
        assert_eq!(by("chat.update")["ts"], "P1");
        assert_eq!(
            by("chat.update")["text"],
            "_Waiting for an operator to approve `bash`: rm -rf build_"
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
