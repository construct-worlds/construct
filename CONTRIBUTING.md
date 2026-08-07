# Contributing to construct

Thanks for hacking on construct! This page covers the day-to-day dev loop.
Repo conventions for agents and maintainers (worktrees, specs, recordings,
release process) live in [AGENTS.md](AGENTS.md).

## Build

```sh
git clone https://github.com/construct-worlds/construct.git
cd construct
cargo build --workspace
```

Everything ships in one binary: `target/debug/construct` is the TUI, the
control CLI, the daemon, and every harness adapter. Building the workspace
rebuilds all of them at once, so whatever you run next is entirely your
branch's code.

## The dev loop

The trick that makes iteration painless: point your freshly built binary at a
**scratch home** so it spins up its own private daemon and never touches your
daily construct instance.

```sh
cargo build
CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct
```

That's it. `CONSTRUCT_HOME` relocates all four path layers (`config/`,
`state/`, `data/`, `run/` — including the IPC socket at
`run/construct.sock`), so the TUI finds no daemon there and auto-starts one
from the same debug binary. You get a fully isolated instance running your
changes — sessions, config, and web UI all separate from your real one. The
two coexist fine; the web UI auto-picks a free port if the default is taken.

Iterate like this:

```sh
# edit code…
cargo build
CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct daemon restart
CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct
```

`daemon restart` swaps in the new build in place; sessions are preserved and
resume after the restart.

Notes:

- **Keep the scratch path short.** The Unix socket lives under it, and socket
  paths have a small OS length limit (~104 bytes on macOS). `/tmp/…` is safe;
  a deep worktree path is not.
- The control CLI works against the same instance — just prefix the env var:

  ```sh
  CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct list
  CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct new --no-tui shell
  CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct daemon paths
  CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct daemon stop
  ```

  (`export CONSTRUCT_HOME=/tmp/construct-dev` in a dedicated shell saves the
  typing.)
- When you're done, `daemon stop` and `rm -rf /tmp/construct-dev` — nothing
  else to clean up.
- Finer-grained overrides exist (`CONSTRUCT_CONFIG_DIR`, `CONSTRUCT_STATE_DIR`,
  `CONSTRUCT_DATA_DIR`, `CONSTRUCT_RUNTIME_DIR`); see
  [docs/configuration.md](docs/configuration.md). `CONSTRUCT_HOME` is the
  one-knob version.

### Working from a branch worktree

This repo's convention is a fresh branch per change, materialized as a git
worktree (see [AGENTS.md](AGENTS.md)). The loop is identical — build inside
the worktree and run *that* tree's binary:

```sh
git worktree add .claude/worktrees/my-change -b my-change origin/main
cd .claude/worktrees/my-change
# edit…
cargo build
CONSTRUCT_HOME=/tmp/construct-dev ./target/debug/construct
```

Each worktree has its own `target/`, so binaries never mix between branches.

### Iterating on the web UI

`crates/daemon/assets/` is embedded into the daemon at compile time, so asset
edits normally need a rebuild + restart. Debug builds can skip that: start the
daemon with `CONSTRUCT_ASSETS_DIR=<worktree>/crates/daemon/assets` and it
serves the files from disk with live reload — edit, save, and the browser
refreshes itself. Details in [AGENTS.md](AGENTS.md#hot-reloading-the-web-ui-dev-only).

## Tests

```sh
cargo test --workspace
```

Run it unfiltered before opening a PR — keyword-filtered runs miss regressions
in shared code. Notes:

- The e2e suite spawns real daemons from `target/debug/construct`, so
  `cargo build` first — `cargo test` alone does not rebuild the binary the
  tests exec, and web tests would exercise stale embedded assets.
- Browser-based web tests need Chrome/Chromium; they skip (not fail) when it
  isn't installed.

## Pull requests

- Branch off latest `main`; changes land only via PR — never push to `main`
  directly.
- Make sure `cargo test --workspace` is green locally; CI runs the same.
- For user-visible TUI/web changes, a short recording or screenshot on the PR
  helps review a lot — [AGENTS.md](AGENTS.md#recording-the-tui) has a recipe.
- Durable design decisions belong in `specs/` (format in
  [AGENTS.md](AGENTS.md#design-specs)).
