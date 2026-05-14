//! Zarvis — agentd's built-in multi-provider agent harness.
//!
//! Talks to OpenAI / Anthropic / Ollama directly (no vendor CLI required),
//! runs its own agent loop, and executes shell + filesystem +
//! agentd-control tools on the model's behalf. See README for the full
//! design.

mod agent;
mod context;
mod provider;
mod tools;

use agentd_protocol::adapter::run;
use agentd_protocol::{Capabilities, InitializeResult, SessionEvent};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "zarvis".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
            ..Default::default()
        },
    };
    run(metadata, |params, ctx| async move {
        let resolved = match agent::resolve_model(&params) {
            Ok(r) => r,
            Err(e) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: format!(
                        "{e}\n\nzarvis needs one of: AGENTD_ZARVIS_MODEL set, \
                         ANTHROPIC_API_KEY set, OPENAI_API_KEY set, or a local \
                         Ollama (set OLLAMA_HOST if not at localhost:11434)."
                    ),
                });
                ctx.emit.emit(SessionEvent::Done { exit_code: 2 });
                return;
            }
        };
        if let Err(e) = agent::run(params, ctx, resolved).await {
            // The loop already emits Error for in-loop failures; this
            // is for fatal-spawn-time issues that escape.
            tracing::warn!(error = ?e, "zarvis agent loop returned with error");
        }
    })
    .await
}
