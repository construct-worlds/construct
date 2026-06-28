# 0049-program-empty-state-onboarding

Status: accepted
Date: 2026-06-27
Area: ux
Scope: What an empty program shows in place of a bare placeholder string.

## Decision

When a program has no content, its body renders an onboarding placeholder instead of a single line of grey instructional text. The placeholder has three parts, top to bottom:

1. A one-line description of what the program is.
2. Every non-blank template as a clickable button, drawn as bordered boxes. Buttons wrap across as many rows as fit the pane width, ordered by name (case-insensitive). When more templates exist than fit the available height, the placeholder shows as many as fit and a trailing "+N more" indicator for the remainder. Clicking a button fills the program with that template's Markdown as a starting point the user then edits.
3. A divider, a smart-clip syntax reference line (session/harness embeds, select-and-Run, `:::clip` fences), and — when available — a reference/docs link footer.

The placeholder appears exactly when the program body is empty, and disappears as soon as any content exists — including the moment a template button is clicked or the first character is typed. The "blank" template is never offered as a button, since it is the empty state itself.

Clicking a template button is an ordinary buffer edit: it records an undo state, stamps the document's template id, and persists on the normal save path (program close / Run), exactly like typed input. It does not commit immediately or bypass the editor.

## Reason

The program is a primary surface, but a bare "type here" prompt does not tell a new user what the program is for or give them a fast way to start. Surfacing templates as one-click buttons turns the empty state into discoverable onboarding while preserving the plain editing model: the buttons are shortcuts for content the user could have typed, not a separate creation flow.

## Consequences

- Only the active program publishes the button hitboxes, so a click never targets an inactive split.
- The placeholder must keep every line within the program width so nothing wraps; wrapping would desync the button hit rows from what is painted. Buttons are packed into rows that never exceed the pane width, and every rendered button (across every row) publishes a hitbox. When the program is too narrow for even one button or too short for a single button row, or no templates exist, it degrades to the description-and-syntax prose with no buttons.
- The button hit geometry is computed in absolute screen cells, which is safe only because an empty program never scrolls (offset is always zero). Each button row occupies three contiguous lines, so a button on grid row `r` spans absolute rows `inner.y + 2 + r*3 ..= +2`. Any future scrolling of the empty state must recompute hits against the scroll offset.
- Template names shown on buttons are truncated to a bounded width so long names cannot blow out the layout.
- The set of templates offered tracks the daemon's template list (built-in plus user templates). It is fetched at client start, refreshed on reconnect, and re-fetched in the background each time the program pane opens so edits to (or new) template files appear on the next open without a daemon restart. The background refresh is non-blocking: the pane opens against the cached list and swaps to the fresh one when it lands.

## Non-Goals

This spec does not define a full template gallery, template management/editing UI, or hover/focus styling for the buttons. The template source directory, live-reload, and reference metadata are covered in [0051](0051-program-custom-templates-source.md).

## Examples

- Opening a fresh program shows the description, the non-blank templates as bordered buttons (ordered by name, wrapping across rows), a divider, a syntax line, and a docs link.
- Clicking the Tasks button replaces the empty body with the Tasks template's Markdown, places the cursor at the end, and the placeholder vanishes; `C-/` undoes back to the empty state.
- Deleting all program content brings the placeholder back.
- On a very narrow program, the same program shows only the description and syntax line, with no buttons.
- With more templates than fit the pane height, the last visible row is followed by "+3 more".
