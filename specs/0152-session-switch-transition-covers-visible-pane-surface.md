# 0152-session-switch-transition-covers-visible-pane-surface

Status: accepted
Date: 2026-07-27
Area: tui
Scope: Session-switch transitions in TUI session panes.

## Decision

A session-switch transition is applied to the final composited content surface
of the split pane that changed sessions. It is painted after the pane's visible
surface and pane-anchored overlays, including Playbook documents, terminal or
chat content, widgets, previews, and hover cards.

Pane chrome remains stable, and other split panes do not participate in the
transition.

## Reason

The Playbook is a peer surface within a session pane, not a layer outside the
session-switch interaction. Applying the transition before the Playbook is
composited lets the Playbook clear and hide the effect, so switching to a
session with its Playbook open appears to glitch only the terminal exposed
underneath it.

## Consequences

- New surfaces or overlays rendered inside a session pane inherit the switch
  transition when they are composed before the pane-level transition pass.
- Transition state remains keyed by split-pane identity so one pane switching
  does not glitch neighboring panes that did not change.
- The pane border and title remain readable and visually stable during the
  short transition.
