# 0199-operator-and-minibuffer-naming

Status: accepted
Date: 2026-08-16
Area: convention
Scope: The two core nouns for standing agents: operators answer channels, the minibuffer dispatches the fleet.

## Decision

- An **operator** is a named, durable definition that answers messages
  arriving on its attached channels by spawning sessions. This concept was
  previously called a *service*.
- The **minibuffer** is the daemon-created dispatcher session rendered in the
  TUI's bottom strip and the web UI's panel. This session was previously
  called the *orchestrator* (session kind) and *operator* (UI label). The
  word "operator" never refers to this session anymore.
- Prose that means the human at the controls says **the user**, never "the
  operator".
- Pre-rename spellings remain readable, not writable: persisted
  `orchestrator` session kinds, `operator_loop_disabled` flags, the
  `[orchestrator]` config table, `CONSTRUCT_OPERATOR_*` tuning env vars,
  `services/` config and data directories, `service:<name>` session-title
  prefixes, and the `/service`, `/services` slash commands are all accepted
  or migrated. New writes always use the new names.

## Reason

"Service" described plumbing, not the product intent: operators are being
grown into role-specialized agents that can also receive fleet-internal
deliveries (inboxes) and work together, not just answer external endpoints.
"Operator" carries that identity (an always-on specialist at a post, in a
Matrix-native register) while remaining a plain English job word. The
dispatcher session had to give up the "operator" label for that to be
unambiguous, and naming it after the surface everyone already used for it —
the minibuffer — avoided introducing a third term.

## Consequences

- Future features address operators by name; nothing new may reuse
  "service" for this concept, and "operator" must not be reused for the
  dispatcher or for the human.
- The compat aliases above must keep loading until a deliberate break is
  specced; removing one is a behavior change, not cleanup.
- Channel vocabulary is unchanged (channels stay channels; Slack's own
  "bot token" / "app token" terms stay Slack's).

## Non-Goals

- Renaming the TUI's transient input-line widget (internally `Prompt`); it
  is invisible vocabulary.
- Prescribing the future inbox channel design; only its naming home.
