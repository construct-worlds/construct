//! End-to-end: the split layout is shared daemon state (spec 0113).
//!
//! The wire-level rules (versioning, conflict rejection, pruning) are unit
//! tested in the daemon. What only an end-to-end test can show is the part
//! the design actually turns on:
//!
//!   * a split written by one client renders as panes in another;
//!   * a viewport too narrow to render panes shows a single session and
//!     never writes the layout back — the rule that stops a phone from
//!     collapsing a desktop's panes;
//!   * crossing the threshold is lossless in both directions.
//!
//! Skipped (not failed) when Chrome isn't installed, matching `web_smoke`.

use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use construct_e2e::Daemon;
use construct_protocol::{LayoutNode, LayoutSplitDirection};
use futures::StreamExt;

const WIDE: (u32, u32) = (1600, 900);
const NARROW: (u32, u32) = (600, 900);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_split_layout_renders_wide_and_is_read_only_narrow() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(construct_protocol::TunnelProvider::None, None)
        .await
        .expect("remote.start");

    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let a = d
        .client
        .create(shell_session_params(&cwd, "alpha"))
        .await
        .expect("create alpha");
    let b = d
        .client
        .create(shell_session_params(&cwd, "beta"))
        .await
        .expect("create beta");

    // Stand in for the TUI: write a two-pane layout straight to the daemon,
    // exactly as the other client would.
    let tree = LayoutNode::Split {
        direction: LayoutSplitDirection::Right,
        ratio_percent: 60,
        first: Box::new(LayoutNode::Leaf {
            id: 1,
            session_id: Some(a.clone()),
        }),
        second: Box::new(LayoutNode::Leaf {
            id: 2,
            session_id: Some(b.clone()),
        }),
    };
    let doc = d
        .client
        .set_layout(tree, Some(0))
        .await
        .expect("seed layout");
    assert_eq!(doc.version, 1);

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping split_layout: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    set_viewport(&page, WIDE).await;
    let url = inject_userinfo(&r.local_url, "remote", &r.password);
    page.goto(&url).await.expect("goto");
    wait_conn_open(&page).await;

    // --- wide: the shared tree renders as panes ---------------------------
    let panes = wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 2.0)
        .await
        .expect("two panes render on a wide viewport");
    assert_eq!(panes, 2.0, "the shared split must render as two panes");

    let dividers: f64 = eval_number(&page, "document.querySelectorAll('#paneGrid .pane-divider').length").await;
    assert_eq!(dividers, 1.0, "one draggable divider between the two panes");

    // The ratio from the shared tree drives the actual pane widths, which is
    // what makes a percentage-based tree meaningful across clients.
    let first_share: f64 = eval_number(
        &page,
        "(() => {
           const panes = document.querySelectorAll('#paneGrid .pane');
           const a = panes[0].getBoundingClientRect().width;
           const b = panes[1].getBoundingClientRect().width;
           return Math.round((a / (a + b)) * 100);
         })()",
    )
    .await;
    assert!(
        (first_share - 60.0).abs() <= 3.0,
        "pane widths should follow the shared 60% ratio, got {first_share}"
    );

    // Exactly one pane is focused, and it hosts the interactive stack. The
    // other is a read-only mirror.
    // Pane titles come from the session list, which arrives asynchronously —
    // a pane still showing a raw session id means the grid never refreshed
    // after the list loaded.
    let titled = wait_for_bool(
        &page,
        "(() => {
           const t = Array.from(document.querySelectorAll('#paneGrid .pane-title'))
             .map((e) => e.textContent.trim());
           return t.some((x) => x.startsWith('alpha')) && t.some((x) => x.startsWith('beta'));
         })()",
    )
    .await;
    assert!(
        titled,
        "panes must show session titles once the list loads, not raw ids"
    );

    // Every pane offers its own split/close controls, and nothing from the
    // interactive stack overlaps them.
    let head_buttons: f64 = eval_number(
        &page,
        "document.querySelectorAll('#paneGrid .pane.is-focused .pane-head button').length",
    )
    .await;
    assert_eq!(
        head_buttons, 3.0,
        "the focused pane keeps its split-right / split-below / close controls"
    );

    let focused: f64 = eval_number(&page, "document.querySelectorAll('#paneGrid .pane.is-focused').length").await;
    assert_eq!(focused, 1.0, "exactly one pane holds focus");
    let stack_in_focused: bool = eval_bool(
        &page,
        "!!document.querySelector('#paneGrid .pane.is-focused #viewStack')",
    )
    .await;
    assert!(
        stack_in_focused,
        "the interactive stack lives in the focused pane"
    );

    // Leave a reviewable artifact of the split actually rendering, the way
    // `web_smoke` leaves a video.
    save_screenshot(&page, "split_layout_wide.png").await;

    // --- narrow: one session, no panes, and NO write back -----------------
    let version_before = d.client.layout().await.expect("layout").version;

    set_viewport(&page, NARROW).await;
    let hidden = wait_for_bool(&page, "document.getElementById('paneGrid').hidden").await;
    assert!(hidden, "a narrow viewport must not render the pane grid");

    // Select the other session from the list the way a phone user would.
    // This must NOT touch the shared layout.
    page.evaluate(format!(
        "(() => {{ selectSession({:?}); return true; }})()",
        if focused_session(&page).await == a { &b } else { &a }
    ))
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(600)).await;

    let after = d.client.layout().await.expect("layout");
    assert_eq!(
        after.version, version_before,
        "a narrow client must never write the shared layout — \
         otherwise opening the page on a phone collapses every other client's panes"
    );
    assert_eq!(
        after.tree.session_ids().len(),
        2,
        "both panes still point at their sessions"
    );

    // --- back to wide: the tree is intact ---------------------------------
    set_viewport(&page, WIDE).await;
    let panes_again = wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 2.0)
        .await
        .expect("panes come back after returning to a wide viewport");
    assert_eq!(
        panes_again, 2.0,
        "crossing the threshold must be lossless: nothing was written on the way down"
    );

    // --- a remote layout change reaches the browser live ------------------
    let current = d.client.layout().await.expect("layout");
    let collapsed = LayoutNode::Leaf {
        id: 1,
        session_id: Some(a.clone()),
    };
    d.client
        .set_layout(collapsed, Some(current.version))
        .await
        .expect("collapse to a single pane");

    let gone = wait_for_bool_eventually(
        &page,
        "document.getElementById('paneGrid').hidden || document.querySelectorAll('#paneGrid .pane').length <= 1",
    )
    .await;
    assert!(
        gone,
        "a layout change made by another client must reach the browser over the broadcast"
    );
}

