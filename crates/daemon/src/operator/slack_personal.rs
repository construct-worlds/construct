//! slack-personal adapter: the user's own Slack account as an operator
//! channel (specs 0201, 0202).
//!
//! Unlike the Socket Mode bot channel, nothing here talks to Slack directly.
//! The channel drives Slack's hosted MCP server through [`super::mcp`] with
//! plain daemon logic — no agent, no model turn — and simulates an event
//! subscription by polling. Construct owns the endpoint and first-run OAuth
//! setup; channel configuration contains no executable command.
//!
//! ## The tool contract
//!
//! The adapter maps the hosted server's native tools onto the channel model:
//! `slack_search_public_and_private` for sweeps, `slack_read_thread` for
//! context, `slack_send_message` for automatic replies, and
//! `slack_send_message_draft` for the safe default.
//!
//! ## Identity rules
//!
//! Everything this channel posts appears as the user, so it never posts
//! progress placeholders, failure notices, or anything else the response mode
//! did not explicitly produce. Its own posts are excluded from ingress by
//! recorded timestamp. Hosted search identifies the user's messages with
//! `from:me`; those messages remain excluded unless a future structured result
//! can explicitly and safely identify the user's DM with themself.

use super::ingress::{
    IngressProgress, IngressReceipt, IngressRequest, OperatorIngress, PENDING_DELIVERY_TTL,
};
use super::mcp::{slack_oauth_credentials_saved, McpClient};
use super::slack::{thread_context_block, SlackHistoryMessage};
use super::{SlackPersonalResponse, SlackPersonalTrigger};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SlackPersonalConfig {
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

const HOSTED_SEARCH_TOOL: &str = "slack_search_public_and_private";
const HOSTED_READ_THREAD_TOOL: &str = "slack_read_thread";
const HOSTED_SEND_TOOL: &str = "slack_send_message";
const HOSTED_DRAFT_TOOL: &str = "slack_send_message_draft";
const HOSTED_SEARCH_PAGE_LIMIT: usize = 20;

/// Search is the hosted server's polling primitive. Its date filter has day
/// precision, so the exact Slack timestamp is applied again after decoding.
/// Paging newest-first lets a normal idle sweep stop as soon as it reaches
/// the existing cursor, while still collecting every newer page before the
/// cursor advances. Cursor-cycle detection protects against a broken server.
async fn sweep_hosted_messages(client: &McpClient, after_ts: &str) -> Result<Vec<SweptMessage>> {
    let mut messages = hosted_search(client, after_ts, false).await?;
    let self_ids: HashSet<String> = hosted_search(client, after_ts, true)
        .await?
        .into_iter()
        .map(|message| message.request_id())
        .collect();
    for message in &mut messages {
        message.is_self |= self_ids.contains(&message.request_id());
        // Hosted results may explicitly identify the account owner's self-DM.
        // Without that signal we stay conservative: the user's own message in
        // a DM with somebody else must never look like a self-DM command.
        message.is_self_dm &= message.is_self && message.is_dm;
    }
    messages.retain(|message| ts_after(&message.ts, after_ts));
    let mut unique = HashMap::new();
    for message in messages {
        unique.entry(message.request_id()).or_insert(message);
    }
    Ok(unique.into_values().collect())
}

async fn hosted_search(
    client: &McpClient,
    after_ts: &str,
    from_me: bool,
) -> Result<Vec<SweptMessage>> {
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();
    let mut messages = Vec::new();
    loop {
        let arguments = hosted_search_arguments(after_ts, from_me, cursor.as_deref());
        let result = client
            .call_tool_value(HOSTED_SEARCH_TOOL, arguments)
            .await
            .context("search Slack messages")?;
        let result = decode_hosted_result(result)?;
        let (page, next) = normalize_hosted_search_page(&result);
        let reached_existing_cursor = page.iter().any(|message| !ts_after(&message.ts, after_ts));
        messages.extend(page);
        if reached_existing_cursor {
            break;
        }
        let next = next
            .filter(|value| !value.is_empty() && Some(value.as_str()) != cursor.as_deref());
        let Some(next) = next else { break };
        if !seen_cursors.insert(next.clone()) {
            break;
        }
        cursor = Some(next);
    }
    Ok(messages)
}

fn hosted_search_arguments(after_ts: &str, from_me: bool, cursor: Option<&str>) -> Value {
    let date = slack_ts_date(after_ts);
    let query = if from_me {
        format!("from:me after:{date}")
    } else {
        format!("after:{date}")
    };
    let mut arguments = json!({
        "query": query,
        "sort": "timestamp",
        "sort_dir": "desc",
        "content_types": "messages",
        "include_context": false,
        "include_bots": false,
        "response_format": "detailed",
        "limit": HOSTED_SEARCH_PAGE_LIMIT,
    });
    if let Some(cursor) = cursor {
        arguments["cursor"] = Value::String(cursor.to_string());
    }
    arguments
}

fn slack_ts_date(ts: &str) -> String {
    ts.split_once('.')
        .map(|(seconds, _)| seconds)
        .unwrap_or(ts)
        .parse::<i64>()
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|instant| instant.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string())
}

