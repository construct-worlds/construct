# 0043-playbook-incremental-search

Status: accepted
Date: 2026-06-27
Area: ux
Scope: Incremental search behavior in markdown playbook editing.

## Decision

Playbook markdown editing uses Emacs-style incremental search with `C-s` to enter search mode, incremental query input, and explicit navigation with `C-s` (next) / `C-r` (previous). Exiting search is split by intent:

- `Enter` accepts the current match and closes search mode.
- `C-g` or `Esc` cancels search mode and restores the cursor to the pre-search
  anchor. Both keys have identical cancel semantics.

While search is active, every typed character extends the query and updates match ranges. Pasted text is also consumed by the search prompt as query text rather than inserted into the playbook document. Search highlights are visible in the playbook body and the active match is visually distinguished from non-active matches.

## Reason

Playbook editing is now a primary markdown editing surface; it needs the same discoverable in-place incremental search behavior users expect from editors for fast local navigation. Explicit mode transitions prevent accidental full-playbook text replacement and make search a reversible, low-risk command.

## Consequences

- Search state is tracked on the playbook popup and does not interfere with smart-clip suggestions or selection gestures.
- Search mode can be re-entered and edited from the current cursor position without closing the playbook.
- When a clean playbook adopts newer Markdown from the daemon, any active search query remains open and recomputes matches against the adopted document.
- Paste routing checks active playbook search before ordinary playbook editing so a pasted search term cannot mutate the document under the prompt.
- The modeline should prefer search status text while search mode is active so users can tell whether a query is empty, failing, or positioned.
- Search highlights must preserve existing playbook visuals (selection, smart-clip spans, and running-shimmer overlay) and remain compatible with wrapped rows.
- Cancelling search restores the original cursor anchor; accepting search keeps the current cursor position.

## Non-Goals

This spec does not define full regex search, case-sensitivity toggles, search/replace, or cross-session playbook search.

## Examples

- `C-s` `a` `l` `p` `h` `a` `C-s` cycles from the first match to the next; `C-r` cycles backward.
- `C-s` then `C-g` (or `Esc`) returns to the query start position and closes the I-search bar state.
- `C-s` with an empty query opens I-search with no active match, then typing begins collecting matches immediately.
