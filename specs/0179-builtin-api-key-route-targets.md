# 0179-builtin-api-key-route-targets

Status: accepted
Date: 2026-08-02
Area: architecture
Scope: When a direct-API-key provider may be offered as a route target without the user declaring an endpoint for it.

## Decision

A provider with a single well-known public endpoint may ship as a **built-in
route target**: when its API-key environment variable is present in the
daemon's environment, it appears in every route picker and native model
catalog with no declaration in `config.toml`.

A built-in is a *default*, never an override:

- A user-declared profile carrying the same route name replaces the built-in
  entirely — its base URL, credential, and default model all win.
- The built-in is synthesized as an ordinary profile, so it is subject to the
  same dialect resolution, credential check, model list, and blocker reporting
  as a declared one. There is no second target type.
- Absent the credential, the target does not exist. It is never listed as
  present-but-blocked, because there is nothing the user declared that a
  blocker would be explaining.

Every direct-API-key provider Construct speaks to qualifies, and all of them
are built in: Anthropic, OpenAI, Gemini, Meta, Grok, and DeepSeek. A provider
is admitted only when both hold:

- its public endpoint is the vendor's *only* one, and
- the router has a translator for its wire dialect, so the target is
  selectable and not merely listed.

The second is a hard gate, not a nicety. A built-in that cannot be routed to
is strictly worse than no built-in: it occupies a route name and spends the
picker's space to say "unavailable".

This does not extend to OAuth/subscription targets, which are discovered from
a local CLI's credential store and already appear automatically, nor to
providers whose endpoint genuinely varies per user — those must be declared.
A provider reachable through both an API key and a subscription login has two
targets, and that is correct: they are different billing paths, and the user
picks which one to spend.

## Reason

Route targets were previously either a subscription login (auto-discovered) or
a `[smith.models.*]` profile (hand-written). That left an inconsistent middle:
a provider reachable with one env var and no other choices still cost the user
a five-line config block before any harness could route to it — configuration
that carried no information, since every field was the vendor's only value.

The asymmetry was also user-visible in the wrong direction. Setting a key made
the provider work in smith immediately, but silently did nothing for the
routing pickers, so "I set the key" and "I can route to it" came apart with no
signal explaining why.

## Consequences

- Adding a built-in means asserting the endpoint is stable and singular. A
  provider whose base URL depends on region, tenant, or deployment must stay
  declaration-only; a built-in pointing at the wrong host fails on first use
  with no config for the user to inspect and correct.
- Route names become a shared namespace between built-ins and user config.
  The collision rule (declared wins) must hold in both directions: a built-in
  added later must never shadow an existing user profile, and removing a user
  profile may make a built-in reappear.
- A built-in's presence depends on the *daemon's* environment, not the user's
  shell. A key exported after the daemon started does not create the target
  until the daemon restarts — the same rule that already governs every
  API-key surface.
- Because a built-in is materialized as a profile, none of the router's
  downstream machinery (dialect translation, published-model ids, effort
  levels, picker blockers) needs to know built-ins exist.
- Retrofitting built-ins onto providers that previously required a profile is
  a behavior change for existing machines: pickers that were empty start
  listing entries. Each provider is admitted deliberately against the two
  criteria above, never because it resembles one already admitted.
- Because the credential check is the only thing gating a built-in, exporting
  a key now has one meaning everywhere: smith can use it *and* every routable
  harness can be pointed at it. A key that works in one place and silently
  not the other is the failure this decision exists to remove, so a new
  direct-API-key provider added to smith should arrive with its built-in.

## Non-Goals

- Not a plugin or discovery mechanism: built-ins are compiled in, not
  contributed at runtime.
- Not a way to smuggle in defaults for providers that need real configuration
  — if a reasonable person would need to look up what to put in a field, that
  provider is not a built-in.
- Does not change smith's own model resolution, which reaches these providers
  through explicit prefixes and its own credential ladder.

## Examples

- A machine exports only `DEEPSEEK_API_KEY`. A new Claude Code session's
  `/model` lists DeepSeek's models as Construct gateway entries, and the
  redirect menu offers DeepSeek — with an empty `config.toml`.
- The same machine adds a `deepseek` profile pointing at an internal
  OpenAI-compatible gateway. The picker now shows that endpoint; the public
  one is gone, because the declaration replaced the built-in rather than
  adding a second entry.
- The key is unset and the daemon restarted. DeepSeek disappears from the
  pickers rather than appearing with "no API key" next to it.
