# 0069-configure-dialog

Status: accepted
Date: 2026-07-06
Area: tui
Scope: The `/configure` dialog's role as the single interactive setup surface for harness and smith-auth onboarding, and when it auto-opens.

## Decision

`/configure` (palette command, always reachable via `M-x configure` / `: configure`) opens a centered modal dialog with two tabs:

- **Harnesses** — every registered harness with its live probe status (spec 0068's `available` + `detail`), refreshed every few seconds while the dialog is open. Selecting a row shows a diagnosis pane with the daemon's `detail` plus client-side "how to fix" guidance (install a CLI, log in, check `PATH`) keyed by harness name, with a generic fallback for community adapters.
- **Smith auth** — every auth method the built-in `smith` harness supports, each with live-detected status (spec 0069's `smith.auth_status` IPC method), and a way to pick one as smith's default (see [[0070-smith-model-pin-persistence]]). One of the listed methods is "Auto-detect": its `available` status must reflect exactly what smith's real no-pin ladder would find (direct API keys only — see [[0071-smith-no-implicit-fallback]]), not whether *any* method on the tab is usable. OAuth subscriptions and Ollama always require picking their own explicit row (which pins `CONSTRUCT_SMITH_MODEL`); a subscription or Ollama being detected must never make "Auto-detect" show as ready, since a session started with no pin wouldn't actually find it.

Navigation is Left/Right (or Tab/Shift-Tab) to switch tabs, Up/Down to move the row selection, Enter to act on the smith-auth tab's selection, Esc to close. Mouse click is supported only for switching tabs by clicking a tab header — row selection stays keyboard-only. The dialog owns every other keystroke *except* the global quit chord: `C-x C-c` still quits the TUI while the dialog is open, because the dialog can auto-open on a user's very first launch and the welcome card advertises that exact shortcut underneath it. No other chord-bound action fires while the dialog is open — quit is a deliberate, narrow exception to the "topmost modal owns all input" rule, not a general reopening of the keymap.

The dialog opens automatically, once per condition, when:

- **First run**: no on-disk marker recording a prior open exists yet. The marker is written the moment the dialog *opens* (auto or via the palette), not when it's dismissed — a user who quits the TUI (`C-x C-c`) while the dialog is still on screen must not be re-nagged on the next launch just because they never got around to closing it.
- **No agent harness available**: every registered harness except `shell` reports `available: false`. This check re-runs every time the dialog would otherwise auto-open (i.e., on every TUI start), independent of the first-run marker — a machine that loses its only working harness (e.g. an expired subscription) gets nagged again even after the first-run marker is set.

Both conditions are checked once at TUI startup, using the harness list already fetched during normal startup (no extra round trip needed to decide whether to auto-open). The dialog is always reopenable via the palette regardless of either condition.

## Reason

Before this dialog, a user with no configured credentials discovered that fact only by creating a session and watching it fail — there was no single place to see every harness's status, every smith auth method's status, and how to fix each, in one screen. Auto-opening on first run gets a fresh install to a working state without the user needing to know `/configure` exists; auto-opening whenever no agent harness is usable (not just on first run) means a credential expiring or a CLI disappearing from `PATH` re-surfaces the same guidance instead of leaving the user to rediscover it via a failed session start.

## Consequences

- Every registered harness's `available`/`detail` (spec 0068) and every smith auth method's live status must stay cheap enough to probe every few seconds without user-visible lag — the dialog reuses the same probing paths the welcome card and harness picker already rely on, not a separate heuristic.
- The first-run marker is a dedicated small file in the state directory, not a field folded into the general `tui-state.json` blob — checking "has this been shown before" must not require parsing the full persisted UI state.
- Because the "no agent harness available" condition re-checks every startup, a user who deliberately runs with only `shell` configured (e.g. a pure terminal-multiplexer use case) sees the dialog every time they launch the TUI. This is accepted: `/configure` is only one keystroke to dismiss, and the alternative (never re-checking) would leave a broken setup silently un-nagged after the first dismissal.
- A modal that can auto-open at startup with no prior user action (this dialog is the only one today) must keep the global quit chord reachable regardless of what else it swallows — the welcome card advertises `C-x C-c` right underneath it, and a modal a user never chose to open cannot be allowed to make that shortcut a lie. This is narrower than a blanket rule for every modal in the TUI: a dialog the user explicitly summoned (e.g. the session picker) has already proven the keymap works for them, so quitting via its own dismiss key first is a reasonable expectation there.

## Non-Goals

- The dialog does not accept secret/API-key text entry — picking a smith auth method whose credential is missing shows guidance for obtaining it, not a form field. See [[0070-smith-model-pin-persistence]].
- The dialog does not apply a smith-auth pick to already-running adapters; it only writes daemon config for sessions started after a restart.
- This spec does not change harness availability semantics themselves (see spec 0068) — only how that data is presented in a dedicated setup surface.

## Examples

- A fresh install with no API keys and no CLIs installed: the TUI's very first frame shows the dialog, Harnesses tab, every wrapper harness (`claude`, `codex`, `antigravity`, `grok`) dimmed unavailable, `shell` available, `smith` unavailable with a detail pointing at the smith-auth tab.
- A user exports `ANTHROPIC_API_KEY` in a different terminal while the dialog is open: within the next refresh tick, the smith-auth tab's "Anthropic API key" row flips from not-detected to detected without closing and reopening the dialog.
- A user dismisses the dialog on first run, then a week later their Claude Code OAuth token's underlying credential file is deleted and no other harness has a credential: the next TUI launch reopens the dialog even though the first-run marker is already set.
- A machine has only a Claude Code subscription credential (no `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/`GEMINI_API_KEY`): the smith-auth tab shows "Claude subscription" as detected and "Auto-detect" as *not* detected — picking "Claude subscription" (which pins `CONSTRUCT_SMITH_MODEL = "claude-oauth:..."`) is required; leaving the pin on "Auto-detect" still errors at session start.
- A brand-new install: the dialog auto-opens on the very first frame (first-run marker written immediately). The user presses `C-x C-c` without ever pressing Esc — the TUI quits normally, and the next launch does not reopen the dialog for the first-run reason (though it still will if no agent harness ended up configured).
