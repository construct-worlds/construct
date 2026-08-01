//! First-party backend for publishing arbitrary channel ingress endpoints.
//!
//! The provider registration speaks only transport and application-protocol
//! metadata. Channel credentials and payloads pass through unchanged to the
//! channel-owned local listener.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::Parser;
use construct_protocol::ChannelPublicEndpoint;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use wstunnel::executor::JoinSetTokioExecutor;

use crate::channel_publication::{
    ApplicationProtocol, BackendEvent, BackendEvents, ChannelIngressEndpoint, IngressTransport,
    PublicationBackend, PublicationKey,
};
use crate::tunnel::auth::{self, AuthorizationDisplay};

const DEFAULT_API_URL: &str = "https://tunnel.zarvis.ai/api/v1/channel-publications";
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_INTERVAL: Duration = Duration::from_secs(20);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct ConstructChannelBackend;

#[derive(Serialize)]
struct RegisterRequest<'a> {
    construct_instance_id: &'a str,
    channel_id: &'a str,
    transport: &'static str,
    protocol: ProtocolProfile,
    /// The provider supplies reachability and TLS only. Authentication remains
    /// end-to-end at the channel listener.
    authorization: &'static str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ProtocolProfile {
    Http {
        path: String,
    },
    #[serde(rename = "websocket")]
    WebSocket {
        path: String,
    },
    Opaque {
        name: String,
    },
}

#[derive(Deserialize)]
struct Registration {
    public_endpoint: ChannelPublicEndpoint,
    relay_url: String,
    remote_port: u16,
    tunnel_token: String,
    ready_url: String,
    expires_in_seconds: u64,
    #[serde(default)]
    reregistration_token: Option<String>,
}

enum Credential {
    Owner(String),
    Reregistration(String),
}

impl Credential {
    fn bearer(&self) -> &str {
        match self {
            Self::Owner(value) | Self::Reregistration(value) => value,
        }
    }
}

struct EventDisplay {
    events: BackendEvents,
}

#[async_trait]
impl AuthorizationDisplay for EventDisplay {
    async fn set_authorization_url(&self, url: Option<String>) {
        self.events.send(BackendEvent::Authorizing(url));
    }
}

#[derive(Parser)]
struct InProcessClient {
    #[command(flatten)]
    client: wstunnel::config::Client,
}

#[async_trait]
impl PublicationBackend for ConstructChannelBackend {
    fn id(&self) -> &'static str {
        "construct"
    }

    fn supports(&self, endpoint: &ChannelIngressEndpoint) -> Result<()> {
        match endpoint.transport {
            IngressTransport::Tcp(address) if address.ip().is_loopback() => {}
            IngressTransport::Tcp(_) => {
                anyhow::bail!("channel publication requires a loopback TCP endpoint")
            }
            IngressTransport::Udp(_) => {
                anyhow::bail!("the Construct provider does not support UDP channels")
            }
        }
        registration_profile(endpoint).map(|_| ())
    }

    async fn run(
        &self,
        key: PublicationKey,
        endpoint: ChannelIngressEndpoint,
        events: BackendEvents,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.supports(&endpoint)?;
        let instance_id = crate::tunnel::stable_construct_instance_id()?;
        let api_url = std::env::var("CONSTRUCT_CHANNEL_PUBLICATION_API_URL")
            .unwrap_or_else(|_| DEFAULT_API_URL.to_string());
        let http = reqwest::Client::new();
        let display = EventDisplay {
            events: events.clone(),
        };
        let mut owner_token = None;
        let mut reregistration_token = None;
        let mut backoff = 1u64;

        loop {
            events.send(BackendEvent::Connecting);
            let registration = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                result = register(
                    &http,
                    &display,
                    &api_url,
                    &instance_id,
                    &key.1,
                    &endpoint,
                    &mut owner_token,
                    &mut reregistration_token,
                ) => result,
            };

            match registration {
                Ok(registration) => {
                    backoff = 1;
                    events.send(BackendEvent::Connecting);
                    let outcome =
                        run_registration(&http, &endpoint, &events, &cancel, registration).await;
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    if let Err(error) = outcome {
                        events.send(BackendEvent::Error(error.to_string()));
                        tracing::warn!(service = %key.0, channel = %key.1, %error, "channel publication route lost; re-registering");
                    }
                }
                Err(error) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    events.send(BackendEvent::Error(error.to_string()));
                    tracing::warn!(service = %key.0, channel = %key.1, %error, "channel publication registration failed; retrying");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            }
            backoff = (backoff * 2).min(30);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn register(
    http: &reqwest::Client,
    display: &dyn AuthorizationDisplay,
    api_url: &str,
    instance_id: &str,
    channel_id: &str,
    endpoint: &ChannelIngressEndpoint,
    owner_token: &mut Option<String>,
    reregistration_token: &mut Option<String>,
) -> Result<Registration> {
    let credential = match reregistration_token.clone() {
        Some(value) => Credential::Reregistration(value),
        None => match owner_token.clone() {
            Some(value) => Credential::Owner(value),
            None => {
                let value = auth::authorize(http, display, api_url).await?;
                *owner_token = Some(value.clone());
                Credential::Owner(value)
            }
        },
    };
    let (transport, protocol) = registration_profile(endpoint)?;
    let response = http
        .post(api_url)
        .bearer_auth(credential.bearer())
        .json(&RegisterRequest {
            construct_instance_id: instance_id,
            channel_id,
            transport,
            protocol,
            authorization: "channel",
        })
        .send()
        .await
        .context("contact Construct channel publication service")?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        match credential {
            Credential::Reregistration(_) => *reregistration_token = None,
            Credential::Owner(_) => *owner_token = None,
        }
    }
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Construct channel publication rejected ({status}){}",
            if detail.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", detail.trim())
            }
        );
    }
    let registration = response
        .json::<Registration>()
        .await
        .context("decode Construct channel publication")?;
    validate_public_endpoint(&registration.public_endpoint, endpoint)?;
    if let Some(token) = registration
        .reregistration_token
        .as_ref()
        .filter(|token| !token.is_empty())
    {
        *reregistration_token = Some(token.clone());
    }
    Ok(registration)
}

