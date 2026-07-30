# 0159-grok-premints-native-session-id

Status: accepted
Date: 2026-07-29
Area: harness
Scope: Interactive Grok sessions bind their native conversation id at spawn via `--session-id`, not by post-hoc newest-mtime discovery under the shared cwd.

## Decision

On first interactive launch (not resume, not same-harness fork), the Grok adapter mints a UUID and passes it as `grok --session-id <uuid>`, then writes that id to `grok_session_id.txt` before the process starts.

Newest-mtime discovery under `~/.grok/sessions/<cwd>/` remains the mechanism for detecting a mid-session harness-native `/clear` (a fresh directory that did not exist at spawn). It is no longer used to *initially* bind a construct session to a native conversation.

Same-harness forks still use `-r <parent> --fork-session --session-id <new>`. Daemon resume still uses `-r <persisted>`. In both of those cases, and on fresh pre-mint, the adapter snapshots every pre-existing native session id under the cwd into the discovery exclusion set so bulk restart and sibling activity cannot steal the binding.

`skip_existing` (skip already-written native transcript lines when projecting into construct's transcript) is true only on daemon resume. A native fork is a brand-new construct session and must project the inherited parent history; a fresh pre-mint has nothing to skip.

## Reason

Grok has no originator tag that would let construct match "this Grok process" to "that session directory" (unlike Codex). The previous first-spawn path started Grok without `-r` / `--session-id` and let a background watcher claim whichever non-fork session directory under the cwd had the newest mtime.

In a shared cwd — the common case for any session not given its own worktree — that heuristic regularly claimed an older orphan conversation (including short one-off tests such as `echo "yes"`) before or instead of the live session. That wrong id was then what same-harness fork read from `grok_session_id.txt`. Because the client skips the portable transcript seed for natively-forking harnesses (spec 0031), the fork inherited only the orphan history.

Pre-minting matches Claude's first-spawn pattern (`--session-id`) and makes the binding authoritative from process start. Discovery keeps its only remaining job: noticing a genuine post-spawn `/clear`.

Separately, treating "we already know a native id" as `skip_existing` conflated resume with fork: a pre-minted fork id made the watcher skip the forked transcript file's prior lines, so chat view missed inherited parent history even when the Grok TUI had it.

## Consequences

- Fresh interactive Grok sessions always own a stable native id from the first moment; `grok_session_id.txt` is correct for later resume and same-harness fork without waiting on discovery.
- Pre-existing orphan session directories under the same cwd cannot be claimed as this session's conversation at startup.
- Mid-session `/clear` still rebinds via newest-mtime among directories that are not in the spawn-time exclusion set, not children, and not sibling construct sessions' current ids (specs 0088, 0138, 0085).
- Same-harness Grok forks continue to skip the portable seed when the parent has a native id; that id is now trustworthy for sessions created under this rule.
- Adapters that already mint or tag at spawn (Claude `--session-id`, Codex originator) are unchanged; this decision is Grok-specific because Grok is the discovery-only interactive harness among the native-fork set.

## Non-Goals

- Retroactively repairing construct sessions whose `grok_session_id.txt` already points at an orphan; those need a new session or a manual rebind.
- Changing which harnesses skip the portable seed on same-harness fork (spec 0031).
- Replacing Grok's on-disk session layout or adding an originator tag to the Grok CLI itself.

## Examples

1. User starts a Grok session in `/repo` while many historical Grok session dirs already exist there. Construct launches `grok --session-id <new-uuid>…`; the watcher is bound to that uuid from the start and never claims an older `echo "yes"` directory.
2. After real work in that session, the user forks Grok→Grok. The daemon reads the parent's pre-minted id, the fork launches with `-r <parent> --fork-session --session-id <fork-uuid>`, and the fork's model context contains the full parent conversation — not a short orphan.
3. User runs `/clear` in the live session. Grok creates a new directory; discovery sees an id that was not in the spawn-time exclusion set, rebinds, and the daemon synthesizes a reset-snapshot fork (spec 0085).
