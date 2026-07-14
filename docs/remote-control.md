# Remote control

`/remote-control` exposes the running daemon through a browser-accessible web
client so you can check and steer the same fleet from another device. The TUI
shows a modal with a QR code, the addresses this machine can be reached at, a
username, and a password; the local modeline shows a `remote` badge while remote
clients are attached.

Opening the dialog does **not** expose the daemon to the internet. It binds the
listener and shows how to reach it on the local network — a phone on the same
Wi-Fi can scan the QR and connect right away, no tunnel involved. Reaching the
daemon from *outside* the local network is a separate, explicit choice you make
from the buttons in the dialog.

| Command / setting | Purpose |
|---|---|
| `/remote-control` | Open the dialog: bind the listener, show the LAN address + QR, and offer tunnel providers. No tunnel is started until you pick one. |
| `/remote-control <password>` | Same, with a user-chosen Basic-auth password. |
| `/remote-control tailscale` | Skip the dialog and start a Tailscale tunnel directly. |
| `/remote-control cloudflare` | Skip the dialog and start a Cloudflare tunnel directly. |
| `/remote-control stop` | Stop the listener/tunnel and rotate credentials for the next start. |
| `/remote-control debug` | Alias for `/remote-control` — kept because the plain dialog is now the local-only resting state. |
| `CONSTRUCT_REMOTE_WS_PORT=<port>` | Start the remote WebSocket listener on daemon boot for scripted/headless use. |
| `CONSTRUCT_REMOTE_PROVIDER=<cloudflare\|tailscale\|none>` | Tunnel provider for the boot-time listener above. Defaults to `cloudflare`. |
| `CONSTRUCT_WEBUI_PORT=<port>` | Override the always-on localhost web UI port. Defaults to `5746`. |

## Tunnel providers

Reaching the daemon from beyond the local network means picking a provider, and
the two on offer make different trade-offs:

- **Cloudflare** runs a `cloudflared` quick tunnel. The URL is reachable from
  anywhere and is unguessable, but it rotates on every run and its only
  protection is that nobody learns it. Needs no account.
- **Tailscale** runs `tailscale serve`. The URL is stable and reachable **only
  from your own tailnet**, so access is gated by your tailnet's rules rather than
  by URL secrecy. Needs Tailscale installed, logged in, and HTTPS certificates
  enabled for the tailnet.

The dialog lists both even when one can't run, greyed out with the reason (not
installed, not logged in, and so on), so you can see what's missing and fix it.

## Authentication and binding

The remote listener binds every interface and gates every request with HTTP
Basic auth. The username is `remote`; the password is generated per session (a
memorable `word.word.NNNN` string) unless you supply your own. Failed attempts
are throttled daemon-wide, so the short password is safe to be the only
credential — but that throttle is load-bearing, and the listener is reachable
from the whole local network, so treat the auth path as security-sensitive.

The daemon also starts a separate localhost-only browser UI at
`http://127.0.0.1:5746/`. That UI is bound to loopback and has **no** auth at
all — it must never be exposed off-machine, which is exactly why it stays on
loopback while the remote-control listener does not.

Tunnel + listener state is persisted under the runtime directory so a daemon
restart preserves the active URL, password, and provider when possible; a
restart never silently rotates the URL or switches how the machine is exposed.
