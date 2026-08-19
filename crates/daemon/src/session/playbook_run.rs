use super::*;

fn block_ref_or_id(block: &construct_protocol::PlaybookBlockView) -> String {
    if !block.block_ref.is_empty() {
        block.block_ref.clone()
    } else {
        block.id.clone()
    }
}

fn playbook_block_ids(
    blocks: &[construct_protocol::PlaybookBlockView],
) -> std::collections::HashSet<String> {
    blocks
        .iter()
        .flat_map(|block| [block_ref_or_id(block), block.content_id.clone()])
        .filter(|id| !id.is_empty())
        .collect()
}

/// Resolve a selection Run's blocks from the client-supplied
/// `selection_block_ids` (spec 0053 consequence, partial-selection fix): each
/// id is matched against a saved block's stable ref/id or its content id.
/// Ids that no longer exist (the document changed underneath the caller) are
/// silently dropped rather than fabricated — the caller falls back to an
/// empty/short result the same way an unresolvable span would. Document order
/// is preserved by iterating `saved_blocks`, matching the ordering guarantee
/// callers rely on for an explicit initial pending declaration.
fn playbook_run_blocks_from_ids(
    ids: &[String],
    saved_blocks: &[construct_protocol::PlaybookBlockView],
) -> Vec<construct_protocol::PlaybookBlockView> {
    let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    saved_blocks
        .iter()
        .filter(|block| {
            wanted.contains(block_ref_or_id(block).as_str())
                || wanted.contains(block.content_id.as_str())
        })
        .cloned()
        .collect()
}

/// Legacy fallback for when no `selection_block_ids` were supplied: re-parse
/// the raw selected text as its own standalone Markdown document and match
/// each resulting span's content-hash against the real document's saved
/// blocks. This only works precisely when the selection exactly spans one or
/// more whole blocks. When it's a strict substring of a single block (e.g. a
/// partial-line selection), the substring's own content-hash never equals the
/// real block's (computed over the full block text), so the exact-hash lookup
/// misses; as a second attempt, fall back to text containment — if exactly
/// one saved block's text contains the span's (trimmed) text, trust that
/// match. Containment is ambiguous (zero or multiple candidates) when the
/// document repeats the same text in more than one block, in which case this
/// still fabricates a phantom block scoped to just the selected substring —
/// a known limitation of running without `selection_block_ids`.
fn playbook_run_blocks_from_spans(
    body: &str,
    saved_blocks: &[construct_protocol::PlaybookBlockView],
) -> Vec<construct_protocol::PlaybookBlockView> {
    let mut by_content: std::collections::HashMap<String, Vec<&construct_protocol::PlaybookBlockView>> =
        std::collections::HashMap::new();
    for block in saved_blocks {
        if !block.content_id.is_empty() {
            by_content
                .entry(block.content_id.clone())
                .or_default()
                .push(block);
        }
    }

    construct_protocol::playbook_block_spans(body)
        .into_iter()
        .map(|span| {
            if let Some(matches) = by_content.get(&span.id) {
                if let [block] = matches.as_slice() {
                    return (*block).clone();
                }
            }
            let trimmed = span.text.trim();
            if !trimmed.is_empty() {
                let containing: Vec<&construct_protocol::PlaybookBlockView> = saved_blocks
                    .iter()
                    .filter(|block| block.text.contains(trimmed))
                    .collect();
                if let [block] = containing.as_slice() {
                    return (*block).clone();
                }
            }
            construct_protocol::PlaybookBlockView {
                id: span.id.clone(),
                block_id: String::new(),
                content_epoch: 0,
                block_ref: String::new(),
                content_id: span.id,
                start_line: span.start_line,
                end_line: span.end_line,
                text: span.text,
                shimmer: false,
                tooltip: None,
            }
        })
        .collect()
}

