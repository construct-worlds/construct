//! Kimi Code OAuth provider — Moonshot's Anthropic-compatible coding backend.
//!
//! Reads the Kimi Code subscription OAuth credentials
//! (`~/.kimi-code/credentials/kimi-code.json`) and calls
//! `https://api.kimi.com/coding/v1/messages` with a `Bearer` access token.
//! The backend speaks the Anthropic Messages wire (the Kimi CLI bundles the
//! Anthropic SDK pointed at this base), so Smith's tools are passed as
//! native `tools` and the shared wire helpers in [`super::anthropic`] do
//! the rest. Unlike `claude-oauth`, no beta header and no identity system
//! block are required — a bare-bearer request was probed and accepted.
//!
//! Kimi access tokens are short-lived (~15 minutes observed), so refresh is
//! not an edge case here: nearly every turn starts by rotating the token
//! through Kimi's public device-flow client id and writing the result back
//! to the same file the official CLI reads, preserving every other field.
//!
//! Compliance note: as with `claude-oauth`, routing the subscription OAuth
//! token straight at the backend is the user's own subscription on their
//! own machine, but it is not a surface Moonshot documents for third-party
//! use. The token endpoint and client id below are reverse-engineered from
//! the Kimi Code CLI and may change without notice.

use super::{LlmProvider, Message, ProviderTurn, TextSink, ToolSpec};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const MESSAGES_URL: &str = "https://api.kimi.com/coding/v1/messages";
/// OAuth token mint/refresh endpoint the Kimi Code client uses.
const TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
/// Public Kimi Code OAuth client id (reverse-engineered; not a secret).
const OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// Refresh when the access token is within this much of expiry. Kept small
/// because the tokens themselves only live ~900 s.
const REFRESH_LEEWAY_SECS: u64 = 120;

pub struct KimiOauth {
    http: reqwest::Client,
    state: Arc<Mutex<AuthState>>,
}

struct AuthState {
    path: PathBuf,
    creds: Creds,
}

struct Creds {
    access_token: String,
    refresh_token: String,
    /// Unix-epoch seconds (the Kimi CLI's own unit). 0 when unknown
    /// (forces a refresh).
    expires_at_secs: u64,
    /// The full credential JSON document, preserved so refresh writes back
    /// every field the official client wrote — only the tokens change.
    doc: Value,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refresh if the expiry is unknown, already past, or inside the leeway window.
fn needs_refresh(expires_at_secs: u64, now: u64) -> bool {
    expires_at_secs == 0 || now + REFRESH_LEEWAY_SECS >= expires_at_secs
}

fn parse_creds(raw: &str) -> Result<Creds> {
    let doc: Value =
        serde_json::from_str(raw.trim()).context("parse Kimi Code credentials JSON")?;
    let access_token = doc
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let refresh_token = doc
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_at_secs = doc.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0);
    if access_token.is_empty() {
        bail!(
            "Kimi Code credentials have no access_token; run `kimi login` and sign in with \
             your Kimi subscription before using the kimi-oauth provider"
        );
    }
    Ok(Creds {
        access_token,
        refresh_token,
        expires_at_secs,
        doc,
    })
}

fn locate_credentials() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("CONSTRUCT_KIMI_OAUTH_CREDENTIALS") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let home_dir = match std::env::var("KIMI_CODE_HOME") {
        Ok(h) if !h.trim().is_empty() => PathBuf::from(h),
        _ => {
            let home = std::env::var("HOME").map_err(|_| anyhow!("$HOME is not set"))?;
            PathBuf::from(home).join(".kimi-code")
        }
    };
    let p = home_dir.join("credentials").join("kimi-code.json");
    if p.exists() {
        return Ok(p);
    }
    Err(anyhow!(
        "could not find Kimi Code OAuth credentials (checked \
         $CONSTRUCT_KIMI_OAUTH_CREDENTIALS and {}). Run `kimi login` and sign in with your \
         Kimi subscription first.",
        p.display()
    ))
}

