//! Fork + subagent lineage view popup (`C-x q` / `q`, spec
//! 0079-fork-and-subagent-lineage-view): a live tree of a session's fork
//! lineage (spec 0078) and subagent parent/child relationships (spec 0014),
//! replacing the old flat fork-count status line.
//!
//! The tree structure and layout live in [`crate::lineage`], independent of
//! `App` — this module only wires that pure logic to live session data,
//! keyboard navigation, and the existing merge/discard action (reused via
//! [`App::apply_fork_merge`], not reimplemented).

use super::*;
use crate::lineage::LineageRow;

/// `App::lineage_popup == None` means closed. Rows are rebuilt from live
/// `App::sessions` on every render/key handling call (see
/// [`App::lineage_rows`]), so a fork completing or merging while the popup
/// is open is reflected immediately without any popup-owned copy of session
/// data going stale.
#[derive(Debug, Clone)]
pub struct LineagePopup {
    /// The session the popup was opened for. The rendered tree is rooted at
    /// its topmost fork/subagent ancestor (see [`crate::lineage::build_tree`]),
    /// so this need not itself be the tree's root.
    pub focus_id: String,
    /// Logical index into the current flattened, selectable (non-`More`)
    /// rows. Clamped at use — a merge/discard or a live session change can
    /// shrink the list out from under a stale index.
    pub selected: usize,
    /// First visible raw-row index, clamped at render time.
    pub scroll: usize,
}

impl App {
    /// Open the popup for the selected session (`C-x q` / `q`). Requires a
    /// selected session — sets a status note and does nothing otherwise.
    pub fn open_lineage_popup(&mut self) {
        let Some(id) = self.selected_id() else {
            self.set_status("lineage: no session selected".to_string());
            return;
        };
        self.chord_state = ChordState::default();
        self.chord_label.clear();
        self.lineage_popup = Some(LineagePopup {
            focus_id: id,
            selected: 0,
            scroll: 0,
        });
    }

    /// Materialize the popup's current rows from live session data. Empty
    /// when the popup is closed or its focus session has since disappeared
    /// (e.g. deleted while the popup was open).
    pub(crate) fn lineage_rows(&self) -> Vec<LineageRow> {
        let Some(popup) = self.lineage_popup.as_ref() else {
            return Vec::new();
        };
        crate::lineage::build_tree(&popup.focus_id, &self.sessions)
            .map(|root| crate::lineage::flatten(&root))
            .unwrap_or_default()
    }

