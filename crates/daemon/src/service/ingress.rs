//! Transport-neutral service ingress.
//!
//! Channel adapters turn their native deliveries into [`IngressRequest`]s and
//! hand them to [`ServiceIngress`]. Session routing, ownership, request
//! deduplication, result reconstruction, and approval handling live here so a
//! channel does not need to reimplement service semantics.

use super::{ServiceConfig, ServiceRouting};
use crate::session::SessionManager;
use anyhow::{anyhow, Result};
use construct_protocol::{
    CreateSessionParams, MessageRole, SessionEvent, SessionKind, SessionState,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const REQUEST_DEDUP_CAP: usize = 4096;

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
            let existing = state.sessions.get(&lookup_key).cloned().or_else(|| {
                // State written by the original single-channel v1 runtime used
                // the bare session key. Preserve those conversations when the
                // legacy channel id is still `http`.
                (self.channel_id == "http")
                    .then(|| state.sessions.get(&key).cloned())
                    .flatten()
            });
            if let Some(id) = existing {
                drop(state);
                let event_cursor = self
                    .shared
                    .manager
                    .detail(&id)
                    .await
                    .map(|detail| detail.events.len())
                    .unwrap_or(0);
                self.shared.manager.send_input(&id, request.message).await?;
                return Ok(IngressReceipt {
                    session: id,
                    event_cursor,
                });
            }
            let id = self
                .create(
                    request.message,
                    Some(format!(
                        "service:{}:{}:{key}",
                        self.shared.name, self.channel_id
                    )),
                )
                .await?;
            state.sessions.insert(lookup_key, id.clone());
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(IngressReceipt {
                session: id,
                event_cursor: 0,
            })
        } else {
            let id = self
                .create(
                    request.message,
                    Some(format!("service:{}:{}", self.shared.name, self.channel_id)),
                )
                .await?;
            let mut state = self.shared.state.lock().await;
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(IngressReceipt {
                session: id,
                event_cursor: 0,
            })
        }
    }

    /// Wait for the final assistant answer belonging to one submitted turn.
    /// The transport is cancelled on configuration reload or daemon shutdown.
    pub(super) async fn wait_for_final(
        &self,
        receipt: &IngressReceipt,
        cancel: &CancellationToken,
    ) -> Result<String> {
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

    async fn create(&self, message: String, title: Option<String>) -> Result<String> {
        // Use one definition snapshot for the whole creation so a reload
        // cannot combine fields from two versions of a service.
        let config = self.shared.config();
        let prompt = if config.instruction.trim().is_empty() {
            message
        } else {
            format!("{}\n\n{}", config.instruction.trim(), message)
        };
        let model = service_session_model(&config.harness, config.model.as_deref())?;
        self.shared
            .manager
            .create(CreateSessionParams {
                harness: config.harness.clone(),
                cwd: config.cwd.clone(),
                prompt: Some(prompt),
                model,
                title,
                mode: Some("headless".to_string()),
                pty_size: None,
                worktree: false,
                env: config.sandbox.session_env(),
                args: Vec::new(),
                kind: SessionKind::User,
                parent_session_id: None,
                group_id: None,
                position_after_session_id: None,
                forked_from: None,
            })
            .await
    }
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
}
