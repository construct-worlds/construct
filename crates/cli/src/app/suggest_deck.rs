//! Suggestion deck (specs 0109/0155) — the TUI surface.
//!
//! `C-x .` on an awaiting-input session opens a two-column popup inside
//! the focused session pane. Generation is **not** kicked off on open —
//! the left column starts with History and Generate (or Regenerate once a
//! hand is cached); the user must request a hand via `session.suggest`.
//! Categories stay visible on the left while the selected category's
//! concrete prompts appear on the right. History and Generate/Regenerate
//! share the same interaction rule: once the left-column row is
//! highlighted, printable keys move focus to the right column and type
//! into its field (fuzzy history search, or optional guided-generation
//! keywords) without requiring →/Enter first. The Generate right column
//! always shows the same surface — header, underlined keyword field, and
//! a highlighted `[ generate ]` / `[ regenerate ]` action chip — whether
//! the row is merely highlighted or fully focused.
//!
//! The popup is otherwise never modal: outside the explicitly selected
//! history-search / keyword surfaces, printable input closes it and takes
//! its normal route (typing always wins).
//!
//! Accepting a row never sends: for PTY sessions the text is typed
//! into the harness's own prompt line (no Enter), for non-PTY sessions
//! it prefills the send-input minibuffer — either way the user reviews
//! and submits with the ordinary send gesture, matching the webui
//! composer behavior and spec 0109's staging rule.

use construct_protocol::{PromptHistoryEntry, SuggestionHand};
use ratatui::layout::Rect;

/// Which column the keyboard is driving. Both columns stay visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckFocus {
    /// Top pick + generated categories + history + regeneration.
    Categories,
    /// Concrete prompts for the highlighted category.
    Cards,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestDeckHit {
    Category(usize),
    Card(usize),
    /// Right-column (re)generate action button.
    GenerateAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuggestDeckHitZone {
    pub area: Rect,
    pub hit: SuggestDeckHit,
}

impl SuggestDeckHitZone {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }
}

/// Per-session `C-x .` affordance chip painted on that session's pane.
/// State (idle / pending / dealt hand) is always looked up by `session_id`,
/// never shared across sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestAffordanceHit {
    pub session_id: String,
    pub area: Rect,
}

impl SuggestAffordanceHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }
}

/// Open-popup state. The hand and history live on `App`; this is only
/// the two cursors plus the history type-ahead query and per-column
/// scroll offsets (when the body is taller than the height cap).
#[derive(Debug, Clone)]
pub struct SuggestDeck {
    /// Session the deck was opened for. A selection change closes it.
    pub session_id: String,
    pub focus: DeckFocus,
    /// Highlighted row in the left category column.
    pub category_selected: usize,
    /// Highlighted row in the right concrete-prompt column.
    pub card_selected: usize,
    /// First visible row of the left column when content exceeds the body.
    pub category_scroll: usize,
    /// First visible row of the right column when content exceeds the body.
    pub card_scroll: usize,
    /// Fuzzy type-ahead query, active only while History is highlighted.
    pub history_query: String,
    /// Explicit keyword-entry surface for a guided regeneration. `Some("")`
    /// means the input is open but still empty.
    pub regenerate_query: Option<String>,
}

impl SuggestDeck {
    pub fn open(session_id: String) -> Self {
        Self {
            session_id,
            focus: DeckFocus::Categories,
            category_selected: 0,
            card_selected: 0,
            category_scroll: 0,
            card_scroll: 0,
            history_query: String::new(),
            regenerate_query: None,
        }
    }

    /// Seed the Generate keyword field from an existing session draft. This
    /// is intentionally local UI state: opening the deck must not consume or
    /// alter the draft in the harness editor.
    fn prefill_generate_keywords(&mut self, categories: &[DeckRow], keywords: String) -> bool {
        let Some(index) = categories
            .iter()
            .position(|row| matches!(row, DeckRow::Generate { .. }))
        else {
            return false;
        };
        self.category_selected = index;
        self.card_selected = 0;
        self.card_scroll = 0;
        self.focus = DeckFocus::Cards;
        self.regenerate_query = Some(keywords);
        true
    }

    /// Keep `selected` inside the viewport of `scroll` given `visible` rows.
    pub fn ensure_visible(scroll: &mut usize, selected: usize, len: usize, visible: usize) {
        if visible == 0 || len == 0 {
            *scroll = 0;
            return;
        }
        let max_scroll = len.saturating_sub(visible);
        if selected < *scroll {
            *scroll = selected;
        } else if selected >= *scroll + visible {
            *scroll = selected + 1 - visible;
        }
        *scroll = (*scroll).min(max_scroll);
    }

    pub fn clamp_scroll(scroll: &mut usize, len: usize, visible: usize) {
        *scroll = (*scroll).min(len.saturating_sub(visible));
    }
}