fn decode_hosted_result(value: Value) -> Result<Value> {
    match value {
        Value::String(text) => serde_json::from_str(&text)
            .context("Slack MCP tool returned unstructured text instead of JSON"),
        value => Ok(value),
    }
}

fn normalize_hosted_search_page(value: &Value) -> (Vec<SweptMessage>, Option<String>) {
    let mut messages = Vec::new();
    collect_hosted_messages(value, &mut messages);
    if let Some(rendered) = value.get("results").and_then(Value::as_str) {
        messages.extend(parse_hosted_search_results(rendered));
    }
    let cursor = find_string(value, &["next_cursor", "nextCursor", "cursor"]).or_else(|| {
        value
            .get("pagination_info")
            .and_then(Value::as_str)
            .and_then(cursor_from_pagination)
    });
    (messages, cursor)
}

fn normalize_hosted_history(value: &Value) -> Vec<SlackHistoryMessage> {
    let mut history = Vec::new();
    collect_hosted_history(value, &mut history);
    if let Some(rendered) = value.get("messages").and_then(Value::as_str) {
        history.extend(parse_hosted_thread_messages(rendered));
    }
    history.sort_by(|a, b| match (a.ts.as_deref(), b.ts.as_deref()) {
        (Some(a), Some(b)) => ts_order(a, b),
        _ => std::cmp::Ordering::Equal,
    });
    history
}

/// Slack's hosted tools return their detailed payload in string fields inside
/// a JSON object. This parser follows the labels emitted by the hosted search
/// tool; the structured-object walker above remains as a compatibility path
/// if Slack starts populating `structuredContent` with message objects.
fn parse_hosted_search_results(rendered: &str) -> Vec<SweptMessage> {
    rendered
        .split("\n### Result ")
        .skip(1)
        .filter_map(|section| {
            let channel_line = labeled_line(section, "Channel:")?;
            let channel = parenthesized_id(channel_line)?;
            let user_line = labeled_line(section, "User:").unwrap_or_default();
            let user = parenthesized_id(user_line).unwrap_or_default();
            let ts = labeled_line(section, "Message_ts:")
                .or_else(|| labeled_line(section, "Message ts:"))
                .and_then(normalize_timestamp)?;
            let permalink = labeled_line(section, "Permalink:")
                .and_then(markdown_link_target)
                .unwrap_or_default();
            let text = section
                .split_once("\nText:\n")
                .map(|(_, text)| {
                    text.split("\n\n---")
                        .next()
                        .unwrap_or(text)
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            let thread_ts = query_parameter(&permalink, "thread_ts").and_then(normalize_timestamp);
            let workspace = permalink
                .split_once("://")
                .and_then(|(_, tail)| tail.split('/').next())
                .unwrap_or_default()
                .to_string();
            // A private channel may be rendered without a leading `#`; only
            // Slack's direct-message ID is safe to treat as an implicit DM.
            // Everything else stays behind the explicit channel allowlist.
            let is_dm = channel.starts_with('D');
            Some(SweptMessage {
                workspace,
                channel,
                ts,
                thread_ts,
                user,
                text,
                is_dm,
                is_self: false,
                // The hosted result does not identify a DM-to-self. Leaving
                // this false is conservative: own messages in ordinary DMs
                // must not be mistaken for operator commands.
                is_self_dm: false,
            })
        })
        .collect()
}

fn parse_hosted_thread_messages(rendered: &str) -> Vec<SlackHistoryMessage> {
    rendered
        .split("=== ")
        .skip(1)
        .filter_map(|section| {
            let user = labeled_line(section, "User:").and_then(parenthesized_id);
            let ts = labeled_line(section, "Message ts:")
                .or_else(|| labeled_line(section, "Message_ts:"))
                .and_then(normalize_timestamp)?;
            let ts_line = section.lines().position(|line| {
                line.trim_start().starts_with("Message ts:")
                    || line.trim_start().starts_with("Message_ts:")
            })?;
            let text = section
                .lines()
                .skip(ts_line + 1)
                .take_while(|line| line.trim() != "---")
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            Some(SlackHistoryMessage {
                user,
                bot_id: None,
                text: Some(text),
                ts: Some(ts),
            })
        })
        .collect()
}

fn labeled_line<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(label).map(str::trim))
}

