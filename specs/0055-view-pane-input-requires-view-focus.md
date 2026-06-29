# 0055-view-pane-input-requires-view-focus

Status: accepted
Date: 2026-06-28
Area: tui
Scope: Keystroke-capturing surfaces drawn in the view pane only consume input while the view pane holds focus, so the session list stays navigable.

## Decision

A surface rendered in the right-hand view pane (the program editor, a session
PTY, a transcript, future view-pane editors) may capture keystrokes only when
focus is on the view pane. When focus is on the session list, navigation keys
(Up/Down, next/prev-session, and the list's own bindings) must reach the list,
regardless of what the view pane is currently showing.

Switching focus to the list (`C-x o` / other-window) while a view-pane surface
stays visible must immediately make the list respond to navigation keys.
Opening or drilling into a view-pane surface that captures input should move
focus to the view pane, so the natural open-then-type flow keeps working.

While the view pane is unfocused and shows such a surface, a stray text key must
not silently auto-focus the view and leak into the surface (or the PTY it
shadows); the list-focused state is for navigation only.

## Reason

The view pane and the session list are two focus targets toggled by other-window.
A view-pane surface that grabs every keystroke whenever it merely exists — not
when it is focused — defeats other-window: the list looks focused but its arrow
keys still drive the surface underneath. This was observed with the program
view: after `C-x o` to the list, Up/Down and next/prev still moved the program
cursor instead of the list selection.

## Consequences

- New view-pane surfaces that read the keyboard must gate their top-level input
  capture on view-pane focus, mirroring how PTY capture is gated. They must not
  key the capture solely off "is this surface open".
- The focus indicator and the input routing must agree: if the status line says
  focus is on the list, list keys win.
- Surfaces that shadow a focusable child (e.g. a program drawn over a PTY) must
  also suppress auto-focus-on-typing while the list is focused, so a stray key
  cannot route to the shadowed child.

## Non-Goals

- Does not change which chords are global. Quit and other global chords continue
  to work from either pane via the keymap.
- Does not dictate the per-surface key bindings used once the view pane is
  focused.

## Examples

- Program visible in the view pane, focus on the list: Up/Down change the
  selected session; `C-x o` back to the view, and Up/Down edit the program
  again.
- Opening a program focuses the view pane so typing immediately edits it.
- A live PTY session focused in the view pane keeps receiving typed keys; moving
  focus to the list stops forwarding them and resumes list navigation.