fn playbook_run_system_status(run: &PlaybookRunProgress) -> &'static str {
    if run.queued_behind_current_turn && !run.seen_running {
        construct_protocol::PLAYBOOK_SHIMMER_STATUS_QUEUED
    } else if run.first_output_seen {
        construct_protocol::PLAYBOOK_SHIMMER_STATUS_AGENT_WORKING
    } else {
        construct_protocol::PLAYBOOK_SHIMMER_STATUS_DELIVERED
    }
}

fn project_playbook_run_status(mut run: PlaybookRunProgress) -> PlaybookRunProgress {
    run.system_status = Some(playbook_run_system_status(&run).to_string());
    run
}

impl SessionManager {
    pub(super) fn playbook_run_snapshot(&self, session_id: &str) -> Option<PlaybookRunProgress> {
        let now_ms = Utc::now().timestamp_millis();
        let mut runs = self.playbook_runs.lock().ok()?;
        let expired = runs
            .get(session_id)
            .is_some_and(|run| !run.agent_managed && run.expires_at_ms <= now_ms);
        if expired {
            runs.remove(session_id);
            return None;
        }
        // An empty pending set means nothing shimmers right now, so report no
        // active run — but KEEP the record so a follow-up declaration can revive
        // it within the same turn (spec 0053): a move/annotate that changes a
        // still-pending block transiently empties the set before the new id is
        // declared, and that must not destroy the run. The record is reaped when
        // the owning session goes idle/terminal. Managed runs have no inactivity
        // deadline; unmanaged optimistic runs still retain the safety backstop.
        match runs.get(session_id) {
            Some(run)
                if !run.pending_block_refs.is_empty() || !run.pending_block_ids.is_empty() =>
            {
                let mut out = run.clone();
                if !out.pending_block_refs.is_empty() {
                    if let Ok((playbook, blocks)) = self.storage.read_playbook_with_blocks(session_id)
                    {
                        let refs: std::collections::HashSet<String> =
                            out.pending_block_refs.iter().cloned().collect();
                        out.pending_block_ids = blocks
                            .iter()
                            .filter(|block| refs.contains(&block_ref_or_id(block)))
                            .map(|block| block.content_id.clone())
                            .collect();
                        if out.pending_block_ids.is_empty() && !playbook.markdown.trim().is_empty() {
                            out.pending_block_ids = out.pending_block_refs.clone();
                        }
                    } else {
                        out.pending_block_ids = out.pending_block_refs.clone();
                    }
                }
                Some(project_playbook_run_status(out.with_refreshed_stage()))
            }
            _ => None,
        }
    }

