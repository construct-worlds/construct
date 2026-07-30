# 0161-home-scoped-loopback-ports

Status: accepted
Date: 2026-07-27
Area: architecture
Scope: How Construct picks and reclaims the model-router and localhost web-UI loopback ports so multiple daemons (distinct homes) coexist with zero config, while a single home stays stable across restart.

## Decision

Each Construct home owns two small runtime files:

- `runtime_dir/router.port` — last successfully bound model-router port
- `runtime_dir/webui.port` — last successfully bound localhost web-UI port

On bind, when the operator has **not** pinned a port:

1. Prefer the persisted value for this home, else the compiled default
   (`8917` router / `5746` web UI).
2. If that address is busy (`EADDRINUSE`), bind an OS-assigned free port.
3. Write the actually-bound port back to the home's runtime file.

A pin short-circuits the auto path:

- `[router] port = N` (config) pins the router; no fallback, no overwrite
  of the persist file.
- `CONSTRUCT_WEBUI_PORT=N` pins the web UI the same way.
- `port = 0` (router, tests only) asks the OS directly and does not persist.

`construct paths` reads the same files, so the printed `webui:` / `router:`
lines match the live listeners for that home.

## Reason

Harness processes outlive the daemon and keep dialing the proxy port they
were given at spawn. Reclaiming **that home's** port after restart is
mandatory for routing to keep working. A single global fixed port made a
second `CONSTRUCT_HOME` fail to bind with no recovery, which forced
manual config for multi-daemon setups.

The web UI has a weaker durability need (nothing outlives the daemon
depending on it), but it is the same class of fixed loopback port and the
same multi-home collision. Giving it the same reclaim/fallback rule means
a second home gets a browser UI without config, and `construct paths`
stays truthful.

Persisting under `runtime_dir` (not `config.toml`) keeps the file as
reclaim state rather than user configuration, and scopes it automatically
to `CONSTRUCT_HOME` / `CONSTRUCT_RUNTIME_DIR`.

## Consequences

- Two homes on one machine boot with zero config: the first claims the
  defaults, the second auto-picks free ports and remembers them.
- Restarting the same home reclaims its last ports, so live harnesses
  keep reaching the router and bookmarks/`construct paths` keep working.
- An explicit pin is sacred: busy pin → loud bind failure, prior persist
  file left untouched.
- Only the auto-selected path writes the persist file. Pins never clobber
  a previous auto choice.
- Clients that need the live port must read the runtime file or call
  `construct paths` rather than hard-coding 8917/5746. New sessions still
  learn the router port from the env the daemon injects at spawn.

## Non-Goals

- Per-session ports. Attribution stays on the proxy credential; one
  listener per home is enough.
- Moving these coordinates into `config.toml` as the source of truth.
  Config remains the pin surface only.
- Auto-fallback for other listeners (remote-control WS, etc.).

## Examples

- Fresh home A: router binds `8917`, web UI binds `5746`, both written to
  A's runtime dir.
- Fresh home B while A is up: both preferred ports busy → B binds free
  ports (e.g. `54321` / `54322`) and persists them under B's runtime dir.
- Restart A: reads `8917` / `5746` from its files, reclaims them; B's
  live harnesses are unaffected.
- Operator sets `[router] port = 9000` on A while 9000 is taken: bind
  fails loudly; A's `router.port` file is not rewritten.
