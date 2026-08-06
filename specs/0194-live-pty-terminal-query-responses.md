# 0194-live-pty-terminal-query-responses

Status: accepted
Date: 2026-08-05
Area: protocol
Scope: Terminal queries from PTY-backed children receive one live response independent of the attached Construct client.

## Decision

The PTY adapter runtime is the terminal counterpart for queries whose answers
come from terminal state it can track, including cursor-position reports. It
answers those queries directly on the live child input stream and removes the
handled query bytes before emitting, persisting, or broadcasting PTY output.

Client renderers do not own these responses. A session receives the same
behavior in the native TUI, web UI, remote clients, and while detached.

## Reason

An interactive child runs behind Construct's PTY rather than directly inside
the user's outer terminal. Forwarding a terminal query to a renderer makes
startup depend on that renderer's emulation features and whether a client is
currently attached. Some full-screen harnesses require cursor-position reports
before drawing and exit when no answer arrives.

Central live handling also prevents two clients from racing to answer the same
query and prevents persisted query bytes from producing input during replay.

## Consequences

- PTY-backed harnesses can initialize while detached or under any Construct
  client surface.
- Exactly one response is written for each handled live query.
- Handled queries are absent from transcript replay and cannot generate future
  child input.
- The runtime must track enough terminal state to answer each supported query
  at its position in the byte stream and update that state on PTY resize.

## Non-Goals

Construct does not emulate every optional terminal capability query. Queries
without a runtime-owned answer continue to pass through unchanged unless a
later decision explicitly adopts them.
