# 0054-list-header-delete-acts-on-contained-sessions

Status: accepted
Date: 2026-06-28
Area: tui
Scope: Invoking delete while a session-list header row is selected removes every session that header stands for, not a single session.

## Decision

The session list contains rows that are not themselves sessions: a project
(group) header, and an "N archived" disclosure row that ends a section (the
ungrouped run, a project, or one parent's subagents). When the delete/kill action
is invoked while the selection is on such a header row, it acts on the whole set
the header represents:

- On a **project header**, delete offers to remove the project, with the option
  to delete its member sessions too (or orphan them).
- On an **"N archived" disclosure row**, delete removes every archived session in
  that exact section — and only the archived ones, never the active siblings
  rendered above the row.

A header delete always asks for confirmation before removing more than the
single highlighted thing, and the confirmation states how many sessions and which
section/project will be affected.

Each contained session is removed through the same per-session delete entry point
used by single-session delete, so the subagent cascade
([0052-session-removal-cascades-to-subagents](0052-session-removal-cascades-to-subagents.md))
applies to each one. A failure on one member is reported but does not abort the
rest of the sweep.

Selecting and deleting an ordinary session row is unchanged: it targets exactly
that one session.

## Reason

Header rows summarize a collection; the natural meaning of "delete this row" is
"delete what it collects," not "delete nothing" or "delete one arbitrary member."
The archived disclosure row is the only practical handle a user has for clearing
out a pile of retired sessions in one gesture — without it, emptying an archive
means expanding the section and deleting each session one at a time. Routing the
sweep through the existing per-session delete keeps a single removal contract
(transcript + worktree teardown, subagent cascade) instead of inventing a parallel
bulk path that could drift from it.

## Consequences

- The membership a header delete resolves must mirror exactly what that header
  renders in the list, so the count in the confirmation matches what disappears.
  Resolve the member set against live state at confirm time, not when the prompt
  opened, so a session that un-archived or moved sections in between is not deleted
  out from under the user.
- Archived-section delete is irreversible for every session it sweeps, the same as
  deleting each individually; it is intentionally a heavier action than the
  expand/collapse the same row performs on left/right.
- Because removal reuses the per-session path, any future change to what deleting a
  session entails is inherited by header-row delete for free.

## Non-Goals

- This does not change the archive-vs-delete distinction
  ([0047-archive-vs-delete-session-lifecycle](0047-archive-vs-delete-session-lifecycle.md));
  a header delete is delete, and does not offer archive of the whole set.
- This does not add a bulk-archive gesture on header rows; it defines delete only.
- This does not alter how an active project header's members behave by default
  (they may still be orphaned rather than deleted); it only fixes the meaning of
  delete on the archived disclosure row and preserves the existing project-delete
  choice.

## Examples

- A user finishes a day of work, expands nothing, moves the cursor to the
  "12 archived" row under a project, and presses the delete chord; after
  confirming, all twelve archived sessions in that project (and their archived
  subagents) are gone, while the project's active sessions remain.
- The cursor is on the ungrouped run's "3 archived" row; deleting removes those
  three top-level archived sessions and leaves every grouped session untouched.
- The cursor is on a normal session row; deleting affects only that session,
  exactly as before.
