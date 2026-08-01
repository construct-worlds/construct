# 0062-webui-view-mode-is-per-session

Status: accepted
Date: 2026-06-29
Area: webui
Scope: The web UI's selected view mode (Terminal / Chat / Playbook) is remembered per session and restored when switching back to that session.

## Decision

In the web UI, the Terminal / Chat / Playbook selector is a per-session choice, not a global one. Each session independently remembers which of the three views it last showed, and switching to a session restores that session's own view — never the view the previously-focused session happened to be in.

- All three modes participate equally. Playbook is a peer of Terminal and Chat (see [0059-webui-playbook-view-parity](0059-webui-playbook-view-parity.md)), so a session left in Playbook reopens in Playbook, exactly as one left in Terminal reopens in Terminal.
- The stored preference persists across reloads (it is saved client-side) and is keyed by session id; removing a session drops its stored preference.
- Absence of a stored preference falls back to the session's natural surface: Terminal for a PTY-backed session, otherwise Chat. The selector only offers the surfaces a session actually has, except Playbook, which every session offers.
- When a single-surface session leaves Playbook back to its only other surface, its stored Playbook preference is dropped so the natural surface is what gets restored next time.

## Reason

Browsing the fleet means moving between sessions of different kinds — a PTY harness the user reads as a terminal, a chat-only adapter, a session whose Playbook the user is editing. Carrying one global view across that switch is wrong: it forces, say, a terminal session to render as Playbook just because the prior session was in Playbook, and it loses the view the user had deliberately chosen for the session they are returning to. Per-session memory makes each session reopen where the user left it, which is the only behavior that scales as the fleet grows.

## Consequences

- Any code path that switches the focused session must restore the *incoming* session's remembered view, not reuse the outgoing session's current view state. A global "current mode" variable may track what is on screen right now, but it must not be the source of truth for what an arriving session should show.
- Entering any mode (including Playbook) must record that choice against the current session before/as the surface is shown, so a later return restores it.
- The persistence layer must accept all three mode values. Adding a fourth view mode in the future requires updating both the load and save filters or the new mode will be silently dropped on reload.

## Non-Goals

- Sharing the per-session view preference across clients or with the TUI. This is a web-client convenience persisted locally; it is not part of the daemon contract.
- Persisting transient within-view state (scroll position, selection) — that is covered by the relevant view's own behavior, not by this selection memory.
