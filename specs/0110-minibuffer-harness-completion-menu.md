# 0110-minibuffer-harness-completion-menu

Status: accepted
Date: 2026-07-25
Area: tui
Scope: How the minibuffer presents a growing set of harness choices for new sessions and forks.

## Decision

The new-session and fork harness pickers render as a completion menu anchored
above the minibuffer input. Each row shows the harness name, its short
description, and current availability. Available entries precede unavailable
entries while preserving registration order within each group. The synthetic
`project` action is always the first new-session entry.

Typing filters rows case-insensitively by name or description. Up and Down move
the highlighted row, with Control-P and Control-N as equivalent previous/next
bindings. Scrolling the mouse wheel over the picker moves the highlight with
the same wrapping behavior. Enter chooses it, Tab completes its name into the
input, and Esc cancels. Rows remain clickable. The menu shows a bounded number
of rows and scrolls to keep the current highlight visible.

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

## Consequences

Future harnesses must remain usable without adding dedicated picker layout or
key bindings. Clients must treat availability detail as opaque display text.
Additional capability information should use progressive disclosure rather
than accumulating badges on every row.

The picker remains part of the minibuffer interaction model: opening it does
not obscure the fleet with a centered dialog, and its keyboard, mouse, cancel,
and fork-default behavior must stay consistent with other minibuffer
completions.

## Non-Goals

This decision does not rank harnesses by subjective recommendation, define
harness categories, or change session-creation and fork protocol calls.
