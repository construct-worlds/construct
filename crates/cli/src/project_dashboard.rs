//! Project dashboard pane (view area when a project header is selected).
//!
//! Turns the previously passive "flat member list" into a live project
//! console: tally, member roster with full-mode detail, activity feed,
//! project-scoped token meter, and a peek preview of the hottest session.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use chrono::{DateTime, Utc};
use construct_protocol::{SessionState, SessionSummary};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;
use crate::token_meter::TokenMeter;

/// Max activity lines retained per project (client ring buffer).
pub const ACTIVITY_CAP: usize = 40;

/// Max chat messages retained per session for the preview strip.
pub const PREVIEW_MESSAGE_CAP: usize = 6;

/// Max messages painted in the preview body.
pub const PREVIEW_SHOW: usize = 3;

/// Compact meter height when the pane is tall enough.
const METER_HEIGHT: u16 = 4;

/// Preview block height (title + chrome + body lines).
const PREVIEW_MIN_HEIGHT: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Running,
    WantsYou,
    Done,
    Errored,
    Created,
    Other,
}

impl ActivityKind {
    pub fn glyph(self) -> &'static str {
        match self {
            ActivityKind::Running => "●",
            ActivityKind::WantsYou => "·",
            ActivityKind::Done => "✓",
            ActivityKind::Errored => "✗",
            ActivityKind::Created => "+",
            ActivityKind::Other => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ActivityKind::Running => "running",
            ActivityKind::WantsYou => "wants you",
            ActivityKind::Done => "done",
            ActivityKind::Errored => "errored",
            ActivityKind::Created => "created",
            ActivityKind::Other => "updated",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub at: DateTime<Utc>,
    pub session_id: String,
    pub label: String,
    pub kind: ActivityKind,
}

#[derive(Debug, Clone)]
pub struct PreviewMessage {
    pub role: PreviewRole,
    pub text: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRole {
    User,
    Assistant,
    Other,
}

/// Hit zones painted last frame for the project dashboard.
#[derive(Debug, Clone, Default)]
pub struct ProjectDashboardHits {
    pub member_rows: Vec<ProjectRowHit>,
    pub feed_rows: Vec<ProjectRowHit>,
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
#[derive(Debug)]
pub struct ProjectDashboard {
    /// Cursor into the sorted member list of the currently selected project.
    pub cursor: usize,
    /// Rows scrolled off the top of the member list.
    pub member_scroll: usize,
    /// Rows scrolled off the top of the activity feed.
    pub feed_scroll: usize,
    /// Project id the cursor/scroll state applies to (reset on switch).
    pub active_project: Option<String>,
    /// Hovered member (mouse) — retargets preview without selecting.
    pub hover_session: Option<String>,
    /// project_id → newest-first activity.
    pub activity: HashMap<String, VecDeque<ActivityEntry>>,
    /// session_id → recent chat messages for preview body.
    pub preview_messages: HashMap<String, VecDeque<PreviewMessage>>,
    /// project_id → live token meter (fed from Cost events).
    pub token_meters: HashMap<String, TokenMeter>,
    /// session_id → last (state, needs_attention) observed for feed diffs.
    pub last_seen: HashMap<String, (SessionState, bool)>,
    /// Hit zones from the last render.
    pub hits: ProjectDashboardHits,
}

impl Default for ProjectDashboard {
    fn default() -> Self {
        Self {
            cursor: 0,
            member_scroll: 0,
            feed_scroll: 0,
            active_project: None,
            hover_session: None,
            activity: HashMap::new(),
            preview_messages: HashMap::new(),
            token_meters: HashMap::new(),
            last_seen: HashMap::new(),
            hits: ProjectDashboardHits::default(),
        }
    }
}

impl ProjectDashboard {
    /// Seed transition baselines from the current session list so the first
    /// real state change after attach produces a feed line (not every
    /// session looking newly created).
    pub fn seed_from_sessions(&mut self, sessions: &[SessionSummary]) {
        for s in sessions {
            self.last_seen
                .entry(s.id.clone())
                .or_insert((s.state, s.needs_attention));
        }
    }

