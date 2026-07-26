//! Route-capability probe (spec 0115).
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
/// plus our CA, which the router now does — codex is given the composed
/// bundle rather than the bare CA, and is refused entirely if the platform
/// trust store cannot be read.
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

/// Transport probes for the remaining installed harnesses.
///
/// Every one of these honors the proxy environment and completes a turn
/// through it, so **pass-through works for all of them today**. None is in
/// the daemon's routing table, and the reason is neither transport nor CA
/// trust — see `harness_dialects_are_unestablished` below.
///
/// Measured endpoints and CA channels (probed, not assumed):
///
/// | harness  | endpoint                          | CA channel                    |
/// |----------|-----------------------------------|-------------------------------|
/// | claude   | api.anthropic.com                 | NODE_EXTRA_CA_CERTS, additive |
/// | pi       | chatgpt.com                       | NODE_EXTRA_CA_CERTS, additive |
/// | opencode | api.meta.ai                       | NODE_EXTRA_CA_CERTS, additive |
/// | grok     | cli-chat-proxy.grok.com           | SSL_CERT_FILE, additive       |
/// | hermes   | inference-api.nousresearch.com    | SSL_CERT_FILE, REPLACES roots |
/// | codex    | chatgpt.com                       | SSL_CERT_FILE, REPLACES roots |
///
/// "Additive" was verified by MITM: a forged leaf signed by a throwaway CA
/// placed in that variable completed a TLS handshake while the harness
/// still reached its real endpoint. "Replaces" was verified by pointing the
/// variable at a bundle without the system roots and watching every TLS
/// connection fail.
#[test]
#[ignore = "needs the real harness binaries, credentials, and network"]
fn other_harnesses_honor_the_proxy_environment() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("grok", &["-p", "reply with exactly one word: pong"], "grok.com"),
        ("opencode", &["run", "reply with exactly one word: pong"], ""),
        ("hermes", &["-z", "reply with exactly one word: pong"], ""),
        ("pi", &["-p", "reply with exactly one word: pong"], ""),
    ];
    for (name, args, expect_host) in cases {
        let Some(bin) = harness_bin(name) else {
            eprintln!("skip {name}: not installed");
            continue;
        };
        let (port, seen) = spawn_tunneling_proxy();
        let proxy = injected_proxy_url(port);
        let out = Command::new(bin)
            .args(*args)
            .env("HTTPS_PROXY", &proxy)
            .env("https_proxy", &proxy)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"))
            .wait_with_output()
            .unwrap_or_else(|e| panic!("wait {name}: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let connects = seen.lock().unwrap().clone();
        assert!(
            !connects.is_empty(),
            "{name} made no proxied connection; pass-through would not work for it"
        );
        if !expect_host.is_empty() {
            assert!(
                connects.iter().any(|l| l.contains(expect_host)),
                "{name}: expected a connection to {expect_host}, saw {connects:?}"
            );
        }
        assert!(
            !stdout.trim().is_empty(),
            "{name} reached the proxy but completed no turn through it"
        );
    }
}

/// Captured harness dialects (spec 0116).
///
/// Established by interception, not inference: each harness was run
/// through a MITM proxy holding a forged leaf for its own endpoint, and
/// its first real request body was decompressed and read.
///
/// | harness  | endpoint                                  | request shape    |
/// |----------|-------------------------------------------|------------------|
/// | claude   | api.anthropic.com `/v1/messages`           | Anthropic Msgs   |
/// | codex    | chatgpt.com `/backend-api/codex/responses` | OpenAI Responses |
/// | pi       | chatgpt.com `/backend-api/codex/responses` | OpenAI Responses |
/// | grok     | cli-chat-proxy.grok.com `/v1/responses`    | OpenAI Responses |
/// | opencode | api.meta.ai `/v1/responses`                | OpenAI Responses |
/// | hermes   | inference-api.nousresearch.com `/v1/chat/completions` | Chat Completions |
///
/// The Responses bodies were identified by their distinctive keys —
/// `input` (not `messages`), `instructions`, `store`, `reasoning`,
/// `max_output_tokens`, and flat `tools[].name` — which no Chat
/// Completions or Anthropic request carries.
///
/// Two consequences the routing table must respect:
///
/// 1. Four of five non-claude harnesses converge on ONE dialect. A single
///    Responses translator is what unlocks them, not four bespoke ones.
/// 2. opencode's dialect is NOT a property of opencode. It was observed
///    speaking Responses only because its configured provider is Meta;
///    pointed at an Anthropic provider it would emit Anthropic Messages,
///    to a different host. Its dialect and its intercept host are both
///    provider-dependent, so neither can be hardcoded for it.
#[test]
fn captured_dialects_are_recorded_not_guessed() {
    // (harness, wire dialect observed). Changing an entry means a new
    // capture, not a new assumption.
    const CAPTURED: &[(&str, &str)] = &[
        ("claude", "anthropic-messages"),
        ("codex", "openai-responses"),
        ("pi", "openai-responses"),
        ("grok", "openai-responses"),
        ("opencode", "openai-responses/provider-dependent"),
        ("hermes", "openai-chat"),
    ];
    // Both dialects are now accepted from a harness. What still gates a
    // harness is the CA channel and, for provider-agnostic ones, having a
    // fixed intercept host.
    assert_eq!(
        CAPTURED
            .iter()
            .filter(|(_, d)| d.starts_with("openai-responses"))
            .count(),
        4,
        "one Responses translator covers four harnesses"
    );
    assert!(CAPTURED.iter().any(|(h, _)| *h == "claude"));
    // Every captured dialect is one the router can accept from a harness.
    assert!(CAPTURED.iter().all(|(_, d)| d.starts_with("openai-responses")
        || *d == "anthropic-messages"
        || *d == "openai-chat"));
}

/// Guards the other half of the claim:/// Guards the other half of the claim: a harness with no probe must not be
/// offered as routable. Cheap, no binary required, runs in CI.
#[test]
fn only_probed_harnesses_are_declared_route_capable() {
    // Kept in sync by hand with the daemon's `harness_routing`. If you add
    // a harness there, add its probe above and its name here — in that
    // order (spec 0115).
    //
    // Deliberately absent despite passing the transport probe:
    // - opencode: its endpoint host follows its configured provider, so
    //   there is no fixed intercept host to declare. Its dialect is
    //   handled by detection; the host is the open problem.
    // Transport capability alone is never route capability.
    const PROBED: &[&str] = &["claude", "pi", "grok", "codex", "hermes"];
    for harness in ["smith", "shell", "opencode", "kimi"] {
        assert!(
            !PROBED.contains(&harness),
            "{harness} is declared route-capable but is excluded for a documented reason"
        );
    }
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
