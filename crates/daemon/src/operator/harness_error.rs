//! Recovering a harness's own error text from what it drew on its PTY.
//!
//! An interactive harness that fails mid-turn does not necessarily tell the
//! daemon anything. It prints the failure into its viewport and returns to its
//! composer, which from outside is indistinguishable from a turn that finished
//! — except that no answer came out of it. The text is right there on screen,
//! and it is usually the single most useful thing to hand the person waiting
//! at the other end of a channel: "stream disconnected before completion"
//! tells them what to do next, "the turn ended without an answer" does not.
//!
//! This is best-effort by construction. It reads a bounded tail of the PTY
//! stream, so it can only ever be as good as what the harness chose to draw,
//! and it is only consulted for a turn already known to have failed — a wrong
//! guess costs a slightly-off sentence in a failure notice, never an answer.

use construct_protocol::{SessionEvent, TimestampedEvent};

/// How much decoded PTY tail to consider: comfortably more than a screenful,
/// far less than a long session's scrollback.
const TAIL_BYTES: usize = 32 * 1024;

/// The longest error text to quote into a channel message.
const MAX_DETAIL: usize = 240;

/// Glyphs harnesses draw to mark a line as an error. Matching the marker
/// rather than the wording keeps this from being a list of every phrase every
/// harness might use.
const ERROR_MARKERS: [char; 5] = ['■', '✗', '✘', '×', '⚠'];

/// Glyphs that begin the harness's own chrome — the composer prompt and its
/// hints. Reaching one means the error text ended and the screen has moved on
/// to furniture that would be nonsense to quote.
const CHROME_MARKERS: [char; 4] = ['›', '‣', '⏵', '>'];

/// The harness's last drawn error, if it drew one.
pub(super) fn harness_error_detail(events: &[TimestampedEvent]) -> Option<String> {
    let lines = display_lines(&pty_tail(events));
    let start = lines.iter().rposition(|line| is_error_line(line))?;
    let mut detail = strip_marker(&lines[start]).to_string();
    // Harnesses wrap a long error across several drawn lines; the URL that
    // says *which* endpoint failed routinely lands on the second one. Keep
    // taking lines until the screen changes subject.
    for line in &lines[start + 1..] {
        if line.is_empty() || starts_with_chrome(line) {
            break;
        }
        if detail.chars().count() + 1 + line.chars().count() > MAX_DETAIL {
            break;
        }
        detail.push(' ');
        detail.push_str(line);
    }
    let detail = detail.trim();
    (!detail.is_empty()).then(|| truncate(detail))
}

fn is_error_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with(ERROR_MARKERS) {
        // A bare marker with nothing after it is a box-drawing character in
        // some unrelated widget, not a message.
        return !strip_marker(trimmed).is_empty();
    }
    // Harnesses that spell it out instead of drawing a glyph. Anchored at the
    // start so an answer *discussing* an error is not mistaken for one.
    let lowered = trimmed.to_ascii_lowercase();
    ["error:", "error ", "fatal:", "failed to "]
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
}

fn starts_with_chrome(line: &str) -> bool {
    line.trim_start().starts_with(CHROME_MARKERS)
}

fn strip_marker(line: &str) -> &str {
    line.trim_start()
        .trim_start_matches(ERROR_MARKERS)
        .trim_start()
}

fn truncate(detail: &str) -> String {
    if detail.chars().count() <= MAX_DETAIL {
        return detail.to_string();
    }
    let kept: String = detail.chars().take(MAX_DETAIL - 1).collect();
    format!("{}…", kept.trim_end())
}

/// The tail of the session's raw PTY byte stream.
///
/// Chunks are concatenated as bytes before being decoded: a harness is free to
/// split a multi-byte character across two writes, and decoding each chunk on
/// its own would turn the error marker itself into replacement characters.
fn pty_tail(events: &[TimestampedEvent]) -> String {
    use base64::Engine as _;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    for event in events.iter().rev() {
        let SessionEvent::Pty { data } = &event.event else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
            continue;
        };
        total += bytes.len();
        chunks.push(bytes);
        if total >= TAIL_BYTES {
            break;
        }
    }
    chunks.reverse();
    String::from_utf8_lossy(&chunks.concat()).into_owned()
}

