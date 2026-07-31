//! Real per-harness availability probing (spec 0068).
//!
//! Every built-in wrapper adapter (claude, codex, opencode, agy, grok, kimi,
//! hermes, pi)
//! declares `binary = "construct"` — the agentd wrapper itself, which is
//! always the currently-running binary, so it always resolves. Checking
//! *that* binary told a user nothing about whether the CLI it wraps (or a
//! usable smith credential) is actually present. This module probes the
//! thing that actually determines whether a session can start.

use construct_protocol::adapter::resolve_command_override;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Result of probing one harness: whether it's usable right now, plus a
/// short human-readable reason surfaced in the picker, welcome card, and
/// `construct harnesses` CLI output.
#[derive(Debug, Clone)]
pub struct Availability {
    pub available: bool,
    pub detail: String,
}

impl Availability {
    pub(crate) fn ready(detail: impl Into<String>) -> Self {
        Self {
            available: true,
            detail: detail.into(),
        }
    }

    pub(crate) fn missing(detail: impl Into<String>) -> Self {
        Self {
            available: false,
            detail: detail.into(),
        }
    }
}

/// Cache for the two probes that shell out or hit the network (macOS
/// keychain read, Ollama reachability). File-existence and env-var checks
/// are cheap enough to redo on every call and are never cached. There's at
/// most one keychain item and one Ollama endpoint to check daemon-wide, so
/// this is a plain pair of slots rather than a map.
#[derive(Default)]
pub struct AvailabilityCache {
    claude_keychain: Option<(Instant, bool)>,
    ollama: Option<(Instant, bool)>,
}

const CACHE_TTL: Duration = Duration::from_secs(20);
/// Bound on the Ollama TCP probe so a firewalled/unreachable host can never
/// make a `harnesses()` call hang.
const OLLAMA_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

fn cached(slot: Option<(Instant, bool)>) -> Option<bool> {
    slot.and_then(|(at, v)| (at.elapsed() < CACHE_TTL).then_some(v))
}

/// Probe a wrapper adapter that shells out to a named CLI. Honors the same
/// `CONSTRUCT_<H>_CMD` / `CONSTRUCT_<H>_BIN` overrides the adapter itself
/// resolves against at spawn time (see
/// `construct_protocol::adapter::resolve_command_override`), so the picker
/// never disagrees with what a new session would actually try to run.
pub fn probe_wrapper_cli(command_env: &str, binary_env: &str, default_bin: &str) -> Availability {
    let cmd = resolve_command_override(command_env, binary_env, default_bin);
    if resolve_bin_path(&cmd.bin).is_some() {
        Availability::ready("ready")
    } else {
        Availability::missing(format!("`{}` CLI not found on daemon PATH", cmd.bin))
    }
}

fn resolve_bin_path(bin: &str) -> Option<PathBuf> {
    let p = PathBuf::from(bin);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    which::which(bin).ok()
}

/// Probe a non-built-in (community) adapter: available when its configured
/// `binary` resolves, matching the daemon's original (pre-probing) fallback
/// semantics — there's no protocol-level way to ask an arbitrary AHP adapter
/// what it wraps. `resolved` is the same lookup `harnesses()` already runs
/// (via `locate_binary`, which also checks next to the daemon's own exe) to
/// populate `HarnessInfo.binary`; reused here instead of re-resolving.
pub fn probe_generic_adapter(
    binary_spec: &str,
    resolved: Option<&std::path::Path>,
) -> Availability {
    if resolved.is_some() {
        Availability::ready("ready")
    } else {
        Availability::missing(format!("`{binary_spec}` binary not found"))
    }
}

