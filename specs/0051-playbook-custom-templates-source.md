# 0051-playbook-custom-templates-source

Status: accepted
Date: 2026-06-28
Area: persistence
Scope: Where playbook templates come from and how they reload.

## Decision

Playbook templates are the built-in set plus any Markdown files in a templates directory. The directory is configurable and templates reload live. A custom template is just a `.md` file — there is no frontmatter and no per-template metadata.

- **Source directory.** Custom templates are read from a directory resolved with this precedence: the `CONSTRUCT_PLAYBOOK_TEMPLATES_DIR` environment variable, then the `[playbook].templates_dir` config option, then the default `<data_dir>/playbook/templates`.
- **A file is a template.** Each `*.md` file in the directory is one template. Its file stem is the template id, and its display name is that stem prettified: `-` and `_` become spaces and each word is title-cased (`code-review.md` → "Code Review"). The file's entire contents are the playbook Markdown, verbatim — including any leading `---`. Custom templates carry no description.
- **Built-ins always present.** The built-in templates (Blank, Tasks, Investigation, Goal) are always offered regardless of the directory. Built-ins carry a short description (surfaced by `construct playbook templates`); custom templates do not.
- **Legacy migration is default-location only.** The one-time `canvas/templates` → `playbook/templates` rename runs only when no directory override is set. When a user points the daemon at an explicit directory, the daemon treats it as user-owned and never moves files into it.
- **Live reload.** The daemon re-reads the directory on every template-list request. The client caches the list but re-fetches it in the background whenever the playbook pane opens, so adding or editing a template file takes effect on the next open without a daemon restart.

## Reason

User templates were already supported from a hardcoded location, but minibuffers could not relocate that directory (e.g. to a dotfiles repo or a shared/synced path) and edits required a daemon restart to take effect. Making the directory configurable and reloading on open turn templates into a lightweight, minibuffer-owned authoring surface without a separate management UI. Treating each file as "filename is the name, contents are the playbook" keeps authoring as simple as dropping a Markdown file in a folder — no frontmatter syntax to learn or get wrong.

## Consequences

- The resolved directory is fixed at daemon start (config + env are read once). Changing the location requires a daemon restart; changing the *contents* of the resolved directory does not.
- The resolver order (env > config > default) must stay stable so an env override always wins over config — useful for one-off or per-invocation redirection.
- With an override set, the legacy `canvas/templates` content under the default data dir is intentionally not migrated or read. Minibuffers relocating templates are responsible for moving existing files themselves.
- The display name is derived purely from the filename, so the way to rename a custom template is to rename its file. A leading `---` in the file is part of the playbook, not metadata, so it appears when the template is loaded.
- Live reload is best-effort and non-blocking: a failed background fetch leaves the cached list in place rather than clearing the placeholder.

## Non-Goals

This spec does not define a template management/editing UI, template validation beyond "resolvable clips" (see [0050](0050-playbook-builtin-template-content.md)), per-template metadata (descriptions, reference links), or watching the directory for changes outside the open-the-pane refresh.

## Examples

- `CONSTRUCT_PLAYBOOK_TEMPLATES_DIR=/srv/templates construct daemon run` reads custom templates from `/srv/templates`; built-ins still appear.
- A `config.toml` with `[playbook]\ntemplates_dir = "~/dotfiles/playbook-templates"` relocates the directory; an env var of the same name overrides it.
- A user drops `code-review.md` into the directory; reopening the playbook shows a "Code Review" button whose playbook is the file's contents, no restart needed.
