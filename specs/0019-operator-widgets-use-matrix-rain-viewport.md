# 0019-operator-widgets-use-matrix-rain-viewport

Status: accepted
Date: 2026-06-05 (amended 2026-07-31)
Area: tui
Scope: Defines the ambient panel's selectable body modes and how the collapsed Operator communicates through it.

## Decision

The ambient panel's body is one of a small set of **named built-in modes** —
the Matrix rain animation and the fleet token meter (spec 0167) — selected by
the user and remembered across launches. Everything below applies to whichever
mode is selected: the Operator widget viewport is a transient overlay on top
of the body, not a mode of its own.

When the Operator session is collapsed, the panel may act as a transient viewport over the Operator session's normal sticky widgets. Operator widgets keep the same lifecycle as all session widgets: sessions create, update, and delete them, and the viewport only controls temporary visibility.

Updating an Operator widget briefly reveals it in the panel. The title bar shows the lowercase `operator` label followed by one square indicator per visible Operator widget, and a mode switch naming the mode currently showing, carrying the same swap glyph as the session list's view-mode toggle so the two controls read as one convention. Hovering the Operator label may reveal the current Operator status in a tooltip. Hovering a widget indicator may reveal that widget's title. Clicking an empty square selects and shows the widget; clicking the filled square hides the widget viewport. The existing close button continues to hide the Operator/ambient panel itself, and is distinct from the mode switch — switching modes never collapses the panel. When the widget viewport hides or no Operator widgets exist, the panel returns to its selected mode.

## Reason

The Operator is an ambient companion, not a critical notification system. Reusing normal session widgets avoids a separate notification lifecycle while still giving the collapsed Operator a peripheral surface for timely, glanceable help.

## Consequences

Missing or ignoring the Matrix-rain widget viewport must not block any user journey. The authoritative widget state remains the Operator session's widget set, and deeper interaction routes through normal widget actions or the Operator session. Future clients may choose a different compact presentation, but should preserve the same separation between widget lifecycle and transient ambient visibility.

## Non-Goals

This does not introduce widget TTLs, dismissed states, or guaranteed notification delivery. It does not make the panel an arbitrary model-drawn program independent of session widgets: modes are a fixed, named set the client ships, not a surface sessions can draw into directly.
