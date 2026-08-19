# 0203-playbook-inline-code-editing

Status: accepted
Date: 2026-08-18
Area: ux
Scope: Completed single-backtick inline code in editable Playbook surfaces.

## Decision

A completed single-backtick Markdown span renders in the Playbook editor as
highlighted code text without visible delimiters. The stored document remains
the exact source Markdown, including both backticks.

The rendered span is an editing boundary. Backspace immediately after it
removes only the closing backtick, which dissolves the formatting and reveals
the opening backtick plus the text as ordinary editable source. Retyping the
closing backtick restores the formatted presentation.

Unmatched opening backticks and multi-backtick runs remain literal source.

## Reason

Inline code should be easy to scan without turning the Playbook into a
WYSIWYG document or losing its Markdown representation. Revealing the source
with one Backspace gives the user an obvious, reversible path back into text
editing while preserving normal source-level deletion semantics.

## Consequences

- Renderers and cursor geometry must account for hidden delimiters while the
  persisted and synchronized offsets continue to address source Markdown.
- Deleting the closing delimiter is a normal source edit and participates in
  undo and live Playbook synchronization.
- Fenced code and richer multi-backtick Markdown code spans are separate
  affordances and must not be inferred from this single-backtick rule.

## Non-Goals

- Full rich-text editing or hiding other Markdown punctuation.
- Defining fenced-code rendering or editing behavior.
