# 0049-playbook-empty-state-onboarding

Status: accepted
Date: 2026-08-12
Area: ux
Scope: What an empty playbook shows in place of a bare placeholder string.

## Decision

When a playbook has no content, its body renders an onboarding placeholder instead of a single line of grey instructional text. The placeholder has three parts, top to bottom:

1. A one-line description of what the playbook is.
2. Every non-blank template as a compact vertical row, ordered by name (case-insensitive). Each row shows the template name and its description. Rows can be activated with the mouse or with the keyboard: Up/Down or C-p/C-n focuses a visible row and Enter applies it, with both the focus highlight and key hint painted in the empty state. When more templates exist than fit the available height, the placeholder shows as many as fit and a trailing "+N more" indicator for the remainder. Activating a row fills the playbook with that template's Markdown as a starting point the user then edits.
3. A divider and a smart-clip syntax reference line (session/harness embeds, select-and-Run, `:::clip` fences).

The placeholder appears exactly when the playbook body is empty, and disappears as soon as any content exists — including the moment a template is applied or the first character is typed. The "blank" template is never offered as a row, since it is the empty state itself.

Applying a template is an ordinary buffer edit: it records an undo state, stamps the document's template id, and publishes through the normal live-edit path, exactly like typed input. It does not bypass the editor.

## Reason

The playbook is a primary surface, but a bare "type here" prompt does not tell a new user what the playbook is for or give them a fast way to start. Surfacing templates as one-click buttons turns the empty state into discoverable onboarding while preserving the plain editing model: the buttons are shortcuts for content the user could have typed, not a separate creation flow.

## Consequences

- Only the active playbook publishes template hitboxes, so a click never targets an inactive split. Keyboard navigation uses the same visible-row set, so it cannot focus a template hidden behind the overflow indicator.
- The placeholder must keep every line within the playbook width so nothing wraps; wrapping would desync template hit rows from what is painted. Names and descriptions are truncated to the available width, and each rendered row publishes a hitbox covering both its name and description. When the playbook is too narrow or short for a template row, or no templates exist, it degrades to the description-and-syntax prose with no template interaction.
- The hit geometry is computed in absolute screen cells, which is safe only because an empty playbook never scrolls (offset is always zero). Any future scrolling of the empty state must recompute hits against the scroll offset.
- Template descriptions come from the shared daemon template list, keeping the TUI choice semantics aligned with the web UI.
- The set of templates offered tracks the daemon's template list (built-in plus user templates). It is fetched at client start, refreshed on reconnect, and re-fetched in the background each time the playbook pane opens so edits to (or new) template files appear on the next open without a daemon restart. The background refresh is non-blocking: the pane opens against the cached list and swaps to the fresh one when it lands.

## Non-Goals

This spec does not define a full template gallery or template management/editing UI. The template source directory and live-reload are covered in [0051](0051-playbook-custom-templates-source.md).

## Examples

- Opening a fresh playbook shows the description, the non-blank templates and their descriptions as vertical rows, a keyboard hint, a divider, and a syntax line.
- Pressing Down focuses the first visible template; pressing Enter replaces the empty body with that template's Markdown, places the cursor at the end, and makes the placeholder vanish. Clicking a row does the same. `C-/` undoes back to the empty state.
- Deleting all playbook content brings the placeholder back.
- On a very narrow playbook, the same playbook shows only the description and syntax line, with no template rows.
- With more templates than fit the pane height, the last visible row is followed by "+3 more".