fn registration_profile(
    endpoint: &ChannelIngressEndpoint,
) -> Result<(&'static str, ProtocolProfile)> {
    let transport = match endpoint.transport {
        IngressTransport::Tcp(_) => "tcp",
        IngressTransport::Udp(_) => anyhow::bail!("UDP publication is unsupported"),
    };
    let protocol = match &endpoint.protocol {
        ApplicationProtocol::Http { path } => ProtocolProfile::Http {
            path: valid_http_path(path)?.to_string(),
        },
        ApplicationProtocol::WebSocket { path } => ProtocolProfile::WebSocket {
            path: valid_http_path(path)?.to_string(),
        },
        ApplicationProtocol::Opaque(name) if !name.trim().is_empty() => {
            ProtocolProfile::Opaque { name: name.clone() }
        }
        ApplicationProtocol::Opaque(_) => anyhow::bail!("opaque protocol name cannot be empty"),
    };
    Ok((transport, protocol))
}

fn valid_http_path(path: &str) -> Result<&str> {
    if !path.starts_with('/') || path.contains(['?', '#']) {
        anyhow::bail!("published HTTP path must be an absolute path without query or fragment");
    }
    Ok(path)
}

async fn run_registration(
    http: &reqwest::Client,
    endpoint: &ChannelIngressEndpoint,
    events: &BackendEvents,
    cancel: &CancellationToken,
    registration: Registration,
) -> Result<()> {
    let local = match endpoint.transport {
        IngressTransport::Tcp(address) => address,
        IngressTransport::Udp(_) => anyhow::bail!("UDP publication is unsupported"),
    };
    let local_host = match local.ip() {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let reverse = format!(
        "tcp://127.0.0.1:{}:{local_host}:{}",
        registration.remote_port,
        local.port()
    );
    let auth_header = format!("Authorization: Bearer {}", registration.tunnel_token);
    let client = InProcessClient::try_parse_from([
        "construct-channel-wstunnel",
        "--remote-to-local",
        &reverse,
        "--http-headers",
        &auth_header,
        &registration.relay_url,
    ])
    .context("configure channel wstunnel client")?
    .client;
    let tunnel = wstunnel::run_client(client, JoinSetTokioExecutor::default());
    tokio::pin!(tunnel);

    let ready_deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = &mut tunnel => {
                result.context("run channel wstunnel client")?;
                anyhow::bail!("channel wstunnel exited before readiness");
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if tunnel_ready(http, &registration.ready_url, &registration.tunnel_token).await {
                    events.send(BackendEvent::Ready(registration.public_endpoint.clone()));
                    break;
                }
                if tokio::time::Instant::now() >= ready_deadline {
                    anyhow::bail!("channel publication did not become ready within {} seconds", READY_TIMEOUT.as_secs());
                }
            }
        }
    }

    let refresh_after =
        Duration::from_secs(registration.expires_in_seconds.saturating_sub(60).max(1));
    let refresh = tokio::time::sleep(refresh_after);
    tokio::pin!(refresh);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = &mut tunnel => {
                result.context("run channel wstunnel client")?;
                anyhow::bail!("channel wstunnel exited");
            }
            _ = &mut refresh => return Err(anyhow!("channel publication capability is expiring")),
            _ = tokio::time::sleep(HEALTH_INTERVAL) => {
                if !tunnel_ready(http, &registration.ready_url, &registration.tunnel_token).await {
                    anyhow::bail!("channel publication gateway route is no longer ready");
                }
            }
        }
    }
}

