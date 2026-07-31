# 0163-service-endpoints-route-channels-to-sessions

Status: proposed
Date: 2026-07-30
Area: architecture
Scope: Construct turns prototyped agent sessions into a reachable service through user-defined endpoints that bind ingress, routing, context, and a sandbox.

## Decision

An agent harness is capable far beyond coding, and a developer can prototype almost any use case interactively in a session. What Construct does not yet offer is the path from that prototype to a *service*: something external systems can call. A **service endpoint** is that path — a named, durable definition with four parts.

**Ingress.** An endpoint is served by a *channel*: the ingress-side mirror of a harness adapter. A channel implementation answers four questions about inbound traffic: how to *verify* a request is authentic (signature, bearer token, HMAC), how to *extract* it into a normalized message (text, session key, attachments, reply context), how to *reply* (post back to the platform, hold the HTTP response, call a callback URL), and what to *acknowledge* immediately (some platforms demand a fast acknowledgment with the real reply delivered asynchronously; a custom sync API may hold the connection for the full turn). The first channel is a generic authenticated HTTP/webhook channel; platform channels (Slack and peers) are translators into that shape and live behind the plugin seam. Platform deliveries carry no ordering or exactly-once guarantee, so deduplication of retried deliveries is part of the channel contract. An endpoint is reachable on the LAN listener without any tunnel, and publicly under the installation's named tunnel hostname on a dedicated service path. Service paths are a distinct route class on the gateway: they bypass the owner's interactive login boundary and delegate authentication to the channel's own verification, while the gateway still enforces coarse per-hostname rate limits.

**Routing.** One mechanism maps requests to harness sessions: a deterministic **session key** derived per message by the channel (a thread id, a caller-supplied key, a constant). The first message with a new key creates a session from the endpoint's profile; later messages with the same key are queued input to that session. A single shared session is a constant key; a session per request is a unique key. Keyed sessions have a lifecycle policy: idle timeout and eviction resolve to *archive*, never delete, so every service conversation leaves an inspectable transcript.

**Context.** The prototype is the deployment artifact. An endpoint's session profile may designate an existing session as its *context session*: new service sessions are derived from it, inheriting its accumulated instructions, corrections, and working state the way forked sessions inherit from their source. The developer refines behavior by refining the context session, not by maintaining a parallel configuration. A profile without a context session states harness, working directory or worktree, and system prompt directly.

**Sandbox.** Service sessions are least-privilege by construction, because their input is third-party and adversarial by assumption. They run under a sandbox profile, without fleet-control tools, inside per-endpoint resource budgets (rate, concurrency, tokens). A tool call that requires approval parks the turn and surfaces the approval to the owner; the session resumes when resolved, or times out with a declined-action reply. It never silently blocks and is never auto-approved outside the sandbox boundary.

Service sessions are real fleet sessions. They appear in clients, grouped or parented under their endpoint, and the owner can open one and participate while the channel keeps flowing through it. Per-endpoint usage — requests by outcome, sessions created and active, queue and turn latency, tokens by harness and model, verification failures, rate-limit rejections — is recorded and queryable.

## Reason

The daemon, stable named tunnels, and session send/create already exist; a service is the missing binding between them. Keeping that binding declarative — an endpoint definition rather than user-written glue — is what makes the capability a product rather than an SDK exercise.

Deterministic session keys are the routing model proven by comparable systems; the modes users ask for (one session, per-message, keyed) are degenerate cases of one mechanism, so a single rule covers all three without policy branching.

The context session exists because the prototype-to-service gap is the problem being solved. If deploying requires re-expressing everything the prototype learned into static configuration, the gap remains; deriving service sessions from the living prototype closes it.

The existing remote-control security model assumes the only remote party is the owner. A service endpoint deliberately invites third-party input, which means prompt injection from the internet can drive an agent on the owner's machine. That flip — not the plumbing — is the core risk, so the least-privilege profile and the scoped route class are part of this decision, not implementation detail.

Sessions staying visible and joinable is the differentiator over hosted bot platforms: the service runs in the owner's fleet, on their choice of harness, observable and interruptible like any other session.

## Consequences

- Adding a channel is a plugin, not a daemon reshape; every channel reduces to the normalized message shape.
- The generic HTTP channel is the compatibility floor: anything that can POST authenticated JSON can integrate before a dedicated channel exists.
- Endpoint definitions, key-to-session maps, and usage counters must survive daemon restart; live sessions resume by the existing resume rules.
- A service session must not reach fleet-control surfaces (creating, addressing, or driving other sessions); granting any such capability is a security change, not a feature.
- Budgets are enforced per endpoint so one noisy endpoint cannot starve the fleet or the wallet.
- The gateway's service route class must never widen to the owner control surface; owner access keeps its interactive login boundary.
- Editing a context session changes future derived sessions, not already-running ones; operators must expect rollout-by-new-session semantics.
- Archive-on-eviction means service history accumulates; retention policy is deferred but the accumulation is accepted.

## Non-Goals

- Multi-tenancy: endpoints serve the installation owner's purposes; callers are not Construct users and get no identity, ACLs, or per-caller isolation beyond the session key.
- Guaranteed delivery or ordering across channel outages; the contract is at-least-once with dedup.
- A hosted marketplace of channels or endpoints.
- Choosing sandbox strength for the user; the profile is theirs to set, with a safe default.

## Examples

- A developer prototypes a support assistant in an interactive session, then creates an endpoint whose context session is that prototype and whose channel is Slack. Each Slack thread becomes a derived session; replies post to the thread; the developer watches — or joins — any of them from the TUI.
- A monitoring system POSTs alerts with a constant session key to the HTTP channel; every alert lands in one long-lived session that accumulates incident context, and turn replies return synchronously in the HTTP response.
- A per-request endpoint fans each call into a fresh sandboxed session parented under the endpoint; finished sessions auto-archive, leaving a browsable tree of every request served.
- A request whose turn attempts a destructive command parks on approval; the owner's phone shows the approval, they decline, and the caller receives a reply saying the action was refused.
