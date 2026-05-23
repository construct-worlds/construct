# Agent-controlled agentd (MCP)


An agent (claude / codex) running inside an agentd session can drive the
daemon itself — list other sessions, read their PTY output, send input,
spawn helper sessions, browse with Chrome DevTools, etc. — via an MCP
stdio server, `agentd-mcp`.

When the claude / codex adapter spawns the child CLI, it automatically:
- Writes a per-session MCP config under `$STATE_DIR/mcp/<session_id>.json`
- Passes `--mcp-config <path>` to the child
- Sets `AGENTD_SESSION_ID=<session_id>` in the child's environment

The MCP server exposes a read + write tool surface that mirrors the IPC:
`agentd_whoami`, `agentd_list_sessions`, `agentd_get_session`,
`agentd_get_transcript`, `agentd_get_output`, `agentd_get_diff`,
`agentd_list_harnesses`, `agentd_create_session`, `agentd_send_input`,
`agentd_send_keys` (raw PTY bytes), `agentd_interrupt_session`,
`agentd_stop_session`, `agentd_kill_session`, `agentd_delete_session`,
`agentd_pin_session`, `agentd_rename_session`.

It also exposes browser tools for MCP-capable harnesses:
`browser_open`, `browser_inspect`, `browser_screenshot`, and
`browser_eval`. Browser tools emit a `BrowserPreview` event back to the
calling session, so the TUI thumbnail window updates for claude/codex MCP
calls the same way it does for zarvis-native browser calls.

`agy`/Antigravity currently receives `AGENTD_SESSION_ID`, but its CLI has
no MCP injection flag; browser tools become available there once the
upstream CLI exposes an MCP server configuration surface.

Opt out with `AGENTD_INJECT_MCP=0` in the daemon's environment.