fn env_present(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Probe the built-in smith harness: available when any credential path
/// smith's own provider selection would pick up actually exists. Mirrors
/// the precedence smith itself uses (explicit model pin, then direct API
/// keys, then OAuth subscriptions, then local Ollama) so the reported
/// status never promises a provider smith wouldn't actually select.
pub async fn probe_smith(cache: &std::sync::Mutex<AvailabilityCache>) -> Availability {
    if env_present("CONSTRUCT_SMITH_MODEL") {
        return Availability::ready("ready (CONSTRUCT_SMITH_MODEL set)");
    }
    if env_present("ANTHROPIC_API_KEY") {
        return Availability::ready("ready (Anthropic API key)");
    }
    if env_present("OPENAI_API_KEY") {
        return Availability::ready("ready (OpenAI API key)");
    }
    if env_present("GEMINI_API_KEY") || env_present("GOOGLE_API_KEY") {
        return Availability::ready("ready (Gemini API key)");
    }
    if env_present("META_API_KEY") || env_present("MODEL_API_KEY") {
        return Availability::ready("ready (Meta API key)");
    }
    if env_present("GROK_API_KEY") || env_present("XAI_API_KEY") {
        return Availability::ready("ready (Grok API key)");
    }
    if claude_oauth_credentials_present(cache).await {
        return Availability::ready("ready (Claude subscription)");
    }
    if codex_auth_file().map(|p| p.exists()).unwrap_or(false) {
        return Availability::ready("ready (Codex subscription)");
    }
    if grok_auth_file().map(|p| p.exists()).unwrap_or(false) {
        return Availability::ready("ready (Grok subscription)");
    }
    if ollama_reachable(cache).await {
        return Availability::ready("ready (local Ollama)");
    }
    Availability::missing("no API key or OAuth credential found")
}

/// Existence-only mirror of `CredStore::locate` in
/// `adapter-smith/src/provider/claude_oauth.rs`: explicit file override,
/// then the default credentials file, then the macOS keychain item. No
/// token parsing or refresh here — a probe only needs to know a credential
/// *exists*, not whether it's still valid.
async fn claude_oauth_credentials_present(cache: &std::sync::Mutex<AvailabilityCache>) -> bool {
    if let Ok(p) = std::env::var("CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS") {
        if !p.is_empty() && PathBuf::from(&p).exists() {
            return true;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if PathBuf::from(home)
            .join(".claude")
            .join(".credentials.json")
            .exists()
        {
            return true;
        }
    }
    if let Some(v) = cached(cache.lock().unwrap().claude_keychain) {
        return v;
    }
    let found = keychain_has_claude_credentials().await;
    cache.lock().unwrap().claude_keychain = Some((Instant::now(), found));
    found
}

const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[cfg(target_os = "macos")]
async fn keychain_has_claude_credentials() -> bool {
    tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE])
        .output()
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
async fn keychain_has_claude_credentials() -> bool {
    false
}

fn codex_auth_file() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("auth.json"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("auth.json"))
}

fn grok_auth_file() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".grok").join("auth.json"))
}

fn kimi_auth_file() -> Option<PathBuf> {
    let base = match std::env::var("KIMI_CODE_HOME") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(".kimi-code"),
    };
    Some(base.join("credentials").join("kimi-code.json"))
}

async fn ollama_reachable(cache: &std::sync::Mutex<AvailabilityCache>) -> bool {
    if let Some(v) = cached(cache.lock().unwrap().ollama) {
        return v;
    }
    let base = std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let found = match host_port(&base) {
        Some(hostport) => tokio::time::timeout(
            OLLAMA_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(hostport),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false),
        None => false,
    };
    cache.lock().unwrap().ollama = Some((Instant::now(), found));
    found
}

/// One auth method the built-in `smith` harness can use (spec 0069's
/// `/configure` dialog, smith-auth tab). `id` is the stable wire id sent to
/// clients and echoed back to pin the method; `model_prefix` is the
/// `CONSTRUCT_SMITH_MODEL` prefix that selects it explicitly (empty for
/// `"auto"`, which clears the pin instead of setting one).
pub struct SmithAuthMethod {
    pub id: &'static str,
    pub label: &'static str,
    pub model_prefix: &'static str,
    pub default_model: &'static str,
    pub available: bool,
    pub detail: String,
}

