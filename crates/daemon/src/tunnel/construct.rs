//! Construct's authenticated, stable-name tunnel backend.
//!
//! The control plane authenticates the tunnel owner, allocates an
//! ephemeral reverse port, and returns a short-lived capability that
//! permits exactly that reverse binding. `wstunnel` carries the bytes;
//! the operator's browser gateway supplies social login and maps the
//! stable hostname to the runtime-only port.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::Parser;
use serde::{Deserialize, Serialize};
use wstunnel::executor::JoinSetTokioExecutor;

use crate::remote::RemoteState;
use crate::tunnel::auth::{self, AuthorizationDisplay};

const DEFAULT_API_URL: &str = "https://tunnel.zarvis.ai/api/v1/tunnels";

/// Cadence of the post-readiness `ready_url` health poll, and how many
/// consecutive failures prove the gateway no longer routes this tunnel.
/// The gateway holds routes in memory only, so a operator deploy restarts
/// it with an empty table while the in-process wstunnel client keeps
/// retrying its transport forever — without this poll the daemon would
/// advertise a dead tunnel until the ~24h capability refresh.
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(20);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_CHECK_FAILURES: u32 = 3;

#[derive(Serialize)]
struct RegisterRequest<'a> {
    construct_instance_id: &'a str,
    tunnel_name: &'a str,
    upstream_username: &'static str,
    upstream_password: &'a str,
}

#[derive(Deserialize)]
struct Registration {
    public_url: String,
    relay_url: String,
    remote_port: u16,
    tunnel_token: String,
    ready_url: String,
    expires_in_seconds: u64,
    /// Credential authorizing the next unattended re-registration of this
    /// reservation. Absent when talking to a operator that predates it, in
    /// which case recovery falls back to the owner credential as before.
    #[serde(default)]
    reregistration_token: Option<String>,
}

/// Which credential a registration attempt presented. The distinction only
/// matters when the operator refuses it: a refused re-registration credential
/// should fall back to the owner credential, while a refused owner credential
/// is what forces a fresh browser handoff.
enum Credential {
    Reregistration(String),
    Owner(String),
}

impl Credential {
    fn bearer(&self) -> &str {
        match self {
            Credential::Reregistration(token) | Credential::Owner(token) => token,
        }
    }
}

/// Which held credential to present, or `None` when the daemon holds neither
/// and has no choice but an interactive handoff.
///
/// The re-registration credential wins whenever it is held. Reaching for the
/// owner credential first would work just as often, but it expires on a login's
/// timescale rather than a route's, so preferring it quietly reintroduces the
/// browser prompt this exists to avoid.
fn select_credential(
    reregistration: &Option<String>,
    owner: &Option<String>,
) -> Option<Credential> {
    if let Some(token) = reregistration {
        return Some(Credential::Reregistration(token.clone()));
    }
    owner.clone().map(Credential::Owner)
}

/// Forget exactly the credential the operator just refused.
///
/// A refused re-registration credential says nothing about the owner
/// credential, which may still be current — dropping both would open a browser
/// that was never needed. A refused owner credential is the case that genuinely
/// requires re-authorization.
fn forget_refused(
    credential: &Credential,
    reregistration: &mut Option<String>,
    owner: &mut Option<String>,
) {
    match credential {
        Credential::Reregistration(_) => *reregistration = None,
        Credential::Owner(_) => *owner = None,
    }
}

pub fn preflight() -> Result<(), String> {
    Ok(())
}

#[derive(Parser)]
struct InProcessClient {
    #[command(flatten)]
    client: wstunnel::config::Client,
}

