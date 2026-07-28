//! End-to-end: the split layout is shared daemon state (spec 0118).
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
        .create(shell_session_params(
            &cwd,
            "beta with a deliberately much longer session name",
        ))
        .await
        .expect("create beta");
    let mut terminal_params = shell_session_params(&cwd, "duplicate terminal");
    terminal_params.mode = Some("interactive".to_string());
    terminal_params.pty_size = Some(construct_protocol::PtySize {
        cols: 100,
        rows: 30,
    });
    let duplicate_terminal = d
        .client
        .create(terminal_params)
        .await
        .expect("create duplicate terminal");

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

    // The title bar carries the two split buttons, each with a hover tooltip
    // (icon-only controls whose meaning isn't guessable), and NO close × —
    // closing a pane lives in the session menu as "close split".
    let split_tips: String = page
        .evaluate(
            "JSON.stringify(Array.from(
               document.querySelectorAll('#paneGrid .pane.is-focused .pane-head button[data-tip]')
             ).map((b) => b.dataset.tip))",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert!(
        split_tips.contains("split horizontal") && split_tips.contains("split vertical"),
        "both split buttons need hover tooltips, got {split_tips}"
    );

    let close_x: f64 = eval_number(
        &page,
        "Array.from(document.querySelectorAll('#paneGrid .pane-head button'))
           .filter((b) => b.textContent.trim() === '×').length",
    )
    .await;
    assert_eq!(
        close_x, 0.0,
        "the title bar carries no × — 'close split' in the session menu replaces it"
    );

    // Focus must not move anything. The focused pane carries an extra
    // control (the menu pill) that the others don't, so its title bar has to
    // be sized independently of what it contains — otherwise focusing a pane
    // nudges its title bar and every pixel below it.
    let head_geometry: String = page
        .evaluate(
            "JSON.stringify(Array.from(document.querySelectorAll('#paneGrid .pane-head'))
               .map((h) => {
                 const r = h.getBoundingClientRect();
                 return [Math.round(r.height), Math.round(r.top)];
               }))",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    let heads: Vec<(i64, i64)> = serde_json::from_str(&head_geometry).unwrap_or_default();
    assert!(heads.len() >= 2, "expected two title bars, got {head_geometry}");
    assert_eq!(
        heads[0].0, heads[1].0,
        "title bars must be the same height focused or not — got {head_geometry}"
    );
    assert_eq!(
        heads[0].1, heads[1].1,
        "title bars must sit at the same y — got {head_geometry}"
    );

    // Content must sit at the same offset inside its pane whether that pane
    // is focused or not. The focused pane nests its surface in the
    // interactive stack while the others mirror it, and any difference in
    // padding between the two shows up as the rendering jumping every time
    // focus moves.
    // A mirrored surface must be laid out exactly like the focused one, or
    // the rendering jumps every time focus moves.
    //
    // Measuring the two panes against each other isn't possible here — the
    // focused pane's transcript only materializes once the session has
    // content, and these shells produce none. So assert the two properties
    // that made them differ instead.
    //
    // 1. The mirror must not re-pad the transcript. The shared rule floors
    //    the inset at 16px; the mirror used to override it to 12/10px.
    let mirror_padding: String = page
        .evaluate(
            "(() => {
               const el = document.querySelector('#paneGrid .pane-mirror .transcript-pane');
               if (!el) return '';
               const cs = getComputedStyle(el);
               return JSON.stringify([cs.paddingLeft, cs.paddingTop]);
             })()",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert_eq!(
        mirror_padding, "[\"16px\",\"16px\"]",
        "a mirrored transcript must keep the same insets it has when focused"
    );

    // 2. A mirrored terminal must sit inside the same wrapper the focused
    //    one uses — that element supplies the terminal's padding and top
    //    border. Inert while no PTY session is mirrored; it bites the moment
    //    one is mounted bare.
    let terminal_mirror_ok: bool = eval_bool(
        &page,
        "Array.from(document.querySelectorAll('#paneGrid .pane-mirror .terminal-host'))
           .every((h) => !!h.closest('.terminal-wrap'))",
    )
    .await;
    assert!(
        terminal_mirror_ok,
        "a mirrored terminal must be wrapped the way the focused one is"
    );

    // ...and switching focus between panes must not move any of it. This is
    // the whole-geometry version of the check above: focus changes which
    // pane is highlighted and which one holds the interactive stack, and
    // neither may shift the grid.
    let geometry_js = "JSON.stringify(Array.from(document.querySelectorAll('#paneGrid .pane'))
         .map((p) => {
           const r = p.getBoundingClientRect();
           const h = p.querySelector('.pane-head').getBoundingClientRect();
           return [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height),
                   Math.round(h.top), Math.round(h.height)];
         }))";
    // Everything after the title cluster must hold still. The session name
    // itself legitimately changes, so the cluster's own width is not part of
    // the comparison — the controls that follow it are.
    let header_js = "JSON.stringify(Array.from(
         document.querySelectorAll('#viewModeToggle, #conn, #sessionRuntime')
       ).map((e) => {
         const r = e.getBoundingClientRect();
         return [e.id, Math.round(r.left), Math.round(r.width)];
       }))";
    let before_focus: String = page
        .evaluate(geometry_js)
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();

    page.evaluate(
        "(() => {
           const panes = document.querySelectorAll('#paneGrid .pane');
           const other = Array.from(panes).find((p) => !p.classList.contains('is-focused'));
           other.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
           return true;
         })()",
    )
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let after_focus: String = page
        .evaluate(geometry_js)
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();

    assert_eq!(
        before_focus, after_focus,
        "changing pane focus must not move the panes or their title bars"
    );

    // A browser refresh keeps the selected session in the URL. That session
    // identifies the locally focused pane (focus itself is intentionally not
    // daemon state), so startup must put the interactive stack back in that
    // pane and hydrate its real surface. Startup must also hydrate the
    // unfocused mirror without letting that background load hide the focused
    // transcript until the user switches away and back.
    let focused_before_refresh = focused_session(&page).await;
    assert_eq!(
        focused_before_refresh, b,
        "the second pane should be focused before exercising refresh"
    );
    page.reload().await.expect("reload focused split");
    wait_conn_open(&page).await;
    assert!(
        wait_for_number(
            &page,
            "document.querySelectorAll('#paneGrid .pane').length",
            2.0,
        )
        .await
        .is_some(),
        "the split layout should return after refresh"
    );
    let restored_js = format!(
        "(() => {{
           const focused = document.querySelector('#paneGrid .pane.is-focused');
           const transcriptPane = state.transcriptPaneById.get({b:?});
           const surfaceVisible = state.mode === 'terminal'
             ? !document.getElementById('terminalWrap').hidden
             : state.mode === 'program'
               ? !document.getElementById('programWrap').hidden
               : !!transcriptPane && !transcriptPane.hidden && focused?.contains(transcriptPane);
           return state.currentId === {b:?}
             && focusedPaneSessionId() === {b:?}
             && !!focused?.querySelector('#viewStack')
             && surfaceVisible;
         }})()"
    );
    let restored = wait_for_bool(&page, &restored_js).await;
    let restored_state: String = page
        .evaluate(
            "JSON.stringify({
               currentId: state.currentId,
               focusedPaneId: state.focusedPaneId,
               focusedPaneSessionId: focusedPaneSessionId(),
               urlRequestedSessionId: state.urlRequestedSessionId,
               mode: state.mode,
               session: state.sessions.find((s) => s.id === state.currentId),
               terminalHidden: document.getElementById('terminalWrap').hidden,
               transcriptMounted: (() => {
                 const pane = state.transcriptPaneById.get(state.currentId);
                 const focused = document.querySelector('#paneGrid .pane.is-focused');
                 return !!pane && !pane.hidden && focused?.contains(pane);
               })(),
               transcript: (() => {
                 const pane = state.transcriptPaneById.get(state.currentId);
                 return pane ? {
                   hidden: pane.hidden,
                   parentId: pane.parentElement?.id,
                   parentClass: pane.parentElement?.className,
                 } : null;
               })(),
               mirroredSessionIds: Array.from(state.mirroredSessionIds),
               stackInFocused: !!document.querySelector('#paneGrid .pane.is-focused #viewStack'),
             })",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert!(
        restored,
        "refresh must restore the focused pane's session and visible surface; got {restored_state}"
    );
    save_screenshot(&page, "split_layout_refresh_restored.png").await;

    // The header must not reflow when the session name changes length.
    // Focusing another pane switches the selected session, and sizing the
    // title to its text made every control after it slide — the visible
    // jolt in the header on each focus change. Driven directly rather than
    // through a focus switch so it can't be confused with async hydration
    // of the incoming session's capabilities.
    let title_probe = |text: &str| {
        format!(
            "(() => {{
               document.getElementById('sessionTitle').textContent = {text:?};
               void document.body.offsetWidth;
               return JSON.stringify(Array.from(
                 document.querySelectorAll('#viewModeToggle, #conn, #sessionRuntime')
               ).map((e) => [e.id, Math.round(e.getBoundingClientRect().left)]));
             }})()"
        )
    };
    let with_short: String = page
        .evaluate(title_probe("a"))
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    let with_long: String = page
        .evaluate(title_probe(
            "a considerably longer session name than the other one",
        ))
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert_eq!(
        with_short, with_long,
        "header controls must not move when the session name's length changes"
    );


    // The menu pill moves into the title bar's right end, the way the TUI
    // hangs its menu off the pane title.
    let menu_in_head: bool = eval_bool(
        &page,
        "!!document.querySelector('#paneGrid .pane.is-focused .pane-head #sessionMenuOverlay')",
    )
    .await;
    assert!(
        menu_in_head,
        "the session menu pill belongs in the pane title bar once one exists"
    );

    // ...and that menu is what now offers the split/close actions.
    let menu_actions: String = page
        .evaluate(
            "JSON.stringify(Array.from(
               document.querySelectorAll('#sessionMenu [data-menu-action]')
             ).map((b) => b.dataset.menuAction))",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    for action in ["split-horizontal", "split-vertical", "close-split"] {
        assert!(
            menu_actions.contains(action),
            "session menu must offer {action}, got {menu_actions}"
        );
    }

    // Closing a split through the menu really collapses the layout, and the
    // collapse is published like any other layout edit.
    let before_close = d.client.layout().await.expect("layout").version;
    page.evaluate(
        "(() => { document.querySelector('#sessionMenu [data-menu-action=\"close-split\"]').click(); return true; })()",
    )
    .await
    .ok();
    let collapsed_ok = wait_for_bool(
        &page,
        "document.getElementById('paneGrid').hidden || document.querySelectorAll('#paneGrid .pane').length === 1",
    )
    .await;
    assert!(collapsed_ok, "\"close split\" must close the focused pane");
    let after_close = d.client.layout().await.expect("layout");
    assert!(
        after_close.version > before_close,
        "closing a pane is a layout edit and must be published to other clients"
    );

    // Split back from a SINGLE pane through the session menu. This is the
    // only path to a first split now that the main title bar carries no
    // split buttons — with one pane there is no pane title bar to hold
    // them — so it has to work from here.
    let menu_reachable = wait_for_bool(
        &page,
        "(() => {
           const btn = document.getElementById('sessionMenuBtn');
           const item = document.querySelector('#sessionMenu [data-menu-action=\"split-horizontal\"]');
           return !!btn && !btn.closest('[hidden]') && !!item && !item.disabled;
         })()",
    )
    .await;
    assert!(
        menu_reachable,
        "with one pane, the session menu must still offer a split"
    );

    page.evaluate(
        "(() => { document.querySelector('#sessionMenu [data-menu-action=\"split-horizontal\"]').click(); return true; })()",
    )
    .await
    .ok();
    assert!(
        wait_for_number(&page, "document.querySelectorAll('#paneGrid .pane').length", 2.0)
            .await
            .is_some(),
        "splitting from the session menu must produce a second pane"
    );

    // That split is a layout edit like any other, so it must be published.
    let after_menu_split = d.client.layout().await.expect("layout");
    assert!(
        after_menu_split.version > after_close.version,
        "a split made from the menu must reach other clients"
    );
    assert_eq!(
        after_menu_split.tree.leaf_count(),
        2,
        "the shared tree gained the pane too"
    );

    let focused: f64 = eval_number(&page, "document.querySelectorAll('#paneGrid .pane.is-focused').length").await;
    assert_eq!(focused, 1.0, "exactly one pane holds focus");
    let focused_border_radius: String = page
        .evaluate(
            "getComputedStyle(document.querySelector('#paneGrid .pane.is-focused')).borderRadius",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert_eq!(
        focused_border_radius, "0px",
        "the focused split highlight must have square corners"
    );
    let stack_in_focused: bool = eval_bool(
        &page,
        "!!document.querySelector('#paneGrid .pane.is-focused #viewStack')",
    )
    .await;
    assert!(
        stack_in_focused,
        "the interactive stack lives in the focused pane"
    );

    // Split creation deliberately starts the new leaf on the same session.
    // A session's canonical surface DOM can only have one parent, so WebUI
    // needs a pane-scoped passive replica for the other leaf rather than
    // moving the one surface back and forth and blanking a pane. This shell
    // currently has its semantic chat view selected; prove that surface is
    // present in both panes before exercising the terminal representation.
    let duplicate_chat_js =
        "(() => {
           const panes = Array.from(document.querySelectorAll('#paneGrid .pane'));
           return panes.length === 2 && panes.every((pane) =>
             Array.from(pane.querySelectorAll('.transcript-pane')).some(
               (transcript) =>
                 transcript.dataset.sessionId === state.currentId &&
                 !transcript.hidden
             )
           );
         })()";
    let duplicate_chat_surfaces = wait_for_bool(&page, &duplicate_chat_js).await;
    let duplicate_chat_state: String = page
        .evaluate(
            "JSON.stringify({
               currentId: state.currentId,
               focusedPaneId: state.focusedPaneId,
               focusedSessionId: focusedPaneSessionId(),
               mode: state.mode,
               panes: Array.from(document.querySelectorAll('#paneGrid .pane')).map((pane) => ({
                 paneId: pane.dataset.paneId,
                 focused: pane.classList.contains('is-focused'),
                 transcripts: Array.from(pane.querySelectorAll('.transcript-pane')).map((p) => ({
                   sessionId: p.dataset.sessionId,
                   hidden: p.hidden,
                   duplicate: p.classList.contains('transcript-duplicate-mirror'),
                   connected: p.isConnected,
                 })),
               })),
             })",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert!(
        duplicate_chat_surfaces,
        "two panes assigned to one chat session must both render its transcript; \
         got {duplicate_chat_state}"
    );

    let terminal_duplicate_tree = LayoutNode::Split {
        direction: LayoutSplitDirection::Right,
        ratio_percent: 50,
        first: Box::new(LayoutNode::Leaf {
            id: 1,
            session_id: Some(duplicate_terminal.clone()),
        }),
        second: Box::new(LayoutNode::Leaf {
            id: 2,
            session_id: Some(duplicate_terminal.clone()),
        }),
    };
    d.client
        .set_layout(
            terminal_duplicate_tree,
            Some(d.client.layout().await.expect("layout before terminal duplicate").version),
        )
        .await
        .expect("assign terminal session to both panes");
    let duplicate_terminal_ready_js = format!(
        "state.currentId === {duplicate_terminal:?} && state.mode === 'terminal'"
    );
    assert!(
        wait_for_bool(&page, &duplicate_terminal_ready_js).await,
        "the duplicated interactive shell should become the focused terminal"
    );

    let both_duplicate_surfaces = wait_for_bool(
        &page,
        "(() => {
           const panes = Array.from(document.querySelectorAll('#paneGrid .pane'));
           return panes.length === 2 && panes.every((pane) => {
             const host = pane.querySelector('.terminal-host');
             return !!host && !host.hidden && !!host.querySelector('.xterm-screen');
           });
         })()",
    )
    .await;
    let duplicate_surface_state: String = page
        .evaluate(
            "JSON.stringify({
               currentId: state.currentId,
               focusedPaneId: state.focusedPaneId,
               focusedSessionId: focusedPaneSessionId(),
               mode: state.mode,
               leaves: layoutLeaves(state.layout.tree),
               mirrors: Array.from(state.duplicateTerminalMirrorByPaneId.entries())
                 .map(([paneId, h]) => ({
                   paneId,
                   sessionId: h.sessionId,
                   loaded: h.loaded,
                   hydrating: h.hydrating,
                   connected: h.host.isConnected,
                   hidden: h.host.hidden,
                   hasScreen: !!h.host.querySelector('.xterm-screen'),
                 })),
               panes: Array.from(document.querySelectorAll('#paneGrid .pane')).map((pane) => {
                 const host = pane.querySelector('.terminal-host');
                 return {
                   paneId: pane.dataset.paneId,
                   focused: pane.classList.contains('is-focused'),
                   hostSessionId: host?.dataset.sessionId || null,
                   hostHidden: host?.hidden ?? null,
                   hasScreen: !!host?.querySelector('.xterm-screen'),
                   body: pane.querySelector('.pane-body')?.innerHTML.slice(0, 200),
                 };
               }),
             })",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    assert!(
        both_duplicate_surfaces,
        "two panes assigned to one terminal session must both render a terminal surface; \
         got {duplicate_surface_state}"
    );

    d.client
        .pty_input(
            &duplicate_terminal,
            b"printf '__duplicate_pane_live__\\n'\r".to_vec(),
        )
        .await
        .expect("write duplicate-pane sentinel");
    let both_duplicate_surfaces_live = wait_for_bool(
        &page,
        "(() => {
           const panes = Array.from(document.querySelectorAll('#paneGrid .pane'));
           return panes.length === 2 && panes.every(
             (pane) => (pane.querySelector('.xterm-rows')?.textContent || '')
               .includes('__duplicate_pane_live__')
           );
         })()",
    )
    .await;
    assert!(
        both_duplicate_surfaces_live,
        "both same-session panes must continue receiving live PTY output"
    );

    // Leave a reviewable artifact of the split actually rendering, the way
    // `web_smoke` leaves a video. Park the pointer over a split button first
    // so the capture shows the hover tooltip rather than a bare glyph.
    hover_split_button(&page).await;
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

