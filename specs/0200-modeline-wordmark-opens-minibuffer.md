# 0200-modeline-wordmark-opens-minibuffer

Status: accepted
Date: 2026-08-16
Area: tui
Scope: The `construct` wordmark at the head of the modeline status bar is a pointer affordance for the minibuffer.

## Decision

The wordmark that opens the modeline status bar is clickable, and hovering it
shows a tooltip naming what it does.

- **Clicking toggles the minibuffer panel.** Clicking while it is closed opens
  it; clicking while it is open closes it.
- **The tooltip names the target and the keybinding.** It identifies the
  minibuffer and, when the panel is closed, states the keyboard route to the
  same place rather than presenting itself as the only way in.
- **The hit region covers exactly the wordmark.** It does not extend across the
  padding around it or the status fields that follow, and it is registered
  whether or not a session is selected.
- **The wordmark is visually distinguished while hovered**, matching the
  treatment already used by the modeline's other clickable regions, so it does
  not read as inert text.

## Reason

The minibuffer is the fleet's command surface, but before this its only
discoverable entry points were a keybinding a user has to already know and a
title in the matrix-rain panel that is not always visible. The wordmark is the
one element of the status bar that is present in every layout, at every window
size, in every view mode — which makes it the stable place to anchor a
permanent affordance for the fleet's command surface.

Toggling rather than opening is what keeps the affordance safe to click twice.
Opening the minibuffer rebuilds its prompt from scratch, so binding the click
straight to the open path would let a second click silently discard input the
user had already typed into an open panel.

Naming the keybinding in the tooltip keeps the pointer affordance a teaching
surface for the keyboard one, rather than a competing path that leaves
pointer-first users never discovering the shortcut.

## Consequences

- The modeline's left group is no longer a single inert run of text: the
  wordmark is peeled off it so it can carry its own hover styling. Future
  changes to that group must keep the wordmark's painted position and its
  registered hit region derived from one shared definition, or the clickable
  region drifts off the word it labels.
- The modeline row now carries clickable regions on both its left and right
  ends. Click dispatch over that row is first-match-wins, so any new region
  added to either side must not overlap an existing one.
- The minibuffer gains a second pointer entry point alongside the matrix-rain
  panel title. Both must continue to toggle, so the two never disagree about
  what a click on "the minibuffer control" does.

## Non-Goals

This does not make the wordmark a general application menu, and it does not
replace or change the keyboard route to the minibuffer. It does not give the
other inert status fields on the modeline click behavior.
