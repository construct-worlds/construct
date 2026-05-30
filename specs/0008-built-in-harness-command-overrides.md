# 0008-built-in-harness-command-overrides

Status: accepted
Date: 2026-05-30
Area: harness
Scope: Applies to built-in harness adapters and user configuration of child commands.

## Decision

Built-in harness adapters must support both binary-only overrides and full command-prefix overrides for the child process they spawn. Full command overrides take precedence over binary-only overrides and are still executed directly, without invoking a shell.

## Reason

Some user environments require more than replacing a binary path. They may need wrappers, launchers, version managers, or command prefixes. At the same time, shell execution would introduce quoting surprises and avoidable security risk.

Supporting direct full command overrides gives users flexibility while preserving predictable process spawning.

## Consequences

Adapters should treat command override parsing as configuration, not as shell script evaluation. Whitespace and quoting can structure arguments, but shell expansion and shell operators should not be part of the contract.

Existing binary-only override behavior should remain as a simpler fallback.

Future built-in harnesses should follow the same override shape so configuration remains consistent.

## Non-Goals

This does not make arbitrary shell snippets a supported adapter launch mechanism.

## Examples

A user can configure a harness to launch through a wrapper command, while agentd still spawns the resulting executable and arguments directly.
