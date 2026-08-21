# 0196-playbook-selection-menu-focus-model

Status: accepted
Date: 2026-08-21
Area: ux
Scope: how keyboard focus moves between the playbook editor and the selection action menu while a selection is active, in every client that offers the menu.

## Decision

Making a non-empty selection in the Playbook shows a passive selection action
menu. Bare Tab transfers keyboard focus into that visible menu without
changing the document. Once focused, Tab moves forward through the menu rows;
S-Tab moves backward, as do the arrow keys. Enter activates the focused row.
The passive menu advertises Tab as its entry key where it has a frame to
advertise on.

This is a client-independent contract. Any Playbook editor that shows the
selection menu owes the user this focus model, whichever text surface it is
built on. A client whose surface would consume Tab as an edit must suppress
that default explicitly: the whole point of the binding is that a selection is
live, so a Tab that reaches the text surface replaces the very text the menu
was offering to act on.

Esc dismisses an open selection menu but preserves the text selection. With
the menu absent, the editor owns the next key: Tab and S-Tab nest and un-nest
every list line spanned by the selection, and Esc clears the selection. This
two-stage cancel is deliberate: menu dismissal and selection cancellation are
separate actions.

`C-o` remains a compatibility alias for focusing the menu. If the menu was
dismissed while the selection remained active, `C-o` may reopen and focus it.
`C-g` remains the direct universal-cancel path that clears the selection and
menu together.

Menu focus is real focus, not a painted highlight: in a client with a focus
concept of its own, the focused row is what that platform reports as focused
and exposes to assistive technology. Borrowing focus must not cost the user
their region — a client whose editor drops selection state on blur restores it
when focus comes back, so the motions that extended the region still extend it
and Escape's second stage still has something to cancel.

## Reason

The menu needs a discoverable, single-key keyboard entry point, while selected
text still needs access to normal list indentation. Giving Esc a menu-only
dismissal step resolves that ownership conflict: Tab enters the visible menu,
and Tab edits the selection after the user explicitly dismisses the menu.

Keeping the selection after the first Esc also avoids making a transient
context menu and the durable editing range one indivisible state. Retaining
`C-o` avoids breaking users who learned the prior focus chord.

## Consequences

- Menu presence, menu focus, and text selection are distinct states. Code and
  tests must not collapse menu dismissal into selection cancellation.
- Bare Tab focuses a visible passive menu; it does not indent until Esc has
  dismissed that menu. Once the menu is absent, Tab and S-Tab retain their
  selected-list editing meanings.
- The advertised entry key and the actual binding must not drift apart.
- Focused-menu Tab/S-Tab navigation remains symmetric, including terminals
  that report S-Tab as Shift+Tab rather than BackTab.
- A client adding the selection menu inherits all three states and both Escape
  stages. Shipping the menu without the focus model is the regression this
  spec exists to prevent: Tab then falls through to the text surface and
  destroys the selection instead of acting on it.
- Because the menu is focusable, it is part of the client's focus order and
  accessibility tree, and dismissal must return focus somewhere deliberate —
  the editor, with the selection still live.

## Non-Goals

- What Enter should do while the menu is merely *shown* but unfocused; this
  spec fixes Tab and Escape ownership, and Enter's meaning only once a row
  actually holds focus.
- Pointer interaction, which is unchanged and remains the primary path in
  clients that have a pointer. Keyboard reachability is additive to it.
- Which rows the menu offers, and how each is spelled per client.
