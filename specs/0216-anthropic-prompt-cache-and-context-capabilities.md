# 0216-anthropic-prompt-cache-and-context-capabilities

Status: accepted
Date: 2026-09-09
Area: harness
Scope: Smith declares Anthropic prompt-cache breakpoints and endpoint context capabilities without assuming every compatible endpoint is first-party.

## Decision

Requests to first-party Anthropic Messages endpoints place ephemeral cache
breakpoints on stable prompt prefixes: the system prompt, the tool-definition
prefix, and the newest user-message boundaries, up to the provider's four
breakpoint limit. First-party Claude OAuth requests use the same policy.
Anthropic-compatible third-party endpoints receive no cache directives by
default.

For models known to require Anthropic's one-million-token context beta, the
first-party API-key provider sends the capability header and reports the larger
effective window to Smith's budget manager. Model profiles may explicitly
configure cache-control support, beta capabilities, and an effective context
window so new first-party models and compatible gateways can be represented
without changing cross-provider defaults. A conservative 200,000-token fallback
remains for Anthropic-compatible endpoints with no declared capability.

## Reason

Smith resends a large, mostly stable system/tool prefix on every tool step.
Correct cache boundaries make that prefix reusable, while the larger-context
header must agree with the budget Smith enforces. Anthropic wire compatibility
alone does not imply support for Anthropic-specific cache fields or betas, so
blindly enabling them can break gateways and other providers.

## Consequences

- Cache directives stay inside first-party Anthropic request construction unless
  a profile opts a compatible endpoint in.
- For a beta-gated model, effective-window reporting and beta headers must
  agree; models/endpoints where the larger window is stable may declare the
  window without an obsolete beta.
- Cache-write and cache-read usage contributes to total input-window occupancy.
- Capability configuration is additive and provider-scoped, so unrelated Smith
  providers retain their existing request shapes.

## Non-Goals

This does not promise that every account is entitled to every Anthropic beta,
and it does not inject Anthropic cache fields into other Messages-compatible
services.
