# 0196-playbook-selection-menu-focus-model

Status: accepted
Date: 2026-08-18
Area: ux
Scope: how keyboard focus moves between the playbook editor and the selection action menu while a selection is active.

## Decision

Making a non-empty selection in the Playbook shows a passive selection action
menu. Bare Tab transfers keyboard focus into that visible menu without
changing the document. Once focused, Tab moves forward through the menu rows;
S-Tab moves backward. The passive menu advertises Tab as its entry key.

Esc dismisses an open selection menu but preserves the text selection. With
the menu absent, the editor owns the next key: Tab and S-Tab nest and un-nest
every list line spanned by the selection, and Esc clears the selection. This
two-stage cancel is deliberate: menu dismissal and selection cancellation are
separate actions.

`C-o` remains a compatibility alias for focusing the menu. If the menu was
dismissed while the selection remained active, `C-o` may reopen and focus it.
`C-g` remains the direct universal-cancel path that clears the selection and
menu together.

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

## Non-Goals

- What Enter should do while the menu is merely shown; this spec fixes Tab and
  Escape ownership, not Enter's meaning.
- Web UI menu focus, which has its own pointer-first interaction model.