fn env_key_method(
    id: &'static str,
    label: &'static str,
    model_prefix: &'static str,
    default_model: &'static str,
    vars: &[&str],
) -> SmithAuthMethod {
    let available = vars.iter().any(|v| env_present(v));
    let detail = match vars {
        [one] => {
            if available {
                format!("{one} is set")
            } else {
                format!("{one} not set")
            }
        }
        [a, b, ..] => {
            if available {
                format!("{a} (or {b}) is set")
            } else {
                format!("neither {a} nor {b} is set")
            }
        }
        [] => String::new(),
    };
    SmithAuthMethod {
        id,
        label,
        model_prefix,
        default_model,
        available,
        detail,
    }
}

/// Every auth method smith supports, each with live-detected status, in the
/// same precedence order `probe_smith` checks (spec 0068). Used by the
/// `smith.auth_status` IPC method that backs the `/configure` dialog's
/// smith-auth tab — unlike `probe_smith` (one pass/fail signal for the
/// harness picker), this breaks the detection down per method so the dialog
/// can list all of them and let the user pin one explicitly.
///
/// The `auto` entry is the exception: its `available` reflects only the
/// four direct-API-key methods, because those are the only ones smith's
/// real auto-detect ladder consults absent a pin (spec 0071) — every other
/// method here requires an explicit `<prefix>:<model>` pin to be selected
/// (spec 0069).
pub async fn smith_auth_methods(
    cache: &std::sync::Mutex<AvailabilityCache>,
) -> Vec<SmithAuthMethod> {
    let anthropic = env_key_method(
        "anthropic_api_key",
        "Anthropic API key",
        "anthropic",
        "claude-opus-4-8",
        &["ANTHROPIC_API_KEY"],
    );
    let openai = env_key_method(
        "openai_api_key",
        "OpenAI API key",
        "openai",
        "gpt-5",
        &["OPENAI_API_KEY"],
    );
    let gemini = env_key_method(
        "gemini_api_key",
        "Gemini API key",
        "gemini",
        "gemini-2.5-pro",
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    );
    let meta = env_key_method(
        "meta_api_key",
        "Meta API key",
        "meta",
        "muse-spark-1.1",
        &["META_API_KEY", "MODEL_API_KEY"],
    );
    let grok_key = env_key_method(
        "grok_api_key",
        "Grok API key",
        "grok",
        "grok-2-latest",
        &["GROK_API_KEY", "XAI_API_KEY"],
    );
    let claude_sub_present = claude_oauth_credentials_present(cache).await;
    let claude_sub = SmithAuthMethod {
        id: "claude_subscription",
        label: "Claude subscription",
        model_prefix: "claude-oauth",
        default_model: "claude-sonnet-4-6",
        available: claude_sub_present,
        detail: if claude_sub_present {
            "Claude Code credentials found".to_string()
        } else {
            "no Claude Code credentials found".to_string()
        },
    };
    let codex_sub_present = codex_auth_file().map(|p| p.exists()).unwrap_or(false);
    let codex_sub = SmithAuthMethod {
        id: "codex_subscription",
        label: "Codex subscription",
        model_prefix: "codex-oauth",
        default_model: "gpt-5.5",
        available: codex_sub_present,
        detail: if codex_sub_present {
            "~/.codex/auth.json found".to_string()
        } else {
            "no ~/.codex/auth.json found".to_string()
        },
    };
    let grok_sub_present = grok_auth_file().map(|p| p.exists()).unwrap_or(false);
    let grok_sub = SmithAuthMethod {
        id: "grok_subscription",
        label: "Grok subscription",
        model_prefix: "grok-oauth",
        default_model: "grok-2-latest",
        available: grok_sub_present,
        detail: if grok_sub_present {
            "~/.grok/auth.json found".to_string()
        } else {
            "no ~/.grok/auth.json found".to_string()
        },
    };
    let kimi_sub_present = kimi_auth_file().map(|p| p.exists()).unwrap_or(false);
    let kimi_sub = SmithAuthMethod {
        id: "kimi_subscription",
        label: "Kimi subscription",
        model_prefix: "kimi-oauth",
        default_model: "k3",
        available: kimi_sub_present,
        detail: if kimi_sub_present {
            "~/.kimi-code/credentials/kimi-code.json found".to_string()
        } else {
            "no ~/.kimi-code/credentials/kimi-code.json found".to_string()
        },
    };
    let ollama_present = ollama_reachable(cache).await;
    let ollama = SmithAuthMethod {
        id: "ollama",
        label: "Local Ollama",
        model_prefix: "ollama",
        default_model: "llama3.1",
        available: ollama_present,
        detail: if ollama_present {
            "Ollama server reachable".to_string()
        } else {
            "no Ollama server reachable".to_string()
        },
    };
    // `auto` mirrors `agent::default_auto_detect_spec`'s actual ladder
    // (spec 0071) — direct API keys only, in this order. It must NOT count
    // the other five methods here: those all require an explicit
    // `<prefix>:<model>` pin to be selected (see spec 0069's auto-vs-
    // explicit distinction), so reporting `auto` as available because e.g.
    // only a Claude subscription exists would tell the dialog "Auto-detect
    // is ready" while a session started without a pin still errors with
    // "no auto-detected smith credential" — the exact promise/behavior
    // mismatch this dialog exists to prevent.
    let auto_available =
        anthropic.available || openai.available || gemini.available || meta.available;
    let auto = SmithAuthMethod {
        id: "auto",
        label: "Auto-detect",
        model_prefix: "",
        default_model: "",
        available: auto_available,
        detail: if auto_available {
            "auto-detects the first set API key: Anthropic → OpenAI → Gemini → Meta".to_string()
        } else {
            "no auto-detected API key set (subscriptions and Ollama must be picked explicitly)"
                .to_string()
        },
    };
    vec![
        anthropic, openai, gemini, meta, grok_key, claude_sub, codex_sub, grok_sub, kimi_sub,
        ollama, auto,
    ]
}

