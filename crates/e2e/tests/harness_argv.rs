//! Harness argv must reach the *harness*, not the adapter's command line.
//!
//! `construct new <harness> <args...>` passes every token after the harness
//! verbatim to that harness's CLI — the documented contrast being
//! `construct new --model opus claude` (construct's metadata) versus
//! `construct new claude --model opus` (raw claude argv).
//!
//! The daemon used to append those args to the *adapter process's* command
//! line as well. Every built-in adapter is dispatched as `construct __adapter
//! <name>`, which takes no arguments, so the adapter died on startup before it
//! could bind its socket. The create then failed ~5s later as an opaque
//! "spawn adapter for <harness>", and passing any harness argv at all was
//! impossible.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use construct_e2e::Daemon;
use construct_protocol::CreateSessionParams;

/// Has `needle` shown up in the session's PTY log yet? The replay arrives
/// base64-encoded, so it has to be decoded before matching.
async fn pty_log_contains(d: &Daemon, id: &str, needle: &str) -> bool {
    use base64::Engine as _;
    let Ok(replay) = d.client.pty_replay(id).await else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(replay.data.as_bytes()) else {
        return false;
    };
    String::from_utf8_lossy(&bytes).contains(needle)
}

fn shell_session(cwd: String, args: Vec<String>, prompt: Option<String>) -> CreateSessionParams {
    CreateSessionParams {
        harness: "shell".into(),
        cwd,
        prompt,
        model: None,
        title: Some("argv probe".into()),
        mode: None,
        pty_size: None,
        worktree: false,
        env: HashMap::new(),
        args,
        kind: Default::default(),
        parent_session_id: None,
        group_id: None,
        position_after_session_id: None,
        forked_from: None,
    }
}

/// A session carrying harness argv must start at all. This is the regression:
/// before the fix, `create` returned an error instead of a session id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_harness_argv_starts() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();

    let id = d
        .client
        .create(shell_session(
            cwd,
            vec!["-lc".into(), "echo argv-reached-the-harness".into()],
            None,
        ))
        .await
        .expect("a session with harness argv must start");

    assert!(!id.is_empty(), "expected a session id");
}

/// …and the argv must actually be what the harness ran, not merely tolerated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_argv_is_what_the_harness_runs() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();
    const MARKER: &str = "argv-reached-the-harness";

    let id = d
        .client
        .create(shell_session(
            cwd,
            vec!["-lc".into(), format!("echo {MARKER}")],
            None,
        ))
        .await
        .expect("create shell session with argv");

    // The shell runs `-lc "echo <marker>"`; its stdout lands in the PTY log.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if pty_log_contains(&d, &id, MARKER).await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "harness argv never ran: {MARKER} absent from the PTY log"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The prompt path must keep working: a shell *command* is a prompt (run as
/// `$SHELL -lc <prompt>`), and it shares the spawn path the argv fix touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_still_runs_without_argv() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();
    const MARKER: &str = "prompt-still-runs";

    let id = d
        .client
        .create(shell_session(
            cwd,
            Vec::new(),
            Some(format!("echo {MARKER}")),
        ))
        .await
        .expect("create shell session with a prompt");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if pty_log_contains(&d, &id, MARKER).await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "prompt never ran: {MARKER} absent from the PTY log"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