/// One selectable row of the popup, precomputed per view so key
/// handling and rendering agree on ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckRow {
    /// The hand's top pick — activating stages this text.
    Top(String),
    /// A verb chip — activating opens its cards.
    Verb {
        index: usize,
        label: String,
        count: usize,
    },
    /// The global-history entry row — activating opens the history view.
    History { count: usize },
    /// Final category that opens optional keyword guidance, then requests
    /// a hand. `regenerate` is true when a hand is already cached (label
    /// "Regenerate"); false on first request (label "Generate").
    Generate { regenerate: bool },
    /// Non-interactive placeholder while generation is in flight.
    Generating,
    /// A concrete prompt (verb card or history entry) — activating
    /// stages this text.
    Card(String),
}

impl DeckRow {
    pub fn is_activatable(&self) -> bool {
        !matches!(self, DeckRow::Generating)
    }
}

/// Rows for the fan view. Opening the deck does **not** start generation;
/// History and Generate are always available so the user can recall prior
/// prompts or explicitly request a hand. While a request is in flight the
/// Generate row is replaced by a non-activatable spinner.
pub fn fan_rows(hand: Option<&SuggestionHand>, history_len: usize, pending: bool) -> Vec<DeckRow> {
    let mut rows = Vec::new();
    if let Some(h) = hand {
        rows.push(DeckRow::Top(h.top.text.clone()));
        for (i, v) in h.verbs.iter().enumerate() {
            rows.push(DeckRow::Verb {
                index: i,
                label: v.label.clone(),
                count: v.cards.len(),
            });
        }
    } else if pending {
        rows.push(DeckRow::Generating);
    }
    // History is always listed so the recall surface is discoverable even
    // before the user has typed anything this session.
    rows.push(DeckRow::History { count: history_len });
    // Generation is an idle action: hide it while a request is already
    // in flight (the Generating row covers that state).
    if !pending {
        rows.push(DeckRow::Generate {
            regenerate: hand.is_some(),
        });
    }
    rows
}

