//! Construct's authenticated, stable-name tunnel backend.
//!
//! The control plane authenticates the tunnel owner, allocates an
//! ephemeral reverse port, and returns a short-lived capability that
//! permits exactly that reverse binding. `wstunnel` carries the bytes;
//! the service's browser gateway supplies social login and maps the
//! stable hostname to the runtime-only port.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::remote::RemoteState;

const DEFAULT_API_URL: &str = "https://tunnel.zarvis.ai/api/v1/tunnels";

#[derive(Serialize)]
struct RegisterRequest<'a> {
    subdomain: &'a str,
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
}

#[derive(Deserialize)]
struct AuthRequest {
    verification_url: String,
    poll_url: String,
    poll_token: String,
    expires_in_seconds: u64,
    interval_seconds: u64,
}

#[derive(Deserialize)]
struct AuthPoll {
    #[serde(default)]
    owner_token: Option<String>,
}

pub fn preflight() -> Result<(), String> {
    let binary = binary();
    if which::which(&binary).is_err() {
        return Err(format!(
            "{binary} is not on PATH — install wstunnel or set CONSTRUCT_WSTUNNEL_BIN"
        ));
    }
    Ok(())
}

pub async fn run_once(
    remote: &RemoteState,
    local_port: u16,
    requested_subdomain: Option<&str>,
) -> Result<()> {
    let subdomain = requested_subdomain
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("a tunnel name is required; choose one in `/remote-connect`"))?;
    validate_subdomain(&subdomain)?;

    let api_url =
        std::env::var("CONSTRUCT_TUNNEL_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let http = reqwest::Client::new();
    let owner_token = authorize(&http, remote, &api_url).await?;
    let registration = http
        .post(&api_url)
        .bearer_auth(&owner_token)
        .json(&RegisterRequest {
            subdomain: &subdomain,
            upstream_username: "remote",
            upstream_password: remote.password(),
        })
        .send()
        .await
        .context("contact Construct tunnel service")?
        .error_for_status()
        .context("Construct tunnel registration rejected")?
        .json::<Registration>()
        .await
        .context("decode Construct tunnel registration")?;

    let reverse = format!(
        "tcp://127.0.0.1:{}:127.0.0.1:{local_port}",
        registration.remote_port
    );
    let auth_header = format!("Authorization: Bearer {}", registration.tunnel_token);
    let mut child = Command::new(binary())
        .args([
            "client",
            "--log-lvl",
            "WARN",
            "--remote-to-local",
            &reverse,
            "--http-headers",
            &auth_header,
            &registration.relay_url,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("spawn wstunnel client")?;

    let pid = child.id().unwrap_or(0);
    if pid != 0 {
        remote.set_tunnel_pid(pid).await;
    }
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("wstunnel stderr not captured"))?;
    let drain = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "wstunnel", "{line}");
        }
    });

    let public_url = normalize_public_url(&registration.public_url)?;
    let ready_url = registration.ready_url;
    let tunnel_token = registration.tunnel_token;
    let refresh_after =
        Duration::from_secs(registration.expires_in_seconds.saturating_sub(60).max(1));
    let readiness = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ready = http
                .get(&ready_url)
                .bearer_auth(&tunnel_token)
                .send()
                .await
                .map(|response| response.status().is_success())
                .unwrap_or(false);
            if ready {
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

    tokio::pin!(readiness);
    tokio::select! {
        ready = &mut readiness => ready?,
        status = child.wait() => {
            drain.abort();
            let status = status.context("wait for wstunnel client")?;
            return Err(anyhow!("wstunnel exited before the tunnel was ready: {status}"));
        }
    }

    let status = tokio::select! {
        status = child.wait() => status.context("wait for wstunnel client")?,
        _ = tokio::time::sleep(refresh_after) => {
            child.start_kill().context("stop wstunnel for capability refresh")?;
            child.wait().await.context("wait for refreshing wstunnel client")?
        }
    };
    drain.abort();
    if !status.success() {
        return Err(anyhow!("wstunnel exited: {status}"));
    }
    Ok(())
}

