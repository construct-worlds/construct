# 0209-reorder-never-dead-ends-at-a-visible-row

Status: accepted
Date: 2026-08-22
Area: ux
Scope: Applies to session-list reorder when the only regions in the move direction are collapsed projects.

## Decision

A reorder command must not refuse while rows the user can plainly see remain in the direction of travel. Skipping collapsed projects (see 0007) is an optimization over hidden *contents*, not a reason to treat a visible project row as the end of the list. When skipping would leave nowhere to land because every region beyond is collapsed, the session enters the nearest one and that project expands, so the session stays visible where the user dropped it.

Reorder refuses only at a true boundary: the top of the first region, or the last position of the last region, where nothing at all lies beyond.

## Reason

Collapse state is asymmetric by construction. The ungrouped region always renders first and can never be collapsed, so moving up always has somewhere to land, while moving down needs an *expanded* region below. The steady state of a working fleet is one expanded project with the rest collapsed — which made the last session of the first project unable to move down at all, while nine project rows sat visibly beneath it.

That asymmetry is invisible to the user. One direction of the same key worked and the other silently did nothing, which reads as a broken binding rather than a bounded list.

Expanding the entered project is what keeps the visible model honest: a reorder that dropped the session into a collapsed project would appear to delete it from the list.

## Consequences

Reorder may change a session's project membership, and may change a project's collapse state. Both were already possible — entering an adjacent expanded project has always re-parented the session — so the rule is that a move which lands somewhere must leave that landing spot visible.

A no-op result from reorder now carries real information: it means the true end of the list, and clients may say so plainly.

Future region kinds must state whether they can be collapsed and where they render, since a region that is both collapsible and terminal would reintroduce the dead end.

## Non-Goals

This does not weaken 0007. Whenever any visible region exists beyond a collapsed one, the collapsed project is still jumped in a single step and its members and collapse state are left untouched.

This says nothing about auto-collapsing a project the session leaves.

## Examples

Projects below the selection are all collapsed: moving the bottom session down enters the first of them, which expands; the rest keep the collapse state the user chose.

A collapsed project sits between the session and an expanded one: moving down still jumps the collapsed project entirely and lands in the expanded one, leaving the skipped project collapsed.

The last session of the last project moves down: nothing lies beyond, so the command reports that there is nothing to reorder past.
