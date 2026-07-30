# 0158-native-catalog-selection-is-visible-state

Status: accepted
Date: 2026-07-29
Area: ux
Scope: How clients display a harness's own selection of a Construct-published catalog model, and how that selection interacts with the manually pinned route in pickers.

## Decision

A harness-side selection of a Construct-published catalog entry
([0157](0157-native-model-catalog-routing.md)) is session state the user
can see, everywhere the session's model is shown.

- **Published ids render decoded.** A model id in Construct's namespace is
  routing data, not a label. Wherever a client shows a session's model —
  status/mode lines, session lists, tooltips, web UI — an id that decodes
  is rendered as its target model together with its route. The raw encoded
  id is never the display form. An id that claims Construct's namespace
  but does not decode is shown raw: display never invents a reading for a
  value the proxy would reject.
- **Storage keeps the harness's report.** The session record continues to
  carry the model exactly as the harness reported it. Decoding happens at
  the display edge. Rewriting the stored value would corrupt the paths
  that hand it back to the harness (respawn, resume) and would erase the
  evidence of what the harness actually asked for.
- **The id codec is a single shared contract.** The encoding is defined
  once, at the protocol layer, and every producer and consumer uses that
  definition. A client re-implementation in another language mirrors it
  and is covered by the same round-trip expectations.
- **Route pickers show the native selection as its own kind of "current".**
  Construct's route picker marks the natively selected route and model
  distinctly from the armed pin — they are different facts: the pin is
  durable session state this picker owns ([0114](0114-session-route-is-durable-session-state.md));
  the native selection is per-request state the harness owns. While a
  native selection is live, the picker also says so in prose, carrying the
  decoded pair, and "Default"/pass-through must not present itself as the
  current state. With no pin armed, the picker opens on the native
  selection.
- **An inert pin says it is inert.** Requests carrying a Construct id
  never consult the pin, so arming a pin while the harness is on a
  Construct catalog entry currently changes nothing. Arming must succeed
  (the pin applies once the harness returns to a native model) but the
  confirmation must state that it is waiting and why. A silent success
  here is a silent no-op, the class of surprise 0114 exists to prevent.

## Reason

Native catalog publication gave one session two model-selection surfaces
with different semantics. Both are legitimate; what is not legitimate is
either one lying about the other. Before this rule, a harness that picked
a Construct entry showed its raw encoded id in every client surface, the
route picker claimed "Default" while every request was being routed, and
arming a pin under a live native selection reported success while doing
nothing observable. Each of these makes routing invisible or misleading,
and invisible routing is indistinguishable from a harness that changed
its own model.

## Consequences

- Any new surface that displays a session's model must decode published
  ids before display. The decoded form pairs model and route; dropping
  the route half hides that the request leaves the harness's native
  provider.
- Clients derive the native selection from the model the harness reports.
  A harness that does not report its model change simply shows no native
  selection — absence of the marker is not proof of pass-through.
- The picker's marking must keep the pin and the native selection
  visually distinct; collapsing them into one "active" marker recreates
  the ambiguity this spec removes.
- Wording stays functional and plain: the picker explains the native
  marker in prose, not iconography alone.

## Non-Goals

- Driving the harness's native picker from Construct's route menu (a
  unified affordance is a separate decision).
- Per-request route history or display of subagent-level selections.
- Changing the proxy's resolution precedence, which
  [0157](0157-native-model-catalog-routing.md) owns.

## Examples

A Claude session picks `gpt-5.6-sol · codex-oauth · Construct` in its own
`/model`. The modeline shows `gpt-5.6-sol · codex-oauth`, not
`claude-construct-codex-oauth/gpt-5.6-sol`. Opening Construct's route
picker shows the `codex-oauth` target and `gpt-5.6-sol` model marked as
the harness's own pick, a line noting the pick was made in the harness's
own picker, and no "current" mark on Default.

With that pick still live, the user arms the `kimi` pin. The status
confirms the pin and states it takes effect when the harness leaves its
native pick.
