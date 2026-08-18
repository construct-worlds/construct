# 0110-minibuffer-harness-completion-menu

Status: accepted
Date: 2026-08-18
Area: tui
Scope: How the minibuffer presents creation actions and a growing set of harness choices for new sessions and forks.

## Decision

The new-session and fork harness pickers render as a completion menu anchored
above the minibuffer input. Each row shows the harness name, its short
description, and current availability. Available entries precede unavailable
entries while preserving registration order within each group. The synthetic
creation actions are always the first new-session entries: `project` first,
then `operator`. Choosing `project` asks for its name. Choosing `operator`
opens a focused, unsaved operator editor immediately, with an editable name
that is made unique by appending a numeric suffix when needed. Operator
creation has no separate `/serve` slash-command path.

Typing filters rows case-insensitively by name or description. Up and Down move
the highlighted row, with Control-P and Control-N as equivalent previous/next
bindings. Scrolling the mouse wheel over the picker moves the highlight with
the same wrapping behavior. Enter chooses it, Tab completes its name into the
input, and Esc cancels. When Tab restores the full list after filtering, the
completed entry remains highlighted so a following Enter chooses the value
shown in the input. Rows remain clickable. The menu shows a bounded number of
rows and scrolls to keep the current highlight visible.
Its height is derived from the complete, unfiltered candidate set and remains
fixed while typing narrows the visible rows, so the input and main content do
not move between keystrokes.

Unavailable harnesses remain discoverable but cannot be selected. Highlighting
one replaces its description with the daemon's availability detail, and
clicking or pressing Enter reports the same reason.

The fork picker initially highlights and pre-fills the source harness while
showing the complete list. This keeps same-harness fork as the one-Enter fast
path without hiding cross-harness choices.

On terminals too short to show the full menu, candidate rows yield space before
the input row does. The minibuffer input always remains visible.

## Reason

A pipe-separated line of names becomes difficult to scan as the supported
harness set grows and does not explain how similarly named choices differ.
Harness descriptions and live availability already exist, so presenting them
as rows improves recognition without adding a second source of harness
metadata or turning session creation into a modal workflow.

Projects, operators, and sessions are peer things a user may mean to create
with the new-item chord. Keeping those choices together gives `C-x C-f` one
predictable creation vocabulary and avoids a separate slash command that is
easy to miss and preserves the superseded service terminology.

## Consequences

Future harnesses must remain usable without adding dedicated picker layout or
key bindings. Clients must treat availability detail as opaque display text.
Additional capability information should use progressive disclosure rather
than accumulating badges on every row.

New top-level creation kinds belong beside `project` and `operator` when they
share this lightweight flow; they should not acquire one-off slash commands.

The picker remains part of the minibuffer interaction model: opening it does
not obscure the fleet with a centered dialog, and its keyboard, mouse, cancel,
and fork-default behavior must stay consistent with other minibuffer
completions.

Filtering may leave empty menu rows because preserving a stable layout takes
priority over collapsing the picker around a small result set.

## Non-Goals

This decision does not rank harnesses by subjective recommendation, define
harness categories, or change session-creation and fork protocol calls.
