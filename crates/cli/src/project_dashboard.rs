//! Project dashboard pane (view area when a project header is selected).
//!
//! A single full-width column of member cards: each card pairs the roster
//! facts (status, title, harness, model, context, activity) with a content
//! line that says what the session is doing, asking, or blocked on — fed
//! from the daemon's durable last-message / last-error snippets and the
//! live pending-approval set, so triage completes without opening the
//! session. A project-scoped token meter sits above the cards.

use std::collections::HashMap;
use std::time::Instant;

use construct_protocol::{MessageRole, SessionState, SessionSummary};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::token_meter::{self, TokenMeter};

/// Compact meter height when the pane is tall enough.
pub(crate) const METER_HEIGHT: u16 = 4;

/// Rows kept for member cards even when the meter would like more.
const CARDS_MIN_HEIGHT: u16 = 6;

/// A tool call waiting on the user, retained per session so the member
/// card can say *what* wants approval, not just that something does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolApproval {
    pub call_id: String,
    pub tool: String,
    pub args_summary: String,
    pub risk: construct_protocol::ToolRisk,
}

/// Hit zones painted last frame for the project dashboard.
#[derive(Debug, Clone, Default)]
pub struct ProjectDashboardHits {
    pub member_rows: Vec<ProjectRowHit>,
}

#[derive(Debug, Clone)]
pub struct ProjectRowHit {
    pub area: Rect,
    pub session_id: String,
}

impl ProjectRowHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }
}

/// Per-client state for the project dashboard surface.
#[derive(Debug, Default)]
pub struct ProjectDashboard {
    /// Cursor into the sorted member list of the currently selected project.
    pub cursor: usize,
    /// Cards scrolled off the top of the member list.
    pub member_scroll: usize,
    /// Project id the cursor/scroll state applies to (reset on switch).
    pub active_project: Option<String>,
    /// Hovered member (mouse) — highlights without selecting.
    pub hover_session: Option<String>,
    /// project_id → live token meter (fed from Cost events).
    pub token_meters: HashMap<String, TokenMeter>,
    /// Hit zones from the last render.
    pub hits: ProjectDashboardHits,
    /// `(project_id, graph rect)` of the meter drawn on the last frame, so the
    /// hover detail can find which bucket the pointer is over. `None` on any
    /// frame that drew no meter (idle project, or a pane too short for one).
    pub meter_graph: Option<(String, Rect)>,
}

impl ProjectDashboard {
    /// Keep cursor/scroll coherent when the selected project changes.
    pub fn ensure_project(&mut self, project_id: &str) {
        if self.active_project.as_deref() != Some(project_id) {
            self.active_project = Some(project_id.to_string());
            self.cursor = 0;
            self.member_scroll = 0;
            self.hover_session = None;
        }
    }

    pub fn clamp_cursor(&mut self, member_count: usize) {
        if member_count == 0 {
            self.cursor = 0;
            self.member_scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(member_count - 1);
    }

    pub fn move_cursor(&mut self, delta: i32, member_count: usize) {
        if member_count == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor as i32;
        let next = (cur + delta).clamp(0, member_count as i32 - 1) as usize;
        self.cursor = next;
    }

    pub fn observe_cost(
        &mut self,
        project_id: &str,
        model: Option<&str>,
        tokens: u64,
        cached: u64,
        now: Instant,
    ) {
        if tokens == 0 {
            return;
        }
        let meter = self
            .token_meters
            .entry(project_id.to_string())
            .or_insert_with(|| TokenMeter::new(now));
        meter.observe(model, tokens, cached, now);
    }

    pub fn observe_busy(
        &mut self,
        project_id: &str,
        model: Option<&str>,
        delta_ms: u64,
        now: Instant,
    ) {
        if delta_ms == 0 {
            return;
        }
        let meter = self
            .token_meters
            .entry(project_id.to_string())
            .or_insert_with(|| TokenMeter::new(now));
        meter.observe_busy(model, delta_ms, now);
    }

    pub fn forget_session(&mut self, session_id: &str) {
        if self.hover_session.as_deref() == Some(session_id) {
            self.hover_session = None;
        }
    }

    pub fn forget_project(&mut self, project_id: &str) {
        self.token_meters.remove(project_id);
        if self.active_project.as_deref() == Some(project_id) {
            self.active_project = None;
            self.cursor = 0;
            self.member_scroll = 0;
            self.hover_session = None;
        }
    }

    pub fn hit_session_at(&self, col: u16, row: u16) -> Option<&str> {
        self.hits
            .member_rows
            .iter()
            .find(|h| h.contains(col, row))
            .map(|h| h.session_id.as_str())
    }
}

/// Project-scoped tally buckets (same ranking as fleet tally, scoped).
#[derive(Debug, Default, Clone)]
pub struct ProjectTally {
    pub working: usize,
    pub wants_you: usize,
    pub errored: usize,
}

/// Members of a project eligible for the dashboard roster.
pub fn project_members<'a>(
    sessions: &'a [SessionSummary],
    project_id: &str,
) -> Vec<&'a SessionSummary> {
    let mut members: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| s.group_id.as_deref() == Some(project_id))
        .filter(|s| !s.archived)
        .filter(|s| matches!(s.kind, construct_protocol::SessionKind::User))
        .filter(|s| s.native_subagent.is_none())
        // Top-level only: subagents hang under parents; forks stay as members
        // of the project (they share group_id) but nest under their parent in
        // the list — include forks that are themselves top-level project work.
        .filter(|s| s.parent_session_id.is_none())
        .collect();
    sort_members(&mut members);
    members
}

