//! `construct doctor` — the CLI half: probe the daemon (without ever
//! starting one), hand the facts to `construct_daemon::doctor`, render, and
//! set the exit code.
//!
//! Doctor's primary use case is a machine where something is already
//! broken, so this deliberately does **not** call `ensure_daemon_running`:
//! a diagnostic that mutates the system it is diagnosing is worse than no
//! diagnostic. See spec 0168.

use std::path::Path;

use anyhow::Result;
use construct_client::Client;
#[cfg(test)]
use construct_daemon::doctor::Section;
use construct_daemon::doctor::{
    DaemonFeature, DaemonFeatures, DaemonHarness, DaemonProbe, DoctorInput, Finding, Report,
    Severity,
};

use crate::ansi::{ColorChoice, Palette};
use crate::BUILD_ID;

pub async fn run(
    socket: &Path,
    socket_overridden: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let daemon = probe_daemon(socket).await;

    let report = construct_daemon::doctor::run(DoctorInput {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_build_id: BUILD_ID.to_string(),
        socket: socket.to_path_buf(),
        socket_overridden,
        daemon,
        update: crate::upgrade::cached_update_probe(),
    })
    .await;

    if json {
        // Machine output is never styled: escape codes would land inside
        // whatever parses this.
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report, color.resolve()));
    }

    if report.has_errors() {
        // `exit` skips destructors, so flush explicitly rather than relying
        // on stdout's flush-on-drop. Exiting this way keeps anyhow from
        // printing a spurious `Error:` line under the report — same
        // technique the `ssh` subcommand uses to forward its child's code.
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    Ok(())
}

/// Ask the daemon what it knows — but only if one is already listening.
async fn probe_daemon(socket: &Path) -> DaemonProbe {
    if !crate::socket_is_live(socket) {
        return DaemonProbe::Down {
            socket_file_present: socket.exists(),
        };
    }

    let client = match Client::connect(socket).await {
        Ok(c) => c,
        Err(e) => {
            return DaemonProbe::Unresponsive {
                error: format!("{e:#}"),
            };
        }
    };

    let ping = match client.ping().await {
        Ok(p) => p,
        Err(e) => {
            return DaemonProbe::Unresponsive {
                error: format!("{e:#}"),
            };
        }
    };

    // Past this point the daemon is demonstrably alive: a harness or
    // feature query that fails is a missing capability on an older daemon,
    // not a broken install, so degrade quietly instead of erroring.
    let harnesses = client
        .harnesses()
        .await
        .map(|list| {
            list.into_iter()
                .map(|h| DaemonHarness {
                    name: h.name,
                    available: h.available,
                    detail: h.detail,
                })
                .collect()
        })
        .unwrap_or_default();

    let features = client.features_status().await.ok().map(|s| DaemonFeatures {
        features: s
            .features
            .into_iter()
            .map(|f| DaemonFeature {
                label: f.label,
                status: f.status,
                detail: f.detail,
            })
            .collect(),
        degradation_observed: s.degradation_observed,
    });

    DaemonProbe::Up {
        build_skew: crate::app::daemon_build_ids_differ(BUILD_ID, ping.build_id.as_deref()),
        build_id: ping.build_id,
        harnesses,
        features,
    }
}

// ───────────────────────────── rendering ─────────────────────────────

/// Width of the `[status]` column, sized for the longest token (`[error]`).
const STATUS_WIDTH: usize = 9;
const LABEL_MIN: usize = 14;
const LABEL_MAX: usize = 28;

/// Paint a severity's status token. Colors are the basic ANSI 16 so the
/// user's terminal profile owns the hues — see [`crate::ansi`].
fn paint_severity(p: Palette, sev: Severity, text: &str) -> String {
    match sev {
        Severity::Ok => p.green(text),
        // Info is context, not a call to action: it must not compete with
        // the warn and error rows for attention.
        Severity::Info => p.dim(text),
        Severity::Warn => p.yellow(text),
        Severity::Error => p.red(text),
    }
}

/// Pure: the whole report as text. No terminal, no I/O — the palette is
/// resolved by the caller and passed in, so tests render both styled and
/// plain without touching the environment.
///
/// Styling is strictly additive: [`Palette::PLAIN`] reproduces the original
/// ASCII output byte for byte, and stripping SGR codes from a styled render
/// yields exactly that. Column math therefore always runs on the unstyled
/// text, and escape codes are wrapped around already-padded tokens.
pub fn render(report: &Report, palette: Palette) -> String {
    let label_width = report
        .sections
        .iter()
        .flat_map(|s| s.findings.iter())
        .map(|f| f.label.chars().count())
        .max()
        .unwrap_or(LABEL_MIN)
        .clamp(LABEL_MIN, LABEL_MAX);

    let mut out = palette.bold("construct doctor");
    out.push('\n');

    for section in &report.sections {
        out.push('\n');
        out.push_str(&palette.bold(&section.title));
        out.push('\n');
        if section.findings.is_empty() {
            out.push_str(&format!("  {}\n", palette.dim("(no checks)")));
            continue;
        }
        for finding in &section.findings {
            out.push_str(&render_finding(finding, label_width, palette));
        }
    }

    out.push('\n');
    out.push_str(&summary_line(report, palette));
    out.push('\n');
    out
}