fn parenthesized_id(text: &str) -> Option<String> {
    let value = text.rsplit_once('(')?.1.split_once(')')?.0.trim();
    let value = value.strip_prefix("ID:").unwrap_or(value).trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn markdown_link_target(text: &str) -> Option<String> {
    let start = text.rfind("](")? + 2;
    let end = text[start..].find(')')? + start;
    Some(text[start..end].to_string())
}

fn query_parameter<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    url.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn cursor_from_pagination(text: &str) -> Option<String> {
    let (_, tail) = text.split_once('`')?;
    let (cursor, _) = tail.split_once('`')?;
    (!cursor.is_empty()).then(|| cursor.to_string())
}

fn collect_hosted_history(value: &Value, history: &mut Vec<SlackHistoryMessage>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_hosted_history(value, history);
            }
        }
        Value::Object(object) => {
            let ts = object_string(object, &["ts", "message_ts", "messageTs", "timestamp"])
                .and_then(|value| normalize_timestamp(&value))
                .or_else(|| {
                    object_string(object, &["permalink", "url"])
                        .and_then(|url| timestamp_from_permalink(&url))
                });
            let text = object_string(object, &["text", "content", "message"]);
            if ts.is_some() && text.is_some() {
                history.push(SlackHistoryMessage {
                    user: object_string(
                        object,
                        &["user_id", "userId", "user", "author_id", "authorId"],
                    ),
                    bot_id: object_string(object, &["bot_id", "botId"]),
                    text,
                    ts,
                });
                return;
            }
            for value in object.values() {
                collect_hosted_history(value, history);
            }
        }
        _ => {}
    }
}

fn collect_hosted_messages(value: &Value, messages: &mut Vec<SweptMessage>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_hosted_messages(value, messages);
            }
        }
        Value::Object(object) => {
            if let Some(message) = hosted_message(object) {
                messages.push(message);
                return;
            }
            for value in object.values() {
                collect_hosted_messages(value, messages);
            }
        }
        _ => {}
    }
}

fn hosted_message(object: &serde_json::Map<String, Value>) -> Option<SweptMessage> {
    let channel = object_string(object, &["channel_id", "channelId", "channel"])?;
    let text = object_string(object, &["text", "content", "message"]).unwrap_or_default();
    let ts = object_string(object, &["ts", "message_ts", "messageTs", "timestamp"])
        .and_then(|value| normalize_timestamp(&value))
        .or_else(|| {
            object_string(object, &["permalink", "url"])
                .and_then(|url| timestamp_from_permalink(&url))
        })?;
    let thread_ts = object_string(object, &["thread_ts", "threadTs", "thread_timestamp"])
        .and_then(|value| normalize_timestamp(&value));
    let channel_type = object_string(
        object,
        &["channel_type", "channelType", "conversation_type"],
    )
    .unwrap_or_default();
    let is_dm = object_bool(object, &["is_dm", "isDm"])
        .unwrap_or_else(|| channel.starts_with('D') || channel_type.eq_ignore_ascii_case("im"));
    Some(SweptMessage {
        workspace: object_string(
            object,
            &[
                "workspace_id",
                "workspaceId",
                "team_id",
                "teamId",
                "workspace",
            ],
        )
        .unwrap_or_default(),
        channel,
        ts,
        thread_ts,
        user: object_string(
            object,
            &["user_id", "userId", "user", "author_id", "authorId"],
        )
        .unwrap_or_default(),
        text,
        is_dm,
        is_self: object_bool(object, &["is_self", "isSelf", "is_from_me", "isFromMe"])
            .unwrap_or(false),
        is_self_dm: object_bool(object, &["is_self_dm", "isSelfDm", "is_self_conversation"])
            .unwrap_or(false),
    })
}

