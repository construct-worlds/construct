# 0189-webui-overlay-focus-restore

Status: accepted
Date: 2026-08-03
Area: webui
Scope: Dismissible modal sheets return keyboard focus to their invoker (or the active surface) so keystrokes never land on `<body>`.

## Decision

Every dismissible overlay in the web UI (rename, settings, close-session,
new-session, and future sheets of the same shape) records the element that
held keyboard focus when it opened and restores that focus when the sheet
closes — whether closed by Escape, Cancel, backdrop click, or a successful
primary action.

If the invoker is gone or no longer focusable (row re-rendered, menu item
inside a now-hidden menu, deleted session), the caret falls back to the
active typing surface for the current view (terminal, composer, or playbook
editor). It never ends on `<body>`.

On touch layouts, the fallback prefers the composer when it is visible over
silently focusing the terminal helper textarea, so the user gets a visible
focus ring / soft keyboard rather than a caret that appears to have vanished.

A successful "create session" path is an exception to invoker restore: focus
moves to the newly selected session's active surface instead of bouncing back
to the new-session button.

Deliberate view-mode toggles (chat / terminal / playbook) also leave a typing
destination — chat lands in the composer; playbook lands in the editor;
terminal follows the existing touch-keyboard policy and falls back to the
mobile composer when xterm focus is skipped.

## Reason

Opening a sheet correctly moves focus into the dialog, but closing used to
only flip `hidden`. The focused control disappears with the sheet, and the
browser parks the caret on `<body>`. The next keystroke then goes nowhere —
not the terminal, not the composer, not the session list — which reads as the
app being broken.

The invoker is the right restore target when the user opened the sheet from a
button they still have; the active surface is the right fallback when the
invoker no longer exists. Touch devices need the composer preference because
focusing xterm's helper textarea without a visible caret is worse than a
visible soft-keyboard entry point.

## Consequences

- New dismissible sheets must capture the invoker on open and call the shared
  restore helper on every close path (including Escape via the overlay stack).
- Approval sheets remain out of scope: they are answered decisions, not
  dismissible overlays, and must not be Escape-dismissed.
- Restoring focus must not scroll the page (`preventScroll` where available).
- Ambient repaints still follow
  [0184-a-repaint-never-takes-the-caret](0184-a-repaint-never-takes-the-caret.md);
  this decision covers deliberate overlay lifecycle, not fleet-driven rebuilds.

## Non-Goals

- Building a general focus trap / roving tabindex system for every dialog.
- Changing which chords or keys dismiss overlays.
- Forcing the soft keyboard open on every session list selection (that remains
  governed by the touch session-switch policy).

## Examples

- Open Rename from the title-bar button, press Escape → focus returns to the
  rename button. Type → the keystroke reaches the chord machine / active
  surface as before the sheet, not `<body>`.
- Open Settings from the usage badge, click the backdrop → focus returns to
  the badge.
- Open Close-session from a list row that is then deleted by another client →
  focus lands on the terminal or composer for the current session.
- Create a session from the new-session sheet → focus lands on the new
  session's surface so the first typed character is input, not lost.
