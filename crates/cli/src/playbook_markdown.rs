//! Source-preserving Markdown context needed by the Playbook editor.
//!
//! Completed one-line triple-backtick spans are presented like completed
//! inline code. Completed multiline fences keep one rendered row per source
//! line while hiding only their delimiter glyphs; incomplete fences remain
//! literal and inert. A shared classifier keeps painting, hit-testing, and
//! editing in agreement.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybookLineKind {
    Markdown,
    CodeFence,
    Code,
    FormattedCodeFence,
    FormattedCode,
}

impl PlaybookLineKind {
    pub(crate) fn is_markdown(self) -> bool {
        matches!(self, Self::Markdown)
    }

    pub(crate) fn is_formatted_code(self) -> bool {
        matches!(self, Self::FormattedCodeFence | Self::FormattedCode)
    }

    pub(crate) fn is_formatted_fence(self) -> bool {
        matches!(self, Self::FormattedCodeFence)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlaybookInlineFence {
    /// Byte range of the full triple-backtick source token.
    pub(crate) source: Range<usize>,
    /// Byte range of the visible content between the delimiters.
    pub(crate) content: Range<usize>,
}

/// Completed, non-empty, exact triple-backtick spans on one source line.
/// Single and double backticks may occur in the body; runs of four or more do
/// not masquerade as triple delimiters.
pub(crate) fn playbook_inline_fences(line: &str) -> Vec<PlaybookInlineFence> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut from = 0usize;
    while from + 3 <= bytes.len() {
        let Some(rel) = line[from..].find("```") else {
            break;
        };
        let start = from + rel;
        let exact_open = (start == 0 || bytes[start - 1] != b'`')
            && bytes.get(start + 3).is_none_or(|b| *b != b'`');
        if !exact_open {
            from = start + 1;
            continue;
        }
        let content_start = start + 3;
        let mut close_from = content_start;
        let mut found = None;
        while close_from + 3 <= bytes.len() {
            let Some(close_rel) = line[close_from..].find("```") else {
                break;
            };
            let close = close_from + close_rel;
            let exact_close = (close == 0 || bytes[close - 1] != b'`')
                && bytes.get(close + 3).is_none_or(|b| *b != b'`');
            if exact_close && close > content_start {
                found = Some(close);
                break;
            }
            close_from = close + 1;
        }
        let Some(close) = found else {
            break;
        };
        out.push(PlaybookInlineFence {
            source: start..close + 3,
            content: content_start..close,
        });
        from = close + 3;
    }
    out
}

/// Byte offset of an exact triple-backtick opener with no matching closer on
/// the same line. The tail remains literal and inert until completed.
pub(crate) fn playbook_unmatched_inline_fence_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let completed = playbook_inline_fences(line);
    let mut from = 0usize;
    while from + 3 <= bytes.len() {
        let rel = line[from..].find("```")?;
        let start = from + rel;
        let exact = (start == 0 || bytes[start - 1] != b'`')
            && bytes.get(start + 3).is_none_or(|b| *b != b'`');
        if exact
            && !completed
                .iter()
                .any(|fence| fence.source.start <= start && start < fence.source.end)
        {
            return Some(start);
        }
        from = start + 3;
    }
    None
}

pub(crate) fn playbook_closing_inline_fence_before_cursor(
    markdown: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    let cursor = cursor.min(markdown.chars().count());
    let before = markdown.chars().take(cursor).collect::<String>();
    let line_start = before.rfind('\n').map_or(0, |idx| idx + 1);
    let line = &before[line_start..];
    playbook_inline_fences(line)
        .into_iter()
        .find(|fence| fence.source.end == line.len())
        .map(|fence| {
            let prefix_chars = before[..line_start].chars().count();
            let close_start = prefix_chars + line[..fence.source.end - 3].chars().count();
            close_start..close_start + 3
        })
}

#[derive(Debug, Default)]
pub(crate) struct PlaybookLineClassifier {
    open_backticks: Option<usize>,
}

impl PlaybookLineClassifier {
    pub(crate) fn classify(&mut self, raw: &str) -> PlaybookLineKind {
        if let Some(open_len) = self.open_backticks {
            if closing_backtick_fence(raw, open_len).is_some() {
                self.open_backticks = None;
                PlaybookLineKind::CodeFence
            } else {
                PlaybookLineKind::Code
            }
        } else if opening_backtick_fence(raw).is_some() {
            self.open_backticks = backtick_fence(raw).map(|(_, len, _)| len);
            PlaybookLineKind::CodeFence
        } else {
            PlaybookLineKind::Markdown
        }
    }
}

