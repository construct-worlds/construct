# 0209-route-model-lists-are-discovered-live

Status: accepted
Date: 2026-08-22
Area: architecture
Scope: Route targets whose provider exposes a model-listing endpoint get their picker rows fetched live, additively over the curated catalog.

## Decision

For every route target whose wire provider has a known model-listing
endpoint, the daemon fetches the live model list and appends the
discovered ids to that target's picker rows. Four rules hold:

1. **Additive, never replacing.** The declared model and the curated
   catalog keep their order at the front of the list; discovered ids
   follow, deduplicated, in the order the endpoint returned them.
2. **Best-effort, never load-bearing.** A failed, slow, empty, or
   unsupported listing degrades to exactly the curated behavior. Menus
   never gate what can be requested, so discovery failing must be
   indistinguishable from discovery not existing.
3. **Bounded and cached.** Listings are fetched on demand (menu opens,
   plus a background warm at router start) through a TTL cache keyed by
   endpoint, under a fixed wall-clock budget. Opening a menu may be
   seconds late once; it never hangs, and repeated opens are free.
   Failures are cached on a shorter TTL than successes.
4. **Switchable off.** A single router setting suppresses all listing
   requests, for environments where the daemon must not originate them.

## Reason

The curated catalog goes stale on human timescales — every vendor model
launch needed a source edit before its id appeared in a picker, while the
id itself already worked when typed. Most routable providers expose a
listing endpoint; using it makes new models (aggregator and stealth ids
especially) appear in menus the day they exist. But listings are remote,
slow, and sometimes wrong, and pickers are interactive — hence additive
merging over a stable curated front, strict time budgets, and failure
behavior identical to the pre-discovery world.

## Consequences

- The curated catalog remains authoritative for ordering and for
  providers without a listing surface (subscription logins, vendors with
  no public endpoint); it must not be removed on the theory that
  discovery replaces it.
- OpenAI-compatible listings mix chat models with embeddings/speech/image
  ids; a small heuristic denylist hides obviously non-chat ids from
  menus. Over-filtering only hides a row (typed ids still work), so the
  filter must stay conservative.
- Discovered lists can be long; picker surfaces must tolerate hundreds of
  rows for aggregator targets.
- Anything consuming a target's model list (route menus, native-harness
  catalogs) inherits discovered ids from the shared list builder; new
  consumers should reuse it rather than re-fetch.

## Non-Goals

- Discovery does not extend to metadata (context windows, pricing,
  capabilities) — ids only. Metadata sync is a separate decision.
- Subscription (OAuth) targets keep their seeded/configured lists; their
  backends have no equivalent public listing surface.
- No periodic background polling beyond the start-time warm: refresh is
  driven by menu opens so an idle daemon originates no listing traffic.

## Examples

- A vendor ships a new model at noon; opening the route menu after the
  cache TTL shows it under that vendor's target with no Construct change.
- An aggregator's stealth model appears in the discovered tail of the
  aggregator target while the curated front (`auto`, pinned ids) stays in
  place.
- Pulling the network cable leaves menus exactly as they were with
  curation alone, after at most one bounded wait.
