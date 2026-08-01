# 0171-playbook-client-edits-publish-and-reconverge

Status: accepted
Date: 2026-08-01
Area: ux
Scope: The obligation an interactive Playbook client has to publish every local mutation and to recover when a publish is rejected.

## Decision

A Playbook is a document a human and an agent hold at the same time, so an interactive client owes it two things.

**Every local mutation publishes.** Typing, deleting, pasting, undoing, indenting, accepting a clip — if it changes the buffer, it goes to the daemon on the same gesture, with no separate save step. A mutation path that changes the buffer and returns without publishing is a defect, not a deferral: the user has no way to tell a local-only change from a shared one, and the change is invisible to the owning agent and to every other client.

**A rejected publish re-converges.** Client edits are anchored to text, so a publish can legitimately fail — an agent may rewrite the region between two keystrokes. The client must treat that as a recoverable conflict: rebase or merge onto the latest document, tell the user something happened, and resume publishing. It must not drop the failure silently.

The second obligation exists because of how the first one fails. A client that stops publishing does not merely lose one edit. Its next edit is anchored against a base the daemon never received, so that publish fails too, and so does every one after it — while the local buffer keeps accepting input and looks correct. A client that also suppresses adopting remote changes while it holds unsaved work (a reasonable thing to do) then stops receiving as well, and the document is silently severed in both directions with the buffer still responding normally.

## Reason

The failure mode this rules out was reachable from three ordinary gestures — undo, paste, and an agent editing the line you are typing on — and in each case the user's evidence was a correct-looking buffer. Work was lost with no error, no marker, and no way to notice before closing the view. Publishing on every mutation removes the first cause; re-converging on rejection bounds the damage of the rest to a single merge the user is told about.

Anchored edits are the right wire format — they let concurrent edits to different regions merge without a lock — but they are only safe if a missed anchor is handled. The merge that recovers from one is the same three-way merge an explicit save already performs, so this obligation costs no new machinery.

## Consequences

- Every buffer-mutating path in a Playbook client goes through publishing. New editing affordances inherit the obligation; a new path that skips it reintroduces the defect for that gesture only, which is how this stayed unnoticed.
- Publish failures are surfaced to the user, not swallowed. "Merged with agent edits" is a normal, expected message, not an error state.
- A client may still refuse to adopt remote changes while it holds unpublished local work, but only as a transient state it is actively resolving — never as a resting state a failed publish can strand it in.
- Cursor and selection positions may shift when a recovery merge lands, the same way they do for any adopted remote change.

## Non-Goals

This does not require an operational-transform or CRDT model, per-keystroke acknowledgement, or that concurrent edits to the same characters merge without conflict. Conflict markers from an unreconcilable merge remain acceptable as long as the user is told.

## Examples

- The user types into a block an agent rewrote a moment ago. The anchored edit no longer matches; the client merges onto the latest document, keeps both sides, and reports the merge. The next keystroke publishes normally.
- The user pastes a block of Markdown. It reaches the daemon on the paste, not on a later save, and the owning agent can act on it immediately.
- The user presses undo. The undone document is published like any other edit; a client where undo only mutates the local buffer is defective.
