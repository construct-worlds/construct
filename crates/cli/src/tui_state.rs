//! Tiny per-user TUI preferences that persist across launches.
//!
//! Only the bare minimum lives here: what session was selected
//! last and a few layout preferences, so reopening the TUI lands
//! the user back where they left off. The file is JSON to stay
//! forgiving — extra keys from a newer client read by an older one
//! are ignored, and a corrupt file just resets to defaults instead
//! of failing the launch.

use std::path::PathBuf;

use agentd_protocol::paths::Paths;
use serde::{Deserialize, Serialize};

fn default_hide_pane_side_borders() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiState {
    #[serde(default)]
    pub last_selected_session_id: Option<String>,
    #[serde(default)]
    pub zoom: crate::app::ZoomMode,
    #[serde(default)]
    pub list_panel_w: Option<u16>,
    #[serde(default)]
    pub pin_strip_h: Option<u16>,
    #[serde(default)]
    pub orchestrator_panel_h: Option<u16>,
    #[serde(default)]
    pub matrix_rain_h: Option<u16>,
    #[serde(default)]
    pub list_collapsed: bool,
    #[serde(default)]
    pub matrix_rain_hidden: bool,
    #[serde(default = "default_hide_pane_side_borders")]
    pub hide_pane_side_borders: bool,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            last_selected_session_id: None,
            zoom: crate::app::ZoomMode::default(),
            list_panel_w: None,
            pin_strip_h: None,
            orchestrator_panel_h: None,
            matrix_rain_h: None,
            list_collapsed: false,
            matrix_rain_hidden: false,
            hide_pane_side_borders: default_hide_pane_side_borders(),
        }
    }
}

fn state_path() -> PathBuf {
    Paths::discover().tui_state_file()
}

pub fn load() -> TuiState {
    let path = state_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return TuiState::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save(state: &TuiState) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::TuiState;

    #[test]
    fn defaults_to_hiding_pane_side_borders() {
        assert!(TuiState::default().hide_pane_side_borders);
    }

    #[test]
    fn missing_border_preference_uses_new_default() {
        let state: TuiState = serde_json::from_str("{}").expect("valid empty state");

        assert!(state.hide_pane_side_borders);
    }

    #[test]
    fn persisted_border_preference_still_round_trips() {
        let state: TuiState =
            serde_json::from_str(r#"{"hide_pane_side_borders":false}"#).expect("valid state");

        assert!(!state.hide_pane_side_borders);
    }
}
