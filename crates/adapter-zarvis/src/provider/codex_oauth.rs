//! Codex OAuth provider — bills against the user's ChatGPT subscription
//! by reading `~/.codex/auth.json` (the credential file the
//! `codex login` command writes) and routing requests through
//! `POST https://chatgpt.com/backend-api/codex/responses`.
//!
//! This is NOT the public platform API at `api.openai.com/v1/responses`:
//!
//!   - Endpoint host + path differ (`chatgpt.com/backend-api/codex/...`).
//!   - Auth is `Authorization: Bearer <oauth-access-token>` from
//!     `auth.json`, plus a `ChatGPT-Account-ID` header carrying the
//!     `account_id` claim from the OAuth `id_token` JWT.
//!   - Required `OpenAI-Beta` + `originator: codex_cli_rs` + non-default
//!     User-Agent — the default `reqwest/*` UA gets blocked by
//!     Cloudflare (verified against `openai/codex` CLI source, which
//!     hits the same wall on WSL2/Linux).
//!   - The request body uses Responses-API shape (`instructions` +
//!     `input` array), not Chat Completions. Models are Codex-specific
//!     strings (`gpt-5`, `gpt-5-codex`, `gpt-5-codex-mini`).
//!
//! Status: scaffolding. Implementation lands in subsequent commits;
//! today this just compiles into the dispatcher and returns a clear
//! "not implemented" error so the rest of the workspace builds.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{LlmProvider, Message, ProviderTurn, TextSink, ToolSpec};

pub struct CodexOauth {
    // Auth state lives behind a Mutex because refresh rotates the
    // refresh_token and we must persist atomically. Stubbed for now;
    // wired up in the auth-loading commit.
    _placeholder: (),
}

impl CodexOauth {
    /// Construct from the on-disk credential file. Returns an error if
    /// `~/.codex/auth.json` is missing, malformed, or doesn't carry an
    /// OAuth `access_token` (i.e. the user is in API-key mode, not the
    /// subscription mode this provider serves).
    pub fn from_env() -> Result<Self> {
        Ok(Self { _placeholder: () })
    }
}

#[async_trait]
impl LlmProvider for CodexOauth {
    fn name(&self) -> &str {
        "codex-oauth"
    }

    async fn complete(
        &self,
        _model: &str,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        Err(anyhow!(
            "codex-oauth provider: not yet implemented. Tracking issue: \
             see feat/codex-oauth-provider branch. Use `openai:` for the \
             platform API in the meantime."
        ))
    }
}
