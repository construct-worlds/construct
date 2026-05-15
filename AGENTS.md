# AGENTS.md

## The minibuffer is just another session

Most TUIs make the bottom command bar a special UI primitive. We don't — it's a regular zarvis session, persisted on disk like any other. Differences:

- **Hidden from the list.** `kind: SessionKind::Orchestrator` filters it out of `list_items`.
- **Auto-created.** `SessionManager::ensure_orchestrator()` runs at daemon start.
- **Rendered in the bottom strip.** Same `ItemHistory::replay` pipeline as the main view, just a different Rect.
- **Specialized system prompt.** Zarvis branches on `AGENTD_SESSION_KIND` to act as the fleet dispatcher instead of a worker.
- **Subscribes to fleet events.** A second IPC connection turns other sessions' `Status{AwaitingInput|Errored|Done}` and `ToolApprovalRequest` into `OBSERVATION:` messages the orchestrator can react to.
- **Approvals render inline in the PTY.** No global minibuffer preempt — the panel *is* the PTY.

Everything else — slash commands, tool-block expand/collapse, input queue during turns, persistence across daemon restart, automode, resume — works identically to any zarvis session, *because the minibuffer is one*. Add minibuffer features as session features.

## Rendering across resize and restart

- **Resize is instant.** No full-history replay. Older content keeps its original line wraps; new content uses the new width.
- **History survives daemon restart.** When a harness can resume silently, the prior scrollback stays visible. When a harness must repaint itself on resume, the daemon hands it a clean slate instead — partial reuse leaves the terminal half-rendered.
- **Sessions come back at the size the user last had.** A resumed session must render at the user's current dimensions on the very first frame, not at a creation-time default.
