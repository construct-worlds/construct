# 0024-operator-panel-shows-observation-trigger

Status: accepted
Date: 2026-06-06
Area: tui
Scope: What the operator panel shows for loop/observation-driven turns.

## Decision

When an `OBSERVATION:` trigger (an ambient monitor finding or a fleet event) drives an operator turn, the operator now echoes that trigger into its panel — dim, above the response — so a reply like `noted` has its question visible. Real user input is not echoed (it echoes itself). The monitor's instruction boilerplate is stripped so only the substance (the finding / event) shows.

The dead ambient-turn PTY/message suppression (keyed on the stale `"ambient operator loop tick"` string from before [0022](0022-operator-ambient-loop-runs-as-monitor-subagent.md)) is removed: the operator's responses are meant to be visible, not hidden.

## Reason

After 0022 the loop's observation string changed, so the suppression never fired and every tick's response + `Worked` status leaked into the panel — but the *input* (a `Message` event, not PTY) was never rendered there. The result was a stream of context-free `noted / Worked / noted …`. The user wants to *see and reason about* what triggered each response, so showing the trigger (not hiding the response) is the fix. Leaving the stale suppression in place was also a trap: "fixing" its string would have silently hidden the responses the user wants.

## Consequences

The panel reads as question → answer: the trigger, then the operator's `noted`/finding/reply. Findings still also surface via the typewriter monolog ([0023](0023-operator-monolog-typewriter.md)) and widgets. The echo fires for fleet-event observations too (consistent context for those replies). The monolog is unaffected — it consumes the response `Message` stream; the echo is PTY-only.

## Non-Goals

Not a new event type (the echo is dim PTY, like any other panel output); does not change rate limiting, the monitor, or the monolog; does not reinstate any ambient-turn suppression.
