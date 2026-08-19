//! Source-preserving Markdown context needed by the Playbook editor.
//!
//! The editor is intentionally not a WYSIWYG renderer, but it still needs to
//! know when ordinary Markdown syntax is inside a raw fenced-code region.  A
//! single stateful classifier keeps painting, cursor geometry, hit-testing,
//! and smart-clip editing on the same interpretation of backtick fences.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybookLineKind {
    Markdown,
    CodeFence,
    Code,
}

impl PlaybookLineKind {
    pub(crate) fn is_markdown(self) -> bool {
        matches!(self, Self::Markdown)
    }
}

#[derive(Debug, Default)]
pub(crate) struct PlaybookLineClassifier {
    open_backticks: Option<usize>,
}

impl PlaybookLineClassifier {
    pub(crate) fn classify(&mut self, raw: &str) -> PlaybookLineKind {
        if let Some(open_len) = self.open_backticks {
            if backtick_fence(raw)
                .is_some_and(|(len, tail)| len >= open_len && tail.trim().is_empty())
            {
                self.open_backticks = None;
                PlaybookLineKind::CodeFence
            } else {
                PlaybookLineKind::Code
            }
        } else if let Some((len, tail)) = backtick_fence(raw) {
            // CommonMark forbids backticks in a backtick fence's info string.
            // Enforcing that keeps an inline run such as ```a`b literal.
            if !tail.contains('`') {
                self.open_backticks = Some(len);
                PlaybookLineKind::CodeFence
            } else {
                PlaybookLineKind::Markdown
            }
        } else {
            PlaybookLineKind::Markdown
        }
    }
}

/// A CommonMark-style backtick fence may be indented by at most three spaces
/// and contains at least three backticks. Returns the run length and remainder.
fn backtick_fence(raw: &str) -> Option<(usize, &str)> {
    let spaces = raw.bytes().take_while(|b| *b == b' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = &raw[spaces..];
    let ticks = rest.bytes().take_while(|b| *b == b'`').count();
    (ticks >= 3).then(|| (ticks, &rest[ticks..]))
}

/// Whether the source character at `offset` belongs to a fence delimiter or
/// fenced body. `offset == markdown.chars().count()` resolves to the final
/// logical line, which is useful when deciding whether a newly typed `@`
/// should open the smart-clip picker.
pub(crate) fn playbook_offset_is_fenced(markdown: &str, offset: usize) -> bool {
    let offset = offset.min(markdown.chars().count());
    let mut classifier = PlaybookLineClassifier::default();
    let mut line_start = 0usize;
    for raw in markdown.split('\n') {
        let kind = classifier.classify(raw);
        let line_end = line_start + raw.chars().count();
        if offset <= line_end {
            return !kind.is_markdown();
        }
        line_start = line_end + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_backtick_fences_and_literal_body() {
        let mut parser = PlaybookLineClassifier::default();
        let kinds: Vec<_> = ["before", "```rust", "# literal", "```", "after"]
            .into_iter()
            .map(|line| parser.classify(line))
            .collect();
        assert_eq!(
            kinds,
            [
                PlaybookLineKind::Markdown,
                PlaybookLineKind::CodeFence,
                PlaybookLineKind::Code,
                PlaybookLineKind::CodeFence,
                PlaybookLineKind::Markdown,
            ]
        );
    }

    #[test]
    fn closing_fence_must_match_opening_run() {
        let mut parser = PlaybookLineClassifier::default();
        assert_eq!(parser.classify("````lang"), PlaybookLineKind::CodeFence);
        assert_eq!(parser.classify("```"), PlaybookLineKind::Code);
        assert_eq!(parser.classify("````"), PlaybookLineKind::CodeFence);
    }

    #[test]
    fn four_space_indent_and_backtick_info_stay_markdown() {
        let mut parser = PlaybookLineClassifier::default();
        assert_eq!(parser.classify("    ```"), PlaybookLineKind::Markdown);
        assert_eq!(parser.classify("```a`b"), PlaybookLineKind::Markdown);
    }

    #[test]
    fn offset_detection_includes_fences_and_unclosed_body() {
        let markdown = "before\n```\n@{session:example}\nstill code";
        assert!(!playbook_offset_is_fenced(markdown, 2));
        assert!(playbook_offset_is_fenced(
            markdown,
            markdown.find("```").unwrap()
        ));
        assert!(playbook_offset_is_fenced(
            markdown,
            markdown.find("@{").unwrap()
        ));
        assert!(playbook_offset_is_fenced(
            markdown,
            markdown.chars().count()
        ));
    }
}
