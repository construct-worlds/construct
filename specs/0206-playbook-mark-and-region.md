# 0206-playbook-mark-and-region

Status: accepted
Date: 2026-08-21
Area: ux
Scope: How the Playbook editor arms and extends a keyboard-driven selection region, in every client that offers one.

## Decision

A Playbook editor that offers keyboard region selection must implement the whole
emacs mark contract, not a fragment of it:

- **Set** — `C-Space` places a zero-width mark at the caret. It never inserts
  text; a client whose text surface would consume the keystroke must suppress
  that default explicitly.
- **Extend** — while the mark is armed, every caret motion the editor supports
  extends the region from the mark instead of collapsing it. That means the
  arrows, `Home`/`End`, the emacs motions `C-f`/`C-b`/`C-n`/`C-p`/`C-a`/`C-e`,
  and whatever word and line/document-boundary motions the platform spells with
  its own modifiers.
- **Act** — the region is the selection the editor's selection-scoped actions
  operate on: copy, cut, run-selection, and the selection verb menu.
- **Cancel** — `C-g` and `Escape` both deactivate the region and leave the caret
  where it is. Cancelling never edits the document.

The region also ends when the user takes it somewhere else: an edit consumes it,
a pointer press hands selection back to the mouse, and mounting a different
session's Playbook starts with no region.

Clients are free to differ on *how* the region is stored — a client built on a
native text surface should let that surface's own selection anchor be the mark
rather than shadowing it — but not on which keys do what.

## Reason

Playbook is one document with more than one editor, and the mark is muscle
memory: a user who sets a mark and presses an arrow expects a region no matter
which client they happen to be in. Half an implementation is worse than none —
a `C-Space` that types a space silently corrupts the document the user is
composing, and a mark that only some motions extend teaches the user to distrust
the binding and fall back to the mouse.

Cancel needs two spellings because the two clients arrived at it from different
directions: `C-g` is the emacs quit that the binding vocabulary implies, and
`Escape` is Construct's browser-safe universal cancel.

## Consequences

- Adding a caret motion to a Playbook editor is not complete until the motion
  also extends an armed region. A motion that only moves the caret silently
  drops the region and looks like a bug in `C-Space`.
- `C-Space` must be claimed at a point where it still can be — before the text
  surface's own default action, and before any client-global keymap that might
  otherwise route it. Clients must verify the keystroke actually arrives rather
  than assuming it does; a host OS may bind `C-Space` (macOS input-source
  switching) above the application entirely, which no client can override.
- Region state is per-editor and transient. It is never saved, never published
  as document content, and never survives a remount.
- This is additive to native selection, not a replacement: pointer selection,
  shift-selection, and the platform clipboard keep working unchanged.

## Non-Goals

- A full emacs mark ring, `exchange-point-and-mark`, or transient-mark
  bookkeeping beyond "the region is active or it is not".
- Keystroke-for-keystroke equivalence between clients on everything else. Spec
  0059 still governs: web Playbook parity is defined by capability, and native
  per-platform text affordances remain the baseline. This spec constrains only
  the mark bindings a client chooses to offer.

## Examples

- Caret mid-line, `C-Space`, then six right-arrows: six characters are selected,
  the document is unchanged, and the selection-scoped menu appears.
- With that region up, `C-e` extends to end of line and `C-n` extends a line
  further down; `C-g` then clears the highlight and leaves the caret where the
  last motion put it.
- With the region up, copy places the region on the clipboard and disarms the
  mark; the next arrow key moves the caret normally.
