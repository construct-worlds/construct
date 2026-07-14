//! Tunnel providers: the things that make the remote listener
//! reachable from beyond the local network.
//!
//! The listener itself binds every interface and is gated by HTTP
//! Basic auth, so a phone on the same Wi-Fi needs no provider at all.
//! A provider is what you reach for when the phone is *not* on the
//! same Wi-Fi, and the two we support answer that differently:
//!
//! - [`cloudflare`] runs a `cloudflared` quick tunnel. The URL is
//!   public but unguessable, and it costs nothing to set up. Reach:
//!   anywhere.
//! - [`tailscale`] runs `tailscale serve`. The URL is stable and
//!   reachable only from the user's own tailnet, so access is gated
//!   by tailnet ACLs instead of by URL secrecy. Reach: your devices.
//!
//! Every backend presents the same shape to the supervisor, which is
//! what lets the rest of the daemon stay provider-agnostic:
//!
//! 1. Spawn a child process in its own process group, so it survives
//!    the daemon's `exec()` on `/construct restart` and the new daemon
//!    can adopt it by PID.
//! 2. Record that PID on [`RemoteState`] so stop + restart-adoption
//!    can find it.
//! 3. Publish a browser URL on [`RemoteState`] once — and only once —
//!    the tunnel is actually serving.
//! 4. Die when SIGTERM'd, releasing whatever it registered.
//!
//! Step 4 is why both backends run their child in the *foreground*.
//! `tailscale serve` also has a `--bg` mode that writes into
//! tailscaled's persistent config; we deliberately don't use it,
//! because a backgrounded mapping outlives a crashed daemon and would
//! leave the machine exposed with nothing left to clean it up. A
//! foreground child tears its own mapping down when it dies, which
//! makes "the tunnel lives exactly as long as the process we can see"
//! true for both providers.

pub mod cloudflare;
pub mod tailscale;

use std::time::Duration;

use construct_protocol::{RemoteProviderInfo, TunnelProvider};

use crate::remote::{process_alive, RemoteState};

/// Providers the dialog offers, in the order it offers them.
/// Tailscale leads because it is the safer of the two: it exposes the
/// daemon to the user's own devices rather than to the internet.
pub const PROVIDERS: [TunnelProvider; 2] = [TunnelProvider::Tailscale, TunnelProvider::Cloudflare];

/// Probe every provider. Read-only — nothing is spawned, so the
/// dialog can call this on every open without side effects.
pub async fn probe_all() -> Vec<RemoteProviderInfo> {
    let mut out = Vec::with_capacity(PROVIDERS.len());
    for p in PROVIDERS {
        out.push(probe(p).await);
    }
    out
}

/// Probe one provider: could it start right now, and if not, what
/// should the user do about it?
pub async fn probe(provider: TunnelProvider) -> RemoteProviderInfo {
    let detail = match provider {
        TunnelProvider::None => None,
        TunnelProvider::Cloudflare => cloudflare::preflight().err(),
        TunnelProvider::Tailscale => tailscale::preflight().await.err(),
    };
    RemoteProviderInfo {
        provider,
        available: detail.is_none(),
        detail,
    }
}

/// Long-running supervisor for one provider, `tokio::spawn`ed by the
/// remote supervisor when the user picks a provider. Loops forever:
/// if the tunnel child dies, the URL is cleared (so clients can tell
/// the URL went stale) and a fresh one is spawned with backoff.
///
/// `adopt_pid != 0` is the `/construct restart` path: a tunnel child
/// spawned by the *previous* daemon survived the `exec()` and is still
/// serving the URL we already restored from the snapshot. Adopt it —
/// poll its liveness rather than spawning a second one — and only fall
/// through to a fresh spawn once it dies. Restarting the daemon must
/// never rotate the user's URL behind their back.
pub async fn run(
    provider: TunnelProvider,
    remote: RemoteState,
    local_port: u16,
    adopt_pid: u32,
) {
    if provider == TunnelProvider::None {
        return;
    }
    let label = provider.label();

    if let Err(detail) = preflight(provider).await {
        tracing::info!(provider = label, "tunnel unavailable: {detail}");
        return;
    }

    if adopt_pid != 0 && process_alive(adopt_pid) {
        let adopted_url = remote.tunnel_url().await;
        tracing::info!(
            provider = label,
            pid = adopt_pid,
            url = adopted_url.as_deref().unwrap_or("(unknown)"),
            "adopting existing tunnel across restart"
        );
        // The adopted PID is NOT our child (it was the prior daemon's,
        // reparented to init by the new-process-group trick), so we
        // can't `wait()` on it. `kill(pid, 0)` every 2s instead — the
        // polling cost is nothing next to keeping the URL alive.
        while process_alive(adopt_pid) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tracing::warn!(
            provider = label,
            pid = adopt_pid,
            "adopted tunnel exited; spawning fresh"
        );
        remote.set_tunnel_url(None).await;
        remote.set_tunnel_pid(0).await;
    }

    let mut backoff_secs: u64 = 1;
    loop {
        match run_once(provider, &remote, local_port).await {
            Ok(()) => {
                tracing::warn!(provider = label, "tunnel exited cleanly; respawning");
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(provider = label, error = %e, "tunnel run failed; backing off");
            }
        }
        remote.set_tunnel_url(None).await;
        remote.set_tunnel_pid(0).await;
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

/// Can this provider start? `Err(detail)` carries a message written
/// for the user, not for the log — it is what the dialog paints under
/// a greyed-out button, and what the start-timeout diagnostic quotes.
pub async fn preflight(provider: TunnelProvider) -> Result<(), String> {
    match provider {
        TunnelProvider::None => Ok(()),
        TunnelProvider::Cloudflare => cloudflare::preflight(),
        TunnelProvider::Tailscale => tailscale::preflight().await,
    }
}

async fn run_once(
    provider: TunnelProvider,
    remote: &RemoteState,
    local_port: u16,
) -> anyhow::Result<()> {
    match provider {
        TunnelProvider::None => Ok(()),
        TunnelProvider::Cloudflare => cloudflare::run_once(remote, local_port).await,
        TunnelProvider::Tailscale => tailscale::run_once(remote, local_port).await,
    }
}