/// Move the real pointer over the focused pane's first split button. CSS
/// `:hover` only responds to genuine input events, so this has to go through
/// CDP rather than a synthetic DOM event.
async fn hover_split_button(page: &Page) {
    use chromiumoxide::layout::Point;
    let rect: String = page
        .evaluate(
            "(() => {
               const b = document.querySelector('#paneGrid .pane.is-focused .pane-head button[data-tip]');
               if (!b) return '';
               const r = b.getBoundingClientRect();
               return JSON.stringify([r.left + r.width / 2, r.top + r.height / 2]);
             })()",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default();
    let Ok(xy) = serde_json::from_str::<(f64, f64)>(&rect) else {
        return;
    };
    // Two moves: Chrome only re-runs hit-testing when the pointer actually
    // changes position, so a single move into a fresh page can land before
    // layout settles and never raise :hover.
    let _ = page.move_mouse(Point::new(xy.0 - 40.0, xy.1 + 40.0)).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = page.move_mouse(Point::new(xy.0, xy.1)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Hovering does apply — the button takes its hover background — but the
    // tooltip itself cannot be asserted here. Headless Chrome has no
    // pointing device, so it reports `hover: none`, and the tooltip's own
    // touch guard (`@media (hover: none)`) correctly suppresses it. Neither
    // `Emulation.setEmulatedMedia` nor Blink's hover-type switches move that
    // media query in this headless build.
    //
    // So what is checked here is what headless can honestly prove: the
    // tooltip's content resolves from `data-tip`, and the hover rule that
    // reveals it exists. Its appearance on a real desktop pointer is left to
    // manual review.
    let wired: bool = page
        .evaluate(
            "(() => {
               const b = document.querySelector('#paneGrid .pane.is-focused .pane-head button[data-tip]');
               if (!b) return false;
               const tip = getComputedStyle(b, '::after');
               if (!tip.content.includes('split')) return false;
               // The `:hover` rule must exist, or the tip could never appear
               // on a pointer device either.
               return Array.from(document.styleSheets).some((sheet) => {
                 let rules;
                 try { rules = sheet.cssRules; } catch (_) { return false; }
                 return Array.from(rules || []).some(
                   (r) => r.selectorText && r.selectorText.includes('.has-tip:hover::after')
                 );
               });
             })()",
        )
        .await
        .ok()
        .and_then(|r| r.into_value::<bool>().ok())
        .unwrap_or(false);
    assert!(
        wired,
        "the tooltip must resolve its text from data-tip and have a :hover reveal rule"
    );
}

/// Focus the first pane in layout order, whichever it is.
async fn focus_first_pane(page: &Page) {
    let _ = page
        .evaluate(
            "(() => {
               const p = document.querySelector('#paneGrid .pane');
               if (p) p.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
               return true;
             })()",
        )
        .await;
}

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