fn save_credentials(path: &PathBuf, doc: &Value) -> Result<()> {
    let json = serde_json::to_string(doc).context("serialize credentials")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

impl KimiOauth {
    pub fn from_env() -> Result<Self> {
        let path = locate_credentials()?;
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let creds = parse_creds(&raw)?;
        let http = reqwest::Client::builder()
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            http,
            state: Arc::new(Mutex::new(AuthState { path, creds })),
        })
    }

    /// Refresh the access token when it's near/at expiry and persist the
    /// rotated tokens back to the credential file. The caller holds the auth
    /// lock across the whole refresh so concurrent turns don't double-rotate.
    async fn ensure_fresh(&self, state: &mut AuthState) -> Result<()> {
        if !needs_refresh(state.creds.expires_at_secs, now_secs()) {
            return Ok(());
        }
        if state.creds.refresh_token.is_empty() {
            bail!(
                "Kimi Code access token expired and no refresh_token is present; run \
                 `kimi login` to re-authenticate"
            );
        }
        // The Kimi token endpoint takes a form-encoded body (matching its
        // own CLI), not JSON.
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", state.creds.refresh_token.as_str()),
            ("client_id", OAUTH_CLIENT_ID),
        ];
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .context("POST oauth/token")?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            let txt = String::from_utf8_lossy(&bytes);
            bail!(
                "Kimi OAuth token refresh failed ({status}): {txt}. Run `kimi login` to \
                 re-authenticate."
            );
        }
        #[derive(Deserialize)]
        struct RefreshResp {
            #[serde(default)]
            access_token: Option<String>,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
        }
        let r: RefreshResp =
            serde_json::from_slice(&bytes).context("parse oauth/token response")?;
        let new_access = r
            .access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("oauth/token response had no access_token"))?;
        state.creds.access_token = new_access;
        if let Some(rt) = r.refresh_token.filter(|s| !s.is_empty()) {
            state.creds.refresh_token = rt;
        }
        // Default to the observed 15-minute lifetime if the server omits
        // expires_in.
        let expires_in = r.expires_in.unwrap_or(900);
        state.creds.expires_at_secs = now_secs() + expires_in;

        // Write the rotated tokens back into the preserved doc, then persist.
        state.creds.doc["access_token"] = json!(state.creds.access_token);
        state.creds.doc["refresh_token"] = json!(state.creds.refresh_token);
        state.creds.doc["expires_at"] = json!(state.creds.expires_at_secs);
        state.creds.doc["expires_in"] = json!(expires_in);
        // Best-effort: we already hold a valid in-memory token, so a persist
        // failure must not fail the live turn (it only risks a re-refresh next
        // process start).
        if let Err(e) = save_credentials(&state.path, &state.creds.doc) {
            eprintln!("kimi-oauth: warning: failed to persist refreshed token: {e}");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl LlmProvider for KimiOauth {
    fn name(&self) -> &str {
        "kimi-oauth"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let access_token = {
            let mut state = self.state.lock().await;
            self.ensure_fresh(&mut state).await?;
            state.creds.access_token.clone()
        };

        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "stream": true,
            "messages": super::anthropic::messages_to_anthropic(messages),
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(super::anthropic::tools_to_anthropic(tools));
        }

        let resp = self
            .http
            .post(MESSAGES_URL)
            .header("authorization", format!("Bearer {access_token}"))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("kimi-oauth POST /coding/v1/messages")?;
        super::anthropic::read_message_stream(resp, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_creds_reads_the_kimi_file_shape() {
        let raw = r#"{"access_token":"at","refresh_token":"rt","expires_at":1785377826,"scope":"kimi-code","token_type":"Bearer","expires_in":900}"#;
        let c = parse_creds(raw).unwrap();
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.expires_at_secs, 1785377826);
        // The untouched fields survive in the preserved doc for write-back.
        assert_eq!(c.doc["scope"], "kimi-code");
        assert_eq!(c.doc["token_type"], "Bearer");
    }

    #[test]
    fn parse_creds_rejects_missing_token() {
        assert!(parse_creds(r#"{"refresh_token":"x"}"#).is_err());
    }

    #[test]
    fn needs_refresh_window() {
        let now = 1_000_000u64;
        assert!(needs_refresh(0, now)); // unknown expiry
        assert!(needs_refresh(now, now)); // already expired
        assert!(needs_refresh(now + REFRESH_LEEWAY_SECS - 1, now)); // inside leeway
        assert!(!needs_refresh(now + REFRESH_LEEWAY_SECS + 1, now)); // comfortably valid
    }
}
