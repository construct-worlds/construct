# 0151-ambient-feature-status-and-fallback

Status: accepted
Date: 2026-07-27
Area: ux
Scope: Ambient smith-dependent features must report their status with a cause, degrade through fallbacks where possible, and never silently no-op.

## Decision

The daemon's *ambient features* — the conveniences it runs on the user's
behalf that depend on the built-in smith harness having a usable model
credential (session auto-naming, next-prompt suggestions for smith/shell
sessions, the minibuffer session) — follow three rules:

1. **Status is queryable and mapped to causes.** The daemon exposes a
   feature-status surface listing each ambient feature as working, degraded,
   or off, with a human-readable reason naming the missing dependency and
   what still works. Clients (the configure dialog, the harness-listing CLI
   output) render these rows verbatim rather than re-deriving the
   dependency themselves.

2. **Auto-naming falls back to the session's own harness.** When the smith
   title generator cannot name a session, a session on a model-backed
   harness gets its title from a hidden one-shot probe session on that same
   harness — the credential the user's session already proved works. smith
   and shell sessions have no fallback and keep their default names.

   "Cannot name a session" is decided by what the *title generator* can
   resolve, not by whether a smith session could start: those are different
   questions on a machine whose only credential is one the title generator
   doesn't accept (spec 0071). It also covers a generator that ran and came
   back empty — a credential that exists but fails is indistinguishable, to
   the user, from one that was never there. Since a session gets one
   auto-naming attempt per daemon run, a generator that fails must hand off
   to the remaining one rather than spend the attempt.

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
- Each generator owes its caller the difference between "this session is
  named, or has stopped wanting a name" and "I produced nothing". Only the
  second hands off. A session deleted or manually renamed mid-generation
  falls in the first group — the handoff must never overwrite a title the
  user chose.
- The feature-status row for auto-naming reports on the generator
  auto-naming would actually use. It cannot be derived from general smith
  availability without promising a generator that never runs.
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
- A user's only credential is an OAuth subscription, pinned as smith's
  model. smith sessions run fine, but the title generator cannot use that
  credential, so auto-naming goes straight to the same-harness probe rather
  than spending the session's one attempt on a generator that must fail.
- A user has an API key that has since been revoked: the title generator
  starts, errors, and hands the session to the same-harness probe, which
  names it. The session does not sit on its hash name until the daemon
  restarts.
- A user opens the suggestion orb in a shell session on a credential-less
  machine: generation is skipped, the daemon latches and broadcasts the
  degradation, and the client shows a clickable notice that opens the
  configure dialog.
