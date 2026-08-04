# 0193-webui-tui-design-language

Status: accepted
Date: 2026-08-04
Area: webui
Scope: The web UI's default look follows the TUI's design language — monospace, flat, hairline-bordered, square — rather than generic chat-app styling.

## Decision

The web UI's baseline visual system (all themes, not just Matrix) is the TUI's
design language translated to pixels:

- **One typeface.** All chrome and content use the monospace stack. There is no
  separate sans-serif UI font.
- **Flat surfaces, hairline borders, square corners.** Depth comes from
  background steps (`bg` → `bg-elev` → `bg-elev-2`) and border weight, never
  from drop shadows, blur, gradients, or rounded corners. Floating surfaces
  (menus, popovers, dialogs, tooltips) are distinguished by a brighter
  `--border-strong` frame — the web peer of the TUI's `border_focused` box.
- **Color is rationed.** Status glyphs carry state color (`○ ● ⏸ ✓ ✗`, same
  vocabulary as the TUI); the accent marks selection and primary actions;
  everything else stays on the fg / dim / faint text ladder.
- **Selection is a bar, not an outline.** The selected session row is a
  full-width tint with a 2px accent edge inset on the left — same idiom for the
  keyboard-active suggestion card. User chat messages reuse the accent-edge
  treatment.
- **Labels are lowercase and functional.** Section titles ("operator",
  "lineage", "playbook"), mode toggles ("tokens ⇄"), dialog titles, and button
  labels follow the TUI's lowercase convention. Matrix theming stays reserved
  for the opt-in Matrix theme.

Semantic exceptions are deliberate: state rings (error/connection), inset
selection markers, the run-shimmer and loading-shimmer animations, and native
scrollbar pill thumbs survive; the Matrix theme keeps its CRT glow on top of
the shared flat layout.

Two tokens extend the palette in both the CSS `:root` block and the JS
`WEB_THEMES` table (which must stay in sync): `--border-strong` (floating-layer
frames) and `--info` (attention/unblock hue, mirroring the TUI's `info` slot).

## Reason

The default themes previously imitated mainstream AI chat apps (sans-serif,
rounded bubbles, soft shadows, pill buttons), which read as generic and clashed
with the product's terminal identity. The TUI already has a coherent, tested
design language; the web UI is the same fleet seen through a browser, so it
should look like the same product.

## Consequences

- New web components must be styled from these rules: mono type, hairline
  1px borders, square corners, `--border-strong` for anything floating, status
  color only on status, lowercase labels.
- No `box-shadow` depth, `backdrop-filter`, or decorative gradients may be
  reintroduced outside the Matrix theme's scoped overrides.
- Mobile keeps 16px inputs (iOS zoom suppression); desktop overrides them down
  to the mono UI size via media query.
- The Matrix theme remains an opt-in skin layered over the shared layout; the
  baseline must stay legible without it.

## Non-Goals

Not a pixel clone of the character grid: the web UI keeps native scrolling,
touch targets, and responsive layout. It borrows the TUI's philosophy, not its
cell geometry.