/// Sort: attention/error first, then running, then most recently active.
pub fn sort_members(members: &mut [&SessionSummary]) {
    members.sort_by(|a, b| {
        rank_member(b)
            .cmp(&rank_member(a))
            .then_with(|| recency_key(b).cmp(&recency_key(a)))
            .then_with(|| a.position.cmp(&b.position))
            .then_with(|| primary_label(a).cmp(&primary_label(b)))
    });
}

fn rank_member(s: &SessionSummary) -> u8 {
    if s.state == SessionState::Errored {
        3
    } else if s.needs_attention {
        2
    } else if s.state == SessionState::Running {
        1
    } else {
        0
    }
}

fn recency_key(s: &SessionSummary) -> i64 {
    s.last_message_at
        .or(s.last_event_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| s.created_at.timestamp_millis())
}

pub fn project_tally(members: &[&SessionSummary]) -> ProjectTally {
    let mut t = ProjectTally::default();
    for s in members {
        if s.state == SessionState::Errored {
            t.errored += 1;
        } else if s.needs_attention {
            t.wants_you += 1;
        } else if s.state == SessionState::Running {
            t.working += 1;
        }
    }
    t
}

/// Lifetime token total across members.
pub fn project_lifetime_tokens(members: &[&SessionSummary]) -> u64 {
    members.iter().map(|s| s.tokens.total()).sum()
}

pub fn primary_label(s: &SessionSummary) -> String {
    s.title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .unwrap_or_else(|| {
            let short = if s.id.len() > 8 { &s.id[..8] } else { &s.id };
            short.to_string()
        })
}

fn harness_label(s: &SessionSummary) -> String {
    let harness = if s.harness == "prime-agent" {
        "prime"
    } else {
        &s.harness
    };
    let mode = s.mode.as_deref().unwrap_or("");
    if mode.eq_ignore_ascii_case("headless") {
        format!("h:{harness}")
    } else {
        harness.to_string()
    }
}

fn short_model_label(model: &str) -> String {
    let base = model.rsplit('/').next().unwrap_or(model);
    // Drop trailing date stamps like -20241022
    let trimmed = base
        .rsplit_once('-')
        .filter(|(_, tail)| tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()))
        .map(|(head, _)| head)
        .unwrap_or(base);
    if trimmed.chars().count() > 18 {
        trimmed.chars().take(17).collect::<String>() + "…"
    } else {
        trimmed.to_string()
    }
}

fn identity_label(s: &SessionSummary) -> String {
    match s.model.as_deref() {
        Some(model) => {
            let mut label = short_model_label(model);
            if let Some(effort) = s.effort.as_deref() {
                label = format!("{label}·{effort}");
            }
            label
        }
        None => {
            let dir = s.worktree.as_deref().unwrap_or(&s.cwd);
            std::path::Path::new(dir)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(dir)
                .to_string()
        }
    }
}

fn format_token_count(n: u64) -> String {
    crate::lineage::format_token_count(n)
}

fn format_age_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn format_busy_ms(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}s", (ms / 1000).max(1))
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}h", ms / 3_600_000)
    }
}

fn context_pct(s: &SessionSummary) -> Option<(usize, u8)> {
    let used = s.context_used?;
    let window = s.context_window.filter(|w| *w > 0)?;
    let pct = ((used as f64 / window as f64) * 100.0).round() as u8;
    let filled = ((used as f64 / window as f64) * 4.0).round() as usize;
    Some((filled.min(4), pct.min(100)))
}

