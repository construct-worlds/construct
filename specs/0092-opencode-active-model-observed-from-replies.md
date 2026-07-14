# 0092-opencode-active-model-observed-from-replies

Status: accepted
Date: 2026-07-13
Area: harness
Scope: How an OpenCode session's active model becomes the model Construct records and shows.

## Decision

An OpenCode session's active model is observed from the assistant replies of its
own native conversation, and reported to the daemon as a model change like any
other harness reports one. The session's recorded model therefore reflects the
model OpenCode actually answered with — including a model the user never chose
explicitly (OpenCode's default) and one switched mid-session through OpenCode's
own model picker.

The observation is process-local, through the same injected plugin that records
the native session id, and is scoped to the conversation Construct owns:
replies belonging to OpenCode's own child sessions do not change the recorded
model. The reported spec is the `provider/model` pair OpenCode itself reports,
which is the form OpenCode's model flag accepts, so a resumed session is put
back on the model it was last running.

A session launched on an explicitly requested model reports nothing until the
model actually differs from the requested one.

## Reason

The recorded model has three possible sources: the model requested at create,
the models an adapter advertises at initialize, and model-change reports from a
running adapter. A wrapper harness that advertises no model list and never
reports a change leaves the session with no model at all whenever the user did
not name one — which is the common case, since OpenCode picks a default — and
every surface that shows the model falls back to "unknown".

Other wrapper harnesses observe the model by reading the transcript file their
CLI writes. OpenCode has no such file: its conversations live in a database
shared by every OpenCode process, so reading it back cannot distinguish this
session's model from a sibling's. Its own reply events are the only signal that
is both authoritative and process-local.

## Consequences

- The model is unknown until OpenCode's first assistant reply of the session.
  A model that is never used is never reported.
- The reported spec must stay in the form OpenCode's model flag accepts, since
  resume re-injects it. A provider whose model id itself contains separators
  must survive the round trip unchanged.
- A model switch is learned from the next reply, not from the switch itself:
  changing model and never sending a turn leaves the recorded model behind.
- OpenCode exposes no reasoning-effort setting, so no effort is reported and
  surfaces that show one for other harnesses show none here.
- Failure to observe the model must not prevent OpenCode from launching; it
  degrades to the model requested at create, or to none.

## Non-Goals

This does not let Construct drive OpenCode's model picker, and it does not
record a per-turn model history — only the currently active model.
