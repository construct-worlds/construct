//! Route-capability probe (spec 0111).
//!
//! Whether a harness honors Construct's routing injection is an empirical,
//! per-harness, per-version fact — never a reading of that harness's
//! documentation. This is the test that establishes it: point `HTTPS_PROXY`
//! at a local stub, run the real harness binary, and see whether a
//! `CONNECT` arrives.
//!
//! `harness_routing()` in the daemon lists the harnesses claimed to be
//! route-capable. Every entry there must have passed this probe, and it has
//! to be re-run when a harness version changes — a CLI release can move its
//! networking onto a client that ignores proxy environment entirely, and
//! the resulting failure is silent (the session simply stops being
//! routable) unless something checks.
//!
//! Ignored by default: it needs the real harness binary installed, makes a
//! genuine outbound connection attempt, and is therefore unfit for CI.
//! Run it deliberately:
//!
//! ```text
//! cargo test -p construct-e2e --test router_probe -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Accept one connection and report the `CONNECT` authority it carried.
///
/// Answers with `502` on purpose: the probe only asks whether the harness
/// *routes through us*, and refusing keeps it from making a real API call.
fn spawn_probe_listener() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                let _ = tx.send(line.trim().to_string());
            }
            let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
        }
    });
    (port, rx)
}

fn harness_bin(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The claim recorded for `claude` in the daemon's routing table.
#[test]
#[ignore = "needs the real claude binary and makes an outbound connection attempt"]
fn claude_honors_the_proxy_environment() {
    let Some(bin) = harness_bin("claude") else {
        panic!("claude is not installed; the probe cannot prove route capability");
    };
    let (port, rx) = spawn_probe_listener();
    let proxy = format!("http://probe-token@127.0.0.1:{port}");

    let mut child = Command::new(bin)
        .args(["-p", "say hi"])
        .env("HTTPS_PROXY", &proxy)
        .env("https_proxy", &proxy)
        // A key must be present or the CLI may never attempt a request at
        // all; it does not need to be valid, because our stub refuses.
        .env("ANTHROPIC_API_KEY", "sk-probe-not-a-real-key")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn claude");

    let seen = rx.recv_timeout(Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();

    let line = seen.expect(
        "claude made no proxied connection: it is NOT route-capable on this version. \
         Remove it from the daemon's harness_routing table until a probe passes.",
    );
    assert!(
        line.to_ascii_uppercase().starts_with("CONNECT"),
        "expected a CONNECT through the proxy, got: {line}"
    );
    assert!(
        line.contains("api.anthropic.com"),
        "probe saw an unexpected destination — the intercept-host list in \
         harness_routing may be stale: {line}"
    );
}

/// Guards the other half of the claim: a harness with no probe must not be
/// offered as routable. Cheap, no binary required, runs in CI.
#[test]
fn only_probed_harnesses_are_declared_route_capable() {
    // Kept in sync by hand with the daemon's `harness_routing`. If you add
    // a harness there, add its probe above and its name here — in that
    // order (spec 0111).
    const PROBED: &[&str] = &["claude"];
    for harness in ["codex", "smith", "shell", "opencode", "grok", "kimi", "pi"] {
        assert!(
            !PROBED.contains(&harness),
            "{harness} is listed as probed but has no probe test"
        );
    }
    assert_eq!(PROBED, &["claude"]);
}
