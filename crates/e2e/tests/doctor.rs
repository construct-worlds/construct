//! `construct doctor` end-to-end (spec 0168).
//!
//! These drive the real CLI as a subprocess and read its stdout, because
//! the properties worth protecting are properties of the *command* — its
//! exit code, and the fact that it changes nothing about the machine it is
//! diagnosing. Neither is observable over IPC.
//!
//! Deliberately not asserted: which harnesses are available (a CI runner
//! has no `claude`/`codex`, so any such assertion is flaky by
//! construction), the router/web-UI port findings (a developer's own
//! daemon may hold the default ports), and the exact wording of any
//! detail string.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().to_path_buf();
        for sub in ["config", "state", "data", "run"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self {
            _tmp: tmp,
            root,
            bin: construct_e2e::construct_bin_path()?,
        })
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/construct.sock")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config/config.toml")
    }

    /// Run `construct doctor --json` against this isolated home.
    fn doctor(&self) -> Result<(i32, Value)> {
        let mut cmd = Command::new(&self.bin);
        // The test may itself be running inside a construct session; an
        // inherited CONSTRUCT_* would point doctor at the developer's real
        // home instead of the fixture.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CONSTRUCT_") {
                cmd.env_remove(key);
            }
        }
        let out = cmd
            .env("CONSTRUCT_CONFIG_DIR", self.root.join("config"))
            .env("CONSTRUCT_STATE_DIR", self.root.join("state"))
            .env("CONSTRUCT_DATA_DIR", self.root.join("data"))
            .env("CONSTRUCT_RUNTIME_DIR", self.root.join("run"))
            .env("CONSTRUCT_NO_AUTOSTART", "1")
            .env("CONSTRUCT_NO_UPDATE_CHECK", "1")
            .args(["doctor", "--json"])
            .output()?;

        let code = out.status.code().unwrap_or(-1);
        let report = serde_json::from_slice(&out.stdout).map_err(|e| {
            anyhow::anyhow!(
                "doctor did not emit JSON ({e}); stdout={:?} stderr={:?}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })?;
        Ok((code, report))
    }
}

/// Every finding, flattened, so lookups don't care which section moved.
fn finding<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["sections"]
        .as_array()
        .expect("sections array")
        .iter()
        .flat_map(|s| s["findings"].as_array().expect("findings array"))
        .find(|f| f["id"] == id)
        .unwrap_or_else(|| panic!("no finding with id `{id}` in {report:#}"))
}

fn severity(report: &Value, id: &str) -> String {
    finding(report, id)["severity"]
        .as_str()
        .expect("severity string")
        .to_string()
}

#[test]
fn reports_a_missing_daemon_without_starting_one() -> Result<()> {
    let fx = Fixture::new()?;
    assert!(!fx.socket().exists(), "fixture starts with no socket");

    let (code, report) = fx.doctor()?;

    // A machine with no daemon running is not a broken machine.
    assert_eq!(
        code, 0,
        "doctor must exit 0 when the only finding is an absent daemon"
    );
    assert_eq!(severity(&report, "daemon.socket"), "warn");
    assert_eq!(report["summary"]["error"], 0);

    // The property that matters most: a diagnostic must not mutate the
    // system it is diagnosing.
    assert!(
        !fx.socket().exists(),
        "doctor started a daemon; it must never do that"
    );
    Ok(())
}

#[test]
fn checks_that_need_the_daemon_are_still_reported_as_skipped() -> Result<()> {
    let fx = Fixture::new()?;
    let (_, report) = fx.doctor()?;

    // Consumers key off finding ids, so an unrunnable check is still
    // emitted — as info, never as a missing key.
    for id in [
        "daemon.build_skew",
        "harnesses.env_skew",
        "features.degradation_observed",
    ] {
        assert_eq!(
            severity(&report, id),
            "info",
            "`{id}` must be present and informational when the daemon is down"
        );
    }
    Ok(())
}

