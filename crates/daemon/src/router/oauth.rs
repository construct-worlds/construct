//! Subscription (OAuth) logins as route targets (spec 0117).
//!
//! A model profile is an endpoint plus a key. A subscription login is
//! neither: the credential lives in whatever store the owning CLI wrote it
//! to, and the endpoint is that CLI's private backend with its own
//! required headers. This module turns the logins already present on the
//! machine into route targets without inventing configuration for them.
//!
//! **The router reads tokens and never refreshes them.** Refresh is a
//! write to a credential store another application owns, and every OAuth
//! client here writes the refreshed token *back* to that store. A second
//! refresher racing the owning CLI can invalidate a token mid-turn, and
//! that failure is intermittent and lands on the wrong component. So the
//! owning CLI stays the single refresh owner: when a token is expired or
//! about to be, the target is reported unavailable with an instruction to
//! use that CLI, rather than the router minting one.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use super::Dialect;

/// Treat a token as unusable this long before it actually expires, so a
/// turn does not start on a credential that dies mid-stream.
const EXPIRY_LEEWAY_SECS: i64 = 120;

/// A subscription login the router knows how to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OauthProvider {
    /// Claude subscription: Anthropic Messages with an OAuth bearer.
    Claude,
    /// ChatGPT subscription: the Codex backend's Responses endpoint.
    Codex,
    /// Grok subscription: xAI's OpenAI-compatible endpoint.
    Grok,
    /// Kimi Code subscription: Moonshot's Anthropic-compatible coding
    /// backend.
    Kimi,
}

impl OauthProvider {
    /// Every provider offered as a route target.
    ///
    /// Antigravity's login is deliberately absent: its backend speaks a
    /// Gemini-shaped protocol the router has no parser or emitter for, and
    /// offering a target it would mistranslate is worse than not offering
    /// it (spec 0116).
    pub const ALL: &'static [OauthProvider] = &[
        OauthProvider::Claude,
        OauthProvider::Codex,
        OauthProvider::Grok,
        OauthProvider::Kimi,
    ];

    /// Route name, matching the model-spec prefix smith uses for the same
    /// login so one vocabulary covers both.
    pub fn name(self) -> &'static str {
        match self {
            OauthProvider::Claude => "claude-oauth",
            OauthProvider::Codex => "codex-oauth",
            OauthProvider::Grok => "grok-oauth",
            OauthProvider::Kimi => "kimi-oauth",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            OauthProvider::Claude | OauthProvider::Kimi => Dialect::AnthropicMessages,
            OauthProvider::Codex => Dialect::OpenAiResponses,
            OauthProvider::Grok => Dialect::OpenAiChat,
        }
    }

    /// Endpoint the login's own CLI talks to. Not a configurable base URL:
    /// these are private backends, and pointing them elsewhere would make
    /// the credential meaningless.
    pub fn endpoint(self) -> &'static str {
        match self {
            OauthProvider::Claude => "https://api.anthropic.com/v1/messages",
            OauthProvider::Codex => "https://chatgpt.com/backend-api/codex/responses",
            OauthProvider::Grok => "https://api.x.ai/v1/chat/completions",
            // Established by probe (2026-07-29): the Kimi Code CLI bundles
            // the Anthropic SDK pointed at this base, and a real Messages
            // request with only the bearer returned a real Messages
            // response.
            OauthProvider::Kimi => "https://api.kimi.com/coding/v1/messages",
        }
    }

    /// Model used when the operator has not chosen one.
    ///
    /// A default that lags a vendor release produces a clean API error
    /// naming the model, which is recoverable; requiring configuration for
    /// a login the machine already has is friction for everyone.
    pub fn default_model(self) -> String {
        self.seed_models().first().cloned().unwrap_or_default()
    }

    /// Models offered for this login before any are configured.
    ///
    /// Taken from the one curated list that already backs smith's `/model`
    /// completion, rather than a second list here. The provider vocabulary
    /// is the same on both sides — a route target is named after the same
    /// billing/auth path a model spec selects — so a private copy would
    /// only create somewhere for the two to disagree.
    ///
    /// This matters beyond tidiness: the Claude subscription path takes
    /// short aliases (`sonnet`, `opus`) rather than full model ids, which a
    /// hand-written list is unlikely to get right.
    ///
    /// For `claude-oauth` and `codex-oauth` this list is the only source,
    /// by decision: their backends are private and neither was observed to
    /// serve a models endpoint, so there is nothing to discover from and
    /// nothing to fall back to. Do not add live discovery for them on the
    /// assumption that an endpoint exists — the grok and kimi subscription
    /// backends do serve one, and that difference was established by
    /// interception, not by symmetry.
    pub fn seed_models(self) -> Vec<String> {
        construct_protocol::slash::models_for_provider(self.name())
    }

    /// Which CLI to use to renew the login, for the unavailable message.
    fn owning_cli(self) -> &'static str {
        match self {
            OauthProvider::Claude => "claude",
            OauthProvider::Codex => "codex",
            OauthProvider::Grok => "grok",
            OauthProvider::Kimi => "kimi",
        }
    }

    /// The exact command that signs in. Most CLIs run their login flow on
    /// launch when the credential is missing or expired; Codex has a
    /// dedicated subcommand that exits once the login lands.
    pub fn login_command(self) -> String {
        match self {
            OauthProvider::Codex => "codex login".to_string(),
            _ => self.owning_cli().to_string(),
        }
    }
}

