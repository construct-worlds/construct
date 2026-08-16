//! Definitions edited on disk reach a running daemon (spec 0173).
//!
//! The configuration directory is a documented, hand-editable surface, so an
//! edit made in a text editor has to apply on the same terms as one made in
//! the UI. That path has no IPC call to assert against — the only evidence is
//! that the daemon's *listeners* change — so it needs a real daemon and a real
//! socket, which is what this exercises.
//!
//! Deliberately not asserted: anything requiring a model. A CI runner has no
//! harness credential, so the test never lets a request reach a session; a
//! rejected request is enough to prove the endpoint is up and is this operator.
//! Timings are polled with generous deadlines rather than slept, because the
//! watcher's interval is an implementation detail and CI runners are slow.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// Long enough to absorb a loaded runner; the watcher itself polls far faster.
const DEADLINE: Duration = Duration::from_secs(30);

fn definition(port: u16, paused: bool) -> String {
    format!(
        "instruction = \"e2e\"\n\
         harness = \"smith\"\n\
         cwd = \".\"\n\
         routing = \"per-event\"\n\
         paused = {paused}\n\
         \n\
         [channels.http1]\n\
         kind = \"http\"\n\
         enabled = true\n\
         port = {port}\n\
         token = \"e2e-secret\"\n"
    )
}

/// A port nothing is listening on right now.
async fn free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    Ok(listener.local_addr()?.port())
}

/// Definitions are written the way an editor would: to a temporary file, then
/// renamed over the target. A partial file is legal for the daemon to see (it
/// simply fails to parse and retries), but writing atomically keeps the test
/// asserting the behavior it means to.
fn write_definition(path: &PathBuf, body: &str) -> Result<()> {
    let temporary = path.with_extension("toml.writing");
    std::fs::write(&temporary, body)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Whether the operator endpoint is answering on `port`.
///
/// An unauthenticated request is enough: a 401 proves something is listening
/// *and* that it is a operator channel rather than an unrelated socket, without
/// ever creating a session.
async fn endpoint_rejects_unauthenticated(client: &reqwest::Client, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/svc/watched");
    match client.post(&url).body("{\"message\":\"probe\"}").send().await {
        Ok(response) => response.status().as_u16() == 401,
        Err(_) => false,
    }
}

async fn wait_until<F, Fut>(what: &str, mut probe: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if probe().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(anyhow!("timed out waiting for {what}"))
}

#[tokio::test]
async fn a_definition_written_by_hand_starts_and_stops_serving() -> Result<()> {
    let daemon = construct_e2e::Daemon::spawn().await?;
    let operators_dir = daemon.dir.path().join("config").join("operators");
    std::fs::create_dir_all(&operators_dir)?;
    let definition_path = operators_dir.join("watched.toml");
    let port = free_port().await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    // Nothing has been written yet, so nothing should be listening. This also
    // guards the test itself: a port that was already busy would make every
    // later assertion meaningless.
    assert!(
        !endpoint_rejects_unauthenticated(&http, port).await,
        "port {port} is already serving before the definition exists"
    );

    // A definition appears in the config directory, with no IPC call and no
    // restart.
    write_definition(&definition_path, &definition(port, false))?;
    wait_until("the hand-written definition to start serving", || {
        endpoint_rejects_unauthenticated(&http, port)
    })
    .await?;

    // An edit to that same file withdraws the endpoint. Before definitions
    // were applied live this did nothing at all: a paused operator kept its
    // listener bound and kept answering.
    write_definition(&definition_path, &definition(port, true))?;
    wait_until("the paused operator to release its port", || async {
        !endpoint_rejects_unauthenticated(&http, port).await
    })
    .await?;

    // ...and resuming brings it back, so the edit path works in both
    // directions rather than only tearing things down.
    write_definition(&definition_path, &definition(port, false))?;
    wait_until("the resumed operator to serve again", || {
        endpoint_rejects_unauthenticated(&http, port)
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn a_definition_that_does_not_parse_leaves_the_operator_running() -> Result<()> {
    let daemon = construct_e2e::Daemon::spawn().await?;
    let operators_dir = daemon.dir.path().join("config").join("operators");
    std::fs::create_dir_all(&operators_dir)?;
    let definition_path = operators_dir.join("watched.toml");
    let port = free_port().await?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    write_definition(&definition_path, &definition(port, false))?;
    wait_until("the definition to start serving", || {
        endpoint_rejects_unauthenticated(&http, port)
    })
    .await?;

    // A file that cannot be parsed must not disturb what is already running:
    // the user's mistake costs them the edit, not the operator.
    write_definition(&definition_path, "this is not valid toml [[[")?;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        endpoint_rejects_unauthenticated(&http, port).await,
        "a definition that does not parse must leave the running operator alone"
    );

    // Correcting the file is picked up on a later pass, so a bad save is not
    // a state the daemon has to be restarted out of.
    write_definition(&definition_path, &definition(port, true))?;
    wait_until("the corrected definition to apply", || async {
        !endpoint_rejects_unauthenticated(&http, port).await
    })
    .await?;

    Ok(())
}