/// Rows for a verb's card list. Empty when the verb index is stale.
pub fn card_rows(hand: Option<&SuggestionHand>, verb: usize) -> Vec<DeckRow> {
    hand.and_then(|h| h.verbs.get(verb))
        .map(|v| {
            v.cards
                .iter()
                .map(|c| DeckRow::Card(c.text.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn fuzzy_history_score(query: &str, text: &str) -> Option<i32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let text = text.to_lowercase();
    if text == query {
        return Some(400);
    }
    if text.starts_with(&query) {
        return Some(300);
    }
    if text.contains(&query) {
        return Some(200);
    }

    // Loose subsequence fallback. Tighter matches win; equal scores keep
    // the history's newest-first order.
    let mut cursor = 0usize;
    let mut first = None;
    let mut last = 0usize;
    for needle in query.chars().filter(|c| !c.is_whitespace()) {
        let found = text[cursor..]
            .char_indices()
            .find_map(|(offset, hay)| (hay == needle).then_some(cursor + offset))?;
        first.get_or_insert(found);
        last = found;
        cursor = found + text[found..].chars().next()?.len_utf8();
    }
    let span = last.saturating_sub(first.unwrap_or(last)) as i32;
    Some(100 - span.min(80))
}

/// Fuzzy-filtered history, newest first for equal matches, capped for display.
pub fn history_rows(history: &[PromptHistoryEntry], query: &str, cap: usize) -> Vec<DeckRow> {
    let mut matches: Vec<(i32, usize, &PromptHistoryEntry)> = history
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            fuzzy_history_score(query, &entry.text).map(|score| (score, index, entry))
        })
        .collect();
    matches.sort_by_key(|(score, index, _)| (std::cmp::Reverse(*score), *index));
    matches
        .into_iter()
        .take(cap)
        .map(|(_, _, entry)| DeckRow::Card(entry.text.clone()))
        .collect()
}

use super::{short_id, App, Minibuffer, MinibufferIntent, PaneFocus};
use construct_protocol::{MessageRole, SessionEvent, SessionState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

/// Matches the daemon's suggestion-probe lifetime cap: past this a
/// silent generation counts as failed and the spinner returns to idle.
pub(crate) const SUGGEST_PENDING_STALE: Duration = Duration::from_millis(125_000);

/// How many history entries the history view shows at most.
pub(crate) const SUGGEST_HISTORY_DISPLAY_CAP: usize = 10;

/// Cap on the two-column body (categories / cards) so a huge hand or
/// history cannot cover the whole session pane. Chrome (borders, header,
/// optional input strip, footer) is counted separately.
pub(crate) const SUGGEST_DECK_MAX_BODY: u16 = 12;

/// Whether the deck reserves a permanent input-strip row in its height
/// (History and Generate both surface one when selected).
pub(crate) fn suggest_reserves_input_row(categories: &[DeckRow]) -> bool {
    categories
        .iter()
        .any(|row| matches!(row, DeckRow::History { .. } | DeckRow::Generate { .. }))
}

impl App {
    pub(crate) fn suggest_pending_active(&self, id: &str) -> bool {
        self.suggest_pending
            .get(id)
            .is_some_and(|at| at.elapsed() < SUGGEST_PENDING_STALE)
    }

    /// Left-column categories, derived fresh so a generated hand can land
    /// while the popup remains open.
    pub(crate) fn suggest_categories(&self, deck: &SuggestDeck) -> Vec<DeckRow> {
        let hand = self.suggestion_hands.get(&deck.session_id);
        fan_rows(
            hand,
            self.prompt_history.len(),
            self.suggest_pending_active(&deck.session_id),
        )
    }

    /// Right-column concrete prompts for the highlighted category.
    pub(crate) fn suggest_cards(&self, deck: &SuggestDeck) -> Vec<DeckRow> {
        let categories = self.suggest_categories(deck);
        match categories.get(deck.category_selected) {
            Some(DeckRow::Top(text)) => vec![DeckRow::Card(text.clone())],
            Some(DeckRow::Verb { index, .. }) => {
                card_rows(self.suggestion_hands.get(&deck.session_id), *index)
            }
            Some(DeckRow::History { .. }) => history_rows(
                &self.prompt_history,
                &deck.history_query,
                SUGGEST_HISTORY_DISPLAY_CAP,
            ),
            Some(DeckRow::Generate { .. }) => Vec::new(),
            Some(DeckRow::Generating) => vec![DeckRow::Generating],
            Some(DeckRow::Card(_)) | None => Vec::new(),
        }
    }

    /// Tallest right-column content across every left-column category
    /// (unfiltered history, full verb card counts). Used to pin the popup
    /// height so switching categories does not resize it.
    pub(crate) fn suggest_max_right_rows(&self, deck: &SuggestDeck) -> usize {
        let categories = self.suggest_categories(deck);
        let mut max = 1usize;
        for row in &categories {
            let n = match row {
                DeckRow::Top(_) => 1,
                DeckRow::Verb { count, .. } => (*count).max(1),
                // Size for the unfiltered history cap so type-ahead filtering
                // does not shrink the popup either.
                DeckRow::History { count } => (*count).min(SUGGEST_HISTORY_DISPLAY_CAP).max(1),
                // Single-line `[ generate ]` action chip under the keyword field.
                DeckRow::Generate { .. } => 1,
                DeckRow::Generating => 1,
                DeckRow::Card(_) => 1,
            };
            max = max.max(n);
        }
        max
    }

    /// `C-x .` / affordance click: toggle the deck for a specific session.
    /// Opening refreshes the global prompt history but does **not** start
    /// generation — the user picks History to recall prior prompts, or
    /// Generate/Regenerate to request a hand (spec 0109: on demand only).
    ///
    /// When `session_id` is `None`, uses the currently selected session
    /// (keyboard chord). Affordance clicks pass the pane's own session so
    /// an unfocused split can open its own deck without borrowing another's
    /// hand / pending state.
    pub(super) async fn toggle_suggest_deck(&mut self) {
        let draft_keywords = self
            .selected_id()
            .and_then(|id| self.suggestion_draft_keywords(&id))
            .filter(|keywords| !keywords.is_empty());
        self.toggle_suggest_deck_for_with_keywords(None, draft_keywords)
            .await;
    }

    pub(super) fn suggestion_draft_keywords(&self, session_id: &str) -> Option<String> {
        self.prompt_drafts
            .get(session_id)
            .map(|draft| draft.buf.as_str())
            .or_else(|| {
                self.editor_states
                    .get(session_id)
                    .map(|editor| editor.buf.as_str())
            })
            .map(str::trim)
            .filter(|keywords| !keywords.is_empty())
            .map(str::to_string)
    }

    pub(super) async fn toggle_suggest_deck_for(&mut self, session_id: Option<String>) {
        self.toggle_suggest_deck_for_with_keywords(session_id, None)
            .await;
    }

    async fn toggle_suggest_deck_for_with_keywords(
        &mut self,
        session_id: Option<String>,
        draft_keywords: Option<String>,
    ) {
        let target = session_id.or_else(|| self.selected_id());
        let Some(id) = target else {
            self.set_status("no session selected".to_string());
            return;
        };
        // Toggle closes only when the open deck already belongs to this
        // session; opening for a different session replaces the previous.
        if self
            .suggest_deck
            .as_ref()
            .is_some_and(|d| d.session_id == id)
        {
            self.suggest_deck = None;
            return;
        }
        // Affordance on an unfocused pane: focus that session so keyboard
        // routing and deck key ownership stay aligned with the painted UI.
        if self.selected_id().as_deref() != Some(id.as_str()) {
            self.select_session(id.clone());
        }
        if let Ok(r) = self.client.prompt_history_list(Some(50)).await {
            self.prompt_history = r.entries;
        }
        let mut deck = SuggestDeck::open(id);
        // fan_rows always yields at least History + Generate (or a spinner
        // while a prior request is still in flight).
        if self.suggest_categories(&deck).is_empty() {
            self.set_status("no suggestions yet — send a prompt first".to_string());
            return;
        }
        if let Some(keywords) = draft_keywords {
            let categories = self.suggest_categories(&deck);
            deck.prefill_generate_keywords(&categories, keywords);
        }
        self.suggest_deck = Some(deck);
    }

    /// Drop a deck that no longer belongs to the selected session (e.g. the
    /// user navigated the list while it was open). Called from selection
    /// changes so chrome never shows another session's deck on the new pane.
    pub(crate) fn close_suggest_deck_if_session_changed(&mut self) {
        let Some(deck) = self.suggest_deck.as_ref() else {
            return;
        };
        if self.selected_id().as_deref() != Some(deck.session_id.as_str()) {
            self.suggest_deck = None;
        }
    }

    /// Deck key routing: returns true when the key was consumed. An
    /// unhandled key closes the deck and returns false so the caller
    /// re-routes the SAME keystroke normally — typing always wins
    /// (spec 0109), and a popup must never own keys it doesn't use.
    pub(super) async fn handle_suggest_deck_key(&mut self, key: KeyEvent) -> bool {
        let Some(deck) = self.suggest_deck.clone() else {
            return false;
        };
        // A selection change since the deck opened orphans it: close and
        // route the key normally.
        if self.selected_id().as_deref() != Some(deck.session_id.as_str()) {
            self.suggest_deck = None;
            return false;
        }
        let categories = self.suggest_categories(&deck);
        let cards = self.suggest_cards(&deck);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // `C-g` is the terminal-wide cancel gesture; keep it exactly
        // equivalent to Escape even while either text surface is active.
        if matches!(key.code, KeyCode::Esc) || (ctrl && matches!(key.code, KeyCode::Char('g'))) {
            self.suggest_deck = None;
            return true;
        }

        let history_selected = matches!(
            categories.get(deck.category_selected),
            Some(DeckRow::History { .. })
        );
        let generate_selected = matches!(
            categories.get(deck.category_selected),
            Some(DeckRow::Generate { .. })
        );
        // History is an explicit text-search surface. Once highlighted,
        // printable input stays here instead of dismissing the popup; all
        // other non-text categories retain "typing always wins".
        if history_selected && !ctrl && !alt {
            match key.code {
                KeyCode::Char(c) => {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.history_query.push(c);
                        d.focus = DeckFocus::Cards;
                        d.card_selected = 0;
                        d.card_scroll = 0;
                    }
                    return true;
                }
                KeyCode::Backspace if !deck.history_query.is_empty() => {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.history_query.pop();
                        d.card_selected = 0;
                        d.card_scroll = 0;
                    }
                    return true;
                }
                _ => {}
            }
        }

        // Generate/Regenerate mirrors History: highlight the left-column
        // row and printable keys auto-enter the right-column keyword field
        // (no →/Enter required). Enter while the right column is focused
        // submits guided (re)generation; Left / empty Backspace returns to
        // the category list without closing the deck.
        if generate_selected && !ctrl && !alt {
            match key.code {
                KeyCode::Char(c) => {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.regenerate_query.get_or_insert_with(String::new).push(c);
                        d.focus = DeckFocus::Cards;
                    }
                    return true;
                }
                KeyCode::Backspace => {
                    let non_empty = deck
                        .regenerate_query
                        .as_ref()
                        .is_some_and(|q| !q.is_empty());
                    if non_empty {
                        if let Some(d) = self.suggest_deck.as_mut() {
                            if let Some(q) = d.regenerate_query.as_mut() {
                                q.pop();
                            }
                        }
                        return true;
                    }
                    // Empty field (or never opened): fall through so Left /
                    // Backspace can leave the surface or close the deck.
                }
                KeyCode::Enter if deck.focus == DeckFocus::Cards => {
                    let keywords = deck.regenerate_query.clone().unwrap_or_default();
                    self.regenerate_suggestions(keywords).await;
                    return true;
                }
                _ => {}
            }
        }

        // Outside History (where `g`/`r` remain ordinary fuzzy-search text)
        // and outside an already-highlighted Generate row (where they type
        // into the keyword field), jump to Generate/Regenerate and open its
        // keyword surface so the highlight tracks.
        if !history_selected
            && !generate_selected
            && !ctrl
            && !alt
            && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('r'))
        {
            if let Some(index) = categories
                .iter()
                .position(|row| matches!(row, DeckRow::Generate { .. }))
            {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.category_selected = index;
                    d.regenerate_query = Some(String::new());
                    d.focus = DeckFocus::Cards;
                }
            }
            return true;
        }

        match key.code {
            KeyCode::Down => self.move_suggest_selection(1),
            KeyCode::Char('n') if ctrl => self.move_suggest_selection(1),
            KeyCode::Up => self.move_suggest_selection(-1),
            KeyCode::Char('p') if ctrl => self.move_suggest_selection(-1),
            KeyCode::Enter => self.activate_suggest_selection(),
            KeyCode::Right => {
                if deck.focus == DeckFocus::Categories {
                    self.activate_suggest_selection();
                }
            }
            KeyCode::Char(c @ '1'..='9') if !ctrl => {
                let idx = (c as usize) - ('1' as usize);
                let len = match deck.focus {
                    DeckFocus::Categories => categories.len(),
                    DeckFocus::Cards => cards.len(),
                };
                if idx < len {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        match d.focus {
                            DeckFocus::Categories => {
                                d.category_selected = idx;
                                d.card_selected = 0;
                                d.card_scroll = 0;
                            }
                            DeckFocus::Cards => d.card_selected = idx,
                        }
                    }
                    self.ensure_suggest_selection_visible();
                    self.activate_suggest_selection();
                }
            }
            KeyCode::Char('h') if !ctrl && !self.prompt_history.is_empty() => {
                if let Some(index) = categories
                    .iter()
                    .position(|row| matches!(row, DeckRow::History { .. }))
                {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.category_selected = index;
                        d.card_selected = 0;
                        d.card_scroll = 0;
                        d.focus = DeckFocus::Cards;
                    }
                    self.ensure_suggest_selection_visible();
                }
            }
            KeyCode::Tab => {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.focus = match d.focus {
                        DeckFocus::Categories => DeckFocus::Cards,
                        DeckFocus::Cards => DeckFocus::Categories,
                    };
                }
            }
            // Left/Backspace hand focus back to categories; from categories
            // they close. History / Generate Backspace edits a non-empty
            // query above. Leaving the Generate keyword surface clears it.
            KeyCode::Left | KeyCode::Backspace => match deck.focus {
                DeckFocus::Categories => self.suggest_deck = None,
                DeckFocus::Cards => {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.focus = DeckFocus::Categories;
                        if generate_selected {
                            d.regenerate_query = None;
                        }
                    }
                }
            },
            _ => {
                self.suggest_deck = None;
                return false;
            }
        }
        true
    }

    async fn regenerate_suggestions(&mut self, keywords: String) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        let guidance = keywords.trim();
        match self
            .client
            .suggest_with_keywords(&deck.session_id, (!guidance.is_empty()).then_some(guidance))
            .await
        {
            Ok(result) if result.started => {
                self.suggestion_hands.remove(&deck.session_id);
                self.suggest_pending
                    .insert(deck.session_id.clone(), Instant::now());
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.regenerate_query = None;
                    d.focus = DeckFocus::Categories;
                    d.category_selected = 0;
                    d.card_selected = 0;
                }
            }
            Ok(_) => {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.regenerate_query = None;
                }
                self.set_status("suggestion regeneration unavailable".to_string());
            }
            Err(error) => {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.regenerate_query = None;
                }
                self.set_status(format!("suggestion regeneration failed: {error}"));
            }
        }
    }

    fn move_suggest_selection(&mut self, delta: isize) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        let categories_len = self.suggest_categories(&deck).len();
        let cards_len = self.suggest_cards(&deck).len();
        let len = match deck.focus {
            DeckFocus::Categories => categories_len,
            DeckFocus::Cards => cards_len,
        };
        if len == 0 {
            return;
        }
        let visible = self.layout.suggest_deck_visible_rows.max(1);
        if let Some(d) = self.suggest_deck.as_mut() {
            match d.focus {
                DeckFocus::Categories => {
                    let cur = d.category_selected as isize;
                    d.category_selected = (cur + delta).rem_euclid(len as isize) as usize;
                    d.card_selected = 0;
                    d.card_scroll = 0;
                    SuggestDeck::ensure_visible(
                        &mut d.category_scroll,
                        d.category_selected,
                        categories_len,
                        visible,
                    );
                }
                DeckFocus::Cards => {
                    let cur = d.card_selected as isize;
                    d.card_selected = (cur + delta).rem_euclid(len as isize) as usize;
                    SuggestDeck::ensure_visible(
                        &mut d.card_scroll,
                        d.card_selected,
                        cards_len,
                        visible,
                    );
                }
            }
        }
    }

    /// Mouse wheel over the suggestion deck. Scrolls the column under the
    /// pointer (divider decides left/right); falls back to the focused
    /// column when the pointer is over chrome. Returns true when consumed.
    pub(super) fn scroll_suggest_deck(&mut self, col: u16, row: u16, delta: isize) -> bool {
        let Some(area) = self.layout.suggest_deck_area else {
            return false;
        };
        if !Self::rect_contains(area, col, row) {
            return false;
        }
        let Some(deck) = self.suggest_deck.clone() else {
            return false;
        };
        let visible = self.layout.suggest_deck_visible_rows.max(1);
        let categories_len = self.suggest_categories(&deck).len();
        let cards_len = self.suggest_cards(&deck).len();
        let left = self
            .layout
            .suggest_deck_divider_x
            .is_some_and(|div| col <= div)
            || (self.layout.suggest_deck_divider_x.is_none()
                && deck.focus == DeckFocus::Categories);
        if let Some(d) = self.suggest_deck.as_mut() {
            if left {
                let max = categories_len.saturating_sub(visible);
                if max > 0 {
                    let next = (d.category_scroll as isize + delta).clamp(0, max as isize) as usize;
                    d.category_scroll = next;
                }
            } else {
                let max = cards_len.saturating_sub(visible);
                if max > 0 {
                    let next = (d.card_scroll as isize + delta).clamp(0, max as isize) as usize;
                    d.card_scroll = next;
                }
            }
        }
        true
    }

    fn ensure_suggest_selection_visible(&mut self) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        let visible = self.layout.suggest_deck_visible_rows.max(1);
        let categories_len = self.suggest_categories(&deck).len();
        let cards_len = self.suggest_cards(&deck).len();
        if let Some(d) = self.suggest_deck.as_mut() {
            SuggestDeck::ensure_visible(
                &mut d.category_scroll,
                d.category_selected,
                categories_len,
                visible,
            );
            SuggestDeck::ensure_visible(&mut d.card_scroll, d.card_selected, cards_len, visible);
        }
    }

    fn activate_suggest_selection(&mut self) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        match deck.focus {
            DeckFocus::Categories => {
                let categories = self.suggest_categories(&deck);
                let Some(row) = categories.get(deck.category_selected) else {
                    return;
                };
                if matches!(row, DeckRow::Generate { .. }) {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.regenerate_query = Some(String::new());
                        d.focus = DeckFocus::Cards;
                        d.card_scroll = 0;
                    }
                } else if row.is_activatable() {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.focus = DeckFocus::Cards;
                        d.card_selected = 0;
                        d.card_scroll = 0;
                    }
                    self.ensure_suggest_selection_visible();
                }
            }
            DeckFocus::Cards => {
                let cards = self.suggest_cards(&deck);
                let Some(DeckRow::Card(text)) = cards.get(deck.card_selected) else {
                    return;
                };
                let text = text.clone();
                self.stage_suggestion(&deck.session_id, text);
            }
        }
    }

    pub(super) async fn hit_suggest_deck(&mut self, hit: SuggestDeckHit) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        match hit {
            SuggestDeckHit::Category(index) => {
                let categories = self.suggest_categories(&deck);
                let Some(row) = categories.get(index) else {
                    return;
                };
                if !row.is_activatable() {
                    return;
                }
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.category_selected = index;
                    d.card_selected = 0;
                    d.card_scroll = 0;
                    d.focus = DeckFocus::Categories;
                    // Leaving another category for Generate keeps the shared
                    // right-column surface; clear any stale keyword field so
                    // a fresh highlight matches a fresh open.
                    if !matches!(row, DeckRow::Generate { .. }) {
                        d.regenerate_query = None;
                    }
                }
                self.ensure_suggest_selection_visible();
                if matches!(row, DeckRow::Generate { .. }) {
                    self.activate_suggest_selection();
                }
            }
            SuggestDeckHit::Card(index) => {
                let cards = self.suggest_cards(&deck);
                if !cards.get(index).is_some_and(DeckRow::is_activatable) {
                    return;
                }
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.card_selected = index;
                    d.focus = DeckFocus::Cards;
                }
                self.ensure_suggest_selection_visible();
                self.activate_suggest_selection();
            }
            SuggestDeckHit::GenerateAction => {
                let keywords = deck.regenerate_query.clone().unwrap_or_default();
                // Ensure the Generate row is focused so a button click after
                // browsing still targets the right session surface.
                if let Some(index) = self
                    .suggest_categories(&deck)
                    .iter()
                    .position(|row| matches!(row, DeckRow::Generate { .. }))
                {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.category_selected = index;
                        d.focus = DeckFocus::Cards;
                        d.regenerate_query.get_or_insert_with(|| keywords.clone());
                    }
                }
                self.regenerate_suggestions(keywords).await;
            }
        }
    }

    /// Stage — never send (spec 0109): PTY sessions get the text typed
    /// into the harness's own prompt line (no Enter appended), non-PTY
    /// sessions get the send-input minibuffer prefilled. Either way the
    /// user reviews and submits with the ordinary send gesture. The hand
    /// stays cached so reopening the deck offers the other picks.
    fn stage_suggestion(&mut self, session_id: &str, text: String) {
        self.suggest_deck = None;
        let pty = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .is_some_and(|s| s.has_pty && s.mode.as_deref() != Some("headless"));
        if pty {
            self.queue_pty_input(session_id.to_string(), text.into_bytes(), "suggestion");
            self.focus = PaneFocus::View;
        } else {
            let cursor = text.chars().count();
            self.minibuffer = Some(Minibuffer {
                prompt: format!("Send to {}: ", short_id(session_id)),
                input: text,
                cursor,
                intent: MinibufferIntent::SendInput {
                    session_id: session_id.to_string(),
                },
                error: None,
            });
        }
    }

    /// Fold a broadcast session event into the suggestion caches: a
    /// dealt hand lands; any turn movement (new turn, user message,
    /// terminal state, reset) invalidates the hand and closes the deck
    /// if it was open for that session (spec 0109). Turn boundaries also
    /// reset the draft sentinel used when opening Generate, so a stale
    /// editor snapshot cannot repopulate the keyword field.
    pub(crate) fn observe_suggestion_event(&mut self, session_id: &str, event: &SessionEvent) {
        match event {
            SessionEvent::Suggestions(hand) => {
                let preserve_history = self.suggest_deck.as_ref().is_some_and(|deck| {
                    deck.session_id == session_id
                        && matches!(
                            self.suggest_categories(deck).get(deck.category_selected),
                            Some(DeckRow::History { .. })
                        )
                });
                self.suggestion_hands
                    .insert(session_id.to_string(), hand.clone());
                self.suggest_pending.remove(session_id);
                if preserve_history {
                    let history_index = self.suggest_deck.as_ref().and_then(|deck| {
                        self.suggest_categories(deck)
                            .iter()
                            .position(|row| matches!(row, DeckRow::History { .. }))
                    });
                    if let (Some(index), Some(deck)) = (history_index, self.suggest_deck.as_mut()) {
                        deck.category_selected = index;
                    }
                }
            }
            SessionEvent::Status {
                state: SessionState::Running,
                ..
            }
            | SessionEvent::Message {
                role: MessageRole::User,
                ..
            }
            | SessionEvent::Done { .. }
            | SessionEvent::Error { .. }
            | SessionEvent::Reset => {
                self.clear_suggestion_draft(session_id);
                self.suggestion_hands.remove(session_id);
                self.suggest_pending.remove(session_id);
                if self
                    .suggest_deck
                    .as_ref()
                    .is_some_and(|d| d.session_id == session_id)
                {
                    self.suggest_deck = None;
                }
            }
            SessionEvent::AgentStatus(status) if !status.active => {
                self.clear_suggestion_draft(session_id);
            }
            SessionEvent::Status {
                state: SessionState::AwaitingInput,
                ..
            } => {
                self.clear_suggestion_draft(session_id);
            }
            _ => {}
        }
    }

    /// Keep an explicit empty draft after a turn boundary. Removing the
    /// optimistic draft would make `suggestion_draft_keywords` fall back to
    /// an older adapter editor snapshot until the next `EditorState` arrives.
    fn clear_suggestion_draft(&mut self, session_id: &str) {
        self.prompt_drafts
            .insert(session_id.to_string(), super::PromptDraft::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use construct_protocol::{SuggestionCard, SuggestionVerb};

    fn hand() -> SuggestionHand {
        SuggestionHand {
            top: SuggestionCard {
                text: "run the tests".into(),
            },
            verbs: vec![
                SuggestionVerb {
                    label: "ship it".into(),
                    cards: vec![
                        SuggestionCard {
                            text: "open a PR".into(),
                        },
                        SuggestionCard {
                            text: "merge it".into(),
                        },
                    ],
                },
                SuggestionVerb {
                    label: "dig deeper".into(),
                    cards: vec![SuggestionCard {
                        text: "add a test".into(),
                    }],
                },
            ],
        }
    }

    fn history(n: usize) -> Vec<PromptHistoryEntry> {
        (0..n)
            .map(|i| PromptHistoryEntry {
                text: format!("p{i}"),
                at_ms: i as i64,
                session_id: None,
                harness: None,
            })
            .collect()
    }
    #[test]
    fn fan_orders_top_verbs_history() {
        let h = hand();
        let rows = fan_rows(Some(&h), 3, false);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], DeckRow::Top("run the tests".into()));
        assert!(matches!(rows[1], DeckRow::Verb { index: 0, .. }));
        assert!(matches!(rows[2], DeckRow::Verb { index: 1, .. }));
        assert_eq!(rows[3], DeckRow::History { count: 3 });
        assert_eq!(rows[4], DeckRow::Generate { regenerate: true });
    }

    #[test]
    fn fan_without_hand_shows_history_and_generate() {
        assert_eq!(
            fan_rows(None, 0, false),
            vec![
                DeckRow::History { count: 0 },
                DeckRow::Generate { regenerate: false },
            ]
        );
        let pending = fan_rows(None, 2, true);
        assert_eq!(pending[0], DeckRow::Generating);
        assert_eq!(pending[1], DeckRow::History { count: 2 });
        assert_eq!(pending.len(), 2, "Generate hides while loading");
        assert!(!pending[0].is_activatable());
    }

    #[test]
    fn card_rows_fall_back_on_stale_verb_index() {
        let h = hand();
        assert_eq!(card_rows(Some(&h), 0).len(), 2);
        assert!(card_rows(Some(&h), 9).is_empty());
        assert!(card_rows(None, 0).is_empty());
    }

    #[test]
    fn history_rows_cap_display() {
        let rows = history_rows(&history(30), "", 10);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0], DeckRow::Card("p0".into()));
    }

    #[test]
    fn ensure_visible_scrolls_selection_into_viewport() {
        let mut scroll = 0usize;
        SuggestDeck::ensure_visible(&mut scroll, 14, 20, 5);
        assert_eq!(scroll, 10, "selection 14 needs scroll so 10..15 is shown");
        SuggestDeck::ensure_visible(&mut scroll, 2, 20, 5);
        assert_eq!(scroll, 2, "selection above the viewport pulls scroll up");
        SuggestDeck::ensure_visible(&mut scroll, 4, 20, 5);
        assert_eq!(scroll, 2, "already-visible selection leaves scroll alone");
        SuggestDeck::clamp_scroll(&mut scroll, 3, 5);
        assert_eq!(scroll, 0, "clamp when content fits the viewport");
    }

    #[test]
    fn draft_keywords_open_generate_without_consuming_the_draft() {
        let categories = fan_rows(None, 0, false);
        let mut deck = SuggestDeck::open("s1".into());

        assert!(deck.prefill_generate_keywords(&categories, "test the CLI".into()));
        assert_eq!(deck.category_selected, 1);
        assert_eq!(deck.focus, DeckFocus::Cards);
        assert_eq!(deck.regenerate_query.as_deref(), Some("test the CLI"));
    }

    #[test]
    fn draft_keywords_do_not_open_a_missing_generate_row() {
        let categories = fan_rows(None, 0, true);
        let mut deck = SuggestDeck::open("s1".into());

        assert!(!deck.prefill_generate_keywords(&categories, "test the CLI".into()));
        assert_eq!(deck.focus, DeckFocus::Categories);
        assert!(deck.regenerate_query.is_none());
    }

    #[test]
    fn max_right_rows_uses_tallest_category() {
        // Verb with 2 cards is taller than top (1) or history (when empty
        // query would still count history entries).
        let h = hand();
        let rows = fan_rows(Some(&h), 3, false);
        // Simulate App::suggest_max_right_rows logic without a full App:
        let max = rows
            .iter()
            .map(|row| match row {
                DeckRow::Top(_) => 1,
                DeckRow::Verb { count, .. } => (*count).max(1),
                DeckRow::History { count } => (*count).min(SUGGEST_HISTORY_DISPLAY_CAP).max(1),
                DeckRow::Generate { .. } => 1,
                DeckRow::Generating | DeckRow::Card(_) => 1,
            })
            .max()
            .unwrap_or(1);
        assert_eq!(max, 3, "history count 3 beats verb cards 2");
        assert!(suggest_reserves_input_row(&rows));
    }

    #[test]
    fn history_rows_fuzzy_filter_and_rank() {
        let entries = vec![
            PromptHistoryEntry {
                text: "cargo build --workspace".into(),
                at_ms: 3,
                session_id: None,
                harness: None,
            },
            PromptHistoryEntry {
                text: "commit the branch".into(),
                at_ms: 2,
                session_id: None,
                harness: None,
            },
            PromptHistoryEntry {
                text: "check background tasks".into(),
                at_ms: 1,
                session_id: None,
                harness: None,
            },
        ];
        assert_eq!(
            history_rows(&entries, "c b", 10),
            vec![
                DeckRow::Card("cargo build --workspace".into()),
                DeckRow::Card("check background tasks".into()),
                DeckRow::Card("commit the branch".into()),
            ]
        );
        assert_eq!(
            history_rows(&entries, "bgtsk", 10),
            vec![DeckRow::Card("check background tasks".into())]
        );
    }
}
