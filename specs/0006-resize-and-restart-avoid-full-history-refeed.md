# 0006-resize-and-restart-avoid-full-history-refeed

Status: accepted
Date: 2026-05-30
Area: tui
Scope: Applies to TUI resize, zoom, restart, and terminal/history rendering.

## Decision

Resize should be instant and should not refeed full history. Existing content may keep its previous wrapping; new content should use the new dimensions. Restart should preserve history only when a harness can resume without repainting over an incompatible terminal state.

## Reason

Full history refeed makes resize and zoom slow for long-running sessions, and it can corrupt or duplicate terminal-like output. Users resize panes and terminals frequently; those operations must feel like layout changes, not transcript reconstruction.

Preserving old wraps is an acceptable tradeoff for speed and stability.

## Consequences

Rendering systems should be append-oriented after resize. They should avoid using resize as a reason to recompute every previous visual line.

When a harness cannot resume cleanly from prior terminal state, the daemon should prefer a clean slate over partial reuse that leaves the terminal half-rendered.

Sessions should come back at the dimensions the user currently has, not at stale creation-time dimensions.

## Non-Goals

This does not require perfect historical reflow after resize. Stable, responsive interaction is more important.

## Examples

After increasing terminal width, old lines may remain wrapped at their old width, while new output uses the wider width.
