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

pub fn preflight() -> Result<(), String> {
    let binary = binary();
    if which::which(&binary).is_err() {
        return Err(format!(
            "{binary} is not on PATH — install wstunnel or set CONSTRUCT_WSTUNNEL_BIN"
        ));
    }
    if std::env::var("CONSTRUCT_TUNNEL_OWNER_TOKEN").is_err() {
        return Err(
            "sign in at tunnel.zarvis.ai, then set CONSTRUCT_TUNNEL_OWNER_TOKEN".to_string(),
        );
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
        .or_else(|| std::env::var("CONSTRUCT_TUNNEL_SUBDOMAIN").ok())
        .ok_or_else(|| {
            anyhow!(
                "a subdomain is required; use `/remote-control construct <name>` or set \
                 CONSTRUCT_TUNNEL_SUBDOMAIN"
            )
        })?;
    validate_subdomain(&subdomain)?;

    let owner_token = std::env::var("CONSTRUCT_TUNNEL_OWNER_TOKEN")
        .context("CONSTRUCT_TUNNEL_OWNER_TOKEN is not set")?;
    let api_url =
        std::env::var("CONSTRUCT_TUNNEL_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    let http = reqwest::Client::new();
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
    let refresh_after = Duration::from_secs(
        registration
            .expires_in_seconds
            .saturating_sub(60)
            .max(1),
    );
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
}
