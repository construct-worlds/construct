//! Transport-neutral service ingress.
//!
//! Channel adapters turn their native deliveries into [`IngressRequest`]s and
//! hand them to [`ServiceIngress`]. Session routing, ownership, request
//! deduplication, result reconstruction, and approval handling live here so a
//! channel does not need to reimplement service semantics.

use super::{ServiceConfig, ServiceRouting, ServiceSessionMode};
use crate::session::SessionManager;
use anyhow::{anyhow, Result};
use construct_protocol::{
    CreateSessionParams, MessageRole, PtySize, SessionEvent, SessionKind, SessionState,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const REQUEST_DEDUP_CAP: usize = 4096;
const PENDING_DELIVERY_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

struct PendingDelivery {
    session_id: String,
    created_at: tokio::time::Instant,
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct PersistedState {
    #[serde(default)]
    pub(super) sessions: HashMap<String, String>,
    /// Every session this service is allowed to expose through a channel.
    /// This is broader than `sessions`: per-event sessions have no routing key
    /// but still need to remain queryable by the channel that created them.
    #[serde(default)]
    pub(super) owned_sessions: HashSet<String>,
}

impl PersistedState {
    pub(super) fn normalize_legacy_ownership(&mut self) {
        self.owned_sessions.extend(self.sessions.values().cloned());
    }
}

/// Per-service state that outlives any one definition.
///
/// Exactly one instance exists per service name for the life of the daemon.
/// Reloads replace `config` in place so routed sessions and delivery history
/// survive definition edits.
pub(crate) struct ServiceIngressShared {
    name: String,
    config: std::sync::RwLock<Arc<ServiceConfig>>,
    manager: Arc<SessionManager>,
    state_path: PathBuf,
    pub(super) state: Mutex<PersistedState>,
    pub(super) seen_requests: Mutex<(VecDeque<String>, HashSet<String>)>,
    pending_deliveries: Mutex<HashMap<String, PendingDelivery>>,
}

impl ServiceIngressShared {
    pub(crate) fn load(
        name: String,
        config: ServiceConfig,
        manager: Arc<SessionManager>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let state_path = data_dir.join("services").join(format!("{name}.json"));
        let mut state: PersistedState = std::fs::read(&state_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        state.normalize_legacy_ownership();
        Arc::new(Self {
            name,
            config: std::sync::RwLock::new(Arc::new(config)),
            manager,
            state_path,
            state: Mutex::new(state),
            seen_requests: Mutex::new(Default::default()),
            pending_deliveries: Mutex::new(Default::default()),
        })
    }

    /// The definition in force now, cloned out of the lock before any await.
    pub(crate) fn config(&self) -> Arc<ServiceConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn set_config(&self, config: ServiceConfig) {
        let mut slot = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Arc::new(config);
    }

    async fn register_delivery(&self, delivery_id: String, session_id: String) {
        let now = tokio::time::Instant::now();
        let mut pending = self.pending_deliveries.lock().await;
        pending
            .retain(|_, delivery| now.duration_since(delivery.created_at) < PENDING_DELIVERY_TTL);
        pending.insert(
            delivery_id,
            PendingDelivery {
                session_id,
                created_at: now,
            },
        );
    }

    async fn cancel_delivery(&self, delivery_id: &str) {
        self.pending_deliveries.lock().await.remove(delivery_id);
    }

    pub(crate) async fn restore_delivery(&self, delivery_id: String, session_id: String) {
        self.register_delivery(delivery_id, session_id).await;
    }

    /// Claim an opaque delivery capability for the session-bound reply tool.
    /// A successful claim is one-shot, so duplicate tool calls cannot post the
    /// same delivery twice.
    pub(crate) async fn claim_delivery(&self, session_id: &str, delivery_id: &str) -> bool {
        let now = tokio::time::Instant::now();
        let mut pending = self.pending_deliveries.lock().await;
        pending
            .retain(|_, delivery| now.duration_since(delivery.created_at) < PENDING_DELIVERY_TTL);
        if pending
            .get(delivery_id)
            .is_some_and(|delivery| delivery.session_id == session_id)
        {
            pending.remove(delivery_id);
            true
        } else {
            false
        }
    }
}

/// One service channel's transport-neutral route into Construct sessions.
pub(crate) struct ServiceIngress {
    channel_id: String,
    shared: Arc<ServiceIngressShared>,
}

impl ServiceIngress {
    pub(crate) fn new(channel_id: String, shared: Arc<ServiceIngressShared>) -> Self {
        Self { channel_id, shared }
    }

    pub(super) fn service_name(&self) -> &str {
        &self.shared.name
    }

    pub(super) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub(super) fn current_config(&self) -> Arc<ServiceConfig> {
        self.shared.config()
    }

    pub(super) async fn submit(&self, request: IngressRequest) -> Result<String> {
        Ok(self.submit_tracked(request).await?.session)
    }

    /// Submit a native channel delivery and retain the transcript position at
    /// which its turn began. Long-lived adapters use this cursor to avoid
    /// mistaking the previous turn's final answer for the new one.
    pub(super) async fn submit_tracked(&self, request: IngressRequest) -> Result<IngressReceipt> {
        if request.message.trim().is_empty() {
            return Err(anyhow!("message must not be empty"));
        }
        if let Some(request_id) = request.request_id.as_deref() {
            let mut seen = self.shared.seen_requests.lock().await;
            let request_key = format!("{}:{request_id}", self.channel_id);
            if seen.1.contains(&request_key) {
                return Err(anyhow!("duplicate request_id"));
            }
            seen.0.push_back(request_key.clone());
            seen.1.insert(request_key);
            if seen.0.len() > REQUEST_DEDUP_CAP {
                if let Some(old) = seen.0.pop_front() {
                    seen.1.remove(&old);
                }
            }
        }
        let config = self.shared.config();
        let new_delivery_id = (config.session_mode == ServiceSessionMode::Interactive)
            .then(|| Uuid::new_v4().simple().to_string());
        let key = match config.routing {
            ServiceRouting::PerEvent => None,
            ServiceRouting::Single => Some("__single__".to_string()),
            ServiceRouting::SessionKey => Some(
                request
                    .session_key
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| anyhow!("session_key is required for session-key routing"))?,
            ),
        };
        if let Some(key) = key {
            let lookup_key = format!("{}:{key}", self.channel_id);
            // Keep lookup + creation atomic for this service. Without this,
            // two concurrent first deliveries for the same key would each
            // create a conversation and one would become orphaned.
            let mut state = self.shared.state.lock().await;
            let existing = self.live_session_for_key(&mut state, &lookup_key, &key).await;
            if let Some(id) = existing {
                drop(state);
                let detail = self.shared.manager.detail(&id).await?;
                let event_cursor = detail.events.len();
                let delivery_id = (detail.summary.mode.as_deref() == Some("interactive"))
                    .then(|| Uuid::new_v4().simple().to_string());
                let message = delivery_id
                    .as_deref()
                    .map(|delivery_id| interactive_delivery_prompt(&request.message, delivery_id))
                    .unwrap_or(request.message);
                if let Some(delivery_id) = delivery_id.as_ref() {
                    self.shared
                        .register_delivery(delivery_id.clone(), id.clone())
                        .await;
                }
                // Not `send_input`: an interactive service session is a live
                // agent TUI, and LF-terminated input lands in its composer
                // without submitting. `deliver_user_text` picks the framing
                // the session's harness actually submits on.
                if let Err(error) = self.shared.manager.deliver_user_text(&id, &message).await {
                    if let Some(delivery_id) = delivery_id.as_deref() {
                        self.shared.cancel_delivery(delivery_id).await;
                    }
                    return Err(error);
                }
                return Ok(IngressReceipt {
                    session: id,
                    event_cursor,
                    delivery_id,
                });
            }
            let id = self
                .create(
                    request.message,
                    Some(format!(
                        "service:{}:{}:{key}",
                        self.shared.name, self.channel_id
                    )),
                    new_delivery_id.as_deref(),
                )
                .await?;
            state.sessions.insert(lookup_key, id.clone());
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(IngressReceipt {
                session: id,
                event_cursor: 0,
                delivery_id: new_delivery_id,
            })
        } else {
            let id = self
                .create(
                    request.message,
                    Some(format!("service:{}:{}", self.shared.name, self.channel_id)),
                    new_delivery_id.as_deref(),
                )
                .await?;
            let mut state = self.shared.state.lock().await;
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(IngressReceipt {
                session: id,
                event_cursor: 0,
                delivery_id: new_delivery_id,
            })
        }
    }

    /// Resolve the session currently serving a routing key, if one is alive.
    ///
    /// A routing entry outlives the session it points at: an operator can
    /// delete a routed session at any time, from any client. A dangling entry
    /// is dropped here so the next delivery opens a fresh conversation —
    /// otherwise that key stays stuck, failing every delivery it ever receives
    /// again. Membership is probed rather than the whole session read, so a
    /// transient transcript read error is never mistaken for a deletion.
    ///
    /// The caller holds the state lock, so pruning and the replacement
    /// insert stay atomic with respect to a concurrent delivery on this key.
    async fn live_session_for_key(
        &self,
        state: &mut PersistedState,
        lookup_key: &str,
        key: &str,
    ) -> Option<String> {
        let existing = state.sessions.get(lookup_key).cloned().or_else(|| {
            // State written by the original single-channel v1 runtime used
            // the bare session key. Preserve those conversations when the
            // legacy channel id is still `http`.
            (self.channel_id == "http")
                .then(|| state.sessions.get(key).cloned())
                .flatten()
        })?;
        if self.shared.manager.get_entry(&existing).await.is_some() {
            return Some(existing);
        }
        tracing::info!(
            service = %self.shared.name,
            channel = %self.channel_id,
            session = %existing,
            "routed session no longer exists; the routing key will open a new session"
        );
        // Both the canonical and any legacy bare-key entry point at this
        // session, so clear the id rather than one key.
        state.sessions.retain(|_, session| session != &existing);
        state.owned_sessions.remove(&existing);
        None
    }

    /// Wait for the final assistant answer belonging to one submitted turn.
    /// The transport is cancelled on configuration reload or daemon shutdown.
    pub(super) async fn wait_for_final(
        &self,
        receipt: &IngressReceipt,
        cancel: &CancellationToken,
    ) -> Result<String> {
        if let Some(delivery_id) = receipt.delivery_id.as_deref() {
            return self
                .wait_for_explicit_reply(receipt, delivery_id, cancel)
                .await;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(anyhow!("channel stopped")),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!("service turn timed out"));
            }
            let Ok(detail) = self.shared.manager.detail(&receipt.session).await else {
                continue;
            };
            let events = detail.events.get(receipt.event_cursor..).unwrap_or(&[]);
            let saw_user = events.iter().any(|event| {
                matches!(
                    event.event,
                    SessionEvent::Message {
                        role: MessageRole::User,
                        ..
                    }
                )
            });
            let ready = matches!(
                detail.summary.state,
                SessionState::AwaitingInput | SessionState::Done | SessionState::Errored
            );
            if saw_user && ready {
                if let Some(reply) = latest_assistant_reply(events.iter().map(|event| &event.event))
                {
                    return Ok(reply);
                }
                if detail.summary.state == SessionState::Errored {
                    return Err(anyhow!("service session errored without a final reply"));
                }
            }
        }
    }

    async fn wait_for_explicit_reply(
        &self,
        receipt: &IngressReceipt,
        delivery_id: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let deadline = tokio::time::Instant::now() + PENDING_DELIVERY_TTL;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    self.shared.cancel_delivery(delivery_id).await;
                    return Err(anyhow!("channel stopped"));
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                self.shared.cancel_delivery(delivery_id).await;
                return Err(anyhow!("service turn timed out"));
            }
            let Ok(detail) = self.shared.manager.detail(&receipt.session).await else {
                continue;
            };
            let events = detail.events.get(receipt.event_cursor..).unwrap_or(&[]);
            if let Some(reply) =
                explicit_service_reply(events.iter().map(|event| &event.event), delivery_id)
            {
                return Ok(reply);
            }
            if detail.summary.state == SessionState::Errored {
                self.shared.cancel_delivery(delivery_id).await;
                return Err(anyhow!(
                    "interactive service session errored before replying"
                ));
            }
        }
    }

    async fn persist_state(&self, state: &PersistedState) -> Result<()> {
        let snapshot = serde_json::to_vec_pretty(state)?;
        if let Some(parent) = self.shared.state_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // This is the only record of which session serves which key. Replace
        // it atomically so a crash cannot strand every routed conversation.
        let temporary = self.shared.state_path.with_extension("json.tmp");
        tokio::fs::write(&temporary, snapshot).await?;
        tokio::fs::rename(&temporary, &self.shared.state_path).await?;
        Ok(())
    }

    pub(super) async fn session_result(&self, session_id: &str) -> Result<Option<IngressResult>> {
        let owned = self
            .shared
            .state
            .lock()
            .await
            .owned_sessions
            .contains(session_id);
        if !owned {
            return Ok(None);
        }
        let Ok(detail) = self.shared.manager.detail(session_id).await else {
            return Ok(None);
        };
        let reply = latest_assistant_reply(detail.events.iter().map(|event| &event.event));
        let ready = matches!(
            detail.summary.state,
            SessionState::AwaitingInput | SessionState::Done | SessionState::Errored
        );

        // A turn stopped at an approval looks exactly like a slow turn from
        // outside, so say which it is. Left unsaid, a caller polls a session
        // that will never move until a human it cannot reach acts.
        let mut approval = None;
        if let Some(pending) = pending_approval(&detail.events) {
            let waited = (chrono::Utc::now() - pending.since).num_seconds().max(0);
            let timeout = self.shared.config().approval_timeout_secs;
            if timeout > 0 && waited >= timeout as i64 {
                // Nobody answered in the window the operator allowed, so stop
                // holding the caller: deny, let the turn resume, and report the
                // refusal rather than reporting "still working" forever.
                let _ = self
                    .shared
                    .manager
                    .tool_decision(session_id, pending.call_id, "deny".to_string())
                    .await;
                tracing::info!(
                    service = %self.shared.name,
                    session = %session_id,
                    tool = %pending.tool,
                    waited,
                    "service approval timed out; denied"
                );
                approval = Some(IngressApproval {
                    tool: pending.tool,
                    summary: None,
                    waited_seconds: waited,
                    outcome: "denied_on_timeout",
                });
            } else {
                approval = Some(IngressApproval {
                    tool: pending.tool,
                    summary: Some(pending.summary),
                    waited_seconds: waited,
                    outcome: "awaiting_operator",
                });
            }
        }

        Ok(Some(IngressResult {
            service: self.shared.name.clone(),
            channel: self.channel_id.clone(),
            session: session_id.to_string(),
            status: detail.summary.state,
            ready,
            reply,
            approval,
        }))
    }

    async fn create(
        &self,
        message: String,
        title: Option<String>,
        delivery_id: Option<&str>,
    ) -> Result<String> {
        // Use one definition snapshot for the whole creation so a reload
        // cannot combine fields from two versions of a service.
        let config = self.shared.config();
        let prompt = service_seed_prompt(&config.instruction, message, delivery_id);
        let model = service_session_model(&config.harness, config.model.as_deref())?;
        let interactive = config.session_mode == ServiceSessionMode::Interactive;
        let env = service_session_env(&config);
        let id = self
            .shared
            .manager
            .create(CreateSessionParams {
                harness: config.harness.clone(),
                cwd: config.cwd.clone(),
                prompt: Some(prompt),
                model,
                title,
                mode: Some(config.session_mode.as_str().to_string()),
                pty_size: interactive.then_some(PtySize {
                    cols: 120,
                    rows: 40,
                }),
                worktree: false,
                env,
                args: Vec::new(),
                kind: SessionKind::User,
                parent_session_id: None,
                group_id: None,
                position_after_session_id: None,
                forked_from: None,
            })
            .await?;
        // Registered as soon as the session exists. The seed prompt starts the
        // turn, but the harness still has to boot and connect its MCP client
        // before it can call the reply tool, so the id is always pending by
        // the time a reply is possible.
        if let Some(delivery_id) = delivery_id {
            self.shared
                .register_delivery(delivery_id.to_string(), id.clone())
                .await;
        }
        Ok(id)
    }
}

