# 0117-subscription-logins-are-route-targets

Status: accepted
Date: 2026-07-26
Area: harness
Scope: How an existing subscription (OAuth) login on the machine becomes a route target, and why the router reads those credentials without ever refreshing them.

## Decision

A subscription login already present on the machine is offered as a route
target, alongside the declared model profiles, with no configuration
required.

- Logins are **discovered, not declared.** Nothing needs to be written in
  config for one to appear; it is offered as soon as its credential is
  readable and unexpired.
- **The router reads credentials and never writes them.** It never
  refreshes, re-mints, or otherwise updates a token in a store another
  application owns. When a token is expired or nearly so, the target is
  reported unavailable with an instruction to use the owning tool, and the
  route is not armable until then.
- A login's **endpoint is not configurable.** These are private backends
  reached with a credential minted for them; relocating one would make the
  credential meaningless. The only configurable aspect is which model to
  send.
- A login's required request decoration — the auth scheme, additional
  headers, and any mandatory system-prompt text its backend rejects
  requests without — is part of the target definition, not optional
  polish. A target whose decoration is unknown is not offered.
- A login whose backend speaks a dialect the router cannot translate is
  **not offered at all**, on the same principle that governs profiles: an
  unusable option is better absent than mistranslating.

## Reason

Model profiles assume an endpoint plus a key. A subscription login is
neither: the credential lives wherever the owning CLI put it, and the
endpoint is that CLI's private backend. Requiring users to describe those
as profiles would ask them to write down things they do not know and cannot
change, and the existing profile design explicitly excludes them for that
reason.

Meanwhile the daemon can already see which logins exist. Offering them
directly removes the most common reason the route picker is empty: a
machine with working subscriptions and no API keys had nothing to select.

The refusal to refresh is the load-bearing decision. Every OAuth client for
these services writes the refreshed token *back* to the shared store. A
second refresher racing the owning CLI can invalidate a token that CLI is
mid-turn on, and the resulting failure is intermittent, affects a tool the
user did not think they were changing, and gets attributed to the wrong
component. Read-only access keeps exactly one refresh owner — the tool that
created the credential — at the cost of a route that goes stale until that
tool is next used. A stale route with a clear message is a far better
failure than a corrupted credential store.

## Consequences

- A login's usable lifetime is bounded by whatever the owning tool
  maintains. Sessions must expect a route to become unavailable between
  turns, and the reason shown must distinguish *expired* (use the tool
  again) from *absent* (sign in), because the user's next action differs.
- Because credentials are read at arm time, a route armed with a valid
  token can still fail later in the session. That failure surfaces as an
  upstream error, not as a corrupted turn.
- Default models are built in per provider so that zero configuration
  works. A default that lags a vendor release produces a clean error naming
  the model; that is accepted in exchange for not demanding configuration
  for a login the machine already has.
- Adding a login means establishing its endpoint, dialect, auth scheme,
  required headers and required prompt text — the same evidentiary standard
  as adding a routable harness. None of it may be guessed.
- Reading another application's stored credential and presenting it to that
  application's backend is a decision with terms-of-service implications
  that belong to the operator, not to Construct. The mechanism is provided;
  the choice to enable routing is explicit and off by default.

## Non-Goals

- Performing OAuth flows: no sign-in, no device-code, no token minting, no
  refresh.
- Making a login's endpoint configurable.
- Offering logins whose backend dialect has no translator.
- Any attempt to keep a login alive in the background.

## Examples

A machine has a Claude subscription login and no API keys configured. The
route picker offers `claude-oauth` immediately, and a harness that speaks a
different dialect reaches it through translation. The request goes out with
the subscription bearer token, the backend's required beta header, and its
required identity line prepended to — not replacing — the harness's own
system prompt.

The same login, expired: the picker still lists it, greyed out, saying the
login has expired and naming the tool to run to renew it, and stating that
the router does not refresh another tool's credential.
