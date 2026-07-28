# 0151-ambient-feature-status-and-fallback

Status: accepted
Date: 2026-07-27
Area: ux
Scope: Ambient smith-dependent features must report their status with a cause, degrade through fallbacks where possible, and never silently no-op.

## Decision

The daemon's *ambient features* — the conveniences it runs on the user's
behalf that depend on the built-in smith harness having a usable model
credential (session auto-naming, next-prompt suggestions for smith/shell
sessions, the operator session) — follow three rules:

1. **Status is queryable and mapped to causes.** The daemon exposes a
   feature-status surface listing each ambient feature as working, degraded,
   or off, with a human-readable reason naming the missing dependency and
   what still works. Clients (the configure dialog, the harness-listing CLI
   output) render these rows verbatim rather than re-deriving the
   dependency themselves.

2. **Auto-naming falls back to the session's own harness.** When smith has
   no credential, a session on a model-backed harness gets its title from a
   hidden one-shot probe session on that same harness — the credential the
   user's session already proved works. smith and shell sessions have no
   fallback and keep their default names.

3. **A degradation that actually bites is announced once.** The first time
   an ambient feature skips work for lack of a smith credential in a daemon
   run, the daemon latches "degradation observed" and notifies clients.
   Clients show a visible, actionable notice (leading to the configure
   dialog) only when that latch is set — a credential-less machine whose
   user never touched an affected feature is never nagged, and a machine
   where the gap bit is never left guessing.

## Reason

Several construct conveniences historically no-opped with only a log line
when smith had no credential. Users saw hash-named sessions or a missing
suggestion orb with no path from symptom to cause: the harness-availability
surfaces (spec 0068, 0069) could say "smith is unavailable" but nothing
connected the degraded *features* to that fact. Worse, auto-naming depended
on smith even for sessions running fully-credentialed harnesses like a
logged-in claude CLI.

## Consequences

- New ambient features must plug into the same status surface and the same
  observed-degradation latch; a silent no-op on a missing credential is a
  bug, not an acceptable default.
- The status rows are the single source of truth for the feature→smith
  dependency mapping; clients must not hardcode it.
- Title generation now has two generators (smith one-shot, same-harness
  probe) that must stay behaviorally aligned: same sanitization, same
  eligibility rules for applying the result.
- The fallback spawns a short-lived hidden session on the user's harness;
  that cost is accepted in exchange for titles working without smith.

## Non-Goals

- Does not change harness availability semantics (spec 0068) or smith's
  fail-loud model resolution for real sessions (spec 0071). The probe
  fallback borrows the *target session's own* harness — it is not an
  implicit smith provider fallback.
- Does not add per-feature enable/disable config beyond what already
  exists.

## Examples

- A user runs only claude-CLI sessions with no API keys exported: sessions
  still get auto-titles (via a hidden claude probe), the features surface
  shows auto-naming as degraded with the reason, and no notice appears
  unless a smith/shell feature actually skips.
- A user opens the suggestion orb in a shell session on a credential-less
  machine: generation is skipped, the daemon latches and broadcasts the
  degradation, and the client shows a clickable notice that opens the
  configure dialog.