fn activity_cell(s: &SessionSummary, now_ms: i64) -> (String, bool) {
    if s.state == SessionState::Running {
        let since = s.busy_running_since_ms.unwrap_or(now_ms);
        let busy = format_busy_ms(now_ms.saturating_sub(since).max(0) as u64);
        (format!("busy {busy}"), true)
    } else {
        let age = s
            .last_message_at
            .or(s.last_event_at)
            .map(|at| {
                format_age_ms(now_ms.saturating_sub(at.timestamp_millis()).max(0) as u64)
            })
            .unwrap_or_else(|| "—".into());
        (format!("{age} ago"), false)
    }
}

fn truncate_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn dominant_cwd(members: &[&SessionSummary]) -> Option<String> {
    if members.is_empty() {
        return None;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for s in members {
        let dir = s.worktree.as_deref().unwrap_or(s.cwd.as_str());
        *counts.entry(dir).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(path, _)| path.to_string())
}

/// What a member card's content line says: the one fact about this session
/// the operator would otherwise open it to learn. Ordered by urgency —
/// a waiting approval beats everything, an error beats conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardLine {
    /// A tool call is waiting on the user: `approve? {tool · args}`.
    Approve(String),
    /// The session errored: `error: {message}`.
    Error(String),
    /// The session stopped while unwatched and its last words are a
    /// question/result waiting on the user: `asks: {text}`.
    Asks(String),
    /// Running, and the latest streamed assistant text: `now: {text}`.
    Now(String),
    /// Running on the user's last message (no assistant text yet this
    /// turn): `on: {text}`.
    On(String),
    /// Idle with the user's message as the last word: `you: {text}`.
    You(String),
    /// Idle assistant text already seen/answered: `last: {text}`.
    Last(String),
    /// Nothing to quote — soft placeholder.
    Quiet(&'static str),
}

/// Choose the content line for a member.
pub fn card_line(s: &SessionSummary, approvals: Option<&[PendingToolApproval]>) -> CardLine {
    if let Some(first) = approvals.and_then(|list| list.first()) {
        let mut text = first.tool.clone();
        if !first.args_summary.trim().is_empty() {
            text = format!("{} · {}", text, first.args_summary.trim());
        }
        let extra = approvals.map(|l| l.len()).unwrap_or(1).saturating_sub(1);
        if extra > 0 {
            text.push_str(&format!("  (+{extra} more)"));
        }
        return CardLine::Approve(text);
    }
    if s.state == SessionState::Errored {
        if let Some(e) = s
            .last_error
            .as_deref()
            .or(s.last_message.as_deref())
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return CardLine::Error(e.to_string());
        }
        return CardLine::Quiet("errored — open session for detail");
    }
    let msg = s
        .last_message
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if s.state == SessionState::Running {
        return match (s.last_message_role, msg) {
            (Some(MessageRole::Assistant), Some(m)) => CardLine::Now(m.to_string()),
            (_, Some(m)) => CardLine::On(m.to_string()),
            (_, None) => match s
                .last_prompt
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                Some(p) => CardLine::On(p.to_string()),
                None => CardLine::Quiet("working…"),
            },
        };
    }
    match (s.last_message_role, msg) {
        (Some(MessageRole::Assistant), Some(m)) if s.needs_attention => {
            CardLine::Asks(m.to_string())
        }
        (Some(MessageRole::User), Some(m)) => CardLine::You(m.to_string()),
        (_, Some(m)) => CardLine::Last(m.to_string()),
        (_, None) => CardLine::Quiet("no messages yet"),
    }
}

impl CardLine {
    fn label(&self) -> &'static str {
        match self {
            CardLine::Approve(_) => "approve?",
            CardLine::Error(_) => "error:",
            CardLine::Asks(_) => "asks:",
            CardLine::Now(_) => "now:",
            CardLine::On(_) => "on:",
            CardLine::You(_) => "you:",
            CardLine::Last(_) => "last:",
            CardLine::Quiet(_) => "",
        }
    }

    fn text(&self) -> &str {
        match self {
            CardLine::Approve(t)
            | CardLine::Error(t)
            | CardLine::Asks(t)
            | CardLine::Now(t)
            | CardLine::On(t)
            | CardLine::You(t)
            | CardLine::Last(t) => t,
            CardLine::Quiet(t) => t,
        }
    }

    fn label_style(&self, theme: &Theme) -> Style {
        match self {
            CardLine::Approve(_) => Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
            CardLine::Error(_) => Style::default().fg(theme.danger),
            CardLine::Asks(_) => Style::default().fg(theme.accent),
            CardLine::Now(_) | CardLine::On(_) => Style::default().fg(theme.success),
            CardLine::You(_) | CardLine::Last(_) | CardLine::Quiet(_) => {
                Style::default().fg(theme.dim)
            }
        }
    }

    fn text_style(&self, theme: &Theme) -> Style {
        match self {
            // Actionable content reads at full strength; ambient at dim.
            CardLine::Approve(_) | CardLine::Error(_) | CardLine::Asks(_) => {
                Style::default().fg(theme.text)
            }
            _ => Style::default().fg(theme.dim),
        }
    }
}

