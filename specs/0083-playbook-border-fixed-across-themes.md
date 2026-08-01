# 0083-playbook-border-fixed-across-themes

Status: accepted
Date: 2026-07-11
Area: ux
Scope: The Playbook pane's frame/border color, across all client UI themes.

## Decision

The Playbook pane's frame — its border, title-bar glyphs and icons painted on
that border, and the session-title mode glyph that stands in for it — always
uses the same accent (the Matrix theme's cyan, in its dark- or light-background
variant) regardless of which named UI theme (`matrix`, `basic`, `dark`,
`light`) is active. This is an intentional, narrow exception to
[[0134-client-ui-themes]]'s general rule that UI colors route through the
active theme palette.

The frame still adapts to whether the terminal/painted background is light or
dark (picking the matching cyan variant), and it is still overridable via the
`[colors]` theme.toml escape hatch, like every other theme slot.

## Reason

The Playbook frame is a distinct, recognizable surface (spec
[[0041-session-playbook-is-orchestration-state]]) that a user needs to spot at a
glance regardless of which color theme they've picked. Letting it drift with
the active theme's second accent (purple in Basic, orange in the neutral
Dark/Light palettes) made it look like an arbitrary, theme-dependent color
choice instead of a stable identity for "this is the Playbook view," and made
before/after comparisons across themes inconsistent for no user benefit.

## Consequences

Any UI element that is documented as "reading as part of the Playbook frame" —
border, corner glyph, the ☰ session-actions icon on the border, the
chat-mode/Playbook toggle glyph in the title bar, the status spinner that
replaces it — must derive its color from this fixed accent, not from the
active theme's `accent_alt`. Adding a new named theme must set this fixed
accent explicitly (to the appropriate light/dark cyan variant) rather than
leaving the Playbook frame to inherit that theme's own second accent.

This does not extend to floating popups that merely appear over or near the
Playbook pane (e.g. the smart-clip autocomplete menu, the session hover-card
preview) or to content-level styling inside a playbook document (heading
colors, checklist bullets, collaborative cursor colors) — those continue to
follow the active theme's own palette like any other UI surface.

## Non-Goals

This does not fix any other UI surface's color to a specific theme, and does
not change how `accent_alt` behaves for its other, unrelated uses (matrix rain
word-reveal accents, playbook markdown bullet/heading colors, collaborative
cursor colors).