    fn lineage_selectable_indices(rows: &[LineageRow]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, r)| r.is_selectable())
            .map(|(i, _)| i)
            .collect()
    }

    fn move_lineage_selection(&mut self, delta: isize) {
        let rows = self.lineage_rows();
        let selectable = Self::lineage_selectable_indices(&rows);
        let Some(popup) = self.lineage_popup.as_mut() else {
            return;
        };
        if selectable.is_empty() {
            popup.selected = 0;
            return;
        }
        let count = selectable.len();
        let current = popup.selected.min(count - 1);
        popup.selected = if delta < 0 {
            current
                .saturating_add(count)
                .saturating_sub(delta.unsigned_abs() % count)
                % count
        } else {
            (current + delta as usize) % count
        };
    }

    /// The session id of the currently-highlighted row, if any (never a
    /// `More` marker row — those aren't selectable).
    fn lineage_selected_session_id(&self) -> Option<String> {
        let popup = self.lineage_popup.as_ref()?;
        let rows = self.lineage_rows();
        let selectable = Self::lineage_selectable_indices(&rows);
        if selectable.is_empty() {
            return None;
        }
        let idx = selectable[popup.selected.min(selectable.len() - 1)];
        rows[idx].session_id().map(|s| s.to_string())
    }

    /// Enter: jump into the highlighted session. A *merged* fork instead
    /// jumps to its parent — the merge point in the graph and the injected
    /// result message are the same transcript event (spec 0078), so this
    /// links to where that event actually lives instead of re-showing the
    /// now-archived fork.
    fn confirm_lineage_selection(&mut self) {
        let Some(id) = self.lineage_selected_session_id() else {
            self.lineage_popup = None;
            return;
        };
        let Some(summary) = self.sessions.iter().find(|s| s.id == id).cloned() else {
            self.set_status(format!("session {} no longer exists", short_id(&id)));
            self.lineage_popup = None;
            return;
        };
        let target = match (&summary.forked_from, &summary.merge) {
            (Some(f), Some(m)) if m.mode == agentd_protocol::ForkMergeMode::Result => {
                f.session_id.clone()
            }
            _ => id,
        };
        self.lineage_popup = None;
        self.select_session(target);
        self.sync_active_window_selection();
        self.focus = PaneFocus::View;
    }

    /// `m` / `d`: merge or discard the highlighted fork, reusing the exact
    /// merge/discard path the `C-x m` minibuffer menu uses
    /// ([`App::apply_fork_merge`], spec 0078) — a direct-key shortcut for
    /// it, not a second implementation. A no-op with a status note when the
    /// highlighted row isn't an open (unmerged, undiscarded) fork.
    async fn lineage_merge_or_discard(&mut self, mode: agentd_protocol::ForkMergeMode) {
        let Some(id) = self.lineage_selected_session_id() else {
            return;
        };
        let is_open_fork = self
            .sessions
            .iter()
            .any(|s| s.id == id && s.forked_from.is_some() && s.merge.is_none());
        if !is_open_fork {
            self.set_status("merge: select an open fork".to_string());
            return;
        }
        self.apply_fork_merge(id, mode).await;
        // The popup stays open — its rows are rebuilt from live
        // `self.sessions` on the very next render/key, so the merged
        // fork's terminal-state styling appears immediately without any
        // extra bookkeeping here.
    }

    /// Route a key while the popup owns input. Navigation/merge/discard/jump
    /// keys return `true` (fully handled; the popup stays open unless it was
    /// Esc). Anything else closes the popup and returns `false`, telling the
    /// caller (`App::on_key`) to re-dispatch the SAME key through ordinary
    /// routing — the same "a closing overlay never eats a live keystroke"
    /// rule `handle_configure_key` documents, so e.g. `C-x C-c` still quits
    /// and `C-x b` still switches sessions with this popup open.
    pub(super) async fn handle_lineage_popup_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.lineage_popup = None;
                true
            }
            // `C-p`/`C-n` mirror the session-list's own emacs-style
            // NextSession/PrevSession bindings so navigation muscle memory
            // carries into this popup; `key.code` alone (crossterm reports
            // Ctrl+letter as `Char` with a CONTROL modifier, not a distinct
            // code) means these arms also accept bare `p`/`n`, same as `k`/`j`
            // below accept bare Up/Down without checking modifiers.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                self.move_lineage_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                self.move_lineage_selection(1);
                true
            }
            KeyCode::Enter => {
                self.confirm_lineage_selection();
                true
            }
            KeyCode::Char('m') => {
                self.lineage_merge_or_discard(agentd_protocol::ForkMergeMode::Result)
                    .await;
                true
            }
            KeyCode::Char('d') => {
                self.lineage_merge_or_discard(agentd_protocol::ForkMergeMode::Discard)
                    .await;
                true
            }
            _ => {
                self.lineage_popup = None;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            harness: "smith".into(),
            cwd: "/tmp".into(),
            title: None,
            state: agentd_protocol::SessionState::Running,
            created_at: chrono::Utc::now(),
            last_event_at: None,
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 0,
            has_pty: false,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: agentd_protocol::SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    async fn test_app_with_sessions(
        sessions: Vec<SessionSummary>,
    ) -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok(_conn) = listener.accept().await else {
                    break;
                };
            }
        });
        let client = agentd_client::Client::connect(&sock)
            .await
            .expect("client connects");
        let app = crate::app::tests::test_app(client, sessions);
        (app, dir, server)
    }

    #[tokio::test]
    async fn open_lineage_popup_requires_a_selection() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![]).await;
        app.open_lineage_popup();
        assert!(app.lineage_popup.is_none());
    }

    #[tokio::test]
    async fn open_lineage_popup_builds_rows_for_the_selected_session() {
        let mut fork = summary("fork");
        fork.forked_from = Some(agentd_protocol::ForkedFrom {
            session_id: "root".into(),
            transcript_seq: 0,
            at_ms: 0,
        });
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        assert!(app.lineage_popup.is_some());
        let rows = app.lineage_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id(), Some("root"));
        assert_eq!(rows[1].session_id(), Some("fork"));
    }

    #[tokio::test]
    async fn ctrl_n_and_ctrl_p_navigate_like_j_and_k() {
        let mut fork = summary("fork");
        fork.forked_from = Some(agentd_protocol::ForkedFrom {
            session_id: "root".into(),
            transcript_seq: 0,
            at_ms: 0,
        });
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        assert_eq!(app.lineage_popup.as_ref().unwrap().selected, 0);
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.lineage_popup.as_ref().unwrap().selected, 1);
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.lineage_popup.as_ref().unwrap().selected, 0);
    }

    #[tokio::test]
    async fn esc_closes_the_popup() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        assert!(
            app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await
        );
        assert!(app.lineage_popup.is_none());
    }

    #[tokio::test]
    async fn unhandled_key_closes_the_popup_and_reports_unhandled() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        let handled = app
            .handle_lineage_popup_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .await;
        assert!(
            !handled,
            "an unbound key must fall through to ordinary routing"
        );
        assert!(app.lineage_popup.is_none());
    }

    #[tokio::test]
    async fn enter_jumps_into_the_selected_session() {
        let mut fork = summary("fork");
        fork.forked_from = Some(agentd_protocol::ForkedFrom {
            session_id: "root".into(),
            transcript_seq: 0,
            at_ms: 0,
        });
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        // Move down onto the fork row.
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.lineage_popup.is_none());
        assert_eq!(app.selected_id().as_deref(), Some("fork"));
    }

    #[tokio::test]
    async fn enter_on_a_merged_fork_jumps_to_the_parent_instead() {
        let mut fork = summary("fork");
        fork.forked_from = Some(agentd_protocol::ForkedFrom {
            session_id: "root".into(),
            transcript_seq: 0,
            at_ms: 0,
        });
        fork.merge = Some(agentd_protocol::ForkMerge {
            mode: agentd_protocol::ForkMergeMode::Result,
            at_ms: 0,
        });
        fork.archived = true;
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selected_id().as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn merge_on_a_non_fork_row_is_a_status_only_no_op() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.open_lineage_popup();
        assert!(
            app.handle_lineage_popup_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
                .await
        );
        // Still open — nothing to merge, just a status note.
        assert!(app.lineage_popup.is_some());
    }
}
