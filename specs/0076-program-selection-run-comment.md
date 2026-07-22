# 0076-program-selection-run-comment

Status: accepted
Date: 2026-07-08
Area: tui
Scope: Keyboard behavior and prompt delivery for running a selected Program region with an optional one-line instruction.

## Decision

When Program text is selected, the TUI selection context menu offers one Run button and an optional instruction field. Pressing Tab while a non-empty Program selection is active moves keyboard focus from the editor into this context menu. Typing while the menu is focused edits the instruction as a single logical line. The instruction may wrap visually in the menu, but newline insertion is not part of the affordance. Enter, or clicking the Run button, runs the selection; when the instruction is non-empty, it is passed with the Program Run prompt.

Selection Run executes in a visible interactive same-harness fork by default, even when the optional instruction is populated. The fork receives the selected Run context but writes progress and results directly to the Program-owning session's document with an explicit target session id; it does not return work for the owner to merge or queue a follow-up turn onto the owner. Holding Shift reverses only the execution destination: Shift+Enter and Shift+click Run deliver the same Run to the Program-owning session. While Shift is held, the menu label and focused-row description preview that it will run on the main session. Full-document/title-bar Run and non-selection API callers retain their established owner-session behavior unless they explicitly request a fork.

When the Run is dispatched to a fork, the daemon annotates the selection with the fork's session clip at dispatch time — the same convention selection verbs and instant dispatch follow — so the Program document shows where the work went and renders the fork's live state in place. The annotated selection keeps its pending shimmer with a plain "Running" status. The annotation is best-effort: if the selection anchor no longer applies verbatim (concurrent edit, ambiguity), the Run proceeds without a clip rather than failing. Owner-targeted Runs (Shift, or callers not requesting a fork) add no session clip: the work happens in the session the user is already looking at, and a self-referential clip would be noise.

A Run fork auto-closes when its task is complete. The fork's dispatch prompt instructs it to archive itself — a soft close that keeps the transcript and the Program's session clip valid — as its final action, only after every block it was dispatched for has settled on the owner Program, and to stay open when work remains pending, it is blocked, or the user has joined the conversation in the fork. This contract is prompt-enforced (the fork performs the archive with its session tools); a daemon-side deterministic close may later back it up, and must preserve the same completion condition and stay-open exceptions. Owner-targeted Runs never auto-close anything: the owning session is the user's own workspace.

Closing a Run fork — self-archive, manual archive, or delete — deterministically settles the shimmer of the blocks the fork was dispatched for, daemon-side, regardless of whether the fork made its own settle edit. The settle must survive text drift: a fork typically rewrites its blocks while working, so identifying them by dispatch-time content alone is insufficient — the fork's session clip traveling with a block also marks it. A closed fork must never leave the owner Program shimmering forever.

The focused instruction editor supports the same basic single-line movement and deletion keys users expect elsewhere in the TUI: C-a, C-e, C-b, C-f, C-d, and C-k. Because the field can wrap visually, C-p, C-n, Up, and Down move between wrapped visual rows. Its Run button remains visually distinct from typed text, is aligned to the right edge, and is highlighted only while the context menu is focused or hovered. The context menu content has one-column horizontal padding inside the border.

The extra instruction is run metadata, not Program content. It must not alter the selected markdown, selection block identity, or optimistic shimmer scope. The daemon appends the instruction to the generated Program Run prompt and disables mechanical fast paths that cannot interpret it.

## Reason

Selection Run is often used to steer a narrow region without editing the Program itself. A transient comment lets the user give one-off direction while preserving the Program document as durable plan/state rather than turning every small instruction into persistent markdown.

## Consequences

Future TUI changes must preserve Tab as the keyboard entry point to the selected-text Run menu while a non-empty selection is active. The comment is optional and trimmed before dispatch. Two otherwise identical Runs with different comments are distinct user intents and must not be deduplicated together.

Non-TUI clients may omit the comment field; compatibility requires the daemon to treat absence as the existing plain Program Run behavior.

## Non-Goals

This does not define a persistent Program comment model, multi-line comments, or web UI parity for the comment affordance.
