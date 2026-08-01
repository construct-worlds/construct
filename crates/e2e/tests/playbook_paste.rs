//! End-to-end regressions for pasting into the TUI Playbook editor.
//!
//! Both tests drive the real `construct` TUI inside a pseudo-terminal
//! against a real daemon, deliver a genuine bracketed paste (the same
//! `ESC [ 200 ~ … ESC [ 201 ~` framing a terminal emitter sends), and
//! then assert against the *daemon's* stored document — the copy the
//! owning agent and every other client actually see.
//!
//! Coverage:
//!
//! - A paste reaches the daemon at all (issue #1103). Today `on_paste`
//!   mutates the client buffer and returns without the flush every
//!   keystroke path performs, so the pasted text never leaves the TUI.
//! - A paste whose line breaks arrive as CR keeps its line structure
//!   (issue #1104). Terminals routinely send `\r` rather than `\n`
//!   inside a bracketed paste; nothing on the client, protocol, or
//!   daemon side normalizes them, so the block collapses into a single
//!   line and a single addressable block.
//!
//! Both are marked `#[ignore]` until their fix lands — they fail on
//! `main` today, and a red `main` blocks every other PR in the repo.
//! Run them explicitly with:
//!
//! ```text
//! cargo test -p construct-e2e --test playbook_paste -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use construct_e2e::{Daemon, Tui};

/// `C-x Space` — open the selected session's Playbook.
const OPEN_PLAYBOOK: &[u8] = b"\x18 ";
/// `C-x C-s` — save the Playbook.
const SAVE_PLAYBOOK: &[u8] = b"\x18\x13";
/// `C-e` — move the caret to end of line.
const END_OF_LINE: &[u8] = b"\x05";

