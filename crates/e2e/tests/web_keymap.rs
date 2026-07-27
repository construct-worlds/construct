//! End-to-end: the web UI answers the TUI's `C-x` chords (spec 0150).
//!
//! The chord machine itself is simple enough that its interest is entirely in
//! the wiring — whether a chord actually reaches the handler in a browser,
//! and whether it does the thing the TUI does. So this drives real key events
//! through the page and asserts on what the UI became:
//!
//!   * a lone prefix arms and is echoed, instead of vanishing;
//!   * `C-x 3` / `C-x 2` split, and the split is the shared layout;
//!   * `C-x o` moves focus between panes without moving the panes;
//!   * `C-x z` zooms this client only, leaving the shared tree alone;
//!   * `C-x 0` closes a pane; `C-x` + an unbound key says so;
//!   * a viewport too narrow to render panes refuses to split.
//!
//! Skipped (not failed) when Chrome isn't installed, matching `web_smoke`.

use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use construct_e2e::Daemon;
use futures::StreamExt;

const WIDE: (u32, u32) = (1600, 900);
const NARROW: (u32, u32) = (700, 900);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_ui_answers_the_tui_chord_keymap() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(construct_protocol::TunnelProvider::None, None)
        .await
        .expect("remote.start");

    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let _a = d
        .client
        .create(shell_session_params(&cwd, "alpha"))
        .await
        .expect("create alpha");

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping web_keymap: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    set_viewport(&page, WIDE).await;
    let url = inject_userinfo(&r.local_url, "remote", &r.password);
    page.goto(&url).await.expect("goto");
    wait_conn_open(&page).await;
    // A session has to be selected before session-scoped chords mean anything.
    assert!(
        wait_for_bool(&page, "!!state.currentId").await,
        "a session should be selected once the list loads"
    );

    // --- a lone prefix arms and is visible --------------------------------
    press(&page, "x", true).await;
    assert!(
        wait_for_bool(&page, "!document.getElementById('chordEcho').hidden").await,
        "a pending C-x must be echoed — an invisible prefix reads as a dead keymap"
    );
    let echo = text_of(&page, "#chordEcho").await;
    assert!(
        echo.starts_with("C-x"),
        "the echo should name the pending chord, got {echo:?}"
    );
    save_screenshot(&page, "web_keymap_chord_pending.png").await;

    // Escape cancels it, as in emacs and the TUI.
    press(&page, "Escape", false).await;
    assert!(
        wait_for_bool(&page, "document.getElementById('chordEcho').hidden").await,
        "Escape must cancel a half-typed chord"
    );

    // --- C-x 3 splits, and the split is shared ----------------------------
    chord(&page, "3").await;
    let panes = wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 2.0)
        .await
        .expect("C-x 3 should split the view into two panes");
    assert_eq!(panes, 2.0);

    // The split must be the *shared* layout, not a local-only rendering —
    // otherwise the chord diverges from what the TUI's C-x 3 does.
    let shared = d.client.layout().await.expect("layout");
    assert!(
        matches!(shared.tree, construct_protocol::LayoutNode::Split { .. }),
        "C-x 3 must write the shared layout, got {:?}",
        shared.tree
    );

    // --- C-x o cycles focus without disturbing the panes ------------------
    let focused_before: f64 = eval_number(&page, "state.focusedPaneId").await;
    chord(&page, "o").await;
    let focused_after = wait_for_changed_number(&page, "state.focusedPaneId", focused_before).await;
    assert!(
        focused_after.is_some(),
        "C-x o should move focus to the other pane"
    );
    let still_two: f64 = eval_number(&page, "document.querySelectorAll('#paneGrid .pane').length").await;
    assert_eq!(still_two, 2.0, "moving focus must not add or remove panes");
    save_screenshot(&page, "web_keymap_split_via_chord.png").await;

    // --- C-x z zooms this client only -------------------------------------
    let version_before = d.client.layout().await.expect("layout").version;
    chord(&page, "z").await;
    assert!(
        wait_for_bool(
            &page,
            "document.getElementById('paneGrid').classList.contains('is-zoomed')"
        )
        .await,
        "C-x z should zoom the focused pane"
    );
    let visible: f64 = eval_number(
        &page,
        "Array.from(document.querySelectorAll('#paneGrid .pane'))
           .filter((p) => p.getBoundingClientRect().width > 0).length",
    )
    .await;
    assert_eq!(visible, 1.0, "zoom should leave exactly one pane on screen");
    // Zoom is per-client (spec 0118): it must not touch the shared tree.
    let version_after = d.client.layout().await.expect("layout").version;
    assert_eq!(
        version_before, version_after,
        "zoom is per-client and must not write the shared layout"
    );
    chord(&page, "z").await;
    assert!(
        wait_for_bool(
            &page,
            "!document.getElementById('paneGrid').classList.contains('is-zoomed')"
        )
        .await,
        "C-x z should toggle back off"
    );

    // --- an unbound key after the prefix is consumed and reported ---------
    chord(&page, "q").await;
    let notice = wait_for_text(&page, "#chordEcho", "not bound").await;
    assert!(
        notice,
        "C-x followed by an unbound key should say so rather than doing nothing"
    );

    // --- C-x 0 closes the focused pane ------------------------------------
    chord(&page, "0").await;
    let back_to_one =
        wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 0.0).await;
    assert!(
        back_to_one.is_some(),
        "C-x 0 should close the pane and leave the single-session view"
    );

    // --- narrow viewports refuse to split ---------------------------------
    // The clamp-on-render rule (spec 0118): a client that can't render panes
    // must not write them for the clients that can.
    set_viewport(&page, NARROW).await;
    let seed = d.client.layout().await.expect("layout").version;
    chord(&page, "3").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after_narrow = d.client.layout().await.expect("layout").version;
    assert_eq!(
        seed, after_narrow,
        "a narrow client must not write a split into the shared layout"
    );
    let told: bool = eval_bool(
        &page,
        "(document.getElementById('chordEcho').textContent || '').includes('too narrow')",
    )
    .await;
    assert!(told, "the refusal should be explained, not silent");
}

/// Send `C-x` then `key` — one complete chord.
async fn chord(page: &Page, key: &str) {
    press(page, "x", true).await;
    press(page, key, false).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// Dispatch a keydown the way a real one arrives: on the focused element, so
/// it propagates up through the document capture listener the keymap uses.
async fn press(page: &Page, key: &str, ctrl: bool) {
    let js = format!(
        "(() => {{
           const target = document.activeElement || document.body;
           target.dispatchEvent(new KeyboardEvent('keydown', {{
             key: {key:?}, ctrlKey: {ctrl}, bubbles: true, cancelable: true,
           }}));
           return true;
         }})()",
        key = key,
        ctrl = ctrl,
    );
    let _ = page.evaluate(js.as_str()).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
}

async fn text_of(page: &Page, sel: &str) -> String {
    let js = format!("(document.querySelector({sel:?})?.textContent || '').trim()");
    page.evaluate(js.as_str())
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default()
}

async fn wait_for_text(page: &Page, sel: &str, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if text_of(page, sel).await.contains(needle) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

async fn wait_for_changed_number(page: &Page, js: &str, from: f64) -> Option<f64> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let v = eval_number(page, js).await;
        if v != from && !v.is_nan() {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
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
            eprintln!("web_keymap artifact: {}", path.display());
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