pub async fn run_once(
    remote: &RemoteState,
    local_port: u16,
    requested_tunnel_name: Option<&str>,
    construct_instance_id: &str,
    cached_owner_token: &mut Option<String>,
    cached_reregistration_token: &mut Option<String>,
) -> Result<()> {
    let tunnel_name = requested_tunnel_name
        .filter(|name| valid_tunnel_name(name))
        .ok_or_else(|| anyhow!("choose a tunnel name using 1–32 lowercase letters, numbers, or hyphens"))?;
    let api_url =
        std::env::var("CONSTRUCT_TUNNEL_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let http = reqwest::Client::new();
    // Prefer the re-registration credential. It is scoped to this reservation
    // and outlives the owner credential, which is what lets a lost route come
    // back with nobody at this machine — reaching for the owner credential
    // first would re-open a browser the remote user cannot get to.
    let credential = match select_credential(cached_reregistration_token, cached_owner_token) {
        Some(credential) => credential,
        None => {
            let token = auth::authorize(&http, remote, &api_url).await?;
            *cached_owner_token = Some(token.clone());
            Credential::Owner(token)
        }
    };
    let response = http
        .post(&api_url)
        .bearer_auth(credential.bearer())
        .json(&RegisterRequest {
            construct_instance_id,
            tunnel_name,
            upstream_username: "remote",
            upstream_password: remote.password(),
        })
        .send()
        .await
        .context("contact Construct tunnel operator")?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        if matches!(credential, Credential::Reregistration(_)) {
            tracing::info!(
                "re-registration credential was refused; falling back to owner authorization"
            );
        }
        forget_refused(
            &credential,
            cached_reregistration_token,
            cached_owner_token,
        );
    }
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail = detail.trim();
        if detail.is_empty() {
            anyhow::bail!("Construct tunnel registration rejected ({status})");
        }
        anyhow::bail!("Construct tunnel registration rejected ({status}): {detail}");
    }
    let registration = response
        .json::<Registration>()
        .await
        .context("decode Construct tunnel registration")?;
    // Every registration mints a fresh one and retires the previous, so this
    // has to be taken on success or the next recovery presents a spent
    // credential. Memory only, and never logged: spec 0146 keeps tunnel
    // credentials off disk, and 0162 does not relax that.
    if let Some(token) = registration.reregistration_token.clone() {
        if !token.is_empty() {
            *cached_reregistration_token = Some(token);
        }
    }

    let reverse = format!(
        "tcp://127.0.0.1:{}:127.0.0.1:{local_port}",
        registration.remote_port
    );
    let auth_header = format!("Authorization: Bearer {}", registration.tunnel_token);
    let client = InProcessClient::try_parse_from([
        "construct-wstunnel",
        "--remote-to-local",
        &reverse,
        "--http-headers",
        &auth_header,
        &registration.relay_url,
    ])
    .context("configure in-process wstunnel client")?
    .client;
    let executor = JoinSetTokioExecutor::default();
    let tunnel = wstunnel::run_client(client, executor);

    let public_url = normalize_public_url(&registration.public_url)?;
    let ready_url = registration.ready_url;
    let tunnel_token = registration.tunnel_token;
    let refresh_after =
        Duration::from_secs(registration.expires_in_seconds.saturating_sub(60).max(1));
    let readiness = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if tunnel_ready(&http, &ready_url, &tunnel_token).await {
                remote.set_tunnel_url(Some(public_url)).await;
                return Ok::<(), anyhow::Error>(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "Construct tunnel did not become reachable within 15s"
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };

    tokio::pin!(readiness, tunnel);
    tokio::select! {
        ready = &mut readiness => ready?,
        result = &mut tunnel => {
            result.context("run in-process wstunnel client")?;
            return Err(anyhow!("wstunnel exited before the tunnel was ready"));
        }
    }

    // The gateway can lose this registration without the transport ever
    // noticing (its route table is memory-only, so a deploy restart wipes
    // it while wstunnel keeps retrying the relay). Keep verifying the
    // ready endpoint; a sustained run of failures means the route is gone,
    // so return cleanly and let the supervisor loop re-register with the
    // cached owner token and republish the public URL.
    let mut health = tokio::time::interval_at(
        tokio::time::Instant::now() + HEALTH_CHECK_INTERVAL,
        HEALTH_CHECK_INTERVAL,
    );
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let refresh = tokio::time::sleep(refresh_after);
    tokio::pin!(refresh);
    let mut consecutive_failures = 0u32;
    loop {
        tokio::select! {
            result = &mut tunnel => {
                result.context("run in-process wstunnel client")?;
                return Err(anyhow!("wstunnel exited"));
            }
            _ = &mut refresh => return Ok(()),
            _ = health.tick() => {
                if tunnel_ready(&http, &ready_url, &tunnel_token).await {
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    if consecutive_failures >= HEALTH_CHECK_FAILURES {
                        tracing::warn!(
                            failures = consecutive_failures,
                            "Construct tunnel gateway no longer routes this tunnel; re-registering"
                        );
                        remote.set_tunnel_url(None).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// One authenticated probe of the gateway's ready endpoint: true only
/// when the gateway still knows this tunnel and can reach its reverse
/// port. Transport errors and non-success statuses both count as "not
/// ready" — the caller only distinguishes healthy from not.
async fn tunnel_ready(http: &reqwest::Client, ready_url: &str, tunnel_token: &str) -> bool {
    http.get(ready_url)
        .bearer_auth(tunnel_token)
        .timeout(HEALTH_CHECK_TIMEOUT)
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

#[async_trait]
impl AuthorizationDisplay for RemoteState {
    async fn set_authorization_url(&self, url: Option<String>) {
        self.set_auth_url(url).await;
    }
}

fn normalize_public_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("operator returned an invalid public URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("operator returned a non-HTTPS public URL");
    }
    Ok(format!("{}/", value.trim_end_matches('/')))
}

fn valid_tunnel_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_must_be_https() {
        assert_eq!(
            normalize_public_url("https://swift-willow-4827.tunnel.zarvis.ai").unwrap(),
            "https://swift-willow-4827.tunnel.zarvis.ai/"
        );
        assert!(normalize_public_url("http://demo.example").is_err());
    }

    #[test]
    fn re_registration_credential_is_preferred_over_the_owner_credential() {
        let reregistration = Some("scoped".to_string());
        let owner = Some("interactive".to_string());
        assert!(matches!(
            select_credential(&reregistration, &owner),
            Some(Credential::Reregistration(token)) if token == "scoped"
        ));
    }

    #[test]
    fn owner_credential_is_used_when_no_scoped_credential_is_held() {
        let owner = Some("interactive".to_string());
        assert!(matches!(
            select_credential(&None, &owner),
            Some(Credential::Owner(token)) if token == "interactive"
        ));
    }

    #[test]
    fn holding_neither_credential_forces_an_interactive_handoff() {
        assert!(select_credential(&None, &None).is_none());
    }

    /// A refused scoped credential must not cost the owner credential too: the
    /// next attempt should quietly retry with it rather than open a browser the
    /// remote user cannot reach.
    #[test]
    fn a_refused_scoped_credential_leaves_the_owner_credential_intact() {
        let mut reregistration = Some("scoped".to_string());
        let mut owner = Some("interactive".to_string());
        let presented = select_credential(&reregistration, &owner).unwrap();

        forget_refused(&presented, &mut reregistration, &mut owner);

        assert_eq!(reregistration, None);
        assert_eq!(owner.as_deref(), Some("interactive"));
        assert!(matches!(
            select_credential(&reregistration, &owner),
            Some(Credential::Owner(_))
        ));
    }

    #[test]
    fn a_refused_owner_credential_is_the_one_that_forces_re_authorization() {
        let mut reregistration = None;
        let mut owner = Some("interactive".to_string());
        let presented = select_credential(&reregistration, &owner).unwrap();

        forget_refused(&presented, &mut reregistration, &mut owner);

        assert_eq!(owner, None);
        assert!(select_credential(&reregistration, &owner).is_none());
    }

    /// A operator that predates the credential simply omits the field, and the
    /// daemon has to keep working against it.
    #[test]
    fn registration_without_a_scoped_credential_still_decodes() {
        let body = serde_json::json!({
            "public_url": "https://demo.tunnel.zarvis.ai",
            "relay_url": "wss://relay.tunnel.zarvis.ai",
            "remote_port": 22255u16,
            "tunnel_token": "t",
            "ready_url": "https://tunnel.zarvis.ai/api/v1/tunnels/x/ready",
            "expires_in_seconds": 86400u64,
        });
        let registration: Registration = serde_json::from_value(body).unwrap();
        assert_eq!(registration.reregistration_token, None);
    }

    #[test]
    fn registration_carries_the_scoped_credential_when_the_operator_issues_one() {
        let body = serde_json::json!({
            "public_url": "https://demo.tunnel.zarvis.ai",
            "relay_url": "wss://relay.tunnel.zarvis.ai",
            "remote_port": 22255u16,
            "tunnel_token": "t",
            "ready_url": "https://tunnel.zarvis.ai/api/v1/tunnels/x/ready",
            "expires_in_seconds": 86400u64,
            "reregistration_token": "scoped",
            "reregistration_expires_in_seconds": 2592000u64,
        });
        let registration: Registration = serde_json::from_value(body).unwrap();
        assert_eq!(registration.reregistration_token.as_deref(), Some("scoped"));
    }

    #[test]
    fn tunnel_name_is_a_short_lowercase_dns_label() {
        assert!(valid_tunnel_name("quiet-otter-42"));
        assert!(valid_tunnel_name("demo"));
        assert!(!valid_tunnel_name(""));
        assert!(!valid_tunnel_name("-demo"));
        assert!(!valid_tunnel_name("demo-"));
        assert!(!valid_tunnel_name("Demo"));
        assert!(!valid_tunnel_name(&"a".repeat(33)));
    }
}
