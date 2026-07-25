# 0111-routing-injection-is-probe-verified

Status: accepted
Date: 2026-07-25
Area: harness
Scope: How Construct decides that a harness can be routed, and what it does when it cannot tell.

## Decision

Whether a harness honors Construct's routing injection is an **empirical,
per-harness, per-version fact**, established by a probe, never inferred
from documentation or from reading the harness's configuration.

- A harness is route-capable only if a probe has shown that it honors the
  proxy environment. Route capability is declared per harness alongside
  its other adapter capabilities.
- Interception additionally requires a per-process certificate-trust
  channel the harness honors. A harness that honors the proxy environment
  but no trust channel is pass-through only.
- Construct never parses a harness's own configuration files to discover
  its endpoint. The origin is observed from the connection
  ([0109](0109-model-routing-is-proxy-transported.md)).
- When a routed-capable session produces no proxied connection, Construct
  records the session as **routing inert** and says so. It never reports
  the session as routed.

Any endpoint value Construct derives rather than observes is a hypothesis.
It must be confirmed against an observed connection before a route is
armed; on mismatch, Construct refuses to arm and reports both values.

## Reason

The alternative — deriving what a harness would have done — requires
reproducing that harness's configuration precedence. That precedence is
undocumented in places, differs per harness, and changes between releases.
A belief about it rots silently, and the failure it produces is a
misrouted credential.

A probe converts the belief into a test. It fails loudly at a CLI version
bump instead of misrouting a user, and it costs one small fixture per
harness.

Refusing on mismatch, rather than proceeding on the derived value, keeps
every error on the safe side: a refusal leaves the session exactly as it
would have been without Construct, which is always a correct outcome.

## Consequences

- Adding routing support for a harness means adding a probe, not reading
  its docs. A harness with no probe is not route-capable, and offering to
  route it is a bug.
- Probes exercise the real harness binary against local stubs and must be
  re-run when a harness version changes. A probe that cannot run (harness
  not installed) leaves capability unproven, which means unavailable.
- Detection of whether injection took effect is by observation of the
  proxy, not by prediction. Non-arrival is a supported outcome with a
  clear report, never an error state for the session.
- Enforcement mechanisms that would prove the absence of a bypass by
  denying direct egress are diagnostic only. Arming them in normal use
  would convert a benign inert session into a dead one, which contradicts
  [0109](0109-model-routing-is-proxy-transported.md)'s rule that a
  routing failure must not degrade the session.
- Because capability is per-version, the set of routable harnesses on a
  given machine is discoverable at runtime and may differ between
  machines. Clients must render unavailability with its reason rather than
  hiding the option.

## Non-Goals

- Emulating any harness's configuration precedence.
- Guaranteeing that a harness cannot bypass the proxy. Construct reports
  what it observes; it does not confine the harness during normal use.
- Probing anything about a harness other than whether Construct's
  injection reaches it.

## Examples

A probe points the proxy environment at a local stub, runs the real
harness binary against a trivial prompt, and records whether the stub
received a connection. The harness is route-capable if and only if it did.

A session is armed for routing but the harness resolves its endpoint
through a channel that ignores the proxy environment. No connection
arrives. Construct marks the session routing inert, shows why, and leaves
the session working exactly as it was.
