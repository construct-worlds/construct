//! slack-personal adapter: the user's own Slack account as an operator
//! channel (specs 0201, 0202).
//!
//! Unlike the Socket Mode bot channel, nothing here talks to Slack directly.
//! The channel drives a user-configured MCP backend through [`super::mcp`]
//! with plain daemon logic — no agent, no model turn — and simulates an event
//! subscription by polling.
//!
//! ## The tool contract
//!
//! A conforming backend exposes these tools, each answering with one text
//! content block whose body is JSON:
//!
//! - `slack_sweep_messages {after_ts}` →
//!   `{"messages": [{"workspace", "channel", "ts", "thread_ts"?, "user",
//!     "text", "is_dm", "is_self", "is_self_dm"}]}` — every message newer
//!   than `after_ts` the account can see, thread replies included.
//!   `is_self` marks the account owner's own messages; `is_self_dm` marks the
//!   owner's DM with themself.
//! - `slack_read_thread {channel, thread_ts, limit}` →
//!   `{"messages": [{"user", "text", "ts", "is_self"}]}` oldest first.
//! - `slack_send_message {channel, thread_ts, text}` → `{"ts"}`.
//! - `slack_create_draft {channel, thread_ts, text}` → `{}`.
//!
//! ## Identity rules
//!
//! Everything this channel posts appears as the user, so it never posts
//! progress placeholders, failure notices, or anything else the response mode
//! did not explicitly produce. Its own posts are excluded from ingress by
//! recorded timestamp, never by author — the user's own messages are
//! legitimate triggers (their DM with themself is a private command line).

use super::ingress::{IngressProgress, IngressReceipt, IngressRequest, OperatorIngress, PENDING_DELIVERY_TTL};
use super::mcp::McpClient;
use super::slack::{thread_context_block, SlackHistoryMessage};
use super::{SlackPersonalResponse, SlackPersonalTrigger};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SlackPersonalConfig {
    pub(super) mcp_command: String,
    pub(super) idle_poll_ceiling: std::time::Duration,
    pub(super) trigger: SlackPersonalTrigger,
    pub(super) default_response: SlackPersonalResponse,
    pub(super) response_overrides: BTreeMap<String, SlackPersonalResponse>,
    pub(super) auto_after: std::time::Duration,
    pub(super) disclosure: bool,
    pub(super) allowed_workspaces: Vec<String>,
    pub(super) allowed_channels: Vec<String>,
    pub(super) thread_context: usize,
}

impl SlackPersonalConfig {
    fn response_for_channel(&self, channel: &str) -> SlackPersonalResponse {
        self.response_overrides
            .get(channel)
            .copied()
            .unwrap_or(self.default_response)
    }
}

/// Appended to every auto-sent reply unless the user turned disclosure off.
/// The channel speaks as the user; recipients deserve to know when it was the
/// agent talking (spec 0202).
const DISCLOSURE_SUFFIX: &str = "\n\n_\u{1F916} sent by an agent_";

/// Bounded polling cadence for a connected backend.
///
/// A newly connected channel has no evidence that Slack is active, so it
/// starts at the configured idle ceiling. Accepted ingress resets the next
/// wait to the global backend-safe floor; every idle sweep doubles that wait
/// until it reaches the ceiling. Traffic outside this channel's accepted
/// scope does not keep the backend on its hottest cadence.
struct PollSchedule {
    active_floor: std::time::Duration,
    idle_ceiling: std::time::Duration,
    next_interval: std::time::Duration,
}

impl PollSchedule {
    fn new(idle_ceiling: std::time::Duration) -> Self {
        let active_floor =
            std::time::Duration::from_secs(construct_protocol::SLACK_PERSONAL_POLL_MIN_SECS);
        let idle_ceiling = idle_ceiling.max(active_floor);
        Self {
            active_floor,
            idle_ceiling,
            next_interval: idle_ceiling,
        }
    }

    fn next_interval(&self) -> std::time::Duration {
        self.next_interval
    }

    fn after_sweep(&mut self, accepted_activity: bool) {
        self.next_interval = if accepted_activity {
            self.active_floor
        } else {
            self.next_interval.saturating_mul(2).min(self.idle_ceiling)
        };
    }
}

