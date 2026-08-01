//! ANSI SGR styling for plain-stdout subcommands.
//!
//! The TUI's [`crate::color`] module is ratatui-only: it exists to quantize a
//! 24-bit palette on its way into a ratatui backend, which a subcommand
//! printing to stdout never touches. This is the other half — a handful of
//! escape codes for commands that are just writing lines.
//!
//! Deliberately limited to the 16 basic SGR colors. Their exact hues belong
//! to the user's terminal profile, which means output stays legible on a
//! light background, inside `screen`, and on Apple Terminal — the terminal
//! that garbles `38;2` truecolor outright (see [`crate::color`]). Nothing
//! here needs depth detection because there is no depth to detect.

use std::io::IsTerminal;

/// When to emit escape codes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color when stdout is a terminal that wants it.
    #[default]
    Auto,
    /// Color unconditionally — for piping into `less -R` or a CI log that
    /// renders escape codes.
    Always,
    /// Never color.
    Never,
}

impl ColorChoice {
    /// Resolve against the real process environment and stdout.
    pub fn resolve(self) -> Palette {
        self.resolve_with(std::io::stdout().is_terminal(), |k| std::env::var(k).ok())
    }

    /// Pure form, for tests.
    fn resolve_with(self, stdout_is_tty: bool, env: impl Fn(&str) -> Option<String>) -> Palette {
        let on = match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            // `NO_COLOR` is honored whenever it is present and non-empty,
            // per no-color.org — its *value* is explicitly not consulted.
            ColorChoice::Auto => {
                stdout_is_tty
                    && !env("NO_COLOR").is_some_and(|v| !v.is_empty())
                    && env("TERM").as_deref() != Some("dumb")
            }
        };
        Palette { on }
    }
}

/// Whether styling is live. Every `Palette` method is a no-op when it is not,
/// so callers never branch on color themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct Palette {
    on: bool,
}

impl Palette {
    /// A palette that emits nothing. Same as `Palette::default()`, named so
    /// that `render(&report, Palette::PLAIN)` says what it means.
    #[cfg(test)]
    pub const PLAIN: Palette = Palette { on: false };

    #[cfg(test)]
    pub fn is_on(self) -> bool {
        self.on
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.on && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn red(self, text: &str) -> String {
        self.paint("1;31", text)
    }
    pub fn green(self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }
    pub fn cyan(self, text: &str) -> String {
        self.paint("36", text)
    }
}

/// Strip SGR sequences. Test-only: the invariant worth protecting is that
/// styling changes escape codes and *nothing else*, so tests assert that a
/// styled render strips back to exactly the plain one.
#[cfg(test)]
pub fn strip(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC [ … m
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if c == 'm' {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn auto_colors_only_a_terminal() {
        assert!(ColorChoice::Auto.resolve_with(true, env_none).is_on());
        assert!(
            !ColorChoice::Auto.resolve_with(false, env_none).is_on(),
            "a redirected stdout is usually a file or an issue comment"
        );
    }

    #[test]
    fn no_color_wins_over_a_terminal() {
        let with = |k: &str, v: &str| {
            let (k, v) = (k.to_string(), v.to_string());
            move |q: &str| (q == k).then(|| v.clone())
        };
        assert!(!ColorChoice::Auto
            .resolve_with(true, with("NO_COLOR", "1"))
            .is_on());
        // Present-but-empty does not count, per no-color.org.
        assert!(ColorChoice::Auto
            .resolve_with(true, with("NO_COLOR", ""))
            .is_on());
        assert!(!ColorChoice::Auto
            .resolve_with(true, with("TERM", "dumb"))
            .is_on());
    }

    #[test]
    fn explicit_choices_ignore_the_environment() {
        let no_color = |q: &str| (q == "NO_COLOR").then(|| "1".to_string());
        assert!(
            ColorChoice::Always.resolve_with(false, no_color).is_on(),
            "--color=always is how you pipe into `less -R`"
        );
        assert!(!ColorChoice::Never.resolve_with(true, env_none).is_on());
    }

    #[test]
    fn a_plain_palette_emits_no_escapes() {
        assert_eq!(Palette::PLAIN.red("boom"), "boom");
        assert_eq!(Palette::PLAIN.bold("hi"), "hi");
    }

    #[test]
    fn styling_survives_a_round_trip_through_strip() {
        let p = ColorChoice::Always.resolve_with(false, env_none);
        for painted in [p.red("a"), p.green("b"), p.dim("c"), p.bold("d")] {
            assert!(painted.contains('\x1b'));
            assert_eq!(strip(&painted).len(), 1);
        }
    }

    #[test]
    fn empty_text_is_never_wrapped() {
        // Padding math elsewhere assumes an empty string costs nothing.
        assert_eq!(ColorChoice::Always.resolve_with(true, env_none).red(""), "");
    }
}
