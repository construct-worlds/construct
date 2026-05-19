//! Remote-control state: the auth token that gates the WebSocket
//! transport, and (in Phase 1C) the public tunnel URL once it's
//! discovered. Lives behind an `Arc` so the WS upgrade handler, the
//! tunnel subprocess monitor, and any future status-display path can
//! all share one view of "what is the current remote URL + token?".
//!
//! Token model is intentionally simple for Phase 1: one daemon-
//! lifetime token, minted at startup, required in the WS upgrade URL
//! path (`/t/<token>`). No per-session scoping yet, no rotation yet.
//! Both are reasonable follow-ups once we have a web client driving
//! real usage and can see what the access patterns are.

use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Shared handle to remote-control state. Cheap to clone (one `Arc`
/// for the inner state). The token field is immutable for the
/// lifetime of the daemon, so it's accessible synchronously; only
/// the tunnel URL (set after cloudflared starts) needs async access.
#[derive(Clone)]
pub struct RemoteState {
    /// Auth token. Required (in the URL path) for any WS upgrade.
    /// Constant for the lifetime of one daemon process. Public
    /// because the upgrade callback runs synchronously and needs to
    /// read this without `.await`.
    token: Arc<String>,
    tunnel_url: Arc<RwLock<Option<String>>>,
}

impl RemoteState {
    /// Mint a fresh state with a new token. Called once at daemon
    /// startup when the WS listener is enabled.
    pub fn new() -> Self {
        let token = Uuid::new_v4().simple().to_string();
        Self {
            token: Arc::new(token),
            tunnel_url: Arc::new(RwLock::new(None)),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Compare a candidate token to the stored one in constant time.
    /// Returns true only on exact match. Length-mismatch short-
    /// circuits — the wire shape leaks "wrong length" but that's
    /// not a real attacker advantage against 122 bits of UUID-v4
    /// randomness in a known-length token.
    pub fn token_matches(&self, candidate: &str) -> bool {
        let real = &self.token;
        if candidate.len() != real.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (a, b) in candidate.bytes().zip(real.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Update the public tunnel URL. Called by the cloudflared
    /// monitor once it reads the `*.trycloudflare.com` URL out of
    /// the subprocess output.
    pub async fn set_tunnel_url(&self, url: Option<String>) {
        *self.tunnel_url.write().await = url;
    }

    pub async fn tunnel_url(&self) -> Option<String> {
        self.tunnel_url.read().await.clone()
    }
}

impl Default for RemoteState {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the token from an HTTP request URI path. Accepts the
/// shape `/t/<token>` (and trailing path segments, ignored). Returns
/// `None` when the path doesn't match.
pub fn token_from_uri_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/t/")?;
    let token = rest.split('/').next()?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Token extraction from URI paths. Strict on the `/t/` prefix
    /// so we don't accidentally accept a request to `/token` (or
    /// any other route the web client might add later).
    #[test]
    fn extracts_token_from_t_path() {
        assert_eq!(token_from_uri_path("/t/abc123"), Some("abc123"));
        assert_eq!(token_from_uri_path("/t/abc123/some/extra"), Some("abc123"));
        assert_eq!(token_from_uri_path("/"), None);
        assert_eq!(token_from_uri_path("/t/"), None);
        assert_eq!(token_from_uri_path("/t"), None);
        assert_eq!(token_from_uri_path("/token/abc123"), None);
        assert_eq!(token_from_uri_path(""), None);
    }

    /// `token_matches` is exact-match only. Empty / wrong-length /
    /// off-by-one inputs all reject.
    #[test]
    fn token_matches_is_exact_only() {
        let s = RemoteState::new();
        let real = s.token().to_string();
        assert!(s.token_matches(&real));
        // Length mismatch.
        assert!(!s.token_matches(&format!("{real}x")));
        assert!(!s.token_matches(&real[..real.len() - 1]));
        // Wrong content of same length.
        let mut wrong = real.clone();
        let first = wrong.remove(0);
        wrong.push(first); // rotate one char
        assert!(!s.token_matches(&wrong));
        // Empty.
        assert!(!s.token_matches(""));
    }

    /// Fresh `RemoteState`s mint independent tokens — no static /
    /// shared state.
    #[test]
    fn fresh_state_mints_unique_tokens() {
        let a = RemoteState::new();
        let b = RemoteState::new();
        assert_ne!(a.token(), b.token());
        // UUID-v4 simple form is 32 hex chars.
        assert_eq!(a.token().len(), 32);
    }

    /// Tunnel URL is settable + readable.
    #[tokio::test]
    async fn tunnel_url_round_trip() {
        let s = RemoteState::new();
        assert_eq!(s.tunnel_url().await, None);
        s.set_tunnel_url(Some("https://x.trycloudflare.com".into())).await;
        assert_eq!(
            s.tunnel_url().await.as_deref(),
            Some("https://x.trycloudflare.com"),
        );
        s.set_tunnel_url(None).await;
        assert_eq!(s.tunnel_url().await, None);
    }
}