/// A CommonMark-style backtick fence may be indented by at most three spaces
/// and contains at least three backticks. Returns indentation, run length, and
/// the remainder after the run.
fn backtick_fence(raw: &str) -> Option<(usize, usize, &str)> {
    let spaces = raw.bytes().take_while(|b| *b == b' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = &raw[spaces..];
    let ticks = rest.bytes().take_while(|b| *b == b'`').count();
    (ticks >= 3).then(|| (spaces, ticks, &rest[ticks..]))
}

fn opening_backtick_fence(raw: &str) -> Option<(usize, usize)> {
    let (spaces, ticks, tail) = backtick_fence(raw)?;
    // CommonMark forbids backticks in a backtick fence's info string.
    (!tail.contains('`')).then_some((spaces, ticks))
}

fn closing_backtick_fence(raw: &str, opening_len: usize) -> Option<(usize, usize)> {
    let (spaces, ticks, tail) = backtick_fence(raw)?;
    (ticks >= opening_len && tail.trim().is_empty()).then_some((spaces, ticks))
}

/// Presentation kind for every source line. A multiline fence is promoted to
/// the formatted variants only after a valid closer is present; this lets an
/// incomplete fence remain literal while still suppressing Markdown features.
pub(crate) fn playbook_line_kinds(markdown: &str) -> Vec<PlaybookLineKind> {
    let lines = markdown.split('\n').collect::<Vec<_>>();
    let mut kinds = vec![PlaybookLineKind::Markdown; lines.len()];
    let mut open: Option<(usize, usize)> = None;

    for (index, raw) in lines.iter().enumerate() {
        if let Some((open_index, open_len)) = open {
            if closing_backtick_fence(raw, open_len).is_some() {
                kinds[open_index] = PlaybookLineKind::FormattedCodeFence;
                for kind in &mut kinds[open_index + 1..index] {
                    *kind = PlaybookLineKind::FormattedCode;
                }
                kinds[index] = PlaybookLineKind::FormattedCodeFence;
                open = None;
            } else {
                kinds[index] = PlaybookLineKind::Code;
            }
        } else if let Some((_, ticks)) = opening_backtick_fence(raw) {
            kinds[index] = PlaybookLineKind::CodeFence;
            open = Some((index, ticks));
        }
    }

    kinds
}

/// Byte range of the backtick run on a multiline fence delimiter line.
pub(crate) fn playbook_fence_delimiter(raw: &str) -> Option<Range<usize>> {
    let (spaces, ticks, _) = backtick_fence(raw)?;
    Some(spaces..spaces + ticks)
}

/// Closing multiline-fence delimiter immediately before the source cursor.
/// Returned offsets are Unicode character offsets in the full document.
pub(crate) fn playbook_closing_multiline_fence_before_cursor(
    markdown: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    let cursor = cursor.min(markdown.chars().count());
    let mut line_start = 0usize;
    let mut open_len = None;
    for raw in markdown.split('\n') {
        if let Some(required) = open_len {
            if let Some((spaces, ticks)) = closing_backtick_fence(raw, required) {
                let delimiter_end = line_start + spaces + ticks;
                if cursor == delimiter_end {
                    return Some(line_start + spaces..delimiter_end);
                }
                open_len = None;
            }
        } else if let Some((_, ticks)) = opening_backtick_fence(raw) {
            open_len = Some(ticks);
        }
        line_start += raw.chars().count() + 1;
    }
    None
}

/// Whether the source character at `offset` belongs to a fence delimiter or
/// fenced body. This includes incomplete fences so extensions remain inert
/// while their literal source is visible.
pub(crate) fn playbook_offset_is_fenced(markdown: &str, offset: usize) -> bool {
    let offset = offset.min(markdown.chars().count());
    let mut classifier = PlaybookLineClassifier::default();
    let mut line_start = 0usize;
    for raw in markdown.split('\n') {
        let kind = classifier.classify(raw);
        let line_end = line_start + raw.chars().count();
        if offset <= line_end {
            let local = offset.saturating_sub(line_start);
            let in_inline_fence = playbook_inline_fences(raw).into_iter().any(|fence| {
                let start = raw[..fence.source.start].chars().count();
                let end = raw[..fence.source.end].chars().count();
                local >= start && local <= end
            });
            let in_unmatched_inline_fence = playbook_unmatched_inline_fence_start(raw)
                .map(|start| raw[..start].chars().count())
                .is_some_and(|start| local >= start);
            return in_inline_fence || in_unmatched_inline_fence || !kind.is_markdown();
        }
        line_start = line_end + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_multiline_fences_as_inert_source() {
        let mut classifier = PlaybookLineClassifier::default();
        let kinds = ["before", "```rust", "# literal", "```", "after"]
            .into_iter()
            .map(|line| classifier.classify(line))
            .collect::<Vec<_>>();
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
    fn completed_multiline_fence_gets_row_stable_formatted_kinds() {
        assert_eq!(
            playbook_line_kinds("before\n```rust\n界🙂\n```\nafter"),
            [
                PlaybookLineKind::Markdown,
                PlaybookLineKind::FormattedCodeFence,
                PlaybookLineKind::FormattedCode,
                PlaybookLineKind::FormattedCodeFence,
                PlaybookLineKind::Markdown,
            ]
        );
        assert_eq!(
            playbook_line_kinds("```rust\nstill open"),
            [PlaybookLineKind::CodeFence, PlaybookLineKind::Code]
        );
    }

    #[test]
    fn multiline_closing_boundary_is_source_offset_and_cjk_safe() {
        let markdown = "前\n```rust\n界🙂\n  ```  \nafter";
        let cursor = "前\n```rust\n界🙂\n  ```".chars().count();
        let closing = playbook_closing_multiline_fence_before_cursor(markdown, cursor).unwrap();
        assert_eq!(
            markdown
                .chars()
                .skip(closing.start)
                .take(closing.len())
                .collect::<String>(),
            "```"
        );
        assert!(playbook_closing_multiline_fence_before_cursor(markdown, cursor - 1).is_none());
    }

    #[test]
    fn closing_fence_must_match_opening_run() {
        let mut classifier = PlaybookLineClassifier::default();
        assert_eq!(
            ["````lang", "```", "body", "````"]
                .into_iter()
                .map(|line| classifier.classify(line))
                .collect::<Vec<_>>(),
            [
                PlaybookLineKind::CodeFence,
                PlaybookLineKind::Code,
                PlaybookLineKind::Code,
                PlaybookLineKind::CodeFence,
            ]
        );
    }

    #[test]
    fn four_space_indent_and_backtick_info_stay_markdown() {
        let mut classifier = PlaybookLineClassifier::default();
        assert_eq!(classifier.classify("    ```"), PlaybookLineKind::Markdown);
        assert_eq!(classifier.classify("```a`b"), PlaybookLineKind::Markdown);
    }

    #[test]
    fn offset_detection_includes_completed_and_unclosed_body() {
        let markdown = "before\n```\n@{session:example}\n```\nafter\n```\nstill code";
        assert!(!playbook_offset_is_fenced(markdown, 2));
        assert!(playbook_offset_is_fenced(
            markdown,
            markdown[..markdown.find("@{").unwrap()].chars().count()
        ));
        assert!(playbook_offset_is_fenced(
            markdown,
            markdown.chars().count()
        ));
    }

    #[test]
    fn inline_triple_fence_is_completed_source_preserving_and_cjk_safe() {
        let line = "before ```界 ` 🙂``` after";
        let fences = playbook_inline_fences(line);
        assert_eq!(fences.len(), 1);
        assert_eq!(&line[fences[0].content.clone()], "界 ` 🙂");

        let markdown = format!("{line}\nnext");
        let cursor = "before ```界 ` 🙂```".chars().count();
        let closing = playbook_closing_inline_fence_before_cursor(&markdown, cursor).unwrap();
        assert_eq!(
            markdown
                .chars()
                .skip(closing.start)
                .take(closing.len())
                .collect::<String>(),
            "```"
        );
        assert!(playbook_offset_is_fenced(
            &markdown,
            "before ```界".chars().count()
        ));
    }

    #[test]
    fn unmatched_and_non_exact_inline_triples_stay_literal() {
        assert!(playbook_inline_fences("```unfinished").is_empty());
        assert!(playbook_inline_fences("````wide````").is_empty());
        assert!(playbook_inline_fences("``````").is_empty());
        assert_eq!(
            playbook_unmatched_inline_fence_start("before ```raw"),
            Some(7)
        );
    }
}
