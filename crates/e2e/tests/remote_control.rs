//! End-to-end: properties of `remote.start` / `remote.stop` that
//! the TUI and headless-browser smokes don't cover.
//!
//! Specifically:
//!
//!  - **Security**: wrong Basic credentials → 401 (i.e. the auth
//!    gate isn't accidentally an open door).
//!  - **Lifecycle**: `remote.stop` is idempotent — first call
//!    reports `was_running: true`, repeat calls report `false`
//!    instead of erroring.
//!  - **Persistence**: the `runtime/remote.json` snapshot file is
//!    written during start and deleted by stop. That snapshot is
//!    load-bearing for the `/agentd restart` URL-preservation
//!    adoption path — if either side regresses, restart silently
//!    rotates the URL.
//!  - **Teardown**: after `remote.stop`, the listener is actually
//!    gone (not just marked stopped) — subsequent HTTP requests
//!    can't connect.
//!
//! Happy-path coverage (HTTP 200 + HTML body + JS boot + WS
//! upgrade) lives in `web_smoke.rs`; this test stays cheap and
//! Chrome-free so it still runs on dev machines without a
//! browser and catches HTTP-layer regressions on every CI run.
//!
//! Uses `TunnelProvider::None` so the test never depends on a tunnel
//! binary being installed or reachable.

use std::time::Duration;

use construct_protocol::TunnelProvider;

use construct_e2e::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_control_security_and_lifecycle() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    let r = d
        .client
        .remote_start(TunnelProvider::None, /* password */ None)
        .await
        .expect("remote.start");
    // Starting without a provider must not publish anything: no tunnel,
    // and the reply says so. If this ever comes back `tunnel_ready`,
    // opening the dialog has started exposing the machine again.
    assert_eq!(r.provider, TunnelProvider::None);
    assert!(!r.tunnel_ready, "no provider means no tunnel");
    assert!(
        r.local_url.starts_with("http://127.0.0.1:"),
        "expected loopback URL, got {}",
        r.local_url
    );
    assert!(
        !r.password.is_empty(),
        "auto-gen password should be non-empty"
    );

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let root = r.local_url.clone();

    // Security gate: wrong password → 401. If this ever 200s, the
    // Basic-auth check is broken and the whole remote-control
    // model collapses.
    let bad = http
        .get(&root)
        .basic_auth("remote", Some("not-the-password"))
        .send()
        .await
        .expect("http get bad pw");
    assert_eq!(bad.status().as_u16(), 401, "wrong pw should be 401");

    // ...and the failed attempt must not have locked the door behind
    // it. The throttle that defends this password delays *failures*
    // only; if it ever starts penalising the legitimate user, an
    // attacker hammering the listener could keep them out.
    let good = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await
        .expect("http get good pw");
    assert_eq!(
        good.status().as_u16(),
        200,
        "correct password must work right after a failed attempt"
    );

    // Snapshot file is written under runtime_dir — that's what
    // the `/agentd restart` adoption path reads to rehydrate the
    // token + password + port.
    let snap = d.dir.path().join("run/remote.json");
    assert!(snap.exists(), "expected snapshot at {}", snap.display());
    let snap_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert_eq!(snap_json["password"].as_str().unwrap(), r.password);
    assert!(snap_json["port"].as_u64().unwrap() > 0);

    // Lifecycle: first stop reports `was_running: true`; second
    // stop is idempotent (`was_running: false`) instead of an
    // error.
    let stop1 = d.client.remote_stop().await.expect("remote.stop #1");
    assert!(stop1.was_running, "first stop should report was_running");
    let stop2 = d.client.remote_stop().await.expect("remote.stop #2");
    assert!(!stop2.was_running, "second stop should be idempotent");

    // Snapshot is deleted after stop so the next daemon boot
    // doesn't try to adopt a tunnel that no longer exists.
    assert!(
        !snap.exists(),
        "snapshot should be removed after remote.stop, still exists at {}",
        snap.display()
    );

    // Teardown: the listener is actually gone — request can't
    // connect, or comes back as a server error.
    let after = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await;
    assert!(
        after.is_err()
            || after
                .ok()
                .map(|r| r.status().is_server_error() || r.status().as_u16() == 502)
                .unwrap_or(false),
        "expected post-stop request to fail or 5xx"
    );
}

