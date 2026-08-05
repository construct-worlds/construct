//! Mobile-layout regression (menu toggle vs terminal scroll): opening and
//! closing the session-list menu reshapes the terminal host, and the browser
//! clamps the xterm viewport's scrollTop while the grid catches up. Those
//! synthetic scroll events must not be read as "the user scrolled to the
//! top": they used to trip the lazy history loader, which visibly replayed
//! the raw PTY backlog over a live terminal and parked the scroll position
//! at the top of the scrollback.
//!
//! The session shape matters: a TUI-agent session accumulates megabytes of
//! pty.log while its *rendered* scrollback stays a few dozen rows (in-place
//! repaints), so "at the bottom" is also within the loader's near-top
//! threshold. The awk spinner below reproduces that shape.

use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetTouchEmulationEnabledParams,
};
use chromiumoxide::page::Page;
use construct_e2e::Daemon;
use construct_protocol::{CreateSessionParams, PtySize, TunnelProvider};
use futures::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mobile_list_toggle_does_not_replay_terminal_history() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(TunnelProvider::None, None)
        .await
        .expect("remote.start");
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let id = d
        .client
        .create(shell_session_params(&cwd, "mobile scroll"))
        .await
        .expect("create shell session");

    // Megabytes of pty.log (so older raw history exists past the screen
    // snapshot's render span) but only ~60 rendered scrollback rows.
    d.client
        .pty_input(
            &id,
            b"awk 'BEGIN{for(i=0;i<60000;i++) printf \"\\rspinner %d aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\", i}'; echo; seq 1 60; echo ALLDONE\r".to_vec(),
        )
        .await
        .expect("pty_input awk");
    wait_for_pty_output(&d, &id, "ALLDONE").await;

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping web_mobile_list_toggle: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.expect("new page");
    set_mobile_viewport(&page, 420, 900).await;
    page.goto(inject_userinfo(&r.local_url, "remote", &r.password))
        .await
        .expect("goto WebUI");
    wait_conn_open(&page).await;
    wait_for_bool(
        &page,
        &format!("state.sessions.some((s) => s.id === {id:?})"),
    )
    .await;

    // Select the session the way a phone user does (this also auto-collapses
    // the list on narrow layouts) and wait for terminal hydration.
    page.evaluate(format!("selectSession({id:?}); true"))
        .await
        .expect("select session");
    wait_for_bool(
        &page,
        &format!(
            "(() => {{ const h = terminalHandleForSession({id:?}); \
             return state.currentId === {id:?} && state.mode === 'terminal' && !!h?.loaded; }})()"
        ),
    )
    .await;
    // Let hydration's geometry claim / resize echoes settle.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Record any history-replay RPC issued after this point.
    page.evaluate(
        r#"
        (() => {
          window.__replayCalls = [];
          const realRpc = rpc;
          rpc = (method, params) => {
            if (method === "session.pty_replay" || method === "session.transcript") {
              window.__replayCalls.push({ method, params: { ...params } });
            }
            return realRpc(method, params);
          };
          return true;
        })()
        "#,
    )
    .await
    .expect("install rpc trace");

    let before = scroll_probe(&page).await;
    assert!(
        before.base_y > 0,
        "test needs rendered scrollback to be meaningful: {before:?}"
    );
    assert!(
        before.viewport_y >= before.base_y,
        "terminal should start at the bottom: {before:?}"
    );

    // Open the session list, let layout + refit + PTY echo settle, close it
    // again, and let everything settle once more.
    toggle_session_list(&page).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    toggle_session_list(&page).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let after = scroll_probe(&page).await;
    save_screenshot(&page, "web_mobile_list_toggle_after.png").await;
    let replay_calls = page
        .evaluate("JSON.stringify(window.__replayCalls)")
        .await
        .ok()
        .and_then(|v| v.into_value::<String>().ok())
        .unwrap_or_default();
    assert_eq!(
        replay_calls, "[]",
        "toggling the session list must not reload terminal history \
         (before={before:?} after={after:?})"
    );
    assert!(
        after.viewport_y >= after.base_y,
        "opening+closing the session list moved the terminal scroll position: \
         before={before:?} after={after:?}"
    );

    drop(page);
    drop(browser);
}

