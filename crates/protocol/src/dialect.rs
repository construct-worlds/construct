//! The construct-flavored Markdown dialect (spec 0074).
//!
//! One registry describes every construct Markdown extension — display
//! blocks, typed references (smart clips), and action links. The registry is
//! the single source of truth for both agent-facing guidance (agent context,
//! program-run payloads) and client rendering: an extension is defined once
//! and is available on every session Markdown surface unless its entry
//! records an explicit restriction.

use serde::{Deserialize, Serialize};

/// The co-edited, runnable program document.
pub const SURFACE_PROGRAM: &str = "program";
/// Agent-authored session widget panels.
pub const SURFACE_WIDGET: &str = "widget";

const ALL_SURFACES: &[&str] = &[SURFACE_PROGRAM, SURFACE_WIDGET];

/// Extension renders content (timelines, tables).
pub const KIND_DISPLAY: &str = "display";
/// Extension references another object (smart clips).
pub const KIND_REFERENCE: &str = "reference";
/// Extension expresses clickable user intent (action links).
pub const KIND_ACTION: &str = "action";

/// One construct Markdown extension, defined once for every surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownExtensionDescriptor {
    pub name: &'static str,
    /// One of [`KIND_DISPLAY`], [`KIND_REFERENCE`], [`KIND_ACTION`].
    pub kind: &'static str,
    pub syntax: &'static str,
    pub description: &'static str,
    pub use_when: &'static str,
    /// Surfaces that render this extension. Defaults to every surface; a
    /// narrower list is a deliberate restriction and must be justified in
    /// [`Self::restriction`].
    pub surfaces: &'static [&'static str],
    /// Stated reason when `surfaces` is narrower than every surface.
    pub restriction: Option<&'static str>,
}

impl MarkdownExtensionDescriptor {
    pub fn applies_to(&self, surface: &str) -> bool {
        self.surfaces.iter().any(|s| *s == surface)
    }
}

/// Serializable projection of a descriptor for agent-facing context payloads
/// and client bootstraps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MarkdownExtension {
    pub name: String,
    pub kind: String,
    pub syntax: String,
    pub description: String,
    pub use_when: String,
    pub surfaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction: Option<String>,
}

impl From<&MarkdownExtensionDescriptor> for MarkdownExtension {
    fn from(d: &MarkdownExtensionDescriptor) -> Self {
        Self {
            name: d.name.to_string(),
            kind: d.kind.to_string(),
            syntax: d.syntax.to_string(),
            description: d.description.to_string(),
            use_when: d.use_when.to_string(),
            surfaces: d.surfaces.iter().map(|s| s.to_string()).collect(),
            restriction: d.restriction.map(|s| s.to_string()),
        }
    }
}

