//! Provider-neutral publication of channel-owned ingress endpoints.
//!
//! This is the boundary between a channel implementation and public
//! reachability:
//!
//! * A channel adapter describes the local endpoint it owns.
//! * This supervisor owns explicit publish/unpublish intent and lifecycle.
//! * A provider backend turns that endpoint into a public address.
//!
//! Neither side parses the other's protocol. A future raw TCP channel can use
//! the same supervisor as today's HTTP channel; a broker-backed outbound
//! channel simply exposes no ingress endpoint and cannot be published.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use construct_protocol::{
    ChannelPublicEndpoint, ChannelPublicationPhase, ChannelPublicationSummary,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub type PublicationKey = (String, String);

/// Network transport a channel listener accepts locally.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum IngressTransport {
    Tcp(SocketAddr),
    /// Reserved in the boundary even though the current first-party provider
    /// intentionally rejects it. Adding UDP must not reshape channel adapters.
    Udp(SocketAddr),
}

/// Application protocol metadata used only to choose the provider's public
/// edge. The provider never parses the local stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ApplicationProtocol {
    Http { path: String },
    WebSocket { path: String },
    Opaque(String),
}

/// A local ingress endpoint supplied by a channel adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelIngressEndpoint {
    pub transport: IngressTransport,
    pub protocol: ApplicationProtocol,
}

impl ChannelIngressEndpoint {
    pub fn loopback_http(port: u16, path: impl Into<String>) -> Self {
        Self {
            transport: IngressTransport::Tcp(SocketAddr::from((
                std::net::Ipv4Addr::LOCALHOST,
                port,
            ))),
            protocol: ApplicationProtocol::Http { path: path.into() },
        }
    }
}

/// State transitions emitted by a long-running provider backend.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    Authorizing(Option<String>),
    Connecting,
    Ready(ChannelPublicEndpoint),
    Error(String),
}

/// Event sink for one backend run.
///
/// The supervisor owns the publication identity hidden inside this value.
/// That lets it discard late events from a cancelled run after the same
/// channel has been published again; providers only report lifecycle state.
#[derive(Clone)]
pub struct BackendEvents {
    key: PublicationKey,
    generation: u64,
    supervisor: mpsc::UnboundedSender<Msg>,
}

impl BackendEvents {
    pub fn send(&self, event: BackendEvent) {
        let _ = self
            .supervisor
            .send(Msg::BackendEvent(self.key.clone(), self.generation, event));
    }
}

#[async_trait]
pub trait PublicationBackend: Send + Sync {
    fn id(&self) -> &'static str;

    /// A backend rejects unsupported transport/protocol combinations before
    /// starting authorization or allocating a public route.
    fn supports(&self, endpoint: &ChannelIngressEndpoint) -> Result<()>;

    /// Run until cancelled or irrecoverably failed. Provider implementations
    /// own retries and scoped re-registration credentials for this lifetime.
    async fn run(
        &self,
        key: PublicationKey,
        endpoint: ChannelIngressEndpoint,
        events: BackendEvents,
        cancel: CancellationToken,
    ) -> Result<()>;
}

pub(crate) enum Msg {
    Reconcile(BTreeMap<PublicationKey, ChannelIngressEndpoint>),
    Publish {
        key: PublicationKey,
        provider: String,
        respond: oneshot::Sender<Result<ChannelPublicationSummary>>,
    },
    Unpublish {
        key: PublicationKey,
        respond: oneshot::Sender<bool>,
    },
    List(oneshot::Sender<Vec<ChannelPublicationSummary>>),
    BackendEvent(PublicationKey, u64, BackendEvent),
    BackendFinished(PublicationKey, u64, Result<(), String>),
}

#[derive(Clone)]
pub struct PublicationHandle(mpsc::UnboundedSender<Msg>);

impl PublicationHandle {
    /// Replace the set of locally available channel endpoints. Any active
    /// publication whose endpoint disappears or changes is withdrawn; a later
    /// reattachment never republishes it without another explicit request.
    pub fn reconcile(&self, endpoints: BTreeMap<PublicationKey, ChannelIngressEndpoint>) {
        let _ = self.0.send(Msg::Reconcile(endpoints));
    }

