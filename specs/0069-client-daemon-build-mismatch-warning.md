# 0069-client-daemon-build-mismatch-warning

Status: accepted
Date: 2026-07-06
Area: protocol
Scope: Desktop clients must surface stale or unknown daemon builds.

## Decision

Desktop clients compare their own build id with the daemon build id reported by IPC and surface a persistent warning whenever the daemon build id differs or is unknown. A missing daemon build id counts as a mismatch by design.

## Reason

Construct is a single binary, but a daemon process can survive while the user launches a newly built client. Without an explicit warning, client and daemon skew looks like arbitrary runtime misbehavior.

## Consequences

Clients must refresh the comparison when they first connect and every time they reconnect. The daemon build field must remain optional so newer daemons stay compatible with older clients, while newer clients can detect older daemons by the missing field.

## Non-Goals

The web client does not need this warning because its assets are served by the daemon process itself, so it cannot run independently from the daemon build serving it.
