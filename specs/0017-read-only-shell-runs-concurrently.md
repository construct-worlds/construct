# 0017-read-only-shell-runs-concurrently

Status: accepted
Date: 2026-06-02
Area: harness
Scope: A `shell` call the model flags read-only is treated as Safe, so independent reads in one turn run concurrently and skip the approval gate.

## Decision

The `shell` tool exposes a `read_only` boolean argument. When the model sets it
`true` on a non-interactive call, that call's effective risk is `Safe` rather
than `Risky`. Safe is the property that lets the agent loop fan calls out
concurrently and run them without an approval prompt, so read-only shell calls
issued together in one turn execute in parallel — the same treatment the
dedicated read-only inspection tools (e.g. `agentd_context`, `agentd_get_diff`)
already receive.

The downgrade is deliberately model-trusting: the harness honors the flag, it
does not parse the command line to verify the command is actually read-only.
The flag's contract — set it only for provably side-effect-free commands (no
writes, redirects, command substitution, or chaining into a mutator) — is
stated in the tool's argument description, and the model is responsible for
respecting it. The downgrade never applies to `interactive: true` calls, which
spawn long-lived processes rather than bounded reads.

A `shell` call without `read_only: true` keeps its `Risky` classification and
its serial, gated execution, unchanged.

## Reason

`shell` is `Risky`, and the agent loop only fans out `Safe` calls; `Risky`
calls serialize through the approval gate one at a time. That made batched
read-only inspections (reading several files at once) impossible to run
concurrently even in `unsafe_auto`, despite costing nothing to parallelize.
Benchmarking against the Codex CLI on the same model and backend showed Codex
parallelizing a meaningful share of its turns while smith stayed effectively
one tool per turn. Trusting an explicit model-set flag is the simplest enabler
that unblocks concurrency without parsing-based heuristics that command
substitution and redirects can defeat.

## Consequences

- Read-only shell calls no longer prompt for approval, including in `manual`
  mode — consistent with how other read-only tools already behave, but a
  change for users who previously approved every shell command. This relies on
  the model labeling honestly; see Non-Goals.
- A mutating command the model mislabels `read_only: true` bypasses the gate.
  The classification must therefore stay narrowly scoped (only `shell`, only an
  explicit `true`, never `interactive`), and the arg description must keep the
  read-only-only contract prominent. Approval semantics for any call the model
  does not flag are unchanged.
- The Safe bucket is unbounded (`join_all` with no concurrency cap). This
  decision does not add one; a large batch of read-only shell calls spawns that
  many processes at once, matching existing Safe-tool behavior.

## Non-Goals

- Not a security boundary. The flag trusts the model, exactly as `auto_review`
  trusts a model reviewer (see [0015-approval-modes](0015-approval-modes.md));
  it does not make a model's read-only claim equivalent to a verified one.
- Does not add command-line parsing / an allowlist to independently confirm a
  command is read-only. That stricter detection (option 1 in the originating
  issue) can be layered on later as defense-in-depth without changing this
  contract.
- Does not address whether the model *emits* parallel calls in the first place;
  that is a separate prompt/tool-surface concern. This spec only governs how
  the harness executes them once emitted.
