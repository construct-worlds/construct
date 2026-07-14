# 0093-remote-control-exposure-is-chosen

Status: accepted
Date: 2026-07-13
Area: ux
Scope: Opening the remote-control dialog must never expose the daemon; exposure beyond the local network is an explicit choice among tunnel providers.

## Decision

Opening the remote-control dialog binds the listener and shows how to reach it, and does nothing else. No tunnel process is started, and nothing is published beyond the local network, until the user picks a provider from the dialog.

The listener binds every interface, not loopback, so the resting state of the dialog is genuinely useful: a phone on the same network can scan the QR and connect with no tunnel at all. Everything the listener serves is gated by HTTP Basic auth with a per-listener password, and failed attempts are throttled daemon-wide so that a short, phone-typeable password can stand as the only credential.

Reaching the daemon from beyond the LAN goes through a tunnel provider. A provider is a child process the daemon spawns in the foreground, holds a PID for, and kills on stop. The provider set sits behind a seam so that adding one is a backend plus an enum variant, not a reshape; today the set is a single provider that reaches the **public internet** with an unguessable, rotating URL.

The dialog lists a provider even when it cannot currently run, with a user-actionable reason, so a disabled button explains itself rather than doing nothing. A provider's URL is only shown once it is actually serving. If a provider cannot publish, the dialog says why and shows no URL — it never falls back to the local address while implying wider reach.

The tunnel and the LAN listener have independent lifetimes. From the tunnel-ready view the user can go **back** — return to the local-network view while leaving the tunnel running, so re-selecting the provider shows the same URL — or **stop** the tunnel, which drops the public URL but leaves the LAN listener, its password, and any LAN-connected clients untouched. Stopping remote control entirely (listener included, credentials rotated) is a separate, explicit action.

## Reason

The dialog used to start a public tunnel the moment it was rendered. Looking at the QR — even to check what the address was, even to dismiss it — put the daemon on the public internet. That is a side effect a user cannot predict from "show me the dialog", and the blast radius is the user's entire machine, since remote control drives every session on it.

Binding only loopback made the no-tunnel state useless: a QR encoding a loopback address points the scanning phone at itself. Making the common case (a phone on the same network) work without any tunnel removes most of the reason to reach for one, which is the best kind of security improvement — the safe path is also the easy one.

Exposure past the LAN is a distinct decision from being reachable on the LAN, and it carries a distinct risk: a public tunnel's security rests entirely on nobody learning the URL. So it is never a side effect — the user asks for it, and can drop it (stop the tunnel) without ending the local-network session they still want.

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
- A user picks the public provider from a coffee shop. The dialog shows "starting…" over the still-valid local QR, then swaps in the public URL once it resolves.
- With the tunnel up, the user goes **back** to the local-network view, then re-selects the provider — and sees the same URL, because the tunnel was never stopped.
- The user presses **stop** in the tunnel-ready view. The public URL is dropped, but the phone connected over the LAN keeps working, because the listener and its password were left in place.
- A user picks a provider whose binary is missing. The dialog explains what to install, and offers a way back. No URL is shown.
