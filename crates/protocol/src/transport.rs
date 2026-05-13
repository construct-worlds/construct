//! Line-delimited JSON over async I/O.
//!
//! Each message is one JSON value followed by `\n`. This is dead-simple to
//! debug (`tail -f`, `jq` in pipes) and matches what the underlying CLIs we
//! wrap already emit.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Read one newline-delimited JSON message. Returns `Ok(None)` on EOF.
pub async fn read_message<R>(reader: &mut R) -> Result<Option<serde_json::Value>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid JSON line: {trimmed}"))?;
        return Ok(Some(v));
    }
}

/// Write a single JSON message followed by a newline. Flushes after write.
pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    writer.write_all(s.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}
