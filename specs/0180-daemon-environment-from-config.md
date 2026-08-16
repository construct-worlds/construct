# 0180-daemon-environment-from-config

Status: accepted
Date: 2026-08-02
Area: convention
Scope: Declaring in config.toml the environment the daemon resolves credentials from and passes to the processes it spawns.

## Decision

Config may declare a table of environment variables that the daemon layers
**underneath its real environment**. A variable that is genuinely set to a
non-empty value always wins; the declared table only fills gaps.

The layered environment applies to two things, and they must stay in step:

- **Every credential the daemon itself resolves** — built-in route targets,
  declared endpoint profiles (whether they name a variable or fall back to a
  provider's default ones), and every surface that reports whether a
  credential is present. A key declared in config must never produce a
  working route while the same machine's status output calls it missing.
- **The base environment of every process the daemon spawns** — session
  adapters and the short-lived helpers it runs for ancillary generation.
  Declaring a credential must reach a harness exactly as exporting it would.

Two classes stay environment-only, because config cannot reach them: the
variables that locate the config file itself, and the daemon's own runtime
knobs, which are not credentials and have config surfaces of their own where
they need one.

The declared values are **not** copied into any per-session state that is
persisted. The floor is re-read from config on each spawn, so rotating a
value takes effect on the next one.

## Reason

A daemon's environment is fixed when its process starts, and an in-place
restart carries that same environment across — so a credential exported after
the daemon started stays invisible to it indefinitely, through any number of
restarts. The failure is silent and inverted from the user's mental model:
the key is plainly in their shell, every tool they run by hand can see it, and
the one long-lived process that needs it cannot.

Config is the surface that does not have this problem: it is re-read on every
start. Letting it carry environment closes the gap without changing process
lifecycle semantics, and it works identically for a daemon started by hand, by
a client, or by a service manager — where "the shell that launched it" is not
a meaningful thing to point at.

Keeping the real environment on top preserves every existing deployment: a
machine that exports its keys behaves exactly as before, and a user can
still override a declared value for one run.

## Consequences

- Credentials may now live in a config file in plaintext. That is the user's
  choice to make, but the surface must say so where the table is documented,
  and the environment must remain a first-class alternative rather than a
  legacy path.
- Any future reader of a credential variable inside the daemon must go
  through the layered lookup rather than reading the process environment
  directly, or it will disagree with the rest of the daemon on a machine that
  uses the table. The same applies to out-of-daemon diagnostics that claim to
  report what the daemon sees: they must build the layer from the same config
  first.
- Spawned processes inherit declared values, so a declared credential is
  visible to every harness the fleet runs, including ones that merely host a
  shell. This matches what exporting the variable would have done, and is the
  reason the table is documented as an environment, not as a keystore.
- Making the floor apply at spawn rather than at session creation means it is
  not captured in persisted session state; a session created before a value
  was declared picks it up on its next spawn with no migration.

## Non-Goals

- Not a secret manager: no encryption, no indirection to an external store,
  no per-session scoping. It is exactly an environment, declared in a file.
- Not a way to configure the daemon's own startup — anything read before the
  config file is located cannot come from it.
- Does not change how any harness resolves a model or credential internally;
  it only changes what environment that resolution happens in.

## Examples

- A machine exports nothing and declares one provider's key in config. A
  restart makes that provider a route target, its models appear in the native
  catalogs, status output reports the key as present, and sessions on that
  harness can use it — the same state the machine would have reached by
  exporting the key before the daemon started.
- The same machine also exports that key, with a different value, for one
  run. Every surface uses the exported value; the declared one is inert until
  the export goes away.
- The declared value is edited and the daemon restarted in place. The new
  value is in effect everywhere, including for sessions that already existed
  before the edit.
