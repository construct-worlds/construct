# 0110-construct-new-harness-argv

Status: accepted
Date: 2026-07-26
Area: cli
Scope: The CLI grammar for creating sessions with harness-specific launch arguments.

## Decision

`construct new` separates construct-owned session options from harness-owned CLI
arguments by position:

```text
construct new [construct-options] <harness> [harness-args...]
```

Construct parses options only before `<harness>`. Every token after `<harness>` is
forwarded verbatim to the selected harness adapter as launch argv. The initial
prompt is provided with `--prompt`/`-p`; it is not a positional argument after the
harness.

## Reason

Harness CLIs evolve independently and often expose flags that construct does not
know about. A positional boundary lets users reach those flags without waiting
for construct to add wrapper options, while avoiding delimiter-heavy invocations
and flag-name collisions. Moving the prompt to an explicit option removes the
ambiguity between “prompt text” and “harness argv”.

## Consequences

Construct-owned options must be placed before the harness name. A flag such as
`--model` before `<harness>` is construct metadata; the same token after
`<harness>` is raw harness argv.

Adapters may append construct-required control arguments after user-provided
harness argv when the underlying CLI requires those arguments for identity,
resume, tools, or prompt delivery. Users should not assume post-harness argv is
the final suffix of the effective command; they should assume only that construct
will forward it without parsing.

## Non-Goals

This decision does not require legacy positional prompts after the harness to be
supported. It also does not standardize every harness CLI flag; construct only
provides a forwarding channel.

## Examples

```sh
construct new --prompt "review this repo" claude
construct new --title "server logs" shell -lc 'tail -f server.log'
construct new --mode headless --prompt "fix tests" codex --approval-mode never
```
