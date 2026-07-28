//! End-to-end PTY geometry ownership across a real WebUI connection and a
//! separate native connection standing in for the TUI.
//!
//! This intentionally uses CDP mouse events against the bundled xterm rather
//! than calling the WebUI handlers directly. Claude enables DECSET any-event
//! mouse tracking, so plain pointer entry produces PTY input even though it
//! must never become a geometry claim.

use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
};
use chromiumoxide::page::Page;
use construct_e2e::Daemon;
use construct_protocol::{CreateSessionParams, PtySize, TunnelProvider};
use futures::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_mouse_hover_does_not_reclaim_pty_after_tui_handoff() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(TunnelProvider::None, None)
        .await
        .expect("remote.start");
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let id = d
        .client
        .create(shell_session_params(&cwd, "hover ownership"))
        .await
        .expect("create shell session");

    let Some((browser, mut handler)) = launch_browser().await else {
        eprintln!("skipping web_pty_ownership: could not launch Chromium");
        return;
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.expect("new page");
    set_viewport(&page, 1600, 900).await;
    page.goto(inject_userinfo(&r.local_url, "remote", &r.password))
        .await
        .expect("goto WebUI");
    wait_conn_open(&page).await;
    wait_for_bool(
        &page,
        &format!("state.sessions.some((s) => s.id === {id:?})"),
    )
    .await;

    page.evaluate(format!(
        r#"
        (async () => {{
          await selectSession({id:?});
          if (state.currentId === {id:?} && state.mode !== "terminal") {{
            await enterTerminalMode({id:?}, {{ focusTerminal: true }});
          }}
          return true;
        }})()
        "#
    ))
    .await
    .expect("select terminal session");
    wait_for_bool(
        &page,
        &format!(
            "(() => {{ const h = terminalHandleForSession({id:?}); \
             return state.currentId === {id:?} && state.mode === 'terminal' && !!h?.loaded; }})()"
        ),
    )
    .await;

    // Keep an ordered trace of every PTY RPC and personalized resize event
    // surrounding the handoff. It is included in assertion failures so a
    // regression shows exactly which browser action reclaimed ownership.
    page.evaluate(
        r#"
        (() => {
          window.__ptyOwnershipTrace = [];
          const realRpc = rpc;
          rpc = (method, params) => {
            if (method === "session.pty_input" || method === "session.pty_resize") {
              window.__ptyOwnershipTrace.push({
                kind: "rpc",
                at: performance.now(),
                method,
                params: { ...params },
              });
            }
            return realRpc(method, params);
          };
          const realHandleNotification = handleNotification;
          handleNotification = (method, params) => {
            if (method === "session/event" && params?.event?.type === "pty_resize") {
              window.__ptyOwnershipTrace.push({
                kind: "event",
                at: performance.now(),
                session_id: params.session_id,
                event: { ...params.event },
              });
            }
            return realHandleNotification(method, params);
          };
          const h = terminalHandleForSession(state.currentId);
          for (const type of [
            "pointerover", "pointerenter", "pointermove",
            "mouseover", "mouseenter", "mousemove",
          ]) {
            h.host.addEventListener(type, (event) => {
              window.__ptyOwnershipTrace.push({
                kind: "dom",
                at: performance.now(),
                type,
                buttons: event.buttons,
              });
            }, { capture: true });
          }
          h.term._core.coreService.onUserInput(() => {
            window.__ptyOwnershipTrace.push({
              kind: "xterm-user-input",
              at: performance.now(),
            });
          });
          return true;
        })()
        "#,
    )
    .await
    .expect("install PTY trace");

    let point = terminal_center(&page).await;
    click_at(&page, point.0, point.1).await;
    wait_for_bool(&page, &format!("state.ptyOwnedSessionIds.has({id:?})")).await;
    // Session hydration may finish with a one-column repaint bump and passive
    // owner refits. Let those finish so any later claim is caused by the hover,
    // not by work still queued from the deliberate browser click.
    tokio::time::sleep(Duration::from_secs(1)).await;
    wait_for_bool(
        &page,
        "state.pendingHydrationBump === null && \
         state.pendingGeometryClaim === null && \
         state.ptyResizeTimer === null && \
         state.pendingPtyResize === null",
    )
    .await;
    let browser_size = current_pty_size(&d, &id).await;
    assert_ne!(
        browser_size,
        PtySize { cols: 73, rows: 21 },
        "browser and TUI test geometries must differ"
    );

    // The native connection now takes ownership at an unmistakably different
    // geometry, exactly as a TUI click/focus resize does.
    let tui_size = PtySize { cols: 73, rows: 21 };
    d.client
        .pty_resize(&id, tui_size.cols, tui_size.rows)
        .await
        .expect("TUI claims geometry");
    wait_for_pty_size(&d, &id, tui_size).await;
    wait_for_bool(&page, &format!("!state.ptyOwnedSessionIds.has({id:?})")).await;

    // Let all work caused by the two deliberate clicks settle. A later
    // takeover must therefore be attributable to pointer entry itself.
    tokio::time::sleep(Duration::from_millis(500)).await;
    page.evaluate(
        r#"
        (async () => {
          window.__ptyOwnershipTrace.length = 0;
          const h = terminalHandleForSession(state.currentId);
          await new Promise((resolve) => h.term.write(
            "\x1b[?1003h\x1b[?1006h",
            resolve,
          ));
          return true;
        })()
        "#,
    )
    .await
    .expect("enable Claude-style any-event mouse tracking");

    // Move from outside the terminal into its screen. CDP dispatches the same
    // browser event sequence as a real desktop pointer crossing the window.
    move_mouse(&page, 2.0, 2.0).await;
    move_mouse(&page, point.0, point.1).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    let actual = current_pty_size(&d, &id).await;
    let trace = ownership_trace(&page).await;
    assert_eq!(
        actual, tui_size,
        "plain WebUI hover reclaimed or resized the PTY after the TUI handoff; trace={trace}"
    );
    assert!(
        !trace.contains(r#""claim":true"#),
        "plain WebUI hover emitted a claiming PTY RPC; trace={trace}"
    );
    assert!(
        trace.contains(r#""method":"session.pty_input""#) && trace.contains(r#""claim":false"#),
        "the test must exercise a real passive Claude mouse report; trace={trace}"
    );

    let classification = page
        .evaluate(
            r#"
            (() => ({
              hover: isPlainSgrPointerHoverReport("\x1b[<35;5;4M"),
              modifiedHover: isPlainSgrPointerHoverReport("\x1b[<39;5;4M"),
              leftDrag: isPlainSgrPointerHoverReport("\x1b[<32;5;4M"),
              rightDrag: isPlainSgrPointerHoverReport("\x1b[<34;5;4M"),
              click: isPlainSgrPointerHoverReport("\x1b[<0;5;4M"),
              release: isPlainSgrPointerHoverReport("\x1b[<0;5;4m"),
              keyboard: isPlainSgrPointerHoverReport("x"),
            }))()
            "#,
        )
        .await
        .expect("classify SGR mouse reports")
        .into_value::<serde_json::Value>()
        .expect("classification result");
    assert_eq!(
        classification,
        serde_json::json!({
            "hover": true,
            "modifiedHover": true,
            "leftDrag": false,
            "rightDrag": false,
            "click": false,
            "release": false,
            "keyboard": false,
        }),
        "only no-button SGR motion may stay passive"
    );
}

async fn current_pty_size(d: &Daemon, id: &str) -> PtySize {
    d.client
        .pty_replay(id)
        .await
        .expect("pty replay")
        .size
        .expect("PTY has a size")
}

async fn wait_for_pty_size(d: &Daemon, id: &str, expected: PtySize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if current_pty_size(d, id).await == expected {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "PTY never reached {expected:?}; current={:?}",
            current_pty_size(d, id).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn ownership_trace(page: &Page) -> String {
    page.evaluate("JSON.stringify(window.__ptyOwnershipTrace || [])")
        .await
        .ok()
        .and_then(|v| v.into_value::<String>().ok())
        .unwrap_or_else(|| "[]".to_string())
}

async fn terminal_center(page: &Page) -> (f64, f64) {
    let value = page
        .evaluate(
            r#"
            (() => {
              const h = terminalHandleForSession(state.currentId);
              const screen = h.term.element.querySelector(".xterm-screen");
              const r = screen.getBoundingClientRect();
              return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
            })()
            "#,
        )
        .await
        .expect("terminal rect")
        .into_value::<serde_json::Value>()
        .expect("terminal point");
    (
        value["x"].as_f64().expect("x"),
        value["y"].as_f64().expect("y"),
    )
}

async fn click_at(page: &Page, x: f64, y: f64) {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(x)
            .y(y)
            .buttons(0)
            .build()
            .expect("mouse move"),
    )
    .await
    .expect("move to terminal");
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .buttons(1)
            .click_count(1)
            .build()
            .expect("mouse down"),
    )
    .await
    .expect("terminal mouse down");
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .buttons(0)
            .click_count(1)
            .build()
            .expect("mouse up"),
    )
    .await
    .expect("terminal mouse up");
}

async fn move_mouse(page: &Page, x: f64, y: f64) {
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(x)
            .y(y)
            .buttons(0)
            .build()
            .expect("mouse move"),
    )
    .await
    .expect("mouse hover");
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

async fn set_viewport(page: &Page, width: u32, height: u32) {
    let _ = page
        .execute(
            SetDeviceMetricsOverrideParams::builder()
                .width(width as i64)
                .height(height as i64)
                .device_scale_factor(1.0)
                .mobile(false)
                .build()
                .expect("device metrics"),
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