#[test]
fn a_malformed_config_is_an_error_that_sets_the_exit_code() -> Result<()> {
    let fx = Fixture::new()?;
    std::fs::write(fx.config_file(), "[adapters\nbroken")?;

    let (code, report) = fx.doctor()?;

    assert_eq!(
        code, 1,
        "a config construct cannot parse must exit non-zero"
    );
    assert_eq!(severity(&report, "config.parse"), "error");
    assert!(
        finding(&report, "config.parse")["fix"].is_string(),
        "an error finding must say how to fix it"
    );

    // The fallback keeps the rest of the report useful rather than
    // aborting at the first bad line.
    assert!(
        finding(&report, "harnesses.summary")["detail"].is_string(),
        "later sections must still run on the built-in defaults"
    );
    Ok(())
}

#[test]
fn every_finding_carries_an_id_a_label_and_a_detail() -> Result<()> {
    let fx = Fixture::new()?;
    let (_, report) = fx.doctor()?;

    let sections = report["sections"].as_array().expect("sections array");
    assert!(!sections.is_empty());

    for f in sections
        .iter()
        .flat_map(|s| s["findings"].as_array().expect("findings array"))
    {
        for key in ["id", "label", "detail", "severity"] {
            let v = f[key].as_str().unwrap_or_else(|| {
                panic!("finding {f:#} is missing a string `{key}`");
            });
            assert!(!v.is_empty(), "finding {f:#} has an empty `{key}`");
        }
    }
    Ok(())
}

#[test]
fn a_live_daemon_is_reported_as_healthy_and_build_matched() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let daemon = rt.block_on(construct_e2e::Daemon::spawn())?;

    let mut cmd = Command::new(construct_e2e::construct_bin_path()?);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONSTRUCT_") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .env("CONSTRUCT_CONFIG_DIR", daemon.dir.path().join("config"))
        .env("CONSTRUCT_STATE_DIR", daemon.dir.path().join("state"))
        .env("CONSTRUCT_DATA_DIR", daemon.dir.path().join("data"))
        .env("CONSTRUCT_RUNTIME_DIR", daemon.dir.path().join("run"))
        .env("CONSTRUCT_NO_UPDATE_CHECK", "1")
        .args(["--socket"])
        .arg(&daemon.socket)
        .args(["doctor", "--json"])
        .output()?;

    let report: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        anyhow::anyhow!(
            "doctor did not emit JSON ({e}); stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        )
    })?;

    assert_eq!(severity(&report, "daemon.socket"), "ok");
    assert_eq!(
        severity(&report, "daemon.build_skew"),
        "ok",
        "the daemon this test spawned is the same binary the CLI ran from"
    );
    assert_eq!(out.status.code().unwrap_or(-1), 0);
    Ok(())
}

/// Guard the socket-path plumbing: `--socket` must reach the report rather
/// than doctor silently diagnosing the default location.
#[test]
fn an_explicit_socket_flag_is_the_one_reported() -> Result<()> {
    let fx = Fixture::new()?;
    let custom = fx.root.join("run/custom.sock");

    let mut cmd = Command::new(&fx.bin);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONSTRUCT_") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .env("CONSTRUCT_CONFIG_DIR", fx.root.join("config"))
        .env("CONSTRUCT_STATE_DIR", fx.root.join("state"))
        .env("CONSTRUCT_DATA_DIR", fx.root.join("data"))
        .env("CONSTRUCT_RUNTIME_DIR", fx.root.join("run"))
        .env("CONSTRUCT_NO_AUTOSTART", "1")
        .env("CONSTRUCT_NO_UPDATE_CHECK", "1")
        .args(["--socket"])
        .arg(&custom)
        .args(["doctor", "--json"])
        .output()?;

    let report: Value = serde_json::from_slice(&out.stdout)?;
    let detail = finding(&report, "daemon.socket")["detail"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        detail.contains("custom.sock"),
        "doctor reported a socket other than the one passed with --socket: {detail}"
    );
    Ok(())
}
