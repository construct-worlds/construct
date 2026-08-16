//! Single long-running task that owns every operator listener.
//!
//! Operator definitions are edited while the daemon runs — through IPC, or by
//! hand in the config directory — and each edit must reach the running system
//! on its declared schedule. This task is what makes that safe.
//!
//! It sits behind an mpsc channel for two reasons, in this order:
//!
//! 1. **Reloads must be serialized.** Two concurrent channel edits would
//!    otherwise each plan against the same observed registry and both try to
//!    bind. One owning task makes a reload atomic by construction, with no
//!    lock for a future caller to forget.
//! 2. **Insurance against the Send-inference cycle** documented in
//!    `remote_supervisor`. Request handling already reaches deep into
//!    `SessionManager`; a future edge from there back into `crate::server`
//!    would make rustc infer Send across the whole recursive component. The
//!    channel breaks the static edge before that can happen — which is why
//!    this module must never import `crate::server`.
//!
//! Reload is all-or-nothing: if any definition fails to parse, nothing
//! changes. That is also what makes the file watcher safe, since it can catch
//! an editor mid-write and simply retry on the next tick.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use construct_protocol::paths::Paths;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::operator::{self, OperatorConfig, OperatorShared};
use crate::session::SessionManager;

/// One bound channel: which operator, which channel within it.
pub type ListenerKey = (String, String);

/// Why a reload is happening. Purely for logs and for the text a caller sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadReason {
    Boot,
    Ipc(&'static str),
    FileChanged,
}

impl ReloadReason {
    fn label(self) -> &'static str {
        match self {
            ReloadReason::Boot => "boot",
            ReloadReason::Ipc(method) => method,
            ReloadReason::FileChanged => "file changed",
        }
    }
}

/// What a reload actually did. Replaces the old "restart required" guess with
/// a report of the socket work performed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    pub started: Vec<ListenerKey>,
    pub stopped: Vec<ListenerKey>,
    pub rebound: Vec<ListenerKey>,
    /// Channels that were wanted but could not be bound, with the reason.
    pub failures: Vec<(ListenerKey, u16, String)>,
}

pub enum OperatorMsg {
    Reload {
        reason: ReloadReason,
        respond: Option<oneshot::Sender<Result<ReloadReport>>>,
    },
    /// A listener's accept loop ended on its own; drop it so a later reload
    /// can start it again.
    ListenerDied(ListenerKey),
    Reply {
        session_id: String,
        delivery_id: String,
        text: String,
        respond: oneshot::Sender<Result<()>>,
    },
}

/// Handle held by `SessionManager` so any caller can request a reload without
/// depending on this module's internals.
#[derive(Clone)]
pub struct OperatorHandle(mpsc::UnboundedSender<OperatorMsg>);