fn service_session_env(config: &ServiceConfig) -> HashMap<String, String> {
    let mut env = config.sandbox.session_env();
    if config.session_mode == ServiceSessionMode::Interactive {
        env.insert("CONSTRUCT_INJECT_MCP".to_string(), "1".to_string());
        env.insert(
            construct_protocol::adapter::SERVICE_REPLY_TOOL_ENV.to_string(),
            "1".to_string(),
        );
        if !config.sandbox.mcp {
            env.insert(
                construct_protocol::adapter::SERVICE_REPLY_ONLY_MCP_ENV.to_string(),
                "1".to_string(),
            );
        }
    }
    env
}

/// Compose the seed prompt a service session is created with: the service's
/// standing instruction, this delivery's message, and — for an interactive
/// session — the binding that tells the agent which delivery id its reply
/// belongs to.
///
/// The opening delivery goes in here rather than being written into the
/// session afterwards (spec 0177). A freshly spawned agent TUI has not
/// attached its input handler yet, and the terminal discards anything written
/// before the harness switches it into raw mode, so a post-spawn write of the
/// first message is silently lost. As the seed prompt it reaches the adapter
/// as structured data and starts the first turn natively (spec 0046).
fn service_seed_prompt(instruction: &str, message: String, delivery_id: Option<&str>) -> String {
    let prompt = if instruction.trim().is_empty() {
        message
    } else {
        format!("{}\n\n{}", instruction.trim(), message)
    };
    match delivery_id {
        Some(delivery_id) => interactive_delivery_prompt(&prompt, delivery_id),
        None => prompt,
    }
}