async fn authorize(
    http: &reqwest::Client,
    remote: &RemoteState,
    tunnel_api_url: &str,
) -> Result<String> {
    let auth_api_url = auth_api_url(tunnel_api_url)?;
    let request = http
        .post(auth_api_url)
        .send()
        .await
        .context("start tunnel.zarvis.ai login")?
        .error_for_status()
        .context("tunnel.zarvis.ai rejected login request")?
        .json::<AuthRequest>()
        .await
        .context("decode tunnel.zarvis.ai login request")?;

    let verification_url = validate_https_url(&request.verification_url)?;
    remote.set_auth_url(Some(verification_url.clone())).await;
    tracing::info!(url = %verification_url, "authorize tunnel.zarvis.ai in a browser");
    if let Err(error) = open_browser(&verification_url) {
        tracing::info!(%error, url = %verification_url, "could not open login browser; showing URL in remote-connect dialog");
    }

    let interval = Duration::from_secs(request.interval_seconds.clamp(1, 10));
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(request.expires_in_seconds.clamp(1, 10 * 60));
    let result = async {
        loop {
            let response = match http
                .get(&request.poll_url)
                .bearer_auth(&request.poll_token)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::debug!(%error, "login poll failed; retrying");
                    tokio::time::sleep(interval).await;
                    continue;
                }
                Err(error) => break Err(error).context("poll tunnel.zarvis.ai login"),
            };
            if response.status() == reqwest::StatusCode::ACCEPTED {
                if tokio::time::Instant::now() >= deadline {
                    break Err(anyhow!("tunnel.zarvis.ai login expired; start again"));
                }
                tokio::time::sleep(interval).await;
                continue;
            }
            let poll = response
                .error_for_status()
                .context("tunnel.zarvis.ai login failed")?
                .json::<AuthPoll>()
                .await
                .context("decode tunnel.zarvis.ai login result")?;
            match poll.owner_token {
                Some(token) if !token.is_empty() => break Ok(token),
                _ => break Err(anyhow!("tunnel.zarvis.ai login omitted authorization")),
            }
        }
    }
    .await;
    remote.set_auth_url(None).await;
    result
}

fn auth_api_url(tunnel_api_url: &str) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(tunnel_api_url).context("invalid Construct tunnel API URL")?;
    let path = url.path().trim_end_matches('/');
    let prefix = path
        .strip_suffix("/tunnels")
        .ok_or_else(|| anyhow!("Construct tunnel API URL must end in /tunnels"))?;
    url.set_path(&format!("{prefix}/auth/requests"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");

    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("open browser for {url}"))?;
    Ok(())
}

fn binary() -> String {
    std::env::var("CONSTRUCT_WSTUNNEL_BIN").unwrap_or_else(|_| "wstunnel".to_string())
}

fn validate_subdomain(value: &str) -> Result<()> {
    let valid = (1..=63).contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        anyhow::bail!(
            "invalid tunnel subdomain `{value}`; use 1–63 lowercase letters, digits, or hyphens"
        );
    }
    Ok(())
}

fn normalize_public_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("service returned an invalid public URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("service returned a non-HTTPS public URL");
    }
    Ok(format!("{}/", value.trim_end_matches('/')))
}

fn validate_https_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("service returned an invalid HTTPS URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("service returned a non-HTTPS URL");
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_validation_is_dns_label_safe() {
        for valid in ["demo", "demo-2", "a"] {
            assert!(validate_subdomain(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "Demo", "-demo", "demo-", "two.labels", "has space"] {
            assert!(validate_subdomain(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn public_url_must_be_https() {
        assert_eq!(
            normalize_public_url("https://demo.user.tunnel.zarvis.ai").unwrap(),
            "https://demo.user.tunnel.zarvis.ai/"
        );
        assert!(normalize_public_url("http://demo.example").is_err());
    }

    #[test]
    fn auth_endpoint_is_derived_from_tunnel_endpoint() {
        assert_eq!(
            auth_api_url("https://tunnel.zarvis.ai/api/v1/tunnels")
                .unwrap()
                .as_str(),
            "https://tunnel.zarvis.ai/api/v1/auth/requests"
        );
        assert!(auth_api_url("https://tunnel.zarvis.ai/wrong").is_err());
    }
}
