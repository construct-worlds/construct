use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::*;

impl App {
    /// Flip keyboard focus between the *active* rolled-down Playbook and the
    /// terminal it exposes. The focus flag and its slide animation live on the
    /// popup itself (see [`PlaybookPopup::set_terminal_focus`]), so stashed
    /// popups in unfocused split panes keep their own slide state — focusing
    /// another window never resets a different Playbook's slide.
    pub(crate) fn set_playbook_terminal_focus(&mut self, focused: bool) {
        if let Some(popup) = self.playbook_popup.as_mut() {
            popup.set_terminal_focus(focused);
        }
    }

    pub(crate) fn toggle_playbook_terminal_focus(&mut self) {
        let Some(popup) = self.playbook_popup.as_ref() else {
            self.set_status("playbook focus: no playbook open".to_string());
            return;
        };
        let terminal_focus = !popup.terminal_focus;
        self.focus = PaneFocus::View;
        self.set_playbook_terminal_focus(terminal_focus);
        if terminal_focus {
            self.set_status("focus: session terminal".to_string());
        } else {
            self.set_status("focus: playbook".to_string());
        }
    }

    /// Path of the TUI's persisted playbook view state (spec 0099: expansion
    /// state is client-local; this file makes it survive TUI restarts).
    #[cfg(not(test))]
    fn playbook_expanded_store_path() -> std::path::PathBuf {
        construct_protocol::paths::Paths::discover()
            .state_dir
            .join("tui-playbook-expanded.json")
    }

    /// Seed a freshly opened popup's expansion map from the persisted
    /// per-session store, loading the store file on first use.
    pub(super) fn seed_playbook_expanded(&mut self, popup: &mut PlaybookPopup) {
        if !self.playbook_expanded_store_loaded {
            self.playbook_expanded_store_loaded = true;
            // Tests exercise the in-memory store only — never the real
            // user state file.
            #[cfg(not(test))]
            {
                self.playbook_expanded_store =
                    std::fs::read_to_string(Self::playbook_expanded_store_path())
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default();
            }
        }
        let Some(saved) = self.playbook_expanded_store.get(&popup.playbook.session_id) else {
            return;
        };
        popup.expanded_attachments = saved
            .iter()
            .filter_map(|(key, (path, rows))| {
                let (hash, idx) = key.split_once(':')?;
                Some((
                    (hash.parse().ok()?, idx.parse().ok()?),
                    (path.clone(), *rows),
                ))
            })
            .collect();
    }

    /// Re-attach expansion state after buffer edits (spec 0099): an entry
    /// whose line changed migrates to the first unclaimed instance with the
    /// same target path — so typing after an inlined image keeps it
    /// expanded. Entries with no surviving same-path instance are kept
    /// under their old key: breaking a link mid-edit and re-completing it
    /// then restores the expansion. Cheap no-op while the buffer is
    /// unchanged; called once per frame for the active popup.
    pub(crate) fn reconcile_playbook_expanded(&mut self) {
        let Some(popup) = self.playbook_popup.as_mut() else {
            return;
        };
        let buffer_hash = crate::ui::playbook_line_key(&popup.buffer);
        if popup.expanded_reconcile_hash == buffer_hash {
            return;
        }
        popup.expanded_reconcile_hash = buffer_hash;
        if popup.expanded_attachments.is_empty() {
            return;
        }
        let current = crate::ui::playbook_attachment_instances(&popup.buffer);
        let current_keys: std::collections::HashSet<_> =
            current.iter().map(|(key, _)| *key).collect();
        let (kept, orphans): (Vec<_>, Vec<_>) = popup
            .expanded_attachments
            .drain()
            .partition(|(key, _)| current_keys.contains(key));
        let mut claimed: std::collections::HashSet<_> = kept.iter().map(|(key, _)| *key).collect();
        popup.expanded_attachments.extend(kept);
        let mut migrated = false;
        for (old_key, (path, rows)) in orphans {
            match current
                .iter()
                .find(|(key, p)| p == &path && !claimed.contains(key))
            {
                Some((new_key, _)) => {
                    claimed.insert(*new_key);
                    popup.expanded_attachments.insert(*new_key, (path, rows));
                    migrated = true;
                }
                None => {
                    popup.expanded_attachments.insert(old_key, (path, rows));
                }
            }
        }
        // Keep the persisted store in step with migrated keys so a TUI
        // restart right after an edit seeds current keys, not stale ones
        // (stale keys would still heal through this same migration, but
        // only while the entry's path survives the session).
        if migrated {
            self.persist_playbook_expanded();
        }
    }