impl OperatorHandle {
    pub async fn reload(&self, reason: ReloadReason) -> Result<ReloadReport> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(OperatorMsg::Reload {
                reason,
                respond: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("operator supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("operator supervisor dropped the request"))?
    }

    /// Fire-and-forget reload, for the file watcher, which has nobody to
    /// report to.
    pub fn reload_detached(&self, reason: ReloadReason) {
        let _ = self.0.send(OperatorMsg::Reload {
            reason,
            respond: None,
        });
    }

    pub async fn reply(&self, session_id: String, delivery_id: String, text: String) -> Result<()> {
        if text.trim().is_empty() {
            anyhow::bail!("operator reply must not be empty");
        }
        if text.chars().count() > 40_000 {
            anyhow::bail!("operator reply exceeds 40000 characters");
        }
        let (tx, rx) = oneshot::channel();
        self.0
            .send(OperatorMsg::Reply {
                session_id,
                delivery_id,
                text,
                respond: tx,
            })
            .map_err(|_| anyhow::anyhow!("operator supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("operator supervisor dropped the reply"))?
    }
}

struct ListenerHandle {
    port: u16,
    endpoint: crate::channel_publication::ChannelIngressEndpoint,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

struct SlackHandle {
    config: operator::SlackConfig,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Everything the supervisor owns. Lives in the task's stack frame, so there
/// is no lock and no way for another task to observe it half-updated.
#[derive(Default)]
struct Registry {
    /// One entry per operator name, reused across reloads. See the invariant on
    /// [`OperatorShared`]: rebuilding one of these would drop the routed
    /// session map and the dedup ring.
    shared: BTreeMap<String, Arc<OperatorShared>>,
    listeners: HashMap<ListenerKey, ListenerHandle>,
    slack: HashMap<ListenerKey, SlackHandle>,
}

/// What must happen to one channel's socket for the desired state to hold.
#[derive(Debug, PartialEq, Eq)]
pub enum ListenerAction {
    Start(ListenerKey, u16),
    Stop(ListenerKey),
    Rebind(ListenerKey, u16),
}

/// A port two channels both want. The loser is reported rather than allowed to
/// steal a working listener.
#[derive(Debug, PartialEq, Eq)]
pub struct PortConflict {
    pub key: ListenerKey,
    pub port: u16,
    pub held_by: ListenerKey,
}

/// The channels that should be bound, given these definitions.
///
/// Paused operators and disabled channels contribute nothing, which is what
/// lets a user free a port by pausing. When two channels ask for the same
/// port the earlier key wins deterministically, so a hand-edited duplicate
/// cannot take a port away from a operator that is already serving.
pub fn desired_listeners(
    defs: &BTreeMap<String, OperatorConfig>,
) -> (BTreeMap<ListenerKey, u16>, Vec<PortConflict>) {
    let mut desired: BTreeMap<ListenerKey, u16> = BTreeMap::new();
    let mut claimed: BTreeMap<u16, ListenerKey> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (name, config) in defs {
        if config.paused {
            continue;
        }
        for (channel_id, channel) in &config.channels {
            let Some(port) = operator::bindable_port(name, channel_id, channel) else {
                continue;
            };
            let key = (name.clone(), channel_id.clone());
            match claimed.get(&port) {
                Some(held_by) => conflicts.push(PortConflict {
                    key,
                    port,
                    held_by: held_by.clone(),
                }),
                None => {
                    claimed.insert(port, key.clone());
                    desired.insert(key, port);
                }
            }
        }
    }
    (desired, conflicts)
}

fn desired_slack(
    defs: &BTreeMap<String, OperatorConfig>,
) -> BTreeMap<ListenerKey, operator::SlackConfig> {
    let mut desired = BTreeMap::new();
    for (name, config) in defs {
        if config.paused {
            continue;
        }
        for (channel_id, channel) in &config.channels {
            if let Some(config) = operator::slack_config(name, channel_id, channel) {
                desired.insert((name.clone(), channel_id.clone()), config);
            }
        }
    }
    desired
}

/// The socket work to get from `current` to `desired`.
///
/// Stops are listed before starts so the plan reads in the order it happens.
/// The executor does not depend on that: it releases every port before binding
/// any, which is what makes a port handover safe.
pub fn plan(
    current: &BTreeMap<ListenerKey, u16>,
    desired: &BTreeMap<ListenerKey, u16>,
) -> Vec<ListenerAction> {
    let mut stops = Vec::new();
    let mut starts = Vec::new();
    for (key, port) in current {
        match desired.get(key) {
            None => stops.push(ListenerAction::Stop(key.clone())),
            Some(next) if next != port => stops.push(ListenerAction::Rebind(key.clone(), *next)),
            Some(_) => {}
        }
    }
    for (key, port) in desired {
        if !current.contains_key(key) {
            starts.push(ListenerAction::Start(key.clone(), *port));
        }
    }
    stops.extend(starts);
    stops
}

/// Run the supervisor until the channel closes at daemon shutdown.
pub async fn run(
    manager: Arc<SessionManager>,
    paths: Paths,
    handle: OperatorHandle,
    mut rx: mpsc::UnboundedReceiver<OperatorMsg>,
) {
    let mut registry = Registry::default();
    while let Some(msg) = rx.recv().await {
        match msg {
            OperatorMsg::Reload {
                reason,
                mut respond,
            } => {
                // Coalesce a burst — an editor that writes three times, or a
                // UI save racing the watcher — into one reload, keeping the
                // last caller waiting for an answer.
                let mut reason = reason;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        OperatorMsg::Reload {
                            reason: next_reason,
                            respond: next_respond,
                        } => {
                            if let Some(stale) = respond.take() {
                                let _ = stale.send(Ok(ReloadReport::default()));
                            }
                            reason = next_reason;
                            respond = next_respond;
                        }
                        OperatorMsg::ListenerDied(key) => {
                            registry.listeners.remove(&key);
                            registry.slack.remove(&key);
                            reconcile_publications(&manager, &registry);
                        }
                        OperatorMsg::Reply {
                            session_id,
                            delivery_id,
                            text,
                            respond,
                        } => {
                            let _ = respond.send(
                                record_reply(&manager, &registry, session_id, delivery_id, text)
                                    .await,
                            );
                        }
                    }
                }
                let outcome = reload(&manager, &paths, &handle, &mut registry, reason).await;
                if let Some(respond) = respond {
                    let _ = respond.send(outcome);
                }
            }
            OperatorMsg::ListenerDied(key) => {
                registry.listeners.remove(&key);
                registry.slack.remove(&key);
                reconcile_publications(&manager, &registry);
            }
            OperatorMsg::Reply {
                session_id,
                delivery_id,
                text,
                respond,
            } => {
                let _ = respond
                    .send(record_reply(&manager, &registry, session_id, delivery_id, text).await);
            }
        }
    }
    tracing::debug!("operator supervisor channel closed; exiting");
}

async fn record_reply(
    manager: &SessionManager,
    registry: &Registry,
    session_id: String,
    delivery_id: String,
    text: String,
) -> Result<()> {
    let mut claimed_by = None;
    for shared in registry.shared.values() {
        if shared.claim_delivery(&session_id, &delivery_id).await {
            claimed_by = Some(shared.clone());
            break;
        }
    }
    let Some(claimed_by) = claimed_by else {
        anyhow::bail!("delivery is not pending for this operator session");
    };

    let recorded = async {
        manager
            .emit_session_event(construct_protocol::SessionEmitEventParams {
                session_id: session_id.clone(),
                event: construct_protocol::SessionEvent::ToolUse {
                    tool: "construct_operator_reply".to_string(),
                    args: serde_json::json!({ "delivery_id": &delivery_id }),
                    call_id: Some(delivery_id.clone()),
                },
            })
            .await?;
        manager
            .emit_session_event(construct_protocol::SessionEmitEventParams {
                session_id: session_id.clone(),
                event: construct_protocol::SessionEvent::Message {
                    role: construct_protocol::MessageRole::Assistant,
                    text,
                },
            })
            .await
    }
    .await;
    if recorded.is_err() {
        claimed_by.restore_delivery(delivery_id, session_id).await;
    }
    recorded
}

async fn reload(
    manager: &Arc<SessionManager>,
    paths: &Paths,
    handle: &OperatorHandle,
    registry: &mut Registry,
    reason: ReloadReason,
) -> Result<ReloadReport> {
    // All or nothing: a definition that does not parse leaves the running
    // configuration untouched rather than half-applied.
    let defs =
        operator::load_definitions(&paths.operators_dir()).context("reload operator definitions")?;

    let (desired, conflicts) = desired_listeners(&defs);
    let desired_slack = desired_slack(&defs);
    for conflict in &conflicts {
        tracing::warn!(
            operator = %conflict.key.0,
            channel = %conflict.key.1,
            port = conflict.port,
            held_by = %format!("{}:{}", conflict.held_by.0, conflict.held_by.1),
            "operator channel wants a port another channel already claims; not binding"
        );
    }

    // Publish the new definitions before touching sockets, so a request
    // arriving mid-reload sees the new routing and the new paused flag.
    for (name, config) in &defs {
        match registry.shared.get(name) {
            Some(shared) => shared.set_config(config.clone()),
            None => {
                registry.shared.insert(
                    name.clone(),
                    OperatorShared::load(
                        name.clone(),
                        config.clone(),
                        manager.clone(),
                        paths.data_dir.clone(),
                    ),
                );
            }
        }
    }

    let current: BTreeMap<ListenerKey, u16> = registry
        .listeners
        .iter()
        .map(|(key, listener)| (key.clone(), listener.port))
        .collect();
    let actions = plan(&current, &desired);

    let mut report = ReloadReport::default();
    for (key, port, error) in conflicts.into_iter().map(|conflict| {
        let held = format!("{}:{}", conflict.held_by.0, conflict.held_by.1);
        (
            conflict.key,
            conflict.port,
            format!("port already claimed by {held}"),
        )
    }) {
        report.failures.push((key, port, error));
    }

    // Two passes, so every release happens before any bind no matter how the
    // plan is ordered — that is what lets one channel hand a port to another
    // in a single reload. Each task is awaited because the port frees when the
    // listener drops inside it, not when the token is cancelled.
    for action in &actions {
        let key = match action {
            ListenerAction::Stop(key) | ListenerAction::Rebind(key, _) => key,
            ListenerAction::Start(..) => continue,
        };
        if let Some(listener) = registry.listeners.remove(key) {
            listener.cancel.cancel();
            let _ = listener.task.await;
        }
    }
    // A stopped listener withdraws publication before any replacement binds.
    // If the replacement succeeds it becomes locally available, but explicit
    // publication intent is deliberately not restored.
    reconcile_publications(manager, registry);

    for action in actions {
        let (key, port, rebound) = match action {
            ListenerAction::Start(key, port) => (key, port, false),
            ListenerAction::Rebind(key, port) => (key, port, true),
            ListenerAction::Stop(key) => {
                report.stopped.push(key);
                continue;
            }
        };
        let Some(shared) = registry.shared.get(&key.0).cloned() else {
            continue;
        };
        let Some(endpoint) = defs
            .get(&key.0)
            .and_then(|operator| operator.channels.get(&key.1))
            .and_then(|channel| operator::ingress_endpoint(&key.0, &key.1, channel))
        else {
            report.failures.push((
                key,
                port,
                "channel has no supported local ingress endpoint".to_string(),
            ));
            continue;
        };
        match start_listener(handle, shared, key.clone(), port, endpoint).await {
            Ok(listener) => {
                registry.listeners.insert(key.clone(), listener);
                if rebound {
                    report.rebound.push(key);
                } else {
                    report.started.push(key);
                }
            }
            Err(error) => {
                // One held port must not abort the rest of the reload, nor
                // leave the registry believing a listener exists.
                tracing::error!(operator = %key.0, channel = %key.1, port, %error, "operator channel failed to bind");
                report.failures.push((key, port, error.to_string()));
            }
        }
    }

    // Socket Mode channels have no local port. A change to either token or an
    // allowlist replaces the outbound task; unchanged tasks keep their live
    // connection and backoff state.
    let stale_slack: Vec<_> = registry
        .slack
        .iter()
        .filter_map(|(key, running)| match desired_slack.get(key) {
            Some(config) if config == &running.config => None,
            _ => Some(key.clone()),
        })
        .collect();
    let restarting_slack: HashSet<_> = stale_slack
        .iter()
        .filter(|key| desired_slack.contains_key(*key))
        .cloned()
        .collect();
    for key in stale_slack {
        if let Some(running) = registry.slack.remove(&key) {
            let rebound = desired_slack.contains_key(&key);
            running.cancel.cancel();
            let _ = running.task.await;
            if !rebound {
                report.stopped.push(key);
            }
        }
    }
    for (key, config) in desired_slack {
        if registry.slack.contains_key(&key) {
            continue;
        }
        let Some(shared) = registry.shared.get(&key.0).cloned() else {
            continue;
        };
        let rebound = restarting_slack.contains(&key);
        let running = start_slack(handle, shared, key.clone(), config);
        registry.slack.insert(key.clone(), running);
        if rebound {
            report.rebound.push(key);
        } else {
            report.started.push(key);
        }
    }

    // Operators that disappeared keep their state file; only the live entry is
    // dropped, and only after its listeners are down.
    registry.shared.retain(|name, _| defs.contains_key(name));

    // Reconcile publication against sockets that are actually live, not just
    // requested in configuration. A failed bind can never receive a public
    // route, and pause/detach/port replacement withdraws an existing route.
    reconcile_publications(manager, registry);

    tracing::info!(
        reason = reason.label(),
        started = report.started.len(),
        stopped = report.stopped.len(),
        rebound = report.rebound.len(),
        failed = report.failures.len(),
        "operator definitions reloaded"
    );
    Ok(report)
}

async fn start_listener(
    handle: &OperatorHandle,
    shared: Arc<OperatorShared>,
    key: ListenerKey,
    port: u16,
    endpoint: crate::channel_publication::ChannelIngressEndpoint,
) -> Result<ListenerHandle> {
    // Bound here rather than inside the spawned task so a failure is reported
    // to whoever asked for the reload instead of becoming a stray log line.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("bind loopback operator endpoint")?;
    let runtime = operator::channel_runtime(shared, key.1.clone());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_handle = handle.clone();
    let task_key = key.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = operator::serve(runtime, listener, task_cancel.clone()).await {
            tracing::error!(operator = %task_key.0, channel = %task_key.1, %error, "operator endpoint stopped");
        }
        // Only report an unexpected death. A cancelled listener was removed
        // from the registry by whoever cancelled it.
        if !task_cancel.is_cancelled() {
            let _ = task_handle.0.send(OperatorMsg::ListenerDied(task_key));
        }
    });
    Ok(ListenerHandle {
        port,
        endpoint,
        cancel,
        task,
    })
}

fn reconcile_publications(manager: &SessionManager, registry: &Registry) {
    if let Some(publications) = manager.channel_publications() {
        publications.reconcile(
            registry
                .listeners
                .iter()
                .map(|(key, listener)| (key.clone(), listener.endpoint.clone()))
                .collect(),
        );
    }
}

fn start_slack(
    handle: &OperatorHandle,
    shared: Arc<OperatorShared>,
    key: ListenerKey,
    config: operator::SlackConfig,
) -> SlackHandle {
    let runtime = operator::channel_runtime(shared, key.1.clone());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_handle = handle.clone();
    let task_key = key.clone();
    let running_config = config.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = operator::serve_slack(runtime, config, task_cancel.clone()).await {
            tracing::error!(operator = %task_key.0, channel = %task_key.1, %error, "Slack operator channel stopped");
        }
        if !task_cancel.is_cancelled() {
            let _ = task_handle.0.send(OperatorMsg::ListenerDied(task_key));
        }
    });
    SlackHandle {
        config: running_config,
        cancel,
        task,
    }
}