    /// Keep cursor/scroll coherent when the selected project changes.
    pub fn ensure_project(&mut self, project_id: &str) {
        if self.active_project.as_deref() != Some(project_id) {
            self.active_project = Some(project_id.to_string());
            self.cursor = 0;
            self.member_scroll = 0;
            self.feed_scroll = 0;
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

    /// Record a state transition into the project's activity feed.
    pub fn observe_session_state(&mut self, session: &SessionSummary, now: DateTime<Utc>) {
        let Some(project_id) = session.group_id.as_deref() else {
            self.last_seen
                .insert(session.id.clone(), (session.state, session.needs_attention));
            return;
        };
        // Native harness mirrors stay out of the scan (spec 0079 / 0169).
        if session.native_subagent.is_some() {
            return;
        }
        if session.archived {
            self.last_seen
                .insert(session.id.clone(), (session.state, session.needs_attention));
            return;
        }

        let prev = self.last_seen.insert(
            session.id.clone(),
            (session.state, session.needs_attention),
        );
        // First sighting only seeds the baseline so reconnect / list hydration
        // does not flood the feed with synthetic "created" lines for every
        // pre-existing member. Real creates call `note_session_created`.
        let Some((prev_state, prev_attention)) = prev else {
            return;
        };
        let label = primary_label(session);
        let entry = if session.state == SessionState::Errored && prev_state != SessionState::Errored
        {
            Some(ActivityKind::Errored)
        } else if session.needs_attention && !prev_attention {
            Some(ActivityKind::WantsYou)
        } else if session.state == SessionState::Running && prev_state != SessionState::Running {
            Some(ActivityKind::Running)
        } else if session.state == SessionState::Done && prev_state != SessionState::Done {
            Some(ActivityKind::Done)
        } else if session.state != prev_state {
            Some(ActivityKind::Other)
        } else {
            None
        }
        .map(|kind| ActivityEntry {
            at: now,
            session_id: session.id.clone(),
            label,
            kind,
        });
        if let Some(entry) = entry {
            let feed = self
                .activity
                .entry(project_id.to_string())
                .or_insert_with(VecDeque::new);
            feed.push_front(entry);
            while feed.len() > ACTIVITY_CAP {
                feed.pop_back();
            }
        }
    }

    /// Record that a session was newly created in a project (not a reconnect seed).
    pub fn note_session_created(&mut self, session: &SessionSummary, now: DateTime<Utc>) {
        let Some(project_id) = session.group_id.as_deref() else {
            return;
        };
        if session.native_subagent.is_some() || session.archived {
            return;
        }
        self.last_seen
            .insert(session.id.clone(), (session.state, session.needs_attention));
        let feed = self
            .activity
            .entry(project_id.to_string())
            .or_insert_with(VecDeque::new);
        feed.push_front(ActivityEntry {
            at: now,
            session_id: session.id.clone(),
            label: primary_label(session),
            kind: ActivityKind::Created,
        });
        while feed.len() > ACTIVITY_CAP {
            feed.pop_back();
        }
    }

    pub fn observe_message(
        &mut self,
        session_id: &str,
        role: PreviewRole,
        text: &str,
        at: DateTime<Utc>,
    ) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // Coalesce streaming assistant deltas into one bubble.
        let feed = self
            .preview_messages
            .entry(session_id.to_string())
            .or_insert_with(VecDeque::new);
        if role == PreviewRole::Assistant {
            if let Some(last) = feed.back_mut() {
                if last.role == PreviewRole::Assistant {
                    last.text.push_str(trimmed);
                    last.at = at;
                    return;
                }
            }
        }
        // Cap individual message size so a huge dump doesn't bloat the pane.
        let text = if trimmed.chars().count() > 400 {
            let mut s: String = trimmed.chars().take(400).collect();
            s.push('…');
            s
        } else {
            trimmed.to_string()
        };
        feed.push_back(PreviewMessage { role, text, at });
        while feed.len() > PREVIEW_MESSAGE_CAP {
            feed.pop_front();
        }
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
        self.last_seen.remove(session_id);
        self.preview_messages.remove(session_id);
        if self.hover_session.as_deref() == Some(session_id) {
            self.hover_session = None;
        }
        for feed in self.activity.values_mut() {
            feed.retain(|e| e.session_id != session_id);
        }
    }

    pub fn forget_project(&mut self, project_id: &str) {
        self.activity.remove(project_id);
        self.token_meters.remove(project_id);
        if self.active_project.as_deref() == Some(project_id) {
            self.active_project = None;
            self.cursor = 0;
            self.member_scroll = 0;
            self.feed_scroll = 0;
            self.hover_session = None;
        }
    }

    pub fn hit_session_at(&self, col: u16, row: u16) -> Option<&str> {
        self.hits
            .member_rows
            .iter()
            .chain(self.hits.feed_rows.iter())
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

/// Pick the session the preview should show.
///
/// Order: hover → cursor (when interactive) → hottest by rank/recency.
pub fn preview_target_id<'a>(
    members: &[&'a SessionSummary],
    cursor: usize,
    hover: Option<&str>,
    interactive: bool,
) -> Option<&'a str> {
    if members.is_empty() {
        return None;
    }
    if let Some(h) = hover {
        if let Some(s) = members.iter().find(|s| s.id == h) {
            return Some(s.id.as_str());
        }
    }
    if interactive {
        return members.get(cursor).map(|s| s.id.as_str());
    }
    // Hottest first — members are already sorted that way.
    members.first().map(|s| s.id.as_str())
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
    let mode = s.mode.as_deref().unwrap_or("");
    if mode.eq_ignore_ascii_case("headless") {
        format!("h:{}", s.harness)
    } else {
        s.harness.clone()
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

/// Render the full project dashboard into `area`.
pub fn render(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    group_name: &str,
    project_id: &str,
    members: &[&SessionSummary],
    dashboard: &mut ProjectDashboard,
    interactive: bool,
    now: Instant,
    now_ms: i64,
) {
    dashboard.ensure_project(project_id);
    dashboard.clamp_cursor(members.len());
    dashboard.hits = ProjectDashboardHits::default();

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
    if row < bottom {
        let mut spans = vec![Span::styled(
            format!(" Project: {group_name} "),
            Style::default()
                .fg(theme.group)
                .add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::styled(
            format!("· {} ", members.len()),
            Style::default().fg(theme.dim),
        ));
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

    // ── Token meter (C2) ────────────────────────────────────────────────
    let meter = dashboard.token_meters.get_mut(project_id);
    let show_meter = area.height >= 12 && w >= 24;
    if show_meter {
        if let Some(meter) = meter {
            meter.advance_to(now);
            let meter_h = METER_HEIGHT.min(bottom.saturating_sub(row).saturating_sub(PREVIEW_MIN_HEIGHT));
            if meter_h >= 2 {
                let meter_area = Rect {
                    x,
                    y: row,
                    width: w,
                    height: meter_h,
                };
                render_project_meter(f, meter_area, theme, meter, now);
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

    // Gap before body columns.
    if row < bottom {
        row = row.saturating_add(1);
    }

    // Reserve preview at the bottom.
    let preview_h = if bottom.saturating_sub(row) >= PREVIEW_MIN_HEIGHT + 3 {
        PREVIEW_MIN_HEIGHT
            .max(4)
            .min(8)
            .min(bottom.saturating_sub(row).saturating_sub(3))
    } else {
        0
    };
    let body_bottom = bottom.saturating_sub(preview_h);
    let body_h = body_bottom.saturating_sub(row);

    // ── Members | Activity (C0 + C1) ─────────────────────────────────────
    if body_h > 0 && w > 0 {
        let split = if w >= 60 {
            let left_w = (w * 3 / 5).max(28).min(w.saturating_sub(22));
            (left_w, w.saturating_sub(left_w).saturating_sub(1))
        } else {
            (w, 0)
        };

        let members_area = Rect {
            x,
            y: row,
            width: split.0,
            height: body_h,
        };
        render_members(
            f,
            members_area,
            theme,
            members,
            dashboard,
            interactive,
            now_ms,
        );

        if split.1 >= 18 {
            let feed_area = Rect {
                x: x.saturating_add(split.0).saturating_add(1),
                y: row,
                width: split.1,
                height: body_h,
            };
            // Vertical divider
            for dy in 0..body_h {
                f.buffer_mut().set_string(
                    x.saturating_add(split.0),
                    row.saturating_add(dy),
                    "│",
                    Style::default().fg(theme.dim),
                );
            }
            render_activity_feed(f, feed_area, theme, project_id, dashboard, now_ms);
        }
    }

    // ── Preview (C0 chrome + C3 body) ───────────────────────────────────
    if preview_h > 0 {
        let preview_area = Rect {
            x,
            y: body_bottom,
            width: w,
            height: preview_h,
        };
        let target = preview_target_id(
            members,
            dashboard.cursor,
            dashboard.hover_session.as_deref(),
            interactive,
        );
        render_preview(f, preview_area, theme, members, target, dashboard, now_ms);
    }
}

fn render_project_meter(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    meter: &TokenMeter,
    _now: Instant,
) {
    let dim = Style::default().fg(theme.dim);
    if meter.is_idle() {
        f.render_widget(
            Paragraph::new(Span::styled(" no token usage reported yet ", dim)),
            area,
        );
        return;
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

    for (col, bucket) in history.iter().enumerate() {
        let x = graph.x + (width - hist_len + col) as u16;
        let total = bucket.total();
        if total == 0 {
            continue;
        }
        let filled = ((total as f64 / scale as f64) * eighths_total as f64).round() as usize;
        let filled = filled.clamp(1, eighths_total);
        // Simplified single-series paint (model colors still applied per band).
        let stacked = bucket.stacked();
        let mut remaining = filled;
        let mut y_eighth = 0usize;
        for (band, tokens) in stacked.iter().rev() {
            if remaining == 0 {
                break;
            }
            let share = ((*tokens as f64 / total as f64) * filled as f64).round() as usize;
            let take = share.min(remaining).max(if remaining > 0 && *tokens > 0 {
                1
            } else {
                0
            });
            let take = take.min(remaining);
            for _ in 0..take {
                let cell_row = y_eighth / 8;
                let eighth = y_eighth % 8;
                if cell_row < cells {
                    let y = graph.y + graph.height.saturating_sub(cell_row as u16 + 1);
                    let glyph = match eighth {
                        0 => "▁",
                        1 => "▂",
                        2 => "▃",
                        3 => "▄",
                        4 => "▅",
                        5 => "▆",
                        6 => "▇",
                        _ => "█",
                    };
                    // Only the top partial of a band uses partial glyphs; solid for full cells.
                    let g = if eighth == 7 || take > 1 { "█" } else { glyph };
                    f.buffer_mut()
                        .set_string(x, y, g, Style::default().fg(meter.band_color(*band)));
                }
                y_eighth += 1;
            }
            remaining = remaining.saturating_sub(take);
        }
        let _ = remaining;
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
}

fn render_members(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    members: &[&SessionSummary],
    dashboard: &mut ProjectDashboard,
    interactive: bool,
    now_ms: i64,
) {
    if area.height == 0 {
        return;
    }
    let mut row = area.y;
    f.render_widget(
        Paragraph::new(Span::styled(
            " members ",
            Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::BOLD),
        )),
        Rect {
            x: area.x,
            y: row,
            width: area.width,
            height: 1,
        },
    );
    row = row.saturating_add(1);

    if members.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  (no sessions — new ones inherit this project)",
                Style::default().fg(theme.dim),
            )),
            Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: 1,
            },
        );
        return;
    }

    let visible_h = area.bottom().saturating_sub(row) as usize;
    // Two rows per member when width allows detail.
    let row_h: usize = if area.width >= 40 && visible_h >= 4 { 2 } else { 1 };
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

        let glyph = s.state.glyph();
        let attention = if s.needs_attention { "·" } else { " " };
        let name = truncate_width(&primary_label(s), (area.width as usize).saturating_sub(18).max(4));
        let harness = harness_label(s);

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
        let used = spans.iter().map(|sp| UnicodeWidthStr::width(sp.content.as_ref())).sum::<usize>();
        let harness_w = UnicodeWidthStr::width(harness.as_str());
        let pad = (area.width as usize).saturating_sub(used + harness_w + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(
            harness,
            Style::default().fg(theme.dim),
        ));

        let line_area = Rect {
            x: area.x,
            y: row,
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(spans)), line_area);
        dashboard.hits.member_rows.push(ProjectRowHit {
            area: Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: row_h as u16,
            },
            session_id: s.id.clone(),
        });
        row = row.saturating_add(1);

        if row_h == 2 && row < area.bottom() {
            let (act, busy) = activity_cell(s, now_ms);
            let mut detail = String::new();
            if let Some((filled, pct)) = context_pct(s) {
                detail.push_str(&"▰".repeat(filled));
                detail.push_str(&"▱".repeat(4usize.saturating_sub(filled)));
                detail.push_str(&format!(" {pct}%  "));
            }
            detail.push_str(&act);
            if !s.tokens.is_zero() {
                detail.push_str(&format!("  {} tok", format_token_count(s.tokens.total())));
            }
            let id = identity_label(s);
            let detail_w = (area.width as usize).saturating_sub(4);
            let left = truncate_width(&detail, detail_w.saturating_sub(UnicodeWidthStr::width(id.as_str()) + 2));
            let pad = detail_w
                .saturating_sub(UnicodeWidthStr::width(left.as_str()))
                .saturating_sub(UnicodeWidthStr::width(id.as_str()));
            let line = format!("    {left}{}{id}", " ".repeat(pad));
            f.render_widget(
                Paragraph::new(Span::styled(
                    truncate_width(&line, area.width as usize),
                    if busy {
                        Style::default().fg(theme.success)
                    } else {
                        Style::default().fg(theme.dim)
                    },
                )),
                Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: 1,
                },
            );
            row = row.saturating_add(1);
        }
    }
}

fn render_activity_feed(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    project_id: &str,
    dashboard: &mut ProjectDashboard,
    now_ms: i64,
) {
    if area.height == 0 {
        return;
    }
    let mut row = area.y;
    f.render_widget(
        Paragraph::new(Span::styled(
            " activity ",
            Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::BOLD),
        )),
        Rect {
            x: area.x,
            y: row,
            width: area.width,
            height: 1,
        },
    );
    row = row.saturating_add(1);

