//! The Construct loadout screen — a cinematic new-session composer.
//!
//! `C-x C-f` no longer opens a one-line harness picker; it drops the user
//! into the *loadout*, the way Neo and Trinity kit up inside the Construct's
//! white void before a run ("Guns. Lots of guns."). Here the loadout is the
//! set of resources an agent task needs:
//!
//! - **WEAPON**   — which harness drives the session (claude / codex / …)
//! - **GEAR**     — the working directory (+ optional isolated git worktree)
//! - **BRIEFING** — the initial prompt handed to the harness
//!
//! This module owns the *state and pure logic* of that screen (field
//! navigation, text editing, filesystem completion, the entrance-animation
//! clock). Rendering lives in `ui::render_loadout`; key handling and session
//! creation live in `app`.

use std::time::Instant;

/// Duration of the rack-slide entrance animation. Short on purpose — the
/// screen is opened often, so the cinematic is a quick flourish, not a gate.
pub const ENTRANCE_MS: u128 = 600;

/// Which slot of the loadout currently holds keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadoutField {
    /// Harness selection (the "gun").
    Weapon,
    /// Working directory.
    Gear,
    /// `+worktree` toggle.
    Worktree,
    /// Initial prompt / mission briefing.
    Briefing,
}

impl LoadoutField {
    /// Cycle order: Weapon → Gear → Worktree → Briefing → (wrap).
    pub fn next(self) -> Self {
        match self {
            LoadoutField::Weapon => LoadoutField::Gear,
            LoadoutField::Gear => LoadoutField::Worktree,
            LoadoutField::Worktree => LoadoutField::Briefing,
            LoadoutField::Briefing => LoadoutField::Weapon,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            LoadoutField::Weapon => LoadoutField::Briefing,
            LoadoutField::Gear => LoadoutField::Weapon,
            LoadoutField::Worktree => LoadoutField::Gear,
            LoadoutField::Briefing => LoadoutField::Worktree,
        }
    }
}

/// A single card in the WEAPON rack. Built from the daemon's `HarnessInfo`,
/// plus one synthetic `project` card that routes to project creation.
#[derive(Debug, Clone)]
pub struct LoadoutHarness {
    pub name: String,
    pub description: Option<String>,
    pub available: bool,
    /// The synthetic "create a project" card — LOAD opens the project-name
    /// prompt instead of spawning a session.
    pub is_project: bool,
}

/// All mutable state of the loadout screen. `None` on `App` means the screen
/// is closed.
#[derive(Debug, Clone)]
pub struct LoadoutState {
    /// When the screen opened — drives the entrance animation.
    pub opened_at: Instant,
    /// Set once the user presses any key, so the entrance never replays.
    pub entrance_skipped: bool,
    /// Currently focused slot.
    pub field: LoadoutField,

    /// WEAPON: the rack of harness cards and the selected index.
    pub harnesses: Vec<LoadoutHarness>,
    pub harness_idx: usize,

    /// GEAR: working directory (char-cursor) and worktree toggle.
    pub cwd: String,
    pub cwd_cursor: usize,
    pub worktree: bool,

    /// BRIEFING: initial prompt (char-cursor over the whole buffer).
    pub prompt: String,
    pub prompt_cursor: usize,

    /// Group inherited from the launching context (selection).
    pub group_id: Option<String>,

    /// Transient inline note (completion result, "unavailable", …). Cleared
    /// by the next text edit.
    pub note: Option<String>,
}

impl LoadoutState {
    pub fn new(
        harnesses: Vec<LoadoutHarness>,
        group_id: Option<String>,
        cwd: String,
        now: Instant,
    ) -> Self {
        // Prefer claude if present and available, else the first available
        // real harness, else just the first card.
        let harness_idx = harnesses
            .iter()
            .position(|h| h.available && !h.is_project && h.name == "claude")
            .or_else(|| {
                harnesses
                    .iter()
                    .position(|h| h.available && !h.is_project)
            })
            .unwrap_or(0);
        let cwd_cursor = cwd.chars().count();
        Self {
            opened_at: now,
            entrance_skipped: false,
            field: LoadoutField::Weapon,
            harnesses,
            harness_idx,
            cwd,
            cwd_cursor,
            worktree: false,
            prompt: String::new(),
            prompt_cursor: 0,
            group_id,
            note: None,
        }
    }

