use super::*;

fn program_block_ids(body: &str) -> std::collections::HashSet<String> {
    agentd_protocol::program_block_spans(body)
        .into_iter()
        .map(|span| span.id)
        .collect()
}

impl SessionManager {
    pub(super) fn program_run_snapshot(&self, session_id: &str) -> Option<ProgramRunProgress> {
        let now_ms = Utc::now().timestamp_millis();
        let mut runs = self.program_runs.lock().ok()?;
        let expired = runs
            .get(session_id)
            .is_some_and(|run| run.expires_at_ms <= now_ms);
        if expired {
            runs.remove(session_id);
            return None;
        }
        // An empty pending set means nothing shimmers right now, so report no
        // active run — but KEEP the record so a follow-up declaration can revive
        // it within the same turn (spec 0053): a move/annotate that changes a
        // still-pending block transiently empties the set before the new id is
        // declared, and that must not destroy the run. The record is reaped when
        // the owning session goes idle/terminal or the inactivity backstop fires.
        match runs.get(session_id) {
            Some(run) if !run.pending_block_ids.is_empty() => Some(run.clone()),
            _ => None,
        }
    }

    /// Build the per-block projection (spec 0053): each block of `markdown` with
    /// its stable id, text, and current shimmer state from the active run.
    pub(super) fn program_blocks_projection(
        &self,
        session_id: &str,
        markdown: &str,
    ) -> Vec<agentd_protocol::ProgramBlockView> {
        let pending: std::collections::HashSet<String> = self
            .program_run_snapshot(session_id)
            .map(|run| run.pending_block_ids.into_iter().collect())
            .unwrap_or_default();
        agentd_protocol::program_block_spans(markdown)
            .into_iter()
            .map(|span| agentd_protocol::ProgramBlockView {
                shimmer: pending.contains(&span.id),
                id: span.id,
                start_line: span.start_line,
                end_line: span.end_line,
                text: span.text,
            })
            .collect()
    }

