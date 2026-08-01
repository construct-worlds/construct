use super::*;

impl App {
    pub fn open_service_title_menu(&mut self, name: String, view: ratatui::layout::Rect) {
        const MENU_W: u16 = 34;
        let menu_h = ServiceTitleMenuAction::ALL.len() as u16 + 2;
        let width = MENU_W.min(view.width.saturating_sub(2).max(1));
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
            ServiceTitleMenuAction::Edit => {
                self.open_edit_service_view(&name);
            }
            ServiceTitleMenuAction::RotateToken => {
                self.update_service_from_title_menu(&name, true, None).await;
            }
            ServiceTitleMenuAction::PauseResume => {
                let paused = self
                    .services
                    .iter()
                    .find(|service| service.name == name)
                    .is_some_and(|service| service.paused);
                self.update_service_from_title_menu(&name, false, Some(!paused))
                    .await;
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

    async fn update_service_from_title_menu(
        &mut self,
        name: &str,
        rotate_token: bool,
        paused: Option<bool>,
    ) {
        let Some(mut service) = self
            .services
            .iter()
            .find(|service| service.name == name)
            .cloned()
        else {
            self.set_status(format!("service {name} not found"));
            return;
        };
        if let Some(paused) = paused {
            service.paused = paused;
        }
        if rotate_token {
            let Some(channel_id) = service
                .channels
                .iter()
                .find(|channel| channel.kind == "http")
                .map(|channel| channel.id.clone())
            else {
                self.set_status(format!("service {name} has no HTTP channel"));
                return;
            };
            match self
                .client
                .rotate_service_channel_secret(name, &channel_id)
                .await
            {
                Ok(result) => {
                    self.refresh_services().await;
                    self.set_status(format!(
                        "new credential for {name} / {}: {} (shown once; restart to apply)",
                        result.channel.id,
                        result.new_secret.unwrap_or_default()
                    ));
                }
                Err(error) => self.set_status(format!("credential rotation failed: {error}")),
            }
            return;
        }
        match self
            .client
            .put_service(construct_protocol::ServicePutParams { service })
            .await
        {
            Ok(result) => {
                self.refresh_services().await;
                self.set_status(format!(
                    "{} {}; restart daemon to apply",
                    result.service.name,
                    if result.service.paused { "paused" } else { "resumed" }
                ));
            }
            Err(error) => {
                self.set_status(format!("service update failed: {error}"));
            }
        }
    }
}
