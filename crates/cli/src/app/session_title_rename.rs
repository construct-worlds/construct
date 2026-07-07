//! Inline session-name editing in a pane's title bar (and the program
//! popup's title bar), as a faster alternative to the bottom-minibuffer
//! `Rename` prompt (`r` / the session-title menu). Editing model mirrors
//! `session_picker`'s search-line cursor: `cursor` is a char index into
//! `buffer`, with the same Emacs motions.

use super::*;

impl App {
    /// Start (or re-seed) an inline rename of `session_id`, pre-filling the
    /// edit buffer with its current title and placing the cursor at the end
    /// — mirroring `OpenRename`'s minibuffer seed. A no-op if this session is
    /// already being renamed; switching to a different session's name
    /// silently discards any unsaved edit of the previous one, same as
    /// clicking away from an unsaved text field.
    pub(super) fn start_session_title_rename(&mut self, session_id: String) {
        if self
            .session_title_rename
            .as_ref()
            .is_some_and(|r| r.session_id == session_id)
        {
            return;
        }
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        let current = s.title.clone().unwrap_or_default();
        let cursor = current.chars().count();
        self.session_title_rename = Some(SessionTitleRename {
            session_id,
            buffer: current,
            cursor,
        });
    }

    fn session_title_rename_push_char(&mut self, c: char) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            let pos = byte_pos(&rename.buffer, rename.cursor);
            rename.buffer.insert(pos, c);
            rename.cursor += 1;
        }
    }

    fn session_title_rename_backspace(&mut self) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            if rename.cursor > 0 {
                let prev = rename.cursor - 1;
                let pos = byte_pos(&rename.buffer, prev);
                rename.buffer.remove(pos);
                rename.cursor = prev;
            }
        }
    }

    fn session_title_rename_delete_forward(&mut self) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            if rename.cursor < rename.buffer.chars().count() {
                let pos = byte_pos(&rename.buffer, rename.cursor);
                rename.buffer.remove(pos);
            }
        }
    }

    fn session_title_rename_move_cursor(&mut self, delta: isize) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            let len = rename.buffer.chars().count();
            rename.cursor = if delta < 0 {
                rename.cursor.saturating_sub(delta.unsigned_abs())
            } else {
                rename.cursor.saturating_add(delta as usize).min(len)
            };
        }
    }

    fn session_title_rename_cursor_to_edge(&mut self, end: bool) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            rename.cursor = if end { rename.buffer.chars().count() } else { 0 };
        }
    }

    fn session_title_rename_kill_to_end(&mut self) {
        if let Some(rename) = self.session_title_rename.as_mut() {
            let pos = byte_pos(&rename.buffer, rename.cursor);
            rename.buffer.truncate(pos);
        }
    }

    /// `Esc`: drop the edit, no RPC, no local mutation.
    fn cancel_session_title_rename(&mut self) {
        self.session_title_rename = None;
    }

    /// `Enter`: commit via `set_title`, matching `MinibufferIntent::Rename`
    /// (empty buffer clears the title).
    async fn commit_session_title_rename(&mut self) {
        let Some(rename) = self.session_title_rename.take() else {
            return;
        };
        let trimmed = rename.buffer.trim().to_string();
        let new_title = if trimmed.is_empty() { None } else { Some(trimmed) };
        match self.client.set_title(&rename.session_id, new_title.clone()).await {
            Ok(()) => {
                if let Some(i) = self.sessions.iter().position(|s| s.id == rename.session_id) {
                    self.sessions[i].title = new_title.clone();
                }
                self.set_status(match &new_title {
                    Some(t) => format!("renamed → {t}"),
                    None => "title cleared".into(),
                });
            }
            Err(e) => self.set_status(format!("rename failed: {e}")),
        }
    }

    /// Route a key while an inline rename owns input. Same editing
    /// primitives as `handle_session_picker_key`'s search line: `Enter`
    /// commits, `Esc` cancels, Emacs motions move the cursor.
    pub(super) async fn handle_session_title_rename_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => self.cancel_session_title_rename(),
            KeyCode::Char('g') if ctrl => self.cancel_session_title_rename(),
            KeyCode::Enter => self.commit_session_title_rename().await,
            KeyCode::Left => self.session_title_rename_move_cursor(-1),
            KeyCode::Right => self.session_title_rename_move_cursor(1),
            KeyCode::Home => self.session_title_rename_cursor_to_edge(false),
            KeyCode::End => self.session_title_rename_cursor_to_edge(true),
            KeyCode::Char('f') if ctrl => self.session_title_rename_move_cursor(1),
            KeyCode::Char('b') if ctrl => self.session_title_rename_move_cursor(-1),
            KeyCode::Char('a') if ctrl => self.session_title_rename_cursor_to_edge(false),
            KeyCode::Char('e') if ctrl => self.session_title_rename_cursor_to_edge(true),
            KeyCode::Char('k') if ctrl => self.session_title_rename_kill_to_end(),
            KeyCode::Backspace => self.session_title_rename_backspace(),
            KeyCode::Delete => self.session_title_rename_delete_forward(),
            KeyCode::Char(c) if !ctrl && !alt && !super_mod => {
                self.session_title_rename_push_char(c)
            }
            _ => {}
        }
    }
}