/// A usable subscription credential.
#[derive(Debug, Clone)]
pub struct OauthCredential {
    pub access_token: String,
    /// ChatGPT account id, required as a header by the Codex backend.
    pub account_id: Option<String>,
}

/// Why a login cannot serve as a route right now: the message fit for a
/// route's `unavailable_reason`, plus — when the fix is signing in with
/// the owning CLI — the exact command that does it, so a client can offer
/// to run it (in a session of its own; the owning tool remains the only
/// writer of the credential, spec 0117).
pub struct LoginBlocker {
    pub reason: String,
    pub login_command: Option<String>,
}

/// Read the login for `provider`. `Err` carries a message fit to show as a
/// route's `unavailable_reason`.
pub fn read_credential(provider: OauthProvider) -> Result<OauthCredential, String> {
    check_login(provider).map_err(|blocker| blocker.reason)
}

/// Like [`read_credential`], keeping the blocker structured.
pub fn check_login(provider: OauthProvider) -> Result<OauthCredential, LoginBlocker> {
    match provider {
        OauthProvider::Claude => read_claude(),
        OauthProvider::Codex => read_codex(),
        OauthProvider::Grok => read_grok(),
        OauthProvider::Kimi => read_kimi(),
    }
    .map_err(|e| match e {
        // Expiry is the one failure a user can fix in one step, so it says
        // exactly how.
        ReadError::Expired => LoginBlocker {
            reason: format!(
                "{} login has expired; run `{}` once to renew it (the router never \
                 refreshes another tool's credential)",
                provider.name(),
                provider.owning_cli()
            ),
            login_command: Some(provider.login_command()),
        },
        ReadError::Missing => LoginBlocker {
            reason: format!(
                "not logged in to {}; run `{}` and sign in",
                provider.name(),
                provider.owning_cli()
            ),
            login_command: Some(provider.login_command()),
        },
        ReadError::Other(msg) => LoginBlocker {
            reason: msg,
            login_command: None,
        },
    })
}

enum ReadError {
    Missing,
    Expired,
    Other(String),
}

fn home() -> Result<PathBuf, ReadError> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ReadError::Other("$HOME is not set".into()))
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Claude: macOS keychain item, or ~/.claude/.credentials.json
// ---------------------------------------------------------------------------

