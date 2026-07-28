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

    // --- C-x o cycles focus across panes AND the list ---------------------
    // The TUI's other-window cycle is list → pane 1 → … → pane N → list, so a
    // full lap must visit both kinds of region. Asserting only "focus moved to
    // the other pane" is what let the list fall out of the cycle unnoticed.
    let focused_before: f64 = eval_number(&page, "state.focusedPaneId").await;
    let mut saw_other_pane = false;
    let mut saw_list = false;
    for _ in 0..4 {
        chord(&page, "o").await;
        if eval_bool(
            &page,
            "document.getElementById('sessionList').contains(document.activeElement)",
        )
        .await
        {
            saw_list = true;
        } else if eval_number(&page, "state.focusedPaneId").await != focused_before {
            saw_other_pane = true;
        }
        let panes: f64 =
            eval_number(&page, "document.querySelectorAll('#paneGrid .pane').length").await;
        assert_eq!(panes, 2.0, "moving focus must not add or remove panes");
    }
    assert!(saw_other_pane, "C-x o should reach the other pane");
    assert!(
        saw_list,
        "C-x o should also reach the session list — the TUI cycles list plus \
         every visible window, and a list left out of the cycle is unreachable"
    );
    // Land back on a pane so the zoom assertions below have one focused.
    for _ in 0..4 {
        if !eval_bool(
            &page,
            "document.getElementById('sessionList').contains(document.activeElement)",
        )
        .await
        {
            break;
        }
        chord(&page, "o").await;
    }
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

