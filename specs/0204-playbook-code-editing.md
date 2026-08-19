# 0204-playbook-code-editing

Status: accepted
Date: 2026-08-18
Area: ux
Scope: Source-preserving inline and multiline backtick-code editing in Playbook surfaces.

## Decision

A completed single-backtick span, exact triple-backtick span on one source
line, or multiline backtick fence renders in the Playbook editor as
highlighted code text without visible delimiter glyphs. The stored document
remains the exact source Markdown, including every delimiter run and newline.

Multiline fences preserve a one-source-line/one-editor-line model. Opening and
closing delimiter lines keep their rows even when hiding the backtick run;
indentation, an opening info string, and trailing spaces remain visible on
those rows. Fence body lines retain their source text and line wrapping while
receiving code highlighting. Cursor, selection, presence, and collaboration
offsets continue to address Unicode character positions in the unmodified
Markdown source. Source positions within a hidden delimiter run map to the
single visual boundary where that run was hidden.

Each rendered span or block is an editing boundary. Backspace immediately
after its closing delimiter removes the complete closing run: one backtick for
inline code, three backticks for an exact one-line triple span, or the full
matching run on a multiline closing row. That dissolves the formatting and
reveals the opening delimiter plus body as ordinary editable source. Retyping
the closing delimiter restores the formatted presentation without rewriting
any other source.

Unmatched delimiters and incomplete multiline fences remain literal source.
Content inside both complete and incomplete fences is inert Markdown source,
so smart clips, attachments, action links, headings, inline-code spans, and
list-aware editing do not activate there.

## Reason

Code should be easy to scan without turning the Playbook into a WYSIWYG
document or losing its Markdown representation. Users expect opening and
closing triple backticks on separate lines to create a formatted code block.
Keeping both delimiter rows resolves that expectation without collapsing the
row or source-offset model. Revealing source with one Backspace provides an
obvious, reversible path into editing while preserving normal source-level
synchronization.

## Consequences

- Renderers and cursor geometry account for hidden delimiters while persisted
  and synchronized offsets continue to address source Markdown.
- Deleting a closing delimiter is a normal source edit and participates in
  undo and live Playbook synchronization.
- CJK and other wide glyphs use display-cell geometry while source offsets
  remain Unicode character offsets.
- A completed one-line triple-backtick span is atomic in the web editor and
  source-addressable in the TUI, matching the existing single-backtick model.
- Multiline fenced regions preserve every source line, hide delimiter glyphs
  only when complete, and suppress interactive Markdown extensions in both
  clients.
- A delimiter-only opening or closing line appears as a highlighted blank row.
  Multiple source offsets within hidden delimiter glyphs necessarily share one
  visual caret position; their collaboration offsets remain distinct.

## Non-Goals

- Full rich-text editing or hiding other Markdown punctuation.
- Collapsing multiline opening and closing fence rows.
- Language-specific syntax highlighting or interpreting the info string.