fn read_claude() -> Result<OauthCredential, ReadError> {
    let raw = claude_raw()?;
    let doc: Value = serde_json::from_str(raw.trim())
        .map_err(|e| ReadError::Other(format!("parse claude credentials: {e}")))?;
    // The document nests the OAuth block under a product key in some
    // versions and inlines it in others; accept both rather than pinning
    // one shape.
    let block = doc
        .get("claudeAiOauth")
        .or_else(|| doc.get("claude_ai_oauth"))
        .unwrap_or(&doc);
    let token = block
        .get("accessToken")
        .or_else(|| block.get("access_token"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or(ReadError::Missing)?;
    let expires_at_ms = block
        .get("expiresAt")
        .or_else(|| block.get("expires_at_ms"))
        .and_then(Value::as_i64);
    if let Some(ms) = expires_at_ms.filter(|ms| *ms > 0) {
        if ms / 1000 - EXPIRY_LEEWAY_SECS <= now_secs() {
            return Err(ReadError::Expired);
        }
    }
    Ok(OauthCredential {
        access_token: token.to_string(),
        account_id: None,
    })
}

fn claude_raw() -> Result<String, ReadError> {
    if let Ok(path) = std::env::var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE") {
        return std::fs::read_to_string(&path).map_err(|_| ReadError::Missing);
    }
    let file = home()?.join(".claude").join(".credentials.json");
    if let Ok(text) = std::fs::read_to_string(&file) {
        return Ok(text);
    }
    #[cfg(target_os = "macos")]
    {
        // The keychain is where Claude Code stores it on macOS. Read-only:
        // we never write this item back.
        let out = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .map_err(|e| ReadError::Other(format!("run security: {e}")))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }
    Err(ReadError::Missing)
}

// ---------------------------------------------------------------------------
// Codex: ~/.codex/auth.json
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct CodexAuth {
    #[serde(default)]
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize, Default)]
struct CodexTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    id_token: String,
}

fn read_codex() -> Result<OauthCredential, ReadError> {
    let path = match std::env::var("CODEX_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h).join("auth.json"),
        _ => home()?.join(".codex").join("auth.json"),
    };
    let bytes = std::fs::read(&path).map_err(|_| ReadError::Missing)?;
    let auth: CodexAuth = serde_json::from_slice(&bytes)
        .map_err(|e| ReadError::Other(format!("parse {}: {e}", path.display())))?;
    let tokens = auth.tokens.ok_or(ReadError::Missing)?;
    if tokens.access_token.trim().is_empty() {
        return Err(ReadError::Missing);
    }
    if jwt_is_expired(&tokens.access_token) {
        return Err(ReadError::Expired);
    }
    // Newer builds store the account id directly; older ones leave it for
    // clients to read out of the id_token.
    let account_id = Some(tokens.account_id.trim().to_string())
        .filter(|a| !a.is_empty())
        .or_else(|| jwt_account_id(&tokens.id_token));
    Ok(OauthCredential {
        access_token: tokens.access_token,
        account_id,
    })
}

/// Read `exp` out of a JWT without verifying it. Verification is the
/// endpoint's job; we only want to avoid starting a turn on a dead token.
fn jwt_is_expired(token: &str) -> bool {
    match jwt_claims(token).and_then(|c| c.get("exp").and_then(Value::as_i64)) {
        Some(exp) => exp - EXPIRY_LEEWAY_SECS <= now_secs(),
        None => false,
    }
}

