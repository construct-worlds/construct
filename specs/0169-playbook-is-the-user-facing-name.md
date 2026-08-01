# 0169-playbook-is-the-user-facing-name

Status: accepted
Date: 2026-08-01
Area: convention
Scope: The per-session executable Markdown document is named "Playbook" everywhere, and every rename of it must carry existing documents forward.

## Decision

The per-session co-authored, executable Markdown document is called a
**Playbook** in every surface: UI labels, documentation, configuration keys,
environment variables, MCP and native tool names, protocol types, and
persisted file names.

Any future rename of this concept must ship a one-time, idempotent migration
that moves every prior generation's on-disk artifacts to the current name, for
both the per-session documents and the shared data directory that holds custom
templates. Migrations are cumulative: a session that has never been opened
since an earlier generation must still land on the current name, no matter how
many renames it skipped. When a session carries artifacts from more than one
generation, the newest generation wins.

## Reason

The document had been called "Canvas" and then "Program". Both names were
opaque about what the thing does. "Program" additionally collides with the most
overloaded word in computing — a reader cannot tell whether it means an
executable, a MIDI Program Change, or this feature — which forced the
documentation to gloss the term every time it appeared.

"Playbook" carries "a document that gets executed" on its face, which is
exactly the property that distinguishes it from an inert `PLAN.md`.

The migration requirement exists because this document is user-authored
content. A rename that leaves it behind is silent data loss: the session opens
with an empty Playbook and the user's prose is still on disk under a name
nothing reads.

## Consequences

- Renaming this concept is never a pure find-and-replace. The persistence
  migration is part of the change, and it must be covered by tests that write
  legacy artifacts and assert the content survives.
- Migration entries accumulate rather than get replaced. Removing an older
  generation's entry strands any install that skipped that version.
- The name appears in externally-visible contracts (MCP tool names, config
  keys, environment variables). Renaming breaks those contracts, so a rename is
  a deliberate compatibility decision, not a cosmetic one.
- Vocabulary that merely contains the word "program" — MIDI Program Change,
  `TERM_PROGRAM`, sandboxed executables, "programmatic" — is unrelated and must
  survive any sweep untouched.

## Non-Goals

This does not govern what a Playbook *contains* or how Run dispatches it; those
decisions live in their own specs. It governs only the name and the obligation
to carry documents across renames.
