use super::*;

impl App {
    pub fn open_service_title_menu(&mut self, name: String, view: ratatui::layout::Rect) {
        let menu_h = ServiceTitleMenuAction::ALL.len() as u16 + 2;
        let width = fleet_title_menu_width(&name, view.width);
        let x = view
            .x
            .saturating_add(view.width)
            .saturating_sub(width.saturating_add(1));
        self.service_title_menu = Some(ServiceTitleMenu {
            name,
            area: ratatui::layout::Rect {
                x,
                y: view.y.saturating_add(1),
                width,
                height: menu_h.min(view.height.saturating_sub(1).max(3)),
            },
        });
    }

    pub(super) async fn run_service_title_menu_action(
        &mut self,
        name: String,
        action: ServiceTitleMenuAction,
    ) {
        self.service_title_menu = None;
        if self.selection.service_name() != Some(name.as_str()) {
            self.select_service(name.clone());
        }

        match action {
            ServiceTitleMenuAction::CopyId => {
                self.run_action(crate::keymap::KeyAction::CopySelectedId)
                    .await
            }
            ServiceTitleMenuAction::SplitHorizontal => {
                self.split_active_window(WindowSplitDirection::Right)
            }
            ServiceTitleMenuAction::SplitVertical => {
                self.split_active_window(WindowSplitDirection::Below)
            }
            ServiceTitleMenuAction::CloseSplit => self.delete_active_window(),
            ServiceTitleMenuAction::Delete => {
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("Delete service {name}? [y/N] "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::ServiceDeleteConfirm { name },
                    error: None,
                });
            }
        }
    }
}
