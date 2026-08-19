# 0204-playbook-code-editing

Status: accepted
Date: 2026-08-18
Area: ux
Scope: Source-preserving single- and triple-backtick code editing in Playbook surfaces.

## Decision

A completed single-backtick span or exact triple-backtick span on one source
line renders in the Playbook editor as highlighted code text without visible
delimiters. The stored document remains the exact source Markdown, including
both delimiter runs.

Each rendered span is an editing boundary. Backspace immediately after it
removes only the complete closing delimiter: one backtick for inline code or
three backticks for triple-backtick code. That dissolves the formatting and
reveals the opening delimiter plus body as ordinary editable source. Retyping
the closing delimiter restores the formatted presentation.

Unmatched delimiters remain literal source. Multiline triple-backtick fences
also remain literal and line-preserving in both editors; content within them is
inert Markdown source, so smart clips, attachments, action links, headings, and
list-aware editing do not activate there. This conservative presentation keeps
the editor's source-line and collaboration-offset invariants intact rather than
collapsing delimiter lines differently across clients.

## Reason

Code should be easy to scan without turning the Playbook into a WYSIWYG
document or losing its Markdown representation. Revealing source with one
Backspace provides an obvious, reversible path into editing while preserving
normal source-level synchronization. Restricting hidden triple delimiters to a
single source line avoids ambiguous cursor rows and remote-cursor placement for
multiline blocks.

## Consequences

- Renderers and cursor geometry account for hidden delimiters while persisted
  and synchronized offsets continue to address source Markdown.
- Deleting a closing delimiter is a normal source edit and participates in
  undo and live Playbook synchronization.
- CJK and other wide glyphs use display-cell geometry while source offsets
  remain Unicode character offsets.
- A completed one-line triple-backtick span is atomic in the web editor and
  source-addressable in the TUI, matching the existing single-backtick model.
- Multiline fenced regions preserve every source line and suppress interactive
  Markdown extensions in both clients.

## Non-Goals

- Full rich-text editing or hiding other Markdown punctuation.
- Collapsing multiline opening and closing fence lines into a WYSIWYG block.
