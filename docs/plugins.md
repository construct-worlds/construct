# Extending construct (plugins)

construct is built around process and data seams that third parties can plug
into today, without patching the binary. This page documents the extension
points that already work, the conventions to follow when publishing an
extension, and where the packaged plugin system is headed.

## What you can extend today

### Community adapters (new harnesses)

Any binary that speaks the AHP protocol (line-delimited JSON-RPC over stdio /
the reconnect socket — see [Adding a harness](adding-a-harness.md)) can be
registered as a harness with one config block, no code changes to construct:

```toml
# ~/.config/construct/config.toml
[adapters.aider]
binary      = "/path/to/construct-adapter-aider"
description = "Aider (community adapter)"
```

The daemon resolves `binary` as an absolute path, then next to its own
executable, then on `PATH`. The harness shows up in the picker, `construct
harnesses`, and `construct new aider` like any built-in; availability is
"binary resolves" (spec 0068's community-adapter tier). Restart the daemon
(`construct daemon restart` — sessions are preserved) after editing the
config.

### Program verbs

Drop a markdown file in `~/.config/construct/verbs/` to add a typed
refinement action to every Program selection menu. One file per verb,
frontmatter for policy, body is the prompt; a file whose `name` matches a
built-in replaces it. No restart needed — verbs reload on every menu open.
See [Program selection verbs](program-verbs.md).

### Program templates

Drop `<name>.md` files into the templates directory (default
`~/.local/share/construct/program/templates`, relocatable via
`[program].templates_dir` or `CONSTRUCT_PROGRAM_TEMPLATES_DIR`). The filename
is the template id; templates reload on every list. See
[Program](program.md).

### IPC clients (automation, remote UIs, observers)

Everything the TUI and web UI do goes through the daemon's Unix-socket
JSON-RPC API — create/drive sessions, subscribe to fleet events, read
transcripts and diffs, edit programs, inject events. An external tool that
connects to `$XDG_RUNTIME_DIR/construct/construct.sock` (or shells out to the
`construct` CLI) is a first-class participant: notification bridges,
dashboards, schedulers, and navigation tools all fit here with zero
registration. `subscribe.events` streams every session's lifecycle events;
`session.emit_event` injects them.

### Session widgets

Agents (and tools acting on their behalf) can render structured UI by writing
markdown files into `$CONSTRUCT_SESSION_WIDGETS_DIR`; clients own the
rendering and interaction. See [Generative widgets](generative-widgets.md).

## Publishing convention: the `construct-plugin` topic

If you publish a construct extension on GitHub — a community adapter, a verb
pack, a template pack, or an IPC-client tool — tag the repository with the
**`construct-plugin`** topic and give the README a one-command install path
(today typically: a build step plus the `config.toml` block or the copy into
the verbs/templates directory).

The topic is the discovery channel: it lets users find extensions with a
GitHub topic search now, and it is the namespace a future marketplace index
will crawl. Repositories tagged today will be picked up automatically once
that index exists.

## Where this is headed

A packaged plugin system is planned on top of these seams — a
`construct-plugin.toml` manifest describing what a repository contributes
(adapters, verbs, templates, MCP tool servers; later user-invocable actions
and event hooks), and a `construct plugin install <owner>/<repo>` /
`construct plugin link <dir>` lifecycle that automates the manual steps
above. The manual paths on this page remain supported; the manifest is a
declarative wrapper over the same registration points, so an extension
published today as "config block + verbs dir" translates directly into a
manifest later.
