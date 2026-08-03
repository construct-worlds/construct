# 0187-tui-redraws-follow-visible-change

Status: accepted
Date: 2026-08-03
Area: tui
Scope: The terminal client redraws on visible changes and animations without continuously repainting a static frame.

## Decision

The TUI's periodic timer may run maintenance without forcing a terminal redraw. A full frame is painted when an input, notification, expiry, or other state change affects visible output, or when a renderer reports that a visible wall-clock animation needs another frame.

Animations retain their normal frame cadence while visible. Quiet wall-clock content may use a low-frequency heartbeat, but a static frame must not be rebuilt at animation cadence. Skipping an idle paint must not skip unrelated event-loop work such as reconnects, resize debounce, hydration, or deferred searches.

## Reason

Building and diffing the complete terminal frame traverses session trees, lineage, histories, and ambient panels. Doing that on every animation tick while nothing visible changes consumes a meaningful fraction of a CPU core and multiplies across idle clients.

## Consequences

New time-based render effects must request follow-up frames for as long as they visibly animate. One-shot expiries must dirty the frame when they lapse. Low-frequency labels and history windows may refresh on the quiet heartbeat rather than at animation speed.

Maintenance scheduling and paint scheduling remain separate: optimizing idle rendering must never delay input handling or background state progress.

## Non-Goals

This decision does not lower the frame rate of visible animation or suppress event-driven updates.
