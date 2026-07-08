# 0076-program-selection-run-comment

Status: accepted
Date: 2026-07-08
Area: tui
Scope: Keyboard behavior and prompt delivery for running a selected Program region with an extra one-line instruction.

## Decision

When Program text is selected, the TUI selection context menu offers a plain Run action and a Run-with-comment action. Pressing Tab while a non-empty Program selection is active moves keyboard focus from the editor into this context menu. Up and Down switch between the plain Run row and the comment Run row. Typing while the menu is focused edits the comment row as a single logical-line instruction. The instruction may wrap visually in the menu, but newline insertion is not part of the affordance. Enter runs the selection; if the comment row is selected, the typed instruction is passed with the Program Run prompt.

The focused comment editor supports the same basic single-line movement and deletion keys users expect elsewhere in the TUI: C-a, C-e, C-b, C-f, C-d, and C-k. Its Run button remains visually distinct from typed text and is aligned to the right edge so it does not read as part of the instruction.

The extra instruction is run metadata, not Program content. It must not alter the selected markdown, selection block identity, or optimistic shimmer scope. The daemon appends the instruction to the generated Program Run prompt and disables mechanical fast paths that cannot interpret it.

## Reason

Selection Run is often used to steer a narrow region without editing the Program itself. A transient comment lets the user give one-off direction while preserving the Program document as durable plan/state rather than turning every small instruction into persistent markdown.

## Consequences

Future TUI changes must preserve Tab as the keyboard entry point to the selected-text Run menu while a non-empty selection is active. The comment is optional and trimmed before dispatch. Two otherwise identical Runs with different comments are distinct user intents and must not be deduplicated together.

Non-TUI clients may omit the comment field; compatibility requires the daemon to treat absence as the existing plain Program Run behavior.

## Non-Goals

This does not define a persistent Program comment model, multi-line comments, or web UI parity for the comment affordance.