/// Inputs for [`ambient_features`], pre-probed by the caller so the mapping
/// itself stays pure and unit-testable.
pub struct FeatureInputs {
    /// smith's credential probe result ([`probe_smith`]).
    pub smith: Availability,
    /// `[suggest] enabled` from config.
    pub suggest_enabled: bool,
    /// The orchestrator's configured harness and that harness's
    /// availability, or `None` when the operator is disabled in config.
    pub orchestrator: Option<(String, Availability)>,
}

/// Map raw availability probes to per-feature status rows (spec 0151).
/// This is the single place that records which ambient features depend on
/// a smith credential and how each degrades without one — clients render
/// these rows verbatim instead of re-deriving the dependency themselves.
pub fn ambient_features(inputs: &FeatureInputs) -> Vec<construct_protocol::FeatureInfo> {
    use construct_protocol::{FeatureInfo, FeatureStatus};
    let smith_ok = inputs.smith.available;
    let auto_title = if smith_ok {
        FeatureInfo {
            id: "auto_title".to_string(),
            label: "Session auto-naming".to_string(),
            status: FeatureStatus::Ok,
            detail: format!("titles generated via smith ({})", inputs.smith.detail),
        }
    } else {
        FeatureInfo {
            id: "auto_title".to_string(),
            label: "Session auto-naming".to_string(),
            status: FeatureStatus::Degraded,
            detail: "no smith credential — sessions on model harnesses fall back to a \
                     one-shot on their own harness; smith and shell sessions keep their \
                     default (hash) names"
                .to_string(),
        }
    };
    let suggestions = if !inputs.suggest_enabled {
        FeatureInfo {
            id: "suggestions".to_string(),
            label: "Next-prompt suggestions".to_string(),
            status: FeatureStatus::Off,
            detail: "disabled in config ([suggest] enabled = false)".to_string(),
        }
    } else if smith_ok {
        FeatureInfo {
            id: "suggestions".to_string(),
            label: "Next-prompt suggestions".to_string(),
            status: FeatureStatus::Ok,
            detail: format!(
                "smith and shell sessions generate via smith ({}); other harnesses use \
                 their own model",
                inputs.smith.detail
            ),
        }
    } else {
        FeatureInfo {
            id: "suggestions".to_string(),
            label: "Next-prompt suggestions".to_string(),
            status: FeatureStatus::Degraded,
            detail: "no smith credential — smith and shell sessions get no suggestions; \
                     other harnesses use their own model"
                .to_string(),
        }
    };
    let operator = match &inputs.orchestrator {
        None => FeatureInfo {
            id: "operator".to_string(),
            label: "Operator session".to_string(),
            status: FeatureStatus::Off,
            detail: "disabled in config ([orchestrator])".to_string(),
        },
        Some((harness, avail)) if avail.available => FeatureInfo {
            id: "operator".to_string(),
            label: "Operator session".to_string(),
            status: FeatureStatus::Ok,
            detail: format!("runs on {harness} ({})", avail.detail),
        },
        // smith's degraded operator still starts and handles slash
        // commands (spec 0071's orchestrator exception); a missing
        // wrapper CLI for a non-smith orchestrator can't start at all.
        Some((harness, avail)) if harness == "smith" => FeatureInfo {
            id: "operator".to_string(),
            label: "Operator session".to_string(),
            status: FeatureStatus::Degraded,
            detail: format!(
                "slash commands only — smith has no model credential ({})",
                avail.detail
            ),
        },
        Some((harness, avail)) => FeatureInfo {
            id: "operator".to_string(),
            label: "Operator session".to_string(),
            status: FeatureStatus::Off,
            detail: format!("{harness} unavailable: {}", avail.detail),
        },
    };
    vec![auto_title, suggestions, operator]
}

