//! Anchored, per-session hover/pin preview of a session's fork/subagent
//! lineage tree (spec 0079-fork-and-subagent-lineage-view), triggered from
//! the harness label in that session's own pane title bar
//! (`apply_pane_title_right_cluster` in `ui.rs`) — a small, session-attached
//! surface layered on top of the existing label, distinct from the
//! full-screen `C-x q` / `q` modal (`lineage_popup`), which stays exactly as
//! it is: a genuine global dialog with its own dedicated `App` slot.
//!
//! This mirrors the shape of the (soon-to-be-removed, spec 0003) session
//! widget hover/pin system — `DynamicUiHover` +
//! `App::dynamic_ui_panel_visible` + `App::toggle_dynamic_ui_widget_pin`
//! (`dynamic_ui.rs`) — as independent state, rather than depending on that
//! system directly.

use std::time::Instant;

use super::App;

/// A session's lineage preview shown transiently because the cursor is over
/// its harness label. `until` is the expiry; every render frame the pointer
/// still sits on the trigger (or the preview body itself) pushes it out —
/// see `crate::app::LINEAGE_PREVIEW_HOVER_GRACE_MS`. Cleared once it lapses
/// or hover moves to a different session's label. At most one across the
/// fleet, mirroring `DynamicUiHover`/`MatrixWidgetHover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineagePreviewHover {
    pub session_id: String,
    pub until: Instant,
}

impl App {
    /// Whether `session_id`'s lineage preview should render this frame:
    /// pinned OR an unexpired hover — the same "pinned-OR-unexpired-hover"
    /// shape as `App::dynamic_ui_panel_visible`, kept as independent state.
    pub fn lineage_preview_visible(&self, session_id: &str) -> bool {
        if self.lineage_preview_pinned.contains(session_id) {
            return true;
        }
        self.lineage_preview_hover
            .as_ref()
            .is_some_and(|h| h.session_id == session_id && h.until > Instant::now())
    }

    /// Toggle a session's lineage preview pin from a harness-label click —
    /// the shape to mirror is `App::toggle_dynamic_ui_widget_pin`.
    pub fn toggle_lineage_preview_pin(&mut self, session_id: String) {
        if !self.lineage_preview_pinned.remove(&session_id) {
            self.lineage_preview_pinned.insert(session_id.clone());
        }
        // The click outcome is authoritative; drop any hover preview of this
        // session so the rendered state reflects the pin toggle immediately.
        if self
            .lineage_preview_hover
            .as_ref()
            .is_some_and(|h| h.session_id == session_id)
        {
            self.lineage_preview_hover = None;
        }
    }

    /// Whether `(col, row)` lands inside the last-rendered lineage preview
    /// box, if one is showing. Used to swallow clicks/drag-starts over the
    /// preview body so it doesn't act as a click-through onto the pane
    /// content underneath — mirrors `is_over_dynamic_ui_overlay`'s role for
    /// the widget popover, kept independent of that system.
    pub(super) fn is_over_lineage_preview(&self, col: u16, row: u16) -> bool {
        self.layout
            .lineage_preview_area
            .is_some_and(|area| Self::rect_contains(area, col, row))
    }
}
