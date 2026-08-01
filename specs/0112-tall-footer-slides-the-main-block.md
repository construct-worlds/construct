# 0112-tall-footer-slides-the-main-block

Status: accepted
Date: 2026-07-26
Area: tui
Scope: A footer taller than one row slides the TUI's main block out of the viewport instead of resizing it.

## Decision

The TUI's **main block** — the session list, the split session views, the pin
strip, the lineage section, any playbook popups — is always laid out at the size
it has when the footer occupies a single row. That size does not depend on how
tall the footer actually is.

When the footer grows beyond one row (the operator panel, the multi-row harness
picker), the block keeps that size and **slides up** by the footer's extra rows.
Rows that leave the top edge are cropped away; the bottom of the block stays
flush with the modeline, so the newest terminal output remains on screen. The
modeline stays pinned directly above the footer and never slides behind it.

The slide is animated, matching the playbook view's slide-aside, and snaps
without easing while the user drags the panel's border so the block tracks the
pointer.

Consequences for the rest of the client:

- **The block renders in its own coordinates**, then the frame buffer is
  scrolled into place. Every hit target the block records during that pass is
  translated into viewport coordinates immediately afterwards; a target whose
  row left the viewport is dropped rather than clamped, so the top visible row
  never fires an affordance that scrolled off above it.
- **Everything that renders after the block** — modeline, footer, modals,
  tooltips anchored to them — is already in viewport coordinates and is not
  translated. Adding state to the frame's recorded geometry requires deciding
  which of the two it is.
- **Geometry whose height feeds layout math** (a playbook's viewport rows, a
  scrollbar's span) keeps its full height through the translation and carries
  the count of rows it lost instead. Anything mapping a viewport row back into
  that geometry's own rows — forwarding the mouse into a child PTY, placing the
  caret in a playbook, sizing a scrollbar drag — adds that count back.

## Reason

Opening the operator panel used to shrink the main block, which resized every
visible pane and asked every child PTY to reflow — for a panel the user opens
and closes constantly. Harnesses that repaint on resize flickered, alt-screen
harnesses re-laid-out their whole frame, and the daemon took a burst of resize
IPC each way. None of that work is wanted: the user is opening a panel, not
resizing their terminal.

Sliding costs nothing on the daemon side, because no pane ever changes size.
It also matches how the playbook view already reveals the terminal underneath it
— a surface moving aside rather than the layout reflowing around it — so the two
reveals read as the same gesture.

## Consequences

- Pane geometry, and therefore child PTY size, is stable across opening,
  resizing and closing any footer. Only a real terminal resize changes it.
- The top of the block is not reachable while a tall footer is open. That is the
  accepted trade: the bottom of a terminal view is where the live output is, so
  the top is the cheaper end to lose.
- Drag gestures on geometry that is only partly on screen (a scrollbar whose top
  scrolled off, a split divider) stay proportional to the full geometry, not the
  visible remainder.
- Any future surface that renders inside the block inherits the slide for free;
  any future surface that renders after it must not be translated twice.

## Non-Goals

- The footer itself does not slide or animate its height; it appears at its full
  height and the block moves to meet it.
- This says nothing about how tall any particular footer is, or about the
  operator panel's own resize/persistence behavior.

## Examples

- The operator panel opens: the session list and split views glide up, their top
  rows leaving the viewport, the last line of each terminal still sitting just
  above the modeline. No session reports a resize.
- The panel is dragged taller: the block follows the pointer immediately, still
  without resizing.
- The panel closes: the block glides back down; the strip it has not covered yet
  stays blank rather than showing stale rows.
- A session row that scrolled off the top is not clickable, and clicking the
  top visible row selects the session actually painted there.