/// Render the full project dashboard into `area`.
#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    project_id: &str,
    members: &[&SessionSummary],
    approvals: &HashMap<String, Vec<PendingToolApproval>>,
    dashboard: &mut ProjectDashboard,
    interactive: bool,
    now: Instant,
    now_ms: i64,
) {
    dashboard.ensure_project(project_id);
    dashboard.clamp_cursor(members.len());
    dashboard.hits = ProjectDashboardHits::default();
    dashboard.meter_graph = None;

    if area.width == 0 || area.height == 0 {
        return;
    }

    let tally = project_tally(members);
    let lifetime = project_lifetime_tokens(members);
    let cwd = dominant_cwd(members);

    let mut row = area.y;
    let bottom = area.y.saturating_add(area.height);
    let x = area.x;
    let w = area.width;

    // ── Header ──────────────────────────────────────────────────────────
    // Project name lives on the main pane title bar (`project: {name}`);
    // this row is tally-only so we don't repeat identity one line below.
    if row < bottom {
        let mut spans = vec![Span::styled(
            format!(" {} ", members.len()),
            Style::default().fg(theme.dim),
        )];
        if tally.working > 0 {
            spans.push(Span::styled(
                format!("●{} ", tally.working),
                Style::default().fg(theme.success),
            ));
        }
        if tally.wants_you > 0 {
            spans.push(Span::styled(
                format!("·{} ", tally.wants_you),
                Style::default().fg(theme.accent),
            ));
        }
        if tally.errored > 0 {
            spans.push(Span::styled(
                format!("✗{} ", tally.errored),
                Style::default().fg(theme.danger),
            ));
        }
        if lifetime > 0 {
            spans.push(Span::styled(
                format!("· {} tok ", format_token_count(lifetime)),
                Style::default().fg(theme.dim),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { x, y: row, width: w, height: 1 });
        row = row.saturating_add(1);
    }

    if row < bottom {
        let mut meta = String::new();
        if let Some(cwd) = cwd.as_deref() {
            let short = truncate_width(cwd, (w as usize).saturating_sub(4).max(8));
            meta.push_str(&short);
        }
        if members.is_empty() {
            if !meta.is_empty() {
                meta.push_str("  ·  ");
            }
            meta.push_str("empty — create a session while this project is selected");
        }
        if !meta.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(meta, Style::default().fg(theme.dim))),
                Rect {
                    x,
                    y: row,
                    width: w,
                    height: 1,
                },
            );
            row = row.saturating_add(1);
        }
    }

    // ── Token meter ─────────────────────────────────────────────────────
    let meter = dashboard.token_meters.get_mut(project_id);
    let show_meter = area.height >= 12 && w >= 24;
    // Recorded after the meter's own borrow ends, so the hover detail can map
    // a pointer back to a bucket on the next frame.
    let mut meter_graph = None;
    if show_meter {
        if let Some(meter) = meter {
            meter.advance_to(now);
            let meter_h =
                METER_HEIGHT.min(bottom.saturating_sub(row).saturating_sub(CARDS_MIN_HEIGHT));
            if meter_h >= 2 {
                let meter_area = Rect {
                    x,
                    y: row,
                    width: w,
                    height: meter_h,
                };
                meter_graph = render_token_meter(f, meter_area, theme, meter, now);
                row = row.saturating_add(meter_h);
            }
        } else if row < bottom {
            f.render_widget(
                Paragraph::new(Span::styled(
                    " no token usage reported yet for this project ",
                    Style::default().fg(theme.dim),
                )),
                Rect {
                    x,
                    y: row,
                    width: w,
                    height: 1,
                },
            );
            row = row.saturating_add(1);
        }
    }
    dashboard.meter_graph = meter_graph.map(|graph| (project_id.to_string(), graph));

    // Gap before the cards.
    if row < bottom {
        row = row.saturating_add(1);
    }

    // ── Member cards ────────────────────────────────────────────────────
    let cards_area = Rect {
        x,
        y: row,
        width: w,
        height: bottom.saturating_sub(row),
    };
    render_members(
        f,
        cards_area,
        theme,
        members,
        approvals,
        dashboard,
        interactive,
        now_ms,
    );
}

