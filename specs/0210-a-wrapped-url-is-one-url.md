# 0210-a-wrapped-url-is-one-url

Status: accepted
Date: 2026-08-25
Area: tui
Scope: A URL the pane's width split across rows is a single clickable URL, whatever indent the renderer put in front of its continuation.

## Decision

Clickable URLs are recovered from the *rendered frame*, not from source text, so that a link stays clickable even inside a session that has taken over the mouse. Reading the frame means reading it as the user sees it: a URL broken across rows is one URL.

Rows are joined on one signal — whether the row above used its last cell:

- A row whose final cell holds content was cut off by the width. The row beneath it continues it, and any indent in front of that continuation is layout the renderer added, not part of the text. The indent is skipped and the two rows join.
- A row that ends in blank cells ended on its own terms. That trailing blank is what stops a URL from running on into the line below, and no join happens.
- A blank row, or a row the frame has no text for, always breaks the run.

A joined URL is one hit: clicking any row of it opens the whole thing, and hovering underlines every row it spans — but never the skipped indent, which is not part of the URL.

## Reason

Wrapping a URL is the common case, not the corner case: long links are what agents emit, and panes are narrow. Recovering only the fragment above the break is worse than not linking at all — the user gets a confident click that opens a truncated, usually 404-ing URL, and the tail is dead to the pointer, so nothing suggests the link was mis-read.

Two renderers wrap differently and both must work. A terminal hard-wraps at the edge and starts the next row at column zero. An application that wraps its own text — markdown bodies, bullet continuations, box-drawn panels — indents the continuation instead. Keying the join on *content in the last cell* rather than on where the continuation starts covers both, because both are ultimately caused by the same thing: content that ran out of width.

The rule is deliberately conservative in the one direction that matters. Over-joining opens a wrong URL, which is a worse failure than under-joining, so a line that had room left over is never treated as continued.

## Consequences

- The signal is the frame's own geometry, so nothing needs to know which harness or renderer produced the text.
- A URL that happens to end exactly in a row's last cell will absorb the start of the row beneath it. This is accepted: it is rare, its blast radius is one wrong link, and narrowing it further would mean guessing at content rather than reading layout.
- Whatever changes how panes pad or indent their rows changes what joins. Anything that pads short rows out to full width would make every row look cut off, and must not.
- Hit-testing and hover highlighting have to agree on the skipped indent, since a cell that cannot be clicked must not be underlined as though it could.

## Non-Goals

Does not extend to reflowing or un-wrapping text generally — the joining exists only to recover a URL, and only for the run of characters that forms one.

## Examples

A continuation the renderer indented — one URL, both rows clickable, the indent not underlined:

```
  https://example.com/a
    bbb/ccc?q=1
```

A line that simply ended — the URL stops, and the prose below is not part of it:

```
see https://example.com/x
    and then some prose
```
