# 0197-tui-copy-selected-fleet-identity

Status: accepted
Date: 2026-08-10
Area: tui
Scope: Copy the exact stable identity of a selected session, project, or service from the TUI.

## Decision

The TUI exposes one copy-ID action for every selectable fleet object. It copies the raw session id for a session, the raw project id for a project, and the stable service name for a service. The action is available as `/copy-id`; pane title menus expose it as the final action where those menus already exist and show the exact identity in the label before it is copied.

Archived disclosure rows do not invent an identity. Invoking the action without a session, project, or service selected leaves the clipboard unchanged and reports that an identifiable fleet item must be selected.

Identity copies use the same clipboard transport and delivery-status semantics as other TUI text copies, including the SSH bridge and OSC 52 behavior.

## Reason

Fleet identities are frequently needed in CLI commands, logs, issue reports, and cross-session coordination. Display titles and shortened ids are useful for scanning but are unsafe to copy as identifiers, while manually selecting a full id is slow and error-prone.

## Consequences

- The copied value is exact and unadorned so it can be pasted directly into commands and APIs.
- Services continue to use their protocol-level stable names as identities; no parallel opaque service id is introduced.
- Copy feedback identifies the fleet object type and distinguishes confirmed clipboard writes from terminal clipboard requests.
- Copy identity has no dedicated keybinding, avoiding conflicts with native editor and terminal keys.
- Future selectable fleet object types must either define their stable copy identity or remain explicitly unsupported by this action.

## Non-Goals

- This does not copy display titles, summaries, or compound labels.
- This does not make archived disclosure controls addressable as fleet objects.