/// Split a raw terminal stream into the lines it would have drawn.
///
/// A TUI does not end its lines with newlines — it moves the cursor and erases.
/// Treating cursor motion and erasure as line breaks is what keeps a redraw
/// from collapsing an error banner, the composer, and the model footer into one
/// run-on string. Everything else in an escape sequence is presentation and is
/// dropped.
fn display_lines(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                    }
                    if matches!(
                        final_byte,
                        Some('A'..='H' | 'J' | 'K' | 'L' | 'M' | 'S' | 'T' | 'f')
                    ) {
                        lines.push(std::mem::take(&mut current));
                    }
                }
                // OSC (window title and friends): runs to BEL or ST.
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' | '\n' => lines.push(std::mem::take(&mut current)),
            ch if ch.is_control() => {}
            ch => current.push(ch),
        }
    }
    lines.push(current);
    lines
        .into_iter()
        .map(|line| line.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn pty(raw: &str) -> TimestampedEvent {
        TimestampedEvent {
            seq: 0,
            at: chrono::Utc::now(),
            event: SessionEvent::Pty {
                data: base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()),
            },
        }
    }

    #[test]
    fn a_codex_stream_failure_is_recovered_whole() {
        // Captured verbatim from the PTY of a live codex operator session whose
        // turn died against a router port that had moved. The banner and the
        // URL that identifies it arrive in two separate writes, and the
        // composer redraw follows immediately behind — the message is only
        // useful if all three are handled: join the first two, stop before the
        // third.
        let events = vec![
            pty("\x1b[39;49m\x1b[K\x1b[39m\x1b[49m\x1b[0m\r\n\x1b[39;49m\x1b[K\x1b[38;5;1;49m■ stream disconnected before completion: error sending request for url\x1b[39m\x1b[49m\x1b[0m"),
            pty("\x1b[39;49m\x1b[K\x1b[38;5;1;49m(https://chatgpt.com/backend-api/codex/responses)\x1b[39m\x1b[49m\x1b[0m\x1b[r\x1b[60;3H\x1b[57;2H\x1b[0m\x1b[49m\x1b[K\x1b[59;1H\x1b[1m›\x1b[59;3H\x1b[22m\x1b[2mExplain this codebase\x1b[61;3H\x1b[22mgpt-5.6-luna high fast\x1b[0m"),
        ];

        assert_eq!(
            harness_error_detail(&events).as_deref(),
            Some(
                "stream disconnected before completion: error sending request for url \
                 (https://chatgpt.com/backend-api/codex/responses)"
            )
        );
    }

    #[test]
    fn the_composer_is_never_quoted_as_part_of_the_error() {
        let events = vec![pty(
            "\x1b[K■ something broke\x1b[K› ask me anything\x1b[Kmodel · ~/somewhere",
        )];
        assert_eq!(
            harness_error_detail(&events).as_deref(),
            Some("something broke")
        );
    }

    #[test]
    fn the_last_error_wins_when_a_session_has_failed_before() {
        // The tail carries every error this session ever drew. Only the one
        // belonging to the turn that just ended is worth reporting.
        let events = vec![pty("■ an old failure\r\n\r\n"), pty("■ the current one\r\n")];
        assert_eq!(
            harness_error_detail(&events).as_deref(),
            Some("the current one")
        );
    }

    #[test]
    fn a_session_that_drew_no_error_yields_nothing() {
        let events = vec![pty("\x1b[Kall good here\x1b[K› ask me anything")];
        assert_eq!(harness_error_detail(&events), None);
    }

    #[test]
    fn spelled_out_errors_are_recognized_without_a_glyph() {
        let events = vec![pty("\x1b[KError: the model refused the request\x1b[K")];
        assert_eq!(
            harness_error_detail(&events).as_deref(),
            Some("Error: the model refused the request")
        );
    }

    #[test]
    fn an_answer_that_merely_discusses_an_error_is_not_one() {
        // Anchoring at the start of a line is what keeps a genuine reply about
        // error handling from being reported as a failure.
        let events = vec![pty(
            "\x1b[KThe function returns an error when the file is missing.\x1b[K",
        )];
        assert_eq!(harness_error_detail(&events), None);
    }

    #[test]
    fn a_multi_byte_glyph_split_across_two_writes_still_matches() {
        // The marker is three bytes and a harness may flush mid-character.
        // Decoding chunk-by-chunk would replace it with U+FFFD and lose the
        // line entirely.
        let marker = "■".as_bytes();
        let head = [&b"\x1b[K"[..], &marker[..1]].concat();
        let tail = [&marker[1..], &b" split banner\r\n"[..]].concat();
        let events = vec![
            TimestampedEvent {
                seq: 0,
                at: chrono::Utc::now(),
                event: SessionEvent::Pty {
                    data: base64::engine::general_purpose::STANDARD.encode(&head),
                },
            },
            TimestampedEvent {
                seq: 1,
                at: chrono::Utc::now(),
                event: SessionEvent::Pty {
                    data: base64::engine::general_purpose::STANDARD.encode(&tail),
                },
            },
        ];
        assert_eq!(
            harness_error_detail(&events).as_deref(),
            Some("split banner")
        );
    }

    #[test]
    fn a_runaway_error_is_capped_rather_than_pasted_whole() {
        let long = "x".repeat(1000);
        let events = vec![pty(&format!("\x1b[K■ {long}\r\n"))];
        let detail = harness_error_detail(&events).unwrap();
        assert!(detail.chars().count() <= MAX_DETAIL);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn sessions_with_no_pty_at_all_are_handled() {
        assert_eq!(harness_error_detail(&[]), None);
    }
}
