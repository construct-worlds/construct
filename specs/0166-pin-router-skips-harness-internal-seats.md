# 0166-pin-router-skips-harness-internal-seats

Status: accepted
Date: 2026-07-31
Area: protocol
Scope: A session's pinned route governs the models that do the session's work, not the models a harness selects to fill its own internal seats.

## Decision

A pinned route substitutes the model on requests carrying a model the user
could have chosen. It does not substitute the model on requests carrying an
**internal seat** — a model the harness selects for itself to perform a
fixed role, such as an approval reviewer. Those requests pass through to the
harness's native provider on the harness's own credential.

The seat set is read from the harness's own catalog rather than maintained
as a list of model names. A catalog entry the vendor marks as not
picker-visible is an internal seat; an entry that is picker-visible, or that
makes no statement, follows the pin. The set is captured once when a session
attaches to the router, because the routing decision happens per request and
cannot afford to consult the catalog each time.

Every model substitution a route performs is recorded, as is every internal
seat that declines one while a pin is armed.

## Reason

A pin expresses "this session's work runs on model M at provider P." A
harness's internal seats are not that work. They are role-specific calls the
harness makes on its own behalf, against prompts and output contracts the
vendor built for one specific model.

Substituting there fails in ways the substitution itself hides. An approval
reviewer handed an unrelated model receives a prompt written for a
classifier, and the shape of what comes back is no longer what the harness
parses. When a reviewer cannot produce a usable verdict, the harness fails
closed and denies the action, so the visible symptom is a blocked session
rather than anything naming the router. Codex ships its reviewer as
`code_mode_only`, meaning it can only answer by emitting tool calls; a
session pinned to a model of a different tool shape puts a model in that
seat that cannot express a verdict at all.

The substitution is invisible from inside the harness, which keeps recording
the model it asked for regardless of what the router sent. Without a record
on our side, a session running on something other than what its transcript
claims cannot be diagnosed from either end. That absence, not the routing
itself, is what makes these failures expensive.

Reading the seat set from the vendor's catalog keeps the rule accurate as
vendors add roles, and keeps the daemon from asserting a fact about a
harness that the harness never told it.

## Consequences

- A pinned session still sends its own transcript to the harness's native
  provider whenever an internal seat runs. A pin is a routing statement, not
  a containment boundary, and must not be described as one.
- Internal seats keep consuming the native provider's quota and credential
  on a pinned session.
- A harness that publishes no readable catalog has no seats, so every model
  follows the pin exactly as before.
- When the catalog cannot be read, the seat set is empty and the previous
  pin-everything behavior returns. This is reported, never silent.
- Cross-provider pins remain unresolved: an internal seat has no counterpart
  on the target, so it passes through instead of being served by the pinned
  provider. Choosing between substituting a wrong model, disabling the role,
  and passing through is a separate decision.

## Non-Goals

- Not a per-thread or per-subagent routing model. A subagent doing the
  session's work follows the pin like any other work.
- Not a policy for which model *should* fill a role on a routed provider.
- Does not change how a request-scoped published model id is resolved; an
  explicit id still wins over the pin.

## Examples

- A session pinned to a third-party provider runs its main turn and its
  subagents on the pinned model. Its approval reviewer continues to run on
  the harness's own reviewer model, at the harness's provider.
- A harness marks a summarization model hidden in a later release. It
  becomes an internal seat with no change to the daemon.
- A pinned session's log records the substitution for each routed request,
  and records the reviewer declining substitution, so a transcript that
  names one model while the traffic went to another is traceable.
