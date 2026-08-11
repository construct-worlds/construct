# 0196-playbook-selection-menu-focus-model

Status: accepted
Date: 2026-08-10
Area: ux
Scope: how keyboard focus moves between the playbook editor and the selection action menu while a selection is active.

## Decision

Making a selection in the playbook shows the selection action menu, but the
menu starts passive: every key — cursor movement, typing, and in particular
the Tab / S-Tab list nesting pair — keeps reaching the editor and operating
on the selection. The menu only starts consuming keys once the user focuses
it with a dedicated chord (`C-o` in the TUI), and the passive menu itself
advertises that chord on its frame. Esc returns focus to the editor without
dismissing the selection; the existing cancel keys still dismiss both.

The focus chord must never be a key that has an editing meaning while a
selection exists. Tab is the canonical counterexample: selections are exactly
when multi-line list nesting is wanted, so a menu that claims Tab makes the
editor's advertised behavior unreachable, and claiming only Tab but not S-Tab
splits a symmetric pair.

## Reason

The first implementation focused the menu with bare Tab. That shadowed the
editor's "Tab nests every list line the selection spans" behavior while
leaving S-Tab live (issue #1106), and the unfocused menu advertised nothing,
so the focus model was undiscoverable and Enter fell through to the editor
with destructive results (issue #1092). A passive popup may not steal keys
the surface underneath it documents.

## Consequences

- Adding keys to the *unfocused* menu path requires proving the editor gives
  that key no meaning while a selection is active.
- The advertised chord and the actual binding must not drift apart; the hint
  lives on the menu frame so a rebinding forces the two to be updated
  together.
- Accepted tradeoff: reaching the menu costs a chord rather than a single
  Tab press.

## Non-Goals

- What Enter should do while the menu is merely shown (issue #1092's
  remaining half) — this spec fixes who owns keys, not Enter's meaning.
- Web UI menu focus, which has its own pointer-first interaction model.
