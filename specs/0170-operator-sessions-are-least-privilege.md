# 0170-operator-sessions-are-least-privilege

Status: accepted
Date: 2026-08-01
Area: architecture
Scope: The capabilities a session gets when its prompts come from a operator channel rather than from the owner.

## Decision

A session created by a operator is confined by default, and widened only by
explicit configuration on that operator.

Every other session in the fleet is prompted by the owner, who is trusted with
the machine. A operator session is prompted by whoever can reach its channel.
That difference is a capability boundary, not a policy preference, so the
confinement is applied at session creation rather than left to the harness or
to an approval prompt.

Withheld by default:

- **Tools that reach outside the session** — enumerating, reading, creating,
  driving, or destroying other sessions, and anything that acts on the daemon
  itself. A operator session's blast radius is its own conversation and its own
  working directory.
- **Fleet access delivered through injected tool servers**, for harnesses that
  take it that way rather than as native tools.

Allowed by default, because they are session-local capability rather than
reach: the harness's own skills, and its ordinary working tools.

Filesystem and network confinement are deliberately *not* expressed as operator
configuration. The harness sandbox already limits writes to the session's
working directory and denies egress, and a operator definition must not be able
to relax a boundary the harness enforces.

A operator definition may re-grant any withheld capability. Doing so is an
explicit, per-operator statement that the channel's callers are trusted with
that reach. Because these limits are not part of the operator edit surface,
editing any other field carries them forward unchanged.

## Reason

A operator turns a request body into a prompt. Prompt injection is therefore
not a hypothetical for operator sessions — it is the normal case, and the
attacker chooses the text.

Before this decision, a operator session held the full fleet-control tool
surface. A request could ask the agent to enumerate every session on the
machine, read their transcripts, drive them with synthesized input, or delete
them. Enumeration succeeded outright; the destructive calls were stopped only
by the interactive approval gate — which is a prompt shown to a human who is
not necessarily watching, on a session created by a stranger's HTTP request.
An approval prompt is a reasonable last line for an owner-driven session and
the wrong only line for a operator one.

Withholding at creation, rather than gating at call time, means the capability
is absent from the model's tool surface entirely. There is no decision to get
wrong, no prompt to socially engineer, and nothing for a misconfigured
approval mode to wave through.

## Consequences

- Adding a tool that reaches other sessions or the daemon means adding it to
  the withheld set; a new tool that silently lands in a operator session's
  surface is a regression, not an oversight.
- Loosening any default here is a security change and must be argued as one.
- Re-granting reach is per operator, never global and never per request.
- A operator that legitimately orchestrates other sessions is still possible,
  but the user must say so in that operator's definition.
- Filesystem and network confinement remain the harness sandbox's
  responsibility; a operator inherits whatever that backend enforces on the
  host, including nothing on hosts where no backend is available.
- Because the harness sandbox is the floor, a harness without one gives a
  operator session weaker filesystem and network confinement than this decision
  implies. Choosing such a harness for a operator is a deployment decision the
  minibuffer owns.

## Non-Goals

- Per-request or per-caller capability grants; the boundary is the operator.
- Replacing the approval gate. Approvals still apply to what remains; this
  decision reduces what is there to approve.
- Sandboxing the owner's own sessions more tightly.

## Examples

- A operator is defined with no capability configuration. A request asking it
  to list every session on the machine gets a plain answer that it has no such
  tool, and no call is attempted.
- A user writes a operator whose job is to triage incidents by opening
  sessions on their behalf, and grants that operator reach explicitly. Its
  sessions can create and drive others; every other operator on the host still
  cannot.
- A user renames a operator and changes its instruction. The capability
  limits stored for it are unchanged by that edit.