/// Wrap `body` in the bracketed-paste framing a terminal sends when
/// the application has enabled bracketed paste (which the TUI does).
fn bracketed(body: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Poll the daemon's stored Playbook until `pred` holds, or return the
/// last document seen when the deadline passes.
async fn wait_for_playbook(
    d: &Daemon,
    session_id: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = d
            .client
            .playbook_get(session_id)
            .await
            .expect("playbook.get")
            .playbook
            .markdown;
        if pred(&last) {
            return Ok(last);
        }
        if Instant::now() > deadline {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Create one shell session, seed its Playbook, and open that Playbook
/// in a fresh TUI with the caret at the end of the seeded line.
///
/// Returns the session id and the live TUI.
async fn playbook_open_with_seed(d: &Daemon, name: &'static str) -> (String, Tui) {
    let cwd = d.socket.parent().unwrap().to_string_lossy().to_string();
    let session = d
        .client
        .create(shell_session_params(&cwd, "paste-target"))
        .await
        .expect("create session");

    d.client
        .playbook_update(construct_protocol::PlaybookUpdateParams {
            session_id: session.clone(),
            markdown: "seed\n".to_string(),
            base_version: None,
            actor: construct_protocol::PlaybookUpdateActor::Human,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("seed playbook");

    let mut tui = Tui::spawn_with_recording(&d.socket, name).expect("spawn TUI");
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // Esc first: on a machine with no configured harnesses the TUI
    // opens the first-run configure popup over the view, and it would
    // swallow the chord below. Esc is the universal cancel and is a
    // no-op when the popup isn't showing.
    tui.send(b"\x1b").expect("send Esc");
    tokio::time::sleep(Duration::from_millis(300)).await;
    tui.send(OPEN_PLAYBOOK).expect("send C-x Space");

    // The seeded line on screen proves the right session's Playbook is
    // open and hydrated before anything is pasted into it.
    tui.wait_for("seed", Duration::from_secs(10))
        .await
        .expect("playbook never opened on the seeded document");

    tui.send(END_OF_LINE).expect("send C-e");
    tokio::time::sleep(Duration::from_millis(200)).await;

    (session, tui)
}

/// Regression for #1103: a paste into the Playbook must reach the
/// daemon, exactly like a typed character does.
///
/// `handle_playbook_key` snapshots the buffer and calls
/// `flush_playbook_live_edit` after dispatching; `on_paste` inserts and
/// returns. So today the pasted text renders in the TUI and never
/// leaves it — and because the client buffer is now ahead of the
/// daemon, every later keystroke's anchored edit fails to apply and is
/// swallowed too.
///
/// The assertion deliberately reads the daemon's document rather than
/// the TUI screen: the screen is already correct today, which is
/// exactly why the bug is invisible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "fails on main: paste never reaches the daemon (#1103)"]
async fn playbook_paste_reaches_the_daemon() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let (session, mut tui) = playbook_open_with_seed(&d, "playbook_paste_reaches_the_daemon").await;

    tui.send(&bracketed("\n- one\n- two\n  - nested\n"))
        .expect("send bracketed paste");

    // The client applied it — establishes that the paste was delivered
    // and parsed, so a failure below is about publishing, not input.
    tui.wait_for("nested", Duration::from_secs(10))
        .await
        .expect("pasted text never rendered in the TUI");

    let stored = wait_for_playbook(&d, &session, Duration::from_secs(5), |md| {
        md.contains("- nested")
    })
    .await;

    let stored = match stored {
        Ok(md) => md,
        Err(last) => panic!(
            "paste never reached the daemon: the TUI renders the pasted \
             block but the stored document is still {last:?}"
        ),
    };

    assert!(
        stored.contains("- one\n- two\n  - nested"),
        "stored document lost the pasted structure: {stored:?}"
    );

    // A paste must not poison the edits that follow it: typing after a
    // paste has to keep syncing.
    tui.send(b"X").expect("type after paste");
    let after = wait_for_playbook(&d, &session, Duration::from_secs(5), |md| md.contains('X')).await;
    assert!(
        after.is_ok(),
        "typing after a paste stopped syncing; daemon still has {:?}",
        after.unwrap_err()
    );
}

/// Regression for #1104: a bracketed paste whose line breaks arrive as
/// CR must produce the same document as one that uses LF.
///
/// Terminals routinely send `\r` for the line breaks inside a paste —
/// it is what a tty expects for Enter, and what tmux's own
/// `paste-buffer` sends unless given `-r`. Nothing normalizes them, so
/// the block collapses to one line, renders as one line, and parses as
/// a single addressable block.
///
/// The paste is followed by an explicit save so this test isolates CR
/// handling from #1103 — it must keep failing for its own reason after
/// the paste-sync fix lands, and keep passing once CR normalization
/// lands regardless of which fix comes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "fails on main: CR line breaks in a paste are stored verbatim (#1104)"]
async fn playbook_paste_normalizes_carriage_returns() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let (session, mut tui) =
        playbook_open_with_seed(&d, "playbook_paste_normalizes_carriage_returns").await;

    // The same payload as the LF test, with every newline delivered as
    // a carriage return.
    tui.send(&bracketed("\r- one\r- two\r  - nested\r"))
        .expect("send bracketed paste");
    tokio::time::sleep(Duration::from_millis(500)).await;

    tui.send(SAVE_PLAYBOOK).expect("send C-x C-s");

    let stored = wait_for_playbook(&d, &session, Duration::from_secs(10), |md| {
        md.contains("nested")
    })
    .await
    .expect("pasted text never reached the daemon even after an explicit save");

    assert!(
        !stored.contains('\r'),
        "carriage returns were stored verbatim instead of being normalized \
         to newlines: {stored:?}"
    );
    assert!(
        stored.contains("- one\n- two\n  - nested"),
        "pasted block lost its line structure: {stored:?}"
    );

    // The user-visible consequence of storing lone CRs: block splitting
    // is newline-based, so the whole paste becomes one addressable
    // block — one shimmer target, one selection-Run target, one opaque
    // line for the agent.
    let blocks = d
        .client
        .playbook_get(&session)
        .await
        .expect("playbook.get")
        .blocks;
    let list_items = blocks
        .iter()
        .filter(|b| b.text.trim_start().starts_with("- "))
        .count();
    assert!(
        list_items >= 3,
        "pasted list should parse as separate blocks, got {list_items} list \
         block(s) from {:?}",
        blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
    );
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