fn object_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match object.get(*key)? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn object_bool(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| object.get(*key)?.as_bool())
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => object_string(object, keys)
            .or_else(|| object.values().find_map(|value| find_string(value, keys))),
        Value::Array(values) => values.iter().find_map(|value| find_string(value, keys)),
        _ => None,
    }
}

fn normalize_timestamp(value: &str) -> Option<String> {
    let value = value.trim();
    if ts_key(value).is_some() {
        return Some(value.to_string());
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|instant| {
            format!(
                "{}.{:06}",
                instant.timestamp(),
                instant.timestamp_subsec_micros()
            )
        })
}

fn timestamp_from_permalink(url: &str) -> Option<String> {
    let compact = url.rsplit("/p").next()?;
    let digits: String = compact.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() <= 6 {
        return None;
    }
    let split = digits.len() - 6;
    Some(format!("{}.{}", &digits[..split], &digits[split..]))
}

fn hosted_response_timestamp(value: &Value) -> Option<String> {
    find_string(value, &["ts", "message_ts", "messageTs", "timestamp"])
        .and_then(|value| normalize_timestamp(&value))
        .or_else(|| {
            find_string(value, &["permalink", "url", "message_link", "messageLink"])
                .and_then(|url| timestamp_from_permalink(&url))
        })
        .or_else(|| match value {
            Value::String(text) => {
                if let Some(timestamp) = timestamp_from_permalink(text) {
                    return Some(timestamp);
                }
                let tail = text.split("ts=").nth(1)?;
                let timestamp: String = tail
                    .chars()
                    .take_while(|character| character.is_ascii_digit() || *character == '.')
                    .collect();
                normalize_timestamp(&timestamp)
            }
            _ => None,
        })
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
        if !slack_oauth_credentials_saved() {
            tracing::info!(
                operator = %ingress.operator_name(),
                channel = %ingress.channel_id(),
                "slack-personal needs Slack OAuth; opening the authorization page in the default browser"
            );
        }
        let client = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            spawned = McpClient::spawn_slack() => spawned,
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
            let swept = sweep_hosted_messages(&client, &cursor).await;
            let messages = match swept {
                Ok(messages) => messages,
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
    match hosted_thread_history(
        &client,
        &message.channel,
        message.thread_ts(),
        config.thread_context,
    )
    .await
    {
        Ok(history) => thread_context_block(&history, &message.ts),
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
                .call_tool_value(
                    HOSTED_DRAFT_TOOL,
                    hosted_reply_arguments(&trace.channel, &trace.thread_ts, reply),
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
        .call_tool_value(
            HOSTED_SEND_TOOL,
            hosted_reply_arguments(&trace.channel, &trace.thread_ts, &text),
        )
        .await
        .context("send Slack message")?;
    if let Some(ts) = hosted_response_timestamp(&result) {
        sent.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(format!("{}:{ts}", trace.channel));
    }
    Ok(())
}

/// Search for the account owner's own messages after the grace period. A
/// matching message in this conversation wins the race: this operator is a
/// backstop, not a competitor. Slack's hosted thread result does not identify
/// the authenticated user, while its native `from:me` search does.
async fn user_replied_after(client: &McpClient, trace: &PersonalTrace) -> Result<bool> {
    let messages = hosted_search(client, &trace.message_ts, true)
        .await
        .context("check Slack thread before delayed send")?;
    Ok(has_user_reply_after(messages.iter(), trace))
}

fn has_user_reply_after<'a>(
    messages: impl IntoIterator<Item = &'a SweptMessage>,
    trace: &PersonalTrace,
) -> bool {
    messages.into_iter().any(|message| {
        message.channel == trace.channel
            && ts_after(&message.ts, &trace.message_ts)
            // Ordinary DMs are one linear conversation. Public and private
            // channels require the same Slack thread.
            && (trace.channel.starts_with('D') || message.thread_ts() == trace.thread_ts)
    })
}

fn hosted_read_thread_arguments(channel: &str, message_ts: &str, limit: usize) -> Value {
    json!({
        "channel_id": channel,
        "message_ts": message_ts,
        "limit": limit,
        "response_format": "detailed",
    })
}

async fn hosted_thread_history(
    client: &McpClient,
    channel: &str,
    message_ts: &str,
    limit: usize,
) -> Result<Vec<SlackHistoryMessage>> {
    let result = client
        .call_tool_value(
            HOSTED_READ_THREAD_TOOL,
            hosted_read_thread_arguments(channel, message_ts, limit),
        )
        .await
        .context("read Slack thread")?;
    let result = decode_hosted_result(result)?;
    Ok(normalize_hosted_history(&result))
}

fn hosted_reply_arguments(channel: &str, thread_ts: &str, message: &str) -> Value {
    json!({
        "channel_id": channel,
        "thread_ts": thread_ts,
        "message": message,
    })
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

    async fn backend_with_owned_messages(
        messages: serde_json::Value,
    ) -> (Backend, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (reader, writer) = super::super::mcp::fake_server(move |name, arguments| {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(name.to_string());
            match name {
                "slack_search_public_and_private" => {
                    assert!(arguments["query"]
                        .as_str()
                        .is_some_and(|query| query.starts_with("from:me after:")));
                    super::super::mcp::text_result(json!({"messages": messages.clone()}))
                }
                "slack_send_message" => super::super::mcp::text_result(json!({
                    "message_link": "https://acme.slack.com/archives/D1/p9000001"
                })),
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
    fn hosted_sweep_uses_slacks_native_search_tool_and_schema() {
        assert_eq!(HOSTED_SEARCH_TOOL, "slack_search_public_and_private");
        assert_eq!(
            hosted_search_arguments("1787068800.123456", false, Some("next-page")),
            json!({
                "query": "after:2026-08-18",
                "sort": "timestamp",
                "sort_dir": "desc",
                "content_types": "messages",
                "include_context": false,
                "include_bots": false,
                "response_format": "detailed",
                "limit": 20,
                "cursor": "next-page",
            })
        );
        assert_eq!(
            hosted_search_arguments("1787068800.123456", true, None)["query"],
            "from:me after:2026-08-18"
        );
    }

    #[tokio::test]
    async fn hosted_sweep_calls_native_search_and_cross_references_from_me() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let rendered = "# Search Results\n\n### Result 1 of 1\nChannel: DM with Ada (ID: D123)\nUser: Owner (ID: U123)\nMessage_ts: 1787079601.123456\nPermalink: [View](https://acme.slack.com/archives/D123/p1787079601123456)\nText:\nmy message\n";
        let (reader, writer) = super::super::mcp::fake_server(move |name, arguments| {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((name.to_string(), arguments.clone()));
            super::super::mcp::text_result(json!({
                "results": rendered,
                "pagination_info": "There are no more results.",
            }))
        });
        let client = McpClient::connect(reader, writer, None)
            .await
            .expect("connect fake MCP backend");
        let messages = sweep_hosted_messages(&client, "1787079500.000001")
            .await
            .expect("hosted sweep");

        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_self);
        assert!(!messages[0].is_self_dm);
        let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(name, _)| name == HOSTED_SEARCH_TOOL));
        assert_eq!(calls[0].1["query"], "after:2026-08-18");
        assert_eq!(calls[1].1["query"], "from:me after:2026-08-18");
    }

    #[test]
    fn hosted_detailed_search_result_maps_to_the_personal_sweep_contract() {
        let result = json!({
            "results": "# Search Results for: after:2026-08-18\n\n## Messages (1 result)\n### Result 1 of 1\nChannel: DM with Ada (ID: D123)\nUser: Ada Lovelace (ID: U456) [member]\nDate: 2026-08-18 12:00:01 PDT\nMessage_ts: 1787079601.123456\nPermalink: [View](https://acme.slack.com/archives/D123/p1787079601123456?thread_ts=1787079500.000001&cid=D123)\nText:\nhello from Slack\n\n---\n",
            "pagination_info": "For more results use cursor `opaque-next=`",
        });
        let (messages, cursor) = normalize_hosted_search_page(&result);
        assert_eq!(cursor.as_deref(), Some("opaque-next="));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].workspace, "acme.slack.com");
        assert_eq!(messages[0].channel, "D123");
        assert_eq!(messages[0].user, "U456");
        assert_eq!(messages[0].ts, "1787079601.123456");
        assert_eq!(messages[0].thread_ts.as_deref(), Some("1787079500.000001"));
        assert_eq!(messages[0].text, "hello from Slack");
        assert!(messages[0].is_dm);
    }

    #[test]
    fn hosted_private_channel_without_hash_does_not_bypass_the_allowlist() {
        let result = json!({
            "results": "# Search Results\n\n### Result 1 of 1\nChannel: private-project (ID: C999)\nUser: Ada Lovelace (ID: U456)\nMessage_ts: 1787079601.123456\nPermalink: [View](https://acme.slack.com/archives/C999/p1787079601123456)\nText:\nprivate message\n",
        });
        let (messages, _) = normalize_hosted_search_page(&result);
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].is_dm);
        assert!(!accepts(&config(), &messages[0]));
    }

    #[test]
    fn hosted_thread_uses_message_ts_and_decodes_detailed_history() {
        assert_eq!(HOSTED_READ_THREAD_TOOL, "slack_read_thread");
        assert_eq!(
            hosted_read_thread_arguments("C123", "1787079500.000001", 50),
            json!({
                "channel_id": "C123",
                "message_ts": "1787079500.000001",
                "limit": 50,
                "response_format": "detailed",
            })
        );
        let result = json!({
            "messages": "=== Thread Parent Message ===\nUser: Ada Lovelace (U456)\nDate: 2026-08-18 12:00:00 PDT\nMessage ts: 1787079500.000001\nparent text\n\n---\n\n=== Thread Reply 1 ===\nUser: Grace Hopper (U789)\nDate: 2026-08-18 12:00:01 PDT\nMessage ts: 1787079601.123456\nreply text\n",
            "pagination_info": "There are no more messages in this thread.",
        });
        let history = normalize_hosted_history(&result);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].user.as_deref(), Some("U456"));
        assert_eq!(history[0].text.as_deref(), Some("parent text"));
        assert_eq!(history[1].ts.as_deref(), Some("1787079601.123456"));
        assert_eq!(history[1].text.as_deref(), Some("reply text"));
    }

    #[tokio::test]
    async fn hosted_thread_read_calls_the_native_tool_and_decodes_text_json() {
        let (reader, writer) = super::super::mcp::fake_server(|name, arguments| {
            assert_eq!(name, "slack_read_thread");
            assert_eq!(
                arguments,
                &hosted_read_thread_arguments("C123", "1787079500.000001", 20)
            );
            super::super::mcp::text_result(json!({
                "messages": "=== Thread Parent Message ===\nUser: Ada (U456)\nMessage ts: 1787079500.000001\nhello\n",
                "pagination_info": "There are no more messages in this thread.",
            }))
        });
        let client = McpClient::connect(reader, writer, None)
            .await
            .expect("connect fake MCP backend");
        let history = hosted_thread_history(&client, "C123", "1787079500.000001", 20)
            .await
            .expect("hosted thread read");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].user.as_deref(), Some("U456"));
        assert_eq!(history[0].text.as_deref(), Some("hello"));
    }

    #[test]
    fn hosted_send_and_draft_use_native_names_arguments_and_message_link() {
        assert_eq!(HOSTED_SEND_TOOL, "slack_send_message");
        assert_eq!(HOSTED_DRAFT_TOOL, "slack_send_message_draft");
        assert_eq!(
            hosted_reply_arguments("C123", "1787079500.000001", "hello"),
            json!({
                "channel_id": "C123",
                "thread_ts": "1787079500.000001",
                "message": "hello",
            })
        );
        assert_eq!(
            hosted_response_timestamp(&Value::String(
                "Message sent: https://acme.slack.com/archives/C123/p1787079601123456".into()
            ))
            .as_deref(),
            Some("1787079601.123456")
        );
    }

    #[tokio::test]
    async fn hosted_send_and_draft_delivery_call_the_native_tools() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (reader, writer) = super::super::mcp::fake_server(move |name, arguments| {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((name.to_string(), arguments.clone()));
            match name {
                "slack_send_message_draft" => super::super::mcp::text_result(json!({})),
                "slack_send_message" => super::super::mcp::text_result(json!({
                    "message_link": "https://acme.slack.com/archives/C123/p1787079601123456"
                })),
                other => panic!("unexpected tool {other}"),
            }
        });
        let client = McpClient::connect(reader, writer, None)
            .await
            .expect("connect fake MCP backend");
        let backend = Backend::default();
        backend.set(Arc::new(client));
        let sent = SentRegistry::default();
        let trace = PersonalTrace {
            channel: "C123".into(),
            thread_ts: "1787079500.000001".into(),
            message_ts: "1787079501.000001".into(),
        };
        let cancel = CancellationToken::new();

        deliver_reply(&config(), &cancel, &backend, &sent, &trace, "draft reply")
            .await
            .expect("create hosted draft");
        let mut automatic = config();
        automatic.default_response = SlackPersonalResponse::Auto;
        automatic.disclosure = false;
        deliver_reply(&automatic, &cancel, &backend, &sent, &trace, "sent reply")
            .await
            .expect("send hosted reply");

        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            [
                (
                    "slack_send_message_draft".to_string(),
                    json!({
                        "channel_id": "C123",
                        "thread_ts": "1787079500.000001",
                        "message": "draft reply",
                    }),
                ),
                (
                    "slack_send_message".to_string(),
                    json!({
                        "channel_id": "C123",
                        "thread_ts": "1787079500.000001",
                        "message": "sent reply",
                    }),
                ),
            ]
        );
        assert!(sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains("C123:1787079601.123456"));
    }

    #[test]
    fn delayed_reply_checks_the_same_public_thread_but_all_of_a_dm() {
        let trace = PersonalTrace {
            channel: "C1".into(),
            thread_ts: "1.0".into(),
            message_ts: "2.0".into(),
        };
        let mut other_thread = message("C1", "3.0");
        other_thread.thread_ts = Some("9.0".into());
        assert!(!has_user_reply_after([&other_thread], &trace));

        let mut same_thread = other_thread.clone();
        same_thread.thread_ts = Some("1.0".into());
        assert!(has_user_reply_after([&same_thread], &trace));

        let dm_trace = PersonalTrace {
            channel: "D1".into(),
            ..trace
        };
        let dm_reply = message("D1", "3.0");
        assert!(has_user_reply_after([&dm_reply], &dm_trace));
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
    fn structured_search_results_remain_supported_if_slack_adds_them() {
        let result = json!({"messages": [
            {"channel_id": "D1", "message_ts": "1.0", "text": "hi", "is_dm": true},
            {"text": "no channel or timestamp"},
            {"channel_id": "C1", "message_ts": "2.0"},
        ]});
        let (messages, _) = normalize_hosted_search_page(&result);
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
        let (backend, calls) = backend_with_owned_messages(json!([
            {"channel_id": "D1", "message_ts": "1.9", "text": "old"},
            {"channel_id": "D2", "message_ts": "2.1", "text": "another DM"}
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
            ["slack_search_public_and_private", "slack_send_message"]
        );
        assert!(sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains("D1:9.000001"));
    }

    #[tokio::test]
    async fn auto_after_yields_when_the_user_answers_first() {
        let (backend, calls) = backend_with_owned_messages(json!([{
            "channel_id": "D1",
            "message_ts": "2.000001",
            "text": "human reply"
        }]))
        .await;
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
            ["slack_search_public_and_private"]
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