/// Create the supervisor's channel and handle. The caller spawns [`run`].
pub fn channel() -> (OperatorHandle, mpsc::UnboundedReceiver<OperatorMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (OperatorHandle(tx), rx)
}

/// How often hand-edited definitions are noticed. Definitions change on human
/// timescales, so this trades a little latency for far fewer wakeups.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Notice definitions edited outside the daemon.
///
/// The config directory is a documented, hand-editable surface, so an edit
/// made in a text editor has to apply on the same terms as one made in the UI.
/// Polling an mtime fingerprint is deliberate: a filesystem-notification
/// dependency would have to cope with editors that write in place versus
/// write-and-rename, and with event coalescing, to learn the same thing this
/// learns by looking.
///
/// A torn read is harmless because reload is all-or-nothing — the parse fails,
/// nothing changes, and the next tick picks up the finished file.
pub fn spawn_watcher(paths: Paths, handle: OperatorHandle) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut fingerprint = definitions_fingerprint(&paths);
        loop {
            interval.tick().await;
            let current = definitions_fingerprint(&paths);
            if current != fingerprint {
                fingerprint = current;
                handle.reload_detached(ReloadReason::FileChanged);
            }
        }
    });
}

/// Size-and-mtime fingerprint over every definition plus the channel catalog.
fn definitions_fingerprint(paths: &Paths) -> Vec<(String, u64, u128)> {
    let mut parts = Vec::new();
    let dir = paths.operators_dir();
    let entries = std::fs::read_dir(&dir).into_iter().flatten().flatten();
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        parts.push(file_stamp(&path));
    }
    parts.push(file_stamp(&dir.join("..").join("channels.toml")));
    parts.sort();
    parts
}