/// Returns the rect the columns occupy, so the caller can record it as the
/// hover-detail hit zone (the legend row below them is not part of it).
pub(crate) fn render_token_meter(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    meter: &TokenMeter,
    _now: Instant,
) -> Option<Rect> {
    let dim = Style::default().fg(theme.dim);
    if meter.is_idle() {
        f.render_widget(
            Paragraph::new(Span::styled(" no token usage reported yet ", dim)),
            area,
        );
        return None;
    }

    let graph_h = area.height.saturating_sub(1).max(1);
    let graph = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: graph_h,
    };
    let width = graph.width as usize;
    let scale = meter.scale(width).max(1);
    let cells = graph.height as usize;
    let eighths_total = cells * 8;
    let history: Vec<_> = meter.window(width).collect();
    let hist_len = history.len();

    // Same column paint as the fleet token meter (#1183): full cells as
    // background fill so fonts whose FULL BLOCK is short of the line box
    // don't leave hairline seams between rows.
    for (col, bucket) in history.iter().enumerate() {
        let x = graph.x + (width - hist_len + col) as u16;
        let total = bucket.total();
        if total == 0 {
            continue;
        }
        let filled = ((total as f64 / scale as f64) * eighths_total as f64).round() as usize;
        let filled = filled.clamp(1, eighths_total);
        let segments = token_meter::stacked_eighths(&bucket.stacked(), total, filled);
        for cell in token_meter::column_cells(&segments, filled, cells) {
            let y = graph.y + graph.height.saturating_sub(cell.row + 1);
            let mut style = Style::default().fg(meter.band_color(cell.fg));
            if let Some(bg) = cell.bg {
                style = style.bg(meter.band_color(bg));
            }
            f.buffer_mut().set_string(x, y, cell.glyph, style);
        }
    }

    // Legend / rate line.
    let legend_y = area.y.saturating_add(area.height.saturating_sub(1));
    let entries = meter.legend(width.min(area.width as usize));
    let rate = meter
        .recent_fleet_rate()
        .map(|r| format!("{:.0}/s", r))
        .unwrap_or_else(|| "—".into());
    let total = format_token_count(meter.window_total(width));
    let mut legend = format!(" {total} tok · {rate}");
    for e in entries.iter().take(3) {
        legend.push_str(&format!("  {} {}", "●", e.label));
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(&legend, area.width as usize),
            dim,
        )),
        Rect {
            x: area.x,
            y: legend_y,
            width: area.width,
            height: 1,
        },
    );
    Some(graph)
}

/// Rows one member card occupies given the pane's shape: 1 (title only) on
/// cramped panes, 2 (title + content), or 3 (a breathing row between cards)
/// when every member fits airily.
fn card_height(area: Rect, member_count: usize) -> usize {
    if area.width < 40 || (area.height as usize) < 2 {
        return 1;
    }
    if member_count > 0 && (area.height as usize) >= member_count * 3 {
        return 3;
    }
    2
}

