# 0151-plugin-system

Status: accepted
Date: 2026-07-27
Area: architecture
Scope: how third-party plugins are packaged, installed, trusted, and merged into construct's extension seams.

## Decision

A plugin is a directory containing a `construct-plugin.toml` manifest. The
manifest declares metadata (`id`, `name`, `version`, `min_construct_version`,
optional `description` and `platforms`) and contributions; every contribution
lands on an extension seam that already exists for out-of-process or
data-file extensions:

- **Adapters** — AHP harnesses, merged into the same adapter registry that
  config-file community adapters use. Exposed as harness `<plugin-id>` when
  the adapter name equals the plugin id, `<plugin-id>:<name>` otherwise.
- **Program verbs and templates** — directories of markdown definition
  files, merged into the existing verb/template registries under forced
  `<plugin-id>:` namespacing. A plugin can never override a built-in or a
  user's own verb/template by name; only the user's own directories have
  that power.
- **MCP tool servers** — injected into harness sessions alongside the
  construct MCP server, carried to adapters through a daemon-set environment
  variable holding resolved commands. The `construct` server name is
  reserved and cannot be displaced. The existing MCP kill switch disables
  plugin servers too. Plugin tools flow through the same approval pipeline
  as every other tool.
- **Actions** — user-invocable commands surfaced through a daemon IPC
  listing (`plugin.list_actions`). Clients populate their palette/slash
  surfaces from that list at runtime and invoke via `plugin.run_action`;
  plugin actions are data, never compiled-in commands. Invocation tokens
  follow the adapter naming rule (`<plugin-id>:<action>`, bare plugin id
  when equal). Running an action spawns its command with plugin identity
  env plus the invoking session's id for `context = "session"` actions,
  fire-and-forget; the process talks back over ordinary IPC.
- **Event hooks** — the daemon spawns a hook's command when a handled
  session event matches one of its declared matchers (an event type tag,
  or `status:<state>`). Hooks are observational and fire-and-forget, may
  declare a per-hook debounce, receive the event as JSON in the
  environment, and must never block or fail the event funnel — a daemon
  with a broken hook still processes every event.

Installed plugins are recorded in a registry file under the data
directory's `plugins/` root; GitHub installs keep a managed checkout there,
`link` registrations point at a developer's working copy. The daemon reads
the registry once at startup; registry mutations apply on the next daemon
restart (which preserves sessions). Broken, disabled, or incompatible
plugins are skipped with a warning — one bad plugin must never prevent the
daemon from starting.

Lifecycle is CLI-owned: `construct plugin install <owner>/<repo>[/subdir]`
(clone → validate → consent → move into place → build steps → register),
`link <dir>`, `list`, `enable`/`disable` (global, per-user), `uninstall`.
Install and link refuse a plugin whose `min_construct_version` is newer
than the running binary, and show a capability summary (adapters, build
commands, injected MCP servers, verb/template dirs) requiring explicit
consent — `--yes` in scripts.

Trust model: plugin code runs as the user with the user's environment, like
any binary they install. Construct's obligations are (1) explicit consent at
install/link showing everything the manifest declares, (2) plugins are
user-level only — nothing in a project tree can register one, and
(3) plugin-contributed tools and sessions pass through the existing
approval/risk model unchanged.

Plugin-owned processes receive identity env (`CONSTRUCT_PLUGIN_ID`,
`CONSTRUCT_PLUGIN_ROOT`, `CONSTRUCT_PLUGIN_CONFIG_DIR`,
`CONSTRUCT_PLUGIN_STATE_DIR`), injected by the daemon after its own
`CONSTRUCT_*` scrub so nested sessions do not inherit another plugin's
identity. Discovery convention: public repos tagged with the
`construct-plugin` GitHub topic.

## Reason

construct already had three extension rings — out-of-process AHP adapters
registered via config, data-file registries (verbs, templates, widgets),
and IPC clients — but no manifest, no install story, no namespacing, and no
consent surface. Community extensions required hand-editing config files
and copying directories. A manifest that is a declarative wrapper over the
existing seams gets a packaging/distribution story without inventing a new
runtime: no dynamic linking (the single-binary decision stands), no plugin
UI toolkit (clients own rendering), and nothing a plugin does that an
ordinary IPC client could not already do — the manifest only automates
registration.

## Consequences

- Future extension points should be added as manifest sections over
  existing seams, not as new runtime mechanisms. Later phases add
  user-invocable actions and event hooks the same way.
- The namespacing rules (`<plugin-id>` / `<plugin-id>:<name>` harnesses,
  forced verb/template prefixes, reserved `construct` MCP name) are load
  order-independent and must be preserved; anything that lets a plugin
  claim an unnamespaced name reopens the override-a-built-in hole.
- User config wins field-by-field over plugin adapter declarations, the
  same layering built-in adapters use — operators can pin or patch a plugin
  adapter without forking it.
- Registry changes require a daemon restart to apply. Acceptable because
  restart preserves sessions; if that ever stops being true, plugins need a
  live-reload path instead.
- The daemon must keep starting when any plugin is broken; plugin loading
  is diagnostics-only, never fatal.

- Plugin actions carry no default keybindings, so the cross-client chord
  parity obligation is not affected. Binding keys (and MIDI mappings) to
  plugin actions, and surfacing actions in the web client, are follow-ups
  that must consume the same runtime listing rather than a compiled table.

## Non-Goals

- No in-process or dynamically linked plugins; the process boundary is the
  plugin boundary.
- No plugin-drawn UI. Plugins contribute semantics (sessions, widgets,
  markdown, commands); clients own rendering and interaction.
- No per-project plugin auto-activation, and no sandboxing promise in this
  phase — the consent prompt is explicit that plugin code runs as the user.
- No separate update channel: update = reinstall.

## Examples

- A repo with one adapter named like its plugin id `aider` installs via
  `construct plugin install someone/construct-aider` and appears as harness
  `aider` in the picker, probed like any community adapter.
- A plugin `diff-review` declaring verbs directory `verbs/` with a file
  `summarize.md` contributes the verb `diff-review:summarize`; a file named
  `simplify.md` contributes `diff-review:simplify` and leaves the built-in
  `simplify` untouched.
- A plugin declaring an MCP server `review` appears to harnesses as MCP
  server `diff-review-review`, and its tool calls require the same
  approvals as any other tool.
