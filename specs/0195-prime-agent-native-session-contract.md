# 0195-prime-agent-native-session-contract

Status: accepted
Date: 2026-08-05
Area: harness
Scope: How Construct runs, identifies, resumes, resets, and forks Prime Agent sessions.

## Decision

Prime Agent is exposed under the canonical Construct harness id
`prime-agent`, matching its installed executable and public product name.
Space-constrained session labels may render the compact name `prime` without
changing the persisted harness id or command syntax.

The adapter uses Prime Agent's public interactive TUI and structured JSON mode.
It selects a private session directory inside each Construct session, captures
the native UUID from the JSONL session header, and persists that UUID in a
Prime-specific sidecar. Resume and native fork resolve the authoritative header
UUID back to its JSONL path; they must not assume the filename UUID is the same.

Prime Agent and Pi may share translation/runtime code only where their public
schemas remain compatible. Their command overrides, mode overrides, session
directories, native-id sidecars, fork variables, binaries, and resume flags
remain separate flavor inputs. Neither harness may read or adopt the other's
private sessions.

## Reason

Prime Agent 0.7.0 intentionally retains Pi's version-3 JSONL messages and JSON
event stream, including model/thinking changes, tool calls/results, token usage,
and exact cost. Sharing that parser avoids two copies drifting apart.

Prime Agent differs operationally: it launches as `prime-agent`, resumes with
`--resume`, uses a different global configuration home, and writes session
files whose filename UUID differs from the native UUID in the header. Treating
it as merely another Pi binary would break restart/fork and risk state crossing
between the two installed harnesses.

## Consequences

- `construct new prime-agent` is the stable creation command.
- Interactive, headless, restart, reset lineage, and same-harness native fork
  use Prime Agent's own persisted conversation.
- Construct-owned Prime Agent state is deleted or archived with its Construct
  session and does not appear in the CLI's ordinary global session picker.
- A future Prime Agent schema divergence must be handled behind its flavor
  boundary or split into its own parser; compatibility must not be assumed from
  ancestry alone.
- Provider traffic remains pass-through until a stable endpoint/dialect and
  trust-channel contract is probe-verified for Prime Agent's configurable
  providers.

## Non-Goals

- Injecting Construct MCP tools into Prime Agent's extension system.
- Translating Prime Agent's native tool policy into Construct approval modes.
- Treating `prime` as a second persisted harness id or duplicate picker entry.