    /// Entrance animation progress, 0.0 → 1.0. Clamps to 1.0 once skipped.
    pub fn entrance_progress(&self, now: Instant) -> f32 {
        if self.entrance_skipped {
            return 1.0;
        }
        let e = now.saturating_duration_since(self.opened_at).as_millis();
        (e as f32 / ENTRANCE_MS as f32).clamp(0.0, 1.0)
    }

    /// True while the entrance is still playing (used to decide whether a key
    /// should skip it rather than act).
    pub fn entrance_active(&self, now: Instant) -> bool {
        !self.entrance_skipped && self.entrance_progress(now) < 1.0
    }

    pub fn selected_harness(&self) -> Option<&LoadoutHarness> {
        self.harnesses.get(self.harness_idx)
    }

    pub fn focus_next(&mut self) {
        self.field = self.field.next();
    }

    pub fn focus_prev(&mut self) {
        self.field = self.field.prev();
    }

    pub fn select_harness_next(&mut self) {
        if self.harnesses.is_empty() {
            return;
        }
        self.harness_idx = (self.harness_idx + 1) % self.harnesses.len();
    }

    pub fn select_harness_prev(&mut self) {
        if self.harnesses.is_empty() {
            return;
        }
        self.harness_idx = (self.harness_idx + self.harnesses.len() - 1) % self.harnesses.len();
    }

    pub fn toggle_worktree(&mut self) {
        self.worktree = !self.worktree;
    }

    /// `~`-expanded working directory, used at submit time and for completion.
    pub fn expanded_cwd(&self) -> String {
        expand_tilde(&self.cwd)
    }

    // ---- text editing for the active text field (Gear cwd / Briefing prompt) ----

