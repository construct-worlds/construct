# 0175-service-view-focus-is-edit-mode

Status: accepted
Date: 2026-08-01
Area: ux
Scope: Focusing a service view puts it in edit mode, and unsaved changes are marked in the pane title.

## Decision

A service view has exactly one interactive state: editing. Every path that
focuses a service — creating one, opening one by name, clicking its pane, or
moving pane focus onto it — leaves the editor open on that service. There is no
read-only service view the user must step out of before typing, and no
"press a key to start editing" step. Moving focus to something that is not a
service closes the editor.

Sidebar selection and pane focus remain distinct. Keyboard navigation that
moves the session-list selection onto a service updates the active pane and
prepares its editor, but keeps focus in the list so Up/Down, C-p/C-n, and
equivalent profile bindings continue traversing rows. An explicit drill-in
action moves focus to the already-prepared service editor.

A command that hands the user a focused service view must also dismiss the
transient input surfaces that would otherwise keep swallowing keystrokes — in
particular the minibuffer/minibuffer panel the command was typed into.

Because there is no view-only state to close back to, Escape means:

- with unsaved edits, discard them and show the saved definition again;
- with nothing unsaved, hand keyboard focus back to the session list, leaving
  the editor open in its pane.

A service being created has never been saved, so it is unsaved from the moment
it appears; Escape discards the draft outright.

The pane title marks unsaved state with a trailing `*` on the service name.
"Unsaved" compares the definition on screen against the one the daemon last
confirmed. Channel attachment and detachment are applied straight to the daemon
rather than staged in the editor, so they never contribute to unsaved state.

The view is one continuous navigable list: the definition fields, then the
channel catalog rows, then the routed session rows, wrapping back to the first
field. Channels are a section, not a definition field. Next/previous
navigation walks all three sections in that order regardless of which keys are
bound to it. Row actions belong to the row that is selected: channel rows
attach, publish, create, edit, delete, and rotate; a routed session row opens
that session, the same as clicking it.

## Reason

The service pane is a form. Splitting it into a "viewing" state and an
"editing" state made the most common action — change a field — cost an extra
keystroke that had no visible prompt, and made keyboard focus ambiguous: a
command could leave a service selected while the keystrokes still went to the
panel it was typed in. Collapsing the two states removes both problems.

Once focus alone implies editing, the user needs two things the old model
supplied implicitly: a way to tell whether what they see has been persisted,
and a way to back out. The title marker answers the first at a glance from any
pane; Escape's two-step meaning answers the second without reintroducing a
state to close into.

Exposing channels and routed sessions as navigable rows rather than as a
summary field removes a level of indirection: the rows are already rendered, so
counting them in a field row duplicated information and forced row selection to
be modelled as a sub-mode of a field.

## Consequences

- Any new way of focusing a service must open its editor. Keyboard traversal
  in the list may prepare that editor without transferring focus out of the
  list.
- Selection state must distinguish definition fields from channel rows and
  session rows explicitly. Indexing rows off a field number reintroduces the
  sub-mode this decision removes.
- Adding, removing, or reordering definition fields must keep the field list,
  the per-field help, and the field count in step.
- Anything that adopts a daemon-confirmed definition into the editor must also
  refresh the saved baseline, or the title will keep claiming unsaved work.
- Global chord prefixes must keep working over an open editor. The editor no
  longer closes to make room for them, so it has to stand aside for chord
  continuation keys.
- Unsaved edits are now discardable by a single keystroke. That is deliberate —
  Escape is the only discard, and it always announces what it did.

## Non-Goals

- This says nothing about how service definitions are stored, validated, or
  applied by the daemon.
- It does not make channel changes transactional with the definition. They stay
  immediate, daemon-side operations.

## Examples

- Creating a service from the fleet panel leaves the new view focused with the
  cursor in its first field; typing edits it immediately, and the title shows
  the name with `*`.
- Editing an existing service's instruction shows `*`; Escape restores the
  saved instruction and clears `*`; Escape again moves the cursor to the
  session list, with the service view still open beside it.
- From the last definition field, pressing Down selects the first channel in
  the catalog; Down again past the last channel selects the first routed
  session; Down past the last session returns to the first field.
