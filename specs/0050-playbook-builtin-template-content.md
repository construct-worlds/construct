# 0050-playbook-builtin-template-content

Status: accepted
Date: 2026-06-28
Area: ux
Scope: What the built-in playbook templates contain and the constraints their Markdown must respect.

## Decision

Built-in playbook templates are scaffolds that both structure a workflow and teach the playbook's capabilities. Each built-in template includes a short, human-facing orientation that explains how running the playbook dispatches work to the owning session and its subagents, and demonstrates smart clips. Sectioned templates give each section a one-line description of what belongs in it.

Template Markdown may only contain smart clips that resolve, and must not contain illustrative or placeholder clips that would render as dangling chips. In practice that means harness clips (which always resolve to a harness) are fine to embed as live examples, while a concrete session reference cannot be baked into a static template because the session does not exist yet. Session embeds and fenced `:::clip` blocks are therefore described in prose ("type @ to embed a live session") rather than shown as literal syntax.

The built-in set is Blank (empty), Tasks (a Todo / Progress / Done board), Investigation (Question / Context / Plan / Findings / Done), and Goal (Goal / Context / Requirements / Verification / Done). Goal is an executable work document: running it tells the owning agent to perform the work, verify the result, and record the outcome.

## Reason

The empty-state placeholder surfaces these templates as one-click starting points, so they are many users' first contact with the playbook. A bare set of headings does not convey that the playbook is an execution surface or that smart clips exist. A small amount of in-document guidance turns each template into onboarding without a separate tutorial. Because playbook execution feeds the document prose to the owning agent, the guidance also orients the agent, while the canonical smart-clip syntax is still injected by the run-context tool rather than relied upon from the template.

The clip constraint exists because ordinary prose and inline code are scanned for clip syntax. Triple-backtick fenced code is a raw, source-preserving region: clip syntax there stays literal and non-interactive. A literal example clip outside such a fence with a non-existent target would render as a broken chip in a brand-new playbook. Restricting active template clips to resolvable targets keeps a freshly applied template clean.

## Consequences

- Editing a built-in template, or authoring a user template, must keep every active embedded clip resolvable. Non-resolvable syntax may be shown literally inside triple-backtick fenced code; outside a fence, describe it in prose instead of embedding it.
- Template guidance should stay short and clearly read as orientation, so it does not read as a task when the playbook is run.
- Renaming or adding a built-in template changes its stable `id`; the empty-state placeholder and any id-based references must be updated together. Template selection copies Markdown into the playbook and is not live-linked, so changing a template does not alter playbookes already created from it.
- Built-in templates should only use Markdown constructs the playbook renderer styles (headings, list items, inline and fenced code, smart clips, and `:::clip` blocks); emphasis renders as literal characters and should be avoided in template bodies.

## Non-Goals

This spec does not define the template-selection UI, user-template discovery, or the smart-clip syntax itself (see the playbook orchestration spec).
