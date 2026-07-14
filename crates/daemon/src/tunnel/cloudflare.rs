//! `cloudflared` quick-tunnel backend.
//!
//! Spawns `cloudflared tunnel --url http://127.0.0.1:<port>`, scrapes
//! the `*.trycloudflare.com` URL out of its stderr banner, and
//! publishes that URL on [`RemoteState`].
//!
//! Quick tunnels are deliberately ephemeral and account-free: the URL
//! is a fresh random subdomain every run, which is exactly why the QR
//! code matters — nobody is going to type it, and nobody can guess it.
//! The flip side is that the URL rotates whenever cloudflared
//! respawns, so the supervisor clears the published URL on death
//! rather than leaving a stale one on screen.

use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::remote::RemoteState;

/// Is `cloudflared` installed?
pub fn preflight() -> Result<(), String> {
    if which::which("cloudflared").is_err() {
        return Err(
            "cloudflared is not on PATH — install it with `brew install cloudflared`, \
             or from github.com/cloudflare/cloudflared/releases"
                .to_string(),
        );
    }
    Ok(())
}

/// Run one cloudflared child to completion. Returns when it exits;
/// the caller decides whether to respawn.
pub async fn run_once(remote: &RemoteState, local_port: u16) -> Result<()> {
    let mut child = Command::new("cloudflared")
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{local_port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Detach into a new process group so cloudflared survives the
        // daemon's `exec()` on `/construct restart`. With this, the
        // new daemon adopts the still-running subprocess and the URL
        // stays valid across the restart.
        //
        // `kill_on_drop` stays false — we explicitly SIGTERM the
        // recorded PID from the stop path when we actually want it
        // gone. A daemon SIGKILL still leaks the subprocess; that's
        // the trade-off for URL preservation, and the next boot's
        // stale-snapshot check refuses to adopt an orphan.
        .process_group(0)
        .spawn()
        .context("spawn cloudflared")?;
    let pid = child.id().unwrap_or(0);
    if pid != 0 {
        remote.set_tunnel_pid(pid).await;
    }

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("cloudflared stderr not captured"))?;

    let scan_remote = remote.clone();
    let scan_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut announced = false;
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "cloudflared", "{line}");
            if !announced {
                if let Some(https_url) = extract_trycloudflare_url(&line) {
                    let host_path = https_url.trim_start_matches("https://");
                    // What the user scans is the browser-facing
                    // https:// form: the HTML at that URL boots a
                    // small JS app that swaps `http(s)` → `ws(s)` for
                    // its WebSocket back to the daemon. Pointing the
                    // QR at the `wss://` form makes it unscannable in
                    // any browser-camera flow — which is exactly what
                    // bit us in the first phone test.
                    let browser_url = format!("https://{host_path}/");
                    tracing::info!(
                        browser = %browser_url,
                        wss = %format!("wss://{host_path}/"),
                        "cloudflare tunnel ready"
                    );
                    scan_remote.set_tunnel_url(Some(browser_url)).await;
                    announced = true;
                }
            }
            // Keep draining so the child never blocks on a full pipe.
        }
    });

    let status = child.wait().await.context("wait cloudflared")?;
    scan_task.abort();
    if !status.success() {
        return Err(anyhow!("cloudflared exited: {status}"));
    }
    Ok(())
}

/// Scan a single line of cloudflared output for the public quick-
/// tunnel URL. Looks for the `https://<sub>.trycloudflare.com` shape
/// and ignores any other URL in the banner (cloudflared advertises its
/// own docs site, which we must not mistake for the tunnel).
fn extract_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    // Trim at the first whitespace / control char — the banner pads
    // the URL with spaces and box-drawing characters, so taking to
    // end-of-line would swallow them.
    let end = rest
        .find(|c: char| c.is_whitespace() || c.is_control())
        .unwrap_or(rest.len());
    let candidate = &rest[..end];
    let trimmed = candidate.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '/'));
    if trimmed.contains(".trycloudflare.com") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real banner is multi-line ASCII art, so we test the
    /// representative line shapes rather than the whole block.
    #[test]
    fn extracts_url_from_banner_line() {
        let line = "2026-05-19 INF +-------------------------------------+";
        assert_eq!(extract_trycloudflare_url(line), None);

        let line = "2026-05-19 INF | https://big-fox-42.trycloudflare.com |";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://big-fox-42.trycloudflare.com"),
        );

        let line = "Visit https://abc-def.trycloudflare.com to access your tunnel";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://abc-def.trycloudflare.com"),
        );

        // cloudflared mentions its own homepage in the banner; that
        // must not be mistaken for the tunnel URL.
        let line = "Documentation: https://developers.cloudflare.com/argo-tunnel/";
        assert_eq!(extract_trycloudflare_url(line), None);
    }
}
