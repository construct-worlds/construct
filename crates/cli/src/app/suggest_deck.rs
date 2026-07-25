//! The suggestion deck (spec 0109): a corner "orb" on the session view
//! that fetches and holds a hand of generated next prompts. Suggestions
//! are generated **on demand**: the orb idles while the session awaits
//! input; clicking it (or `C-x s`) asks the daemon to generate via
//! `session.suggest`, the orb spins, and when the hand arrives the verb
//! fan opens — chips along a quarter arc from the corner. Picking a
//! verb swaps the fan for a vertical card stack. Accepting anything
//! sends the text through the ordinary `session.input` path — the deck
//! never has its own send machinery.
//!
//! Interaction contract: the deck only ever consumes keys while it is
//! open, and any printable key closes it and falls through to normal
//! routing — typing always wins over suggestions.

use super::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::time::Instant;

/// Reveal stagger between fan chips during the deal animation.
const CHIP_REVEAL_MS: u128 = 80;
/// How long the orb pulses after a fresh hand is dealt.
const ORB_PULSE_MS: u128 = 4_000;
/// How long a pending `session.suggest` shows the spinner before the
/// orb gives up and returns to idle (matches the daemon's probe cap).
const REQUEST_STALE_MS: u128 = 125_000;
/// Spinner frames while generation is in flight.
const SPINNER: [&str; 4] = ["⠋", "⠙", "⠸", "⠴"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckOpen {
    Closed,
    /// Verb fan open; `sel` indexes the fan (0 = top pick, 1.. = verbs).
    Fan { sel: usize },
    /// Card stack open for verb `verb`; `sel` indexes its cards.
    Stack { verb: usize, sel: usize },
}

#[derive(Debug, Clone, Default)]
pub struct SuggestionDeckState {
    /// The dealt hand, once generation finished.
    pub hand: Option<construct_protocol::SuggestionHand>,
    pub open: DeckOpen,
    /// When the hand arrived — drives the orb pulse.
    pub dealt_at: Option<Instant>,
    /// When the fan was opened — drives the chip reveal stagger.
    pub opened_at: Option<Instant>,
    /// When `session.suggest` was sent and no hand has arrived yet —
    /// drives the orb spinner.
    pub requested_at: Option<Instant>,
}

impl Default for DeckOpen {
    fn default() -> Self {
        DeckOpen::Closed
    }
}

impl SuggestionDeckState {
    /// Fan item count: top pick + one chip per verb.
    fn fan_len(&self) -> usize {
        self.hand.as_ref().map(|h| 1 + h.verbs.len()).unwrap_or(0)
    }

    fn generating(&self) -> bool {
        self.requested_at
            .map(|t| t.elapsed().as_millis() < REQUEST_STALE_MS)
            .unwrap_or(false)
    }

    /// A freshly-dealt hand arrived: store it and open the fan — the
    /// user explicitly asked for it, so it shouldn't need a second
    /// activation.
    pub fn deal(&mut self, hand: construct_protocol::SuggestionHand) {
        self.hand = Some(hand);
        self.requested_at = None;
        self.dealt_at = Some(Instant::now());
        self.open = DeckOpen::Fan { sel: 0 };
        self.opened_at = Some(Instant::now());
    }
}

impl App {
    /// Orb activation (`C-x s` or a click on the badge): toggle an
    /// existing hand open/closed, or — with no hand yet — request
    /// generation from the daemon and start the spinner.
    pub(crate) async fn suggestion_deck_toggle(&mut self) {
        let Some(sid) = self.selected_id() else {
            return;
        };
        let awaiting = self
            .selected_session()
            .map(|s| s.state == construct_protocol::SessionState::AwaitingInput)
            .unwrap_or(false);
        if !awaiting {
            return;
        }
        let deck = self.suggestions.entry(sid.clone()).or_default();
        if deck.hand.is_some() {
            match deck.open {
                DeckOpen::Closed => {
                    deck.open = DeckOpen::Fan { sel: 0 };
                    deck.opened_at = Some(Instant::now());
                }
                _ => {
                    deck.open = DeckOpen::Closed;
                    deck.opened_at = None;
                }
            }
            return;
        }
        if deck.generating() {
            return;
        }
        deck.requested_at = Some(Instant::now());
        match self.client.suggest(&sid).await {
            Ok(r) if r.started => {}
            Ok(_) => {
                if let Some(d) = self.suggestions.get_mut(&sid) {
                    d.requested_at = None;
                }
            }
            Err(e) => {
                if let Some(d) = self.suggestions.get_mut(&sid) {
                    d.requested_at = None;
                }
                self.set_status(format!("suggest failed: {e}"));
            }
        }
    }

    /// Key intake while a deck exists for the selected session. Returns
    /// true when the key was consumed. Only an OPEN deck consumes keys;
    /// printable keys close the deck and fall through so typing always
    /// reaches the session untouched.
    pub(crate) async fn suggestion_deck_handle_key(&mut self, key: &KeyEvent) -> bool {
        let Some(sid) = self.selected_id() else {
            return false;
        };
        let Some(deck) = self.suggestions.get_mut(&sid) else {
            return false;
        };
        if deck.open == DeckOpen::Closed || deck.hand.is_none() {
            return false;
        }
        if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
            // Chorded keys (C-x …, C-c) keep their global meaning.
            return false;
        }
        let hand = deck.hand.clone().expect("checked above");
        match key.code {
            KeyCode::Esc => {
                deck.open = DeckOpen::Closed;
                deck.opened_at = None;
                true
            }
            KeyCode::Up => {
                match &mut deck.open {
                    DeckOpen::Fan { sel } | DeckOpen::Stack { sel, .. } => {
                        *sel = sel.saturating_sub(1)
                    }
                    DeckOpen::Closed => {}
                }
                true
            }
            KeyCode::Down => {
                match &mut deck.open {
                    DeckOpen::Fan { sel } => {
                        *sel = (*sel + 1).min(hand.verbs.len());
                    }
                    DeckOpen::Stack { verb, sel } => {
                        let max = hand
                            .verbs
                            .get(*verb)
                            .map(|v| v.cards.len().saturating_sub(1))
                            .unwrap_or(0);
                        *sel = (*sel + 1).min(max);
                    }
                    DeckOpen::Closed => {}
                }
                true
            }
            KeyCode::Left | KeyCode::Backspace => {
                match deck.open {
                    DeckOpen::Stack { verb, .. } => {
                        deck.open = DeckOpen::Fan { sel: verb + 1 };
                    }
                    DeckOpen::Fan { .. } => {
                        deck.open = DeckOpen::Closed;
                        deck.opened_at = None;
                    }
                    DeckOpen::Closed => {}
                }
                true
            }
            KeyCode::Right => {
                if let DeckOpen::Fan { sel } = deck.open {
                    if sel >= 1 && hand.verbs.get(sel - 1).is_some() {
                        deck.open = DeckOpen::Stack { verb: sel - 1, sel: 0 };
                    }
                }
                true
            }
            KeyCode::Enter => {
                match deck.open {
                    DeckOpen::Fan { sel: 0 } => {
                        self.suggestion_deck_send(&sid, hand.top.text.clone()).await;
                    }
                    DeckOpen::Fan { sel } => {
                        if hand.verbs.get(sel - 1).is_some() {
                            deck.open = DeckOpen::Stack { verb: sel - 1, sel: 0 };
                        }
                    }
                    DeckOpen::Stack { verb, sel } => {
                        if let Some(text) = hand
                            .verbs
                            .get(verb)
                            .and_then(|v| v.cards.get(sel))
                            .map(|c| c.text.clone())
                        {
                            self.suggestion_deck_send(&sid, text).await;
                        }
                    }
                    DeckOpen::Closed => {}
                }
                true
            }
            // Typing always wins: close and let the key take its normal
            // route (PTY input, chords, whatever it means).
            KeyCode::Char(_) | KeyCode::Tab => {
                deck.open = DeckOpen::Closed;
                deck.opened_at = None;
                false
            }
            _ => false,
        }
    }

    /// True when the pointer is over the suggestion orb or the open
    /// fan/stack overlay. `on_mouse` checks this before piping events
    /// into a mouse-grabbing child PTY (claude fullscreen), which would
    /// otherwise swallow the very click that operates the deck. While
    /// the overlay is open the whole pane acts as its backdrop, so every
    /// position counts as "over".
    pub(crate) fn mouse_over_suggestion_deck(&self, col: u16, row: u16) -> bool {
        let over = |r: &Rect| -> bool {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        };
        if self.layout.suggestion_orb_hit.as_ref().is_some_and(over) {
            return true;
        }
        if self.layout.suggestion_chip_hits.iter().any(|(r, _)| over(r)) {
            return true;
        }
        if self.layout.suggestion_card_hits.iter().any(|(r, _)| over(r)) {
            return true;
        }
        self.selected_id()
            .and_then(|sid| self.suggestions.get(&sid))
            .map(|d| d.open != DeckOpen::Closed && d.hand.is_some())
            .unwrap_or(false)
    }

    /// Mouse intake. Returns true when the click was consumed.
    pub(crate) async fn suggestion_deck_handle_click(&mut self, col: u16, row: u16) -> bool {
        let hit = |r: &Rect| -> bool {
            col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
        };
        if let Some(orb) = self.layout.suggestion_orb_hit {
            if hit(&orb) {
                self.suggestion_deck_toggle().await;
                return true;
            }
        }
        let Some(sid) = self.selected_id() else {
            return false;
        };
        let hand = self
            .suggestions
            .get(&sid)
            .and_then(|d| d.hand.clone());
        let chip = self
            .layout
            .suggestion_chip_hits
            .iter()
            .find(|(r, _)| hit(r))
            .map(|(_, i)| *i);
        if let (Some(i), Some(hand)) = (chip, hand.as_ref()) {
            if i == 0 {
                self.suggestion_deck_send(&sid, hand.top.text.clone()).await;
            } else if hand.verbs.get(i - 1).is_some() {
                if let Some(deck) = self.suggestions.get_mut(&sid) {
                    deck.open = DeckOpen::Stack { verb: i - 1, sel: 0 };
                }
            }
            return true;
        }
        let card = self
            .layout
            .suggestion_card_hits
            .iter()
            .find(|(r, _)| hit(r))
            .map(|(_, i)| *i);
        if let (Some(i), Some(hand)) = (card, hand.as_ref()) {
            if let Some(DeckOpen::Stack { verb, .. }) =
                self.suggestions.get(&sid).map(|d| d.open)
            {
                if let Some(text) = hand
                    .verbs
                    .get(verb)
                    .and_then(|v| v.cards.get(i))
                    .map(|c| c.text.clone())
                {
                    self.suggestion_deck_send(&sid, text).await;
                }
            }
            return true;
        }
        // Click anywhere else while open acts as a backdrop: close the
        // deck and swallow the click so it doesn't also hit the PTY.
        if let Some(deck) = self.suggestions.get_mut(&sid) {
            if deck.open != DeckOpen::Closed {
                deck.open = DeckOpen::Closed;
                deck.opened_at = None;
                return true;
            }
        }
        false
    }

    /// Accept a suggestion: drop the hand (the turn it described is over
    /// the moment we send) and route the text through the ordinary
    /// session-input path.
    async fn suggestion_deck_send(&mut self, sid: &str, text: String) {
        self.suggestions.remove(sid);
        if let Err(e) = self.client.send_input(sid, text).await {
            self.set_status(format!("suggestion send failed: {e}"));
        }
    }
}

