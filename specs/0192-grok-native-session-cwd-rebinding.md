# 0192-grok-native-session-cwd-rebinding

Status: accepted
Date: 2026-08-04
Area: harness
Scope: Interactive Grok's native transcript watcher follows the on-disk session directory even when Grok chdirs away from construct's recorded session cwd after spawn.

## Decision

When construct has a known Grok native session id (pre-minted, resumed, or forked), the adapter locates that session's on-disk directory by:

1. Preferring the path under construct's recorded session cwd (`~/.grok/sessions/<urlencode(cwd)>/<id>/`), when that directory exists.
2. Otherwise scanning `~/.grok/sessions/*/<id>/` for a directory with that id (newest mtime wins if multiple).

Once the directory is found under a different cwd encoding, the watcher rebinds transcript, updates, context-gauge, and context-breakdown paths to that directory, and treats Grok's recorded cwd from `summary.json` (`info.cwd`) as the discovery cwd for subsequent mid-session `/clear` newest-mtime scans.

Sibling exclusion (spec 0088) continues to use construct's original session cwd when deciding which other construct sessions share a folder — it keys on construct metadata, not Grok's process cwd.

## Reason

Grok organizes native sessions per process cwd. Construct records the cwd at session create (typically the TUI client's `current_dir()`). After spawn, the Grok CLI may chdir into a project or git root (observed: `OLDPWD` stays at the spawn cwd while `PWD` becomes the repo, and files land under the repo's url-encoded path). The watcher then looked only under the spawn cwd, found nothing, and never emitted `ModelChanged`, `Cost`, or `ContextUsage` — even though those files existed under the rebased path.

This is system-specific in appearance: machines where the TUI is started from the same directory Grok ends up using never hit the mismatch.

## Consequences

- Model name, per-turn token usage, and context gauge/breakdown work for Grok sessions whose process cwd diverges from construct's recorded cwd.
- Pre-existing native directories under the rebased cwd are added to the discovery exclusion set when the rebase is first observed, so they cannot be mistaken for a mid-session `/clear`.
- Resume with `skip_existing` only advances transcript line cursors after the real directory is attached, so a delayed rebase does not re-project prior native history.
- Scanning all cwd encodings is O(number of distinct cwd folders under `~/.grok/sessions/`), not O(all sessions); it runs only when the preferred path is missing or paths need rebinding.

## Non-Goals

- Changing how construct chooses the spawn cwd for new sessions (TUI still uses its process cwd; minibuffers can pass `--cwd`).
- Moving Grok's own on-disk layout or preventing Grok from chdiring.
- Retroactively repairing construct transcripts that already missed events while the watcher was on the wrong path.

## Examples

1. TUI started from `$HOME`, user creates a Grok session (construct cwd = `$HOME`). Grok chdirs into `~/construct` and writes under `~/.grok/sessions/%2F…%2Fconstruct/<uuid>/`. The watcher finds that directory by id, rebinds, and emits model/usage from the real files.
2. Session created with `construct new --cwd ~/construct grok` and Grok never chdirs: preferred path exists; scan is unused.
3. Mid-session `/clear` after a rebase: discovery scans under the rebased cwd; pre-rebase baseline ids remain excluded.