/// One message returned by a sweep. Unknown fields are ignored so a backend
/// can carry extras; missing booleans default to the conservative reading.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct SweptMessage {
    #[serde(default)]
    pub(super) workspace: String,
    pub(super) channel: String,
    pub(super) ts: String,
    #[serde(default)]
    pub(super) thread_ts: Option<String>,
    #[serde(default)]
    pub(super) user: String,
    #[serde(default)]
    pub(super) text: String,
    #[serde(default)]
    pub(super) is_dm: bool,
    #[serde(default)]
    pub(super) is_self: bool,
    #[serde(default)]
    pub(super) is_self_dm: bool,
}

impl SweptMessage {
    fn thread_ts(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(&self.ts)
    }

    fn session_key(&self) -> String {
        format!("{}:{}:{}", self.workspace, self.channel, self.thread_ts())
    }

    /// One message's identity, shared with the echo registry.
    fn request_id(&self) -> String {
        format!("{}:{}", self.channel, self.ts)
    }
}

/// Where a reply must go, persisted as the delivery's channel context so a
/// restarted daemon can finish the wait (same role as the bot channel's
/// trace, minus affordances — this channel never places any).
#[derive(Clone, Default, Serialize, Deserialize)]
struct PersonalTrace {
    channel: String,
    thread_ts: String,
    message_ts: String,
}

/// The messages this channel itself posted, by `channel:ts`. Sweeps return
/// them like anything else — they are the user's messages, as far as Slack is
/// concerned — and this registry is what keeps the channel from answering
/// itself. In-memory only: the cursor starts at "now" on daemon start, so
/// pre-restart posts are never swept again.
type SentRegistry = Arc<Mutex<HashSet<String>>>;

/// The currently connected backend, replaced on reconnect. Reply tasks read
/// it at send time so a turn that outlives one backend process posts through
/// its successor.
#[derive(Default)]
struct Backend {
    current: Mutex<Option<Arc<McpClient>>>,
}

impl Backend {
    fn set(&self, client: Arc<McpClient>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(client);
    }

    fn clear(&self) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn get(&self) -> Result<Arc<McpClient>> {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| anyhow!("slack-personal backend is not connected"))
    }
}

pub(super) async fn serve(
    ingress: Arc<OperatorIngress>,
    config: SlackPersonalConfig,
    cancel: CancellationToken,
) -> Result<()> {
    let backend = Arc::new(Backend::default());
    let sent: SentRegistry = Arc::default();
    reconcile_outstanding(&ingress, &config, &cancel, &backend, &sent).await;
    // Only what arrives from here on is swept. Messages sent while no daemon
    // ran are the user's own inbox to deal with — replaying days of history
    // through an operator that answers as them would be far worse.
    let mut cursor = now_slack_ts();
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let client = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            spawned = McpClient::spawn(&config.mcp_command) => spawned,
        };
        let client = match client {
            Ok(client) => Arc::new(client),
            Err(error) => {
                tracing::warn!(
                    operator = %ingress.operator_name(),
                    channel = %ingress.channel_id(),
                    %error,
                    retry_seconds = backoff.as_secs(),
                    "slack-personal backend failed to start; retrying"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
                continue;
            }
        };
        backend.set(client.clone());
        backoff = std::time::Duration::from_secs(1);
        tracing::info!(
            operator = %ingress.operator_name(),
            channel = %ingress.channel_id(),
            "slack-personal channel connected to its MCP backend"
        );
        let mut poll_schedule = PollSchedule::new(config.idle_poll_ceiling);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(poll_schedule.next_interval()) => {}
            }
            let swept = client
                .call_tool("slack_sweep_messages", json!({"after_ts": cursor}))
                .await;
            let messages = match swept {
                Ok(result) => parse_swept(&result),
                Err(error) => {
                    tracing::warn!(
                        operator = %ingress.operator_name(),
                        channel = %ingress.channel_id(),
                        %error,
                        "slack-personal sweep failed; restarting the backend"
                    );
                    backend.clear();
                    break;
                }
            };
            let (accepted, next_cursor) = {
                let sent = sent.lock().unwrap_or_else(|e| e.into_inner());
                plan_deliveries(messages, &config, &cursor, &sent)
            };
            cursor = next_cursor;
            poll_schedule.after_sweep(!accepted.is_empty());
            for message in accepted {
                let (ingress, config, cancel, backend, sent) = (
                    ingress.clone(),
                    config.clone(),
                    cancel.clone(),
                    backend.clone(),
                    sent.clone(),
                );
                tokio::spawn(async move {
                    if let Err(error) =
                        process_delivery(&ingress, &config, &cancel, &backend, &sent, message).await
                    {
                        tracing::warn!(
                            operator = %ingress.operator_name(),
                            channel = %ingress.channel_id(),
                            %error,
                            "slack-personal delivery failed"
                        );
                    }
                });
            }
        }
    }
}

