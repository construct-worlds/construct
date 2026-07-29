//! Suggestion deck (specs 0109/0155) — the TUI surface.
//!
//! `C-x s` on an awaiting-input session requests generation via
//! `session.suggest` and opens a compact popup anchored above the
//! modeline: the dealt hand's top pick and verbs as rows, plus a
//! `history` row backed by the global prompt history. The popup is
//! never modal — it owns only its navigation keys, and any other key
//! closes it and takes its normal route (typing always wins).
//!
//! Accepting a row never sends: for PTY sessions the text is typed
//! into the harness's own prompt line (no Enter), for non-PTY sessions
//! it prefills the send-input minibuffer — either way the user reviews
//! and submits with the ordinary send gesture, matching the webui
//! composer behavior and spec 0109's staging rule.

use construct_protocol::{PromptHistoryEntry, SuggestionHand};

/// Which level of the deck the popup is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckView {
    /// Top pick + verb rows + history row.
    Fan,
    /// The cards of one verb.
    Cards { verb: usize },
    /// The global prompt history, newest first.
    History,
}

/// Open-popup state. The hand and history themselves live on `App`
/// (cached per session / globally); this is only the view cursor.
#[derive(Debug, Clone)]
pub struct SuggestDeck {
    /// Session the deck was opened for. A selection change closes it.
    pub session_id: String,
    pub view: DeckView,
    /// Highlighted row index within the current view's rows.
    pub selected: usize,
}

impl SuggestDeck {
    pub fn open(session_id: String) -> Self {
        Self {
            session_id,
            view: DeckView::Fan,
            selected: 0,
        }
    }
}

/// One selectable row of the popup, precomputed per view so key
/// handling and rendering agree on ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckRow {
    /// The hand's top pick — activating stages this text.
    Top(String),
    /// A verb chip — activating opens its cards.
    Verb { index: usize, label: String, count: usize },
    /// The global-history entry row — activating opens the history view.
    History { count: usize },
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

/// Rows for the fan view. The hand may not have arrived yet (the deck
/// opens immediately on request so history stays reachable while the
/// spinner runs); an empty result means there is nothing to show and
/// the deck should not open.
pub fn fan_rows(
    hand: Option<&SuggestionHand>,
    history_len: usize,
    pending: bool,
) -> Vec<DeckRow> {
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
    if history_len > 0 {
        rows.push(DeckRow::History { count: history_len });
    }
    // A lone Generating row carries no selectable content, but the
    // popup still opens for it: it is the request's only feedback.
    rows
}

/// Rows for a verb's card list. Empty when the verb index is stale
/// (hand replaced while the view was open) — callers fall back to Fan.
pub fn card_rows(hand: Option<&SuggestionHand>, verb: usize) -> Vec<DeckRow> {
    hand.and_then(|h| h.verbs.get(verb))
        .map(|v| v.cards.iter().map(|c| DeckRow::Card(c.text.clone())).collect())
        .unwrap_or_default()
}