fn render_finding(finding: &Finding, label_width: usize, palette: Palette) -> String {
    // Pad first, colorize second. Escape bytes are invisible but they are
    // still bytes: `{status:<WIDTH$}` on a styled token would count them
    // toward the width and shear the column.
    let status = format!("[{}]", finding.severity.as_str());
    let status_pad = " ".repeat(STATUS_WIDTH.saturating_sub(status.chars().count()));
    let label_pad = " ".repeat(label_width.saturating_sub(finding.label.chars().count()));

    let mut out = format!(
        "  {}{status_pad}{}{label_pad}  {}\n",
        paint_severity(palette, finding.severity, &status),
        finding.label,
        finding.detail
    );

    // Continuation lines align under the detail column.
    let indent = " ".repeat(2 + STATUS_WIDTH + label_width + 2);
    for note in &finding.notes {
        out.push_str(&format!("{indent}{}\n", palette.dim(note)));
    }
    if let Some(fix) = &finding.fix {
        // Only the marker is colored — the command stays in the default
        // foreground so a copy-paste selection reads as ordinary text.
        out.push_str(&format!("{indent}{} {fix}\n", palette.cyan("fix:")));
    }
    out
}

fn summary_line(report: &Report, p: Palette) -> String {
    let s = &report.summary;
    // A zero count is not news; dim it so the eye lands on what is nonzero.
    let count = |n: usize, word: &str, sev: Severity| {
        let text = format!("{n} {word}");
        if n == 0 {
            p.dim(&text)
        } else {
            paint_severity(p, sev, &text)
        }
    };
    let counts = format!(
        "{}, {}, {}, {}",
        count(s.ok, "ok", Severity::Ok),
        count(s.info, "info", Severity::Info),
        count(s.warn, "warn", Severity::Warn),
        count(s.error, "error", Severity::Error),
    );
    if s.error == 0 {
        format!("{counts} — {}", p.green("construct looks healthy."))
    } else {
        let noun = if s.error == 1 { "problem" } else { "problems" };
        let verdict = format!(
            "{} {noun} will prevent construct from working. Fix the [error] lines above.",
            s.error
        );
        format!("{counts} — {}", p.red(&verdict))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(label: &str, severity: Severity, detail: &str) -> Finding {
        // The daemon crate builds findings through private constructors, so
        // tests assemble them field-wise.
        Finding {
            id: format!("test.{label}"),
            label: label.to_string(),
            severity,
            detail: detail.to_string(),
            fix: None,
            notes: Vec::new(),
        }
    }

    fn section(title: &str, findings: Vec<Finding>) -> Section {
        Section {
            id: title.to_string(),
            title: title.to_string(),
            findings,
        }
    }

    /// Build through the daemon's real summarizer so these tests exercise
    /// the same counts the command prints.
    fn report(sections: Vec<Section>) -> Report {
        Report::summarize(sections)
    }

    #[test]
    fn every_status_token_fits_the_status_column() {
        for sev in [
            Severity::Ok,
            Severity::Info,
            Severity::Warn,
            Severity::Error,
        ] {
            assert!(
                format!("[{}]", sev.as_str()).len() < STATUS_WIDTH,
                "{sev:?} overflows the status column"
            );
        }
    }

    #[test]
    fn a_long_label_is_clamped_so_one_check_cannot_wreck_the_layout() {
        let long = "a".repeat(60);
        let r = report(vec![section(
            "s",
            vec![
                finding(&long, Severity::Ok, "detail"),
                finding("short", Severity::Ok, "detail"),
            ],
        )]);
        let out = render(&r, Palette::PLAIN);

        // The long label is not truncated, but the column stops growing —
        // the short row's detail sits at LABEL_MAX, not at 60.
        let short_line = out.lines().find(|l| l.contains("short")).unwrap();
        assert_eq!(
            short_line.find("detail").unwrap(),
            2 + STATUS_WIDTH + LABEL_MAX + 2
        );
        assert!(out.contains("[ok]"));
    }

    #[test]
    fn notes_and_fix_lines_are_indented_under_the_detail_column() {
        let mut f = finding("PATH", Severity::Warn, "2 binaries on PATH");
        f.notes = vec!["1. /usr/local/bin/construct".into()];
        f.fix = Some("rm '/opt/homebrew/bin/construct'".into());
        let r = report(vec![section("environment", vec![f])]);
        let out = render(&r, Palette::PLAIN);

        let note = out
            .lines()
            .find(|l| l.contains("1. /usr/local/bin"))
            .unwrap();
        let fix = out.lines().find(|l| l.contains("fix:")).unwrap();
        let detail_col = out
            .lines()
            .find(|l| l.contains("2 binaries"))
            .unwrap()
            .find("2 binaries")
            .unwrap();

        assert_eq!(note.len() - note.trim_start().len(), detail_col);
        assert_eq!(fix.len() - fix.trim_start().len(), detail_col);
        assert!(
            fix.trim_start().starts_with("fix: "),
            "fix lines stay greppable"
        );
    }

    #[test]
    fn an_empty_section_says_so_rather_than_vanishing() {
        let r = report(vec![section("logins", vec![])]);
        assert!(render(&r, Palette::PLAIN).contains("(no checks)"));
    }

    #[test]
    fn a_clean_report_ends_with_a_healthy_summary() {
        let r = report(vec![section(
            "s",
            vec![finding("version", Severity::Ok, "0.16.7")],
        )]);
        let out = render(&r, Palette::PLAIN);
        assert!(
            out.trim_end().ends_with("construct looks healthy."),
            "{out}"
        );
        assert!(out.contains("1 ok, 0 info, 0 warn, 0 error"));
    }

    #[test]
    fn errors_pluralize_and_point_at_the_error_lines() {
        let r = report(vec![section(
            "s",
            vec![
                finding("data", Severity::Error, "not writable"),
                finding("config", Severity::Error, "parse failed"),
            ],
        )]);
        assert!(
            render(&r, Palette::PLAIN).contains("2 problems will prevent construct from working"),
            "{}",
            render(&r, Palette::PLAIN)
        );

        let one = report(vec![section(
            "s",
            vec![finding("data", Severity::Error, "not writable")],
        )]);
        assert!(render(&one, Palette::PLAIN).contains("1 problem will prevent"));
    }

    /// One report exercising every styled element: all four severities, a
    /// clamped label, notes, a fix line, and an empty section.
    fn kitchen_sink() -> Report {
        let mut warn = finding("PATH", Severity::Warn, "2 binaries on PATH");
        warn.notes = vec!["1. /usr/local/bin/construct".into()];
        warn.fix = Some("rm '/opt/homebrew/bin/construct'".into());
        report(vec![
            section(
                "environment",
                vec![
                    finding("version", Severity::Ok, "0.16.7"),
                    finding(&"b".repeat(40), Severity::Info, "skipped"),
                    warn,
                    finding("config", Severity::Error, "parse failed"),
                ],
            ),
            section("logins", vec![]),
        ])
    }

    #[test]
    fn color_adds_escape_codes_and_changes_nothing_else() {
        let r = kitchen_sink();
        let plain = render(&r, Palette::PLAIN);
        let styled = render(&r, ColorChoice::Always.resolve());

        assert!(styled.contains('\x1b'), "nothing was styled at all");
        // The load-bearing invariant. Every column offset asserted by the
        // other tests holds under color for free, and piping through a
        // stripper (or `NO_COLOR`) recovers the exact original output.
        assert_eq!(crate::ansi::strip(&styled), plain);
    }

    #[test]
    fn each_severity_gets_its_own_color() {
        let styled = render(&kitchen_sink(), ColorChoice::Always.resolve());
        for (token, code) in [
            ("[ok]", "32"),
            ("[info]", "2"),
            ("[warn]", "33"),
            ("[error]", "1;31"),
        ] {
            assert!(
                styled.contains(&format!("\x1b[{code}m{token}\x1b[0m")),
                "{token} should be painted with SGR {code}"
            );
        }
    }

    #[test]
    fn padding_stays_outside_the_escape_codes() {
        // Regression guard for the tempting one-liner
        // `format!("{styled:<STATUS_WIDTH$}")`, which pads to the *byte*
        // width and so eats the column whenever styling is on.
        let styled = render(&kitchen_sink(), ColorChoice::Always.resolve());
        let ok_line = styled.lines().find(|l| l.contains("[ok]")).unwrap();
        assert!(
            ok_line.contains("\x1b[0m     version"),
            "reset must come before the pad: {ok_line:?}"
        );
    }

    #[test]
    fn warnings_alone_still_read_as_healthy() {
        let r = report(vec![section(
            "daemon",
            vec![finding("socket", Severity::Warn, "no daemon running")],
        )]);
        assert!(
            render(&r, Palette::PLAIN).contains("construct looks healthy."),
            "a machine with no daemon running is not a broken machine"
        );
    }
}