    pub async fn publish(
        &self,
        service_name: String,
        channel_id: String,
        provider: String,
    ) -> Result<ChannelPublicationSummary> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Msg::Publish {
                key: (service_name, channel_id),
                provider,
                respond: tx,
            })
            .map_err(|_| anyhow!("channel publication supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow!("channel publication supervisor dropped the request"))?
    }

    pub async fn unpublish(&self, service_name: String, channel_id: String) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Msg::Unpublish {
                key: (service_name, channel_id),
                respond: tx,
            })
            .map_err(|_| anyhow!("channel publication supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow!("channel publication supervisor dropped the request"))
    }

    pub async fn list(&self) -> Result<Vec<ChannelPublicationSummary>> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(Msg::List(tx))
            .map_err(|_| anyhow!("channel publication supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow!("channel publication supervisor dropped the request"))
    }
}

struct ActivePublication {
    summary: ChannelPublicationSummary,
    endpoint: ChannelIngressEndpoint,
    generation: u64,
    cancel: CancellationToken,
}

pub fn channel() -> (PublicationHandle, mpsc::UnboundedReceiver<Msg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (PublicationHandle(tx), rx)
}

pub async fn run(
    backends: Vec<Arc<dyn PublicationBackend>>,
    handle: PublicationHandle,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    manager: Option<Arc<crate::session::SessionManager>>,
) {
    let backends: HashMap<String, Arc<dyn PublicationBackend>> = backends
        .into_iter()
        .map(|backend| (backend.id().to_string(), backend))
        .collect();
    let mut available = BTreeMap::new();
    let mut active: HashMap<PublicationKey, ActivePublication> = HashMap::new();
    let mut next_generation = 1u64;

    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Reconcile(next) => {
                let withdraw: Vec<_> = active
                    .iter()
                    .filter(|(key, publication)| next.get(*key) != Some(&publication.endpoint))
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in withdraw {
                    if let Some(publication) = active.remove(&key) {
                        publication.cancel.cancel();
                        notify(&manager, &key, None);
                        tracing::info!(service = %key.0, channel = %key.1, "channel publication withdrawn because its local endpoint changed");
                    }
                }
                available = next;
            }
            Msg::Publish {
                key,
                provider,
                respond,
            } => {
                let result = if let Some(current) = active.get(&key) {
                    if current.summary.provider == provider {
                        Ok(current.summary.clone())
                    } else {
                        Err(anyhow!(
                            "channel `{}/{}` is already published through `{}`",
                            key.0,
                            key.1,
                            current.summary.provider
                        ))
                    }
                } else {
                    match (
                        backends.get(&provider).cloned(),
                        available.get(&key).cloned(),
                    ) {
                        (None, _) => Err(anyhow!(
                            "unsupported channel publication provider `{provider}`"
                        )),
                        (Some(_), None) => Err(anyhow!(
                            "channel `{}/{}` has no active local ingress endpoint",
                            key.0,
                            key.1
                        )),
                        (Some(backend), Some(endpoint)) => match backend.supports(&endpoint) {
                            Err(error) => Err(error),
                            Ok(()) => {
                                let summary = ChannelPublicationSummary {
                                    service_name: key.0.clone(),
                                    channel_id: key.1.clone(),
                                    provider: backend.id().to_string(),
                                    phase: ChannelPublicationPhase::Authorizing,
                                    public_endpoint: None,
                                    auth_url: None,
                                    error: None,
                                };
                                let cancel = CancellationToken::new();
                                let generation = next_generation;
                                next_generation = next_generation
                                    .checked_add(1)
                                    .expect("publication generation exhausted");
                                active.insert(
                                    key.clone(),
                                    ActivePublication {
                                        summary: summary.clone(),
                                        endpoint: endpoint.clone(),
                                        generation,
                                        cancel: cancel.clone(),
                                    },
                                );
                                notify(&manager, &key, active.get(&key).map(|item| &item.summary));
                                let backend = backend.clone();
                                let finished = handle.0.clone();
                                let task_key = key.clone();
                                let events = BackendEvents {
                                    key: key.clone(),
                                    generation,
                                    supervisor: handle.0.clone(),
                                };
                                tokio::spawn(async move {
                                    let result = backend
                                        .run(task_key.clone(), endpoint, events, cancel)
                                        .await
                                        .map_err(|error| error.to_string());
                                    let _ = finished
                                        .send(Msg::BackendFinished(task_key, generation, result));
                                });
                                Ok(summary)
                            }
                        },
                    }
                };
                let _ = respond.send(result);
            }
            Msg::Unpublish { key, respond } => {
                let removed = active.remove(&key);
                if let Some(publication) = &removed {
                    publication.cancel.cancel();
                }
                if removed.is_some() {
                    notify(&manager, &key, None);
                }
                let _ = respond.send(removed.is_some());
            }
            Msg::List(respond) => {
                let mut summaries: Vec<_> =
                    active.values().map(|item| item.summary.clone()).collect();
                summaries.sort_by(|a, b| {
                    (&a.service_name, &a.channel_id).cmp(&(&b.service_name, &b.channel_id))
                });
                let _ = respond.send(summaries);
            }
            Msg::BackendEvent(key, generation, event) => {
                if let Some(publication) = active
                    .get_mut(&key)
                    .filter(|publication| publication.generation == generation)
                {
                    publication.summary.error = None;
                    match event {
                        BackendEvent::Authorizing(url) => {
                            publication.summary.phase = ChannelPublicationPhase::Authorizing;
                            publication.summary.auth_url = url;
                            publication.summary.public_endpoint = None;
                        }
                        BackendEvent::Connecting => {
                            publication.summary.phase = ChannelPublicationPhase::Connecting;
                            publication.summary.auth_url = None;
                            publication.summary.public_endpoint = None;
                        }
                        BackendEvent::Ready(endpoint) => {
                            publication.summary.phase = ChannelPublicationPhase::Ready;
                            publication.summary.auth_url = None;
                            publication.summary.public_endpoint = Some(endpoint);
                        }
                        BackendEvent::Error(error) => {
                            publication.summary.phase = ChannelPublicationPhase::Error;
                            publication.summary.auth_url = None;
                            publication.summary.public_endpoint = None;
                            publication.summary.error = Some(error);
                        }
                    }
                    let summary = publication.summary.clone();
                    notify(&manager, &key, Some(&summary));
                }
            }
            Msg::BackendFinished(key, generation, result) => {
                if let Some(publication) = active
                    .get_mut(&key)
                    .filter(|publication| publication.generation == generation)
                {
                    if publication.cancel.is_cancelled() {
                        active.remove(&key);
                    } else {
                        publication.summary.phase = ChannelPublicationPhase::Error;
                        publication.summary.auth_url = None;
                        publication.summary.public_endpoint = None;
                        publication.summary.error = Some(match result {
                            Ok(()) => "publication ended unexpectedly".to_string(),
                            Err(error) => error,
                        });
                        let summary = publication.summary.clone();
                        notify(&manager, &key, Some(&summary));
                    }
                }
            }
        }
    }

    for publication in active.into_values() {
        publication.cancel.cancel();
    }
}