fn parse_swept(result: &serde_json::Value) -> Vec<SweptMessage> {
    let Some(entries) = result.get("messages").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match serde_json::from_value(entry.clone()) {
            Ok(message) => Some(message),
            Err(error) => {
                tracing::warn!(%error, "slack-personal backend returned a malformed message; skipping it");
                None
            }
        })
        .collect()
}

/// Decide which swept messages become deliveries, and how far the cursor
/// advances. Pure so the policy — the part that decides what an agent may see
/// and answer as the user — is directly testable.
///
/// The cursor advances over *every* swept message, accepted or not: it tracks
/// sweep progress, and a rejected message must not be re-examined forever.
fn plan_deliveries(
    messages: Vec<SweptMessage>,
    config: &SlackPersonalConfig,
    cursor: &str,
    sent: &HashSet<String>,
) -> (Vec<SweptMessage>, String) {
    let mut next_cursor = cursor.to_string();
    let mut accepted = Vec::new();
    for message in messages {
        if !ts_after(&message.ts, cursor) {
            continue;
        }
        if ts_after(&message.ts, &next_cursor) {
            next_cursor = message.ts.clone();
        }
        if sent.contains(&message.request_id()) {
            continue;
        }
        if accepts(config, &message) {
            accepted.push(message);
        }
    }
    // Oldest first, so two messages in one thread reach the conversation in
    // the order they were said.
    accepted.sort_by(|a, b| ts_order(&a.ts, &b.ts));
    (accepted, next_cursor)
}

/// The scope-and-trigger policy of spec 0202, reduced to this channel's v1
/// options. An unconfigured scope forwards nothing: channels require an
/// explicit allowlist entry, and the only implicit scope is the user's DMs —
/// which the default `dm` trigger covers because a DM is addressed to the
/// user by construction.
fn accepts(config: &SlackPersonalConfig, message: &SweptMessage) -> bool {
    if message.text.trim().is_empty() {
        return false;
    }
    if !config.allowed_workspaces.is_empty()
        && !config
            .allowed_workspaces
            .iter()
            .any(|id| id == &message.workspace)
    {
        return false;
    }
    if message.is_dm {
        // The user's own words in a DM with someone else are them talking to
        // that person, not to the operator. In their self-DM they have nobody
        // else to be talking to — that is the private command line.
        !message.is_self || message.is_self_dm
    } else {
        config.trigger == SlackPersonalTrigger::All
            && !message.is_self
            && config
                .allowed_channels
                .iter()
                .any(|id| id == &message.channel)
    }
}

async fn process_delivery(
    ingress: &Arc<OperatorIngress>,
    config: &SlackPersonalConfig,
    cancel: &CancellationToken,
    backend: &Arc<Backend>,
    sent: &SentRegistry,
    message: SweptMessage,
) -> Result<()> {
    let session_key = message.session_key();
    let prompt = match first_engagement_context(ingress, config, backend, &message, &session_key).await
    {
        Some(context) => format!("{context}\n\n{}", message.text),
        None => message.text.clone(),
    };
    let key = message.request_id();
    let receipt = ingress
        .submit_tracked(IngressRequest {
            message: prompt,
            session_key: Some(session_key),
            request_id: Some(key.clone()),
        })
        .await?;
    let trace = PersonalTrace {
        channel: message.channel.clone(),
        thread_ts: message.thread_ts().to_string(),
        message_ts: message.ts.clone(),
    };
    ingress
        .record_outstanding(&key, &receipt, serde_json::to_value(&trace).unwrap_or_default())
        .await;
    resolve_delivery(
        ingress,
        config,
        cancel,
        backend,
        sent,
        &key,
        trace,
        receipt,
        std::time::Duration::ZERO,
    )
    .await
}