fn jwt_account_id(token: &str) -> Option<String> {
    let claims = jwt_claims(token)?;
    claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

// ---------------------------------------------------------------------------
// Grok: ~/.grok/auth.json — a map of entries, newest unexpired wins
// ---------------------------------------------------------------------------

fn read_grok() -> Result<OauthCredential, ReadError> {
    let path = match std::env::var("GROK_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h).join(".grok").join("auth.json"),
        _ => home()?.join(".grok").join("auth.json"),
    };
    let bytes = std::fs::read(&path).map_err(|_| ReadError::Missing)?;
    let entries: serde_json::Map<String, Value> = serde_json::from_slice(&bytes)
        .map_err(|e| ReadError::Other(format!("parse {}: {e}", path.display())))?;

    let now = now_secs();
    let mut best: Option<(Option<i64>, String)> = None;
    let mut saw_expired = false;
    for entry in entries.values() {
        let Some(token) = entry.get("key").and_then(Value::as_str) else {
            continue;
        };
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let expiry = entry
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp());
        if let Some(exp) = expiry {
            if exp - EXPIRY_LEEWAY_SECS <= now {
                saw_expired = true;
                continue;
            }
        }
        // Prefer the entry that lasts longest; an entry with no expiry is
        // treated as the least specific and only used if nothing else fits.
        let better = match (&best, &expiry) {
            (None, _) => true,
            (Some((None, _)), Some(_)) => true,
            (Some((Some(cur), _)), Some(new)) => new > cur,
            _ => false,
        };
        if better {
            best = Some((expiry, token.to_string()));
        }
    }
    match best {
        Some((_, token)) => Ok(OauthCredential {
            access_token: token,
            account_id: None,
        }),
        None if saw_expired => Err(ReadError::Expired),
        None => Err(ReadError::Missing),
    }
}

// ---------------------------------------------------------------------------
// Kimi: ~/.kimi-code/credentials/kimi-code.json
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct KimiAuth {
    #[serde(default)]
    access_token: String,
    /// Unix seconds. The Kimi CLI issues short-lived access tokens
    /// (`expires_in: 900` observed), so this route goes stale within
    /// minutes of the CLI's last refresh — expected under the read-only
    /// rule; the expired message names `kimi` as the renewing tool.
    #[serde(default)]
    expires_at: Option<i64>,
}

fn read_kimi() -> Result<OauthCredential, ReadError> {
    let path = match std::env::var("KIMI_CODE_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h),
        _ => home()?.join(".kimi-code"),
    }
    .join("credentials")
    .join("kimi-code.json");
    let bytes = std::fs::read(&path).map_err(|_| ReadError::Missing)?;
    let auth: KimiAuth = serde_json::from_slice(&bytes)
        .map_err(|e| ReadError::Other(format!("parse {}: {e}", path.display())))?;
    if auth.access_token.trim().is_empty() {
        return Err(ReadError::Missing);
    }
    if let Some(exp) = auth.expires_at.filter(|e| *e > 0) {
        if exp - EXPIRY_LEEWAY_SECS <= now_secs() {
            return Err(ReadError::Expired);
        }
    }
    Ok(OauthCredential {
        access_token: auth.access_token,
        account_id: None,
    })
}

