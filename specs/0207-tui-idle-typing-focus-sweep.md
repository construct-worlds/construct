# 0207-tui-idle-typing-focus-sweep

Status: accepted
Date: 2026-08-21
Area: tui
Scope: The focused pane replays its focus-acquisition sweep when typing resumes after a long silence.

## Decision

When a keystroke reaches the pane that already holds keyboard focus, and no keystroke has reached a focused pane for at least an idle threshold in the low single-digit minutes, that pane replays the focus-acquisition border sweep — the same animation, on the same surface, with no second visual invented for this case.

The transition is what animates, not the typing:

- Every key the focused pane consumes refreshes the idle clock, so continuous or rapid typing never replays the sweep. At most one replay per idle-then-resume transition.
- A key that arrives while a sweep is already on screen is absorbed: it neither restarts nor extends that sweep. Focus acquisition and idle resume therefore never stack into a double animation, including for the very keystroke that moved focus.
- The first key of a session counts as ending an idle gap, since the TUI has been sitting untouched since it opened.

Only keys that a focus-bearing surface consumes qualify. Floating keyboard surfaces — modal dialogs, transient popups, the prompt/minibuffer strip — are not focus-border targets, so keys they swallow animate nothing; the pane gets the cue back on the next key that reaches it. Surfaces hosted *inside* a focused pane are the pane for this purpose.

The idle threshold is a single named constant with an environment override, so demos and recordings can shorten the wait without the shipped default changing.

## Reason

Focus acquisition answers "where did my focus go" at the moment it moves. It does not answer "where does my typing go" for a user returning to a terminal they left minutes ago — by then the acquisition cue is long gone, and a fleet of similar panes gives the eye nothing to lock onto. Replaying the cue on the resuming keystroke costs one 200 ms animation per absence and nothing at all during work.

The threshold sits in the minutes because anything shorter fires during ordinary working pauses — reading output, waiting out a harness turn, composing the next instruction — where the user has not lost track of focus and the motion is pure distraction.

## Consequences

- Idle-resume derives from the same animation state as focus acquisition, so any change to the sweep's look, duration, or geometry applies to both without a second code path.
- The animation still requests frames only while visible; an idle TUI keeps its existing wake cadence, and a keystroke that otherwise skips its paint (PTY passthrough) must still paint while a sweep is on screen.
- The set of surfaces treated as "not the pane" is conservative: a popup that consumes some keys and falls through on others suppresses the cue for the key that dismisses it. A missed cue is preferable to flashing a border the keystroke never reached.

## Non-Goals

- Signaling idleness on its own, without a keystroke — no timeout-driven pulsing, breathing, or dimming.
- Animating mouse activity, incoming harness output, or session state changes.
- Changing focus order, keybindings, input routing, or the focus-acquisition behavior itself.
