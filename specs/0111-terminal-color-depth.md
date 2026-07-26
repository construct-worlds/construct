# 0111-terminal-color-depth

Status: accepted
Date: 2026-07-25
Area: tui
Scope: The TUI renders in 24-bit color but must down-convert to whatever the attached terminal can actually render, at one choke point, rather than trusting every terminal with truecolor.

## Decision

The palette stays authored in 24-bit RGB. Color depth is resolved **once at
startup** from the environment, and every color is quantized to that depth at
the **last stop before the terminal** — the rendering backend — not at the
palette.

Three depths are supported: 24-bit, 256-color (the xterm 6×6×6 cube plus the
grayscale ramp), and the 16 basic SGR colors.

- **Truecolor is the default.** A terminal is downgraded only when the
  environment positively identifies one known to mishandle 24-bit sequences.
  Silence is not evidence of incapability: `COLORTERM` is routinely lost over
  SSH, and guessing 256 from its absence would downgrade the majority of
  capable terminals.
- **A positive known-bad identification outranks a `COLORTERM=truecolor`
  claim.** `COLORTERM` is a hint any shell profile can export, and often does
  by cargo-cult; a terminal that identifies *itself* and has no 24-bit support
  is a fact.
- **A single environment variable forces the depth**, overriding all detection,
  for the cases where the guess is wrong in either direction.
- **Quantization never targets the low 16 palette entries.** Their hues belong
  to the user's terminal profile, so a palette slot mapped onto one would drift
  with the profile instead of holding the color the theme asked for. (At the
  16-color depth, where there is nothing else to target, this necessarily
  yields.)
- **Colors the user pinned to a specific index stay at that index** when the
  depth can express it.
- **A downgrade is visible in the modeline**, alongside the theme name.

## Reason

Terminals that lack 24-bit color do not gracefully approximate it. Apple
Terminal — which ships on every Mac — drops the `38;2` introducer and re-reads
the channel values as independent SGR parameters, so channels that coincide
with SGR codes hijack the cell: a slot whose blue channel is 104 paints a
bright-blue background, one carrying 92 and 103 paints green-on-yellow, and the
`2` of the introducer switches on the faint attribute everywhere. The result is
an unreadable frame, and the user has no way to tell that the cause is their
terminal rather than the app.

Quantizing at the backend rather than in the palette is what makes the rule
total. A frame carries three sources of color: the theme, colors computed
during rendering (fades, blends, gauges), and colors that child harnesses wrote
into their own panes and that we re-render as our own cells. Only the last stop
before the terminal sees all three.

## Consequences

- New palette slots and ad-hoc render colors need no per-site awareness of
  color depth; they are quantized by construction.
- Any future render path that writes color to the terminal *outside* the
  backend (raw escape sequences, a second backend, a direct writer) breaks this
  guarantee and must quantize itself or be routed through the backend.
- Themes must stay legible after quantization: slots that differ only in a few
  RGB steps may collapse onto the same indexed color, so palettes should not
  rely on near-identical hues to carry meaning.
- Detection is heuristic and will occasionally be wrong. That is acceptable
  only because the forced override exists and the modeline shows what was
  chosen.
- The terminal-background probe (light/dark detection) and the background color
  we report to child sessions stay in true RGB: they describe intent, and
  quantization is a rendering concern.

## Non-Goals

- Dithering, perceptual color-space matching, or contrast repair after
  quantization. Nearest-neighbor in RGB is enough to keep a frame readable.
- Runtime re-detection or a user-facing depth switcher. The environment is
  read once; changing it means restarting the client.
- Making 16-color terminals look like the 24-bit palette. At four bits the
  theme's identity is largely gone, and that is accepted.

## Examples

- A terminal that advertises truecolor, or advertises nothing: the palette goes
  out exactly as authored.
- Apple Terminal, even with `COLORTERM=truecolor` exported by the user's shell:
  256-color output, and the modeline names the downgrade.
- A user-pinned `indexed:34` slot on a 256-color terminal: still index 34.
- A light theme's near-white frame background on a 256-color terminal: the
  nearest light neutral, so the frame still reads as a light background instead
  of falling back to the terminal's own.