    pub(super) fn start_program_run(
        &self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        initial: Option<&[bool]>,
    ) -> Option<ProgramRunProgress> {
        let spans = agentd_protocol::program_block_spans(body);
        if spans.is_empty() {
            if let Ok(mut runs) = self.program_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let body_ids: std::collections::HashSet<String> =
            spans.iter().map(|s| s.id.clone()).collect();
        let now_ms = Utc::now().timestamp_millis();
        let pending: std::collections::HashSet<String> =
            if let Some(decl) = initial.filter(|d| d.len() == spans.len()) {
                // Explicit initial pending set, in document order (spec 0053).
                spans
                    .iter()
                    .zip(decl.iter())
                    .filter(|(_, &on)| on)
                    .map(|(s, _)| s.id.clone())
                    .collect()
            } else if is_selection {
                body_ids
            } else if let Ok(runs) = self.program_runs.lock() {
                if let Some(old) = runs.get(session_id) {
                    // Re-run mid-flight preserves the agent's prior narrowing:
                    // keep only blocks that are still pending and still present.
                    let old_ids: std::collections::HashSet<String> =
                        old.pending_block_ids.iter().cloned().collect();
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
        if pending.is_empty() {
            // An explicit all-settled initial set leaves nothing to shimmer.
            if let Ok(mut runs) = self.program_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let run = ProgramRunProgress {
            run_id: format!("{session_id}:{now_ms}"),
            started_at_ms: now_ms,
            expires_at_ms: now_ms + PROGRAM_RUN_MAX_MS,
            pending_block_ids: pending.into_iter().collect(),
            seen_running: false,
            first_output_seen: false,
            // Unmanaged until the agent narrows it with a declaration/edit.
            // Until then it is the optimistic full-program shimmer and stays
            // subject to the owning-session idle stop signal.
            agent_managed: false,
        };
        if let Ok(mut runs) = self.program_runs.lock() {
            runs.insert(session_id.to_string(), run.clone());
        }
        Some(run)
    }

    /// Apply a partial shimmer declaration after an edit (spec 0053): drop
    /// blocks whose id no longer exists (changed/removed), then set each
    /// declared id pending or settled. Ids absent from the post-edit document
    /// are ignored (fail closed — the block changed underneath the caller).
    pub(super) fn narrow_program_run(
        &self,
        session_id: &str,
        markdown: &str,
        decls: &[agentd_protocol::ProgramShimmerDecl],
    ) {
        let now_ms = Utc::now().timestamp_millis();
        let current = program_block_ids(markdown);
        if let Ok(mut runs) = self.program_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A declaration/edit during the run means the agent is actively
            // managing it: from here on, trust the declarations and the
            // inactivity backstop to clear it, not the owning session's idle
            // transition (a self-scheduling agent goes idle while delegated or
            // background work is still in flight). See spec 0042.
            run.agent_managed = true;
            // Refresh the inactivity backstop — the run is still being worked.
            run.expires_at_ms = now_ms + PROGRAM_RUN_MAX_MS;
            run.pending_block_ids.retain(|id| current.contains(id));
            for decl in decls {
                if !current.contains(&decl.id) {
                    continue;
                }
                if decl.shimmer {
                    if !run.pending_block_ids.contains(&decl.id) {
                        run.pending_block_ids.push(decl.id.clone());
                    }
                } else {
                    run.pending_block_ids.retain(|id| id != &decl.id);
                }
            }
            // Reap only on the inactivity backstop. An empty pending set does
            // NOT remove the run mid-turn (spec 0053): a still-running agent may
            // re-declare a moved block's new id next, and destroying the run
            // would make that revival a no-op. Idle/terminal reaping is owned by
            // note_session_state_for_program_run.
            if run.expires_at_ms <= now_ms {
                runs.remove(session_id);
            }
        }
    }

    /// Authoritatively replace a run's pending set with `pending_ids`
    /// (intersected with blocks present in `markdown`). Used by a program
    /// update's complete declaration (spec 0053); a no-op when no run is active.
    pub(super) fn set_program_run_pending(
        &self,
        session_id: &str,
        markdown: &str,
        pending_ids: std::collections::HashSet<String>,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        let current = program_block_ids(markdown);
        if let Ok(mut runs) = self.program_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A complete declaration is active management (spec 0042): keep the
            // run alive past owning-session idle and refresh the backstop.
            run.agent_managed = true;
            run.expires_at_ms = now_ms + PROGRAM_RUN_MAX_MS;
            run.pending_block_ids = pending_ids
                .into_iter()
                .filter(|id| current.contains(id))
                .collect();
            // Reap only on the inactivity backstop (spec 0053); an empty
            // declaration mid-turn keeps the run alive for revival.
            if run.expires_at_ms <= now_ms {
                runs.remove(session_id);
            }
        }
    }

    pub(super) fn mark_program_run_output_seen(&self, session_id: &str) {
        let mut updated = false;
        if let Ok(mut runs) = self.program_runs.lock() {
            if let Some(run) = runs.get_mut(session_id) {
                if !run.first_output_seen {
                    run.first_output_seen = true;
                    updated = true;
                }
            }
        }
        if updated {
            if let Ok(program) = self.storage.read_program(session_id) {
                self.broadcast_program_state(program);
            }
        }
    }

    pub(super) fn note_session_state_for_program_run(
        &self,
        session_id: &str,
        state: agentd_protocol::SessionState,
    ) {
        use agentd_protocol::SessionState;
        let mut clear = false;
        let mut updated = false;
        if let Ok(mut runs) = self.program_runs.lock() {
            if let Some(run) = runs.get_mut(session_id) {
                match state {
                    SessionState::Running => {
                        if !run.seen_running {
                            run.seen_running = true;
                            updated = true;
                        }
                    }
                    SessionState::Done | SessionState::Errored => {
                        // Terminal: the owning agent is gone and can never
                        // settle the remaining blocks, so clear authoritatively
                        // once the run was seen running — whether or not it is
                        // agent-managed.
                        if run.seen_running {
                            clear = true;
                        }
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
                        // letting an empty record linger to the backstop. See
                        // specs 0042 and 0053.
                        if run.seen_running
                            && (!run.agent_managed || run.pending_block_ids.is_empty())
                        {
                            clear = true;
                        }
                    }
                    SessionState::Pending | SessionState::Paused => {}
                }
            }
            if clear {
                runs.remove(session_id);
            }
        }
        if clear || updated {
            if let Ok(program) = self.storage.read_program(session_id) {
                self.broadcast_program_state(program);
            }
        }
    }
}