/// The four ways the first keymap could be entered but not left, or reached a
/// surface the TUI reaches and the browser did not. Each of these was a real
/// report, and each is the kind of failure a table-shaped test misses: the
/// binding fires, so a "is it bound?" assertion passes — what's wrong is where
/// it leaves the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_keymap_escapes_dialogs_toggles_program_and_drives_the_list() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(construct_protocol::TunnelProvider::None, None)
        .await
        .expect("remote.start");

    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    for title in ["alpha", "beta", "gamma"] {
        // INTERACTIVE, not the default headless: only a non-headless PTY
        // session renders in terminal view, and terminal view is the one that
        // grabs the caret on a switch. A headless session opens in chat and
        // never exercises the focus path this test exists to pin down.
        let mut params = shell_session_params(&cwd, title);
        params.mode = Some("interactive".to_string());
        d.client.create(params).await.expect("create session");
    }

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping web_keymap dialogs test: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    set_viewport(&page, WIDE).await;
    let url = inject_userinfo(&r.local_url, "remote", &r.password);
    page.goto(&url).await.expect("goto");
    wait_conn_open(&page).await;
    assert!(wait_for_bool(&page, "!!state.currentId").await, "a session should be selected");

    // --- a dialog opened by a chord can be closed by the keyboard ---------
    // `C-x C-f` opens the new-session sheet. Before this, nothing dismissed
    // it: the keymap could open a modal it had no way out of.
    press(&page, "x", true).await;
    press(&page, "f", true).await;
    assert!(
        wait_for_bool(&page, "!document.getElementById('newSessionSheet').hidden").await,
        "C-x C-f should open the new-session sheet"
    );
    press(&page, "Escape", false).await;
    assert!(
        wait_for_bool(&page, "document.getElementById('newSessionSheet').hidden").await,
        "Escape must close the new-session sheet — a chord that opens a modal \
         must have a keyboard way out"
    );

    // The same must hold for every other dismissible sheet.
    let _ = page.evaluate("openSettingsSheet(); true").await;
    assert!(
        wait_for_bool(&page, "!document.getElementById('settingsSheet').hidden").await,
        "settings sheet should open"
    );
    press(&page, "Escape", false).await;
    assert!(
        wait_for_bool(&page, "document.getElementById('settingsSheet').hidden").await,
        "Escape must close the settings sheet too"
    );

    // --- C-x Space toggles Program, rather than only entering it ----------
    let before: String = page
        .evaluate("state.mode")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    chord(&page, " ").await;
    assert!(
        wait_for_bool(&page, "state.mode === 'program'").await,
        "C-x Space should open Program"
    );
    chord(&page, " ").await;
    let js = format!("state.mode === {before:?}");
    assert!(
        wait_for_bool(&page, js.as_str()).await,
        "C-x Space again must return to {before:?} — the TUI's OpenProgram toggles"
    );

    // --- C-x o reaches the session list, not just the other pane ----------
    chord(&page, "3").await;
    assert!(
        wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 2.0)
            .await
            .is_some(),
        "C-x 3 should split"
    );
    // Cycle until focus lands in the list; with 2 panes that is at most three
    // presses. Before the fix the list was simply never in the cycle.
    let mut reached_list = false;
    for _ in 0..3 {
        chord(&page, "o").await;
        if eval_bool(
            &page,
            "document.getElementById('sessionList').contains(document.activeElement)",
        )
        .await
        {
            reached_list = true;
            break;
        }
    }
    assert!(
        reached_list,
        "C-x o must include the session list in the focus cycle, as the TUI's \
         other-window does — otherwise the list is unreachable on a split"
    );

    // --- with the list focused, the TUI's bare keys drive it --------------
    // (Repeatability across a real view switch is covered on its own in
    // `web_keymap_list_navigation_keeps_the_caret_on_the_list`, which needs an
    // interactive session and no split to exercise the focus-stealing path.)
    let selected_before: String = page
        .evaluate("state.currentId")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    press(&page, "n", true).await;
    let moved = {
        let js = format!("state.currentId !== {selected_before:?}");
        wait_for_bool(&page, js.as_str()).await
    };
    assert!(
        moved,
        "C-n must move the list selection when the list has focus — the TUI \
         binds it bare and a focused row is just as modal"
    );
    // And back, so the pair is verified rather than just one direction.
    press(&page, "p", true).await;
    let js = format!("state.currentId === {selected_before:?}");
    assert!(
        wait_for_bool(&page, js.as_str()).await,
        "C-p must move the selection back the other way"
    );

    // Bare keys must stay scoped: typing in the composer is not list navigation.
    let _ = page
        .evaluate("document.getElementById('input').focus(); true")
        .await;
    let before_typing: String = page
        .evaluate("state.currentId")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    press(&page, "n", false).await;
    let unchanged: String = page
        .evaluate("state.currentId")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert_eq!(
        before_typing, unchanged,
        "a bare letter typed into the composer must never move the list selection"
    );
}