/// Context for a thread the operator is being pulled into, fenced the same
/// way as the bot channel's: text other people wrote, to read and not obey.
async fn first_engagement_context(
    ingress: &OperatorIngress,
    config: &SlackPersonalConfig,
    backend: &Backend,
    message: &SweptMessage,
    session_key: &str,
) -> Option<String> {
    if config.thread_context == 0 || message.thread_ts() == message.ts {
        return None;
    }
    if ingress.has_session(session_key).await {
        return None;
    }
    let client = backend.get().ok()?;
    let result = client
        .call_tool(
            "slack_read_thread",
            json!({
                "channel": message.channel,
                "thread_ts": message.thread_ts(),
                "limit": config.thread_context,
            }),
        )
        .await;
    match result {
        Ok(result) => {
            let history: Vec<SlackHistoryMessage> = result
                .get("messages")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            thread_context_block(&history, &message.ts)
        }
        Err(error) => {
            tracing::warn!(%error, "slack-personal thread history unavailable; answering from the message alone");
            None
        }
    }
}

/// Wait out one delivery's turn and act on the outcome per the response mode.
///
/// A failed turn is logged and cleared, never posted: this channel's failure
/// prose would appear as the user's own words.
#[allow(clippy::too_many_arguments)]
async fn resolve_delivery(
    ingress: &Arc<OperatorIngress>,
    config: &SlackPersonalConfig,
    cancel: &CancellationToken,
    backend: &Arc<Backend>,
    sent: &SentRegistry,
    key: &str,
    trace: PersonalTrace,
    receipt: IngressReceipt,
    already_waited: std::time::Duration,
) -> Result<()> {
    // The progress feed exists for channels that render it; this one must
    // stay silent while working, so the receiver is simply dropped.
    let (progress_tx, _progress_rx) = watch::channel(IngressProgress::default());
    let budget = PENDING_DELIVERY_TTL.saturating_sub(already_waited);
    let reply = ingress
        .wait_for_final(&receipt, cancel, &progress_tx, budget)
        .await;
    if cancel.is_cancelled() {
        // The channel is stopping, not the turn; the record is what lets the
        // next daemon pick this wait back up.
        return Err(anyhow!("channel stopped"));
    }
    match reply {
        Ok(reply) => {
            if let Err(error) = deliver_reply(config, cancel, backend, sent, &trace, &reply).await {
                if cancel.is_cancelled() {
                    // Keep the outstanding record: a replacement channel task
                    // can resume the completed turn and restart its grace wait.
                    return Err(error);
                }
                tracing::warn!(%error, "slack-personal reply delivery failed");
            }
        }
        Err(error) => {
            tracing::warn!(
                operator = %ingress.operator_name(),
                channel = %ingress.channel_id(),
                %error,
                "slack-personal turn ended without an answer; leaving the conversation untouched"
            );
        }
    }
    ingress.clear_outstanding(key).await;
    Ok(())
}

async fn deliver_reply(
    config: &SlackPersonalConfig,
    cancel: &CancellationToken,
    backend: &Backend,
    sent: &SentRegistry,
    trace: &PersonalTrace,
    reply: &str,
) -> Result<()> {
    match config.response_for_channel(&trace.channel) {
        SlackPersonalResponse::Draft => {
            let client = backend.get()?;
            client
                .call_tool(
                    "slack_create_draft",
                    json!({
                        "channel": trace.channel,
                        "thread_ts": trace.thread_ts,
                        "text": reply,
                    }),
                )
                .await
                .context("create Slack draft")?;
        }
        SlackPersonalResponse::Auto => {
            let client = backend.get()?;
            send_auto_reply(config, &client, sent, trace, reply).await?;
        }
        SlackPersonalResponse::AutoAfter => {
            tokio::select! {
                _ = cancel.cancelled() => return Err(anyhow!("channel stopped")),
                _ = tokio::time::sleep(config.auto_after) => {}
            }
            // Resolve the backend after the wait so reconnecting during the
            // grace period does not leave this delivery on a dead process.
            let client = backend.get()?;
            if user_replied_after(&client, trace).await? {
                tracing::info!(
                    channel = %trace.channel,
                    thread_ts = %trace.thread_ts,
                    trigger_ts = %trace.message_ts,
                    "slack-personal delayed reply yielded to the user's reply"
                );
            } else {
                send_auto_reply(config, &client, sent, trace, reply).await?;
            }
        }
    }
    Ok(())
}

