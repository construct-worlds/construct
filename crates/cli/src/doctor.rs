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
use construct_daemon::doctor::{
    DaemonFeature, DaemonFeatures, DaemonHarness, DaemonProbe, DoctorInput, Finding, Report,
};
#[cfg(test)]
use construct_daemon::doctor::{Section, Severity};

use crate::BUILD_ID;

pub async fn run(socket: &Path, socket_overridden: bool, json: bool) -> Result<()> {
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
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
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

/// Pure: the whole report as text. No terminal, no I/O — the TUI's colour
/// helpers are ratatui-only and unusable from a plain subcommand, so this
/// is deliberately ASCII and pipe-friendly.
pub fn render(report: &Report) -> String {
    let label_width = report
        .sections
        .iter()
        .flat_map(|s| s.findings.iter())
        .map(|f| f.label.chars().count())
        .max()
        .unwrap_or(LABEL_MIN)
        .clamp(LABEL_MIN, LABEL_MAX);

    let mut out = String::from("construct doctor\n");

    for section in &report.sections {
        out.push('\n');
        out.push_str(&section.title);
        out.push('\n');
        if section.findings.is_empty() {
            out.push_str("  (no checks)\n");
            continue;
        }
        for finding in &section.findings {
            out.push_str(&render_finding(finding, label_width));
        }
    }

    out.push('\n');
    out.push_str(&summary_line(report));
    out.push('\n');
    out
}

fn render_finding(finding: &Finding, label_width: usize) -> String {
    let status = format!("[{}]", finding.severity.as_str());
    let mut out = format!(
        "  {status:<STATUS_WIDTH$}{:<label_width$}  {}\n",
        finding.label, finding.detail
    );

    // Continuation lines align under the detail column.
    let indent = " ".repeat(2 + STATUS_WIDTH + label_width + 2);
    for note in &finding.notes {
        out.push_str(&format!("{indent}{note}\n"));
    }
    if let Some(fix) = &finding.fix {
        out.push_str(&format!("{indent}fix: {fix}\n"));
    }
    out
}

fn summary_line(report: &Report) -> String {
    let s = &report.summary;
    let counts = format!(
        "{} ok, {} info, {} warn, {} error",
        s.ok, s.info, s.warn, s.error
    );
    if s.error == 0 {
        format!("{counts} — construct looks healthy.")
    } else {
        let noun = if s.error == 1 { "problem" } else { "problems" };
        format!("{counts} — {} {noun} will prevent construct from working. Fix the [error] lines above.", s.error)
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
        let out = render(&r);

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
        let out = render(&r);

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
        assert!(render(&r).contains("(no checks)"));
    }

    #[test]
    fn a_clean_report_ends_with_a_healthy_summary() {
        let r = report(vec![section(
            "s",
            vec![finding("version", Severity::Ok, "0.16.7")],
        )]);
        let out = render(&r);
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
            render(&r).contains("2 problems will prevent construct from working"),
            "{}",
            render(&r)
        );

        let one = report(vec![section(
            "s",
            vec![finding("data", Severity::Error, "not writable")],
        )]);
        assert!(render(&one).contains("1 problem will prevent"));
    }

    #[test]
    fn warnings_alone_still_read_as_healthy() {
        let r = report(vec![section(
            "daemon",
            vec![finding("socket", Severity::Warn, "no daemon running")],
        )]);
        assert!(
            render(&r).contains("construct looks healthy."),
            "a machine with no daemon running is not a broken machine"
        );
    }
}