#[allow(clippy::too_many_arguments)]
fn render_members(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    members: &[&SessionSummary],
    approvals: &HashMap<String, Vec<PendingToolApproval>>,
    dashboard: &mut ProjectDashboard,
    interactive: bool,
    now_ms: i64,
) {
    if area.height == 0 {
        return;
    }
    if members.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  (no sessions — new ones inherit this project)",
                Style::default().fg(theme.dim),
            )),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        return;
    }

    let visible_h = area.height as usize;
    let row_h = card_height(area, members.len());
    let page = (visible_h / row_h).max(1);

    // Keep cursor visible.
    if interactive {
        if dashboard.cursor < dashboard.member_scroll {
            dashboard.member_scroll = dashboard.cursor;
        } else if dashboard.cursor >= dashboard.member_scroll + page {
            dashboard.member_scroll = dashboard.cursor + 1 - page;
        }
    }
    let max_scroll = members.len().saturating_sub(page);
    dashboard.member_scroll = dashboard.member_scroll.min(max_scroll);

    let mut row = area.y;
    let end = (dashboard.member_scroll + page).min(members.len());
    for (i, s) in members[dashboard.member_scroll..end].iter().enumerate() {
        let idx = dashboard.member_scroll + i;
        if row >= area.bottom() {
            break;
        }
        let selected = interactive && idx == dashboard.cursor;
        let hovered = dashboard.hover_session.as_deref() == Some(s.id.as_str());
        let mark = if selected {
            "›"
        } else if hovered {
            "·"
        } else {
            " "
        };

        // ── Title row: marks + name, right-aligned meta ──────────────────
        let glyph = s.state.glyph();
        let attention = if s.needs_attention { "·" } else { " " };
        let (act, busy) = activity_cell(s, now_ms);

        // Meta parts, least important first — dropped from the left until
        // the row fits alongside a readable title.
        let mut parts: Vec<String> = Vec::new();
        if !s.tokens.is_zero() {
            parts.push(format!("{} tok", format_token_count(s.tokens.total())));
        }
        if let Some((filled, pct)) = context_pct(s) {
            parts.push(format!(
                "{}{} {pct}%",
                "▰".repeat(filled),
                "▱".repeat(4usize.saturating_sub(filled))
            ));
        }
        parts.push(identity_label(s));
        parts.push(harness_label(s));
        let marks_w = 4usize; // "{mark}{attention}{glyph} "
        let width = area.width as usize;
        let title_min = 16usize.min(width.saturating_sub(marks_w));
        let act_w = UnicodeWidthStr::width(act.as_str());
        loop {
            let meta_w = parts
                .iter()
                .map(|p| UnicodeWidthStr::width(p.as_str()) + 3)
                .sum::<usize>()
                + act_w;
            if parts.is_empty() || marks_w + title_min + 2 + meta_w <= width {
                break;
            }
            parts.remove(0);
        }
        let meta_dim = parts.join(" · ");
        let meta_w = if meta_dim.is_empty() {
            act_w
        } else {
            UnicodeWidthStr::width(meta_dim.as_str()) + 3 + act_w
        };
        let name = truncate_width(
            &primary_label(s),
            width.saturating_sub(marks_w + meta_w + 2).max(4),
        );

        let mut spans = vec![
            Span::styled(
                format!("{mark}{attention}{glyph} "),
                state_style(theme, s.state),
            ),
            Span::styled(
                name,
                if selected {
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                },
            ),
        ];
        let used = spans
            .iter()
            .map(|sp| UnicodeWidthStr::width(sp.content.as_ref()))
            .sum::<usize>();
        let pad = width.saturating_sub(used + meta_w);
        spans.push(Span::raw(" ".repeat(pad)));
        if !meta_dim.is_empty() {
            spans.push(Span::styled(
                format!("{meta_dim} · "),
                Style::default().fg(theme.dim),
            ));
        }
        spans.push(Span::styled(
            act,
            if busy {
                Style::default().fg(theme.success)
            } else {
                Style::default().fg(theme.dim)
            },
        ));

        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: 1,
            },
        );
        dashboard.hits.member_rows.push(ProjectRowHit {
            area: Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: row_h.min(2) as u16,
            },
            session_id: s.id.clone(),
        });
        row = row.saturating_add(1);

        // ── Content row: what the session is doing / asking ──────────────
        if row_h >= 2 && row < area.bottom() {
            let line = card_line(s, approvals.get(&s.id).map(Vec::as_slice));
            let label = line.label();
            let text = line.text().replace('\n', " ");
            let prefix = if label.is_empty() {
                "    └ ".to_string()
            } else {
                format!("    └ {label} ")
            };
            let text = truncate_width(
                &text,
                (area.width as usize)
                    .saturating_sub(UnicodeWidthStr::width(prefix.as_str()))
                    .max(4),
            );
            let spans = vec![
                Span::styled(prefix, line.label_style(theme)),
                Span::styled(text, line.text_style(theme)),
            ];
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: 1,
                },
            );
            row = row.saturating_add(1);
        }

        // Breathing row between cards in the airy layout.
        if row_h >= 3 {
            row = row.saturating_add(1);
        }
    }
}

