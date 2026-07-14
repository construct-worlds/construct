//! `tailscale serve` backend.
//!
//! Reaches the daemon from the user's own devices instead of from the
//! whole internet. `tailscale serve` proxies
//! `https://<node>.<tailnet>.ts.net/` to our loopback listener, and
//! only machines logged into the same tailnet can resolve or reach
//! that name — access is gated by tailnet ACLs, not by URL secrecy.
//!
//! Two consequences worth knowing, because they make this backend
//! shaped differently from [`super::cloudflare`]:
//!
//! - **The URL is derivable, not scraped.** It comes from
//!   `tailscale status --json`, so we know it *before* spawning
//!   anything, and there is no banner to parse. What we wait for is
//!   readiness, not discovery.
//! - **The URL is stable.** It is the node's own name, so it survives
//!   restarts and respawns. A user can bookmark it, which is the whole
//!   appeal next to a cloudflared URL that rotates on every spawn.
//!
//! We deliberately use `serve` (tailnet-only) rather than `funnel`
//! (public internet). Funnel would duplicate what Cloudflare already
//! does, but with strictly more setup: it needs a `funnel` node
//! attribute in the tailnet policy and HTTPS certs enabled, both of
//! which require an admin approving an interactive browser flow that a
//! daemon cannot drive on the user's behalf.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::remote::RemoteState;

/// How long to let `tailscale status --json` run before giving up. The
/// CLI talks to a local daemon over a unix socket, so this is
/// generous; the timeout exists to keep a wedged tailscaled from
/// hanging the remote-control dialog forever.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace period between spawning `tailscale serve` and publishing its
/// URL. Serve registers its mapping with the local tailscaled almost
/// immediately, but "almost" is not "before the user's phone can scan
/// the QR we are about to draw". Waiting out a beat — and confirming
/// the child is still alive — means a scan lands on a listener that is
/// actually serving, rather than a 502.
const SERVE_SETTLE: Duration = Duration::from_millis(700);

/// Locate the `tailscale` CLI.
///
/// The Mac App Store build is the gotcha here: it ships the CLI as the
/// *same binary as the GUI*, buried in the app bundle and never
/// symlinked onto PATH. A user with a perfectly working Tailscale can
/// therefore have no `tailscale` command at all, so PATH alone is not
/// a sound availability check.
fn find_binary() -> Option<PathBuf> {
    if let Ok(p) = which::which("tailscale") {
        return Some(p);
    }
    const FALLBACKS: &[&str] = &[
        // Standalone/macsys and Homebrew-on-Intel.
        "/usr/local/bin/tailscale",
        // Homebrew on Apple Silicon.
        "/opt/homebrew/bin/tailscale",
        // Mac App Store bundle — note the capital T, and that this is
        // the GUI binary doing double duty as the CLI.
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        // Linux distro packages.
        "/usr/bin/tailscale",
    ];
    FALLBACKS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Build a `Command` for the tailscale CLI.
///
/// `TAILSCALE_BE_CLI` is what stops the App Store bundle from deciding
/// it was asked to launch the GUI: the same binary branches on this to
/// pick CLI mode. It is harmless for every other build, so we set it
/// unconditionally rather than trying to detect which variant we found.
fn cli(bin: &PathBuf) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("TAILSCALE_BE_CLI", "1");
    cmd
}

/// The subset of `tailscale status --json` we care about.
#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    /// Domains tailscaled can obtain a TLS cert for. This is the
    /// authoritative serve hostname when present.
    #[serde(rename = "CertDomains")]
    cert_domains: Option<Vec<String>>,
    #[serde(rename = "Self")]
    self_node: Option<SelfNode>,
}

#[derive(Debug, Deserialize)]
struct SelfNode {
    /// MagicDNS name, e.g. `"box.tail1234.ts.net."` — note the
    /// trailing dot, which is a fully-qualified-domain-name artifact
    /// and must not survive into a URL.
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
}

impl Status {
    /// The hostname `tailscale serve` will publish us under.
    ///
    /// `CertDomains` is authoritative — it is precisely the set of
    /// names tailscaled can get a certificate for, and serve needs a
    /// certificate. We fall back to the node's MagicDNS name only
    /// because a tailnet with HTTPS freshly enabled can briefly report
    /// an empty `CertDomains`, and the two agree in practice.
    fn serve_host(&self) -> Option<String> {
        if let Some(d) = self
            .cert_domains
            .as_ref()
            .and_then(|v| v.iter().find(|d| !d.is_empty()))
        {
            return Some(d.clone());
        }
        let dns = self.self_node.as_ref()?.dns_name.as_ref()?;
        let trimmed = dns.trim_end_matches('.');
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

/// Ask the local tailscaled what it thinks its state is.
async fn query_status(bin: &PathBuf) -> Result<Status> {
    let out = tokio::time::timeout(
        STATUS_TIMEOUT,
        cli(bin).args(["status", "--json"]).output(),
    )
    .await
    .map_err(|_| anyhow!("`tailscale status` timed out"))?
    .context("run `tailscale status --json`")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`tailscale status` failed: {}",
            err.trim().lines().next().unwrap_or("(no output)")
        ));
    }
    serde_json::from_slice(&out.stdout).context("parse `tailscale status --json`")
}

