//! A fake slack-personal MCP backend, speaking the tool contract of spec
//! 0201 over stdio.
//!
//! Used by the daemon's tests and by hand when driving a slack-personal
//! channel end-to-end without Slack:
//!
//! - `FAKE_SLACK_MESSAGES` — path to a JSON array of swept-message objects.
//!   Each `slack_sweep_messages` call returns the ones newer than `after_ts`.
//!   The file is re-read on every sweep, so appending to it while a daemon
//!   polls simulates messages arriving.
//! - `FAKE_SLACK_CALLS` — optional path; every `tools/call` is appended to it
//!   as one JSON line `{"tool": ..., "arguments": ...}` for assertions.
//!
//! `slack_send_message` answers with a fresh timestamp, `slack_create_draft`
//! with an empty object, `slack_read_thread` with the sweep-file messages of
//! that thread.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut sent_serial = 0u64;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue; // notification
        };
        let method = message["method"].as_str().unwrap_or_default();
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-slack-mcp", "version": "0"},
            }),
            "tools/call" => {
                let tool = message["params"]["name"].as_str().unwrap_or_default();
                let arguments = &message["params"]["arguments"];
                record_call(tool, arguments);
                let body = match tool {
                    "slack_sweep_messages" => sweep(arguments),
                    "slack_read_thread" => read_thread(arguments),
                    "slack_send_message" => {
                        sent_serial += 1;
                        serde_json::json!({"ts": format!("9999999999.{sent_serial:06}")})
                    }
                    "slack_create_draft" => serde_json::json!({}),
                    _ => serde_json::json!({"error": format!("unknown tool {tool}")}),
                };
                serde_json::json!({
                    "content": [{"type": "text", "text": body.to_string()}]
                })
            }
            _ => serde_json::json!({}),
        };
        let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}

fn messages_from_file() -> Vec<serde_json::Value> {
    let Ok(path) = std::env::var("FAKE_SLACK_MESSAGES") else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn sweep(arguments: &serde_json::Value) -> serde_json::Value {
    let after = arguments["after_ts"].as_str().unwrap_or("0");
    let after = ts(after);
    let newer: Vec<_> = messages_from_file()
        .into_iter()
        .filter(|message| ts(message["ts"].as_str().unwrap_or("0")) > after)
        .collect();
    serde_json::json!({"messages": newer})
}

fn read_thread(arguments: &serde_json::Value) -> serde_json::Value {
    let channel = arguments["channel"].as_str().unwrap_or_default();
    let thread = arguments["thread_ts"].as_str().unwrap_or_default();
    let in_thread: Vec<_> = messages_from_file()
        .into_iter()
        .filter(|message| {
            message["channel"].as_str() == Some(channel)
                && (message["thread_ts"].as_str() == Some(thread)
                    || message["ts"].as_str() == Some(thread))
        })
        .collect();
    serde_json::json!({"messages": in_thread})
}

fn record_call(tool: &str, arguments: &serde_json::Value) {
    let Ok(path) = std::env::var("FAKE_SLACK_CALLS") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(
        file,
        "{}",
        serde_json::json!({"tool": tool, "arguments": arguments})
    );
}

fn ts(value: &str) -> (u64, u64) {
    let (secs, frac) = value.split_once('.').unwrap_or((value, "0"));
    let frac = format!("{frac:0<6}");
    (
        secs.parse().unwrap_or(0),
        frac.get(..6).and_then(|f| f.parse().ok()).unwrap_or(0),
    )
}
