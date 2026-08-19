# 0203-tui-focus-border-sweep

Status: accepted
Date: 2026-08-18
Area: tui
Scope: Keyboard focus acquisition is acknowledged by a brief directional highlight around the newly focused pane's full perimeter.

## Decision

When keyboard focus moves to a different TUI pane or focusable sidebar section, that surface plays a roughly 200 ms border highlight sweeping from its top-left toward its bottom-right, then settles into the ordinary focused-border appearance.

Focus identity includes the session-list rows, the lineage section, and each split window independently. Moving between sibling split windows therefore retriggers the animation even though both use the same general view-focus route. The initial pane at TUI startup does not animate.

The sweep temporarily draws all four border edges, including their corners, even when the pane's steady chrome hides its side and bottom borders or exposes only a header rule. The highlight changes the border line's foreground color and weight; it does not fill or reverse the cells' backgrounds. Once the sweep passes, the pane immediately returns to its configured steady border visibility.

The effect must not change pane geometry, content layout, terminal size, or input routing. Edge-to-edge borderless zoom views do not animate.

## Reason

Static border-color changes are easy to miss when focus jumps among similarly shaped panes, especially in a split layout. A short directional motion gives the eye a clear acquisition cue without becoming a persistent distraction or delaying interaction.

## Consequences

- Every keyboard, mouse, or external-controller path that changes the existing focus state receives the same affordance because rendering derives it from focus identity rather than from individual input handlers.
- The animation uses monotonic time and requests frames only while visible; an otherwise idle TUI keeps its normal redraw cadence.
- Focus changes during an active sweep restart the effect on the newest target.
- Pane-specific border hues and steady focused/unfocused semantics remain authoritative after the sweep ends.

## Non-Goals

- Animating selection changes within a focused pane.
- Persistently adding borders to borderless or intentionally hidden-edge layouts.
- Changing focus order, keybindings, or mouse behavior.