async fn send_auto_reply(
    config: &SlackPersonalConfig,
    client: &McpClient,
    sent: &SentRegistry,
    trace: &PersonalTrace,
    reply: &str,
) -> Result<()> {
    let text = if config.disclosure {
        format!("{reply}{DISCLOSURE_SUFFIX}")
    } else {
        reply.to_string()
    };
    let result = client
        .call_tool(
            "slack_send_message",
            json!({
                "channel": trace.channel,
                "thread_ts": trace.thread_ts,
                "text": text,
            }),
        )
        .await
        .context("send Slack message")?;
    if let Some(ts) = result.get("ts").and_then(|value| value.as_str()) {
        sent.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{}:{ts}", trace.channel));
    }
    Ok(())
}

#[derive(Deserialize)]
struct GraceMessage {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    is_self: bool,
}

/// Re-read the thread after the grace period. The account owner's own
/// message wins the race: this operator is a backstop, not a competitor.
async fn user_replied_after(client: &McpClient, trace: &PersonalTrace) -> Result<bool> {
    let result = client
        .call_tool(
            "slack_read_thread",
            json!({
                "channel": trace.channel,
                "thread_ts": trace.thread_ts,
                "limit": construct_protocol::SLACK_THREAD_CONTEXT_MAX,
            }),
        )
        .await
        .context("check Slack thread before delayed send")?;
    let messages: Vec<GraceMessage> = result
        .get("messages")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("parse Slack thread before delayed send")?
        .unwrap_or_default();
    Ok(has_user_reply_after(&messages, &trace.message_ts))
}

fn has_user_reply_after(messages: &[GraceMessage], trigger_ts: &str) -> bool {
    messages
        .iter()
        .any(|message| message.is_self && ts_after(&message.ts, trigger_ts))
}

/// Finish deliveries accepted before a restart. Resumable waits are picked
/// back up on what remains of their allowance; anything else is cleared
/// without a word in the thread — see the identity rules above.
async fn reconcile_outstanding(
    ingress: &Arc<OperatorIngress>,
    config: &SlackPersonalConfig,
    cancel: &CancellationToken,
    backend: &Arc<Backend>,
    sent: &SentRegistry,
) {
    for (key, record) in ingress.outstanding().await {
        let Ok(trace) = serde_json::from_value::<PersonalTrace>(record.context.clone()) else {
            ingress.clear_outstanding(&key).await;
            continue;
        };
        if trace.channel.is_empty() {
            ingress.clear_outstanding(&key).await;
            continue;
        }
        let waited = (chrono::Utc::now() - record.submitted_at)
            .to_std()
            .unwrap_or_default();
        let receipt = ingress.resume_outstanding(&record).await;
        let Some(receipt) = receipt.filter(|_| waited < PENDING_DELIVERY_TTL) else {
            tracing::info!(
                operator = %ingress.operator_name(),
                channel = %ingress.channel_id(),
                session = %record.session,
                waited_seconds = waited.as_secs(),
                "outstanding slack-personal delivery cannot be resumed; dropping it silently"
            );
            ingress.clear_outstanding(&key).await;
            continue;
        };
        let (ingress, config, cancel, backend, sent) = (
            ingress.clone(),
            config.clone(),
            cancel.clone(),
            backend.clone(),
            sent.clone(),
        );
        tokio::spawn(async move {
            if let Err(error) = resolve_delivery(
                &ingress, &config, &cancel, &backend, &sent, &key, trace, receipt, waited,
            )
            .await
            {
                tracing::warn!(%error, "resumed slack-personal delivery failed");
            }
        });
    }
}

fn now_slack_ts() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:06}", now.as_secs(), now.subsec_micros())
}

/// Slack timestamps compared as (seconds, fraction) rather than as floats —
/// a modern ts has 16 significant digits, right at f64's edge.
fn ts_key(ts: &str) -> Option<(u64, u64)> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    // "1.5" and "1.500000" are the same instant; right-pad to compare.
    let frac = format!("{frac:0<6}");
    Some((secs.parse().ok()?, frac.get(..6)?.parse().ok()?))
}

