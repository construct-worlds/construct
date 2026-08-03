# 0185-fleet-events-never-block-the-client-loop

Status: accepted
Date: 2026-08-02
Area: tui
Scope: Applies to a client's handling of daemon-pushed fleet notifications that change which session a pane shows.

## Decision

Handling a daemon notification must not block a client's input/render loop on
another daemon round-trip. When a notification moves a pane onto a different
session, the client applies the local, already-known consequences immediately —
selection, view mode, scrollback, cleanup of the departed session's state — and
requests the newly shown session's history in the background, applying it when
it arrives.

A background load carries the selection generation it was dispatched for. A
result whose generation is stale, or whose session is no longer the one
selected, is discarded rather than painted.

## Reason

Dispatching a mutation off the loop is not enough on its own: the mutation's
completion arrives as a notification, and whatever the client does in response
runs on the loop. Reselection after a deletion is the common case — the
neighbor's transcript and PTY replay are exactly the two largest fetches the
client makes, plus a terminal-emulator parse of the replayed bytes, so awaiting
them inline stalls rendering and input for as long as that session's history is
large. The user reads that stall as the deletion itself being slow, which it is
not.

Discarding stale loads is required because the user keeps navigating while a
fetch is in flight; without a generation check, a slow load lands in whatever
pane is showing when it returns.

## Consequences

A pane may briefly show an empty or previous-state view before its history
arrives — an acceptable trade for a loop that never stops accepting input.
Anything a background load needs from client state must be snapshotted as plain
data at dispatch time, so the fetch and any expensive parsing can run off the
loop; results come back as data to install, not as work to perform.

Any future client-side reaction to a fleet notification that needs data the
client does not already hold must follow this shape rather than awaiting inline.

## Non-Goals

This does not require deferring work the client can do from state it already
has: local cleanup, selection repair, and layout updates belong in the
notification handler, where they keep the visible tree consistent in the same
frame.

Explicit user-initiated refreshes are not covered by this rule; a user who asks
for a reload has asked for its cost.
