# 0035-tui-clipboard-targets-user-terminal

Status: accepted
Date: 2026-06-23
Area: tui
Scope: TUI text selection copies to the clipboard of the terminal the user is operating, even when the client runs remotely.

## Decision

When TUI text selection is copied, the clipboard target is the user's controlling terminal, not necessarily the host where the client process runs. Local pasteboard commands may be used when the client is local, but remote sessions must prefer an OSC 52 terminal clipboard request. When the terminal path includes a terminal multiplexer, the client must emit the multiplexer-aware form needed to reach the outer terminal while still allowing multiplexers with native OSC 52 handling to process the request.

In an SSH session, the client cannot reliably see terminal multiplexers that run on the user's local machine outside the SSH connection. Remote OSC 52 output must therefore include a bounded set of equivalent requests: the direct OSC 52 request plus passthrough-wrapped requests for common outer tmux/screen layers. Remote emission should favor reaching the user's terminal over trying to infer a single exact terminal path from incomplete environment variables.

The UI must not claim that OSC 52 definitely changed the clipboard, because the process can only know that it wrote the request. It may report a sent copy request; it may report copied only when a local pasteboard command succeeds.

## Reason

Over SSH, host-local clipboard tools target the remote machine, which is not where the user is sitting. OSC 52 is the terminal protocol mechanism that can travel back through the terminal stream to the user's laptop clipboard. Terminal multiplexers such as tmux and screen can intercept that sequence, so a plain OSC 52 write is not enough for common remote setups. A particularly common case is SSH running inside a local tmux pane on the laptop: the Linux process sees SSH variables, but not the local `TMUX` variable, so environment-based multiplexer detection alone is insufficient.

## Consequences

- Remote clipboard behavior depends on the user's terminal and multiplexer allowing clipboard writes; the client cannot confirm acceptance.
- The copy status distinguishes a confirmed local pasteboard write from a terminal clipboard request.
- Clipboard escape generation is part of terminal fidelity and should be tested as bytes, independent of the developer's current terminal emulator.
- Remote OSC 52 output may contain multiple equivalent clipboard requests for the same payload. This is acceptable because successful requests write the same value, while unsupported passthrough wrappers should be ignored by terminals that do not understand them.

## Non-Goals

- This does not add a separate clipboard transport over SSH.
- This does not bypass terminal or multiplexer security settings that intentionally disable clipboard writes.