    /// Build the per-block projection (spec 0053): each block of `markdown` with
    /// its stable ref, text, and current shimmer state from the active run.
    pub(super) fn playbook_blocks_projection(
        &self,
        session_id: &str,
        markdown: &str,
    ) -> Vec<construct_protocol::PlaybookBlockView> {
        let run = self
            .playbook_runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(session_id).cloned())
            .filter(|run| run.agent_managed || run.expires_at_ms > Utc::now().timestamp_millis());
        let pending_refs: std::collections::HashSet<String> = run
            .as_ref()
            .map(|run| run.pending_block_refs.iter().cloned().collect())
            .unwrap_or_default();
        let pending_ids: std::collections::HashSet<String> = run
            .as_ref()
            .map(|run| run.pending_block_ids.iter().cloned().collect())
            .unwrap_or_default();
        let has_stable_refs = !pending_refs.is_empty();
        let tooltips = run
            .map(|run| run.pending_block_tooltips)
            .unwrap_or_default();
        let blocks = self
            .storage
            .read_playbook_with_blocks(session_id)
            .map(|(_, blocks)| blocks)
            .unwrap_or_else(|_| {
                construct_protocol::playbook_block_spans(markdown)
                    .into_iter()
                    .map(|span| construct_protocol::PlaybookBlockView {
                        id: span.id.clone(),
                        block_id: String::new(),
                        content_epoch: 0,
                        block_ref: String::new(),
                        content_id: span.id,
                        start_line: span.start_line,
                        end_line: span.end_line,
                        text: span.text,
                        shimmer: false,
                        tooltip: None,
                    })
                    .collect()
            });
        blocks
            .into_iter()
            .map(|mut block| {
                let key = block_ref_or_id(&block);
                block.shimmer = if has_stable_refs {
                    pending_refs.contains(&key)
                } else {
                    pending_ids.contains(&key) || pending_ids.contains(&block.content_id)
                };
                block.tooltip = tooltips
                    .get(&key)
                    .or_else(|| tooltips.get(&block.content_id))
                    .cloned();
                block
            })
            .collect()
    }

    /// Test helper: arm a run and immediately mark it delivered to its own
    /// session, which is what every caller that isn't a fork does one await
    /// later. Unit tests drive the lifecycle directly, so they need the run
    /// already past the dispatch gate.
    #[cfg(test)]
    pub(super) fn start_playbook_run(
        &self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        initial: Option<&[bool]>,
    ) -> Option<PlaybookRunProgress> {
        let run = self.start_playbook_run_with_dispatch_state(
            session_id,
            body,
            is_selection,
            initial,
            false,
            None,
        );
        if run.is_some() {
            self.mark_playbook_run_dispatched(
                session_id,
                construct_protocol::SessionState::AwaitingInput,
            );
        }
        run
    }

    pub(super) fn start_playbook_run_with_dispatch_state(
        &self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        initial: Option<&[bool]>,
        queued_behind_current_turn: bool,
        selection_block_ids: Option<&[String]>,
    ) -> Option<PlaybookRunProgress> {
        let (blocks, is_full_document_body) =
            match self.storage.read_playbook_with_blocks(session_id) {
                Ok((playbook, saved_blocks)) if playbook.markdown.trim() == body.trim() => {
                    (saved_blocks, true)
                }
                Ok((_playbook, saved_blocks)) => {
                    let by_ids = selection_block_ids
                        .filter(|ids| !ids.is_empty())
                        .map(|ids| playbook_run_blocks_from_ids(ids, &saved_blocks));
                    let blocks = by_ids
                        .unwrap_or_else(|| playbook_run_blocks_from_spans(body, &saved_blocks));
                    (blocks, false)
                }
                Err(_) => (playbook_run_blocks_from_spans(body, &[]), false),
            };
        if blocks.is_empty() {
            if let Ok(mut runs) = self.playbook_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let body_ids: std::collections::HashSet<String> =
            blocks.iter().map(block_ref_or_id).collect();
        let now_ms = Utc::now().timestamp_millis();
        let adds_to_existing = is_selection || (initial.is_some() && !is_full_document_body);
        let pending: std::collections::HashSet<String> =
            if let Some(decl) = initial.filter(|d| d.len() == blocks.len()) {
                // Explicit initial pending set, in document order (spec 0053).
                blocks
                    .iter()
                    .zip(decl.iter())
                    .filter(|(_, &on)| on)
                    .map(|(block, _)| block_ref_or_id(block))
                    .collect()
            } else if is_selection {
                body_ids
            } else if let Ok(runs) = self.playbook_runs.lock() {
                if let Some(old) = runs.get(session_id) {
                    // Re-run mid-flight preserves the agent's prior narrowing:
                    // keep only blocks that are still pending and still present.
                    let old_ids: std::collections::HashSet<String> = old
                        .pending_block_refs
                        .iter()
                        .chain(old.pending_block_ids.iter())
                        .cloned()
                        .collect();
                    let kept: std::collections::HashSet<String> =
                        body_ids.intersection(&old_ids).cloned().collect();
                    if kept.is_empty() {
                        body_ids
                    } else {
                        kept
                    }
                } else {
                    body_ids
                }
            } else {
                body_ids
            };
        if adds_to_existing {
            if let Ok(mut runs) = self.playbook_runs.lock() {
                if let Some(run) = runs.get_mut(session_id) {
                    let previous_pending: std::collections::HashSet<String> = run
                        .pending_block_refs
                        .iter()
                        .chain(run.pending_block_ids.iter())
                        .cloned()
                        .collect();
                    let mut added = 0usize;
                    for id in pending {
                        if previous_pending.contains(&id) {
                            continue;
                        }
                        if !run.pending_block_refs.contains(&id)
                            && !run.pending_block_ids.contains(&id)
                        {
                            run.pending_block_refs.push(id);
                            added += 1;
                        }
                    }
                    run.total_block_count = run.total_block_count.saturating_add(added);
                    if !run.agent_managed {
                        run.expires_at_ms = now_ms + PLAYBOOK_RUN_MAX_MS;
                    }
                    run.refresh_stage();
                    return Some(project_playbook_run_status(run.clone()));
                }
            }
        }
        if pending.is_empty() {
            // An explicit all-settled initial set leaves nothing to shimmer.
            if let Ok(mut runs) = self.playbook_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let total_block_count = pending.len();
        let run = PlaybookRunProgress {
            run_id: format!("{session_id}:{now_ms}"),
            started_at_ms: now_ms,
            expires_at_ms: now_ms + PLAYBOOK_RUN_MAX_MS,
            system_status: None,
            pending_block_ids: Vec::new(),
            pending_block_refs: pending.into_iter().collect(),
            pending_block_tooltips: std::collections::HashMap::new(),
            seen_running: false,
            first_output_seen: false,
            queued_behind_current_turn,
            // The run is armed before its prompt goes out (#1122), so it does
            // not yet belong to any session's turn: it binds to its execution
            // session, and starts reading that session's lifecycle, only once
            // the prompt has actually been delivered (spec 0176).
            execution_session_id: None,
            dispatched_at_ms: None,
            awaiting_turn_start: false,
            // Unmanaged until the agent narrows it with a declaration/edit.
            // Until then it is the optimistic full-playbook shimmer and stays
            // subject to the owning-session idle stop signal.
            agent_managed: false,
            stage: construct_protocol::PlaybookRunStage::Delivered,
            settled_block_count: 0,
            total_block_count,
        }
        .with_refreshed_stage();
        if let Ok(mut runs) = self.playbook_runs.lock() {
            runs.insert(session_id.to_string(), run.clone());
        }
        Some(project_playbook_run_status(run))
    }

    /// Apply a partial shimmer declaration after an edit (spec 0053): drop
    /// blocks whose id no longer exists (changed/removed), then set each
    /// declared id pending or settled. Ids absent from the post-edit document
    /// are ignored (fail closed — the block changed underneath the caller).
    pub(super) fn narrow_playbook_run(
        &self,
        session_id: &str,
        markdown: &str,
        decls: &[construct_protocol::PlaybookShimmerDecl],
    ) {
        let blocks = self.playbook_blocks_projection(session_id, markdown);
        let current = playbook_block_ids(&blocks);
        let by_decl: std::collections::HashMap<String, String> = blocks
            .iter()
            .flat_map(|block| {
                let key = block_ref_or_id(block);
                [(key.clone(), key.clone()), (block.content_id.clone(), key)]
            })
            .filter(|(from, _)| !from.is_empty())
            .collect();
        if let Ok(mut runs) = self.playbook_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A declaration/edit during the run means the agent is actively
            // managing it: from here on, trust explicit settlement and terminal
            // lifecycle signals, not a timer or the owning session's idle
            // transition (a self-scheduling agent goes idle while delegated or
            // background work is still in flight). See spec 0042.
            run.agent_managed = true;
            run.pending_block_refs.retain(|id| current.contains(id));
            run.pending_block_ids.retain(|id| current.contains(id));
            for decl in decls {
                let Some(key) = by_decl.get(&decl.id).cloned() else {
                    continue;
                };
                if decl.shimmer {
                    if !run.pending_block_refs.contains(&key) {
                        run.pending_block_refs.push(key.clone());
                    }
                    if let Some(tip) = decl
                        .tooltip
                        .as_deref()
                        .and_then(construct_protocol::normalize_playbook_tooltip)
                    {
                        run.pending_block_tooltips.insert(key, tip);
                    }
                } else {
                    run.pending_block_refs.retain(|id| id != &key);
                    run.pending_block_ids.retain(|id| id != &decl.id);
                    run.pending_block_tooltips.remove(&key);
                    run.pending_block_tooltips.remove(&decl.id);
                }
            }
            run.pending_block_tooltips.retain(|id, _| {
                run.pending_block_refs.contains(id) || run.pending_block_ids.contains(id)
            });
            run.pending_block_ids.clear();
            // An empty pending set does NOT remove the run mid-turn (spec
            // 0053): a still-running agent may re-declare a moved block's new
            // id next, and destroying the run would make that revival a no-op.
            // Idle/terminal reaping is owned by
            // note_session_state_for_playbook_run.
        }
    }

    /// Authoritatively replace a run's pending set with `pending` — a map from
    /// each pending block's id to its optional run-status tooltip, intersected
    /// with blocks present in `markdown`. Used by a playbook update's complete
    /// declaration (specs 0053, 0056); a no-op when no run is active.
    pub(super) fn set_playbook_run_pending(
        &self,
        session_id: &str,
        markdown: &str,
        pending: std::collections::HashMap<String, Option<String>>,
    ) {
        let blocks = self.playbook_blocks_projection(session_id, markdown);
        let current = playbook_block_ids(&blocks);
        let by_decl: std::collections::HashMap<String, String> = blocks
            .iter()
            .flat_map(|block| {
                let key = block_ref_or_id(block);
                [(key.clone(), key.clone()), (block.content_id.clone(), key)]
            })
            .filter(|(from, _)| !from.is_empty())
            .collect();
        if let Ok(mut runs) = self.playbook_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A complete declaration is active management (spec 0042): keep the
            // run alive past owning-session idle with no inactivity deadline.
            run.agent_managed = true;
            let pending: Vec<(String, Option<String>)> = pending
                .into_iter()
                .filter_map(|(id, tip)| {
                    by_decl
                        .get(&id)
                        .filter(|key| current.contains(*key))
                        .cloned()
                        .map(|key| (key, tip))
                })
                .collect();
            run.pending_block_tooltips = pending
                .iter()
                .filter_map(|(id, tip)| {
                    tip.as_deref()
                        .and_then(construct_protocol::normalize_playbook_tooltip)
                        .map(|t| (id.clone(), t))
                })
                .collect();
            run.pending_block_refs = pending.into_iter().map(|(id, _)| id).collect();
            run.pending_block_ids.clear();
            // An empty declaration mid-turn keeps the run alive for revival
            // until an authoritative lifecycle signal settles it (spec 0053).
        }
    }

    /// Settle pending blocks that explicitly delegate work to a session which
    /// has reached a terminal lifecycle state. This is the orphan cleanup for
    /// ordinary full-Playbook delegation: selection-Run forks have additional
    /// dispatch tracking, but a normal managed run expresses responsibility by
    /// carrying `@{session:<id>}` on the pending block itself.
    fn settle_playbook_blocks_for_terminal_clip(&self, terminal_session_id: &str) {
        let owners: Vec<String> = self
            .playbook_runs
            .lock()
            .map(|runs| runs.keys().cloned().collect())
            .unwrap_or_default();
        let mut matches = Vec::new();
        for owner in owners {
            let Ok((playbook, blocks)) = self.storage.read_playbook_with_blocks(&owner) else {
                continue;
            };
            let terminal_refs: Vec<(String, String)> = blocks
                .iter()
                .filter(|block| {
                    construct_protocol::playbook_scan_smart_clips(&block.text)
                        .iter()
                        .any(|clip| {
                            clip.type_name == "session" && clip.target == terminal_session_id
                        })
                })
                .map(|block| (block_ref_or_id(block), block.content_id.clone()))
                .collect();
            if !terminal_refs.is_empty() {
                matches.push((owner, playbook, terminal_refs));
            }
        }

        let mut changed = Vec::new();
        if let Ok(mut runs) = self.playbook_runs.lock() {
            for (owner, playbook, terminal_refs) in matches {
                let mut settled_last_block = false;
                let Some(run) = runs.get_mut(&owner) else {
                    continue;
                };
                let before = run.pending_block_count();
                for (block_ref, content_id) in terminal_refs {
                    run.pending_block_refs.retain(|id| id != &block_ref);
                    run.pending_block_ids
                        .retain(|id| id != &block_ref && id != &content_id);
                    run.pending_block_tooltips.remove(&block_ref);
                    run.pending_block_tooltips.remove(&content_id);
                }
                if run.pending_block_count() != before {
                    run.refresh_stage();
                    settled_last_block = run.pending_block_count() == 0;
                    changed.push(playbook);
                }
                if settled_last_block {
                    runs.remove(&owner);
                }
            }
        }
        for playbook in changed {
            self.broadcast_playbook_state(playbook);
        }
    }

    /// The active run's pending blocks keyed by their *block id* — the part of
    /// a block ref that survives a semantic edit — with each one's current
    /// tooltip.
    ///
    /// Block refs are `block_id:content_epoch`, and editing a block's text
    /// advances the epoch, so a pending ref stops matching the block it names
    /// the moment anyone types in it. That is deliberate for stale agent
    /// declarations (spec 0053), but it must not settle work that is still in
    /// flight just because a human annotated the task while the agent worked
    /// on it (spec 0048: shimmer tracks the work, not the text). Keying by
    /// block id gives the edit path a way to carry the pending state — and the
    /// agent's status tooltip — onto the block's new ref.
    pub(super) fn playbook_run_pending_by_block_id(
        &self,
        session_id: &str,
    ) -> std::collections::HashMap<String, Option<String>> {
        let Ok(runs) = self.playbook_runs.lock() else {
            return Default::default();
        };
        let Some(run) = runs.get(session_id) else {
            return Default::default();
        };
        run.pending_block_refs
            .iter()
            .filter_map(|r| {
                let block_id = r.split(':').next().filter(|id| !id.is_empty())?;
                Some((
                    block_id.to_string(),
                    run.pending_block_tooltips.get(r).cloned(),
                ))
            })
            .collect()
    }

    /// Drop a session's active run, for a dispatch that never left the
    /// building — the fork failed to spawn, or the prompt could not be
    /// delivered. Without this the playbook would shimmer for a turn that is
    /// never going to happen (#1122).
    pub(super) fn clear_playbook_run(&self, session_id: &str) {
        let removed = self
            .playbook_runs
            .lock()
            .ok()
            .and_then(|mut runs| runs.remove(session_id))
            .is_some();
        if removed {
            if let Ok(playbook) = self.storage.read_playbook(session_id) {
                self.broadcast_playbook_state(playbook);
            }
        }
    }

    /// Bind an armed run to the session its turn will actually happen in.
    /// Called as soon as a fork Run knows its fork's id, so the owner's own
    /// lifecycle can never be mistaken for the fork's work (spec 0176).
    pub(super) fn bind_playbook_run_execution(&self, session_id: &str, execution_session_id: &str) {
        if let Ok(mut runs) = self.playbook_runs.lock() {
            if let Some(run) = runs.get_mut(session_id) {
                run.execution_session_id = Some(execution_session_id.to_string());
            }
        }
    }

    /// The run's prompt has reached `execution_session_id`: from here on that
    /// session's turn is this run's turn (spec 0176).
    ///
    /// `state_at_dispatch` is the execution session's state as the daemon has
    /// observed it so far. Anything but idle means a turn is already in
    /// flight — the session's boot, or whatever it was doing before — so the
    /// `Running` currently in effect is not ours: this run's turn starts at
    /// the next `Running` after an intervening idle. Because the daemon's view
    /// of a session's state is advanced by the same in-order event drain that
    /// reports these transitions, an observed idle also means every event the
    /// adapter emitted before it has already been consumed — so a `Running`
    /// seen after an idle-at-dispatch can only be a new turn.
    pub(super) fn mark_playbook_run_dispatched(
        &self,
        execution_session_id: &str,
        state_at_dispatch: construct_protocol::SessionState,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        if let Ok(mut runs) = self.playbook_runs.lock() {
            // The run is keyed by its Playbook's session; a fork Run's
            // execution session is a different one, already bound above.
            let key = runs
                .iter()
                .find(|(key, run)| {
                    run.execution_session_id.as_deref() == Some(execution_session_id)
                        || (run.execution_session_id.is_none()
                            && key.as_str() == execution_session_id)
                })
                .map(|(key, _)| key.clone());
            if let Some(run) = key.and_then(|key| runs.get_mut(&key)) {
                run.execution_session_id = Some(execution_session_id.to_string());
                run.dispatched_at_ms = Some(now_ms);
                run.awaiting_turn_start =
                    state_at_dispatch != construct_protocol::SessionState::AwaitingInput;
            }
        }
    }

    /// Find the run whose lifecycle `session_id` drives: the one it executes,
    /// falling back to the one it owns while that run has no execution
    /// session bound yet.
    fn playbook_run_key_for_execution_session(
        runs: &std::collections::HashMap<String, PlaybookRunProgress>,
        session_id: &str,
    ) -> Option<String> {
        runs.iter()
            .find(|(key, run)| match run.execution_session_id.as_deref() {
                Some(exec) => exec == session_id,
                None => key.as_str() == session_id,
            })
            .map(|(key, _)| key.clone())
    }

    pub(super) fn mark_playbook_run_output_seen(&self, session_id: &str) {
        let mut updated = false;
        let mut owner: Option<String> = None;
        if let Ok(mut runs) = self.playbook_runs.lock() {
            let key = Self::playbook_run_key_for_execution_session(&runs, session_id);
            if let Some(key) = key {
                if let Some(run) = runs.get_mut(&key) {
                    // Output from before the prompt was delivered is the
                    // session's own boot chatter, not this run producing
                    // something (spec 0176).
                    if !run.first_output_seen && run.dispatched_at_ms.is_some() {
                        run.first_output_seen = true;
                        updated = true;
                        owner = Some(key);
                    }
                }
            }
        }
        let session_id = owner.as_deref().unwrap_or(session_id);
        if updated {
            if let Ok(playbook) = self.storage.read_playbook(session_id) {
                self.broadcast_playbook_state(playbook);
            }
        }
    }

    pub(super) fn note_session_state_for_playbook_run(
        &self,
        session_id: &str,
        state: construct_protocol::SessionState,
    ) {
        use construct_protocol::SessionState;
        // A terminal worker may not execute or own the run at all; its smart
        // clip is the relationship to the pending block. Process that orphan
        // cleanup before the execution-session lookup can return early.
        if state.is_terminal() {
            self.settle_playbook_blocks_for_terminal_clip(session_id);
        }
        let now_ms = Utc::now().timestamp_millis();
        let mut clear = false;
        let mut updated = false;
        let mut owner: Option<String> = None;
        if let Ok(mut runs) = self.playbook_runs.lock() {
            // Progress and stop signals come from the session executing the
            // run — the fork for a fork Run — never from a bystander that
            // merely owns the Playbook (spec 0176). A terminal state is the
            // exception: the Playbook's own session dying takes its run with
            // it, because nothing is left to render or settle it.
            let executed_key = runs
                .iter()
                .find(|(_, run)| run.execution_session_id.as_deref() == Some(session_id))
                .map(|(key, _)| key.clone());
            let is_execution_session = executed_key.is_some();
            let key = match executed_key {
                Some(key) => key,
                // Not executing anything: only this session's own run, and
                // only its death, is ours to act on.
                None if runs.contains_key(session_id)
                    && matches!(state, SessionState::Done | SessionState::Errored) =>
                {
                    session_id.to_string()
                }
                None => return,
            };
            owner = Some(key.clone());
            if let Some(run) = runs.get_mut(&key) {
                // Nothing this session reports counts toward the run until the
                // run's prompt has actually reached it: before that, every
                // transition is the session's boot or its previous turn
                // winding down (spec 0176). This is what makes arming the run
                // before delivery (#1122) safe.
                let dispatched = is_execution_session && run.dispatched_at_ms.is_some();
                match state {
                    SessionState::Running if dispatched && !run.awaiting_turn_start => {
                        if !run.seen_running {
                            run.seen_running = true;
                            updated = true;
                        }
                    }
                    SessionState::Running => {}
                    SessionState::AwaitingInput if dispatched && run.awaiting_turn_start => {
                        // The turn that was already in flight at dispatch has
                        // ended. This idle belongs to it, not to us; the next
                        // Running is the start of our turn.
                        run.awaiting_turn_start = false;
                    }
                    SessionState::AwaitingInput if !dispatched => {}
                    SessionState::Done | SessionState::Errored => {
                        // Terminal: the owning agent is gone and can never
                        // settle the remaining blocks, so clear
                        // authoritatively — whether or not it is
                        // agent-managed, and whether or not it was ever seen
                        // running. Gating this on `seen_running` meant a
                        // session that died before its first Running
                        // transition (a crashed harness, a rejected prompt, a
                        // killed session) left the whole playbook shimmering
                        // until its unmanaged safety deadline, with nothing
                        // left alive that could ever settle it (#1090).
                        clear = true;
                    }
                    SessionState::AwaitingInput => {
                        // Idle but still alive. For an unmanaged run (a
                        // non-declaring harness's optimistic shimmer, never
                        // narrowed) this is the turn-end stop signal. For a
                        // managed run it is NOT — unless its pending set is empty:
                        // a self-scheduling agent goes idle while delegated work
                        // is still pending (keep shimmering), but a managed run
                        // with nothing pending has either finished or only
                        // transiently emptied, and an idle turn means there is no
                        // pending declaration to revive — so reap it rather than
                        // letting an empty managed record linger indefinitely.
                        // See specs 0042 and 0053.
                        if run.seen_running
                            && (!run.agent_managed
                                || (run.pending_block_refs.is_empty()
                                    && run.pending_block_ids.is_empty()))
                        {
                            clear = true;
                        }
                        // Idle without ever having reported a turn. Past a
                        // short debounce this is a dispatch that went nowhere
                        // — a harness that rejected the prompt or failed
                        // before starting — and no other stop signal can ever
                        // fire for it, because they all hang off the session
                        // reporting something (#1090).
                        //
                        // This cannot truncate a long turn. A session actually
                        // working reports Running, and one that has been seen
                        // running is handled above and keeps shimmering for
                        // however many hours the work takes. The debounce
                        // covers the dispatch window itself, where a trailing
                        // idle from the previous turn can still arrive before
                        // this one starts. It runs from delivery, not from
                        // when the run was armed: a fork Run's prompt only
                        // goes out once the fork's harness is ready, which can
                        // be seconds after arming (spec 0149).
                        if !run.seen_running
                            && !run.first_output_seen
                            && !run.agent_managed
                            && now_ms.saturating_sub(
                                run.dispatched_at_ms.unwrap_or(run.started_at_ms),
                            ) > PLAYBOOK_RUN_IDLE_WITHOUT_TURN_GRACE_MS
                        {
                            clear = true;
                        }
                    }
                    SessionState::Pending | SessionState::Paused => {}
                }
            }
            if clear {
                runs.remove(&key);
            }
        }
        let session_id = owner.as_deref().unwrap_or(session_id);
        if clear || updated {
            if let Ok(playbook) = self.storage.read_playbook(session_id) {
                self.broadcast_playbook_state(playbook);
            }
        }
    }
}
