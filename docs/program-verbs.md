# Program selection verbs

Alongside Run, a Program selection offers **verbs**: typed refinement actions
that improve the selected Markdown itself rather than executing it. Run turns
a selection into orchestration; a verb turns it into better orchestration
state — challenging an assumption, simplifying a plan, crystallizing loose
prose into a spec, or interviewing you to resolve ambiguity.

## Using a verb

Select text in the Program to open the selection context menu. It offers
`▶ Run`, an optional free-text instruction field, and one button per
available verb.

- `Tab` moves keyboard focus into the menu. `Up`/`Down` (and `C-p`/`C-n`)
  cycle the highlighted row through Comment, Run, then each verb in order;
  `Enter` activates whichever row is highlighted, and a plain click activates
  whatever it lands on.
- While a row is highlighted, its **description** shows at the bottom of the
  menu, wrapped across as many lines as it needs — never truncated. A verb's
  description is always led by its declared effect, `Annotate:` or
  `Rewrite:`, so you know exactly what it's about to do to the document
  before you run it.
- The free-text instruction composes with a verb: type guidance, then pick a
  verb, and the instruction is appended to that verb's own prompt. It never
  replaces the verb.

## Built-in verbs

| Verb | Effect | Interaction | What it does |
|---|---|---|---|
| **Challenge assumptions** | Annotate | Single-shot | Surfaces the 2–3 most load-bearing assumptions in the selection and what breaks if each is wrong. |
| **Simplify** | Rewrite | Single-shot | Reduces the selection to the minimum that preserves its core intent. |
| **Crystallize spec** | Rewrite | Single-shot | Restructures loose prose into a goal, constraints, and acceptance criteria. |
| **Interview** | Annotate | Interactive | Asks questions to resolve ambiguity, then hands back a decision digest. |

## How a verb runs

A verb spawns a dedicated session forked from the Program's owning session —
when the owning session's harness supports native fork-resume, the verb
inherits its actual conversation, not just the document text. The verb
session can read the Program but has **no tool that can edit it**; it
returns a structured result instead, and the daemon applies that result on
its behalf:

- If the selection hasn't changed since the verb started, the result merges
  **mechanically** — no further model round-trip.
- If it has (you edited the selection while the verb was running), the
  daemon asks the owning session to reconcile the result into the document
  as it now stands.

The selection is annotated with the verb session's `@{session:…}` clip the
moment it starts, so you always have a provenance link to what ran. Once its
result merges, the verb session is soft-archived — its transcript and clip
stay resolvable, but it drops out of the active session list.

## Interactive verbs and the pinned terminal

An `interactive` verb (Interview, and any custom verb you mark as one) needs
your input mid-run. Single-clicking its `@{session:…}` clip **pins** its
preview card open as a live, keyboard-focused terminal, right next to the
selection:

- Type to answer — keystrokes go to the verb session, not the Program
  buffer. `Esc` also forwards to the session (useful for interrupting a
  turn or dismissing its menus) rather than unpinning.
- The global `C-x` chord prefix still drives the TUI keymap while pinned
  (`C-x o`, `C-x z`, …); `C-x C-x` sends one literal `C-x` byte through to
  the session, same as a captured terminal.
- **Unpin** by clicking the same clip again, or by clicking anywhere else —
  in the Program body, its chrome, or outside the modal.
- Double-clicking the clip navigates to the full session view instead of
  pinning.

If the pinned session isn't visible anywhere else, the card takes **size
ownership**: it resizes the session's terminal to the card's own dimensions,
so you see full-fidelity output instead of a crop. Drag the card's
right/bottom border to resize it, or its top border to move it anywhere in
the Program. When the session *is* visible elsewhere (a split pane, say),
the card stays a crop of that view instead, pannable with the mouse wheel
(or `Shift`+arrows, the guaranteed keyboard fallback).

## Authoring your own verb

A verb is one Markdown file with frontmatter:

```markdown
---
label: Threat model
effect: annotate
interaction: single-shot
order: 50
comment: Internal only — not for external distribution.
---

You are a security reviewer. The selected region of a planning document is
your entire jurisdiction. List realistic abuse cases against it and note
which, if any, are already mitigated elsewhere in the plan.
```

| Field | Required | Meaning |
|---|---|---|
| `name` | No | Stable id, referenced by `construct_program_verb_execute`. Defaults to the file's own name (`threat-model.md` → `threat-model`) — most verbs need no `name:` line at all. |
| `label` | Yes | Human label shown on the menu button. |
| `effect` | Yes | `annotate` (insert new content below the selection) or `rewrite` (replace it). |
| `interaction` | Yes | `single-shot` (runs unattended) or `interactive` (holds a dialogue with you first — see above). |
| `description` | No | Shown when the row is highlighted, prefixed automatically with its effect. Write it as a lowercase continuation clause: `surface hidden assumptions…`, not `Surface hidden assumptions…` or `Annotate hidden assumptions…` — the effect word is supplied for you. |
| `order` | No | Menu sort hint (built-ins use 10/20/30/40). |
| `comment` | No | Free-text notes for people reading the file — provenance, license attribution, anything. Never sent to the model. |

The body is the verb's purpose prompt. It may place its own context with
template placeholders — a placeholder you use suppresses the matching
default framing, so you never get it twice:

| Placeholder | Value |
|---|---|
| `{{ program.content }}` | The full Program document (bounded in size; truncated copies point back at the live read tool). |
| `{{ program.selected_text }}` | The selection itself. |
| `{{ program.additional_instruction }}` | The free-text instruction typed alongside the verb, empty if none. |

Drop the file into `verbs/` under construct's config directory (default
`~/.config/construct/verbs`, or `$CONSTRUCT_CONFIG_DIR/verbs` — see
[Configuration](configuration.md)). No restart or code change needed: every
client renders its menu from the daemon's advertised verb list. A user
file's `name` — explicit or filename-derived — replaces a built-in of the
same name; malformed files are skipped with a log warning rather than
breaking the rest of the list.

## MCP

Agents (and the orchestrator) reach verbs through `construct_program_list_verbs`
(returns each verb's `name`, `label`, `effect`, and `interaction`) and
`construct_program_verb_execute` (`verb: <name>`, `selection: <markdown>`).

## Design references

Normative design records live in `specs/` —
`0089-program-selection-verbs.md` (verb lifecycle, merge semantics, the
markdown-file registry) and `0090-program-clip-pin-interactive-terminal.md`
(the pinned inline terminal).