/// Is Tailscale installed, running, logged in, and able to serve?
///
/// Every `Err` here is phrased as the next thing the user should do,
/// because this string is what the dialog paints under a greyed-out
/// Tailscale button.
pub async fn preflight() -> Result<(), String> {
    let Some(bin) = find_binary() else {
        return Err(
            "Tailscale is not installed — get it from tailscale.com/download".to_string(),
        );
    };
    let status = match query_status(&bin).await {
        Ok(s) => s,
        // The CLI failing to reach its daemon is overwhelmingly "the
        // app isn't running", and saying so is more use than echoing a
        // socket error.
        Err(e) => {
            tracing::debug!(error = %e, "tailscale status probe failed");
            return Err("Tailscale is installed but not running — start the Tailscale app".to_string());
        }
    };
    match status.backend_state.as_deref() {
        Some("Running") => {}
        Some("NeedsLogin") | Some("NoState") | Some("Stopped") => {
            return Err("Tailscale is not logged in — run `tailscale up`".to_string());
        }
        Some("NeedsMachineAuth") => {
            return Err(
                "This machine is waiting for tailnet approval — approve it in the Tailscale admin console"
                    .to_string(),
            );
        }
        Some(other) => return Err(format!("Tailscale is not ready (state: {other})")),
        None => return Err("Tailscale did not report a state".to_string()),
    }
    if status.serve_host().is_none() {
        return Err(
            "Tailscale has no HTTPS name for this machine — enable HTTPS certificates \
             for your tailnet in the admin console"
                .to_string(),
        );
    }
    Ok(())
}

/// Run one `tailscale serve` child to completion.
///
/// The child runs in the **foreground** on purpose. `--bg` would write
/// the mapping into tailscaled's persistent config, where it outlives
/// this process, this daemon, and the next reboot — so a daemon that
/// crashed would leave the machine served with nothing around to
/// un-serve it. A foreground child removes its own mapping when it
/// exits, which makes the tunnel's lifetime exactly the lifetime of a
/// process we hold a PID for. That is the same contract cloudflared
/// gives us, and it is what lets the supervisor treat both the same.
pub async fn run_once(remote: &RemoteState, local_port: u16) -> Result<()> {
    let bin = find_binary().ok_or_else(|| anyhow!("tailscale CLI not found"))?;
    let status = query_status(&bin).await?;
    let host = status
        .serve_host()
        .ok_or_else(|| anyhow!("tailscale reported no HTTPS name for this machine"))?;
    let url = format!("https://{host}/");

    let mut child = cli(&bin)
        .args([
            "serve",
            // Never block on a confirmation prompt: there is no one at
            // this terminal to answer it.
            "--yes",
            "--https=443",
            &format!("http://127.0.0.1:{local_port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Same reasoning as cloudflared: its own process group, so it
        // survives the daemon's `exec()` on restart and the next
        // daemon can adopt it by PID.
        .process_group(0)
        .spawn()
        .context("spawn `tailscale serve`")?;
    let pid = child.id().unwrap_or(0);
    if pid != 0 {
        remote.set_tunnel_pid(pid).await;
    }

    // Drain both pipes so the child never blocks on a full one. Serve
    // is quiet in the happy path; when it is unhappy, this is where it
    // says so, so the lines are worth keeping at debug.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "tailscale", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "tailscale", "{line}");
            }
        });
    }

    // Publish only after the child has proved it intends to stay up.
    // A serve that is going to fail — no permission to write the serve
    // config being the common one — fails fast, and we would rather
    // surface that as a start error than draw a QR for a URL that
    // 502s.
    tokio::time::sleep(SERVE_SETTLE).await;
    if let Some(exit) = child.try_wait().context("poll `tailscale serve`")? {
        return Err(anyhow!("`tailscale serve` exited immediately: {exit}"));
    }
    tracing::info!(browser = %url, "tailscale serve ready (tailnet-only)");
    remote.set_tunnel_url(Some(url)).await;

    let exit = child.wait().await.context("wait `tailscale serve`")?;
    if !exit.success() {
        return Err(anyhow!("`tailscale serve` exited: {exit}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Status {
        serde_json::from_str(json).expect("parse status json")
    }

    /// `CertDomains` is authoritative when tailscaled offers it.
    #[test]
    fn serve_host_prefers_cert_domains() {
        let s = parse(
            r#"{
                "BackendState": "Running",
                "CertDomains": ["box.tail1234.ts.net"],
                "Self": {"DNSName": "other.tail1234.ts.net."}
            }"#,
        );
        assert_eq!(s.serve_host().as_deref(), Some("box.tail1234.ts.net"));
    }

    /// Falling back to the MagicDNS name means stripping the FQDN's
    /// trailing dot — leaving it in produces a URL that no browser
    /// will load.
    #[test]
    fn serve_host_falls_back_to_dns_name_without_trailing_dot() {
        let s = parse(
            r#"{
                "BackendState": "Running",
                "Self": {"DNSName": "box.tail1234.ts.net."}
            }"#,
        );
        assert_eq!(s.serve_host().as_deref(), Some("box.tail1234.ts.net"));
    }

    /// An empty `CertDomains` must not shadow the fallback.
    #[test]
    fn serve_host_ignores_empty_cert_domains() {
        let s = parse(
            r#"{
                "BackendState": "Running",
                "CertDomains": [],
                "Self": {"DNSName": "box.tail1234.ts.net."}
            }"#,
        );
        assert_eq!(s.serve_host().as_deref(), Some("box.tail1234.ts.net"));
    }

    /// A logged-out node has no name to serve under.
    #[test]
    fn serve_host_absent_when_no_names() {
        let s = parse(r#"{"BackendState": "NeedsLogin"}"#);
        assert_eq!(s.serve_host(), None);
    }
}