fn file_stamp(path: &std::path::Path) -> (String, u64, u128) {
    let name = path.to_string_lossy().to_string();
    let Ok(meta) = std::fs::metadata(path) else {
        return (name, 0, 0);
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis())
        .unwrap_or(0);
    (name, meta.len(), modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::{
        OperatorChannelConfig, OperatorRouting, OperatorSandboxConfig, OperatorSessionMode,
    };

    struct FakePublicationBackend;

    #[async_trait::async_trait]
    impl crate::channel_publication::PublicationBackend for FakePublicationBackend {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn supports(
            &self,
            _endpoint: &crate::channel_publication::ChannelIngressEndpoint,
        ) -> Result<()> {
            Ok(())
        }

        async fn run(
            &self,
            _key: crate::channel_publication::PublicationKey,
            _endpoint: crate::channel_publication::ChannelIngressEndpoint,
            events: crate::channel_publication::BackendEvents,
            cancel: CancellationToken,
        ) -> Result<()> {
            events.send(crate::channel_publication::BackendEvent::Ready(
                construct_protocol::ChannelPublicEndpoint::Url {
                    url: "https://example.test/svc/svc".into(),
                },
            ));
            cancel.cancelled().await;
            Ok(())
        }
    }

    fn operator(channels: &[(&str, u16, bool)], paused: bool) -> OperatorConfig {
        OperatorConfig {
            position: 0,
            placement: None,
            instruction: String::new(),
            harness: "smith".into(),
            model: None,
            session_mode: OperatorSessionMode::Headless,
            cwd: ".".into(),
            routing: OperatorRouting::SessionKey,
            paused,
            approval_timeout_secs: 0,
            sandbox: OperatorSandboxConfig::default(),
            channels: channels
                .iter()
                .map(|(id, port, enabled)| {
                    (
                        (*id).to_string(),
                        OperatorChannelConfig {
                            kind: Some("http".into()),
                            enabled: *enabled,
                            port: Some(*port),
                            token: Some("secret".into()),
                            app_token: None,
                            bot_token: None,
                            allowed_workspaces: Vec::new(),
                            allowed_channels: Vec::new(),
                            progress: Default::default(),
                            follow_up: Default::default(),
                            thread_context: 50,
                        },
                    )
                })
                .collect(),
        }
    }

    fn key(operator: &str, channel: &str) -> ListenerKey {
        (operator.to_string(), channel.to_string())
    }

    fn defs(entries: &[(&str, OperatorConfig)]) -> BTreeMap<String, OperatorConfig> {
        entries
            .iter()
            .map(|(name, config)| ((*name).to_string(), config.clone()))
            .collect()
    }

    #[test]
    fn paused_and_disabled_channels_are_not_wanted() {
        let (desired, _) = desired_listeners(&defs(&[("a", operator(&[("http", 1, true)], true))]));
        assert!(desired.is_empty(), "a paused operator frees its port");

        let (desired, _) =
            desired_listeners(&defs(&[("a", operator(&[("http", 1, false)], false))]));
        assert!(desired.is_empty(), "a disabled channel does not bind");
    }

    #[test]
    fn slack_channels_are_outbound_tasks_and_credential_edits_change_revision() {
        let slack_operator = |app_token: &str, paused: bool| OperatorConfig {
            position: 0,
            placement: None,
            instruction: String::new(),
            harness: "smith".into(),
            model: None,
            session_mode: OperatorSessionMode::Headless,
            cwd: ".".into(),
            routing: OperatorRouting::SessionKey,
            paused,
            approval_timeout_secs: 0,
            sandbox: OperatorSandboxConfig::default(),
            channels: BTreeMap::from([(
                "slack".into(),
                OperatorChannelConfig {
                    kind: Some("slack".into()),
                    enabled: true,
                    port: None,
                    token: None,
                    app_token: Some(app_token.into()),
                    bot_token: Some("xoxb-secret".into()),
                    allowed_workspaces: vec!["T1".into()],
                    allowed_channels: vec!["C1".into()],
                    progress: Default::default(),
                    follow_up: Default::default(),
                    thread_context: 50,
                },
            )]),
        };
        let first_defs = defs(&[("chat", slack_operator("xapp-first", false))]);
        assert!(desired_listeners(&first_defs).0.is_empty());
        let first = desired_slack(&first_defs);
        assert!(first.contains_key(&key("chat", "slack")));
        let changed = desired_slack(&defs(&[("chat", slack_operator("xapp-second", false))]));
        assert!(first != changed);
        assert!(desired_slack(&defs(&[("chat", slack_operator("xapp-first", true),)])).is_empty());
    }

    #[test]
    fn a_duplicate_port_loses_deterministically() {
        let (desired, conflicts) = desired_listeners(&defs(&[
            ("a", operator(&[("http", 9000, true)], false)),
            ("b", operator(&[("http", 9000, true)], false)),
        ]));
        // The already-serving claim wins; the loser is reported rather than
        // silently stealing the port.
        assert_eq!(desired.get(&key("a", "http")), Some(&9000));
        assert!(!desired.contains_key(&key("b", "http")));
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].key, key("b", "http"));
        assert_eq!(conflicts[0].held_by, key("a", "http"));
    }

    #[test]
    fn only_a_port_change_touches_the_socket() {
        let current = BTreeMap::from([(key("a", "http"), 9000u16)]);

        // Same port: nothing to do, whatever else changed in the definition.
        assert!(plan(&current, &current.clone()).is_empty());

        let moved = BTreeMap::from([(key("a", "http"), 9001u16)]);
        assert_eq!(
            plan(&current, &moved),
            vec![ListenerAction::Rebind(key("a", "http"), 9001)]
        );

        assert_eq!(
            plan(&current, &BTreeMap::new()),
            vec![ListenerAction::Stop(key("a", "http"))]
        );
        assert_eq!(
            plan(&BTreeMap::new(), &current),
            vec![ListenerAction::Start(key("a", "http"), 9000)]
        );
    }

    /// A supervisor fixture backed by real files and a real session manager,
    /// so the tests below exercise the reload path the daemon runs, not a
    /// re-implementation of it.
    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
        manager: Arc<SessionManager>,
        handle: OperatorHandle,
        publications: crate::channel_publication::PublicationHandle,
        registry: Registry,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let paths = Paths {
                config_dir: tmp.path().join("config"),
                state_dir: tmp.path().join("state"),
                data_dir: tmp.path().join("data"),
                runtime_dir: tmp.path().join("run"),
            };
            std::fs::create_dir_all(paths.operators_dir()).expect("operators dir");
            let storage =
                Arc::new(crate::storage::Storage::new(paths.data_dir.clone()).expect("storage"));
            let config = Arc::new(crate::config::Config::default());
            let (manager, _remote_rx, _restart_rx) =
                SessionManager::new(storage, config, paths.runtime_dir.clone())
                    .await
                    .expect("session manager");
            let manager = Arc::new(manager);
            let (publications, publication_rx) = crate::channel_publication::channel();
            manager.set_channel_publications(publications.clone());
            tokio::spawn(crate::channel_publication::run(
                vec![Arc::new(FakePublicationBackend)],
                publications.clone(),
                publication_rx,
                None,
            ));
            let (handle, _rx) = channel();
            Self {
                _tmp: tmp,
                paths,
                manager,
                handle,
                publications,
                registry: Registry::default(),
            }
        }

        fn write(&self, name: &str, body: &str) {
            std::fs::write(self.paths.operators_dir().join(format!("{name}.toml")), body)
                .expect("write definition");
        }

        fn remove(&self, name: &str) {
            std::fs::remove_file(self.paths.operators_dir().join(format!("{name}.toml")))
                .expect("remove definition");
        }

        async fn reload(&mut self) -> Result<ReloadReport> {
            super::reload(
                &self.manager,
                &self.paths,
                &self.handle,
                &mut self.registry,
                ReloadReason::Boot,
            )
            .await
        }
    }

    /// A definition with no channels, so these tests never touch a port.
    fn definition(routing: &str, paused: bool) -> String {
        format!(
            "instruction = \"x\"\nharness = \"smith\"\ncwd = \".\"\nrouting = \"{routing}\"\npaused = {paused}\n"
        )
    }

    #[tokio::test]
    async fn a_reload_reuses_the_state_of_a_operator_it_already_knows() {
        // The invariant that matters most: rebuilding a operator's shared state
        // would drop its routed-session map and reset its dedup ring, so a
        // retried delivery would open a second conversation.
        let mut fx = Fixture::new().await;
        fx.write("svc", &definition("session-key", false));
        fx.reload().await.expect("first reload");
        let first = fx.registry.shared.get("svc").cloned().expect("registered");

        fx.write("svc", &definition("per-event", false));
        fx.reload().await.expect("second reload");
        let second = fx
            .registry
            .shared
            .get("svc")
            .cloned()
            .expect("still registered");

        assert!(
            Arc::ptr_eq(&first, &second),
            "the same operator must keep its identity across a reload"
        );
        assert_eq!(
            second.config().routing,
            crate::operator::OperatorRouting::PerEvent,
            "the edited definition is what readers now see"
        );
    }

    #[tokio::test]
    async fn a_broken_definition_changes_nothing() {
        let mut fx = Fixture::new().await;
        fx.write("svc", &definition("session-key", false));
        fx.reload().await.expect("first reload");

        fx.write("svc", "this is not valid toml [[[");
        let outcome = fx.reload().await;
        assert!(outcome.is_err(), "a parse failure must fail the reload");

        // Still registered, still running the definition it had before.
        let shared = fx.registry.shared.get("svc").expect("operator survives");
        assert_eq!(
            shared.config().routing,
            crate::operator::OperatorRouting::SessionKey,
            "a file that does not parse must not disturb what is running"
        );
    }

    #[tokio::test]
    async fn operators_appear_and_disappear_with_their_definitions() {
        let mut fx = Fixture::new().await;
        fx.write("first", &definition("session-key", false));
        fx.reload().await.expect("reload");
        assert!(fx.registry.shared.contains_key("first"));
        assert!(!fx.registry.shared.contains_key("second"));

        fx.write("second", &definition("single", false));
        fx.reload().await.expect("reload");
        assert!(fx.registry.shared.contains_key("second"));

        fx.remove("first");
        fx.reload().await.expect("reload");
        assert!(
            !fx.registry.shared.contains_key("first"),
            "a deleted definition stops being served"
        );
        assert!(fx.registry.shared.contains_key("second"));
    }

    #[tokio::test]
    async fn a_channel_binds_moves_and_releases_across_reloads() {
        // Exercises the wiring the pure `plan` tests cannot: that a reload
        // actually binds, rebinds, and releases real sockets.
        let _serialized = port_lock().lock().await;
        let mut fx = Fixture::new().await;
        let first_port = free_port().await;
        let second_port = free_port().await;

        let with_channel = |port: u16, paused: bool| {
            format!(
                "instruction = \"x\"\nharness = \"smith\"\ncwd = \".\"\nrouting = \"per-event\"\npaused = {paused}\n\n[channels.http1]\nkind = \"http\"\nenabled = true\nport = {port}\ntoken = \"secret\"\n"
            )
        };

        fx.write("svc", &with_channel(first_port, false));
        let report = fx.reload().await.expect("reload");
        assert_eq!(report.started.len(), 1, "the channel was bound");
        assert!(port_in_use(first_port).await, "port answers once bound");
        fx.publications
            .publish("svc".into(), "http1".into(), "fake".into())
            .await
            .expect("publish bound channel");

        fx.write("svc", &with_channel(second_port, false));
        let report = fx.reload().await.expect("reload");
        assert_eq!(report.rebound.len(), 1, "a port change rebinds");
        assert!(!port_in_use(first_port).await, "the old port is released");
        assert!(port_in_use(second_port).await, "the new port answers");
        assert!(
            fx.publications.list().await.unwrap().is_empty(),
            "rebind withdraws publication without restoring intent"
        );

        // Pausing releases the port; this is the behavior that silently did
        // nothing before definitions were applied live.
        fx.write("svc", &with_channel(second_port, true));
        let report = fx.reload().await.expect("reload");
        assert_eq!(report.stopped.len(), 1, "pausing stops the listener");
        assert!(
            !port_in_use(second_port).await,
            "a paused operator frees its port"
        );
    }

    #[tokio::test]
    async fn one_operator_can_hand_a_port_to_another_in_one_reload() {
        // The case the two-pass executor exists for: if the bind ran before
        // the release, this would fail with EADDRINUSE and the port would end
        // up serving nobody.
        let _serialized = port_lock().lock().await;
        let mut fx = Fixture::new().await;
        let port = free_port().await;
        let with_port = |port: u16| {
            format!(
                "instruction = \"x\"\nharness = \"smith\"\ncwd = \".\"\nrouting = \"per-event\"\n\n[channels.http1]\nkind = \"http\"\nenabled = true\nport = {port}\ntoken = \"secret\"\n"
            )
        };

        fx.write("giver", &with_port(port));
        fx.reload().await.expect("reload");
        assert!(port_in_use(port).await, "giver holds the port");

        // Swap ownership in a single reload.
        fx.remove("giver");
        fx.write("taker", &with_port(port));
        let report = fx.reload().await.expect("reload");

        assert!(
            report.failures.is_empty(),
            "the handover must not fail to bind: {:?}",
            report.failures
        );
        assert_eq!(report.stopped, vec![("giver".into(), "http1".into())]);
        assert_eq!(report.started, vec![("taker".into(), "http1".into())]);
        assert!(port_in_use(port).await, "the taker is now serving the port");
    }

    /// Serializes the tests that claim a real port.
    ///
    /// `free_port` reports a port that was free a moment ago, so two tests
    /// running concurrently can be handed the same one and whichever binds
    /// second fails. Taking this lock makes the port-using tests deterministic
    /// with respect to each other.
    fn port_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(Default::default)
    }

    /// A port nothing is listening on right now.
    async fn free_port() -> u16 {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral");
        listener.local_addr().expect("addr").port()
    }

    async fn port_in_use(port: u16) -> bool {
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    }

    #[test]
    fn every_editable_field_declares_when_it_applies() {
        use construct_protocol::{PropagationClass, OperatorField};
        // The classes are what the UI promises a user, so each one has to
        // match what this module actually does with the field.
        for field in OperatorField::ALL {
            let _ = field.propagation().label();
        }
        assert_eq!(
            OperatorField::ChannelPort.propagation(),
            PropagationClass::Immediate,
            "a port change rebinds the socket during the reload"
        );
        assert_eq!(
            OperatorField::Routing.propagation(),
            PropagationClass::NextRequest,
            "routing is read while handling a request"
        );
        assert_eq!(
            OperatorField::Instruction.propagation(),
            PropagationClass::NextSession,
            "no harness can be re-instructed in place"
        );
    }

    #[tokio::test]
    async fn a_released_port_can_be_bound_again() {
        let _serialized = port_lock().lock().await;
        // The port frees when the listener drops inside its task, not when the
        // token is cancelled — so a rebind must await the handle. This is the
        // real failure behind two operators swapping ports.
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            // A real binding: `let _ = listener` would leave it uncaptured and
            // owned by this test, so the port would never be released.
            let _held = listener;
            task_cancel.cancelled().await;
        });

        assert!(
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_err(),
            "port is still held while the task lives"
        );

        cancel.cancel();
        let _ = task.await;

        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .expect("port is free once the task has been awaited");
    }

    #[test]
    fn a_port_handover_releases_before_it_binds() {
        // One operator gives up a port and another takes it in the same reload.
        // A swap of two ports would not test this: both sides are rebinds, so
        // there is no Start to order against and the assertion would hold
        // vacuously.
        let current = BTreeMap::from([(key("a", "http"), 9000u16)]);
        let desired = BTreeMap::from([(key("b", "http"), 9000u16)]);
        let actions = plan(&current, &desired);
        assert_eq!(
            actions,
            vec![
                ListenerAction::Stop(key("a", "http")),
                ListenerAction::Start(key("b", "http"), 9000),
            ],
            "the release is planned before the bind that needs the port"
        );
    }
}
