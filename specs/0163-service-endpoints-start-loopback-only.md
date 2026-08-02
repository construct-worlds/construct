# 0163-service-endpoints-start-loopback-only

Status: accepted
Date: 2026-07-30
Area: architecture
Scope: The initial service-endpoint ingress boundary.

## Decision

Services are daemon-owned definitions. In v1 each service is declared in its
own `services/<name>.toml` file under the Construct configuration directory and
start a generic HTTP webhook only on loopback. Public reachability, when
explicitly requested for an active channel, is layered over that listener by
the channel-publication boundary in 0174. A service owns its instruction,
harness, model, working directory, and routing policy; a channel supplies the
transport-specific credential and listener configuration. A service may own
zero or more named channel instances. Channel ids are stable keys inside the
service definition, and each HTTP channel owns its own loopback port,
credential, and enabled state. The HTTP port is therefore channel
configuration, not service configuration; two HTTP channels on one service
must use different ports. Channel definitions live in the daemon-wide
`channels.toml` catalog beside the `services/` directory. A service TOML's
`channels` map is its attachment state: attaching copies a catalog definition
into the service, while detaching removes only that attachment and preserves
the catalog definition for later reuse. Channel ids are globally exclusive;
the catalog reports the owning service so clients can disable an already-used
channel. Existing nested channel definitions migrate into the catalog. V1
supports HTTP channel instances only, while the resource model leaves room for
future channel kinds.

The TUI renders services as top-level rows in the ordinary session list rather
than in a separate sidebar section. A service row uses the distinct `◈` type
glyph, participates in the same scrolling, keyboard navigation, and pointer
selection as a session, and opens a first-class service view in the active
split rather than a modal. Expanding or collapsing a service's routed-session
children is a per-user TUI preference that survives client restarts; stale
preferences are discarded when their service no longer exists. The view presents
fields and focused-field guidance side by side with a padded interior, stacking
the guidance below the fields only when the terminal is too narrow for two
readable columns. The same view
exposes two related sections—`Channels` and `Sessions`—and keeps
create/edit/delete actions in the view's normal focus lifecycle. `Activity` is
not a third section: its aggregate count is represented by the `Sessions`
header. Each routed session row identifies the channel that created it and the
caller-facing session key (or `event` for per-event sessions). Selecting a row
with the pointer jumps directly to that ordinary session view; the row remains
the navigation bridge between service configuration and the live conversation.

The service view uses the same top-right title-actions affordance as a session
view. Its menu contains only pane actions—split or close the pane—and delete
the service. Service definition fields are edited directly in the focused
service view; channel-specific editing and credential rotation belong to the
selected channel row, while pausing or resuming ingress is a service field.
The editor lists every catalog channel under `Channels`: an attached channel uses
the filled-square `▣` glyph, an available channel uses the empty-square `▢`
glyph, and a channel owned by another service is dimmed with its owner shown.
Space attaches or detaches the selected available/owned channel; Enter edits
an attached channel. The routed-session list is a separate `Sessions` section
below the channel catalog. Global `C-x` chords remain available while the service view is
focused; session-only commands such as `C-x .` explain their scope instead of
being silently swallowed. When a remote-control dialog, session picker, or
other minibuffer is open, that transient surface owns keyboard input before
the service view's default key handling; service-view commands resume when it
closes.

While editing, `C-n` and `C-p` move between definition fields alongside the
arrow and Tab keys. Harness and model are picker-only fields: typed, pasted,
and backspace input never edits them. Enter opens an inline list of detected
harnesses or the same route/model catalog used by the pin-router picker; the
same motion keys move the highlighted choice before Enter applies it. The
current value remains selectable when it is not present in the detected
catalog, so editing an older or custom definition never silently discards its
value. Row text stays focused on the value; the contextual guidance column
provides the action hint instead of repeating key instructions on every row.

The WebTUI mirrors this surface at `/services/<name>`. Services appear in the
same list model as sessions, using the same `◈` glyph and row selection. Its
wide service view uses the same definition/help columns, and narrow layouts
stack those columns. A service may occupy a shared split pane alongside a
session; a pane identity therefore carries either a session id or a service
name, never an ambiguous URL or display label. Its `Sessions` rows expose the
same channel association and select the matching session when clicked.

The HTTP channel accepts authenticated JSON deliveries and routes them into
ordinary headless Construct sessions. `session-key` routing persists the
key-to-session mapping, namespaced by channel id, `single` uses one shared
mapping per channel, and `per-event` creates a fresh session. Disabled
channels do not bind listeners after restart; pausing a service disables all
of its channels. Service ingress acknowledges accepted work
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
- Channel mutations preserve other channels and the service behavior fields;
  detaching the final channel leaves a valid but inert service definition and
  keeps the channel available in the catalog.
- Channel catalog ownership is exclusive across services. Detach is reversible
  and never destroys channel credentials; permanent catalog deletion is outside
  the v1 service-view flow.
- Channel summaries never return bearer credentials. Creation and explicit
  rotation return a generated credential once, and ordinary service edits do
  not replace it.
- Channel-scoped routing and request deduplication keep equal caller keys or
  request ids on different channels from colliding accidentally.
- Bearer credentials are service-scoped and never logged.
- A tunnel route extends this boundary only through an explicit per-channel
  publication action; none is implied by enabling or attaching a service.
- Service views participate in shared split layout state across TUI and
  WebTUI, while keyboard/pointer focus remains client-local.
- TUI service-row disclosure state is client-local and persists across
  launches rather than becoming shared daemon state.
- V1 does not promise channel plugins or response streaming.
  Runtime definition reload was subsequently added; see the spec on applying
  service definitions without a restart, which supersedes this consequence.

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