/// Navigating the list must not hand the caret to the session view.
///
/// Selecting a session enters that session's view, and a terminal view claims
/// the keyboard — so the first `C-n` worked and every one after it went to the
/// terminal instead of the list. The selection moved once and then navigation
/// was dead, which is invisible to any assertion that only checks "did the
/// selection change".
///
/// Three conditions all have to hold for the steal to be reachable, which is
/// why this is a separate test rather than more assertions on the shared one:
///   * the sessions must be INTERACTIVE — a headless PTY session opens in chat,
///     and chat never grabbed focus;
///   * the client must believe it is a desktop — headless Chrome reports touch
///     points, and the touch path deliberately skips the terminal auto-focus;
///   * no split may be open — the shared test leaves one, and the pane the
///     caret lands in changes which surface claims it.
///
/// Known limit, stated so nobody trusts this further than it goes: the steal
/// only manifests when this test is not competing with the rest of the suite.
/// Run alone (`--test web_keymap web_keymap_list_navigation`) it fails without
/// the fix, which is how the fix was verified. Run alongside the other keymap
/// tests, the switch finishes late enough that the caret is never observed to
/// move, and it passes either way. So treat a green here as evidence only when
/// it was run in isolation; the repeatability assertions below still hold in
/// both modes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_keymap_list_navigation_keeps_the_caret_on_the_list() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(construct_protocol::TunnelProvider::None, None)
        .await
        .expect("remote.start");

    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    for title in ["alpha", "beta", "gamma"] {
        let mut params = shell_session_params(&cwd, title);
        params.mode = Some("interactive".to_string());
        d.client.create(params).await.expect("create session");
    }

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping web_keymap list-focus test: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    set_viewport(&page, WIDE).await;
    let url = inject_userinfo(&r.local_url, "remote", &r.password);
    page.goto(&url).await.expect("goto");
    wait_conn_open(&page).await;
    assert!(wait_for_bool(&page, "!!state.currentId").await, "a session should be selected");
    let _ = page
        .evaluate("window.isLikelyTouchDevice = () => false; true")
        .await;

    // Assert the preconditions rather than assume them: without all three the
    // test would pass for the wrong reason, which is exactly how the first
    // version of this coverage slipped through.
    assert!(
        wait_for_bool(&page, "state.mode === 'terminal'").await,
        "sessions must open in terminal view for the caret-stealing path to exist"
    );
    assert!(
        eval_bool(&page, "shouldFocusTerminalAfterSessionSwitch()").await,
        "the switch must be trying to claim the caret, or this proves nothing"
    );

    let _ = page.evaluate("toggleListFocus(); true").await;
    assert!(
        wait_for_bool(
            &page,
            "document.getElementById('sessionList').contains(document.activeElement)",
        )
        .await,
        "the list should take focus"
    );

    let first: String = page
        .evaluate("state.currentId")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();

    press(&page, "n", true).await;
    let js = format!("state.currentId !== {first:?}");
    assert!(
        wait_for_bool(&page, js.as_str()).await,
        "C-n should move the selection"
    );

    // Wait for the switch to actually FINISH before judging focus. The caret is
    // stolen at the very end of entering the terminal view, after hydration, so
    // a fixed sleep is a false-green generator: under parallel test load the
    // steal simply lands after the sleep and the assertion sails through. Tie
    // the wait to the real completion signal instead.
    assert!(
        wait_for_bool(
            &page,
            "(() => { const h = state.terminalById.get(state.currentId);
                      return !!h && h.loaded === true; })()",
        )
        .await,
        "the incoming session's terminal should finish hydrating"
    );
    // Sample continuously rather than once. The steal lands at the tail of the
    // switch, and under parallel test load that can be any time within a couple
    // of seconds — a single sample at a fixed offset silently passes when the
    // steal happens just after it. Failing if focus EVER leaves the list makes
    // the check independent of when the switch happens to finish.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !eval_bool(
            &page,
            "document.getElementById('sessionList').contains(document.activeElement)",
        )
        .await
        {
            let active = text_of_active(&page).await;
            panic!(
                "after a list-driven switch the caret must stay on the list row, \
                 but it moved to `{active}` — the TUI's list keeps its cursor, and \
                 that is what makes C-n/C-p repeatable"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The consequence, stated directly: navigation keeps going.
    let second: String = page
        .evaluate("state.currentId")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    press(&page, "n", true).await;
    let js = format!("state.currentId !== {second:?}");
    assert!(
        wait_for_bool(&page, js.as_str()).await,
        "a second C-n must keep walking the list — the reported bug is that \
         navigation stopped after one step"
    );

    // And back up, both steps, still without touching the mouse.
    press(&page, "p", true).await;
    let js = format!("state.currentId === {second:?}");
    assert!(wait_for_bool(&page, js.as_str()).await, "C-p should walk back");
    press(&page, "p", true).await;
    let js = format!("state.currentId === {first:?}");
    assert!(
        wait_for_bool(&page, js.as_str()).await,
        "repeated C-p should return to where navigation started"
    );
}

/// A label for whatever currently holds focus, for failure messages.
async fn text_of_active(page: &Page) -> String {
    page.evaluate(
        "(() => { const a = document.activeElement;
                  return a ? (a.className || a.tagName) : 'none'; })()",
    )
    .await
    .ok()
    .and_then(|r| r.into_value::<String>().ok())
    .unwrap_or_default()
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
