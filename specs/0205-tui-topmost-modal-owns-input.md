# 0205-tui-topmost-modal-owns-input

Status: accepted
Date: 2026-08-19
Area: tui
Scope: Input precedence when transient TUI surfaces overlap pane-local surfaces such as the rolled-down Playbook.

## Decision

The topmost rendered modal owns input before every surface painted beneath it. The frame records both the modal's bounds and its identity; keyboard, paste, and pointer routing must consult the same modal precedence represented by the render order instead of inferring ownership from whether an underlying surface exists.

An explicit modal consumes inputs it does not use unless that modal's own established semantics say otherwise. In particular, the keyboard-only session picker consumes pointer events without acting on them, Help keeps its close-or-scroll behavior, and the remote-control dialog consumes every key while routing its registered pointer controls.

Modal precedence does not alter dismissal semantics. A Playbook remains a pane-local roll-down surface; clicking outside it may interact with exposed panes without closing it. A dialog that closes on an outside click closes itself rather than a covered Playbook. A modal that can auto-open without user action keeps the close-and-reroute rule for every input it does not claim, so the dismissing input is processed exactly once by the next eligible surface.

## Reason

Render order and input order can diverge when one shared rectangle stores only the last modal geometry while event handlers separately test whether a Playbook or other underlying surface exists. The visible dialog then appears focused but keystrokes and clicks mutate the covered editor. Recording the owner alongside the geometry makes hit-testing follow what the user can actually see.

## Consequences

- New modal renderers must register their identity when they register modal bounds.
- Event paths must not give a covered Playbook, pane, or child PTY first refusal merely because it remains mounted.
- Pointer-down events on a modal cannot start focus, rename, resize, text-selection, or editor gestures underneath it.
- Existing per-modal close, fallthrough, scrolling, and keyboard-only behavior remains authoritative.

## Non-Goals

- Defining one universal dismissal key or outside-click behavior for every modal.
- Turning non-modal overlays, menus, or tutorial cards into modals.
