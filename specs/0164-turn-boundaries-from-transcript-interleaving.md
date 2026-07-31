# 0164-turn-boundaries-from-transcript-interleaving

Status: accepted
Date: 2026-07-31
Area: tui
Scope: How the TUI locates "where a turn begins" in a rendered session so per-turn fork affordances can anchor there.

## Decision

Turn boundaries are derived from the transcript's own event interleaving:
a user `Message` event marks the start of a turn, and its position among
the persisted `Pty`/tool/message events IS the boundary's position in the
rendered history. No adapter emits a dedicated marker, and no bytes are
injected into the PTY stream.

Boundaries materialize as zero-height entries in the client's items-model
render pipeline, which reports each visible boundary's screen row. The row
anchors a hover-revealed `⑂` affordance; activating it opens the fork
flow with the branch point locked to the transcript seq just before the
turn's user message (spec 0163).

Boundary recording is limited to sessions whose rendered history the
client synthesizes itself — the items model: interactive smith and
headless harnesses. Raw-PTY sessions get no inline boundary anchors; for
them the per-turn fork surface is the turn picker (keyboard, palette, and
the session menu's "fork from turn"), which reads turns straight from the
transcript and works for every harness and every existing session.

## Reason

Two facts make the transcript interleaving the right source of truth:

- Both the live event stream and post-restart rehydration walk the same
  persisted, seq-ordered event sequence, so a boundary lands at the same
  point in the rendered history in both paths without any extra
  persistence.
- In-band PTY markers were considered and rejected: injecting bytes into
  a child's output risks splitting its escape sequences, and — decisive —
  it cannot help where it would be needed most. Wrapped harnesses that
  run as alternate-screen TUIs (claude, codex, grok, …) repaint their
  transcript region freely; there is no stable scrollback row for any
  marker to anchor to, in-band or not. A content-anchored icon there
  would be unreliable by construction, which is worse than absent.

The items-model restriction also protects rendering performance: raw-PTY
sessions ride a fast persistent-parser path and a shadow-parser
scrollback path that only exist while their history is pure PTY bytes; a
synthesized boundary item would silently evict them from both.

## Consequences

- Boundary placement inside PTY-backed items-model sessions is as precise
  as the adapter's event timing: the anchor lands where the user Message
  event interleaved with the byte stream, which can trail the on-screen
  echo by a moment. The fork anchor seq itself is always exact.
- Every per-turn fork surface (inline `⑂`, turn picker, chat view) must
  compute anchors the same way — the seq just before the turn's user
  message — so the same turn forks identically from any surface.
- Adding inline anchors for a new harness means making its rendering
  items-model (or otherwise client-synthesized), not inventing a marker
  protocol.

## Non-Goals

- Inline anchors on alternate-screen children's own rendering. If a
  harness someday exposes structured transcript rendering to the client,
  it can join the items model instead.
- Turn anchors in the wrapped-line chat view. Its renderer delegates
  wrapping to the widget layer, so it has no per-line screen geometry to
  hit-test against today; giving it anchors first requires client-side
  wrapping.
