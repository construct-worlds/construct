use super::*;

impl App {
    pub fn open_operator_title_menu(&mut self, name: String, view: ratatui::layout::Rect) {
        let menu_h = OperatorTitleMenuAction::ALL.len() as u16 + 2;
        let width = fleet_title_menu_width(&name, view.width);
        let x = view
            .x
            .saturating_add(view.width)
            .saturating_sub(width.saturating_add(1));
        self.operator_title_menu = Some(OperatorTitleMenu {
            name,
            area: ratatui::layout::Rect {
                x,
                y: view.y.saturating_add(1),
                width,
                height: menu_h.min(view.height.saturating_sub(1).max(3)),
            },
        });
    }

    pub(super) async fn run_operator_title_menu_action(
        &mut self,
        name: String,
        action: OperatorTitleMenuAction,
    ) {
        self.operator_title_menu = None;
        if self.selection.operator_name() != Some(name.as_str()) {
            self.select_operator(name.clone());
        }

        match action {
            OperatorTitleMenuAction::CopyId => {
                self.run_action(crate::keymap::KeyAction::CopySelectedId)
                    .await
            }
            OperatorTitleMenuAction::SplitHorizontal => {
                self.split_active_window(WindowSplitDirection::Right)
            }
            OperatorTitleMenuAction::SplitVertical => {
                self.split_active_window(WindowSplitDirection::Below)
            }
            OperatorTitleMenuAction::CloseSplit => self.delete_active_window(),
            OperatorTitleMenuAction::Delete => {
                // Same path as C-x k / dd on a selected operator.
                self.run_action(crate::keymap::KeyAction::OpenDeleteConfirm)
                    .await
            }
        }
    }
}
