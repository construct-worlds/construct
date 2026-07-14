# 0093-remote-control-exposure-is-chosen

Status: accepted
Date: 2026-07-13
Area: ux
Scope: Opening the remote-control dialog must never expose the daemon; exposure beyond the local network is an explicit choice among tunnel providers.

## Decision

Opening the remote-control dialog binds the listener and shows how to reach it, and does nothing else. No tunnel process is started, and nothing is published beyond the local network, until the user picks a provider from the dialog.

The listener binds every interface, not loopback, so the resting state of the dialog is genuinely useful: a phone on the same network can scan the QR and connect with no tunnel at all. Everything the listener serves is gated by HTTP Basic auth with a per-listener password, and failed attempts are throttled daemon-wide so that a short, phone-typeable password can stand as the only credential.

Tunnel providers are a set, not a single vendor. Each provider is a child process the daemon spawns in the foreground, holds a PID for, and kills on stop. Two exist today:

- One that reaches the **public internet** with an unguessable, rotating URL.
- One that reaches **only the user's own private network overlay**, with a stable URL, where access is gated by that overlay's access rules rather than by URL secrecy.

The dialog lists every provider the daemon knows about, including ones that cannot currently run, each with a user-actionable reason. A provider's URL is only shown once it is actually serving. If a provider cannot publish, the dialog says why and shows no URL — it never falls back to the local address while implying wider reach.

## Reason

The dialog used to start a public tunnel the moment it was rendered. Looking at the QR — even to check what the address was, even to dismiss it — put the daemon on the public internet. That is a side effect a user cannot predict from "show me the dialog", and the blast radius is the user's entire machine, since remote control drives every session on it.

Binding only loopback made the no-tunnel state useless: a QR encoding a loopback address points the scanning phone at itself. Making the common case (a phone on the same network) work without any tunnel removes most of the reason to reach for one, which is the best kind of security improvement — the safe path is also the easy one.

The two providers are not redundant. A public tunnel needs no account and reaches anywhere, but its security rests entirely on nobody learning the URL. A private-overlay tunnel is reachable only from devices the user has already enrolled, so a leaked URL is not a breach. Neither dominates, so the choice belongs to the user, made with the trade-off in front of them.

## Consequences

- **Opening the dialog must stay free of side effects beyond binding.** Any future work that starts a tunnel, registers a name, or contacts a third party on dialog-open is a regression, whatever the convenience.
- **The listener's auth is load-bearing.** It binds every interface, so the password gate and its failure throttle are the only things between the local network and full control of the machine. Weakening either — a lockout-free retry path, an unauthenticated route, a shorter password — must be treated as a security change, not a UX change.
- **The separate always-on local web UI has no auth and must remain loopback-only.** It must not inherit this listener's reasoning.
- **Providers must publish only when serving.** A URL shown before the tunnel is live sends the user to a dead address, which reads as "the feature is broken" rather than "wait a moment".
- **Foreground children, not background registration.** A provider that registers itself with a persistent local service and exits would outlive a crashed daemon, leaving the machine exposed with nothing left to withdraw it. Providers must die with the process the daemon holds a PID for.
- **A restart resumes through the same provider it was using.** Restarting must not rotate the user's URL or silently switch how the machine is exposed.
- **Adding a provider is a matter of a preflight and a spawn.** Preflight failures are user-facing prose, not log lines: they are what the dialog paints under a disabled button.

## Non-Goals

- Choosing a provider *for* the user based on what happens to be installed. The reachability trade-off is theirs to make.
- Guaranteeing the local-network address is reachable. It is reported, not promised; routing (VPNs, client isolation) can defeat it, and a provider is the remedy.
- Authentication beyond a single shared password. Per-user or per-session credentials are a separate decision.

## Examples

- A user types the remote-control command, scans the QR with a phone on the same Wi-Fi, and works from the sofa. No tunnel process ever ran.
- A user opens the dialog, sees the private-overlay provider greyed out with "not logged in — run the login command", runs it, reopens the dialog, and the option is now selectable.
- A user picks the public provider from a coffee shop. The dialog shows "starting…" over the still-valid local QR, then swaps in the public URL once it resolves.
- A user picks a provider whose binary is missing. The dialog explains what to install, and offers a way back to the other options. No URL is shown.