/// Which `smith_auth_methods` entry a pinned `CONSTRUCT_SMITH_MODEL` spec
/// currently selects, by matching its `provider:` prefix. `None` pin
/// (unset/empty) resolves to `"auto"`; a pin whose prefix doesn't match any
/// known method (an `@profile` or hand-edited spec) resolves to `None` — the
/// dialog then shows no row as the current pick.
pub fn current_smith_auth_method(
    pinned: Option<&str>,
    methods: &[SmithAuthMethod],
) -> Option<String> {
    let Some(spec) = pinned.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some("auto".to_string());
    };
    let prefix = spec.split(':').next().unwrap_or(spec);
    methods
        .iter()
        .find(|m| !m.model_prefix.is_empty() && m.model_prefix == prefix)
        .map(|m| m.id.to_string())
}

/// Strip an optional `scheme://` and path/query suffix, defaulting to port
/// 11434 (Ollama's default) when the remaining host has no explicit port.
fn host_port(base_url: &str) -> Option<String> {
    let without_scheme = base_url.rsplit("://").next().unwrap_or(base_url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if host_port.is_empty() {
        return None;
    }
    if host_port.contains(':') {
        Some(host_port.to_string())
    } else {
        Some(format!("{host_port}:11434"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_defaults_ollama_port() {
        assert_eq!(
            host_port("http://localhost:11434"),
            Some("localhost:11434".to_string())
        );
        assert_eq!(
            host_port("http://example.com"),
            Some("example.com:11434".to_string())
        );
        assert_eq!(
            host_port("example.com:9999"),
            Some("example.com:9999".to_string())
        );
        assert_eq!(host_port(""), None);
    }

    /// The feature mapping (spec 0151) must connect a missing smith
    /// credential to every ambient feature it silently disables, and must
    /// not report anything degraded when smith is usable.
    #[test]
    fn ambient_features_reflect_smith_availability() {
        use construct_protocol::FeatureStatus;
        let ok = ambient_features(&FeatureInputs {
            smith: Availability::ready("ready (Anthropic API key)"),
            suggest_enabled: true,
            orchestrator: Some((
                "smith".to_string(),
                Availability::ready("ready (Anthropic API key)"),
            )),
        });
        assert!(ok.iter().all(|f| f.status == FeatureStatus::Ok), "{ok:#?}");

        let missing = ambient_features(&FeatureInputs {
            smith: Availability::missing("no API key or OAuth credential found"),
            suggest_enabled: true,
            orchestrator: Some((
                "smith".to_string(),
                Availability::missing("no API key or OAuth credential found"),
            )),
        });
        assert!(
            missing.iter().all(|f| f.status == FeatureStatus::Degraded),
            "{missing:#?}"
        );
        // Every degraded row names the cause so a user reading one row in
        // isolation still learns what to fix.
        assert!(missing
            .iter()
            .all(|f| f.detail.contains("smith") && f.detail.contains("credential")));

        let off = ambient_features(&FeatureInputs {
            smith: Availability::missing("no API key or OAuth credential found"),
            suggest_enabled: false,
            orchestrator: None,
        });
        assert_eq!(off[1].status, FeatureStatus::Off, "{off:#?}");
        assert_eq!(off[2].status, FeatureStatus::Off, "{off:#?}");

        // A non-smith orchestrator harness that's missing means the operator
        // can't start at all — Off, not smith-style Degraded.
        let wrapper = ambient_features(&FeatureInputs {
            smith: Availability::missing("no API key or OAuth credential found"),
            suggest_enabled: true,
            orchestrator: Some((
                "claude".to_string(),
                Availability::missing("`claude` CLI not found on daemon PATH"),
            )),
        });
        assert_eq!(wrapper[2].status, FeatureStatus::Off, "{wrapper:#?}");
    }

    #[test]
    fn probe_wrapper_cli_reports_missing_binary() {
        let unique = format!("construct-test-nonexistent-bin-{}", std::process::id());
        let avail = probe_wrapper_cli(
            "CONSTRUCT_TEST_NONEXISTENT_CMD",
            "CONSTRUCT_TEST_NONEXISTENT_BIN",
            &unique,
        );
        assert!(!avail.available);
        assert!(avail.detail.contains(&unique));
    }

    #[test]
    fn probe_wrapper_cli_finds_binary_on_path() {
        // `sh` is present on every platform this daemon supports.
        let avail = probe_wrapper_cli("CONSTRUCT_TEST_SH_CMD", "CONSTRUCT_TEST_SH_BIN", "sh");
        assert!(avail.available);
    }

    /// Regression for the `auto` entry counting subscription/Ollama
    /// availability: a machine with only a Codex subscription credential
    /// (no direct API key) must NOT report `auto` as available, since
    /// `agent::default_auto_detect_spec`'s real ladder (spec 0071) never
    /// consults subscriptions or Ollama without an explicit pin. Reporting
    /// `auto` as ready here would tell `/configure` "Auto-detect is ready"
    /// while a session started without a pin still errors at start — the
    /// exact mismatch this dialog exists to prevent.
    #[tokio::test]
    async fn auto_unavailable_when_only_a_subscription_credential_exists() {
        // `HOME` and `CODEX_HOME` are process-global, and tests in other
        // modules point them at their own fixtures. A lock private to this
        // module excluded only its own tests, so a router test setting
        // `CODEX_HOME` could still interleave with this one's read. Take the
        // crate-wide guard every env-mutating test uses.
        let _lock = crate::router::oauth::test_env_guard();
        let key_vars = [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
        ];
        let saved_keys: Vec<Option<String>> =
            key_vars.iter().map(|v| std::env::var(v).ok()).collect();
        for v in key_vars {
            std::env::remove_var(v);
        }
        let saved_home = std::env::var("HOME").ok();
        let saved_codex_home = std::env::var("CODEX_HOME").ok();
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".codex")).expect("mkdir");
        std::fs::write(tmp.path().join(".codex").join("auth.json"), "{}").expect("write");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CODEX_HOME");

        let cache = std::sync::Mutex::new(AvailabilityCache::default());
        let methods = smith_auth_methods(&cache).await;

        for (v, saved) in key_vars.iter().zip(saved_keys) {
            match saved {
                Some(val) => std::env::set_var(v, val),
                None => std::env::remove_var(v),
            }
        }
        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_codex_home {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }

        let codex = methods
            .iter()
            .find(|m| m.id == "codex_subscription")
            .expect("codex entry present");
        assert!(
            codex.available,
            "fixture ~/.codex/auth.json should be detected"
        );
        let auto = methods
            .iter()
            .find(|m| m.id == "auto")
            .expect("auto entry present");
        assert!(
            !auto.available,
            "auto must not report available from a subscription-only credential"
        );
        assert!(auto.detail.contains("subscriptions and Ollama"));
    }
}