fn notify(
    manager: &Option<Arc<crate::session::SessionManager>>,
    key: &PublicationKey,
    publication: Option<&ChannelPublicationSummary>,
) {
    if let Some(manager) = manager {
        manager.broadcast_channel_publication(
            construct_protocol::ChannelPublicationNotificationPayload {
                service_name: key.0.clone(),
                channel_id: key.1.clone(),
                publication: publication.cloned(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeBackend;

    #[async_trait]
    impl PublicationBackend for FakeBackend {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn supports(&self, endpoint: &ChannelIngressEndpoint) -> Result<()> {
            match endpoint.transport {
                IngressTransport::Tcp(_) => Ok(()),
                IngressTransport::Udp(_) => Err(anyhow!("UDP is unsupported")),
            }
        }

        async fn run(
            &self,
            key: PublicationKey,
            _endpoint: ChannelIngressEndpoint,
            events: BackendEvents,
            cancel: CancellationToken,
        ) -> Result<()> {
            let _ = key;
            events.send(BackendEvent::Ready(ChannelPublicEndpoint::Url {
                url: "https://example.test/hook".into(),
            }));
            cancel.cancelled().await;
            Ok(())
        }
    }

    async fn fixture() -> PublicationHandle {
        let backend: Arc<dyn PublicationBackend> = Arc::new(FakeBackend);
        let (handle, rx) = channel();
        tokio::spawn(run(vec![backend], handle.clone(), rx, None));
        handle
    }

    #[tokio::test]
    async fn publication_is_explicit_and_endpoint_scoped() {
        let handle = fixture().await;
        let key = ("alerts".to_string(), "http".to_string());
        handle.reconcile(BTreeMap::from([(
            key.clone(),
            ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts"),
        )]));
        tokio::task::yield_now().await;
        assert!(handle.list().await.unwrap().is_empty());

        handle
            .publish(key.0.clone(), key.1.clone(), "fake".into())
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let publication = handle.list().await.unwrap().pop().unwrap();
        assert_eq!(publication.phase, ChannelPublicationPhase::Ready);
    }

    #[tokio::test]
    async fn endpoint_removal_withdraws_and_does_not_auto_republish() {
        let handle = fixture().await;
        handle.reconcile(BTreeMap::from([(
            ("alerts".into(), "http".into()),
            ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts"),
        )]));
        tokio::task::yield_now().await;
        handle
            .publish("alerts".into(), "http".into(), "fake".into())
            .await
            .unwrap();

        handle.reconcile(BTreeMap::new());
        tokio::task::yield_now().await;
        assert!(handle.list().await.unwrap().is_empty());

        handle.reconcile(BTreeMap::from([(
            ("alerts".into(), "http".into()),
            ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts"),
        )]));
        tokio::task::yield_now().await;
        assert!(handle.list().await.unwrap().is_empty());
    }

    struct LateBackend {
        runs: AtomicUsize,
    }

    #[async_trait]
    impl PublicationBackend for LateBackend {
        fn id(&self) -> &'static str {
            "late"
        }

        fn supports(&self, _endpoint: &ChannelIngressEndpoint) -> Result<()> {
            Ok(())
        }

        async fn run(
            &self,
            _key: PublicationKey,
            _endpoint: ChannelIngressEndpoint,
            events: BackendEvents,
            cancel: CancellationToken,
        ) -> Result<()> {
            if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
                cancel.cancelled().await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                events.send(BackendEvent::Ready(ChannelPublicEndpoint::Url {
                    url: "https://stale.example.test/".into(),
                }));
                return Ok(());
            }

            events.send(BackendEvent::Ready(ChannelPublicEndpoint::Url {
                url: "https://current.example.test/".into(),
            }));
            cancel.cancelled().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelled_run_cannot_mutate_a_later_publication() {
        let backend: Arc<dyn PublicationBackend> = Arc::new(LateBackend {
            runs: AtomicUsize::new(0),
        });
        let (handle, rx) = channel();
        tokio::spawn(run(vec![backend], handle.clone(), rx, None));
        handle.reconcile(BTreeMap::from([(
            ("alerts".into(), "http".into()),
            ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts"),
        )]));
        tokio::task::yield_now().await;

        handle
            .publish("alerts".into(), "http".into(), "late".into())
            .await
            .unwrap();
        assert!(handle
            .unpublish("alerts".into(), "http".into())
            .await
            .unwrap());
        handle
            .publish("alerts".into(), "http".into(), "late".into())
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let publication = handle.list().await.unwrap().pop().unwrap();
        assert_eq!(publication.phase, ChannelPublicationPhase::Ready);
        assert_eq!(
            publication.public_endpoint,
            Some(ChannelPublicEndpoint::Url {
                url: "https://current.example.test/".into(),
            })
        );
    }
}
