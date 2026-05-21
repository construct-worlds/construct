//! End-to-end: drive the bundled web client in a real headless
//! Chromium via the Chrome DevTools Protocol. Catches the kind
//! of regressions that wire-level tests miss — JS boot, the
//! HTTP-vs-WS demux on the same port, xterm.js init, the
//! `setConnState("open", ...)` path that fires after the WS
//! upgrade succeeds.
//!
//! Skipped (not failed) when Chrome / Chromium isn't installed
//! on the host, so dev machines without a browser don't see
//! spurious failures. GitHub-hosted `ubuntu-latest` runners
//! ship Google Chrome pre-installed, so this runs in CI by
//! default.

use std::time::{Duration, Instant};

use agentd_e2e::Daemon;
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_client_loads_and_websocket_connects() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(/* local_only */ true, /* password */ None)
        .await
        .expect("remote.start");

    // Headless Chrome with the conservative flag set Linux CI
    // expects. `--no-sandbox` is required because GitHub runners
    // run as root inside a container; `--disable-gpu` avoids
    // shader-compile failures on headless servers without a GPU.
    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .build()
        .expect("browser config");
    let launch = Browser::launch(config).await;
    let (browser, mut handler) = match launch {
        Ok(pair) => pair,
        Err(e) => {
            // No Chrome on this host — emit a hint and pass.
            // We can't easily `#[ignore]` conditionally, so this
            // is the next best thing for dev machines.
            eprintln!(
                "skipping web_smoke: could not launch Chromium ({e}). \
                 Install Google Chrome to run this test locally."
            );
            return;
        }
    };
    let _handler_task = tokio::spawn(async move {
        while handler.next().await.is_some() {}
    });

    let page = browser.new_page("about:blank").await.expect("new page");

    // Embed Basic credentials directly in the URL. Chrome still
    // sends the resulting `Authorization` header for the initial
    // navigation (it only hides the userinfo in the address bar
    // for spoofing reasons) and caches them in its per-origin
    // HTTP auth credentials store. The subsequent WebSocket
    // upgrade — which can't take its own header from CDP because
    // the browser's WS API doesn't expose request headers —
    // picks the cached creds up automatically. Modern CDP
    // `Fetch`-domain interception (`Page::authenticate`) is the
    // documented alternative but is unreliable on the first
    // navigation in headless mode (see chromiumoxide#issues).
    let url_with_creds = inject_userinfo(&r.url, "remote", &r.password);
    page.goto(&url_with_creds).await.expect("goto");

    // The web client's JS sets `#conn`'s `data-state` to `"open"`
    // after the WebSocket upgrade succeeds. Polling that
    // attribute is a direct signal that the whole stack
    // (HTTP+WS demux, token gating, Basic auth, ws.onopen) is
    // working.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state: String = page
            .evaluate("document.getElementById('conn')?.dataset?.state || ''")
            .await
            .and_then(|r| r.into_value::<String>().map_err(Into::into))
            .unwrap_or_default();
        if state == "open" {
            break;
        }
        if Instant::now() > deadline {
            // Pull the body text to surface what the page is
            // showing — usually an error from the JS console or
            // an empty body if the JS never ran.
            let body: String = page
                .evaluate("document.body?.innerText || ''")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_else(|| "(no body)".into());
            panic!(
                "web client never reached conn state='open' (last={state:?}).\n\
                 --- page body ---\n{body}\n-----------------"
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // Sanity: the static HTML / bundled JS rendered. The empty
    // session-list label is visible in the layout regardless of
    // whether any sessions exist on the daemon.
    let body: String = page
        .evaluate("document.body.innerText || ''")
        .await
        .expect("body innerText")
        .into_value::<String>()
        .expect("string");
    assert!(
        body.contains("sessions") || body.contains("session"),
        "expected 'session(s)' in rendered body, got:\n{body}"
    );

    // Page-level sanity: the bundled xterm.js was loaded (i.e.
    // the embedded `/t/<token>/static/xterm.js` request
    // succeeded). The web client puts `Terminal` on `window` as
    // a side effect of importing the script.
    let xterm_present: bool = page
        .evaluate("typeof window.Terminal === 'function'")
        .await
        .expect("evaluate xterm")
        .into_value::<bool>()
        .expect("bool");
    assert!(
        xterm_present,
        "bundled xterm.js never loaded (window.Terminal !== 'function')"
    );
}

/// Inject `user:password@` userinfo into the authority of an
/// `http://` URL. Doesn't touch the path or fragment. Cheap
/// hand-rolled splitter (avoids pulling in a URL crate just for
/// one test).
fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{user}:{pw}@{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{user}:{pw}@{rest}")
    } else {
        url.to_string()
    }
}
