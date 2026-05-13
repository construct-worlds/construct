# agentd

A terminal "agent fleet" — run and supervise multiple coding-agent sessions across heterogeneous harnesses (Claude Code, Codex, generic shell, ...) from one TUI.

Status: **early — milestone 1 in progress.**

## Layout

- `crates/protocol` — wire types for AHP (daemon-adapter) and IPC (client-daemon)
- `crates/daemon` — `agentd` daemon
- `crates/cli` — `agent` TUI client
- `crates/adapter-shell` — generic shell adapter
- `crates/adapter-claude` — Claude Code adapter
- `crates/adapter-codex` — Codex adapter

## License

MIT
