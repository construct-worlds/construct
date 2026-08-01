# 0172-playbook-stores-newline-normalized-markdown

Status: accepted
Date: 2026-08-01
Area: persistence
Scope: The line-ending form a stored Playbook document is guaranteed to be in.

## Decision

A stored Playbook contains no carriage returns. CRLF and lone CR line endings are normalized to LF before the document is persisted, and that normalization belongs to the store, not to the goodwill of each writer. Clients also normalize on input, so a caret lands where the user expects immediately rather than after a round trip, but the store's guarantee is what the rest of the system relies on.

## Reason

A Playbook is Markdown that several very different consumers read back: the block splitter that gives each block its stable identity, the renderers in each client, and the agent that receives the document on a Run. Those consumers agree on LF and only LF. A lone CR is not a line break to any of them — the block splitter sees one block, and the TUI renderer paints nothing — so a document with CR line endings is one opaque line to the whole system while looking merely "wrong" to the user.

Carriage returns arrive by ordinary means, not exotic ones. Terminals routinely send CR for the line breaks inside a bracketed paste, because CR is what a tty expects for Enter. A user pasting a Markdown block into the Playbook from their terminal is the common case, not an edge case.

Normalizing per client would leave the guarantee only as strong as the newest client. Doing it in the store means a document that reaches disk is well-formed no matter which surface, tool, or agent wrote it.

## Consequences

- Every path that persists a Playbook normalizes: wholesale replacement, anchored edits, and any future write path. A new write path that skips it can reintroduce single-block documents.
- Anchored edits are matched against normalized stored text, so a caller whose anchor contains a CR will not match. Callers normalize their own input rather than relying on the anchor being taken verbatim.
- Writers cannot use line endings to carry meaning in a Playbook. Nothing does, and Markdown gives them none.
- Round-tripping a CRLF file through a Playbook returns it with LF endings.

## Non-Goals

This says nothing about other whitespace: trailing spaces, tabs, and blank-line runs are content and are preserved exactly. It does not apply to session PTY input, where CR is the correct byte to send.
