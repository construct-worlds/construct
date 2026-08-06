//! Prime Agent adapter entry point.
//!
//! Prime Agent intentionally retains Pi's JSON event and JSONL session
//! formats, so its runtime lives in the shared Pi-family adapter core while
//! this crate supplies a first-class Construct adapter boundary.

pub async fn run() -> anyhow::Result<()> {
    construct_adapter_pi::run_prime_agent().await
}
