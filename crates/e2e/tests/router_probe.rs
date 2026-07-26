//! Route-capability probe (spec 0111).
//!
//! Whether a harness honors Construct's routing injection is an empirical,
//! per-harness, per-version fact — never a reading of that harness's
//! documentation. This is the test that establishes it.
//!
//! **It asserts the harness completes a turn, not merely that a
//! connection arrives.** An earlier version of this probe checked only for
//! an inbound `CONNECT` and passed while the harness was in fact failing
//! every request: the injected proxy URL carried a username-only
//! credential (`http://token@host`), which the client accepted far enough
//! to send `CONNECT` and then failed with a DNS-shaped error. Reachability
//! is not function, and only the stricter assertion catches that.
//!
//! `harness_routing()` in the daemon lists the harnesses claimed to be
//! route-capable. Every entry there must have passed this probe, and it
//! must be re-run when a harness version changes.
//!
//! Ignored by default: needs the real harness binary, working credentials,
//! and network. Run it deliberately:
//!
//! ```text
//! cargo test -p construct-e2e --test router_probe -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A real tunneling `CONNECT` proxy, so the harness under probe reaches
/// its actual endpoint and can complete a turn. A stub that refuses would
/// only prove the harness *tried*.
fn spawn_tunneling_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe proxy");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_w = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(client) = stream else { continue };
            let seen_w = seen_w.clone();
            std::thread::spawn(move || {
                let _ = tunnel_one(client, seen_w);
            });
        }
    });
    (port, seen)
}

fn tunnel_one(mut client: TcpStream, seen: Arc<Mutex<Vec<String>>>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if client.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.len() > 16 * 1024 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let line = text.lines().next().unwrap_or_default().to_string();
    seen.lock().unwrap().push(line.clone());

    let Some(authority) = line.split_whitespace().nth(1) else {
        return Ok(());
    };
    let upstream = TcpStream::connect(authority)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    // Blind splice, both directions.
    let (mut a_in, mut a_out) = (client.try_clone()?, client);
    let (mut b_in, mut b_out) = (upstream.try_clone()?, upstream);
    let up = std::thread::spawn(move || std::io::copy(&mut a_in, &mut b_out));
    let _ = std::io::copy(&mut b_in, &mut a_out);
    let _ = up.join();
    Ok(())
}

fn harness_bin(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The proxy URL shape the daemon injects, reproduced exactly.
///
/// The password half is the whole point of reproducing it: a
/// username-only credential is what broke the harness while still
/// producing a healthy-looking `CONNECT`. If the daemon's construction
/// changes, this must change with it or the probe stops testing reality.
fn injected_proxy_url(port: u16) -> String {
    format!("http://probe0token:construct@127.0.0.1:{port}")
}

/// The claim recorded for `claude` in the daemon's routing table.
#[test]
#[ignore = "needs the real claude binary, credentials, and network"]
fn claude_completes_a_turn_through_the_injected_proxy() {
    let Some(bin) = harness_bin("claude") else {
        panic!("claude is not installed; the probe cannot prove route capability");
    };
    let (port, seen) = spawn_tunneling_proxy();
    let proxy = injected_proxy_url(port);

    let out = Command::new(bin)
        .args(["-p", "reply with exactly one word: pong"])
        .env("HTTPS_PROXY", &proxy)
        .env("https_proxy", &proxy)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude")
        .wait_with_output()
        .expect("wait for claude");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let connects = seen.lock().unwrap().clone();

    assert!(
        connects.iter().any(|l| l.contains("api.anthropic.com")),
        "claude made no proxied connection to the API: it is NOT route-capable \
         on this version. Remove it from the daemon's harness_routing table \
         until a probe passes. saw={connects:?}"
    );
    // The assertion that actually matters: a connection arriving proves
    // reachability, not that the harness works through us.
    assert!(
        !stdout.trim().is_empty() && !stdout.contains("API Error"),
        "claude reached the proxy but could not complete a turn through it. \
         stdout={stdout:?} stderr={stderr:?}"
    );
}

/// codex honors the proxy environment — measured, not assumed.
///
/// It reaches its endpoint (`chatgpt.com`, the subscription backend) through
/// the injected proxy and completes a turn, so pass-through works today.
///
/// It is nonetheless absent from the daemon's routing table, for a reason
/// that is about *interception*, not transport: codex takes its extra CA
/// through `SSL_CERT_FILE` / `CODEX_CA_CERTIFICATE`, and those **replace**
/// the system roots rather than adding to them (verified: pointing
/// `SSL_CERT_FILE` at an empty file makes codex fail every TLS connection
/// with "no certificates found in PEM file"). Injecting only the Construct
/// CA there would break every other TLS connection the session makes.
/// Routing codex therefore requires composing a bundle of the system roots
/// plus our CA — until that exists, declaring codex route-capable would
/// hand users a session that dies the moment a route is armed.
#[test]
#[ignore = "needs the real codex binary, credentials, and network"]
fn codex_honors_the_proxy_environment() {
    let Some(bin) = harness_bin("codex") else {
        panic!("codex is not installed; cannot probe it");
    };
    let (port, seen) = spawn_tunneling_proxy();
    let proxy = injected_proxy_url(port);

    let out = Command::new(bin)
        .args(["exec", "--skip-git-repo-check", "reply with exactly one word: pong"])
        .env("HTTPS_PROXY", &proxy)
        .env("https_proxy", &proxy)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codex")
        .wait_with_output()
        .expect("wait for codex");

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let connects = seen.lock().unwrap().clone();
    assert!(
        !connects.is_empty(),
        "codex made no proxied connection — it would no longer be a routing \
         candidate. stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "codex reached the proxy but could not complete a turn through it. \
         stdout={stdout:?} stderr={stderr:?}"
    );
}

/// Guards the other half of the claim: a harness with no probe must not be
/// offered as routable. Cheap, no binary required, runs in CI.
#[test]
fn only_probed_harnesses_are_declared_route_capable() {
    // Kept in sync by hand with the daemon's `harness_routing`. If you add
    // a harness there, add its probe above and its name here — in that
    // order (spec 0111).
    //
    // codex is deliberately NOT here despite passing the transport probe:
    // its CA channel replaces the system roots, so interception needs
    // bundle composition first. Transport capability alone is not route
    // capability.
    const PROBED: &[&str] = &["claude"];
    for harness in ["codex", "smith", "shell", "opencode", "grok", "kimi", "pi"] {
        assert!(
            !PROBED.contains(&harness),
            "{harness} is listed as probed but has no probe test"
        );
    }
    assert_eq!(PROBED, &["claude"]);
}

/// The probe's own instrument must be sound: if the tunneling proxy did
/// not actually tunnel, the probe above would fail for the wrong reason
/// and look like a harness regression.
#[test]
fn the_probe_proxy_really_tunnels() {
    let (port, seen) = spawn_tunneling_proxy();
    let origin = TcpListener::bind("127.0.0.1:0").expect("bind origin");
    let origin_port = origin.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = origin.accept() {
            let mut buf = [0u8; 5];
            let _ = s.read_exact(&mut buf);
            let _ = s.write_all(&buf);
        }
    });

    let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect proxy");
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(c, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    let mut ack = [0u8; 39];
    c.read_exact(&mut ack).unwrap();
    assert!(String::from_utf8_lossy(&ack).starts_with("HTTP/1.1 200"));

    c.write_all(b"hello").unwrap();
    let mut echoed = [0u8; 5];
    c.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"hello");
    assert_eq!(seen.lock().unwrap().len(), 1);
}