async fn tunnel_ready(http: &reqwest::Client, ready_url: &str, token: &str) -> bool {
    http.get(ready_url)
        .bearer_auth(token)
        .timeout(HEALTH_TIMEOUT)
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

fn validate_public_endpoint(
    public: &ChannelPublicEndpoint,
    local: &ChannelIngressEndpoint,
) -> Result<()> {
    match (&local.protocol, public) {
        (ApplicationProtocol::Http { path }, ChannelPublicEndpoint::Url { url })
        | (ApplicationProtocol::WebSocket { path }, ChannelPublicEndpoint::Url { url }) => {
            let parsed = reqwest::Url::parse(url).context("provider returned an invalid URL")?;
            let expected_scheme = match local.protocol {
                ApplicationProtocol::Http { .. } => "https",
                ApplicationProtocol::WebSocket { .. } => "wss",
                ApplicationProtocol::Opaque(_) => unreachable!(),
            };
            if parsed.scheme() != expected_scheme || parsed.host_str().is_none() {
                anyhow::bail!("provider returned an invalid public {expected_scheme} URL");
            }
            if parsed.path() != path || parsed.query().is_some() || parsed.fragment().is_some() {
                anyhow::bail!("provider public URL must use the channel's canonical path `{path}`");
            }
        }
        (ApplicationProtocol::Opaque(_), ChannelPublicEndpoint::Socket { host, port }) => {
            if host.trim().is_empty() || *port == 0 {
                anyhow::bail!("provider returned an invalid public socket");
            }
        }
        (ApplicationProtocol::Http { .. }, ChannelPublicEndpoint::Socket { .. })
        | (ApplicationProtocol::WebSocket { .. }, ChannelPublicEndpoint::Socket { .. }) => {
            anyhow::bail!("provider returned a socket for a URL-based channel")
        }
        (ApplicationProtocol::Opaque(_), ChannelPublicEndpoint::Url { .. }) => {
            anyhow::bail!("provider returned a URL for an opaque stream channel")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_preserve_protocol_without_owning_it() {
        assert_eq!(
            registration_profile(&ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts"))
                .unwrap(),
            (
                "tcp",
                ProtocolProfile::Http {
                    path: "/svc/alerts".into()
                }
            )
        );
        let opaque = ChannelIngressEndpoint {
            transport: IngressTransport::Tcp("127.0.0.1:9000".parse().unwrap()),
            protocol: ApplicationProtocol::Opaque("postgres".into()),
        };
        assert_eq!(
            registration_profile(&opaque).unwrap(),
            (
                "tcp",
                ProtocolProfile::Opaque {
                    name: "postgres".into()
                }
            )
        );
        assert!(
            registration_profile(&ChannelIngressEndpoint::loopback_http(8787, "svc/alerts"))
                .is_err()
        );
        assert!(registration_profile(&ChannelIngressEndpoint::loopback_http(
            8787,
            "/svc/alerts?debug=1"
        ))
        .is_err());
    }

    #[test]
    fn public_addresses_are_typed_and_validated() {
        let http = ChannelIngressEndpoint::loopback_http(8787, "/svc/alerts");
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Url {
                url: "https://hook.example/svc/alerts".into()
            },
            &http
        )
        .is_ok());
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Url {
                url: "http://hook.example/svc/alerts".into()
            },
            &http
        )
        .is_err());
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Url {
                url: "https://hook.example/wrong".into()
            },
            &http
        )
        .is_err());

        let opaque = ChannelIngressEndpoint {
            transport: IngressTransport::Tcp("127.0.0.1:9000".parse().unwrap()),
            protocol: ApplicationProtocol::Opaque("postgres".into()),
        };
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Socket {
                host: "tcp.example".into(),
                port: 443,
            },
            &opaque,
        )
        .is_ok());
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Url {
                url: "https://tcp.example/".into(),
            },
            &opaque,
        )
        .is_err());

        let websocket = ChannelIngressEndpoint {
            transport: IngressTransport::Tcp("127.0.0.1:9001".parse().unwrap()),
            protocol: ApplicationProtocol::WebSocket {
                path: "/events".into(),
            },
        };
        assert!(validate_public_endpoint(
            &ChannelPublicEndpoint::Url {
                url: "wss://events.example/events".into(),
            },
            &websocket,
        )
        .is_ok());
    }

    #[test]
    fn registration_carries_capability_metadata_not_channel_secrets() {
        let request = RegisterRequest {
            construct_instance_id: "installation",
            channel_id: "alerts",
            transport: "tcp",
            protocol: ProtocolProfile::Http {
                path: "/svc/alerts".into(),
            },
            authorization: "channel",
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["authorization"], "channel");
        assert_eq!(value["protocol"]["kind"], "http");
        assert_eq!(value["protocol"]["path"], "/svc/alerts");
        assert!(value.get("token").is_none());
        assert!(value.get("upstream_password").is_none());
        assert!(value.get("service_name").is_none());

        let websocket = serde_json::to_value(ProtocolProfile::WebSocket {
            path: "/events".into(),
        })
        .unwrap();
        assert_eq!(websocket["kind"], "websocket");
    }
}