fn interactive_delivery_prompt(message: &str, delivery_id: &str) -> String {
    format!(
        "{message}\n\n[Construct service delivery {delivery_id}]\n\
         Send the caller-facing final response with the `construct_service_reply` tool using \
         delivery_id `{delivery_id}`. The tool is the only delivery path; do not choose a \
         workspace, channel, recipient, or thread yourself."
    )
}

/// Turn a durable service model selection into the id accepted by the
/// session's native model surface. Plain harness-native models pass through;
/// Construct route/model ids retain both halves and receive Claude's special
/// gateway prefix only when a Claude session is actually spawned.
fn service_session_model(harness: &str, model: Option<&str>) -> Result<Option<String>> {
    let Some(model) = model else {
        return Ok(None);
    };
    Ok(Some(
        construct_protocol::published_model::published_model_id_for_harness_from_id(harness, model)?
            .unwrap_or_else(|| model.to_string()),
    ))
}

pub(super) struct IngressReceipt {
    pub(super) session: String,
    event_cursor: usize,
    delivery_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct IngressRequest {
    pub(super) message: String,
    #[serde(default)]
    pub(super) session_key: Option<String>,
    #[serde(default)]
    pub(super) request_id: Option<String>,
}

#[derive(Serialize)]
pub(super) struct IngressResult {
    pub(super) service: String,
    pub(super) channel: String,
    pub(super) session: String,
    pub(super) status: SessionState,
    pub(super) ready: bool,
    pub(super) reply: Option<String>,
    pub(super) approval: Option<IngressApproval>,
}

#[derive(Serialize)]
pub(super) struct IngressApproval {
    pub(super) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<String>,
    pub(super) waited_seconds: i64,
    pub(super) outcome: &'static str,
}

/// Reconstruct the caller-facing reply from a session transcript.
///
/// Harnesses stream an assistant turn as many small `Message` events (one per
/// token delta), so the reply is the *concatenation* of the trailing assistant
/// run, not its last element. Walking backwards, transcript bookkeeping
/// (status, cost, usage, reasoning) is skipped, and collection stops at the
/// first boundary that ends the turn's final answer — the user's own message,
/// or a tool call whose narration precedes it — so a tool-using turn reports
/// the answer rather than the commentary leading up to it.
///
/// A tool *result* that trails the answer is not such a boundary. Harnesses do
/// not guarantee that a tool's result is recorded before the assistant text it
/// produced: a result flushed afterwards lands past the final answer and, read
/// as a boundary, hides it completely. The turn then looks answerless — a
/// polling caller sees `ready` with no reply, and a waiting one blocks until
/// its timeout even though the answer is sitting in the transcript. Trailing
/// results are therefore skipped until collection has actually begun; once
/// some answer text is in hand, any tool event is again the boundary that ends
/// it.
pub(super) fn latest_assistant_reply<'a>(
    events: impl DoubleEndedIterator<Item = &'a SessionEvent>,
) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for event in events.rev() {
        match event {
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text,
            } => parts.push(text.as_str()),
            SessionEvent::ToolResult { .. } if parts.is_empty() => {}
            SessionEvent::Message { .. }
            | SessionEvent::ToolUse { .. }
            | SessionEvent::ToolResult { .. } => break,
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.concat())
}