#[derive(Debug, Clone, Copy)]
struct ScrollProbe {
    viewport_y: i64,
    base_y: i64,
}

async fn scroll_probe(page: &Page) -> ScrollProbe {
    let v = page
        .evaluate(
            r#"
            (() => {
              const active = state.term.buffer.active;
              return { viewportY: active.viewportY, baseY: active.baseY };
            })()
            "#,
        )
        .await
        .expect("scroll probe eval")
        .into_value::<serde_json::Value>()
        .expect("scroll probe value");
    ScrollProbe {
        viewport_y: v["viewportY"].as_i64().expect("viewportY"),
        base_y: v["baseY"].as_i64().expect("baseY"),
    }
}

async fn save_screenshot(page: &Page, name: &str) {
    use chromiumoxide::cdp::browser_protocol::page::{
        CaptureScreenshotFormat, CaptureScreenshotParams,
    };
    let Ok(dir) = construct_e2e::artifact_dir() else {
        return;
    };
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .build();
    if let Ok(bytes) = page.screenshot(params).await {
        let path = dir.join(name);
        if std::fs::write(&path, bytes).is_ok() {
            eprintln!("web_mobile_list_toggle artifact: {}", path.display());
        }
    }
}

async fn toggle_session_list(page: &Page) {
    page.evaluate("document.getElementById('toggleList').click(); true")
        .await
        .expect("toggle session list");
}

async fn wait_for_pty_output(d: &Daemon, id: &str, needle: &str) {
    use base64::Engine as _;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let replay = d.client.pty_replay(id).await.expect("pty_replay");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(replay.data.as_bytes())
            .unwrap_or_default();
        if String::from_utf8_lossy(&bytes).contains(needle) {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "shell output never contained {needle:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn launch_browser() -> Option<(Browser, chromiumoxide::Handler)> {
    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .build()
        .ok()?;
    Browser::launch(config).await.ok()
}

async fn set_mobile_viewport(page: &Page, width: u32, height: u32) {
    let _ = page
        .execute(
            SetDeviceMetricsOverrideParams::builder()
                .width(width as i64)
                .height(height as i64)
                .device_scale_factor(2.0)
                .mobile(true)
                .build()
                .expect("device metrics"),
        )
        .await;
    // Report a touch digitizer so the WebUI shows its mobile terminal
    // controls (composer + virtual keyboard), like a real phone.
    let _ = page
        .execute(
            SetTouchEmulationEnabledParams::builder()
                .enabled(true)
                .max_touch_points(5)
                .build()
                .expect("touch emulation"),
        )
        .await;
    let _ = page
        .evaluate("window.dispatchEvent(new Event('resize')); true")
        .await;
}

async fn wait_conn_open(page: &Page) {
    wait_for_bool(
        page,
        "document.getElementById('conn')?.dataset?.state === 'open'",
    )
    .await;
}

async fn wait_for_bool(page: &Page, js: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = page
            .evaluate(js)
            .await
            .ok()
            .and_then(|v| v.into_value::<bool>().ok())
            .unwrap_or(false);
        if ready {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "condition never became true: {js}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn inject_userinfo(url: &str, user: &str, password: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{user}:{password}@{rest}"),
        None => url.to_string(),
    }
}

fn shell_session_params(cwd: &str, title: &str) -> CreateSessionParams {
    CreateSessionParams {
        harness: "shell".to_string(),
        cwd: cwd.to_string(),
        prompt: None,
        model: None,
        title: Some(title.to_string()),
        mode: None,
        pty_size: Some(PtySize {
            cols: 100,
            rows: 30,
        }),
        worktree: false,
        env: std::collections::HashMap::new(),
        args: Vec::new(),
        kind: Default::default(),
        parent_session_id: None,
        group_id: None,
        position_after_session_id: None,
        forked_from: None,
    }
}