/// Extra request headers the login's backend requires beyond the bearer.
///
/// These are not optional decoration: each backend rejects requests that
/// omit them, and they are part of what makes the credential usable at all.
pub fn extra_headers(provider: OauthProvider, cred: &OauthCredential) -> Vec<(&'static str, String)> {
    match provider {
        OauthProvider::Claude => vec![
            ("anthropic-beta", "oauth-2025-04-20".to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
        OauthProvider::Codex => {
            let mut h = vec![
                ("originator", "codex_cli_rs".to_string()),
                ("openai-beta", "responses=experimental".to_string()),
            ];
            if let Some(account) = cred.account_id.as_deref() {
                h.push(("chatgpt-account-id", account.to_string()));
            }
            h
        }
        OauthProvider::Grok => Vec::new(),
        // Probed: a bare-bearer request succeeds; `anthropic-version` is
        // accepted but not required, so nothing is mandatory here.
        OauthProvider::Kimi => Vec::new(),
    }
}

/// Request parameters a login's backend rejects outright.
///
/// A dialect says what shape a request takes; a *target* decides which of
/// that shape it accepts, and the two are not the same. The Codex backend
/// speaks Responses but refuses `max_output_tokens` with a 400 — while the
/// public Responses endpoint requires nothing of the sort and grok's
/// accepts it happily. Both lists below come from intercepted real
/// requests, not from documentation.
///
/// Dropping a cap is lossy: a harness that asked for a token limit does not
/// get one enforced at the target. That is the accepted cost of a
/// translation whose alternative is a refused turn (spec 0116).
pub fn unsupported_params(provider: OauthProvider) -> &'static [&'static str] {
    match provider {
        // Observed accepted set: include, input, instructions, model,
        // parallel_tool_calls, prompt_cache_key, reasoning, store, stream,
        // text, tool_choice, tools. Anything else is a 400.
        OauthProvider::Codex => &["max_output_tokens", "temperature", "top_p"],
        // Kimi probed with the full Anthropic parameter surface (system,
        // temperature, top_p, stop_sequences, metadata) — all accepted.
        OauthProvider::Claude | OauthProvider::Grok | OauthProvider::Kimi => &[],
    }
}

/// Effort support per subscription login (spec 0160).
pub fn effort_support(provider: OauthProvider) -> super::EffortSupport {
    match provider {
        OauthProvider::Codex => super::EffortSupport::Verbatim,
        OauthProvider::Claude => super::EffortSupport::Thinking,
        OauthProvider::Grok | OauthProvider::Kimi => super::EffortSupport::Unsupported,
    }
}

/// System-prompt text a login's backend requires the request to open with.
///
/// The Claude subscription backend expects the Claude Code identity line;
/// requests without it are refused. Prepending it is a requirement of the
/// endpoint, not a claim about who is calling.
pub fn required_system_prefix(provider: OauthProvider) -> Option<&'static str> {
    match provider {
        OauthProvider::Claude => {
            Some("You are Claude Code, Anthropic's official CLI for Claude.")
        }
        _ => None,
    }
}