fn ts_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (ts_key(a), ts_key(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

fn ts_after(a: &str, b: &str) -> bool {
    ts_order(a, b) == std::cmp::Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn backend_with_thread(
        messages: serde_json::Value,
    ) -> (Backend, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (reader, writer) = super::super::mcp::fake_server(move |name, _| {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(name.to_string());
            match name {
                "slack_read_thread" => {
                    super::super::mcp::text_result(json!({"messages": messages.clone()}))
                }
                "slack_send_message" => super::super::mcp::text_result(json!({"ts": "9.000001"})),
                other => panic!("unexpected tool {other}"),
            }
        });
        let client = McpClient::connect(reader, writer, None)
            .await
            .expect("connect fake MCP backend");
        let backend = Backend::default();
        backend.set(Arc::new(client));
        (backend, calls)
    }

    fn config() -> SlackPersonalConfig {
        SlackPersonalConfig {
            mcp_command: "fake".into(),
            idle_poll_ceiling: std::time::Duration::from_secs(20),
            trigger: SlackPersonalTrigger::Dm,
            default_response: SlackPersonalResponse::Draft,
            response_overrides: BTreeMap::new(),
            auto_after: std::time::Duration::from_secs(60),
            disclosure: true,
            allowed_workspaces: Vec::new(),
            allowed_channels: Vec::new(),
            thread_context: 50,
        }
    }

    #[test]
    fn polling_starts_idle_then_backs_off_progressively_after_activity() {
        let mut schedule = PollSchedule::new(std::time::Duration::from_secs(20));
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(20));

        schedule.after_sweep(true);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(5));

        schedule.after_sweep(false);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(10));
        schedule.after_sweep(false);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(20));
        schedule.after_sweep(false);
        assert_eq!(
            schedule.next_interval(),
            std::time::Duration::from_secs(20),
            "idle polling stays bounded by the configured ceiling"
        );
    }

    #[test]
    fn accepted_activity_always_resets_polling_to_the_safe_floor() {
        let mut schedule = PollSchedule::new(std::time::Duration::from_secs(30));
        schedule.after_sweep(true);
        schedule.after_sweep(false);
        schedule.after_sweep(false);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(20));

        schedule.after_sweep(true);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(5));
        schedule.after_sweep(true);
        assert_eq!(
            schedule.next_interval(),
            std::time::Duration::from_secs(5),
            "continued activity remains at the floor"
        );
    }

    #[test]
    fn a_floor_sized_idle_ceiling_remains_fixed() {
        let mut schedule = PollSchedule::new(std::time::Duration::from_secs(5));
        schedule.after_sweep(true);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(5));
        schedule.after_sweep(false);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn out_of_scope_messages_do_not_keep_polling_hot() {
        let mut schedule = PollSchedule::new(std::time::Duration::from_secs(20));
        schedule.after_sweep(true);
        assert_eq!(schedule.next_interval(), std::time::Duration::from_secs(5));

        let (accepted, _) = plan_deliveries(
            vec![message("C-unlisted", "2.0")],
            &config(),
            "1.0",
            &HashSet::new(),
        );
        schedule.after_sweep(!accepted.is_empty());
        assert_eq!(
            schedule.next_interval(),
            std::time::Duration::from_secs(10),
            "rejected account traffic counts as idle for this channel"
        );
    }

    fn message(channel: &str, ts: &str) -> SweptMessage {
        SweptMessage {
            workspace: "T1".into(),
            channel: channel.into(),
            ts: ts.into(),
            thread_ts: None,
            user: "U-other".into(),
            text: "hello".into(),
            is_dm: channel.starts_with('D'),
            is_self: false,
            is_self_dm: false,
        }
    }

    #[test]
    fn channel_response_mode_overrides_the_default_only_for_an_exact_match() {
        let mut config = config();
        config.default_response = SlackPersonalResponse::Draft;
        config
            .response_overrides
            .insert("C-sensitive".into(), SlackPersonalResponse::AutoAfter);
        config
            .response_overrides
            .insert("D-private".into(), SlackPersonalResponse::Auto);

        assert_eq!(
            config.response_for_channel("C-sensitive"),
            SlackPersonalResponse::AutoAfter,
            "an exact CHANNEL=auto-after override wins over the global default"
        );
        assert_eq!(
            config.response_for_channel("C-other"),
            SlackPersonalResponse::Draft,
            "channels without an override inherit the global response mode"
        );
        assert_eq!(
            config.response_for_channel("c-sensitive"),
            SlackPersonalResponse::Draft,
            "Slack channel IDs are exact, case-sensitive keys"
        );
        assert_eq!(
            config.response_for_channel("D-private"),
            SlackPersonalResponse::Auto,
            "DM conversation IDs may be overridden through the same map"
        );
    }

    #[test]
    fn dms_are_forwarded_but_channels_need_an_explicit_allowlist_entry() {
        // Spec 0201: an unconfigured scope is not forwarded. The default
        // trigger's only scope is DMs; a channel message needs the trigger
        // widened AND the channel allowlisted.
        let dm = message("D1", "2.0");
        assert!(accepts(&config(), &dm));

        let channel_message = message("C1", "2.0");
        assert!(!accepts(&config(), &channel_message), "dm trigger ignores channels");

        let mut widened = config();
        widened.trigger = SlackPersonalTrigger::All;
        assert!(
            !accepts(&widened, &channel_message),
            "an unlisted channel is an unconfigured scope even for trigger=all"
        );

        widened.allowed_channels = vec!["C1".into()];
        assert!(accepts(&widened, &channel_message));
    }

    #[test]
    fn the_users_own_words_trigger_only_their_self_dm() {
        // In a DM with someone else, the user is talking to that person; an
        // operator that answered would be interrupting their own
        // conversation as them. In their self-DM there is nobody else to be
        // addressing — that is the private command line (spec 0202).
        let mut own_dm_message = message("D1", "2.0");
        own_dm_message.is_self = true;
        assert!(!accepts(&config(), &own_dm_message));

        let mut self_dm = own_dm_message.clone();
        self_dm.is_self_dm = true;
        assert!(accepts(&config(), &self_dm));

        let mut own_channel_message = message("C1", "2.0");
        own_channel_message.is_self = true;
        let mut widened = config();
        widened.trigger = SlackPersonalTrigger::All;
        widened.allowed_channels = vec!["C1".into()];
        assert!(
            !accepts(&widened, &own_channel_message),
            "the operator never publicly answers the user's own channel messages"
        );
    }

    #[test]
    fn workspace_allowlist_gates_everything_including_dms() {
        let mut config = config();
        config.allowed_workspaces = vec!["T1".into()];
        assert!(accepts(&config, &message("D1", "2.0")));
        let mut foreign = message("D1", "2.0");
        foreign.workspace = "T2".into();
        assert!(!accepts(&config, &foreign));
    }

    #[test]
    fn the_cursor_advances_over_rejected_messages_too() {
        // The cursor tracks sweep progress, not acceptance. If a rejected
        // message held it back, the same message would be re-fetched and
        // re-rejected on every poll forever.
        let mut config = config();
        config.trigger = SlackPersonalTrigger::Dm;
        let messages = vec![message("C-unlisted", "5.0"), message("D1", "3.0")];
        let (accepted, cursor) = plan_deliveries(messages, &config, "1.0", &HashSet::new());
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].channel, "D1");
        assert_eq!(cursor, "5.0");
    }

    #[test]
    fn already_seen_and_echoed_messages_are_not_delivered() {
        let sent: HashSet<String> = ["D1:4.0".to_string()].into();
        let messages = vec![
            message("D1", "2.0"), // at/behind the cursor: already swept
            message("D1", "4.0"), // the channel's own post, echoed back
            message("D1", "6.0"),
        ];
        let (accepted, cursor) = plan_deliveries(messages, &config(), "2.0", &sent);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].ts, "6.0");
        assert_eq!(cursor, "6.0");
    }

    #[test]
    fn deliveries_come_out_oldest_first() {
        let messages = vec![message("D1", "9.1"), message("D1", "9.05"), message("D2", "9.2")];
        let (accepted, _) = plan_deliveries(messages, &config(), "1.0", &HashSet::new());
        let order: Vec<&str> = accepted.iter().map(|m| m.ts.as_str()).collect();
        assert_eq!(order, ["9.05", "9.1", "9.2"]);
    }

    #[test]
    fn slack_timestamps_compare_as_instants_not_strings() {
        // "9.9" < "10.0" as instants but not as strings, and a modern ts has
        // 16 significant digits — one past what f64 can hold exactly.
        assert!(ts_after("10.000000", "9.900000"));
        assert!(ts_after("1755500000.000002", "1755500000.000001"));
        assert!(!ts_after("1755500000.000001", "1755500000.000001"));
        // A fraction written short is the same instant padded.
        assert!(!ts_after("2.5", "2.500000"));
        assert!(ts_after("2.500001", "2.5"));
    }

    #[test]
    fn a_thread_reply_routes_to_its_thread_and_a_root_to_itself() {
        let mut reply = message("C1", "222.22");
        reply.thread_ts = Some("111.11".into());
        assert_eq!(reply.session_key(), "T1:C1:111.11");
        assert_eq!(reply.request_id(), "C1:222.22");
        assert_eq!(message("C1", "111.11").session_key(), "T1:C1:111.11");
    }

    #[test]
    fn swept_parsing_skips_malformed_entries_and_keeps_the_rest() {
        let result = serde_json::json!({"messages": [
            {"channel": "D1", "ts": "1.0", "text": "hi", "is_dm": true},
            {"text": "no channel or ts"},
            {"channel": "C1", "ts": "2.0"},
        ]});
        let messages = parse_swept(&result);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].channel, "D1");
        assert!(messages[0].is_dm);
        assert!(!messages[1].is_dm, "missing booleans read conservatively");
    }

    #[test]
    fn empty_messages_are_never_deliveries() {
        let mut empty = message("D1", "2.0");
        empty.text = "   ".into();
        assert!(!accepts(&config(), &empty));
    }

    #[tokio::test]
    async fn auto_after_waits_then_rechecks_and_sends() {
        let (backend, calls) = backend_with_thread(json!([
            {"ts": "1.9", "is_self": true},
            {"ts": "2.1", "is_self": false}
        ]))
        .await;
        let mut config = config();
        config.default_response = SlackPersonalResponse::AutoAfter;
        config.auto_after = std::time::Duration::from_millis(30);
        config.disclosure = false;
        let sent = SentRegistry::default();
        let trace = PersonalTrace {
            channel: "D1".into(),
            thread_ts: "1.0".into(),
            message_ts: "2.0".into(),
        };
        let cancel = CancellationToken::new();
        let mut delivery = Box::pin(deliver_reply(
            &config, &cancel, &backend, &sent, &trace, "answer",
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), delivery.as_mut())
                .await
                .is_err(),
            "auto-after must not touch Slack before its deadline"
        );
        assert!(calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty());

        delivery.await.expect("delayed delivery");
        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            ["slack_read_thread", "slack_send_message"]
        );
        assert!(sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains("D1:9.000001"));
    }

    #[tokio::test]
    async fn auto_after_yields_when_the_user_answers_first() {
        let (backend, calls) =
            backend_with_thread(json!([{"ts": "2.000001", "is_self": true}])).await;
        let mut config = config();
        config.default_response = SlackPersonalResponse::AutoAfter;
        config.auto_after = std::time::Duration::from_millis(1);
        let sent = SentRegistry::default();
        let trace = PersonalTrace {
            channel: "D1".into(),
            thread_ts: "1.0".into(),
            message_ts: "2.0".into(),
        };

        deliver_reply(
            &config,
            &CancellationToken::new(),
            &backend,
            &sent,
            &trace,
            "answer",
        )
        .await
        .expect("grace check");

        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            ["slack_read_thread"]
        );
        assert!(sent.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn the_trace_round_trips_for_restart_reconciliation() {
        let trace = PersonalTrace {
            channel: "D1".into(),
            thread_ts: "1.1".into(),
            message_ts: "2.2".into(),
        };
        let decoded: PersonalTrace =
            serde_json::from_value(serde_json::to_value(&trace).unwrap()).unwrap();
        assert_eq!(decoded.channel, "D1");
        assert_eq!(decoded.thread_ts, "1.1");
        assert_eq!(decoded.message_ts, "2.2");
    }
}
