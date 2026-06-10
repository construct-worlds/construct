# 0027-loadout-screen-for-new-sessions

Status: accepted
Date: 2026-06-07
Area: tui
Scope: Creating a session in the TUI happens through a full-screen cinematic "loadout" composer, not a one-line minibuffer prompt.

## Decision

The new-session action (`OpenNewSession`, bound to `C-x C-f`) opens **the Construct loadout** — a full-screen takeover that composes the resources an agent task needs before it is spawned:

- **Harness** — the harness that drives the session.
- **Working dir** — the working directory, plus an optional isolated git worktree.
- **Initial prompt** — the first prompt handed to the harness.

The slots are labeled with plain terms (`HARNESS` / `WORKING DIR` / `INITIAL PROMPT`), not themed names — the screen's identity comes from its framing, not from renaming the inputs.

Confirming the loadout ("LOAD" / "jack in") creates the session from the assembled values in one shot. The screen replaces the previous one-line harness-picker minibuffer entirely; there is no separate quick path.

Properties that must hold:

- **It is a full takeover.** While open it captures all keyboard input and the main TUI layout is not drawn underneath it.
- **The prompt comes first.** The initial prompt is the top slot and holds focus on open — the task is what the user composes; harness and directory are pre-filled details below it. While the prompt is empty, Enter confirms immediately (preserving the open → Enter fast path); once it has text, Enter inserts newlines and an explicit chord confirms.
- **It opens with working defaults, and the defaults learn.** A harness is preselected (the last successfully used one when still available, else a real available one), the working directory defaults to the last used one that still exists (else the process directory), the worktree toggle remembers its last setting. Learned defaults persist across launches with the other TUI preferences.
- **Mistakes are caught before they cost anything.** A working directory that doesn't exist is flagged inline and blocks confirmation; a typed-but-unconfirmed prompt is protected from a single stray Esc (a second Esc discards).
- **Mouse parity.** The slots, harness cards, worktree toggle, and a visible confirm button are clickable, matching the rest of the TUI's mouse support.
- **The entrance is a brief, skippable flourish.** A short staggered "rack-slide" animation plays on open (evoking the Construct loading program). Any key skips straight to the composed form; it never blocks interaction for more than its short duration.
- **The synthetic `project` option lives here.** Creating a project (session group) is offered as one of the WEAPON cards, preserving discoverability now that the harness-picker minibuffer is gone. Choosing it routes to the existing project-name prompt instead of spawning a session.
- **It composes over the existing create-session contract.** The loadout only fills fields the session-create request already supports (harness, cwd, worktree, initial prompt, inherited group). It does not require new protocol surface to deliver the working directory or initial prompt.
- **Group context is inherited from the current selection**, exactly as the prior flow did, so a session created "inside" a group joins it.

## Reason

The single-line harness picker only let the user choose a harness; the working directory was always the TUI's process directory and there was no way to set an initial prompt, even though the daemon already accepted both. Composing the task up front — harness + where it runs + what it should do — is the natural unit of "starting work," and a dedicated surface gives the initial prompt room to breathe instead of a cramped command line. The cinematic framing is a deliberate product identity choice for "construct": kitting out a loadout before a run.

## Consequences

- New session creation is a distinct UI mode with its own focus/state machine, key handling, and render path, separate from the minibuffer.
- The minibuffer is no longer used for harness selection; that machinery (harness picker render, click-to-pick hit-testing, Tab-completion of harness names) was removed rather than left dead.
- Any future entry point that wants to create a session should open the loadout (or call the same create-session helper) rather than re-introduce a one-line harness prompt.
- The loadout must keep a fast confirm path: opening and immediately confirming with defaults must remain possible, so the cinematic and the slot navigation must never be mandatory steps.
- The working-directory picker starts minimal (an editable path with filesystem completion + a worktree toggle); a richer browsable picker is a future extension and must not regress the confirm-with-defaults path.

## Non-Goals

- This does not change how sessions are created at the protocol/daemon layer.
- It does not define a full filesystem browser or multi-file context selection; only a single working directory in its first form.
- It does not govern non-TUI clients (web UI), which may present session creation differently.