/// `remote.providers` reports every provider the daemon knows about,
/// whether or not it could run — the dialog needs the unavailable ones
/// too, so it can grey them out and say why.
///
/// Deliberately does not assert *availability*: whether cloudflared
/// happens to be installed is a property of the machine running the
/// test, not of the code. What must hold is that Cloudflare is
/// described, and that anything unavailable explains itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_providers_describes_every_provider() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    let r = d
        .client
        .remote_providers()
        .await
        .expect("remote.providers");

    let listed: Vec<TunnelProvider> = r.providers.iter().map(|p| p.provider).collect();
    assert!(
        listed.contains(&TunnelProvider::Cloudflare),
        "cloudflare must be offered even when it isn't installed: {listed:?}"
    );
    assert!(
        !listed.contains(&TunnelProvider::None),
        "`None` is the absence of a provider, not a button: {listed:?}"
    );

    for p in &r.providers {
        if p.available {
            assert!(
                p.detail.is_none(),
                "{:?} is available, so it has nothing to explain",
                p.provider
            );
        } else {
            let detail = p.detail.as_deref().unwrap_or("");
            assert!(
                !detail.is_empty(),
                "{:?} is unavailable and must say why — a dead button with no \
                 explanation is the bug this field exists to prevent",
                p.provider
            );
        }
    }
}

/// The listener binds every interface, not just loopback.
///
/// This is what makes the dialog's default state useful: a phone on the
/// same Wi-Fi reaches the daemon with no tunnel at all. A regression to
/// a loopback-only bind would leave that QR pointing the phone at
/// itself, and nothing else in the suite would notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_is_reachable_off_loopback() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    let r = d
        .client
        .remote_start(TunnelProvider::None, None)
        .await
        .expect("remote.start");

    // Derive the port from the loopback URL and dial it on a
    // non-loopback local address. We don't use `lan_url` here: CI hosts
    // often have no RFC1918 address at all, and this property — "the
    // socket is not bound to 127.0.0.1" — is testable without one.
    let port: u16 = r
        .local_url
        .rsplit(':')
        .next()
        .and_then(|s| s.trim_end_matches('/').parse().ok())
        .expect("port out of local_url");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // 0.0.0.0 as a destination means "this host" on every platform we
    // target, and it only connects if the listener bound the wildcard
    // address rather than the loopback one.
    let res = http
        .get(format!("http://0.0.0.0:{port}/"))
        .basic_auth("remote", Some(&r.password))
        .send()
        .await
        .expect("connect to wildcard-bound listener");
    assert_eq!(
        res.status().as_u16(),
        200,
        "listener must answer on a non-loopback address"
    );
}

/// The dialog's `stop` button stops the tunnel but keeps the LAN
/// listener. This test can't spawn a real tunnel (the e2e daemon runs
/// with tunnels disabled), but it pins the load-bearing half: a
/// tunnel-only stop must leave the listener answering with the *same*
/// password, so a phone connected over the LAN isn't kicked off when
/// the user drops the public URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tunnel_only_stop_keeps_lan_listener() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    let r = d
        .client
        .remote_start(TunnelProvider::None, None)
        .await
        .expect("remote.start");
    let root = r.local_url.clone();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Tunnel-only stop with no tunnel running is a no-op that reports
    // nothing was torn down — and crucially does not take the listener
    // with it.
    let stop = d
        .client
        .remote_stop_with(/* tunnel_only */ true)
        .await
        .expect("remote.stop tunnel-only");
    assert!(!stop.was_running, "no tunnel was running to stop");

    let after = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await
        .expect("listener still reachable after tunnel-only stop");
    assert_eq!(
        after.status().as_u16(),
        200,
        "the LAN listener + password must survive a tunnel-only stop"
    );

    // A full stop, by contrast, takes everything down.
    let full = d.client.remote_stop().await.expect("remote.stop full");
    assert!(full.was_running, "full stop should tear the listener down");
    let gone = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await;
    assert!(
        gone.is_err() || gone.map(|r| !r.status().is_success()).unwrap_or(true),
        "listener must be gone after a full stop"
    );
}
