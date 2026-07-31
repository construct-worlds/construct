# 0165-pin-router-selects-reasoning-effort

Status: accepted
Date: 2026-07-31
Area: ux
Scope: how the Construct pin/redirect dialog chooses reasoning effort for a routed session.

## Decision

The model-router pin dialog may offer a third column of reasoning-effort
levels when the selected route and model advertise a real selectable scale.
Arming a pin records optional effort alongside route name and model; that
effort is durable session state restored with the pin, and the proxy applies
it on pin-routed requests.

- Effort remains request state on the wire ([0160](0160-reasoning-effort-is-routable-request-state.md)):
  the pin stores a preferred value that the proxy injects when the session's
  durable pin is the route in force.
- Request-scoped native catalog selections keep the harness request body's
  effort. Catalog-resolved arms do not carry the pin's effort.
- The third column appears only when `list_routes` returns more than one
  effort level for the highlighted model. Single-level "provider default"
  stubs and unsupported targets omit the column and arm without effort.
- Interaction: target selects, model selects (and commits immediately when
  there is no effort scale), effort commits. Keyboard Right walks
  targets → models → efforts; Left walks back.

## Reason

Users already pick effort in harness-native pickers when Construct publishes
a catalog. The pin dialog is the Construct-owned path for the same choice
when traffic is redirected via a durable session pin — without encoding
effort into model ids (forbidden by 0160) or inventing a second control
surface elsewhere in the status bar.

## Consequences

- `SessionRoute` and `session.set_route` carry optional `effort`.
- `RouteOption` carries per-model effort lists for the picker; empty means
  no third column for that model.
- `ArmedRoute` carries `pin_effort` only when the arm came from a pin.
  Rebuild/translate injects it before dialect-specific mapping.
- Restored pins re-apply the stored effort with the model.
- The modeline shows pin effort on the routed side of the substitution
  (`origin → model (effort)`).

## Non-Goals

- Per-request effort history for pins.
- Surfacing effort in harnesses that have no effort concept.
- Overriding effort on Construct-published catalog model ids (those remain
  harness-owned per 0157/0158).

## Examples

User opens the model-router dialog, picks `codex-oauth`, then `gpt-5.6-sol`,
then `high`. The session record stores
`{ name: codex-oauth, model: gpt-5.6-sol, effort: high }`. The next native
model request redirected by the pin is rebuilt with reasoning effort
`high`, even if the harness body said `medium`.