fn state_style(theme: &Theme, state: SessionState) -> Style {
    match state {
        SessionState::Running => Style::default().fg(theme.success),
        SessionState::AwaitingInput => Style::default().fg(theme.success),
        SessionState::Errored => Style::default().fg(theme.danger),
        SessionState::Done => Style::default().fg(theme.dim),
        SessionState::Paused => Style::default().fg(theme.warning),
        SessionState::Pending => Style::default().fg(theme.dim),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use construct_protocol::{SessionKind, SessionSummary, TokenTally, ToolRisk};

    fn session(
        id: &str,
        project: Option<&str>,
        state: SessionState,
        attention: bool,
    ) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            harness: "smith".into(),
            cwd: "/tmp".into(),
            title: Some(id.into()),
            auto_title_pending: false,
            state,
            created_at: Utc::now(),
            last_event_at: Some(Utc::now()),
            last_message_at: Some(Utc::now()),
            cost_usd: None,
            model: Some("gpt-test".into()),
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count: 0,
            has_pty: true,
            mode: Some("interactive".into()),
            pinned: false,
            position: 0,
            group_id: project.map(str::to_string),
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: TokenTally::default(),
            context_used: Some(50),
            context_window: Some(100),
            context_segments: Vec::new(),
            approval_mode: Default::default(),
            kind: SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: attention,
            forked_from: None,
            merge: None,
        }
    }

    #[test]
    fn prime_agent_uses_compact_dashboard_label() {
        let mut interactive = session("s1", None, SessionState::Running, false);
        interactive.harness = "prime-agent".into();
        assert_eq!(harness_label(&interactive), "prime");

        interactive.mode = Some("headless".into());
        assert_eq!(harness_label(&interactive), "h:prime");
    }

    #[test]
    fn sort_puts_attention_and_errors_first() {
        let a = session("idle", Some("p"), SessionState::AwaitingInput, false);
        let b = session("run", Some("p"), SessionState::Running, false);
        let c = session("need", Some("p"), SessionState::AwaitingInput, true);
        let d = session("err", Some("p"), SessionState::Errored, false);
        let mut members = vec![&a, &b, &c, &d];
        sort_members(&mut members);
        assert_eq!(members[0].id, "err");
        assert_eq!(members[1].id, "need");
        assert_eq!(members[2].id, "run");
        assert_eq!(members[3].id, "idle");
    }

    #[test]
    fn tally_is_ranked_and_disjoint() {
        let a = session("a", Some("p"), SessionState::Errored, true); // errored wins
        let b = session("b", Some("p"), SessionState::Running, true); // wants you
        let c = session("c", Some("p"), SessionState::Running, false);
        let d = session("d", Some("p"), SessionState::AwaitingInput, false);
        let members = vec![&a, &b, &c, &d];
        let t = project_tally(&members);
        assert_eq!(t.errored, 1);
        assert_eq!(t.wants_you, 1);
        assert_eq!(t.working, 1);
    }

    #[test]
    fn project_members_excludes_archived_and_native() {
        let live = session("live", Some("p"), SessionState::Running, false);
        let mut arch = session("arch", Some("p"), SessionState::Done, false);
        arch.archived = true;
        let mut native = session("nat", Some("p"), SessionState::Running, false);
        native.native_subagent = Some(construct_protocol::NativeSubagentRef {
            owner_session_id: "live".into(),
            native_id: "x".into(),
            projected_seq: 0,
        });
        let other = session("other", Some("q"), SessionState::Running, false);
        let sessions = vec![live.clone(), arch, native, other];
        let members = project_members(&sessions, "p");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id, "live");
        let _ = live;
    }

    /// The content line answers "what would I learn by opening this
    /// session" in urgency order: a waiting approval preempts everything,
    /// an error preempts conversation, and only an *unwatched* stop turns
    /// assistant text into an "asks" — idle interactive sessions the
    /// operator already saw stay at "last".
    #[test]
    fn card_line_ranks_approval_over_error_over_conversation() {
        let mut s = session("s", Some("p"), SessionState::Errored, false);
        s.last_error = Some("cargo test exited 101".into());
        s.last_message_role = Some(MessageRole::Assistant);
        s.last_message = Some("I'll rerun the suite.".into());

        let approvals = vec![PendingToolApproval {
            call_id: "c1".into(),
            tool: "bash".into(),
            args_summary: "rm -rf target".into(),
            risk: ToolRisk::Risky,
        }];
        assert_eq!(
            card_line(&s, Some(&approvals)),
            CardLine::Approve("bash · rm -rf target".into())
        );
        assert_eq!(
            card_line(&s, None),
            CardLine::Error("cargo test exited 101".into())
        );

        s.state = SessionState::AwaitingInput;
        s.needs_attention = true;
        assert_eq!(
            card_line(&s, None),
            CardLine::Asks("I'll rerun the suite.".into())
        );
        s.needs_attention = false;
        assert_eq!(
            card_line(&s, None),
            CardLine::Last("I'll rerun the suite.".into())
        );
    }

    /// Running cards distinguish "the agent is talking" (now:) from "the
    /// agent is chewing on the user's message" (on:), and fall back to the
    /// spawn prompt for headless sessions that haven't spoken yet.
    #[test]
    fn card_line_running_prefers_assistant_then_user_then_prompt() {
        let mut s = session("s", Some("p"), SessionState::Running, false);
        s.last_message_role = Some(MessageRole::Assistant);
        s.last_message = Some("Editing token_meter.rs".into());
        assert_eq!(
            card_line(&s, None),
            CardLine::Now("Editing token_meter.rs".into())
        );

        s.last_message_role = Some(MessageRole::User);
        s.last_message = Some("fix the flaky test".into());
        assert_eq!(
            card_line(&s, None),
            CardLine::On("fix the flaky test".into())
        );

        s.last_message = None;
        s.last_message_role = None;
        s.last_prompt = Some("audit the daemon".into());
        assert_eq!(card_line(&s, None), CardLine::On("audit the daemon".into()));

        s.last_prompt = None;
        assert_eq!(card_line(&s, None), CardLine::Quiet("working…"));
    }

    /// The dashboard's meter records the rect its columns occupy so the hover
    /// detail can map a pointer back to a bucket, and records nothing on a
    /// frame that drew no columns — a stale rect would keep answering hovers
    /// over whatever replaced it.
    #[test]
    fn the_meter_records_its_graph_rect_only_while_it_draws_one() {
        let live = session("live", Some("p"), SessionState::Running, false);
        let members = vec![&live];
        let mut dash = ProjectDashboard::default();
        let theme = Theme::default();
        let now = Instant::now();
        let now_ms = Utc::now().timestamp_millis();
        let approvals = HashMap::new();
        let area = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 30,
        };
        let draw = |dash: &mut ProjectDashboard| {
            let backend = ratatui::backend::TestBackend::new(area.width, area.height);
            let mut term = ratatui::Terminal::new(backend).expect("terminal");
            term.draw(|f| {
                render(
                    f, area, &theme, "p", &members, &approvals, dash, true, now, now_ms,
                )
            })
            .expect("draw");
        };

        // No meter for this project yet: nothing to hover.
        draw(&mut dash);
        assert_eq!(dash.meter_graph, None);

        let mut meter = TokenMeter::new(now);
        meter.observe(Some("claude-opus-5"), 12_000, 4_000, now);
        dash.token_meters.insert("p".into(), meter);
        draw(&mut dash);
        let (project, graph) = dash.meter_graph.clone().expect("the meter drew columns");
        assert_eq!(project, "p");
        assert!(graph.width > 0 && graph.height > 0, "{graph:?}");
        assert!(
            graph.y + graph.height < area.y + area.height,
            "the graph rect stops above the legend row: {graph:?}"
        );

        // A pane too short for a meter draws none, and clears the rect.
        let short = Rect { height: 10, ..area };
        let backend = ratatui::backend::TestBackend::new(short.width, short.height);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            render(
                f, short, &theme, "p", &members, &approvals, &mut dash, true, now, now_ms,
            )
        })
        .expect("draw");
        assert_eq!(dash.meter_graph, None);
    }

    /// Member cards paint a content line under each title and register one
    /// hit zone per member covering both rows.
    #[test]
    fn cards_render_content_lines_with_hit_zones() {
        let mut ask = session("wire slack", Some("p"), SessionState::AwaitingInput, true);
        ask.last_message_role = Some(MessageRole::Assistant);
        ask.last_message = Some("Should replies thread under the original?".into());
        let mut run = session("meter", Some("p"), SessionState::Running, false);
        run.last_message_role = Some(MessageRole::Assistant);
        run.last_message = Some("Editing token_meter.rs".into());
        let members = vec![&ask, &run];
        let mut dash = ProjectDashboard::default();
        let theme = Theme::default();
        let approvals = HashMap::new();
        let area = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 24,
        };
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            render(
                f,
                area,
                &theme,
                "p",
                &members,
                &approvals,
                &mut dash,
                true,
                Instant::now(),
                Utc::now().timestamp_millis(),
            )
        })
        .expect("draw");
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            text.push('\n');
        }
        assert!(text.contains("asks:"), "{text}");
        assert!(
            text.contains("Should replies thread under the original?"),
            "{text}"
        );
        assert!(text.contains("now:"), "{text}");
        assert!(text.contains("Editing token_meter.rs"), "{text}");
        assert_eq!(dash.hits.member_rows.len(), 2);
        assert!(dash.hits.member_rows.iter().all(|h| h.area.height == 2));
    }
}