// --- helpers ---------------------------------------------------------------

async fn save_screenshot(page: &Page, name: &str) {
    use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, CaptureScreenshotParams};
    let Ok(dir) = construct_e2e::artifact_dir() else {
        return;
    };
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .build();
    if let Ok(bytes) = page.screenshot(params).await {
        let path = dir.join(name);
        if std::fs::write(&path, bytes).is_ok() {
            eprintln!("split_layout artifact: {}", path.display());
        }
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

async fn set_viewport(page: &Page, (w, h): (u32, u32)) {
    use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
    let _ = page
        .execute(
            SetDeviceMetricsOverrideParams::builder()
                .width(w as i64)
                .height(h as i64)
                .device_scale_factor(1.0)
                .mobile(false)
                .build()
                .expect("device metrics"),
        )
        .await;
    // The client re-renders panes on `resize`; CDP metric overrides don't
    // always fire one on their own.
    let _ = page
        .evaluate("window.dispatchEvent(new Event('resize')); true")
        .await;
    tokio::time::sleep(Duration::from_millis(350)).await;
}

async fn eval_number(page: &Page, js: &str) -> f64 {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<f64>().ok())
        .unwrap_or(f64::NAN)
}

async fn eval_bool(page: &Page, js: &str) -> bool {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<bool>().ok())
        .unwrap_or(false)
}

async fn focused_session(page: &Page) -> String {
    page.evaluate("state.currentId || ''")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default()
}

async fn wait_for_number(page: &Page, js: &str, want: f64) -> Option<f64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = eval_number(page, js).await;
        if v == want {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn wait_for_bool(page: &Page, js: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if eval_bool(page, js).await {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn wait_for_bool_eventually(page: &Page, js: &str) -> bool {
    wait_for_bool(page, js).await
}

async fn wait_conn_open(page: &Page) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state: String = page
            .evaluate("document.getElementById('conn')?.dataset?.state || ''")
            .await
            .ok()
            .and_then(|r| r.into_value::<String>().ok())
            .unwrap_or_default();
        if state == "open" {
            return;
        }
        assert!(Instant::now() <= deadline, "web client never connected");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{user}:{pw}@{rest}"),
        None => url.to_string(),
    }
}

fn shell_session_params(cwd: &str, title: &str) -> construct_protocol::CreateSessionParams {
    construct_protocol::CreateSessionParams {
        harness: "shell".to_string(),
        cwd: cwd.to_string(),
        prompt: None,
        model: None,
        title: Some(title.to_string()),
        mode: None,
        pty_size: None,
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
