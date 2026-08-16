# 0170-service-sessions-are-least-privilege

Status: accepted
Date: 2026-08-01
Area: architecture
Scope: The capabilities a session gets when its prompts come from a service channel rather than from the owner.

## Decision

A session created by a service is confined by default, and widened only by
explicit configuration on that service.

Every other session in the fleet is prompted by the owner, who is trusted with
the machine. A service session is prompted by whoever can reach its channel.
That difference is a capability boundary, not a policy preference, so the
confinement is applied at session creation rather than left to the harness or
to an approval prompt.

Withheld by default:

- **Tools that reach outside the session** — enumerating, reading, creating,
  driving, or destroying other sessions, and anything that acts on the daemon
  itself. A service session's blast radius is its own conversation and its own
  working directory.
- **Fleet access delivered through injected tool servers**, for harnesses that
  take it that way rather than as native tools.

Allowed by default, because they are session-local capability rather than
reach: the harness's own skills, and its ordinary working tools.

Filesystem and network confinement are deliberately *not* expressed as service
configuration. The harness sandbox already limits writes to the session's
working directory and denies egress, and a service definition must not be able
to relax a boundary the harness enforces.

A service definition may re-grant any withheld capability. Doing so is an
explicit, per-service statement that the channel's callers are trusted with
that reach. Because these limits are not part of the service edit surface,
editing any other field carries them forward unchanged.

## Reason

A service turns a request body into a prompt. Prompt injection is therefore
not a hypothetical for service sessions — it is the normal case, and the
attacker chooses the text.

Before this decision, a service session held the full fleet-control tool
surface. A request could ask the agent to enumerate every session on the
machine, read their transcripts, drive them with synthesized input, or delete
them. Enumeration succeeded outright; the destructive calls were stopped only
by the interactive approval gate — which is a prompt shown to a human who is
not necessarily watching, on a session created by a stranger's HTTP request.
An approval prompt is a reasonable last line for an owner-driven session and
the wrong only line for a service one.

Withholding at creation, rather than gating at call time, means the capability
is absent from the model's tool surface entirely. There is no decision to get
wrong, no prompt to socially engineer, and nothing for a misconfigured
approval mode to wave through.

## Consequences

- Adding a tool that reaches other sessions or the daemon means adding it to
  the withheld set; a new tool that silently lands in a service session's
  surface is a regression, not an oversight.
- Loosening any default here is a security change and must be argued as one.
- Re-granting reach is per service, never global and never per request.
- A service that legitimately orchestrates other sessions is still possible,
  but the user must say so in that service's definition.
- Filesystem and network confinement remain the harness sandbox's
  responsibility; a service inherits whatever that backend enforces on the
  host, including nothing on hosts where no backend is available.
- Because the harness sandbox is the floor, a harness without one gives a
  service session weaker filesystem and network confinement than this decision
  implies. Choosing such a harness for a service is a deployment decision the
  minibuffer owns.

## Non-Goals

- Per-request or per-caller capability grants; the boundary is the service.
- Replacing the approval gate. Approvals still apply to what remains; this
  decision reduces what is there to approve.
- Sandboxing the owner's own sessions more tightly.

## Examples

- A service is defined with no capability configuration. A request asking it
  to list every session on the machine gets a plain answer that it has no such
  tool, and no call is attempted.
- A user writes a service whose job is to triage incidents by opening
  sessions on their behalf, and grants that service reach explicitly. Its
  sessions can create and drive others; every other service on the host still
  cannot.
- A user renames a service and changes its instruction. The capability
  limits stored for it are unchanged by that edit.
