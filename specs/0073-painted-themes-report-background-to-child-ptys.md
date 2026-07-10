# 0073-painted-themes-report-background-to-child-ptys

Status: accepted
Date: 2026-07-10
Area: architecture
Scope: How child PTY terminal-background probes (OSC 11) are answered when a client theme paints the frame background.

## Decision

When a connected client's theme paints the full frame background itself, child PTY sessions that ask for the terminal background color must receive that painted color. Themes that intentionally leave the terminal background visible, such as Matrix and Basic, must not cause a synthesized response.

The daemon is the single authority for answering these probes:

- Clients report their painted background (or "none") to the daemon as per-connection state, re-sent on connect and on theme change. Reports are removed when the connection closes.
- The effective background is the most recent report among live connections; when it is "none", probes pass through unanswered and the child's own fallback applies, as before this spec.
- The daemon scans live child PTY output for background probes, answers each probe exactly once by writing the reply into the child's input, and strips the probe from the byte stream that clients, transcripts, and replay logs see — so no attached terminal emulator (a real terminal, or xterm.js in the web client) can answer a second time.

Clients must never answer child probes themselves by injecting bytes into a session's input.

## Reason

Interactive CLIs query the terminal background color to choose readable foreground colors. For painted Construct themes, the visually relevant background is the one Construct paints, not the user's underlying terminal.

The first implementation answered from the TUI by watching the broadcast byte stream and injecting replies into child input. That design corrupts sessions: every attached client answers independently (duplicate replies), replies arrive when the child is no longer waiting for them (stray input a cooked-mode shell echoes into its output stream), and the same probe bytes can be re-observed and re-answered. A probe must be answered by exactly one authority that sits in front of the child's PTY — the daemon.

## Consequences

- Future theme changes must preserve the painted/background-aware distinction: painted themes report their color, background-aware themes report "none".
- Web terminal emulators always paint their configured terminal background,
  including themes whose native-TUI counterpart is background-aware. Web
  clients therefore report the xterm background color on connect and theme
  change so live probes remain under the daemon's single-response authority.
- Responses are generated from reported theme data, so live theme changes affect subsequent probes without adapter changes.
- Because probes are stripped from the downstream stream whenever a painted background is in effect, client-side terminal emulators only see (and may answer) probes when no painted background is reported — which preserves pre-spec behavior for background-aware themes.
- With several clients connected on different themes, the most recent reporter wins; children see one coherent answer, not one answer per client.
- Historical replay must not generate input. Terminal emulators can answer
  OSC/CSI queries while parsing output, so clients suppress their input path
  while feeding stored bytes into an emulator. Otherwise every reload or
  attached tab can inject a stale response into the live child.

## Non-Goals

- Replayed transcript bytes and historical PTY snapshots never trigger responses; only live child output does.

## Examples

- A child runs `printf '\x1b]11;?\x07'` while the user's TUI uses the dark painted theme: the child receives one `\x1b]11;rgb:…\x07` reply with the theme's background; the probe bytes never appear in the transcript or any client's stream.
- The same child under the native TUI's Matrix theme, with no painted client
  connected: no reply is synthesized, so the probe passes through as before.
- A WebUI xterm using Matrix reports its configured dark-green background;
  the daemon answers and strips the live probe before xterm sees it.
- Reloading a WebUI session whose PTY history contains an old probe paints
  that history without sending xterm's generated response to the live child.
