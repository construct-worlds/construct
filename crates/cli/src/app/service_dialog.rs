//! Inline service-view editor and service definition lifecycle.

use super::*;

const FIELD_COUNT: usize = 8;
pub const SERVICE_PICKER_VISIBLE_ROWS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceDialogMode {
    Create,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceDialogPickerKind {
    Harness,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDialogPickerOption {
    pub value: String,
    pub label: String,
    pub detail: String,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceDialog {
    pub mode: ServiceDialogMode,
    pub service: ServiceSummary,
    pub selected_field: usize,
    pub note: Option<String>,
    pub new_token: Option<String>,
    pub confirm_delete: bool,
    pub picker: Option<ServiceDialogPickerKind>,
    pub picker_selected: usize,
    pub picker_scroll: usize,
}

impl ServiceDialog {
    pub fn field_value(&self, field: usize) -> String {
        match field {
            0 => self.service.name.clone(),
            1 => self.service.instruction.replace('\n', " ↵ "),
            2 => self.service.harness.clone(),
            3 => self.service.model.clone().unwrap_or_default(),
            4 => self.service.cwd.clone(),
            5 => self.service.routing.clone(),
            6 => self
                .service
                .http_port
                .map(|port| port.to_string())
                .unwrap_or_default(),
            7 => {
                if self.service.paused {
                    "paused".to_string()
                } else {
                    "serving".to_string()
                }
            }
            _ => String::new(),
        }
    }

    pub fn picker_options(&self, app: &App) -> Vec<ServiceDialogPickerOption> {
        match self.picker {
            Some(ServiceDialogPickerKind::Harness) => {
                let mut options: Vec<_> = app
                    .harnesses
                    .iter()
                    .map(|harness| ServiceDialogPickerOption {
                        value: harness.name.clone(),
                        label: harness.name.clone(),
                        detail: harness
                            .detail
                            .clone()
                            .or_else(|| harness.description.clone())
                            .unwrap_or_default(),
                        available: harness.available,
                    })
                    .collect();
                if !options
                    .iter()
                    .any(|option| option.value == self.service.harness)
                {
                    options.insert(
                        0,
                        ServiceDialogPickerOption {
                            value: self.service.harness.clone(),
                            label: self.service.harness.clone(),
                            detail: "current value; no daemon probe available".to_string(),
                            available: false,
                        },
                    );
                }
                options
            }
            Some(ServiceDialogPickerKind::Model) => {
                let mut options = vec![ServiceDialogPickerOption {
                    value: String::new(),
                    label: "Default".to_string(),
                    detail: "let the selected harness choose its default model".to_string(),
                    available: true,
                }];
                if !app.service_route_catalog.is_empty() {
                    for route in &app.service_route_catalog {
                        let models = if route.models.is_empty() {
                            vec![route.model.clone()]
                        } else {
                            route.models.clone()
                        };
                        for model in models {
                            if model.trim().is_empty() {
                                continue;
                            }
                            let detail = if let Some(reason) = route.unavailable_reason.as_deref() {
                                format!("{} · {reason}", route.name)
                            } else {
                                format!("route: {}", route.name)
                            };
                            options.push(ServiceDialogPickerOption {
                                value: model.clone(),
                                label: format!("{} / {model}", route.name),
                                detail,
                                available: route.unavailable_reason.is_none(),
                            });
                        }
                    }
                } else if let Some(harness) = app
                    .harnesses
                    .iter()
                    .find(|harness| harness.name == self.service.harness)
                {
                    options.extend(harness.capabilities.models.iter().map(|model| {
                        ServiceDialogPickerOption {
                            value: model.clone(),
                            label: model.clone(),
                            detail: "advertised by this harness".to_string(),
                            available: true,
                        }
                    }));
                }
                if let Some(current) = self.service.model.as_deref() {
                    if !options.iter().any(|option| option.value == current) {
                        options.push(ServiceDialogPickerOption {
                            value: current.to_string(),
                            label: current.to_string(),
                            detail: "current value; not advertised by this harness".to_string(),
                            available: false,
                        });
                    }
                }
                options
            }
            None => Vec::new(),
        }
    }
}

fn default_service(app: &App, suggested: String) -> ServiceSummary {
    let selected = app.selected_session();
    ServiceSummary {
        name: suggested,
        instruction: String::new(),
        harness: selected
            .map(|session| session.harness.clone())
            .unwrap_or_else(|| "smith".to_string()),
        model: selected.and_then(|session| session.model.clone()),
        cwd: selected
            .map(|session| session.cwd.clone())
            .unwrap_or_else(|| ".".to_string()),
        routing: "session-key".to_string(),
        paused: false,
        http_port: Some(8787),
        has_http_token: false,
    }
}

fn valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

impl App {
    pub async fn refresh_services(&mut self) {
        match self.client.list_services().await {
            Ok(mut services) => {
                services.sort_by(|a, b| a.name.cmp(&b.name));
                self.services = services;
            }
            Err(error) => self.set_status(format!("services refresh failed: {error}")),
        }
    }

    pub fn open_new_service_view(&mut self, suggested: impl Into<String>) {
        let suggested = suggested.into();
        self.configure_popup = None;
        self.session_picker = None;
        self.select_service(suggested.clone());
        self.service_dialog = Some(ServiceDialog {
            mode: ServiceDialogMode::Create,
            service: default_service(self, suggested),
            selected_field: 0,
            note: Some("Enter saves this service as its own TOML file.".to_string()),
            new_token: None,
            confirm_delete: false,
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
        });
    }

    pub fn open_edit_service_view(&mut self, name: &str) -> bool {
        let Some(service) = self
            .services
            .iter()
            .find(|service| service.name == name)
            .cloned()
        else {
            self.set_status(format!("service {name} not found"));
            return false;
        };
        self.configure_popup = None;
        self.session_picker = None;
        self.select_service(name.to_string());
        self.service_dialog = Some(ServiceDialog {
            mode: ServiceDialogMode::Edit,
            service,
            selected_field: 1,
            note: Some("Changes apply after the daemon restarts.".to_string()),
            new_token: None,
            confirm_delete: false,
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
        });
        true
    }

    fn open_service_picker(&mut self, kind: ServiceDialogPickerKind) {
        let Some(mut dialog) = self.service_dialog.clone() else {
            return;
        };
        dialog.picker = Some(kind);
        dialog.picker_scroll = 0;
        let options = dialog.picker_options(self);
        let current = match kind {
            ServiceDialogPickerKind::Harness => dialog.service.harness.as_str(),
            ServiceDialogPickerKind::Model => dialog.service.model.as_deref().unwrap_or(""),
        };
        dialog.picker_selected = options
            .iter()
            .position(|option| option.value == current)
            .unwrap_or(0);
        self.service_dialog = Some(dialog);
        self.ensure_service_picker_visible();
    }

    fn ensure_service_picker_visible(&mut self) {
        let Some(dialog) = self.service_dialog.as_mut() else {
            return;
        };
        if dialog.picker_selected < dialog.picker_scroll {
            dialog.picker_scroll = dialog.picker_selected;
        } else if dialog.picker_selected >= dialog.picker_scroll + SERVICE_PICKER_VISIBLE_ROWS {
            dialog.picker_scroll = dialog
                .picker_selected
                .saturating_sub(SERVICE_PICKER_VISIBLE_ROWS - 1);
        }
    }

    fn move_service_picker(&mut self, delta: isize) {
        let Some(snapshot) = self.service_dialog.clone() else {
            return;
        };
        if snapshot.picker.is_none() {
            return;
        }
        let options = snapshot.picker_options(self);
        if options.is_empty() {
            return;
        }
        if let Some(dialog) = self.service_dialog.as_mut() {
            dialog.picker_selected = (dialog.picker_selected as isize + delta)
                .rem_euclid(options.len() as isize) as usize;
        }
        self.ensure_service_picker_visible();
    }

    fn choose_service_picker(&mut self) {
        let Some(snapshot) = self.service_dialog.clone() else {
            return;
        };
        let Some(kind) = snapshot.picker else {
            return;
        };
        let options = snapshot.picker_options(self);
        let Some(option) = options.get(snapshot.picker_selected) else {
            return;
        };
        let value = option.value.clone();
        if let Some(dialog) = self.service_dialog.as_mut() {
            match kind {
                ServiceDialogPickerKind::Harness => dialog.service.harness = value,
                ServiceDialogPickerKind::Model => {
                    dialog.service.model = (!value.is_empty()).then_some(value)
                }
            }
            dialog.picker = None;
            dialog.picker_selected = 0;
            dialog.picker_scroll = 0;
            dialog.note = None;
            dialog.new_token = None;
            dialog.confirm_delete = false;
        }
    }

    fn edit_service_dialog_text(&mut self, mut edit: impl FnMut(&mut String)) {
        let Some(dialog) = self.service_dialog.as_mut() else {
            return;
        };
        if dialog.mode == ServiceDialogMode::Edit && dialog.selected_field == 0 {
            dialog.note = Some("Service names cannot be changed after creation.".to_string());
            return;
        }
        match dialog.selected_field {
            0 => edit(&mut dialog.service.name),
            1 => edit(&mut dialog.service.instruction),
            // Harness and model are catalog-backed fields. They deliberately
            // do not accept typed or pasted text: Enter opens the picker and
            // the picker is the only way to change either value.
            2 | 3 => return,
            4 => edit(&mut dialog.service.cwd),
            6 => {
                let mut value = dialog
                    .service
                    .http_port
                    .map(|port| port.to_string())
                    .unwrap_or_default();
                edit(&mut value);
                dialog.service.http_port = value.parse::<u16>().ok().filter(|port| *port > 0);
            }
            _ => {}
        }
        dialog.note = None;
        dialog.new_token = None;
        dialog.confirm_delete = false;
    }

    pub(super) fn insert_service_dialog_text(&mut self, text: &str) -> bool {
        if self.service_dialog.is_none() {
            return false;
        }
        let sanitized = text.replace(['\r', '\n'], " ");
        self.edit_service_dialog_text(|value| value.push_str(&sanitized));
        true
    }

    fn cycle_service_dialog_value(&mut self, reverse: bool) {
        let Some(dialog) = self.service_dialog.as_mut() else {
            return;
        };
        match dialog.selected_field {
            5 => {
                const ROUTING: [&str; 3] = ["session-key", "per-event", "single"];
                let current = ROUTING
                    .iter()
                    .position(|value| *value == dialog.service.routing)
                    .unwrap_or(0);
                let next = if reverse {
                    current.checked_sub(1).unwrap_or(ROUTING.len() - 1)
                } else {
                    (current + 1) % ROUTING.len()
                };
                dialog.service.routing = ROUTING[next].to_string();
            }
            7 => dialog.service.paused = !dialog.service.paused,
            _ => {}
        }
        dialog.note = None;
        dialog.confirm_delete = false;
    }

    async fn save_service_dialog(&mut self, rotate_token: bool) {
        let Some(dialog) = self.service_dialog.clone() else {
            return;
        };
        let service = dialog.service;
        let validation_error = if !valid_service_name(&service.name) {
            Some("Name must be 1–32 lowercase letters, digits, or interior hyphens.")
        } else if service.harness.trim().is_empty() {
            Some("Harness cannot be empty.")
        } else if service.cwd.trim().is_empty() {
            Some("Working directory cannot be empty.")
        } else if !matches!(
            service.routing.as_str(),
            "session-key" | "per-event" | "single"
        ) {
            Some("Routing must be session-key, per-event, or single.")
        } else if service.http_port.is_none() {
            Some("HTTP port must be between 1 and 65535.")
        } else {
            None
        };
        if let Some(message) = validation_error {
            if let Some(dialog) = self.service_dialog.as_mut() {
                dialog.note = Some(message.to_string());
            }
            return;
        }

        match self
            .client
            .put_service(construct_protocol::ServicePutParams {
                service,
                rotate_token,
            })
            .await
        {
            Ok(result) => {
                self.refresh_services().await;
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.mode = ServiceDialogMode::Edit;
                    dialog.service = result.service;
                    dialog.new_token = result.new_token;
                    dialog.confirm_delete = false;
                    dialog.note = Some(if dialog.new_token.is_some() {
                        "Saved. Copy the token now; it is shown only once. Restart to apply."
                            .to_string()
                    } else {
                        "Saved. Restart the daemon to apply changes.".to_string()
                    });
                }
            }
            Err(error) => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.note = Some(format!("Save failed: {error}"));
                }
            }
        }
    }

    async fn delete_service_dialog(&mut self) {
        let Some(name) = self
            .service_dialog
            .as_ref()
            .map(|dialog| dialog.service.name.clone())
        else {
            return;
        };
        match self.client.delete_service(name.clone()).await {
            Ok(()) => {
                let was_selected = self.selection.service_name() == Some(name.as_str());
                if self.main_windows.clear_service(&name) {
                    self.push_layout();
                }
                self.service_dialog = None;
                self.refresh_services().await;
                if was_selected {
                    self.selection = Selection::None;
                    self.ensure_selection_valid();
                    self.sync_active_window_selection();
                }
                self.set_status(format!(
                    "{name} deleted; restart daemon to withdraw the endpoint"
                ));
            }
            Err(error) => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.note = Some(format!("Delete failed: {error}"));
                    dialog.confirm_delete = false;
                }
            }
        }
    }

    pub(super) async fn handle_service_dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(snapshot) = self.service_dialog.clone() else {
            return false;
        };
        if snapshot.confirm_delete {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.delete_service_dialog().await,
                KeyCode::Esc | KeyCode::Char('n') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.confirm_delete = false;
                        dialog.note = Some("Delete cancelled.".to_string());
                    }
                }
                _ => {}
            }
            return true;
        }
        if snapshot.picker.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.picker = None;
                        dialog.picker_selected = 0;
                        dialog.picker_scroll = 0;
                    }
                }
                KeyCode::Enter => self.choose_service_picker(),
                KeyCode::Up => self.move_service_picker(-1),
                KeyCode::Down => self.move_service_picker(1),
                KeyCode::Char('p') if ctrl => self.move_service_picker(-1),
                KeyCode::Char('n') if ctrl => self.move_service_picker(1),
                KeyCode::Char('x') if ctrl => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.picker = None;
                        dialog.picker_selected = 0;
                        dialog.picker_scroll = 0;
                    }
                    return false;
                }
                _ => {}
            }
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_service_dialog(false).await;
                    return true;
                }
                KeyCode::Char('r') if snapshot.mode == ServiceDialogMode::Edit => {
                    self.save_service_dialog(true).await;
                    return true;
                }
                KeyCode::Char('d') if snapshot.mode == ServiceDialogMode::Edit => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.confirm_delete = true;
                        dialog.note = Some(
                            "Delete this service? Enter/y confirms; Esc/n cancels.".to_string(),
                        );
                    }
                    return true;
                }
                KeyCode::Char('x') => {
                    // Preserve global C-x chords by closing and falling back.
                    self.service_dialog = None;
                    return false;
                }
                KeyCode::Char('p') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.selected_field = dialog
                            .selected_field
                            .checked_sub(1)
                            .unwrap_or(FIELD_COUNT - 1);
                    }
                    return true;
                }
                KeyCode::Char('n') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.selected_field = (dialog.selected_field + 1) % FIELD_COUNT;
                    }
                    return true;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.service_dialog = None,
            KeyCode::Enter => match snapshot.selected_field {
                2 => self.open_service_picker(ServiceDialogPickerKind::Harness),
                3 => self.open_service_picker(ServiceDialogPickerKind::Model),
                _ => self.save_service_dialog(false).await,
            },
            KeyCode::Tab | KeyCode::Down => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.selected_field = (dialog.selected_field + 1) % FIELD_COUNT;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.selected_field = dialog
                        .selected_field
                        .checked_sub(1)
                        .unwrap_or(FIELD_COUNT - 1);
                }
            }
            KeyCode::Left => self.cycle_service_dialog_value(true),
            KeyCode::Right | KeyCode::Char(' ') if matches!(snapshot.selected_field, 5 | 7) => {
                self.cycle_service_dialog_value(false)
            }
            KeyCode::Backspace => self.edit_service_dialog_text(|value| {
                value.pop();
            }),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.edit_service_dialog_text(|value| value.push(ch));
            }
            _ => {}
        }
        true
    }
}