    let empty = VecDeque::new();
    let feed = dashboard
        .activity
        .get(project_id)
        .unwrap_or(&empty);
    if feed.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  (state changes appear here)",
                Style::default().fg(theme.dim),
            )),
            Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: 1,
            },
        );
        return;
    }

    let page = area.bottom().saturating_sub(row) as usize;
    let max_scroll = feed.len().saturating_sub(page.max(1));
    dashboard.feed_scroll = dashboard.feed_scroll.min(max_scroll);
    let start = dashboard.feed_scroll;
    let end = (start + page).min(feed.len());

    for entry in feed.iter().skip(start).take(end.saturating_sub(start)) {
        if row >= area.bottom() {
            break;
        }
        let age = format_age_ms(
            now_ms
                .saturating_sub(entry.at.timestamp_millis())
                .max(0) as u64,
        );
        let kind_style = match entry.kind {
            ActivityKind::Errored => Style::default().fg(theme.danger),
            ActivityKind::WantsYou => Style::default().fg(theme.accent),
            ActivityKind::Running => Style::default().fg(theme.success),
            ActivityKind::Done => Style::default().fg(theme.dim),
            ActivityKind::Created => Style::default().fg(theme.dim),
            ActivityKind::Other => Style::default().fg(theme.dim),
        };
        let name = truncate_width(
            &entry.label,
            (area.width as usize).saturating_sub(16).max(4),
        );
        let line = Line::from(vec![
            Span::styled(format!(" {age:>4} "), Style::default().fg(theme.dim)),
            Span::styled(format!("{} ", entry.kind.glyph()), kind_style),
            Span::styled(
                format!("{} ", entry.kind.label()),
                kind_style,
            ),
            Span::styled(name, Style::default().fg(theme.text)),
        ]);
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: 1,
            },
        );
        dashboard.hits.feed_rows.push(ProjectRowHit {
            area: Rect {
                x: area.x,
                y: row,
                width: area.width,
                height: 1,
            },
            session_id: entry.session_id.clone(),
        });
        row = row.saturating_add(1);
    }
}