/// Rows for the history view, newest first, capped for display.
pub fn history_rows(history: &[PromptHistoryEntry], cap: usize) -> Vec<DeckRow> {
    history
        .iter()
        .take(cap)
        .map(|e| DeckRow::Card(e.text.clone()))
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

impl App {
    pub(crate) fn suggest_pending_active(&self, id: &str) -> bool {
        self.suggest_pending
            .get(id)
            .is_some_and(|at| at.elapsed() < SUGGEST_PENDING_STALE)
    }

    /// The deck rows for the popup's current view, derived fresh from
    /// the caches so key handling and rendering always agree.
    pub(crate) fn suggest_rows(&self, deck: &SuggestDeck) -> Vec<DeckRow> {
        let hand = self.suggestion_hands.get(&deck.session_id);
        match deck.view {
            DeckView::Fan => fan_rows(
                hand,
                self.prompt_history.len(),
                self.suggest_pending_active(&deck.session_id),
            ),
            DeckView::Cards { verb } => card_rows(hand, verb),
            DeckView::History => history_rows(&self.prompt_history, SUGGEST_HISTORY_DISPLAY_CAP),
        }
    }

    /// `C-x s`: toggle the deck. Opening refreshes the global prompt
    /// history and, when the session is at a turn boundary with no hand
    /// cached and no request in flight, kicks off generation — the deck
    /// opens immediately either way so history stays reachable while
    /// the spinner runs (mirrors the web deck).
    pub(super) async fn toggle_suggest_deck(&mut self) {
        if self.suggest_deck.is_some() {
            self.suggest_deck = None;
            return;
        }
        let Some(id) = self.selected_id() else {
            self.set_status("no session selected".to_string());
            return;
        };
        if let Ok(r) = self.client.prompt_history_list(Some(50)).await {
            self.prompt_history = r.entries;
        }
        let awaiting = self
            .selected_session()
            .is_some_and(|s| s.state == SessionState::AwaitingInput);
        if awaiting
            && !self.suggestion_hands.contains_key(&id)
            && !self.suggest_pending_active(&id)
        {
            if let Ok(r) = self.client.suggest(&id).await {
                if r.started {
                    self.suggest_pending.insert(id.clone(), Instant::now());
                }
            }
        }
        let deck = SuggestDeck::open(id);
        if self.suggest_rows(&deck).is_empty() {
            self.set_status("no suggestions yet — send a prompt first".to_string());
            return;
        }
        self.suggest_deck = Some(deck);
    }

    /// Deck key routing: returns true when the key was consumed. An
    /// unhandled key closes the deck and returns false so the caller
    /// re-routes the SAME keystroke normally — typing always wins
    /// (spec 0109), and a popup must never own keys it doesn't use.
    pub(super) fn handle_suggest_deck_key(&mut self, key: KeyEvent) -> bool {
        let Some(deck) = self.suggest_deck.clone() else {
            return false;
        };
        // A selection change since the deck opened orphans it: close and
        // route the key normally.
        if self.selected_id().as_deref() != Some(deck.session_id.as_str()) {
            self.suggest_deck = None;
            return false;
        }
        let rows = self.suggest_rows(&deck);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down => self.move_suggest_selection(1),
            KeyCode::Char('n') if ctrl => self.move_suggest_selection(1),
            KeyCode::Up => self.move_suggest_selection(-1),
            KeyCode::Char('p') if ctrl => self.move_suggest_selection(-1),
            KeyCode::Enter => self.activate_suggest_row(deck.selected),
            KeyCode::Char(c @ '1'..='9') if !ctrl => {
                let idx = (c as usize) - ('1' as usize);
                if idx < rows.len() {
                    self.activate_suggest_row(idx);
                }
            }
            KeyCode::Char('h')
                if !ctrl
                    && matches!(deck.view, DeckView::Fan)
                    && !self.prompt_history.is_empty() =>
            {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.view = DeckView::History;
                    d.selected = 0;
                }
            }
            // Left/Backspace step back one level; from the fan they close.
            KeyCode::Left | KeyCode::Backspace => match deck.view {
                DeckView::Fan => self.suggest_deck = None,
                _ => {
                    if let Some(d) = self.suggest_deck.as_mut() {
                        d.view = DeckView::Fan;
                        d.selected = 0;
                    }
                }
            },
            KeyCode::Esc => self.suggest_deck = None,
            _ => {
                self.suggest_deck = None;
                return false;
            }
        }
        true
    }

    fn move_suggest_selection(&mut self, delta: isize) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        let len = self.suggest_rows(&deck).len();
        if len == 0 {
            return;
        }
        if let Some(d) = self.suggest_deck.as_mut() {
            let cur = d.selected as isize;
            d.selected = (cur + delta).rem_euclid(len as isize) as usize;
        }
    }

    fn activate_suggest_row(&mut self, index: usize) {
        let Some(deck) = self.suggest_deck.clone() else {
            return;
        };
        let rows = self.suggest_rows(&deck);
        let Some(row) = rows.get(index) else {
            return;
        };
        match row {
            DeckRow::Top(text) | DeckRow::Card(text) => {
                let text = text.clone();
                self.stage_suggestion(&deck.session_id, text);
            }
            DeckRow::Verb { index, .. } => {
                let verb = *index;
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.view = DeckView::Cards { verb };
                    d.selected = 0;
                }
            }
            DeckRow::History { .. } => {
                if let Some(d) = self.suggest_deck.as_mut() {
                    d.view = DeckView::History;
                    d.selected = 0;
                }
            }
            DeckRow::Generating => {}
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
            self.set_status("suggestion staged — Enter in the session sends it".to_string());
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
    /// if it was open for that session (spec 0109).
    pub(crate) fn observe_suggestion_event(&mut self, session_id: &str, event: &SessionEvent) {
        match event {
            SessionEvent::Suggestions(hand) => {
                self.suggestion_hands
                    .insert(session_id.to_string(), hand.clone());
                self.suggest_pending.remove(session_id);
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
            _ => {}
        }
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
                        SuggestionCard { text: "open a PR".into() },
                        SuggestionCard { text: "merge it".into() },
                    ],
                },
                SuggestionVerb {
                    label: "dig deeper".into(),
                    cards: vec![SuggestionCard { text: "add a test".into() }],
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
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0], DeckRow::Top("run the tests".into()));
        assert!(matches!(rows[1], DeckRow::Verb { index: 0, .. }));
        assert!(matches!(rows[2], DeckRow::Verb { index: 1, .. }));
        assert_eq!(rows[3], DeckRow::History { count: 3 });
    }

    #[test]
    fn fan_without_hand_shows_spinner_then_history() {
        let rows = fan_rows(None, 2, true);
        assert_eq!(rows[0], DeckRow::Generating);
        assert_eq!(rows[1], DeckRow::History { count: 2 });
        assert!(!rows[0].is_activatable());
        // Not pending and nothing cached: nothing to show.
        assert!(fan_rows(None, 0, false).is_empty());
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
        let rows = history_rows(&history(30), 10);
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0], DeckRow::Card("p0".into()));
    }
}
