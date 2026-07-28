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

## Packaged plugins (`construct plugin`)

The manual paths above stay supported, but a repository can declare all of
them at once with a `construct-plugin.toml` manifest at its root (spec
0151) and become installable with one command:

```console
$ construct plugin install owner/repo          # or owner/repo/subdir
$ construct plugin link ~/src/my-plugin        # local development, no clone
$ construct plugin list
$ construct plugin disable <id> / enable <id>
$ construct plugin uninstall <id>
```

`install` clones the repo under the data directory's `plugins/` root, shows
a capability summary, asks for consent (`--yes` in scripts, `--ref` to pin
a tag/commit), runs the manifest's build steps, and registers the plugin.
Registry changes apply on the next `construct daemon restart` — sessions
are preserved, so applying is cheap.

### Manifest

```toml
[plugin]
id = "diff-review"                  # namespaces everything the plugin adds
name = "Diff Review"
version = "0.3.0"
min_construct_version = "0.16.0"    # older constructs refuse to install
description = "Rich diff review workflow"
platforms = ["macos", "linux"]      # optional; omit for all

[[build]]                           # run once at install, cwd = plugin root
command = ["cargo", "build", "--release"]

[[adapters]]                        # AHP harness; binary relative to root
name = "reviewer"                   # exposed as harness diff-review:reviewer
binary = "target/release/reviewer-adapter"
description = "Headless review harness"

[verbs]                             # program verbs, namespaced diff-review:<name>
dir = "verbs"

[templates]                         # program templates, id diff-review:<stem>
dir = "templates"

[[mcp_servers]]                     # injected into every harness session
name = "review"                     # registered as mcpServers.diff-review-review
command = ["target/release/review-mcp", "serve"]
```

An adapter (or MCP server) named exactly like the plugin id is exposed
without the namespace suffix — a plugin `aider` with adapter `aider` is
simply harness `aider`.

### Runtime contract

Plugin-owned processes receive `CONSTRUCT_PLUGIN_ID`,
`CONSTRUCT_PLUGIN_ROOT`, `CONSTRUCT_PLUGIN_CONFIG_DIR`
(`<config>/plugins/<id>/`), and `CONSTRUCT_PLUGIN_STATE_DIR`
(`<state>/plugins/<id>/`). Durable plugin state belongs in those two
directories. For everything else, a plugin process is an ordinary IPC
client: anything you can do as `construct …` or over the daemon socket, a
plugin can do too.

### Trust

There is no sandbox: install/link runs the plugin's build and runtime code
as your user, with your environment, and with full daemon access — exactly
like any binary you install. construct's guarantees are narrower and
deliberate: nothing in a project tree can register a plugin (plugins are
user-level only), the consent prompt lists everything the manifest
declares before anything runs, and plugin-contributed tools go through the
same approval flow as every other tool. Review the repository, prefer
`--ref`-pinned installs, and treat `--yes` as the trusted-author path.