/// Truncate to `max` display chars with an ellipsis.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Corner orb badge. Drawn inside the session view pane, inset from the
/// right edge so it doesn't sit on the terminal scrollbar track. Idle it
/// reads `◈ suggest`; while generating it spins; with a dealt hand it
/// shows the item count (and pulses briefly).
pub(crate) fn render_suggestion_orb(f: &mut Frame, area: Rect, app: &mut App) {
    app.layout.suggestion_orb_hit = None;
    app.layout.suggestion_anchor = None;
    let Some(sid) = app.selected_id() else {
        return;
    };
    let awaiting = app
        .selected_session()
        .map(|s| s.state == construct_protocol::SessionState::AwaitingInput)
        .unwrap_or(false);
    if !awaiting || area.width < 16 || area.height < 4 {
        return;
    }
    let deck = app.suggestions.get(&sid);
    let has_hand = deck.map(|d| d.hand.is_some()).unwrap_or(false);
    let open = deck.map(|d| d.open != DeckOpen::Closed).unwrap_or(false);
    let generating = deck.map(|d| d.generating()).unwrap_or(false);
    let label = if open {
        " ◈ ✕ ".to_string()
    } else if has_hand {
        format!(" ◈ {} ", deck.map(|d| d.fan_len()).unwrap_or(0))
    } else if generating {
        let frame = deck
            .and_then(|d| d.requested_at)
            .map(|t| (t.elapsed().as_millis() / 120) as usize % SPINNER.len())
            .unwrap_or(0);
        format!(" ◈ {} ", SPINNER[frame])
    } else {
        " ◈ suggest ".to_string()
    };
    let w = label.chars().count() as u16;
    // Inset 2 cols from the right edge: the terminal scrollbar owns the
    // outermost column of the view pane.
    let x = area.x + area.width.saturating_sub(w + 2);
    let y = area.y + area.height.saturating_sub(2);
    let rect = Rect::new(x, y, w, 1);
    let pulse_on = deck
        .and_then(|d| d.dealt_at)
        .map(|t| {
            t.elapsed().as_millis() < ORB_PULSE_MS && (t.elapsed().as_millis() / 400) % 2 == 0
        })
        .unwrap_or(false);
    let style = if open || pulse_on || generating {
        Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(app.theme.accent)
    };
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), rect);
    app.layout.suggestion_orb_hit = Some(rect);
    app.layout.suggestion_anchor = Some(area);
}