    fn active_text(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.field {
            LoadoutField::Gear => Some((&mut self.cwd, &mut self.cwd_cursor)),
            LoadoutField::Briefing => Some((&mut self.prompt, &mut self.prompt_cursor)),
            _ => None,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        // The cwd is a single line — newlines only make sense in the briefing.
        if c == '\n' && self.field != LoadoutField::Briefing {
            return;
        }
        self.note = None;
        if let Some((s, cur)) = self.active_text() {
            let mut chars: Vec<char> = s.chars().collect();
            let idx = (*cur).min(chars.len());
            chars.insert(idx, c);
            *s = chars.into_iter().collect();
            *cur = idx + 1;
        }
    }

    pub fn backspace(&mut self) {
        self.note = None;
        if let Some((s, cur)) = self.active_text() {
            if *cur == 0 {
                return;
            }
            let mut chars: Vec<char> = s.chars().collect();
            let idx = *cur - 1;
            if idx < chars.len() {
                chars.remove(idx);
            }
            *s = chars.into_iter().collect();
            *cur = idx;
        }
    }

    pub fn delete_forward(&mut self) {
        self.note = None;
        if let Some((s, cur)) = self.active_text() {
            let mut chars: Vec<char> = s.chars().collect();
            if *cur < chars.len() {
                chars.remove(*cur);
                *s = chars.into_iter().collect();
            }
        }
    }

    pub fn cursor_left(&mut self) {
        if let Some((_, cur)) = self.active_text() {
            *cur = cur.saturating_sub(1);
        }
    }

    pub fn cursor_right(&mut self) {
        if let Some((s, cur)) = self.active_text() {
            let len = s.chars().count();
            *cur = (*cur + 1).min(len);
        }
    }

    pub fn cursor_home(&mut self) {
        if let Some((s, cur)) = self.active_text() {
            let (line, _) = line_col(s, *cur);
            *cur = index_for(s, line, 0);
        }
    }

    pub fn cursor_end(&mut self) {
        if let Some((s, cur)) = self.active_text() {
            let (line, _) = line_col(s, *cur);
            *cur = index_for(s, line, usize::MAX);
        }
    }

    /// Whether the prompt cursor is on the first line (so an `Up` should
    /// leave the prompt and move to the previous slot instead of moving the
    /// text cursor).
    pub fn cursor_on_first_line(&self) -> bool {
        line_col(&self.prompt, self.prompt_cursor).0 == 0
    }

    /// Whether the prompt cursor is on the last line (so a `Down` should
    /// leave the prompt and move to the next slot).
    pub fn cursor_on_last_line(&self) -> bool {
        let last = self.prompt.split('\n').count().saturating_sub(1);
        line_col(&self.prompt, self.prompt_cursor).0 >= last
    }

    /// Briefing-only: move the cursor up one line, keeping the column.
    pub fn cursor_up(&mut self) {
        if self.field != LoadoutField::Briefing {
            return;
        }
        let (line, col) = line_col(&self.prompt, self.prompt_cursor);
        if line == 0 {
            self.prompt_cursor = 0;
            return;
        }
        self.prompt_cursor = index_for(&self.prompt, line - 1, col);
    }

    /// Briefing-only: move the cursor down one line, keeping the column.
    pub fn cursor_down(&mut self) {
        if self.field != LoadoutField::Briefing {
            return;
        }
        let (line, col) = line_col(&self.prompt, self.prompt_cursor);
        let last = self.prompt.split('\n').count().saturating_sub(1);
        if line >= last {
            self.prompt_cursor = self.prompt.chars().count();
            return;
        }
        self.prompt_cursor = index_for(&self.prompt, line + 1, col);
    }

    pub fn newline(&mut self) {
        if self.field == LoadoutField::Briefing {
            self.insert_char('\n');
        }
    }

    /// Shell-style path completion for the cwd field. Returns true if the
    /// field text changed (so the caller can decide Tab should advance to the
    /// next slot when there's nothing left to complete).
    pub fn complete_cwd(&mut self) -> bool {
        let input = self.expanded_cwd();
        let (dir, prefix) = match input.rfind('/') {
            Some(i) => (input[..=i].to_string(), input[i + 1..].to_string()),
            None => (String::from("./"), input.clone()),
        };
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => {
                self.note = Some("no such directory".to_string());
                return false;
            }
        };
        let mut names: Vec<String> = read
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        names.sort();
        if names.is_empty() {
            self.note = Some(format!("no match for {prefix}"));
            return false;
        }
        if names.len() == 1 {
            let full = format!("{dir}{}/", names[0]);
            let changed = full != self.cwd;
            self.cwd = full;
            self.cwd_cursor = self.cwd.chars().count();
            self.note = None;
            return changed;
        }
        let common = longest_common_prefix(&names);
        let completed = format!("{dir}{common}");
        let changed = completed.chars().count() > input.chars().count();
        if changed {
            self.cwd = completed;
            self.cwd_cursor = self.cwd.chars().count();
        }
        // Show a few candidates as a hint either way.
        let mut shown: Vec<String> = names.iter().take(6).cloned().collect();
        if names.len() > 6 {
            shown.push("…".to_string());
        }
        self.note = Some(format!("matches: {}", shown.join("  ")));
        changed
    }
}

/// Expand a leading `~` / `~/` to `$HOME`.
fn expand_tilde(p: &str) -> String {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        let home = home_dir();
        return format!("{}/{rest}", home.trim_end_matches('/'));
    }
    p.to_string()
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