fn render_preview(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    members: &[&SessionSummary],
    target_id: Option<&str>,
    dashboard: &ProjectDashboard,
    now_ms: i64,
) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.dim));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(id) = target_id else {
        f.render_widget(
            Paragraph::new(Span::styled(
                " preview — select a project member ",
                Style::default().fg(theme.dim),
            )),
            inner,
        );
        return;
    };
    let Some(s) = members.iter().find(|m| m.id == id) else {
        return;
    };

    let mut row = inner.y;
    if row >= inner.bottom() {
        return;
    }

    // Chrome line: name · state · model · context · activity
    let (act, busy) = activity_cell(s, now_ms);
    let mut chrome = format!(
        " preview: {}  {} {}  {}",
        primary_label(s),
        s.state.glyph(),
        s.state.label(),
        identity_label(s),
    );
    if let Some((filled, pct)) = context_pct(s) {
        chrome.push_str(&format!(
            "  {}{} {}%",
            "▰".repeat(filled),
            "▱".repeat(4usize.saturating_sub(filled)),
            pct
        ));
    }
    chrome.push_str(&format!("  {act}"));
    f.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(&chrome, inner.width as usize),
            if busy {
                Style::default().fg(theme.success)
            } else {
                Style::default().fg(theme.dim)
            },
        )),
        Rect {
            x: inner.x,
            y: row,
            width: inner.width,
            height: 1,
        },
    );
    row = row.saturating_add(1);

    // Body: recent messages (C3)
    let msgs = dashboard.preview_messages.get(id);
    let body_h = inner.bottom().saturating_sub(row);
    if body_h == 0 {
        return;
    }

    match msgs {
        Some(m) if !m.is_empty() => {
            let show: Vec<&PreviewMessage> = m.iter().rev().take(PREVIEW_SHOW).collect();
            for msg in show.into_iter().rev() {
                if row >= inner.bottom() {
                    break;
                }
                let role = match msg.role {
                    PreviewRole::User => "you",
                    PreviewRole::Assistant => "agent",
                    PreviewRole::Other => "…",
                };
                let text = msg.text.replace('\n', " ");
                let line = format!(
                    "  {}: {}",
                    role,
                    truncate_width(&text, (inner.width as usize).saturating_sub(8).max(4))
                );
                f.render_widget(
                    Paragraph::new(Span::styled(
                        truncate_width(&line, inner.width as usize),
                        Style::default().fg(theme.text),
                    )),
                    Rect {
                        x: inner.x,
                        y: row,
                        width: inner.width,
                        height: 1,
                    },
                );
                row = row.saturating_add(1);
            }
        }
        _ => {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "  (no recent chat cached — open session to load history)",
                    Style::default().fg(theme.dim),
                ))
                .wrap(Wrap { trim: false }),
                Rect {
                    x: inner.x,
                    y: row,
                    width: inner.width,
                    height: body_h,
                },
            );
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
    use construct_protocol::{SessionKind, SessionSummary, TokenTally};

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
    fn activity_feed_records_transitions_only() {
        let mut dash = ProjectDashboard::default();
        let mut s = session("s1", Some("p1"), SessionState::Running, false);
        let t0 = Utc::now();
        // First observe seeds baseline without a feed line.
        dash.observe_session_state(&s, t0);
        assert!(dash.activity.get("p1").map(|f| f.is_empty()).unwrap_or(true));

        // Same state → no new entry
        dash.observe_session_state(&s, t0);
        assert!(dash.activity.get("p1").map(|f| f.is_empty()).unwrap_or(true));

        s.state = SessionState::AwaitingInput;
        s.needs_attention = true;
        dash.observe_session_state(&s, t0);
        assert_eq!(dash.activity["p1"][0].kind, ActivityKind::WantsYou);

        dash.note_session_created(&session("s2", Some("p1"), SessionState::Pending, false), t0);
        assert_eq!(dash.activity["p1"][0].kind, ActivityKind::Created);
    }

    #[test]
    fn preview_target_prefers_hover_then_cursor() {
        let a = session("a", Some("p"), SessionState::Running, false);
        let b = session("b", Some("p"), SessionState::Done, false);
        let members = vec![&a, &b];
        assert_eq!(
            preview_target_id(&members, 1, Some("a"), true),
            Some("a")
        );
        assert_eq!(preview_target_id(&members, 1, None, true), Some("b"));
        assert_eq!(preview_target_id(&members, 1, None, false), Some("a"));
    }

    #[test]
    fn message_cache_coalesces_assistant_stream() {
        let mut dash = ProjectDashboard::default();
        let t = Utc::now();
        dash.observe_message("s1", PreviewRole::Assistant, "Hel", t);
        dash.observe_message("s1", PreviewRole::Assistant, "lo", t);
        dash.observe_message("s1", PreviewRole::User, "hi", t);
        let msgs = &dash.preview_messages["s1"];
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "Hello");
        assert_eq!(msgs[1].text, "hi");
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
}