/// Topmost overlay: verb fan or card stack, anchored to the orb. Called
/// from `finish_frame` so it sits above every base pane (but under the
/// session-picker / configure modals, which render after it).
pub(crate) fn render_suggestion_overlay(f: &mut Frame, app: &mut App) {
    app.layout.suggestion_chip_hits.clear();
    app.layout.suggestion_card_hits.clear();
    let Some(area) = app.layout.suggestion_anchor else {
        return;
    };
    let Some(orb) = app.layout.suggestion_orb_hit else {
        return;
    };
    let Some(sid) = app.selected_id() else {
        return;
    };
    let accent = app.theme.accent;
    let accent_alt = app.theme.accent_alt;
    let Some(deck) = app.suggestions.get(&sid) else {
        return;
    };
    let Some(hand) = deck.hand.as_ref() else {
        return;
    };
    match deck.open {
        DeckOpen::Closed => {}
        DeckOpen::Fan { sel } => {
            let n = 1 + hand.verbs.len();
            let revealed = deck
                .opened_at
                .map(|t| (t.elapsed().as_millis() / CHIP_REVEAL_MS) as usize + 1)
                .unwrap_or(n)
                .min(n);
            // True quarter-circle fan around the corner orb: chip 0 sits
            // mostly LEFT of the orb, the last chip mostly ABOVE it, and
            // the rest sweep the arc between. The x-radius is ~2.6× the
            // y-radius to correct for ~1:2 terminal cell aspect. Panes too
            // short for the arc fall back to a compact stair stack.
            let ry = ((n as u16) + 4)
                .min(area.height.saturating_sub(3))
                .max(3);
            let rx = (f32::from(ry) * 2.6)
                .round()
                .min(f32::from(area.width.saturating_sub(24).max(8))) as u16;
            let short = area.height < (n as u16) * 2 + 5;
            let mut rows_used: Vec<u16> = Vec::new();
            let mut chips: Vec<(Rect, usize, Line)> = Vec::new();
            for i in 0..n.min(revealed) {
                let (dx, mut dy) = if short {
                    ((2 * i) as u16, (i + 2) as u16)
                } else {
                    // θ sweeps 15°..85° across the chips.
                    let t = if n == 1 {
                        0.5
                    } else {
                        i as f32 / (n - 1) as f32
                    };
                    let th = (15.0 + 70.0 * t).to_radians();
                    (
                        (f32::from(rx) * th.cos()).round() as u16,
                        ((f32::from(ry) * th.sin()).round() as u16).max(2),
                    )
                };
                // One chip per row, whatever the rounding did.
                while rows_used.contains(&dy) {
                    dy += 1;
                }
                rows_used.push(dy);
                let (text, color) = if i == 0 {
                    (format!("▶ {}", trunc(&hand.top.text, 44)), accent)
                } else {
                    let label = &hand.verbs[i - 1].label;
                    (format!("{label} ›"), accent_alt)
                };
                let label = format!(" {text} ");
                let w = (label.chars().count() as u16).min(area.width.saturating_sub(2));
                // The chip's RIGHT edge lands on the arc point, dx cells
                // left of the orb's right edge.
                let right = orb.x + orb.width;
                let x = right.saturating_sub(dx).saturating_sub(w).max(area.x + 1);
                let y = orb.y.saturating_sub(dy).max(area.y);
                let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
                if i == sel {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let rect = Rect::new(x, y, w, 1);
                chips.push((
                    rect,
                    i,
                    Line::from(Span::styled(trunc(&label, w as usize), style)),
                ));
            }
            for (rect, i, line) in chips {
                f.render_widget(Clear, rect);
                f.render_widget(Paragraph::new(line), rect);
                app.layout.suggestion_chip_hits.push((rect, i));
            }
        }
        DeckOpen::Stack { verb, sel } => {
            let Some(v) = hand.verbs.get(verb) else {
                return;
            };
            let w = area.width.saturating_sub(4).clamp(24, 64);
            let h = (v.cards.len() as u16 + 3).min(area.height.saturating_sub(2));
            let x = (orb.x + orb.width).saturating_sub(w).max(area.x + 1);
            let y = orb.y.saturating_sub(h).max(area.y);
            let rect = Rect::new(x, y, w, h);
            f.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent_alt))
                .title(Span::styled(
                    format!(" {} ", v.label),
                    Style::default().fg(accent_alt).add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            let mut lines: Vec<Line> = Vec::new();
            for (i, card) in v.cards.iter().enumerate() {
                if (i as u16) >= inner.height.saturating_sub(1) {
                    break;
                }
                let marker = if i == sel { "▶ " } else { "· " };
                let mut style = Style::default().fg(accent);
                if i == sel {
                    style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
                }
                let text = format!(
                    "{marker}{}",
                    trunc(&card.text, inner.width.saturating_sub(3) as usize)
                );
                lines.push(Line::from(Span::styled(text, style)));
                app.layout.suggestion_card_hits.push((
                    Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
                    i,
                ));
            }
            lines.push(Line::from(Span::styled(
                "⏎ send · ⌫ back · esc close",
                Style::default().add_modifier(Modifier::DIM),
            )));
            f.render_widget(Paragraph::new(lines), inner);
        }
    }
}
