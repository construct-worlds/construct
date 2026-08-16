//! Interactive owner authorization shared by every first-party publication.
//!
//! The browser handoff belongs to the provider, not to remote control. Keeping
//! it behind this tiny display boundary lets remote control and arbitrary
//! channel publications authenticate through the same account flow without
//! sharing lifecycle state or credentials.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

#[async_trait]
pub trait AuthorizationDisplay: Send + Sync {
    /// Show or clear the short-lived browser URL. This is never an owner token.
    async fn set_authorization_url(&self, url: Option<String>);
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

pub async fn authorize(
    http: &reqwest::Client,
    display: &dyn AuthorizationDisplay,
    publication_api_url: &str,
) -> Result<String> {
    let request = http
        .post(auth_api_url(publication_api_url)?)
        .send()
        .await
        .context("start tunnel.zarvis.ai login")?
        .error_for_status()
        .context("tunnel.zarvis.ai rejected login request")?
        .json::<AuthRequest>()
        .await
        .context("decode tunnel.zarvis.ai login request")?;

    let verification_url = validate_https_url(&request.verification_url)?;
    display
        .set_authorization_url(Some(verification_url.clone()))
        .await;
    tracing::info!(url = %verification_url, "authorize tunnel.zarvis.ai in a browser");
    if let Err(error) = open_browser(&verification_url) {
        tracing::info!(%error, url = %verification_url, "could not open login browser; showing URL in client");
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
    display.set_authorization_url(None).await;
    result
}

pub(crate) fn auth_api_url(publication_api_url: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(publication_api_url)
        .context("invalid Construct publication API URL")?;
    let path = url.path().trim_end_matches('/');
    let prefix = path
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .ok_or_else(|| anyhow!("Construct publication API URL must have a resource path"))?;
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

fn validate_https_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value).context("operator returned an invalid HTTPS URL")?;
    if url.scheme() != "https" || url.host_str().is_none() {
        anyhow::bail!("operator returned a non-HTTPS URL");
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_endpoint_is_sibling_of_any_publication_collection() {
        assert_eq!(
            auth_api_url("https://tunnel.zarvis.ai/api/v1/tunnels")
                .unwrap()
                .as_str(),
            "https://tunnel.zarvis.ai/api/v1/auth/requests"
        );
        assert_eq!(
            auth_api_url("https://tunnel.zarvis.ai/api/v1/channel-publications")
                .unwrap()
                .as_str(),
            "https://tunnel.zarvis.ai/api/v1/auth/requests"
        );
    }
}