/// Char `(line, col)` for a char index into a possibly multi-line string.
fn line_col(s: &str, cur: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for (i, ch) in s.chars().enumerate() {
        if i == cur {
            return (line, col);
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Inverse of [`line_col`]: char index for a `(line, col)`, clamped. A `col`
/// of `usize::MAX` resolves to end-of-line.
fn index_for(s: &str, target_line: usize, target_col: usize) -> usize {
    let mut line = 0;
    let mut col = 0;
    let mut idx = 0;
    let mut last_on_target = None;
    for ch in s.chars() {
        if line == target_line {
            if col == target_col {
                return idx;
            }
            last_on_target = Some(idx + 1);
        }
        if ch == '\n' {
            if line == target_line {
                // Reached end of the target line before hitting target_col.
                return idx;
            }
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        idx += 1;
    }
    // Target line is the last line (no trailing newline).
    if line == target_line {
        if target_col == usize::MAX {
            return idx;
        }
        return last_on_target.unwrap_or(idx).min(idx);
    }
    idx
}

fn longest_common_prefix(strs: &[String]) -> String {
    let mut out = String::new();
    let Some(first) = strs.first() else {
        return out;
    };
    'outer: for (i, c) in first.chars().enumerate() {
        for s in &strs[1..] {
            if s.chars().nth(i) != Some(c) {
                break 'outer;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(name: &str, available: bool) -> LoadoutHarness {
        LoadoutHarness {
            name: name.to_string(),
            description: None,
            available,
            is_project: false,
        }
    }

    fn state() -> LoadoutState {
        let harnesses = vec![
            harness("codex", true),
            harness("claude", true),
            harness("shell", true),
        ];
        LoadoutState::new(harnesses, None, "/tmp".to_string(), Instant::now())
    }

    #[test]
    fn defaults_to_claude_when_available() {
        let s = state();
        assert_eq!(s.selected_harness().unwrap().name, "claude");
    }

    #[test]
    fn field_cycles_forward_and_back() {
        let mut s = state();
        assert_eq!(s.field, LoadoutField::Weapon);
        s.focus_next();
        assert_eq!(s.field, LoadoutField::Gear);
        s.focus_prev();
        assert_eq!(s.field, LoadoutField::Weapon);
        s.focus_prev();
        assert_eq!(s.field, LoadoutField::Briefing);
    }

    #[test]
    fn harness_selection_wraps() {
        let mut s = state();
        s.harness_idx = 0;
        s.select_harness_prev();
        assert_eq!(s.harness_idx, 2);
        s.select_harness_next();
        assert_eq!(s.harness_idx, 0);
    }

    #[test]
    fn briefing_edits_and_newlines() {
        let mut s = state();
        s.field = LoadoutField::Briefing;
        for c in "ab".chars() {
            s.insert_char(c);
        }
        s.newline();
        s.insert_char('c');
        assert_eq!(s.prompt, "ab\nc");
        // Cursor up keeps column, lands on line 0.
        s.cursor_up();
        assert_eq!(line_col(&s.prompt, s.prompt_cursor).0, 0);
    }

    #[test]
    fn prompt_line_boundaries_drive_slot_exit() {
        let mut s = state();
        s.field = LoadoutField::Briefing;
        // Empty prompt: cursor is on both the first and last line.
        assert!(s.cursor_on_first_line());
        assert!(s.cursor_on_last_line());
        s.insert_char('a');
        s.newline();
        s.insert_char('b'); // "a\nb", cursor on line 1
        assert!(!s.cursor_on_first_line());
        assert!(s.cursor_on_last_line());
        s.cursor_up(); // back to line 0
        assert!(s.cursor_on_first_line());
        assert!(!s.cursor_on_last_line());
    }

    #[test]
    fn cwd_field_rejects_newline() {
        let mut s = state();
        s.field = LoadoutField::Gear;
        s.cwd.clear();
        s.cwd_cursor = 0;
        s.insert_char('\n');
        assert_eq!(s.cwd, "");
    }

    #[test]
    fn backspace_edits_active_field() {
        let mut s = state();
        s.field = LoadoutField::Gear;
        s.cwd = "abc".to_string();
        s.cwd_cursor = 3;
        s.backspace();
        assert_eq!(s.cwd, "ab");
        assert_eq!(s.cwd_cursor, 2);
    }
}