fn explicit_service_reply<'a>(
    events: impl Iterator<Item = &'a SessionEvent>,
    delivery_id: &str,
) -> Option<String> {
    let mut matched_tool = false;
    for event in events {
        match event {
            SessionEvent::ToolUse { tool, args, .. }
                if tool == "construct_service_reply"
                    && args.get("delivery_id").and_then(serde_json::Value::as_str)
                        == Some(delivery_id) =>
            {
                matched_tool = true;
            }
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text,
            } if matched_tool => return Some(text.clone()),
            _ => {}
        }
    }
    None
}

/// A tool call this session is stopped at, waiting for the operator.
pub(super) struct PendingApproval {
    pub(super) call_id: String,
    pub(super) tool: String,
    pub(super) summary: String,
    since: chrono::DateTime<chrono::Utc>,
}

/// The approval a turn is currently stopped at, if any.
///
/// Resolutions are not recorded in the transcript, so a pending approval is
/// identified positionally: it is pending exactly when the request is the last
/// thing of consequence in the transcript. Once the operator answers, the
/// turn appends past it and the request stops trailing.
pub(super) fn pending_approval(
    events: &[construct_protocol::TimestampedEvent],
) -> Option<PendingApproval> {
    for event in events.iter().rev() {
        match &event.event {
            SessionEvent::ToolApprovalRequest {
                call_id,
                tool,
                args_summary,
                ..
            } => {
                return Some(PendingApproval {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    summary: args_summary.clone(),
                    since: event.at,
                })
            }
            SessionEvent::Message { .. }
            | SessionEvent::ToolUse { .. }
            | SessionEvent::ToolResult { .. } => return None,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn shared_for_delivery_tests() -> Arc<ServiceIngressShared> {
        let tmp = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let (manager, _remote_rx, _restart_rx) = crate::session::SessionManager::new(
            storage,
            Arc::new(crate::config::Config::default()),
            tmp.path().join("run"),
        )
        .await
        .expect("session manager");
        ServiceIngressShared::load(
            "svc".to_string(),
            ServiceConfig {
                instruction: String::new(),
                harness: "codex".into(),
                model: None,
                session_mode: ServiceSessionMode::Interactive,
                cwd: ".".into(),
                routing: ServiceRouting::SessionKey,
                paused: false,
                approval_timeout_secs: 0,
                sandbox: super::super::ServiceSandboxConfig::default(),
                channels: Default::default(),
            },
            Arc::new(manager),
            tmp.path().join("data"),
        )
    }

    #[test]
    fn result_serialization_preserves_the_http_v1_shape() {
        let result = IngressResult {
            service: "alerts".into(),
            channel: "http".into(),
            session: "s123".into(),
            status: SessionState::Running,
            ready: false,
            reply: None,
            approval: Some(IngressApproval {
                tool: "shell".into(),
                summary: Some("run tests".into()),
                waited_seconds: 9,
                outcome: "awaiting_operator",
            }),
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "service": "alerts",
                "channel": "http",
                "session": "s123",
                "status": "running",
                "ready": false,
                "reply": null,
                "approval": {
                    "tool": "shell",
                    "summary": "run tests",
                    "waited_seconds": 9,
                    "outcome": "awaiting_operator",
                },
            })
        );
    }

    #[test]
    fn timed_out_approval_omits_the_summary_like_http_v1() {
        let approval = IngressApproval {
            tool: "shell".into(),
            summary: None,
            waited_seconds: 120,
            outcome: "denied_on_timeout",
        };

        assert_eq!(
            serde_json::to_value(approval).unwrap(),
            serde_json::json!({
                "tool": "shell",
                "waited_seconds": 120,
                "outcome": "denied_on_timeout",
            })
        );
    }

    #[test]
    fn service_gateway_model_keeps_its_route_when_spawning_codex() {
        assert_eq!(
            service_session_model("codex", Some("construct-claude-oauth/sonnet")).unwrap(),
            Some("construct-claude-oauth/sonnet".into())
        );
    }

    #[test]
    fn service_gateway_model_uses_claudes_native_gateway_prefix() {
        assert_eq!(
            service_session_model("claude", Some("construct-codex-oauth/gpt-5.6-sol")).unwrap(),
            Some("claude-construct-codex-oauth/gpt-5.6-sol".into())
        );
    }

    #[test]
    fn service_native_model_passes_through_unchanged() {
        assert_eq!(
            service_session_model("codex", Some("gpt-5.6-sol")).unwrap(),
            Some("gpt-5.6-sol".into())
        );
    }

    #[test]
    fn interactive_prompt_binds_the_reply_tool_to_the_delivery() {
        let prompt = interactive_delivery_prompt("hello", "delivery-123");
        assert!(prompt.starts_with("hello\n\n"));
        assert!(prompt.contains("construct_service_reply"));
        assert_eq!(prompt.matches("delivery-123").count(), 2);
        assert!(prompt.contains("do not choose a workspace, channel, recipient, or thread"));
    }

    #[test]
    fn interactive_first_delivery_is_carried_by_the_seed_prompt() {
        // Spec 0177: the opening channel message must reach the harness as the
        // session's seed prompt, delivery binding included. Returning the bare
        // message here would mean the daemon had to write it into the PTY
        // after spawn, which is exactly the race that silently swallowed the
        // first Slack message of a thread.
        let prompt = service_seed_prompt("Be brief.", "ship it".to_string(), Some("d-1"));

        assert!(prompt.starts_with("Be brief.\n\nship it\n\n"));
        assert!(prompt.contains("construct_service_reply"));
        assert_eq!(prompt.matches("d-1").count(), 2);
    }

    #[test]
    fn headless_seed_prompt_carries_no_delivery_binding() {
        assert_eq!(
            service_seed_prompt("  ", "ship it".to_string(), None),
            "ship it"
        );
        assert_eq!(
            service_seed_prompt("Be brief.", "ship it".to_string(), None),
            "Be brief.\n\nship it"
        );
    }

    #[test]
    fn explicit_reply_requires_the_matching_delivery_tool_call() {
        let events = [
            SessionEvent::ToolUse {
                tool: "construct_service_reply".into(),
                args: serde_json::json!({ "delivery_id": "other" }),
                call_id: Some("other".into()),
            },
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "wrong".into(),
            },
            SessionEvent::ToolUse {
                tool: "construct_service_reply".into(),
                args: serde_json::json!({ "delivery_id": "wanted" }),
                call_id: Some("wanted".into()),
            },
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "right".into(),
            },
        ];

        assert_eq!(
            explicit_service_reply(events.iter(), "wanted").as_deref(),
            Some("right")
        );
        assert_eq!(explicit_service_reply(events.iter(), "missing"), None);
    }

    /// Deleting a routed session must not brick its routing key.
    ///
    /// Nothing prunes the routing map when a session is deleted — the operator
    /// can do that from any client, and the service never hears about it — so
    /// the delivery path has to tolerate an entry pointing at a session that is
    /// gone. Left unhandled, every later delivery on that key failed on the
    /// missing session instead of starting a new conversation.
    #[tokio::test]
    async fn a_deleted_routed_session_frees_its_routing_key() {
        let shared = shared_for_delivery_tests().await;
        let ingress = ServiceIngress::new("http".to_string(), shared);
        let mut state = PersistedState::default();
        state
            .sessions
            .insert("http:caller-a".into(), "deleted-session".into());
        state
            .sessions
            .insert("http:caller-b".into(), "other-session".into());
        state.owned_sessions.insert("deleted-session".into());
        state.owned_sessions.insert("other-session".into());

        let resolved = ingress
            .live_session_for_key(&mut state, "http:caller-a", "caller-a")
            .await;

        assert_eq!(
            resolved, None,
            "a routing entry whose session no longer exists must not be reused"
        );
        assert!(
            !state.sessions.contains_key("http:caller-a"),
            "the dangling entry must be dropped so the key can be routed again"
        );
        assert!(!state.owned_sessions.contains("deleted-session"));
        assert_eq!(
            state.sessions.get("http:caller-b").map(String::as_str),
            Some("other-session"),
            "pruning one key must leave every other conversation routed"
        );
        assert!(state.owned_sessions.contains("other-session"));
    }

    /// `single` routing shares this resolution path under a fixed key, so the
    /// deleted-session recovery has to hold for it too.
    #[tokio::test]
    async fn a_deleted_single_routing_session_frees_the_shared_key() {
        let shared = shared_for_delivery_tests().await;
        let ingress = ServiceIngress::new("slack1".to_string(), shared);
        let mut state = PersistedState::default();
        state
            .sessions
            .insert("slack1:__single__".into(), "deleted-session".into());
        state.owned_sessions.insert("deleted-session".into());

        let resolved = ingress
            .live_session_for_key(&mut state, "slack1:__single__", "__single__")
            .await;

        assert_eq!(resolved, None);
        assert!(state.sessions.is_empty());
        assert!(state.owned_sessions.is_empty());
    }

    /// The v1 http state keyed conversations by the bare session key. Those
    /// entries are still honored, so they must be prunable on the same terms.
    #[tokio::test]
    async fn a_deleted_legacy_http_session_frees_its_bare_routing_key() {
        let shared = shared_for_delivery_tests().await;
        let ingress = ServiceIngress::new("http".to_string(), shared);
        let mut state = PersistedState::default();
        state
            .sessions
            .insert("caller-a".into(), "deleted-session".into());
        state.owned_sessions.insert("deleted-session".into());

        let resolved = ingress
            .live_session_for_key(&mut state, "http:caller-a", "caller-a")
            .await;

        assert_eq!(resolved, None);
        assert!(
            state.sessions.is_empty(),
            "the legacy bare-key entry must be dropped, not just the canonical one"
        );
        assert!(state.owned_sessions.is_empty());
    }

    /// An unrouted key is not a deletion: nothing to prune, nothing to reuse.
    #[tokio::test]
    async fn an_unknown_routing_key_resolves_to_no_session() {
        let shared = shared_for_delivery_tests().await;
        let ingress = ServiceIngress::new("http".to_string(), shared);
        let mut state = PersistedState::default();
        state
            .sessions
            .insert("http:caller-b".into(), "other-session".into());

        let resolved = ingress
            .live_session_for_key(&mut state, "http:caller-a", "caller-a")
            .await;

        assert_eq!(resolved, None);
        assert_eq!(
            state.sessions.get("http:caller-b").map(String::as_str),
            Some("other-session")
        );
    }

    #[tokio::test]
    async fn delivery_claim_is_session_bound_and_one_shot() {
        let shared = shared_for_delivery_tests().await;
        shared
            .register_delivery("delivery-123".into(), "session-a".into())
            .await;

        assert!(!shared.claim_delivery("session-b", "delivery-123").await);
        assert!(shared.claim_delivery("session-a", "delivery-123").await);
        assert!(!shared.claim_delivery("session-a", "delivery-123").await);
    }

    #[test]
    fn interactive_service_mcp_profile_is_least_privilege_unless_granted() {
        let mut config = ServiceConfig {
            instruction: String::new(),
            harness: "codex".into(),
            model: None,
            session_mode: ServiceSessionMode::Interactive,
            cwd: ".".into(),
            routing: ServiceRouting::SessionKey,
            paused: false,
            approval_timeout_secs: 0,
            sandbox: super::super::ServiceSandboxConfig::default(),
            channels: Default::default(),
        };

        let confined = service_session_env(&config);
        assert_eq!(confined.get("CONSTRUCT_INJECT_MCP").map(String::as_str), Some("1"));
        assert_eq!(
            confined
                .get(construct_protocol::adapter::SERVICE_REPLY_TOOL_ENV)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            confined
                .get(construct_protocol::adapter::SERVICE_REPLY_ONLY_MCP_ENV)
                .map(String::as_str),
            Some("1")
        );

        config.sandbox.mcp = true;
        let granted = service_session_env(&config);
        assert_eq!(granted.get("CONSTRUCT_INJECT_MCP").map(String::as_str), Some("1"));
        assert_eq!(
            granted
                .get(construct_protocol::adapter::SERVICE_REPLY_TOOL_ENV)
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            granted.get(construct_protocol::adapter::SERVICE_REPLY_ONLY_MCP_ENV),
            None,
            "general MCP grants keep plugin tools, including an installed Slack MCP, available"
        );
    }
}
