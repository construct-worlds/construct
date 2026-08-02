# 0184-a-repaint-never-takes-the-caret

Status: accepted
Date: 2026-08-02
Area: webui
Scope: A client repainting itself because of fleet activity must never move the user's typing caret, and must not repaint at all when nothing it draws has changed.

## Decision

Repainting is a consequence of *other people's* activity. Typing is the user's.
The two must not collide.

Two rules, and a client that renders shared state must honor both:

1. **A repaint restores the caret it displaces.** If a repaint relocates the
   DOM subtree that holds the focused element, it puts focus back before
   yielding — synchronously, in the same task, so no keystroke can land in the
   gap. This covers whichever surface the user was typing into: composer,
   playbook editor, or terminal. A terminal needs its own terminal API to be
   refocused; focusing the underlying textarea alone does not restore keyboard
   and IME state.

2. **A repaint driven by ambient state is conditional on that state mattering.**
   Before rebuilding, a client compares what it is about to draw against what it
   drew last time and skips the rebuild when they agree. Deliberate user actions
   — splitting, closing, moving focus, resizing — repaint unconditionally.

Rule 1 without rule 2 is not enough. Restoring focus is not free: it interrupts
an in-progress IME composition, which means a repaint arriving mid-syllable
still eats input for anyone typing a composed script. Rule 2 is what keeps the
repaint from happening in the first place.

## Reason

Fleet activity is continuous and unrelated to what the user is doing. A single
busy session pushes state several times a second — status transitions, token
counts, spinner frames — and every one of them reaches every connected client.

A layout that mounts one live surface and re-parents it into whichever pane
holds focus makes that traffic dangerous: moving a node out of the document
blurs whatever inside it had focus, and putting it back does not return focus.
So a background worker's status tick silently kills the caret of a user typing
in a completely different pane. The symptom is not read as a bug in the client —
it is read as the app being broken, because the user is mid-sentence and their
keystrokes stop arriving with nothing on screen to explain why.

The cost of the rules is a comparison per repaint and one focus snapshot. That
is much cheaper than the repaint being skipped, and far cheaper than the
re-parent, refit and reflow the skip avoids.

## Consequences

- Any state a pane draws must be part of what the conditional repaint compares.
  Adding a session-derived detail to a pane without adding it there produces a
  pane that silently stops updating — the failure mode this rule trades for.
- Content that arrives asynchronously (a transcript that finishes loading, a
  terminal library that finishes downloading) repaints by asking for one
  directly, not by waiting for ambient traffic to trigger it.
- Focus stays per-client and is never written to or read from the daemon; this
  rule is about not *losing* focus, and does not weaken
  [0118-split-layout-is-shared-daemon-state](0118-split-layout-is-shared-daemon-state.md).
- Restoration must not outlive the repaint. Re-asserting the old caret on a
  later frame would fight deliberate focus moves — clicking a pane to select
  another session must still end with focus in that pane.

## Non-Goals

- Eliminating re-parenting. One live surface moved between panes is what lets
  the composer, editor and widget machinery stay singletons; this rule makes
  that design safe rather than replacing it.
- Preserving selection, scroll position, or hover across a repaint. Only the
  typing caret is protected.
- Applying the conditional repaint to user-initiated layout changes, which must
  always be immediate.

## Examples

- A user is typing a prompt in the left pane while an agent in the right pane
  streams output. The caret stays in the left pane; no keystroke is dropped.
- A background session the user cannot even see finishes its turn. The session
  list updates; the panes are not rebuilt, because no pane's title or surface
  changed.
- That same session is showing in a pane and its title changes as it finishes.
  The pane is rebuilt, and a user typing in another pane keeps the caret.
- A user clicks an unfocused pane while typing in the focused one. Focus ends in
  the clicked pane, not back where it started.
