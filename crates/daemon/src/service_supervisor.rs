//! Single long-running task that owns every service listener.
//!
//! Service definitions are edited while the daemon runs — through IPC, or by
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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result};
use construct_protocol::paths::Paths;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::service::{self, ServiceConfig, ServiceShared};
use crate::session::SessionManager;

/// One bound channel: which service, which channel within it.
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

pub enum ServiceMsg {
    Reload {
        reason: ReloadReason,
        respond: Option<oneshot::Sender<Result<ReloadReport>>>,
    },
    /// A listener's accept loop ended on its own; drop it so a later reload
    /// can start it again.
    ListenerDied(ListenerKey),
}

/// Handle held by `SessionManager` so any caller can request a reload without
/// depending on this module's internals.
#[derive(Clone)]
pub struct ServiceHandle(mpsc::UnboundedSender<ServiceMsg>);

impl ServiceHandle {
    pub async fn reload(&self, reason: ReloadReason) -> Result<ReloadReport> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(ServiceMsg::Reload {
                reason,
                respond: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("service supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("service supervisor dropped the request"))?
    }

    /// Fire-and-forget reload, for the file watcher, which has nobody to
    /// report to.
    pub fn reload_detached(&self, reason: ReloadReason) {
        let _ = self.0.send(ServiceMsg::Reload {
            reason,
            respond: None,
        });
    }
}

struct ListenerHandle {
    port: u16,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Everything the supervisor owns. Lives in the task's stack frame, so there
/// is no lock and no way for another task to observe it half-updated.
#[derive(Default)]
struct Registry {
    /// One entry per service name, reused across reloads. See the invariant on
    /// [`ServiceShared`]: rebuilding one of these would drop the routed
    /// session map and the dedup ring.
    shared: BTreeMap<String, Arc<ServiceShared>>,
    listeners: HashMap<ListenerKey, ListenerHandle>,
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
/// Paused services and disabled channels contribute nothing, which is what
/// lets an operator free a port by pausing. When two channels ask for the same
/// port the earlier key wins deterministically, so a hand-edited duplicate
/// cannot take a port away from a service that is already serving.
pub fn desired_listeners(
    defs: &BTreeMap<String, ServiceConfig>,
) -> (BTreeMap<ListenerKey, u16>, Vec<PortConflict>) {
    let mut desired: BTreeMap<ListenerKey, u16> = BTreeMap::new();
    let mut claimed: BTreeMap<u16, ListenerKey> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (name, config) in defs {
        if config.paused {
            continue;
        }
        for (channel_id, channel) in &config.channels {
            let Some(port) = service::bindable_port(name, channel_id, channel) else {
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

/// The socket work to get from `current` to `desired`.
///
/// Every stop precedes every start. Two services swapping ports would
/// otherwise deadlock on `EADDRINUSE`, and the executor relies on this
/// ordering being baked into the plan rather than remembered at the call site.
pub fn plan(
    current: &BTreeMap<ListenerKey, u16>,
    desired: &BTreeMap<ListenerKey, u16>,
) -> Vec<ListenerAction> {
    let mut stops = Vec::new();
    let mut starts = Vec::new();
    for (key, port) in current {
        match desired.get(key) {
            None => stops.push(ListenerAction::Stop(key.clone())),
            Some(next) if next != port => {
                stops.push(ListenerAction::Rebind(key.clone(), *next))
            }
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
    handle: ServiceHandle,
    mut rx: mpsc::UnboundedReceiver<ServiceMsg>,
) {
    let mut registry = Registry::default();
    while let Some(msg) = rx.recv().await {
        match msg {
            ServiceMsg::Reload {
                reason,
                mut respond,
            } => {
                // Coalesce a burst — an editor that writes three times, or a
                // UI save racing the watcher — into one reload, keeping the
                // last caller waiting for an answer.
                let mut reason = reason;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        ServiceMsg::Reload {
                            reason: next_reason,
                            respond: next_respond,
                        } => {
                            if let Some(stale) = respond.take() {
                                let _ = stale.send(Ok(ReloadReport::default()));
                            }
                            reason = next_reason;
                            respond = next_respond;
                        }
                        ServiceMsg::ListenerDied(key) => {
                            registry.listeners.remove(&key);
                        }
                    }
                }
                let outcome = reload(&manager, &paths, &handle, &mut registry, reason).await;
                if let Some(respond) = respond {
                    let _ = respond.send(outcome);
                }
            }
            ServiceMsg::ListenerDied(key) => {
                registry.listeners.remove(&key);
            }
        }
    }
    tracing::debug!("service supervisor channel closed; exiting");
}

async fn reload(
    manager: &Arc<SessionManager>,
    paths: &Paths,
    handle: &ServiceHandle,
    registry: &mut Registry,
    reason: ReloadReason,
) -> Result<ReloadReport> {
    // All or nothing: a definition that does not parse leaves the running
    // configuration untouched rather than half-applied.
    let defs = service::load_definitions(&paths.services_dir())
        .context("reload service definitions")?;

    let (desired, conflicts) = desired_listeners(&defs);
    for conflict in &conflicts {
        tracing::warn!(
            service = %conflict.key.0,
            channel = %conflict.key.1,
            port = conflict.port,
            held_by = %format!("{}:{}", conflict.held_by.0, conflict.held_by.1),
            "service channel wants a port another channel already claims; not binding"
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
                    ServiceShared::load(
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

    // Stops first (the plan guarantees the ordering), awaiting each task so
    // the port is actually released — it frees when the listener drops inside
    // the task, not when the token is cancelled.
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
        match start_listener(handle, shared, key.clone(), port).await {
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
                tracing::error!(service = %key.0, channel = %key.1, port, %error, "service channel failed to bind");
                report.failures.push((key, port, error.to_string()));
            }
        }
    }

    // Services that disappeared keep their state file; only the live entry is
    // dropped, and only after its listeners are down.
    registry.shared.retain(|name, _| defs.contains_key(name));

    tracing::info!(
        reason = reason.label(),
        started = report.started.len(),
        stopped = report.stopped.len(),
        rebound = report.rebound.len(),
        failed = report.failures.len(),
        "service definitions reloaded"
    );
    Ok(report)
}

async fn start_listener(
    handle: &ServiceHandle,
    shared: Arc<ServiceShared>,
    key: ListenerKey,
    port: u16,
) -> Result<ListenerHandle> {
    // Bound here rather than inside the spawned task so a failure is reported
    // to whoever asked for the reload instead of becoming a stray log line.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("bind loopback service endpoint")?;
    let runtime = service::channel_runtime(shared, key.1.clone());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_handle = handle.clone();
    let task_key = key.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = service::serve(runtime, listener, task_cancel.clone()).await {
            tracing::error!(service = %task_key.0, channel = %task_key.1, %error, "service endpoint stopped");
        }
        // Only report an unexpected death. A cancelled listener was removed
        // from the registry by whoever cancelled it.
        if !task_cancel.is_cancelled() {
            let _ = task_handle.0.send(ServiceMsg::ListenerDied(task_key));
        }
    });
    Ok(ListenerHandle { port, cancel, task })
}

/// Create the supervisor's channel and handle. The caller spawns [`run`].
pub fn channel() -> (ServiceHandle, mpsc::UnboundedReceiver<ServiceMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ServiceHandle(tx), rx)
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
pub fn spawn_watcher(paths: Paths, handle: ServiceHandle) {
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
    let dir = paths.services_dir();
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
    use crate::service::{ServiceChannelConfig, ServiceRouting, ServiceSandboxConfig};

    fn service(channels: &[(&str, u16, bool)], paused: bool) -> ServiceConfig {
        ServiceConfig {
            instruction: String::new(),
            harness: "smith".into(),
            model: None,
            cwd: ".".into(),
            routing: ServiceRouting::SessionKey,
            paused,
            approval_timeout_secs: 0,
            sandbox: ServiceSandboxConfig::default(),
            channels: channels
                .iter()
                .map(|(id, port, enabled)| {
                    (
                        (*id).to_string(),
                        ServiceChannelConfig {
                            kind: Some("http".into()),
                            enabled: *enabled,
                            port: Some(*port),
                            token: Some("secret".into()),
                        },
                    )
                })
                .collect(),
        }
    }

    fn key(service: &str, channel: &str) -> ListenerKey {
        (service.to_string(), channel.to_string())
    }

    fn defs(entries: &[(&str, ServiceConfig)]) -> BTreeMap<String, ServiceConfig> {
        entries
            .iter()
            .map(|(name, config)| ((*name).to_string(), config.clone()))
            .collect()
    }

    #[test]
    fn paused_and_disabled_channels_are_not_wanted() {
        let (desired, _) = desired_listeners(&defs(&[("a", service(&[("http", 1, true)], true))]));
        assert!(desired.is_empty(), "a paused service frees its port");

        let (desired, _) = desired_listeners(&defs(&[("a", service(&[("http", 1, false)], false))]));
        assert!(desired.is_empty(), "a disabled channel does not bind");
    }

    #[test]
    fn a_duplicate_port_loses_deterministically() {
        let (desired, conflicts) = desired_listeners(&defs(&[
            ("a", service(&[("http", 9000, true)], false)),
            ("b", service(&[("http", 9000, true)], false)),
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

    #[test]
    fn every_editable_field_declares_when_it_applies() {
        use construct_protocol::{PropagationClass, ServiceField};
        // The classes are what the UI promises an operator, so each one has to
        // match what this module actually does with the field.
        for field in ServiceField::ALL {
            let _ = field.propagation().label();
        }
        assert_eq!(
            ServiceField::ChannelPort.propagation(),
            PropagationClass::Immediate,
            "a port change rebinds the socket during the reload"
        );
        assert_eq!(
            ServiceField::Routing.propagation(),
            PropagationClass::NextRequest,
            "routing is read while handling a request"
        );
        assert_eq!(
            ServiceField::Instruction.propagation(),
            PropagationClass::NextSession,
            "no harness can be re-instructed in place"
        );
    }

    #[tokio::test]
    async fn a_released_port_can_be_bound_again() {
        // The port frees when the listener drops inside its task, not when the
        // token is cancelled — so a rebind must await the handle. This is the
        // real failure behind two services swapping ports.
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
    fn every_stop_precedes_every_start() {
        // Two services swapping ports: starting before stopping would fail
        // with EADDRINUSE, so the ordering is part of the plan itself.
        let current = BTreeMap::from([(key("a", "http"), 9000u16), (key("b", "http"), 9001u16)]);
        let desired = BTreeMap::from([(key("a", "http"), 9001u16), (key("b", "http"), 9000u16)]);
        let actions = plan(&current, &desired);
        let last_stop = actions
            .iter()
            .rposition(|action| !matches!(action, ListenerAction::Start(..)))
            .expect("a rebind is a stop");
        let first_start = actions
            .iter()
            .position(|action| matches!(action, ListenerAction::Start(..)));
        assert!(
            first_start.is_none_or(|start| last_stop < start),
            "stops must come first: {actions:?}"
        );
    }
}