    /// Write-through the active popup's expansion map to the per-session
    /// store and the state file. Best-effort: a failed write only costs
    /// persistence, never the in-memory state.
    pub(super) fn persist_playbook_expanded(&mut self) {
        let Some(popup) = self.playbook_popup.as_ref() else {
            return;
        };
        let session_id = popup.playbook.session_id.clone();
        let serialized: HashMap<String, (String, u16)> = popup
            .expanded_attachments
            .iter()
            .map(|((hash, idx), entry)| (format!("{hash}:{idx}"), entry.clone()))
            .collect();
        if serialized.is_empty() {
            self.playbook_expanded_store.remove(&session_id);
        } else {
            self.playbook_expanded_store.insert(session_id, serialized);
        }
        // Tests exercise the in-memory store only — never the real user
        // state file.
        #[cfg(not(test))]
        {
            let path = Self::playbook_expanded_store_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.playbook_expanded_store) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    pub(super) async fn open_playbook_popup(&mut self) {
        let Some(session_id) = self.selected_id() else {
            self.set_status("playbook: no session selected".to_string());
            return;
        };
        // Replacing a popup that holds a size-owning pinned clip must hand
        // the session its standard size back first (spec 0090).
        self.set_playbook_pinned_clip(None).await;

        match self.client.playbook_get(&session_id).await {
            Ok(result) => {
                let version = result.playbook.version;
                let now = Instant::now();
                self.adopt_daemon_playbook_run(&result.playbook.session_id, result.active_run);
                for cursor in result.collaborators {
                    self.playbook_collaborators
                        .insert(cursor.client_id.clone(), cursor);
                }
                self.playbook_popups.remove(&result.playbook.session_id);
                let mut popup = playbook_popup_from_document(result.playbook, result.blocks, now);
                // Restore the caret + scroll the user left when this playbook was
                // last hidden, so hide→show is position-preserving rather than
                // jumping back to the top.
                self.restore_playbook_view_state(&mut popup);
                // Restore persisted image-expansion state (spec 0099).
                self.seed_playbook_expanded(&mut popup);
                self.playbook_popup = Some(popup);
                // Opening a playbook focuses the view pane so its keystrokes are
                // captured immediately for editing. `C-x o` then hands focus
                // back to the list for navigation while the playbook stays
                // visible (see the focus gate in `on_key`).
                self.focus = PaneFocus::View;
                self.set_playbook_terminal_focus(false);
                self.set_status(format!("playbook opened at version {version}"));
                // Live reload: the daemon re-reads the templates dir on every
                // call, but the client caches the list. Kick off a non-blocking
                // refresh on open so edits / new template files surface in the
                // empty-state placeholder without a daemon restart.
                self.refresh_playbook_templates();
                self.refresh_playbook_verbs();
            }
            Err(e) => {
                self.set_status(format!("playbook get failed: {e}"));
            }
        }
    }

    /// Fetch `session_id`'s playbook document in the background for a widget
    /// `:::clip playbook` projection (spec 0074). Non-blocking: the widget
    /// renders a "loading playbook…" line this frame and the projection paints
    /// once the fetch lands on `playbook_projection_tx`; afterwards the cache
    /// stays fresh through `playbook/state` notifications, which the daemon
    /// broadcasts for every playbook change regardless of open playbook views.
    /// At most one fetch per session is in flight; a failed fetch stays
    /// pending (no per-frame retry) until a notification fills the cache.
    pub(crate) fn request_playbook_projection(&mut self, session_id: String) {
        if self.playbook_markdown_cache.contains_key(&session_id) {
            return;
        }
        if !self.playbook_projection_pending.insert(session_id.clone()) {
            return;
        }
        // Renders can run outside a tokio runtime (pure render tests); no
        // runtime simply means no fetch — the projection keeps its loading
        // line, exactly like a fetch that has not landed yet.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let client = self.client.clone();
        let tx = self.playbook_projection_tx.clone();
        handle.spawn(async move {
            if let Ok(result) = client.playbook_get(&session_id).await {
                let _ = tx.send((session_id, result.playbook.markdown));
            }
        });
    }

    /// Fetch playbook templates from the daemon in the background and deliver them
    /// to the event loop via `playbook_templates_tx`. Non-blocking: the playbook
    /// opens immediately against the cached list and swaps to the fresh one when
    /// it lands, so there's no flicker and a slow daemon never stalls the open.
    fn refresh_playbook_templates(&self) {
        let client = self.client.clone();
        let tx = self.playbook_templates_tx.clone();
        tokio::spawn(async move {
            if let Ok(result) = client.playbook_templates().await {
                let _ = tx.send(result.templates);
            }
        });
    }

    /// `refresh_playbook_templates`'s counterpart for playbook verbs (spec
    /// 0087): the same "fetch fresh, deliver via channel" shape so a newly
    /// dropped `verbs/*.md` file appears in the selection menu on the next
    /// playbook open without a daemon restart.
    fn refresh_playbook_verbs(&self) {
        let client = self.client.clone();
        let tx = self.playbook_verbs_tx.clone();
        tokio::spawn(async move {
            if let Ok(result) = client.playbook_verbs().await {
                let _ = tx.send(result.verbs);
            }
        });
    }

    pub(super) async fn toggle_playbook_popup(&mut self) {
        if self.playbook_popup.is_some() {
            self.close_playbook_popup().await;
        } else {
            self.open_playbook_popup().await;
        }
    }

    pub(super) async fn restore_open_playbook_popups(&mut self, session_ids: &[String]) {
        let mut seen = HashSet::new();
        let live_sessions: HashSet<String> = self
            .sessions
            .iter()
            .filter(|s| is_user_list_session(s))
            .map(|s| s.id.clone())
            .collect();
        let selected_id = self.selection.session_id().map(str::to_string);
        let now = Instant::now();
        for session_id in session_ids {
            if !seen.insert(session_id.clone()) || !live_sessions.contains(session_id) {
                continue;
            }
            match self.client.playbook_get(session_id).await {
                Ok(result) => {
                    self.adopt_daemon_playbook_run(&result.playbook.session_id, result.active_run);
                    for cursor in result.collaborators {
                        self.playbook_collaborators
                            .insert(cursor.client_id.clone(), cursor);
                    }
                    let mut popup = playbook_popup_from_document(result.playbook, result.blocks, now);
                    // Restored popups get their persisted image-expansion
                    // state back too (spec 0099) — without this, a TUI
                    // restart came back collapsed and the next interaction
                    // persisted the empty map over the stored one.
                    self.seed_playbook_expanded(&mut popup);
                    if selected_id.as_deref() == Some(session_id.as_str()) {
                        self.playbook_popup = Some(popup);
                    } else {
                        self.playbook_popups.insert(session_id.clone(), popup);
                    }
                }
                Err(e) => {
                    self.status = Some((
                        format!("playbook restore failed for {}: {e}", short_id(session_id)),
                        Instant::now(),
                    ));
                }
            }
        }
    }

    pub(crate) fn open_playbook_session_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        if let Some(popup) = self.playbook_popup.as_ref() {
            if !popup.closing && seen.insert(popup.playbook.session_id.clone()) {
                ids.push(popup.playbook.session_id.clone());
            }
        }
        for popup in self.playbook_popups.values() {
            if !popup.closing && seen.insert(popup.playbook.session_id.clone()) {
                ids.push(popup.playbook.session_id.clone());
            }
        }
        ids.sort();
        ids
    }

