//! End-to-end regression: a Playbook run stops shimmering when the session
//! that owns it dies (issue #1090).
//!
//! The unit tests in `construct-daemon` cover the state machine directly.
//! This one covers the path that produced the bug in real use — a real
//! session, a real dispatch, a real kill — because the gap was not in the
//! reaping logic's shape but in which lifecycle facts it trusted.

use std::time::{Duration, Instant};

use construct_e2e::Daemon;

/// Run a Playbook, then kill the owning session before it ever reports a
/// turn. Nothing alive can settle those blocks, so they must stop
/// shimmering — not ride the inactivity backstop for ten minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn playbook_stops_shimmering_when_the_owning_session_dies() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let cwd = d.socket.parent().unwrap().to_string_lossy().to_string();
    let session = d
        .client
        .create(shell_session_params(&cwd, "shimmer-target"))
        .await
        .expect("create session");

    d.client
        .playbook_update(construct_protocol::PlaybookUpdateParams {
            session_id: session.clone(),
            markdown: "# T\n\n- alpha\n- beta\n".to_string(),
            base_version: None,
            actor: construct_protocol::PlaybookUpdateActor::Human,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("seed playbook");

    d.client
        .playbook_execute(construct_protocol::PlaybookExecuteParams {
            session_id: session.clone(),
            selection: None,
            base_version: None,
            comment: None,
            shimmer: None,
            selection_block_ids: None,
            fork: false,
        })
        .await
        .expect("playbook.execute");

    // Poll rather than read once: the daemon delivers the run prompt to the
    // session *before* it registers the run, so a client that reads the moment
    // `playbook_execute` returns is racing that internal ordering.
    let lit_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lit = d
            .client
            .playbook_get(&session)
            .await
            .expect("playbook.get")
            .blocks
            .iter()
            .any(|b| b.shimmer);
        if lit {
            break;
        }
        if Instant::now() > lit_deadline {
            let state = d
                .client
                .list()
                .await
                .expect("list")
                .iter()
                .find(|s| s.id == session)
                .map(|s| s.state);
            panic!(
                "precondition: Run never lit the executed region \
                 (session state: {state:?})"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    d.client.kill(&session).await.expect("kill session");

    // Well inside the run's backstop: this is about the terminal state being
    // an authoritative stop signal, not about waiting one out.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let blocks = d
            .client
            .playbook_get(&session)
            .await
            .expect("playbook.get")
            .blocks;
        if !blocks.iter().any(|b| b.shimmer) {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "playbook is still shimmering after the owning session died; \
                 nothing left alive can ever settle it: {:?}",
                blocks
                    .iter()
                    .filter(|b| b.shimmer)
                    .map(|b| &b.text)
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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