/// Every construct Markdown extension. Adding an entry here updates agent
/// guidance (agent context and program-run payloads) and the surface gating
/// clients consult; client renderers implement the entry once and reuse it on
/// every listed surface.
pub const CONSTRUCT_MARKDOWN_EXTENSIONS: &[MarkdownExtensionDescriptor] = &[
    MarkdownExtensionDescriptor {
        name: "timeline",
        kind: KIND_DISPLAY,
        syntax: ":::timeline\n- [x] [Run checks](agentd:action/run-checks?key=r) and [Start demo](agentd:action/start-demo?key=d)\n  - [x] Nested done\n    - [ ] Deeper todo\n- [~] Active/current\n- [ ] Todo\n- [!] Blocked\n- Plain milestone\n:::",
        description: "Render top-level bullet/checklist items as a vertical timeline with connector rows between bullet icons. Indented nested lines render below their parent item at arbitrary list depth, and each top-level item keeps bottom padding. Supports [x] done, [~] active/current, [ ] todo, [!] blocked/warning, plain bullet items, and inline agentd action links with optional ?key= shortcuts.",
        use_when: "Use for multi-step task progress, mission plans, status history, and review/check workflows where connected bullets read better than a plain list.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "table",
        kind: KIND_DISPLAY,
        syntax: "| Check | Status |\n| --- | :---: |\n| build | [x] ok |\n| tests | [Run](agentd:action/run-tests?key=t) |",
        description: "Render a GitHub-flavored Markdown table: a header row, then a `| --- | :--: |` delimiter row (colons set column alignment — `:--` left, `:-:` center, `--:` right), then one row per line. Outer pipes are optional. Cells may contain inline agentd action links. Wide tables shrink to fit the surface.",
        use_when: "Use for compact tabular status — checks, metrics, file/owner lists, side-by-side comparisons — where columns read better than prose or a flat list.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "action-link",
        kind: KIND_ACTION,
        syntax: "[Run checks](agentd:action/run-checks?key=r&close=1)",
        description: "An inline Markdown link using the `agentd:action/<action-id>` scheme. Activating it delivers `OBSERVATION: ui.action <action-id>` to the owning session as user intent, still subject to normal approval and safety policy. `?key=<key>` adds a keyboard shortcut (active only when explicit); `close=1` dismisses the surface after activation. Only the user activates action links: running a program never triggers the links it contains.",
        use_when: "Use for compact steering affordances — run, pause, approve, open — in widget or program Markdown.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "session",
        kind: KIND_REFERENCE,
        syntax: "@{session:<session_id> ...}",
        description: "References an existing session; inspect, resume, focus, or summarize that session when relevant. Clients render it as a live chip showing the session's current state.",
        use_when: "Use to tie a line of Markdown to the session doing that work, in a program or a widget.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "harness",
        kind: KIND_REFERENCE,
        syntax: "@{harness:<name> ...}",
        description: "References an agent harness such as codex, claude, or shell; create or resume a suitable subagent when the program calls for delegated work.",
        use_when: "Use to declare which harness should pick up an item of delegated work.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "typed-reference",
        kind: KIND_REFERENCE,
        syntax: "@{<type>:<target> ...}",
        description: "A generic compact typed reference. Preserve unknown types and resolve them only when you have an appropriate tool or context.",
        use_when: "Use for compact references whose type has no dedicated entry; renderers show unknown types inertly.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "clip-block",
        kind: KIND_REFERENCE,
        syntax: ":::clip <type> ... :::",
        description: "A larger typed clip block. Treat the block body as attached structured context for the surrounding instructions.",
        use_when: "Use when a reference needs attributes or body content that does not fit an inline typed reference.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "session-response",
        kind: KIND_REFERENCE,
        syntax: "session-response",
        description: "References captured or live session output; summarize or consult the referenced output when it is available.",
        use_when: "Use to attach a session's output as context rather than copying it.",
        surfaces: ALL_SURFACES,
        restriction: None,
    },
    MarkdownExtensionDescriptor {
        name: "program-section",
        kind: KIND_REFERENCE,
        syntax: ":::clip program\nsection=\"<heading>\"\n:::",
        description: "Projects the named section (a heading and its content) of the owning session's program document, read-only and live. One source of truth: edits happen on the program through its own write path, and every projection follows automatically.",
        use_when: "Use in a widget to mirror program state — for example a Progress section — instead of maintaining a second copy that can go stale.",
        surfaces: &[SURFACE_WIDGET],
        restriction: Some(
            "Widget-only: a program projecting its own section would recurse, and cross-program projection is out of scope; programs reference other sessions with @{session:...} instead.",
        ),
    },
];

/// Extensions available on `surface`, in registry order.
pub fn extensions_for_surface(
    surface: &str,
) -> impl Iterator<Item = &'static MarkdownExtensionDescriptor> + '_ {
    CONSTRUCT_MARKDOWN_EXTENSIONS
        .iter()
        .filter(move |ext| ext.applies_to(surface))
}

/// The full registry as serializable entries, for context payloads and
/// client bootstraps.
pub fn markdown_extensions() -> Vec<MarkdownExtension> {
    CONSTRUCT_MARKDOWN_EXTENSIONS
        .iter()
        .map(MarkdownExtension::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_extension_names_at_least_one_surface() {
        for ext in CONSTRUCT_MARKDOWN_EXTENSIONS {
            assert!(
                !ext.surfaces.is_empty(),
                "extension {} lists no surfaces",
                ext.name
            );
        }
    }

    #[test]
    fn restricted_extensions_record_a_reason() {
        for ext in CONSTRUCT_MARKDOWN_EXTENSIONS {
            let restricted = ext.surfaces.len() < ALL_SURFACES.len();
            assert_eq!(
                restricted,
                ext.restriction.is_some(),
                "extension {} must record a restriction reason iff it narrows surfaces",
                ext.name
            );
        }
    }

    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for ext in CONSTRUCT_MARKDOWN_EXTENSIONS {
            assert!(seen.insert(ext.name), "duplicate extension name {}", ext.name);
        }
    }

    #[test]
    fn program_surface_gets_display_action_and_reference_extensions() {
        let kinds: std::collections::HashSet<&str> = extensions_for_surface(SURFACE_PROGRAM)
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(KIND_DISPLAY));
        assert!(kinds.contains(KIND_ACTION));
        assert!(kinds.contains(KIND_REFERENCE));
    }

    #[test]
    fn widget_surface_gets_smart_clips() {
        assert!(extensions_for_surface(SURFACE_WIDGET).any(|e| e.name == "session"));
        assert!(extensions_for_surface(SURFACE_WIDGET).any(|e| e.name == "program-section"));
    }
}