/// Serializes tests that steer credential discovery through environment
/// variables.
///
/// Those variables are process-global while Rust runs tests in parallel
/// threads, so two tests pointing `CODEX_HOME` at different directories
/// interleave — one clearing it between the other's set and read, which
/// surfaces as "not logged in" for a login that is plainly there. Every
/// test that sets or clears one takes this lock first; the rest of the
/// suite still runs in parallel.
#[cfg(test)]
pub(crate) fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn every_provider_has_a_reachable_shape() {
        for p in OauthProvider::ALL {
            assert!(p.endpoint().starts_with("https://"), "{}", p.name());
            assert!(!p.default_model().is_empty(), "{}", p.name());
            assert!(!p.name().is_empty());
        }
    }

    /// The seed list must come from the shared completion catalog, so the
    /// router and smith cannot disagree about what a provider offers.
    #[test]
    fn seed_models_come_from_the_shared_catalog() {
        for p in OauthProvider::ALL {
            let seeds = p.seed_models();
            assert!(!seeds.is_empty(), "{} has no models listed", p.name());
            for model in &seeds {
                let spec = format!("{}:{model}", p.name());
                assert!(
                    construct_protocol::slash::MODEL_COMPLETIONS.contains(&spec.as_str()),
                    "{spec} is not in the shared catalog"
                );
            }
        }
        // The subscription path's short aliases are exactly what a
        // hand-written list would have got wrong.
        assert!(OauthProvider::Claude
            .seed_models()
            .iter()
            .any(|m| m == "sonnet"));
    }

    /// Antigravity is excluded on purpose: no Gemini dialect exists, so a
    /// target for it would mistranslate rather than fail.
    #[test]
    fn antigravity_is_not_offered() {
        assert!(!OauthProvider::ALL
            .iter()
            .any(|p| p.name().contains("antigravity")));
        assert_eq!(OauthProvider::ALL.len(), 4);
    }

    #[test]
    fn claude_reads_a_credentials_file_and_checks_expiry() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let future = (chrono::Utc::now().timestamp() + 3600) * 1000;
        std::fs::write(
            &path,
            serde_json::json!({"claudeAiOauth":{"accessToken":"sk-live","expiresAt":future}})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE", &path);
        let cred = read_credential(OauthProvider::Claude).expect("usable");
        assert_eq!(cred.access_token, "sk-live");

        // An expired token is reported as renewable, not as missing — the
        // user's next step differs.
        let past = (chrono::Utc::now().timestamp() - 60) * 1000;
        std::fs::write(
            &path,
            serde_json::json!({"claudeAiOauth":{"accessToken":"sk-old","expiresAt":past}})
                .to_string(),
        )
        .unwrap();
        let err = read_credential(OauthProvider::Claude).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert!(err.contains("never"), "must say we do not refresh: {err}");
        std::env::remove_var("CONSTRUCT_CLAUDE_CREDENTIALS_FILE");
    }

    #[test]
    fn codex_reads_auth_json_and_its_account_id() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::json!({"tokens":{
                "access_token":"at","refresh_token":"rt","account_id":"acct_1"
            }})
            .to_string(),
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", dir.path());
        let cred = read_credential(OauthProvider::Codex).expect("usable");
        assert_eq!(cred.access_token, "at");
        assert_eq!(cred.account_id.as_deref(), Some("acct_1"));
        let headers = extra_headers(OauthProvider::Codex, &cred);
        assert!(headers.iter().any(|(k, v)| *k == "chatgpt-account-id" && v == "acct_1"));
        assert!(headers.iter().any(|(k, _)| *k == "originator"));
        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn missing_logins_say_how_to_sign_in() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CODEX_HOME", dir.path());
        let err = read_credential(OauthProvider::Codex).unwrap_err();
        assert!(err.contains("not logged in"), "{err}");
        assert!(err.contains("codex"), "{err}");
        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn grok_picks_the_longest_lived_unexpired_entry() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let grok = dir.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        let soon = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        let later = (chrono::Utc::now() + chrono::Duration::hours(5)).to_rfc3339();
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        std::fs::write(
            grok.join("auth.json"),
            serde_json::json!({
                "a": {"key":"tok-soon","expires_at":soon},
                "b": {"key":"tok-later","expires_at":later},
                "c": {"key":"tok-dead","expires_at":past},
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("GROK_HOME", dir.path());
        let cred = read_credential(OauthProvider::Grok).expect("usable");
        assert_eq!(cred.access_token, "tok-later");
        std::env::remove_var("GROK_HOME");
    }

    #[test]
    fn all_grok_entries_expired_reports_expiry_not_absence() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let grok = dir.path().join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        std::fs::write(
            grok.join("auth.json"),
            serde_json::json!({"a": {"key":"tok","expires_at":past}}).to_string(),
        )
        .unwrap();
        std::env::set_var("GROK_HOME", dir.path());
        let err = read_credential(OauthProvider::Grok).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        std::env::remove_var("GROK_HOME");
    }

    #[test]
    fn kimi_reads_its_credentials_file_and_checks_expiry() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials");
        std::fs::create_dir_all(&creds).unwrap();
        let future = chrono::Utc::now().timestamp() + 3600;
        std::fs::write(
            creds.join("kimi-code.json"),
            serde_json::json!({
                "access_token":"kimi-live","refresh_token":"kimi-rt",
                "expires_at":future,"scope":"kimi-code","token_type":"Bearer","expires_in":900
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("KIMI_CODE_HOME", dir.path());
        let cred = read_credential(OauthProvider::Kimi).expect("usable");
        assert_eq!(cred.access_token, "kimi-live");
        assert!(cred.account_id.is_none());

        // Kimi's 15-minute tokens make expiry the common state; the message
        // must name `kimi` as the renewing tool and say we never refresh.
        let past = chrono::Utc::now().timestamp() - 60;
        std::fs::write(
            creds.join("kimi-code.json"),
            serde_json::json!({"access_token":"kimi-old","expires_at":past}).to_string(),
        )
        .unwrap();
        let err = read_credential(OauthProvider::Kimi).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        assert!(err.contains("`kimi`"), "{err}");
        assert!(err.contains("never"), "must say we do not refresh: {err}");
        std::env::remove_var("KIMI_CODE_HOME");
    }

    #[test]
    fn kimi_missing_login_says_how_to_sign_in() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KIMI_CODE_HOME", dir.path());
        let err = read_credential(OauthProvider::Kimi).unwrap_err();
        assert!(err.contains("not logged in"), "{err}");
        assert!(err.contains("kimi"), "{err}");
        std::env::remove_var("KIMI_CODE_HOME");
    }

    /// Missing/expired logins carry the owning CLI's sign-in command so a
    /// client can offer to run it; other blockers must not, because they
    /// are not fixed by signing in.
    #[test]
    fn login_blockers_carry_the_sign_in_command() {
        let _env = test_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("KIMI_CODE_HOME", dir.path());
        let blocker = check_login(OauthProvider::Kimi).unwrap_err();
        assert_eq!(blocker.login_command.as_deref(), Some("kimi"));
        assert_eq!(blocker.reason, read_credential(OauthProvider::Kimi).unwrap_err());
        std::env::remove_var("KIMI_CODE_HOME");
    }

    /// Codex is the one CLI with a dedicated login subcommand that exits
    /// once the login lands; the others run their flow on plain launch.
    #[test]
    fn the_sign_in_command_is_the_owning_clis() {
        assert_eq!(OauthProvider::Codex.login_command(), "codex login");
        assert_eq!(OauthProvider::Claude.login_command(), "claude");
        assert_eq!(OauthProvider::Grok.login_command(), "grok");
        assert_eq!(OauthProvider::Kimi.login_command(), "kimi");
    }

    /// The kimi backend was probed with the full Anthropic parameter
    /// surface and a bare-bearer request; nothing is dropped and nothing
    /// extra is required.
    #[test]
    fn the_kimi_backend_needs_no_decoration() {
        assert!(unsupported_params(OauthProvider::Kimi).is_empty());
        assert!(required_system_prefix(OauthProvider::Kimi).is_none());
        let cred = OauthCredential {
            access_token: "t".into(),
            account_id: None,
        };
        assert!(extra_headers(OauthProvider::Kimi, &cred).is_empty());
    }

    /// REGRESSION: claude routed to codex-oauth failed with
    /// `400 Unsupported parameter: max_output_tokens`. Anthropic requires
    /// `max_tokens`, so every claude request carried one and every
    /// translation produced the parameter the Codex backend refuses.
    #[test]
    fn the_codex_backend_rejects_parameters_the_others_accept() {
        assert!(unsupported_params(OauthProvider::Codex).contains(&"max_output_tokens"));
        assert!(unsupported_params(OauthProvider::Codex).contains(&"temperature"));
        // grok's Responses endpoint takes both; this is per-target, not
        // per-dialect, even though both targets speak Responses.
        assert!(unsupported_params(OauthProvider::Grok).is_empty());
        assert!(unsupported_params(OauthProvider::Claude).is_empty());
    }

    #[test]
    fn the_claude_backend_requires_its_identity_line() {
        assert!(required_system_prefix(OauthProvider::Claude)
            .unwrap()
            .contains("Claude Code"));
        assert!(required_system_prefix(OauthProvider::Grok).is_none());
    }

    #[test]
    fn reads_exp_and_account_id_out_of_a_jwt() {
        use base64::Engine;
        let claims = serde_json::json!({
            "exp": chrono::Utc::now().timestamp() + 3600,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_jwt"}
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("h.{payload}.sig");
        assert!(!jwt_is_expired(&token));
        assert_eq!(jwt_account_id(&token).as_deref(), Some("acct_jwt"));

        let expired = serde_json::json!({"exp": chrono::Utc::now().timestamp() - 10});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&expired).unwrap());
        assert!(jwt_is_expired(&format!("h.{payload}.sig")));
    }
}