    /// The session id of the playbook smart-clip occupying the cell at
    /// `(col, row)`, if any. Reads the hitboxes captured during the last playbook
    /// render; used for clip click-to-focus and hover-preview.
    pub(super) fn playbook_clip_session_at(&self, col: u16, row: u16) -> Option<String> {
        self.layout
            .playbook_clip_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .map(|hit| hit.session_id.clone())
    }

    pub(crate) fn sync_playbook_popup_with_selection(&mut self) {
        let selected_id = self.selection.session_id().map(str::to_string);
        let active_id = self
            .playbook_popup
            .as_ref()
            .map(|popup| popup.playbook.session_id.clone());
        if active_id.as_deref() == selected_id.as_deref() {
            return;
        }
        self.stash_active_playbook_popup();
        if let Some(selected_id) = selected_id {
            if let Some(mut popup) = self.playbook_popups.remove(&selected_id) {
                popup.closing = false;
                self.playbook_popup = Some(popup);
            }
        }
    }

    fn stash_active_playbook_popup(&mut self) {
        if let Some(popup) = self.playbook_popup.take() {
            if !popup.closing {
                self.playbook_popups
                    .insert(popup.playbook.session_id.clone(), popup);
            }
        }
    }

    /// Flush a popup's edits to the daemon as a whole-document write.
    ///
    /// If the document advanced underneath us (an agent edited it while the
    /// human was typing), the daemon rejects the stale `base_version`; we then
    /// re-read the latest content and 3-way merge our edits onto it, using the
    /// last-saved content as the common ancestor. Disjoint edits merge silently;
    /// only genuinely overlapping edits produce conflict markers. Either way the
    /// write lands, so hiding the playbook never blocks and no edit is lost.
    async fn save_playbook_popup_document(
        &self,
        popup: &PlaybookPopup,
    ) -> Result<Option<PlaybookSaveOutcome>> {
        let mut ours = playbook_normalize_smart_clip_instance_ids(&popup.buffer);
        if ours == popup.saved_markdown {
            return Ok(None);
        }
        // The content our edits are based on — the common ancestor for a merge.
        let mut ancestor = popup.saved_markdown.clone();
        let mut base = popup.playbook.version;
        let mut merged = false;
        let mut conflicted = false;
        // Retry to absorb further updates that land between our re-read and
        // our write.
        for _ in 0..5 {
            let params = construct_protocol::PlaybookUpdateParams {
                session_id: popup.playbook.session_id.clone(),
                markdown: ours.clone(),
                base_version: Some(base),
                actor: construct_protocol::PlaybookUpdateActor::Human,
                template_id: popup.playbook.template_id.clone(),
                note: None,
                // Human co-edit save: no shimmer declaration — the daemon
                // narrows the active run by content change only (spec 0053).
                shimmer: None,
                shimmer_tooltips: None,
            };
            match self.client.playbook_update(params).await {
                Ok(result) => {
                    return Ok(Some(PlaybookSaveOutcome {
                        playbook: result.playbook,
                        blocks: result.blocks,
                        merged,
                        conflicted,
                    }));
                }
                Err(e) if e.to_string().contains("playbook conflict") => {
                    let latest = self
                        .client
                        .playbook_get(&popup.playbook.session_id)
                        .await?
                        .playbook;
                    let theirs = latest.markdown;
                    merged = true;
                    match diffy::merge(&ancestor, &ours, &theirs) {
                        Ok(clean) => ours = clean,
                        Err(with_markers) => {
                            ours = with_markers;
                            conflicted = true;
                        }
                    }
                    // The content we just merged onto becomes the ancestor for
                    // any further round.
                    ancestor = theirs;
                    base = latest.version;
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow::anyhow!(
            "playbook merge: gave up after repeated concurrent updates"
        ))
    }

    pub(super) async fn save_open_playbook_popups(&mut self) {
        let active = self.playbook_popup.clone();
        if let Some(popup) = active.as_ref() {
            match self.save_playbook_popup_document(popup).await {
                Ok(Some(outcome)) => {
                    if let Some(active) = self.playbook_popup.as_mut() {
                        active.buffer = outcome.playbook.markdown.clone();
                        active.saved_markdown = outcome.playbook.markdown.clone();
                        active.blocks = outcome.blocks.clone();
                        active.playbook = outcome.playbook;
                        active.cursor = active.cursor.min(active.buffer.chars().count());
                        active.preferred_col = None;
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = Some((format!("playbook save failed: {e}"), Instant::now())),
            }
        }

        let cached: Vec<(String, PlaybookPopup)> = self
            .playbook_popups
            .iter()
            .map(|(id, popup)| (id.clone(), popup.clone()))
            .collect();
        for (session_id, popup) in cached {
            match self.save_playbook_popup_document(&popup).await {
                Ok(Some(outcome)) => {
                    if let Some(cached) = self.playbook_popups.get_mut(&session_id) {
                        cached.buffer = outcome.playbook.markdown.clone();
                        cached.saved_markdown = outcome.playbook.markdown.clone();
                        cached.blocks = outcome.blocks.clone();
                        cached.playbook = outcome.playbook;
                        cached.cursor = cached.cursor.min(cached.buffer.chars().count());
                        cached.preferred_col = None;
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = Some((format!("playbook save failed: {e}"), Instant::now())),
            }
        }
    }

    pub(super) async fn save_playbook_popup(&mut self) -> bool {
        let Some(popup) = self.playbook_popup.as_ref() else {
            return true;
        };
        match self.save_playbook_popup_document(popup).await {
            Ok(Some(outcome)) => {
                let version = outcome.playbook.version;
                let (merged, conflicted) = (outcome.merged, outcome.conflicted);
                if let Some(popup) = self.playbook_popup.as_mut() {
                    popup.buffer = outcome.playbook.markdown.clone();
                    popup.saved_markdown = outcome.playbook.markdown.clone();
                    popup.blocks = outcome.blocks.clone();
                    popup.playbook = outcome.playbook;
                    popup.cursor = popup.cursor.min(popup.buffer.chars().count());
                    popup.preferred_col = None;
                }
                if conflicted {
                    self.set_status(format!(
                        "playbook merged with conflicts to resolve (version {version})"
                    ));
                } else if merged {
                    self.set_status(format!(
                        "playbook merged with agent edits (version {version})"
                    ));
                } else {
                    self.set_status(format!("playbook saved version {version}"));
                }
                true
            }
            Ok(None) => true,
            Err(e) => {
                self.set_status(format!("playbook save failed: {e}"));
                false
            }
        }
    }

    pub(super) async fn execute_playbook_popup(
        &mut self,
        selection: Option<String>,
        selected_block_ids: Option<HashSet<String>>,
        comment: Option<String>,
    ) -> bool {
        self.execute_playbook_popup_target(selection, selected_block_ids, comment, false)
            .await
    }

    pub(super) async fn execute_playbook_popup_target(
        &mut self,
        selection: Option<String>,
        selected_block_ids: Option<HashSet<String>>,
        comment: Option<String>,
        fork: bool,
    ) -> bool {
        let Some(session_id) = self
            .playbook_popup
            .as_ref()
            .map(|popup| popup.playbook.session_id.clone())
        else {
            self.set_status("playbook run failed: no active playbook".to_string());
            return false;
        };

        // Snapshot the last daemon-synced content *before* saving, so a re-Run
        // can tell which blocks the user changed (vs. ones the agent settled,
        // which are already folded into `saved_markdown`). See spec 0042.
        let prev_saved = self
            .playbook_popup
            .as_ref()
            .map(|popup| popup.saved_markdown.clone())
            .unwrap_or_default();

        let selection =
            selection.map(|selection| playbook_normalize_smart_clip_instance_ids(&selection));
        let comment = comment
            .map(|comment| comment.trim().to_string())
            .filter(|comment| !comment.is_empty());
        let is_selection = selection.is_some();
        let pre_save_run_body = match selection.as_deref() {
            Some(sel) => sel.to_string(),
            None => self
                .playbook_popup
                .as_ref()
                .map(|popup| playbook_normalize_smart_clip_instance_ids(&popup.buffer))
                .unwrap_or_default(),
        };

        // Run overlap/idempotency guard (spec 0042 consequence). A double
        // `C-x C-r` / double-click a Run button is delivered by the TUI's
        // serialized event loop as two separate calls into this function —
        // usually milliseconds *after* the first has already completed its
        // save/execute round trip, which is why an in-flight flag alone is
        // not enough: we also debounce an identical repeat for a short
        // window after a successful dispatch. Keying on session + scope +
        // executed body means this only ever coalesces a truly identical
        // repeat gesture: a selection run while a full run is in flight, a
        // different selection, and a full re-Run whose body changed all
        // still dispatch (see spec 0042's re-Run and selection-adds-to-
        // in-flight semantics).
        let dispatch_fingerprint = match comment.as_deref() {
            Some(comment) => format!("{pre_save_run_body}\n\nrun comment:\n{comment}"),
            None => pre_save_run_body.clone(),
        };
        let dispatch_key: PlaybookRunDispatchKey = (
            session_id.clone(),
            is_selection,
            hash_playbook_run_body(&dispatch_fingerprint),
        );
        if let Some(state) = self.playbook_run_dispatch.get(&dispatch_key) {
            let suppress = match state {
                PlaybookRunDispatchState::InFlight => true,
                PlaybookRunDispatchState::Dispatched(at) => {
                    at.elapsed() < Duration::from_millis(PLAYBOOK_RUN_DEDUP_WINDOW_MS)
                }
            };
            if suppress {
                self.set_status("run already dispatched".to_string());
                return false;
            }
        }
        self.playbook_run_dispatch
            .insert(dispatch_key.clone(), PlaybookRunDispatchState::InFlight);

        let selected_block_ids = selected_block_ids.or_else(|| {
            is_selection
                .then(|| {
                    self.playbook_popup
                        .as_ref()
                        .and_then(Self::selected_playbook_block_ids)
                })
                .flatten()
        });
        let pending = match selected_block_ids {
            Some(ids) if is_selection => self.playbook_run_pending_with_existing(&session_id, ids),
            _ => self.playbook_run_pending_for_body(
                &session_id,
                &pre_save_run_body,
                is_selection,
                &prev_saved,
            ),
        };
        self.start_playbook_run_with_pending(&session_id, pending);

        let dirty = self.playbook_popup.as_ref().is_some_and(|popup| {
            playbook_normalize_smart_clip_instance_ids(&popup.buffer) != popup.saved_markdown
        });
        if dirty && !self.save_playbook_popup().await {
            self.playbook_runs.remove(&session_id);
            self.playbook_run_dispatch.remove(&dispatch_key);
            return false;
        }

        let base_version = self
            .playbook_popup
            .as_ref()
            .map(|popup| popup.playbook.version);
        // Optimistic feedback (spec 0042): start the Run shimmer the instant
        // Run is pressed, before the execute round trip, so the affordance
        // covers the agent's latency rather than the request's. The executed
        // body is the selection if present, else the whole (now-saved) buffer.
        let run_body = match selection.as_deref() {
            Some(sel) => sel.to_string(),
            None => self
                .playbook_popup
                .as_ref()
                .map(|popup| popup.buffer.clone())
                .unwrap_or_default(),
        };
        let selected_block_ids = is_selection
            .then(|| {
                self.playbook_popup
                    .as_ref()
                    .and_then(Self::selected_playbook_block_ids)
            })
            .flatten();
        // The overlap-based real block ids the selection covers (spec 0053
        // consequence: partial-line/partial-block selection fix) — sent to
        // the daemon as `selection_block_ids` so it trusts this identity
        // instead of re-parsing the raw selected text and hash-matching,
        // which only works when the selection exactly spans whole blocks.
        let selection_block_ids: Option<Vec<String>> = selected_block_ids
            .as_ref()
            .filter(|ids| !ids.is_empty())
            .map(|ids| ids.iter().cloned().collect());
        let pending = match selected_block_ids {
            Some(ids) if is_selection => self.playbook_run_pending_with_existing(&session_id, ids),
            _ => {
                self.playbook_run_pending_for_body(&session_id, &run_body, is_selection, &prev_saved)
            }
        };
        self.start_playbook_run_with_pending(&session_id, pending.clone());
        let shimmer = if is_selection {
            Self::playbook_run_all_shimmer_for_body(&run_body)
        } else {
            Self::playbook_run_shimmer_for_body(&run_body, &pending)
        };
        let params = construct_protocol::PlaybookExecuteParams {
            session_id: session_id.clone(),
            selection,
            base_version,
            comment,
            // Echo the TUI's optimistic pending set so a mid-flight full re-Run
            // cannot be narrowed back to old pending refs before the planning
            // pass sees user-edited blocks.
            shimmer,
            selection_block_ids,
            fork,
        };
        match self.client.playbook_execute(params).await {
            Ok(result) => {
                self.adopt_daemon_playbook_run(&session_id, result.active_run);
                let scope = if is_selection { "selection" } else { "playbook" };
                self.set_status(format!(
                    "playbook run sent ({scope}, version {})",
                    result.playbook.version
                ));
                // Dispatch landed: start the debounce window rather than
                // clearing the guard outright, so an identical repeat
                // gesture arriving right behind it is still coalesced.
                self.playbook_run_dispatch.insert(
                    dispatch_key,
                    PlaybookRunDispatchState::Dispatched(Instant::now()),
                );
                true
            }
            Err(e) => {
                // The request never landed — retract the optimistic shimmer
                // and the guard, so the user can retry immediately.
                self.playbook_runs.remove(&session_id);
                self.playbook_run_dispatch.remove(&dispatch_key);
                self.set_status(format!("playbook run failed: {e}"));
                false
            }
        }
    }

    /// Begin (or refresh) the Run shimmer for `session_id` over `body` — the
    /// executed Markdown (full playbook or selection). Records the block
    /// signatures to shimmer; they settle as their content changes and the run
    /// clears once the first agent output is observed. See spec 0042.
    ///
    /// A re-Run while a run is still active preserves the narrowing the agent
    /// established: it re-shimmers only the blocks the user changed since the
    /// last daemon sync (`prev_saved`) plus blocks that were still pending —
    /// blocks the agent already settled stay calm. A selection run adds its
    /// own scope to any shimmer already in flight rather than replacing it —
    /// running one snippet must not dim blocks another in-flight run is still
    /// working on; a fresh run with nothing in flight shimmers just the
    /// selected region.
    #[cfg(test)]
    pub(super) fn start_playbook_run(
        &mut self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        prev_saved: &str,
    ) {
        let pending = self.playbook_run_pending_for_body(session_id, body, is_selection, prev_saved);
        self.start_playbook_run_with_pending(session_id, pending);
    }

    pub(super) fn playbook_run_pending_for_body(
        &self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        prev_saved: &str,
    ) -> HashSet<String> {
        let body_ids = playbook_run_pending_ids(body);
        if body_ids.is_empty() {
            return HashSet::new();
        }
        match self.playbook_runs.get(session_id) {
            Some(old) if !is_selection => {
                let prev_ids = playbook_run_pending_ids(prev_saved);
                let narrowed: HashSet<String> = body_ids
                    .difference(&prev_ids)
                    .chain(body_ids.intersection(&old.pending))
                    .cloned()
                    .collect();
                if narrowed.is_empty() {
                    body_ids
                } else {
                    narrowed
                }
            }
            Some(old) => old.pending.union(&body_ids).cloned().collect(),
            None => body_ids,
        }
    }

    pub(super) fn playbook_run_shimmer_for_body(
        body: &str,
        pending: &HashSet<String>,
    ) -> Option<Vec<bool>> {
        let shimmer: Vec<bool> = construct_protocol::playbook_block_spans(body)
            .into_iter()
            .map(|span| pending.contains(&span.id))
            .collect();
        (!shimmer.is_empty()).then_some(shimmer)
    }

    fn playbook_run_all_shimmer_for_body(body: &str) -> Option<Vec<bool>> {
        let len = construct_protocol::playbook_block_spans(body).len();
        (len > 0).then(|| vec![true; len])
    }

    /// Union `ids` with the pending set of any Run already in flight for
    /// `session_id`. Used by selection runs so that optimistically shimmering
    /// the freshly-run block never clears shimmer another in-flight run
    /// already declared elsewhere in the playbook (see spec 0042).
    pub(super) fn playbook_run_pending_with_existing(
        &self,
        session_id: &str,
        ids: HashSet<String>,
    ) -> HashSet<String> {
        match self.playbook_runs.get(session_id) {
            Some(old) => old.pending.union(&ids).cloned().collect(),
            None => ids,
        }
    }

    pub(super) fn start_playbook_run_with_pending(
        &mut self,
        session_id: &str,
        pending: HashSet<String>,
    ) {
        if pending.is_empty() {
            self.playbook_runs.remove(session_id);
            return;
        }
        let now = Instant::now();
        let pending_since = match self.playbook_runs.get(session_id) {
            Some(old) => old.merged_pending_since(&pending, now),
            None => pending.iter().cloned().map(|id| (id, now)).collect(),
        };
        self.playbook_runs.insert(
            session_id.to_string(),
            PlaybookRun {
                started_at: now,
                total_block_count: pending.len(),
                pending,
                pending_tooltips: HashMap::new(),
                pending_since,
                system_status: None,
                deadline: now + Duration::from_millis(PLAYBOOK_RUN_MAX_MS),
                agent_managed: false,
                first_output_seen: false,
                stage: construct_protocol::PlaybookRunStage::Pressed,
                daemon_confirmed: false,
                daemon_adopted_at: None,
                settled_block_count: 0,
            },
        );
    }

    /// Reap only unmanaged optimistic shimmers that outlive their safety
    /// deadline. Managed runs settle from declarations and terminal lifecycle
    /// signals instead of elapsed wall time (spec 0042).
    pub(super) fn expire_playbook_runs(&mut self, now: Instant) {
        self.playbook_runs
            .retain(|_, run| run.agent_managed || now < run.deadline);
        self.expire_playbook_settle_flourishes(now);
        self.expire_playbook_run_dispatch_guard(now);
    }

    /// Prune Run overlap/idempotency guard entries once their dedup window
    /// has lapsed, so the map does not grow unboundedly as the user edits and
    /// re-Runs a playbook over a long session. `InFlight` entries are left
    /// alone — they are always cleared by `execute_playbook_popup` itself when
    /// their dispatch resolves.
    fn expire_playbook_run_dispatch_guard(&mut self, now: Instant) {
        let window = Duration::from_millis(PLAYBOOK_RUN_DEDUP_WINDOW_MS);
        self.playbook_run_dispatch.retain(|_, state| match state {
            PlaybookRunDispatchState::InFlight => true,
            PlaybookRunDispatchState::Dispatched(at) => now.saturating_duration_since(*at) < window,
        });
    }

    pub(super) fn record_playbook_settle_flourishes(
        &mut self,
        session_id: &str,
        previous_pending: &HashSet<String>,
        next_pending: &HashSet<String>,
        now: Instant,
    ) {
        let settled: Vec<String> = previous_pending.difference(next_pending).cloned().collect();
        if settled.is_empty() {
            return;
        }
        let flourishes = self
            .playbook_settle_flourishes
            .entry(session_id.to_string())
            .or_default();
        for block_ref in settled {
            flourishes.insert(block_ref, now);
        }
    }

    fn expire_playbook_settle_flourishes(&mut self, now: Instant) {
        let ttl = Duration::from_millis(crate::app::PLAYBOOK_SETTLE_FLASH_MS);
        self.playbook_settle_flourishes.retain(|_, flourishes| {
            flourishes.retain(|_, started_at| now.saturating_duration_since(*started_at) < ttl);
            !flourishes.is_empty()
        });
    }

    pub(super) async fn close_playbook_popup(&mut self) {
        if !self.save_playbook_popup().await {
            return;
        }
        // A size-owning pinned clip releases its session's terminal size
        // when the Playbook goes away with it (spec 0090).
        self.set_playbook_pinned_clip(None).await;
        if let Some(session_id) = self
            .playbook_popup
            .as_ref()
            .map(|popup| popup.playbook.session_id.clone())
        {
            let _ = self
                .client
                .playbook_cursor(construct_protocol::PlaybookCursorParams {
                    session_id,
                    cursor: 0,
                    selection_anchor: None,
                    selection_head: None,
                    version: None,
                    label: Some("TUI".to_string()),
                    clear: true,
                })
                .await;
        }
        // Capture caret + scroll before the popup fades so reopening this
        // session's playbook restores them (the popup itself is dropped once the
        // close animation lapses — see `render_playbook_popup`).
        self.remember_playbook_view_state();
        self.set_playbook_terminal_focus(false);
        if let Some(popup) = self.playbook_popup.as_mut() {
            self.playbook_popups.remove(&popup.playbook.session_id);
            let now = Instant::now();
            popup.closing = true;
            popup.hide_after = now + Duration::from_millis(PLAYBOOK_REVEAL_MS);
        }
    }

    /// Snapshot the active playbook's caret + scroll into `playbook_view_memory`
    /// so a later reopen of the same session's playbook restores them. Every
    /// path that hides the playbook calls this before the popup is dropped.
    pub(super) fn remember_playbook_view_state(&mut self) {
        if let Some(popup) = self.playbook_popup.as_ref() {
            self.playbook_view_memory.insert(
                popup.playbook.session_id.clone(),
                PlaybookViewMemory {
                    cursor: popup.cursor,
                    preferred_col: popup.preferred_col,
                    scroll_offset: popup.scroll_offset,
                    cover_percent: popup.cover_percent,
                },
            );
        }
    }

    /// Reapply a remembered caret + scroll (captured when the playbook was last
    /// hidden) onto a freshly-loaded popup, so a hide→show cycle lands on the
    /// same position. Consumes the entry. The cursor is clamped to the buffer
    /// (the document may have changed on the daemon); an out-of-range scroll is
    /// clamped by the renderer.
    pub(super) fn restore_playbook_view_state(&mut self, popup: &mut PlaybookPopup) {
        if let Some(memory) = self.playbook_view_memory.remove(&popup.playbook.session_id) {
            popup.cursor = memory.cursor.min(popup.buffer.chars().count());
            popup.preferred_col = memory.preferred_col;
            popup.scroll_offset = memory.scroll_offset;
            popup.cover_percent = memory.cover_percent.clamp(
                crate::app::PLAYBOOK_COVER_PERCENT_MIN,
                crate::app::PLAYBOOK_COVER_PERCENT_MAX,
            );
        }
    }
}

/// Hash of a playbook Run's executed body, used only to key the Run
/// overlap/idempotency guard (`App::playbook_run_dispatch`, spec 0042
/// consequence). A collision would at worst suppress a legitimately
/// different Run for one dedup window — an acceptable, vanishingly unlikely
/// trade against keying on the whole body text.
fn hash_playbook_run_body(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}
