//! `construct doctor` — the diagnostic report.
//!
//! Doctor reports and never repairs (spec 0168). It must work when the
//! daemon is down — that is its primary use case — so every check here runs
//! in-process against the filesystem, the environment, and the same probes
//! the daemon itself uses. Facts that only a live daemon can supply are
//! passed in by the caller via [`DoctorInput`] rather than fetched here: the
//! daemon crate has no IPC client, and an IPC round trip would be
//! unreachable exactly when doctor is needed most.
//!
//! Checks reuse the daemon's real probes (`crate::availability`,
//! `crate::router::oauth`) rather than reimplementing them. A doctor that
//! probed differently from the daemon would be worse than no doctor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use construct_protocol::paths::Paths;
use serde::Serialize;

use crate::availability::{
    self, probe_harness, probe_smith, smith_auth_methods, Availability, AvailabilityCache,
    FeatureInputs, SmithAuthMethod,
};
use crate::config::Config;
use crate::router::oauth::{self, OauthProvider};

// ───────────────────────────── data model ─────────────────────────────

/// How bad a finding is. Declaration order is severity order (`Ord`).
///
/// Only [`Severity::Error`] sets a non-zero exit code, and it means
/// "construct cannot work correctly on this machine" — not "something is
/// unconfigured". A missing optional harness or an absent daemon is a
/// `Warn`; making either an `Error` would make `doctor` fail on healthy
/// machines and destroy the exit code's meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// One check result.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable machine key (`"paths.state_dir"`, `"logins.codex-oauth"`).
    /// Always emitted, even when the check could not run — consumers key
    /// off this and never have to handle a missing key.
    pub id: String,
    pub label: String,
    pub severity: Severity,
    /// One line. Never empty.
    pub detail: String,
    /// The exact command that fixes this, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Continuation lines (a migration block, per-method rows, a TOML
    /// caret diagram).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Finding {
    fn new(id: &str, label: &str, severity: Severity, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            severity,
            detail: detail.into(),
            fix: None,
            notes: Vec::new(),
        }
    }

    fn ok(id: &str, label: &str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Severity::Ok, detail)
    }

    fn info(id: &str, label: &str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Severity::Info, detail)
    }

    fn warn(id: &str, label: &str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Severity::Warn, detail)
    }

    fn error(id: &str, label: &str, detail: impl Into<String>) -> Self {
        Self::new(id, label, Severity::Error, detail)
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    fn with_fix_opt(mut self, fix: Option<String>) -> Self {
        self.fix = fix;
        self
    }

    fn with_notes(mut self, notes: Vec<String>) -> Self {
        self.notes = notes;
        self
    }

    /// A check that could not run because the daemon is down. Info, never
    /// a warning — the daemon being absent is already reported once, by
    /// `daemon.socket`.
    fn skipped(id: &str, label: &str) -> Self {
        Self::info(id, label, "skipped — daemon not running")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub id: String,
    pub title: String,
    pub findings: Vec<Finding>,
}

impl Section {
    fn new(id: &str, title: &str, findings: Vec<Finding>) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            findings,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub info: usize,
    pub warn: usize,
    pub error: usize,
    pub worst: Severity,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub sections: Vec<Section>,
    pub summary: Summary,
}

impl Report {
    /// The single place counts and `worst` are computed. Public so
    /// renderers can build a report from hand-assembled sections in tests.
    pub fn summarize(sections: Vec<Section>) -> Self {
        let mut summary = Summary {
            ok: 0,
            info: 0,
            warn: 0,
            error: 0,
            worst: Severity::Ok,
        };
        for f in sections.iter().flat_map(|s| s.findings.iter()) {
            match f.severity {
                Severity::Ok => summary.ok += 1,
                Severity::Info => summary.info += 1,
                Severity::Warn => summary.warn += 1,
                Severity::Error => summary.error += 1,
            }
            summary.worst = summary.worst.max(f.severity);
        }
        Self { sections, summary }
    }

    pub fn has_errors(&self) -> bool {
        self.summary.error > 0
    }
}

// ───────────────────────────── inputs ─────────────────────────────

/// Facts the CLI gathers before calling [`run`] — everything requiring an
/// IPC client or a CLI-crate constant.
#[derive(Debug, Clone)]
pub struct DoctorInput {
    pub client_version: String,
    pub client_build_id: String,
    pub socket: PathBuf,
    /// The socket came from `--socket`, so path-derived advice about the
    /// default location would be misleading.
    pub socket_overridden: bool,
    pub daemon: DaemonProbe,
    pub update: UpdateProbe,
}

#[derive(Debug, Clone)]
pub enum DaemonProbe {
    /// Nothing is listening. `socket_file_present` distinguishes a stale
    /// socket file left by a dead daemon from no socket at all.
    Down { socket_file_present: bool },
    /// The socket accepted a connection but the RPC failed. This is the
    /// only daemon state that is an `Error`.
    Unresponsive { error: String },
    Up {
        build_id: Option<String>,
        build_skew: bool,
        harnesses: Vec<DaemonHarness>,
        features: Option<DaemonFeatures>,
    },
}

#[derive(Debug, Clone)]
pub struct DaemonHarness {
    pub name: String,
    pub available: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DaemonFeature {
    pub label: String,
    pub status: construct_protocol::FeatureStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct DaemonFeatures {
    pub features: Vec<DaemonFeature>,
    pub degradation_observed: bool,
}

#[derive(Debug, Clone)]
pub enum UpdateProbe {
    /// `CONSTRUCT_NO_UPDATE_CHECK=1`.
    Disabled,
    NoCache,
    Cached {
        latest: Option<String>,
        age: Option<std::time::Duration>,
        newer: bool,
    },
}

// ───────────────────────────── entry point ─────────────────────────────

/// Build the full report. Never fails: every failure is a [`Finding`].
pub async fn run(input: DoctorInput) -> Report {
    let paths = Paths::discover();
    let cache = Mutex::new(AvailabilityCache::default());

    let (config_section, cfg) = config_section(&paths);

    let sections = vec![
        environment_section(&input),
        paths_section(&paths),
        config_section,
        daemon_section(&input, &paths, &cfg),
        harnesses_section(&input, &cfg, &cache).await,
        logins_section(&cfg, &cache).await,
        features_section(&input, &cfg, &cache).await,
    ];

    Report::summarize(sections)
}

// ───────────────────────────── environment ─────────────────────────────

fn environment_section(input: &DoctorInput) -> Section {
    let mut findings = vec![Finding::ok(
        "env.version",
        "version",
        format!("{} (build {})", input.client_version, input.client_build_id),
    )];

    let current_exe = std::env::current_exe().ok();
    findings.push(match &current_exe {
        Some(p) => Finding::ok("env.binary", "binary", p.display().to_string()),
        None => Finding::warn(
            "env.binary",
            "binary",
            "could not resolve the running executable's path",
        ),
    });

    findings.push(classify_path_shadowing(
        &construct_on_path(),
        current_exe.as_deref(),
    ));
    findings.push(update_finding(&input.update));
    findings.push(overrides_finding());

    Section::new("environment", "environment", findings)
}

/// Every distinct `construct` on PATH, in PATH order, as
/// `(path-as-found, canonical)`.
///
/// `which_all_global` rather than `which_all`: the latter passes the
/// current directory, so a `./construct` in the user's project would be
/// reported as a PATH entry. `which` performs no deduplication of its own,
/// so a duplicated PATH entry or a symlink into a shared install would
/// otherwise read as a false shadow.
fn construct_on_path() -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let Ok(found) = which::which_all_global("construct") else {
        return out;
    };
    for path in found {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if out.iter().any(|(_, c)| *c == canonical) {
            continue;
        }
        out.push((path, canonical));
    }
    out
}

/// Pure: classify PATH resolution given the discovered entries and the
/// running executable. A shadow is a foot-gun, not a breakage — never an
/// error.
fn classify_path_shadowing(entries: &[(PathBuf, PathBuf)], current_exe: Option<&Path>) -> Finding {
    let running = current_exe.and_then(|p| std::fs::canonicalize(p).ok().or(Some(p.to_path_buf())));

    match entries {
        [] => {
            let fix = current_exe.and_then(|p| {
                p.parent()
                    .map(|d| format!("export PATH=\"{}:$PATH\"", d.display()))
            });
            Finding::warn(
                "env.path",
                "PATH",
                "`construct` is not on PATH; only the binary you invoked directly will work",
            )
            .with_fix_opt(fix)
        }
        [(found, canonical)] => {
            if running.as_deref().is_none_or(|r| r == canonical) {
                Finding::ok("env.path", "PATH", found.display().to_string())
            } else {
                Finding::warn(
                    "env.path",
                    "PATH",
                    format!(
                        "you ran {}, but PATH resolves `construct` to {}",
                        running.as_deref().unwrap_or(Path::new("?")).display(),
                        found.display()
                    ),
                )
            }
        }
        many => {
            let notes = many
                .iter()
                .enumerate()
                .map(|(i, (found, canonical))| {
                    let marker = if running.as_deref() == Some(canonical.as_path()) {
                        "  (in use)"
                    } else {
                        ""
                    };
                    format!("{}. {}{}", i + 1, found.display(), marker)
                })
                .collect();
            Finding::warn(
                "env.path",
                "PATH",
                format!(
                    "{} `construct` binaries on PATH; the first one wins",
                    many.len()
                ),
            )
            .with_notes(notes)
            .with_fix(format!("rm '{}'", many[1].0.display()))
        }
    }
}

fn update_finding(update: &UpdateProbe) -> Finding {
    match update {
        UpdateProbe::Disabled => Finding::info(
            "env.update",
            "update check",
            "disabled via CONSTRUCT_NO_UPDATE_CHECK",
        ),
        UpdateProbe::NoCache => Finding::info(
            "env.update",
            "update check",
            "no cached result yet (doctor never checks online)",
        ),
        UpdateProbe::Cached { latest, age, newer } => {
            let when = age.map(format_age).unwrap_or_else(|| "unknown".to_string());
            match (latest, newer) {
                (Some(v), true) => Finding::warn(
                    "env.update",
                    "update check",
                    format!("{v} is available (cached {when} ago)"),
                )
                .with_fix("construct upgrade"),
                (Some(v), false) => Finding::ok(
                    "env.update",
                    "update check",
                    format!("up to date (latest {v}, cached {when} ago)"),
                ),
                (None, _) => Finding::info(
                    "env.update",
                    "update check",
                    "no cached result yet (doctor never checks online)",
                ),
            }
        }
    }
}

/// Which `CONSTRUCT_*` / `XDG_*` variables are redirecting this install.
/// Purely informational, but it is the first thing to check when someone
/// reports "my sessions vanished".
fn overrides_finding() -> Finding {
    const VARS: &[&str] = &[
        "CONSTRUCT_HOME",
        "CONSTRUCT_CONFIG_DIR",
        "CONSTRUCT_STATE_DIR",
        "CONSTRUCT_DATA_DIR",
        "CONSTRUCT_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ];
    let set: Vec<String> = VARS
        .iter()
        .filter_map(|v| std::env::var(v).ok().map(|val| format!("{v}={val}")))
        .collect();
    if set.is_empty() {
        Finding::ok("env.overrides", "path overrides", "none set")
    } else {
        Finding::info(
            "env.overrides",
            "path overrides",
            format!("{} set", set.len()),
        )
        .with_notes(set)
    }
}

// ───────────────────────────── paths ─────────────────────────────

fn paths_section(paths: &Paths) -> Section {
    let dirs: [(&str, &str, &Path); 4] = [
        ("paths.config_dir", "config", &paths.config_dir),
        ("paths.state_dir", "state", &paths.state_dir),
        ("paths.data_dir", "data", &paths.data_dir),
        ("paths.runtime_dir", "runtime", &paths.runtime_dir),
    ];

    let mut findings: Vec<Finding> = dirs
        .iter()
        .map(|(id, label, path)| dir_finding(id, label, path))
        .collect();

    let config_file = paths.config_file();
    findings.push(if config_file.exists() {
        Finding::ok(
            "paths.config_file",
            "config file",
            config_file.display().to_string(),
        )
    } else {
        Finding::info(
            "paths.config_file",
            "config file",
            "absent — built-in defaults in use",
        )
        .with_fix(format!(
            "construct daemon default-config > {}",
            crate::shell_quote(&config_file)
        ))
    });

    let sessions_root = paths.sessions_root();
    let count = std::fs::read_dir(&sessions_root)
        .map(|d| d.flatten().count())
        .unwrap_or(0);
    findings.push(Finding::info(
        "paths.sessions_root",
        "sessions",
        format!("{count} on disk at {}", sessions_root.display()),
    ));

    findings.push(match crate::legacy_migration_notice(paths) {
        None => Finding::ok(
            "paths.legacy",
            "legacy layout",
            "no pre-rename `agentd` directories",
        ),
        Some(notice) => {
            let mut lines = notice.lines().filter(|l| !l.trim().is_empty());
            let head = lines
                .next()
                .unwrap_or("legacy `agentd` directories found")
                .trim()
                .to_string();
            Finding::warn("paths.legacy", "legacy layout", head)
                .with_notes(lines.map(|l| l.trim_end().to_string()).collect())
        }
    });

    Section::new("paths", "paths", findings)
}

fn dir_finding(id: &str, label: &str, path: &Path) -> Finding {
    let exists = path.exists();
    let is_dir = path.is_dir();
    let writable = is_writable(path);
    let parent_writable = path.parent().is_none_or(is_writable);

    let (severity, detail, fix) = classify_dir(exists, is_dir, writable, parent_writable);
    let detail = format!("{} — {detail}", path.display());
    let fix = fix.map(|f| match f {
        DirFix::Create => format!("mkdir -p {}", crate::shell_quote(path)),
        DirFix::Chmod => format!("chmod u+w {}", crate::shell_quote(path)),
        DirFix::Remove => format!("rm {}", crate::shell_quote(path)),
    });
    Finding::new(id, label, severity, detail).with_fix_opt(fix)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirFix {
    Create,
    Chmod,
    Remove,
}

/// Pure: the five outcomes for a construct-owned directory.
fn classify_dir(
    exists: bool,
    is_dir: bool,
    writable: bool,
    parent_writable: bool,
) -> (Severity, &'static str, Option<DirFix>) {
    match (exists, is_dir, writable, parent_writable) {
        (false, _, _, true) => (
            Severity::Info,
            "absent; will be created when the daemon starts",
            None,
        ),
        (false, _, _, false) => (
            Severity::Error,
            "absent, and its parent is not writable",
            Some(DirFix::Create),
        ),
        (true, false, _, _) => (
            Severity::Error,
            "exists but is not a directory",
            Some(DirFix::Remove),
        ),
        (true, true, false, _) => (Severity::Error, "not writable", Some(DirFix::Chmod)),
        (true, true, true, _) => (Severity::Ok, "writable", None),
    }
}

/// `access(2)` with `W_OK` — a read-only syscall, no probe file left behind.
fn is_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated C string for the duration of
    // the call; `access` only reads it.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

// ───────────────────────────── config ─────────────────────────────

/// Returns the section plus the `Config` the remaining sections should use.
/// On a parse failure that is the built-in default set, so harness / login /
/// feature checks still run and still say something useful.
fn config_section(paths: &Paths) -> (Section, Config) {
    let config_file = paths.config_file();
    let (parse_finding, cfg) = match Config::load_or_default(paths) {
        Ok(cfg) => {
            let detail = format!(
                "{} adapters, {} smith model profile(s)",
                cfg.adapters.len(),
                cfg.smith.models.len()
            );
            (Finding::ok("config.parse", "config", detail), cfg)
        }
        Err(e) => {
            let rendered = format!("{e:#}");
            let mut lines = rendered.lines();
            let head = lines
                .next()
                .unwrap_or("config.toml could not be parsed")
                .to_string();
            let notes: Vec<String> = lines
                .map(|l| l.trim_end().to_string())
                .chain(std::iter::once(
                    "later checks use built-in defaults; your config.toml was not applied".into(),
                ))
                .collect();
            let mut fallback = Config::default();
            crate::config::merge_builtin_adapters(&mut fallback);
            (
                Finding::error("config.parse", "config", head)
                    .with_notes(notes)
                    .with_fix(format!(
                        "${{EDITOR:-vi}} {}",
                        crate::shell_quote(&config_file)
                    )),
                fallback,
            )
        }
    };

    // Doctor runs with no daemon, so it must build the same environment the
    // daemon would from the same config before probing any credential
    // (spec 0180) — otherwise it reports "no API key" for a provider that
    // works, which is worse than not checking. On a parse failure this
    // installs nothing, matching the fallback config it just chose.
    crate::daemon_env::install(cfg.daemon.env.clone());

    let orchestrator = match cfg.orchestrator.effective_harness() {
        Some(h) => Finding::info("config.orchestrator", "operator", format!("enabled ({h})")),
        None => Finding::info("config.orchestrator", "operator", "disabled"),
    };

    let router = Finding::info(
        "config.router",
        "router",
        format!(
            "enabled={} publish_models={}",
            cfg.router.enabled, cfg.router.publish_models
        ),
    );

    (
        Section::new(
            "config",
            "config",
            vec![parse_finding, orchestrator, router],
        ),
        cfg,
    )
}

// ───────────────────────────── daemon ─────────────────────────────

fn daemon_section(input: &DoctorInput, paths: &Paths, cfg: &Config) -> Section {
    let socket = input.socket.display().to_string();
    let socket_finding = match &input.daemon {
        DaemonProbe::Up { .. } => {
            Finding::ok("daemon.socket", "socket", format!("{socket} (live)"))
        }
        DaemonProbe::Down {
            socket_file_present: true,
        } => Finding::warn(
            "daemon.socket",
            "socket",
            format!("stale socket file at {socket}; nothing is listening"),
        )
        .with_fix("construct daemon start"),
        DaemonProbe::Down {
            socket_file_present: false,
        } => Finding::warn(
            "daemon.socket",
            "socket",
            format!("no daemon running ({socket})"),
        )
        .with_fix("construct daemon start"),
        DaemonProbe::Unresponsive { error } => Finding::error(
            "daemon.socket",
            "socket",
            format!("{socket} accepts connections but `ping` failed: {error}"),
        )
        .with_fix("construct daemon restart"),
    };

    let daemon_up = matches!(input.daemon, DaemonProbe::Up { .. });

    let skew_finding = match &input.daemon {
        DaemonProbe::Up {
            build_id,
            build_skew,
            ..
        } => {
            if *build_skew {
                Finding::warn(
                    "daemon.build_skew",
                    "build skew",
                    format!(
                        "client {} vs daemon {}",
                        input.client_build_id,
                        build_id.as_deref().unwrap_or("unknown")
                    ),
                )
                .with_fix("construct daemon restart")
            } else {
                Finding::ok("daemon.build_skew", "build skew", "client matches daemon")
            }
        }
        _ => Finding::skipped("daemon.build_skew", "build skew"),
    };

    let log_file = paths.log_file();
    let log_finding = match std::fs::metadata(&log_file) {
        Ok(m) => Finding::info(
            "daemon.log",
            "daemon log",
            format!("{} ({})", log_file.display(), format_bytes(m.len())),
        )
        .with_fix(format!("tail -f {}", crate::shell_quote(&log_file))),
        Err(_) => Finding::info("daemon.log", "daemon log", "none yet"),
    };

    let router_port = construct_protocol::paths::preferred_router_port(paths, cfg.router.port);
    let router_finding = port_finding(
        "daemon.router_port",
        "router port",
        cfg.router.enabled,
        daemon_up,
        router_port,
    );

    let webui_port = construct_protocol::paths::read_persisted_port(&paths.webui_port_file())
        .unwrap_or(construct_protocol::paths::DEFAULT_WEBUI_PORT);
    let webui_finding = port_finding(
        "daemon.webui_port",
        "web UI port",
        true,
        daemon_up,
        webui_port,
    );

    Section::new(
        "daemon",
        "daemon",
        vec![
            socket_finding,
            skew_finding,
            log_finding,
            router_finding,
            webui_finding,
        ],
    )
}

fn port_finding(id: &str, label: &str, enabled: bool, daemon_up: bool, port: u16) -> Finding {
    let open = enabled && port_open(port);
    let (severity, detail) = classify_port(enabled, daemon_up, open, port);
    let finding = Finding::new(id, label, severity, detail);
    match severity {
        Severity::Warn if daemon_up => finding.with_fix("construct daemon restart"),
        Severity::Warn => finding.with_fix(format!("lsof -nP -iTCP:{port} -sTCP:LISTEN")),
        _ => finding,
    }
}

/// Pure: the port truth table.
///
/// The non-obvious row is "daemon down but the port is occupied" — that is
/// a warning, because the next daemon start will collide with whatever is
/// squatting there.
fn classify_port(enabled: bool, daemon_up: bool, open: bool, port: u16) -> (Severity, String) {
    match (enabled, daemon_up, open) {
        (false, _, _) => (Severity::Info, "disabled in config".to_string()),
        (true, true, true) => (Severity::Ok, format!("listening on 127.0.0.1:{port}")),
        (true, true, false) => (
            Severity::Warn,
            format!("enabled, but nothing is listening on 127.0.0.1:{port}"),
        ),
        (true, false, true) => (
            Severity::Warn,
            format!("port {port} is already in use while no construct daemon is running"),
        ),
        (true, false, false) => (
            Severity::Info,
            format!("port {port} free (daemon not running)"),
        ),
    }
}

/// Loopback-only reachability probe. Not a network call.
fn port_open(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100)).is_ok()
}

// ───────────────────────────── harnesses ─────────────────────────────

/// Install hints per built-in harness. The generic arm is derived
/// mechanically from the same env names `probe_wrapper_cli` consults.
const HARNESS_FIX: &[(&str, &str)] = &[
    ("claude", "npm i -g @anthropic-ai/claude-code"),
    ("codex", "npm i -g @openai/codex"),
    ("opencode", "curl -fsSL https://opencode.ai/install | bash"),
    ("antigravity", "install Google Antigravity, then ensure `agy` is on PATH"),
    ("agy", "install Google Antigravity, then ensure `agy` is on PATH"),
    ("grok", "npm i -g @vibe-kit/grok-cli"),
    ("kimi", "curl -fsSL https://kimi.com/code/install.sh | bash"),
    ("hermes", "install the Hermes agent, then ensure `hermes` is on PATH"),
    ("pi", "install the pi coding agent, then ensure `pi` is on PATH"),
    ("muse", "install Muse Code and run `muse login`, then ensure `muse` is on PATH"),
    (
        "smith",
        "give smith a credential: set ANTHROPIC_API_KEY or OPENAI_API_KEY, or run `claude` to sign in",
    ),
];

fn harness_fix(name: &str) -> String {
    HARNESS_FIX
        .iter()
        .find(|(h, _)| *h == name)
        .map(|(_, fix)| (*fix).to_string())
        .unwrap_or_else(|| {
            format!(
                "install the `{name}` CLI, or set CONSTRUCT_{}_BIN=/path/to/{name} in the daemon's environment",
                name.to_uppercase().replace('-', "_")
            )
        })
}

/// Probe every configured adapter locally, using the daemon's own ladder.
async fn probe_all(
    cfg: &Config,
    cache: &Mutex<AvailabilityCache>,
) -> BTreeMap<String, Availability> {
    let mut out = BTreeMap::new();
    for (name, acfg) in cfg.adapters.iter() {
        let spec = acfg.binary.clone().unwrap_or_else(|| name.clone());
        let resolved = crate::adapter::locate_binary(&spec);
        let avail = probe_harness(cache, name, &spec, resolved.as_deref()).await;
        out.insert(name.clone(), avail);
    }
    out
}

async fn harnesses_section(
    input: &DoctorInput,
    cfg: &Config,
    cache: &Mutex<AvailabilityCache>,
) -> Section {
    let local = probe_all(cfg, cache).await;

    // The daemon is the source of truth when it is up: its PATH and env are
    // what `session.create` will actually use, which is not necessarily the
    // shell doctor is running in.
    let (rows, source): (Vec<(String, bool, String)>, &str) = match &input.daemon {
        DaemonProbe::Up { harnesses, .. } if !harnesses.is_empty() => (
            harnesses
                .iter()
                .map(|h| {
                    (
                        h.name.clone(),
                        h.available,
                        h.detail.clone().unwrap_or_else(|| "unknown".to_string()),
                    )
                })
                .collect(),
            "daemon",
        ),
        _ => (
            local
                .iter()
                .map(|(name, a)| (name.clone(), a.available, a.detail.clone()))
                .collect(),
            "local probe — daemon not running",
        ),
    };

    let total = rows.len();
    let available = rows.iter().filter(|(_, ok, _)| *ok).count();
    let summary_sev = if available == total {
        Severity::Ok
    } else {
        Severity::Warn
    };
    let mut findings = vec![Finding::new(
        "harnesses.summary",
        "available",
        summary_sev,
        format!("{available}/{total} available ({source})"),
    )];

    for (name, ok, detail) in &rows {
        let id = format!("harnesses.{name}");
        findings.push(if *ok {
            Finding::ok(&id, name, detail.clone())
        } else {
            Finding::warn(&id, name, detail.clone()).with_fix(harness_fix(name))
        });
    }

    // "`claude` works in my terminal but construct says it's missing" is
    // the most confusing failure in this whole surface, and it happens
    // whenever the daemon was started from a shell with a different PATH.
    findings.push(match &input.daemon {
        DaemonProbe::Up { harnesses, .. } if !harnesses.is_empty() => {
            let disagreements: Vec<String> = harnesses
                .iter()
                .filter_map(|h| {
                    let local_avail = local.get(&h.name)?;
                    (local_avail.available != h.available).then(|| {
                        format!(
                            "{}: daemon says {}, this shell says {}",
                            h.name,
                            if h.available { "available" } else { "missing" },
                            if local_avail.available {
                                "available"
                            } else {
                                "missing"
                            }
                        )
                    })
                })
                .collect();
            if disagreements.is_empty() {
                Finding::ok(
                    "harnesses.env_skew",
                    "environment",
                    "the daemon and this shell agree about what is installed",
                )
            } else {
                Finding::warn(
                    "harnesses.env_skew",
                    "environment",
                    format!(
                        "the daemon's PATH differs from this shell's for {} harness(es)",
                        disagreements.len()
                    ),
                )
                .with_notes(disagreements)
                .with_fix("construct daemon restart   # from a shell with the right PATH")
            }
        }
        _ => Finding::skipped("harnesses.env_skew", "environment"),
    });

    Section::new("harnesses", "harnesses", findings)
}

// ───────────────────────────── logins ─────────────────────────────

async fn logins_section(cfg: &Config, cache: &Mutex<AvailabilityCache>) -> Section {
    let mut findings: Vec<Finding> = OauthProvider::ALL
        .iter()
        .map(|p| login_finding(p.name(), oauth::check_login(*p).err()))
        .collect();

    let methods = smith_auth_methods(cache).await;
    findings.push(smith_auth_finding(cfg, &methods));

    Section::new("logins", "logins", findings)
}

/// Pure: render one provider's login state.
///
/// `LoginBlocker::reason` already carries the exact guidance ("… has
/// expired; run `claude` once to renew it"), so it is rendered verbatim
/// rather than reworded here.
fn login_finding(name: &str, blocker: Option<oauth::LoginBlocker>) -> Finding {
    let id = format!("logins.{name}");
    match blocker {
        None => Finding::ok(&id, name, "logged in"),
        Some(b) => Finding::warn(&id, name, b.reason).with_fix_opt(b.login_command),
    }
}

fn smith_auth_finding(cfg: &Config, methods: &[SmithAuthMethod]) -> Finding {
    let pinned = cfg
        .adapters
        .get("smith")
        .and_then(|a| a.env.get("CONSTRUCT_SMITH_MODEL"))
        .map(String::as_str);
    let current = availability::current_smith_auth_method(pinned, methods);

    let notes: Vec<String> = methods
        .iter()
        .map(|m| {
            let mark = if m.available { "ok" } else { "--" };
            let current_marker = if current.as_deref() == Some(m.id) {
                "  <- current"
            } else {
                ""
            };
            format!("{:<22} [{mark}]  {}{current_marker}", m.label, m.detail)
        })
        .collect();

    let available = methods.iter().filter(|m| m.available).count();
    if available == 0 {
        Finding::warn(
            "logins.smith_auth",
            "smith auth",
            "no credential available — smith sessions and ambient features will not work",
        )
        .with_notes(notes)
        .with_fix("set ANTHROPIC_API_KEY or OPENAI_API_KEY, or run `claude` to sign in")
    } else {
        Finding::ok(
            "logins.smith_auth",
            "smith auth",
            format!("{available} credential method(s) available"),
        )
        .with_notes(notes)
    }
}

// ───────────────────────────── features ─────────────────────────────

async fn features_section(
    input: &DoctorInput,
    cfg: &Config,
    cache: &Mutex<AvailabilityCache>,
) -> Section {
    let (rows, degradation) = match &input.daemon {
        DaemonProbe::Up {
            features: Some(f), ..
        } => (
            f.features
                .iter()
                .map(|x| (x.label.clone(), x.status, x.detail.clone()))
                .collect::<Vec<_>>(),
            Some(f.degradation_observed),
        ),
        _ => {
            let smith = probe_smith(cache).await;
            let orchestrator = match cfg.orchestrator.effective_harness() {
                None => None,
                Some(name) => {
                    let avail = if name == "smith" {
                        smith.clone()
                    } else {
                        let spec = cfg
                            .adapters
                            .get(name)
                            .and_then(|c| c.binary.clone())
                            .unwrap_or_else(|| name.to_string());
                        let resolved = crate::adapter::locate_binary(&spec);
                        probe_harness(cache, name, &spec, resolved.as_deref()).await
                    };
                    Some((name.to_string(), avail))
                }
            };
            let features = availability::ambient_features(&FeatureInputs {
                smith,
                title_gen: availability::smith_title_gen_available(),
                suggest_enabled: cfg.suggest.enabled,
                orchestrator,
            });
            (
                features
                    .into_iter()
                    .map(|f| (f.label, f.status, f.detail))
                    .collect(),
                None,
            )
        }
    };

    const DEGRADED_FIX: &str =
        "give smith a credential (ANTHROPIC_API_KEY / OPENAI_API_KEY, or run `claude`), then `construct daemon restart`";

    let mut findings: Vec<Finding> = rows
        .into_iter()
        .map(|(label, status, detail)| {
            let id = format!("features.{}", label.to_lowercase().replace([' ', '-'], "_"));
            let severity = feature_severity(status);
            let f = Finding::new(&id, &label, severity, detail);
            if severity == Severity::Warn {
                f.with_fix(DEGRADED_FIX)
            } else {
                f
            }
        })
        .collect();

    findings.push(match degradation {
        None => Finding::skipped("features.degradation_observed", "degradation"),
        Some(true) => Finding::warn(
            "features.degradation_observed",
            "degradation",
            "an ambient feature has already skipped work during this daemon run",
        )
        .with_fix(DEGRADED_FIX),
        Some(false) => Finding::ok(
            "features.degradation_observed",
            "degradation",
            "no ambient feature has skipped work",
        ),
    });

    Section::new("features", "ambient features", findings)
}

fn feature_severity(status: construct_protocol::FeatureStatus) -> Severity {
    use construct_protocol::FeatureStatus as S;
    match status {
        S::Ok => Severity::Ok,
        S::Degraded => Severity::Warn,
        S::Off => Severity::Info,
    }
}

// ───────────────────────────── helpers ─────────────────────────────

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn format_age(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 90 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 48 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // ── classify_dir ──

    #[test]
    fn absent_directory_with_a_writable_parent_is_informational() {
        let (sev, _, fix) = classify_dir(false, false, false, true);
        assert_eq!(sev, Severity::Info);
        assert_eq!(fix, None);
    }

    #[test]
    fn absent_directory_with_an_unwritable_parent_is_an_error() {
        let (sev, _, fix) = classify_dir(false, false, false, false);
        assert_eq!(sev, Severity::Error);
        assert_eq!(fix, Some(DirFix::Create));
    }

    #[test]
    fn a_file_where_a_directory_belongs_is_an_error() {
        let (sev, _, fix) = classify_dir(true, false, true, true);
        assert_eq!(sev, Severity::Error);
        assert_eq!(fix, Some(DirFix::Remove));
    }

    #[test]
    fn an_unwritable_directory_is_an_error() {
        let (sev, _, fix) = classify_dir(true, true, false, true);
        assert_eq!(sev, Severity::Error);
        assert_eq!(fix, Some(DirFix::Chmod));
    }

    #[test]
    fn a_writable_directory_is_ok() {
        let (sev, _, fix) = classify_dir(true, true, true, true);
        assert_eq!(sev, Severity::Ok);
        assert_eq!(fix, None);
    }

    // ── classify_path_shadowing ──

    #[test]
    fn no_construct_on_path_warns_with_an_export_fix() {
        let f = classify_path_shadowing(&[], Some(Path::new("/opt/c/bin/construct")));
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.fix.as_deref(), Some("export PATH=\"/opt/c/bin:$PATH\""));
    }

    #[test]
    fn a_single_matching_entry_is_ok() {
        let exe = p("/usr/local/bin/construct");
        let f = classify_path_shadowing(&[(exe.clone(), exe.clone())], Some(&exe));
        assert_eq!(f.severity, Severity::Ok);
    }

    #[test]
    fn a_single_entry_that_is_not_what_we_ran_warns() {
        let f = classify_path_shadowing(
            &[(
                p("/opt/homebrew/bin/construct"),
                p("/opt/homebrew/bin/construct"),
            )],
            Some(Path::new("/tmp/build/construct")),
        );
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.detail.contains("PATH resolves"));
    }

    #[test]
    fn multiple_entries_warn_and_list_every_one_marking_the_live_binary() {
        let first = p("/usr/local/bin/construct");
        let second = p("/opt/homebrew/bin/construct");
        let f = classify_path_shadowing(
            &[
                (first.clone(), first.clone()),
                (second.clone(), second.clone()),
            ],
            Some(&first),
        );
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.notes.len(), 2);
        assert!(f.notes[0].contains("(in use)"), "{:?}", f.notes);
        assert!(!f.notes[1].contains("(in use)"));
        assert_eq!(
            f.fix.as_deref(),
            Some("rm '/opt/homebrew/bin/construct'"),
            "the fix removes the shadowed copy, not the live one"
        );
    }

    // ── classify_port ──

    #[test]
    fn a_disabled_port_is_informational_whatever_else_is_true() {
        for daemon_up in [true, false] {
            for open in [true, false] {
                assert_eq!(
                    classify_port(false, daemon_up, open, 8917).0,
                    Severity::Info
                );
            }
        }
    }

    #[test]
    fn an_enabled_port_is_ok_only_when_the_daemon_is_up_and_listening() {
        assert_eq!(classify_port(true, true, true, 8917).0, Severity::Ok);
    }

    #[test]
    fn an_enabled_port_with_a_live_daemon_and_nothing_listening_warns() {
        assert_eq!(classify_port(true, true, false, 8917).0, Severity::Warn);
    }

    #[test]
    fn a_port_occupied_while_no_daemon_runs_warns_about_the_squatter() {
        let (sev, detail) = classify_port(true, false, true, 8917);
        assert_eq!(sev, Severity::Warn);
        assert!(detail.contains("already in use"), "{detail}");
    }

    #[test]
    fn a_free_port_with_no_daemon_is_informational() {
        assert_eq!(classify_port(true, false, false, 8917).0, Severity::Info);
    }

    // ── login_finding ──

    #[test]
    fn a_valid_login_is_ok_with_no_fix() {
        let f = login_finding("claude-oauth", None);
        assert_eq!(f.severity, Severity::Ok);
        assert_eq!(f.fix, None);
    }

    #[test]
    fn a_blocked_login_renders_the_blockers_reason_verbatim() {
        let reason = "codex-oauth login has expired; run `codex` once to renew it";
        let f = login_finding(
            "codex-oauth",
            Some(oauth::LoginBlocker {
                reason: reason.into(),
                login_command: Some("codex login".into()),
            }),
        );
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(
            f.detail, reason,
            "the blocker's own wording already tells the user what to do"
        );
        assert_eq!(f.fix.as_deref(), Some("codex login"));
    }

    // ── feature_severity ──

    #[test]
    fn feature_status_maps_degraded_to_warn_and_off_to_info() {
        use construct_protocol::FeatureStatus as S;
        assert_eq!(feature_severity(S::Ok), Severity::Ok);
        assert_eq!(feature_severity(S::Degraded), Severity::Warn);
        assert_eq!(feature_severity(S::Off), Severity::Info);
    }

    // ── summarize ──

    #[test]
    fn an_empty_report_is_ok_with_zero_counts() {
        let r = Report::summarize(vec![]);
        assert_eq!(r.summary.worst, Severity::Ok);
        assert_eq!(r.summary.error, 0);
        assert!(!r.has_errors());
    }

    #[test]
    fn summarize_counts_each_severity_and_reports_the_worst() {
        let r = Report::summarize(vec![Section::new(
            "s",
            "s",
            vec![
                Finding::ok("a", "a", "d"),
                Finding::info("b", "b", "d"),
                Finding::warn("c", "c", "d"),
                Finding::warn("d", "d", "d"),
                Finding::error("e", "e", "d"),
            ],
        )]);
        assert_eq!(
            (
                r.summary.ok,
                r.summary.info,
                r.summary.warn,
                r.summary.error
            ),
            (1, 1, 2, 1)
        );
        assert_eq!(r.summary.worst, Severity::Error);
        assert!(r.has_errors());
    }

    #[test]
    fn warnings_alone_never_make_the_report_fail() {
        let r = Report::summarize(vec![Section::new(
            "s",
            "s",
            vec![Finding::warn("a", "a", "d"), Finding::warn("b", "b", "d")],
        )]);
        assert_eq!(r.summary.worst, Severity::Warn);
        assert!(
            !r.has_errors(),
            "a machine with no daemon and a missing optional harness is healthy"
        );
    }

    // ── config fallback ──

    #[test]
    fn a_malformed_config_is_an_error_but_still_yields_the_builtin_adapters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "[adapters\nbroken").unwrap();
        let paths = Paths {
            config_dir: tmp.path().to_path_buf(),
            state_dir: tmp.path().to_path_buf(),
            data_dir: tmp.path().to_path_buf(),
            runtime_dir: tmp.path().to_path_buf(),
        };

        let (section, cfg) = config_section(&paths);
        let parse = &section.findings[0];
        assert_eq!(parse.id, "config.parse");
        assert_eq!(parse.severity, Severity::Error);
        assert!(
            parse.fix.is_some(),
            "a broken config must say how to edit it"
        );

        for name in ["shell", "claude", "codex", "smith"] {
            assert!(
                cfg.adapters.contains_key(name),
                "fallback config lost the built-in `{name}` adapter"
            );
        }
    }

    #[test]
    fn a_valid_config_parses_and_reports_its_adapter_count() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().to_path_buf(),
            state_dir: tmp.path().to_path_buf(),
            data_dir: tmp.path().to_path_buf(),
            runtime_dir: tmp.path().to_path_buf(),
        };
        let (section, cfg) = config_section(&paths);
        assert_eq!(section.findings[0].severity, Severity::Ok);
        assert!(!cfg.adapters.is_empty());
    }

    // ── legacy layout ──

    #[test]
    fn a_clean_tree_reports_no_legacy_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
            data_dir: tmp.path().join("data"),
            runtime_dir: tmp.path().join("run"),
        };
        let legacy = Paths {
            config_dir: tmp.path().join("legacy/config"),
            state_dir: tmp.path().join("legacy/state"),
            data_dir: tmp.path().join("legacy/data"),
            runtime_dir: tmp.path().join("legacy/run"),
        };
        assert!(crate::legacy_migration_notice_with_paths(&paths, &legacy).is_none());
    }

    #[test]
    fn planted_legacy_directories_produce_a_migration_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: tmp.path().join("config"),
            state_dir: tmp.path().join("state"),
            data_dir: tmp.path().join("data"),
            runtime_dir: tmp.path().join("run"),
        };
        let legacy = Paths {
            config_dir: tmp.path().join("legacy/config"),
            state_dir: tmp.path().join("legacy/state"),
            data_dir: tmp.path().join("legacy/data"),
            runtime_dir: tmp.path().join("legacy/run"),
        };
        std::fs::create_dir_all(&legacy.data_dir).unwrap();
        let notice = crate::legacy_migration_notice_with_paths(&paths, &legacy)
            .expect("a planted legacy data dir must be reported");
        assert!(notice.contains("data"), "{notice}");
    }

    // ── formatting helpers ──

    #[test]
    fn byte_and_age_formatting_stay_compact() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_age(std::time::Duration::from_secs(30)), "30s");
        assert_eq!(format_age(std::time::Duration::from_secs(3600 * 5)), "5h");
    }
}
