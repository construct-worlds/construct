use super::*;

/// How long a tally panel outlives the pointer leaving it. Long enough to
/// travel from the tally down onto the panel — without the grace the panel
/// would close in the gap between the two and could never be clicked.
const FLEET_PANEL_LINGER: Duration = Duration::from_millis(500);

impl App {
    /// Open, refresh, or expire the fleet-tally panel.
    ///
    /// `now` is passed in rather than read from the clock so the linger is
    /// testable without sleeping, matching `matrix_widget_visible`.
    ///
    /// Called from the run loop before each draw, not from the frame timer:
    /// the loop body also runs after every mouse event, so hover-open lands
    /// on the next frame instead of waiting out a tick.
    pub fn update_fleet_tally_panel(&mut self, now: Instant) {
        // Help swallows mouse-down before the click handler ever sees it, so
        // rows shown under it would be unclickable — don't show them.
        if self.help_visible {
            self.fleet_panel = None;
            return;
        }

        let pointer = self.mouse_pos;
        let hovered = pointer.and_then(|(mx, my)| {
            self.layout
                .list_title_tally_hits
                .iter()
                .find(|h| h.contains(mx, my))
                .cloned()
        });

        // Hovering a tally opens its panel; hovering a *different* tally
        // switches to that one rather than leaving the old panel up.
        if let Some(hit) = &hovered {
            if self.fleet_panel.as_ref().is_none_or(|p| p.kind != hit.kind) {
                self.fleet_panel = Some(FleetTallyPanel {
                    kind: hit.kind,
                    rows: Vec::new(),
                    overflow: 0,
                    area: ratatui::layout::Rect::default(),
                    anchor: hit.clone(),
                    pinned: false,
                    hide_after: None,
                });
            }
        }

        let Some(mut panel) = self.fleet_panel.take() else {
            return;
        };

        // Re-anchor against this frame's tallies. A missing anchor means the
        // tally is gone — the bucket emptied, the sidebar collapsed, or the
        // title slid off under a tall footer — so the panel goes with it.
        let Some(anchor) = self
            .layout
            .list_title_tally_hits
            .iter()
            .find(|h| h.kind == panel.kind)
            .cloned()
        else {
            return;
        };
        panel.anchor = anchor;

        // Rows are rebuilt from the live list every frame, so the panel can
        // never list something the tally has stopped counting.
        let items = self.list_items();
        let all = crate::ui::fleet_status_buckets(&items)
            .bucket(panel.kind)
            .to_vec();
        if all.is_empty() {
            return;
        }
        let Some(frame) = self.layout.frame_area else {
            return;
        };
        let title = panel.kind.tooltip(all.len());
        let (area, shown) = crate::ui::fleet_tally_panel_geometry(&all, &title, &panel.anchor, frame);
        panel.overflow = all.len().saturating_sub(shown);
        panel.rows = all.into_iter().take(shown).collect();
        panel.area = area;

        // Held open over either surface; the grace period covers the gap
        // between them. Pinning opts out of expiry entirely.
        let over_panel = pointer.is_some_and(|(mx, my)| panel.contains(mx, my));
        if panel.pinned || hovered.is_some() || over_panel {
            panel.hide_after = None;
        } else {
            match panel.hide_after {
                None => panel.hide_after = Some(now + FLEET_PANEL_LINGER),
                Some(deadline) if deadline <= now => return,
                Some(_) => {}
            }
        }

        self.fleet_panel = Some(panel);
    }

    /// The panel eats the wheel while the pointer is over it. It doesn't
    /// scroll — but letting the wheel through would scroll the session list
    /// underneath a stationary floating index, which reads as broken.
    pub fn fleet_panel_owns_wheel(&self, col: u16, row: u16) -> bool {
        self.fleet_panel
            .as_ref()
            .is_some_and(|p| p.contains(col, row))
    }

    /// Click on a tally: pin its panel open, or close an already-pinned one.
    ///
    /// Pinning is the only way in on terminals that never forward mouse
    /// motion (macOS Terminal.app among them), where a hover-only panel
    /// would be permanently invisible.
    pub fn toggle_fleet_tally_panel(&mut self, hit: &FleetTallyHit) {
        match self.fleet_panel.as_mut() {
            Some(p) if p.kind == hit.kind && p.pinned => self.fleet_panel = None,
            // Already open from a hover: clicking commits it.
            Some(p) if p.kind == hit.kind => {
                p.pinned = true;
                p.hide_after = None;
            }
            // Rows and geometry are filled in by the updater before the
            // next draw.
            _ => {
                self.fleet_panel = Some(FleetTallyPanel {
                    kind: hit.kind,
                    rows: Vec::new(),
                    overflow: 0,
                    area: ratatui::layout::Rect::default(),
                    anchor: hit.clone(),
                    pinned: true,
                    hide_after: None,
                });
            }
        }
    }

    /// Select what a panel row points at and bring it on screen. The list
    /// doesn't scroll to follow selection on its own, so without the
    /// scroll request this click could land on a row still below the fold —
    /// the exact problem the panel exists to solve.
    pub fn activate_fleet_panel_row(&mut self, row: &crate::ui::FleetPanelRow) {
        self.fleet_panel = None;
        self.focus = PaneFocus::List;
        match &row.target {
            crate::ui::FleetPanelTarget::Session(id) => {
                self.select_session(id.clone());
                self.list_scroll_target = Some(id.clone());
            }
            crate::ui::FleetPanelTarget::Group(id) => {
                self.select_group(id.clone());
                self.list_scroll_target = Some(id.clone());
            }
        }
        self.sync_active_window_selection();
    }
}
