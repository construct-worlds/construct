# 0163-service-endpoints-start-loopback-only

Status: accepted
Date: 2026-07-30
Area: architecture
Scope: The initial service-endpoint ingress boundary.

## Decision

Services are daemon-owned definitions. In v1 each service is declared in its
own `services/<name>.toml` file under the Construct configuration directory and
expose a generic HTTP webhook only on loopback. A service owns its instruction,
harness, model, working directory, and routing policy; a channel supplies the
transport-specific credential and listener configuration.

The TUI lists services in a dedicated sidebar section between lineage and the
operator panel. Creating or selecting a service opens one editor for the full
definition; edits continue to preserve the one-file-per-service boundary. The
editor presents fields and focused-field guidance side by side with a padded
interior, stacking the guidance below the fields only when the terminal is too
narrow for two readable columns.

The HTTP channel accepts authenticated JSON deliveries and routes them into
ordinary headless Construct sessions. `session-key` routing persists the
key-to-session mapping, `single` uses one shared mapping, and `per-event`
creates a fresh session. Service ingress acknowledges accepted work
asynchronously; it does not hold the webhook connection for an agent turn.
The acknowledgement's session id can be queried through an authenticated
service-scoped result route. The result reports session status and the latest
assistant reply; it does not expose the full fleet transcript.

## Reason

Making a coding harness callable should not also make the owner's computer
publicly reachable. A loopback-only generic webhook provides a small,
auditable compatibility floor while preserving the existing explicit exposure
and owner-control boundaries. Reusing ordinary sessions keeps service work
visible and recoverable in the fleet.

## Consequences

- Service keys and session mappings survive daemon restart.
- Service ownership of keyed and per-event sessions survives daemon restart,
  so result reads remain scoped after recovery.
- Service definitions can be created, edited, and removed atomically without
  rewriting the global Construct configuration.
- Bearer credentials are service-scoped and never logged.
- A future tunnel route, plugin channel, or synchronous reply mechanism extends
  this boundary; none is implied by enabling a service.
- V1 does not promise runtime definition reload, sandbox-policy enforcement,
  channel plugins, response streaming, or public exposure.

## Non-Goals

- Replacing the remote-control listener or its owner authentication.
- Publishing a service automatically.
- Cross-channel identity linking or multi-tenant caller accounts.

## Examples

A local monitor POSTs alerts with a constant session key to one loopback HTTP
service. Every delivery enters the same headless session, which remains visible
to the owner in the normal fleet UI. The caller polls
`GET /svc/<service>/sessions/<session-id>` with the same bearer credential until
`ready` is true, then reads `reply`.
