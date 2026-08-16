# 0019-minibuffer-widgets-use-matrix-rain-viewport

Status: accepted
Date: 2026-06-05 (amended 2026-08-16)
Area: tui
Scope: Defines the ambient panel's selectable body modes and how the collapsed Minibuffer communicates through it.

## Decision

The ambient panel's body is one of a small set of **named built-in modes** —
the Matrix rain animation and the fleet token meter (spec 0167) — selected by
the user and remembered across launches. A client with no recorded choice
opens on the token meter, and opens with the panel expanded rather than
collapsed: the meter answers a question the user has, where the animation is
ambience, and a meter nobody has been shown answers nothing. An absent setting means "never asked", which covers a
fresh install and an upgrade from before the panel had modes alike; anyone
who has actually picked a mode keeps it. Everything below applies to whichever
mode is selected: the Minibuffer widget viewport is a transient overlay on top
of the body, not a mode of its own.

When the Minibuffer session is collapsed, the panel may act as a transient viewport over the Minibuffer session's normal sticky widgets. Minibuffer widgets keep the same lifecycle as all session widgets: sessions create, update, and delete them, and the viewport only controls temporary visibility.

Updating a Minibuffer widget briefly reveals it in the panel. The title bar names the pane with the lowercase `monitor` title — the pane observes the fleet; `minibuffer` names the dispatcher session (spec 0199) and must never double as this pane's name. The title may also carry the Minibuffer affordances (loop toggle, status tooltip, approval alert, click-to-open) when the bar has no room for a second chip; the hover tooltip still names the Minibuffer so the control is not confused with the pane itself. Then come one square indicator per visible Minibuffer widget, and a mode switch naming the mode currently showing, carrying the same swap glyph as the session list's view-mode toggle so the two controls read as one convention. Hovering the title may reveal the current Minibuffer status in a tooltip. Hovering a widget indicator may reveal that widget's title. Clicking an empty square selects and shows the widget; clicking the filled square hides the widget viewport. The existing close button continues to hide the monitor/ambient panel itself, and is distinct from the mode switch — switching modes never collapses the panel. When the widget viewport hides or no Minibuffer widgets exist, the panel returns to its selected mode.

## Reason

The Minibuffer is an ambient companion, not a critical notification system. Reusing normal session widgets avoids a separate notification lifecycle while still giving the collapsed Minibuffer a peripheral surface for timely, glanceable help.

## Consequences

Missing or ignoring the Matrix-rain widget viewport must not block any user journey. The authoritative widget state remains the Minibuffer session's widget set, and deeper interaction routes through normal widget actions or the Minibuffer session. Future clients may choose a different compact presentation, but should preserve the same separation between widget lifecycle and transient ambient visibility.

## Non-Goals

This does not introduce widget TTLs, dismissed states, or guaranteed notification delivery. It does not make the panel an arbitrary model-drawn playbook independent of session widgets: modes are a fixed, named set the client ships, not a surface sessions can draw into directly.
