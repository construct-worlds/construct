# 0051-program-custom-templates-source

Status: accepted
Date: 2026-06-28
Area: persistence
Scope: Where program templates come from, how they reload, and the metadata a template may carry.

## Decision

Program templates are the built-in set plus any Markdown files in a templates directory. The directory is configurable, templates reload live, and each template may carry reference metadata.

- **Source directory.** Custom templates are read from a directory resolved with this precedence: the `CONSTRUCT_PROGRAM_TEMPLATES_DIR` environment variable, then the `[program].templates_dir` config option, then the default `<data_dir>/program/templates`. Each `*.md` file is one template; its file stem is the template id. A leading `---` frontmatter block may set `name`, `description`, and `reference`.
- **Built-ins always present.** The built-in templates (Blank, Tasks, Investigation) are always offered regardless of the directory, and built-ins carry a default `reference` to the program docs.
- **Legacy migration is default-location only.** The one-time `canvas/templates` → `program/templates` rename runs only when no directory override is set. When an operator points the daemon at an explicit directory, the daemon treats it as operator-owned and never moves files into it.
- **Live reload.** The daemon re-reads the directory on every template-list request. The client caches the list but re-fetches it in the background whenever the program pane opens, so adding or editing a template file takes effect on the next open without a daemon restart.
- **Reference metadata.** A template may carry an optional `reference` (a URL / link to related docs). The empty-state placeholder surfaces it as a docs-link footer.

## Reason

User templates were already supported from a hardcoded location, but operators could not relocate that directory (e.g. to a dotfiles repo or a shared/synced path), edits required a daemon restart to take effect, and templates had no way to point at their own documentation. Making the directory configurable, reloading on open, and adding a reference field turn templates into a lightweight, operator-owned authoring surface without a separate management UI.

## Consequences

- The resolved directory is fixed at daemon start (config + env are read once). Changing the location requires a daemon restart; changing the *contents* of the resolved directory does not.
- The resolver order (env > config > default) must stay stable so an env override always wins over config — useful for one-off or per-invocation redirection.
- With an override set, the legacy `canvas/templates` content under the default data dir is intentionally not migrated or read. Operators relocating templates are responsible for moving existing files themselves.
- `reference` is optional and serialized only when present, so it is backward compatible on the wire; older clients ignore it.
- Frontmatter values are parsed as `key: value` on the first colon, so a `reference:` URL keeps everything after `reference:` (including its own colons).
- Live reload is best-effort and non-blocking: a failed background fetch leaves the cached list in place rather than clearing the placeholder.

## Non-Goals

This spec does not define a template management/editing UI, template validation beyond "resolvable clips" (see [0050](0050-program-builtin-template-content.md)), per-template reference rendering beyond a single placeholder footer, or watching the directory for changes outside the open-the-pane refresh.

## Examples

- `CONSTRUCT_PROGRAM_TEMPLATES_DIR=/srv/templates construct daemon run` reads custom templates from `/srv/templates`; built-ins still appear.
- A `config.toml` with `[program]\ntemplates_dir = "~/dotfiles/program-templates"` relocates the directory; an env var of the same name overrides it.
- A user drops `review.md` with `---\nname: Review\nreference: https://wiki/review\n---` into the directory; reopening the program shows a "Review" button and a "Docs:" link, no restart needed.
