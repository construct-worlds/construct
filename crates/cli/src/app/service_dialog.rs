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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceChannelDialogMode {
    Create,
    Edit,
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
    pub confirm_delete: bool,
    pub picker: Option<ServiceDialogPickerKind>,
    pub picker_selected: usize,
    pub picker_scroll: usize,
    pub selected_channel: usize,
    pub channel_editor: Option<ServiceChannelDialog>,
}

#[derive(Debug, Clone)]
pub struct ServiceChannelDialog {
    pub mode: ServiceChannelDialogMode,
    pub service_name: String,
    pub channel: ServiceChannelSummary,
    pub selected_field: usize,
    pub note: Option<String>,
    pub new_secret: Option<String>,
    pub confirm_delete: bool,
    pub app_token: String,
    pub bot_token: String,
}

/// Address-level actions exposed by a channel publication. Keeping this typed
/// prevents the TUI from assuming every future ingress protocol produces a
/// browser URL: URLs can be opened and copied, while socket endpoints can only
/// be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceChannelActionAddress {
    AuthorizationUrl(String),
    PublicUrl(String),
    PublicSocket(String),
}

impl ServiceChannelActionAddress {
    pub fn value(&self) -> &str {
        match self {
            Self::AuthorizationUrl(value) | Self::PublicUrl(value) | Self::PublicSocket(value) => {
                value
            }
        }
    }

    pub fn can_open(&self) -> bool {
        matches!(self, Self::AuthorizationUrl(_) | Self::PublicUrl(_))
    }

    fn noun(&self) -> &'static str {
        match self {
            Self::AuthorizationUrl(_) => "authorization URL",
            Self::PublicUrl(_) => "public URL",
            Self::PublicSocket(_) => "public endpoint",
        }
    }
}

/// The complete action surface for one attached ingress channel. Renderers
/// consume this model instead of branching on channel kinds or publication
/// protocol details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceChannelActions {
    pub service_name: String,
    pub channel_index: usize,
    pub channel_id: String,
    pub published: bool,
    pub address: Option<ServiceChannelActionAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceChannelAction {
    TogglePublication,
    OpenAddress,
    CopyAddress,
}

fn channel_field_count(editor: &ServiceChannelDialog) -> usize {
    if editor.channel.kind == "slack" {
        7
    } else {
        4
    }
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
            6 => format!("{} attached", self.service.channels.len()),
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
        channels: Vec::new(),
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
        match self.client.list_service_channel_catalog().await {
            Ok(mut channels) => {
                channels.sort_by(|a, b| a.id.cmp(&b.id));
                self.service_channel_catalog = channels;
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.selected_channel = dialog
                        .selected_channel
                        .min(self.service_channel_catalog.len().saturating_sub(1));
                }
            }
            Err(error) => self.set_status(format!("channel catalog refresh failed: {error}")),
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
            confirm_delete: false,
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
            selected_channel: 0,
            channel_editor: None,
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
            note: Some("Saved edits apply live — see each field for when.".to_string()),
            confirm_delete: false,
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
            selected_channel: 0,
            channel_editor: None,
        });
        true
    }

    fn suggested_channel_id(&self, _service: &ServiceSummary) -> String {
        if !self
            .service_channel_catalog
            .iter()
            .any(|channel| channel.id == "http")
        {
            return "http".to_string();
        }
        (2..=99)
            .map(|index| format!("http-{index}"))
            .find(|id| {
                !self
                    .service_channel_catalog
                    .iter()
                    .any(|channel| channel.id == *id)
            })
            .unwrap_or_else(|| format!("http-{}", self.service_channel_catalog.len() + 1))
    }

    fn suggested_channel_port(&self, service: &ServiceSummary) -> u16 {
        let mut used: std::collections::HashSet<u16> = service
            .channels
            .iter()
            .filter_map(|channel| channel.port)
            .collect();
        used.extend(
            self.service_channel_catalog
                .iter()
                .filter_map(|channel| channel.port),
        );
        (8787..=u16::MAX)
            .find(|port| !used.contains(port))
            .unwrap_or(8787)
    }

    pub fn open_new_service_channel(&mut self) -> bool {
        let Some(service) = self
            .service_dialog
            .as_ref()
            .map(|dialog| dialog.service.clone())
        else {
            return false;
        };
        let id = self.suggested_channel_id(&service);
        let port = self.suggested_channel_port(&service);
        let Some(dialog) = self.service_dialog.as_mut() else {
            return false;
        };
        dialog.selected_field = 6;
        dialog.channel_editor = Some(ServiceChannelDialog {
            mode: ServiceChannelDialogMode::Create,
            service_name: dialog.service.name.clone(),
            channel: ServiceChannelSummary {
                id,
                kind: "http".to_string(),
                enabled: true,
                port: Some(port),
                has_credential: false,
                has_app_token: false,
                has_bot_token: false,
                allowed_workspace_count: 0,
                allowed_channel_count: 0,
                allowed_workspaces: Vec::new(),
                allowed_channels: Vec::new(),
                attached_to: Some(dialog.service.name.clone()),
                publication: None,
            },
            selected_field: 0,
            note: Some("HTTP channels bind on loopback as soon as they are saved.".to_string()),
            new_secret: None,
            confirm_delete: false,
            app_token: String::new(),
            bot_token: String::new(),
        });
        true
    }

    pub fn open_edit_service_channel(&mut self, index: usize) -> bool {
        let Some(dialog) = self.service_dialog.as_mut() else {
            return false;
        };
        let Some(channel) = self.service_channel_catalog.get(index).cloned() else {
            return false;
        };
        if channel.attached_to.as_deref() != Some(dialog.service.name.as_str()) {
            return false;
        }
        dialog.selected_field = 6;
        dialog.selected_channel = index;
        dialog.channel_editor = Some(ServiceChannelDialog {
            mode: ServiceChannelDialogMode::Edit,
            service_name: dialog.service.name.clone(),
            channel,
            selected_field: 2,
            note: Some("Channel changes bind or unbind the listener immediately.".to_string()),
            new_secret: None,
            confirm_delete: false,
            app_token: String::new(),
            bot_token: String::new(),
        });
        true
    }

    async fn toggle_service_channel(&mut self, index: usize) {
        let Some(dialog) = self.service_dialog.as_ref() else {
            return;
        };
        let service_name = dialog.service.name.clone();
        let Some(channel) = self.service_channel_catalog.get(index).cloned() else {
            return;
        };
        let operation = match channel.attached_to.as_deref() {
            None => Some((true, channel.id.clone())),
            Some(owner) if owner == service_name => Some((false, channel.id.clone())),
            Some(owner) => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.note = Some(format!(
                        "Channel `{}` is already attached to service `{owner}`.",
                        channel.id
                    ));
                }
                None
            }
        };
        let Some((attach, channel_id)) = operation else {
            return;
        };
        let result = if attach {
            self.client
                .attach_service_channel(&service_name, &channel_id)
                .await
        } else {
            self.client
                .detach_service_channel(&service_name, &channel_id)
                .await
        };
        match result {
            Ok(result) => {
                self.refresh_services().await;
                if let Some(dialog) = self.service_dialog.as_mut() {
                    if let Some(service) = self
                        .services
                        .iter()
                        .find(|service| service.name == service_name)
                        .cloned()
                    {
                        dialog.service = service;
                    }
                    dialog.note = Some(format!(
                        "Channel `{channel_id}` {}: {}.",
                        if attach { "attached" } else { "detached" },
                        result.applied.summary()
                    ));
                }
            }
            Err(error) => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.note = Some(format!(
                        "Channel `{channel_id}` {} failed: {error}",
                        if attach { "attach" } else { "detach" }
                    ));
                }
            }
        }
    }

    pub(super) fn apply_channel_publication(
        &mut self,
        payload: construct_protocol::ChannelPublicationNotificationPayload,
    ) {
        for channel in &mut self.service_channel_catalog {
            if channel.id == payload.channel_id
                && channel.attached_to.as_deref() == Some(payload.service_name.as_str())
            {
                channel.publication = payload.publication.clone();
            }
        }
        for service in &mut self.services {
            if service.name != payload.service_name {
                continue;
            }
            for channel in &mut service.channels {
                if channel.id == payload.channel_id {
                    channel.publication = payload.publication.clone();
                }
            }
        }
        if let Some(dialog) = self.service_dialog.as_mut() {
            if dialog.service.name == payload.service_name {
                for channel in &mut dialog.service.channels {
                    if channel.id == payload.channel_id {
                        channel.publication = payload.publication.clone();
                    }
                }
                if let Some(editor) = dialog.channel_editor.as_mut() {
                    if editor.channel.id == payload.channel_id {
                        editor.channel.publication = payload.publication;
                    }
                }
            }
        }
    }

    async fn toggle_channel_publication(&mut self, service_name: &str, index: usize) {
        let service_name = service_name.to_string();
        let Some(channel) = self.service_channel_catalog.get(index).cloned() else {
            return;
        };
        if channel.attached_to.as_deref() != Some(service_name.as_str()) {
            if let Some(dialog) = self.service_dialog.as_mut() {
                dialog.note = Some("Attach the channel before publishing it.".to_string());
            }
            return;
        }

        if channel.publication.is_some() {
            match self
                .client
                .unpublish_service_channel(&service_name, &channel.id)
                .await
            {
                Ok(_) => self.apply_channel_publication(
                    construct_protocol::ChannelPublicationNotificationPayload {
                        service_name,
                        channel_id: channel.id,
                        publication: None,
                    },
                ),
                Err(error) => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.note = Some(format!("Unpublish failed: {error}"));
                    }
                }
            }
        } else {
            match self
                .client
                .publish_service_channel(&service_name, &channel.id, "construct")
                .await
            {
                Ok(publication) => self.apply_channel_publication(
                    construct_protocol::ChannelPublicationNotificationPayload {
                        service_name,
                        channel_id: channel.id,
                        publication: Some(publication),
                    },
                ),
                Err(error) => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.note = Some(format!("Publish failed: {error}"));
                    }
                }
            }
        }
    }

    /// Build the protocol-neutral action model for an attached ingress
    /// channel. A local port is the client-visible ingress capability today;
    /// an existing publication remains withdrawable even if its listener has
    /// disappeared while the notification is in flight.
    pub fn service_channel_actions(
        &self,
        service_name: &str,
        index: usize,
    ) -> Option<ServiceChannelActions> {
        let channel = self.service_channel_catalog.get(index)?;
        if channel.attached_to.as_deref() != Some(service_name)
            || (channel.port.is_none() && channel.publication.is_none())
        {
            return None;
        }

        let address = channel.publication.as_ref().and_then(|publication| {
            use construct_protocol::{ChannelPublicEndpoint as Endpoint, ChannelPublicationPhase};

            let public = publication
                .public_endpoint
                .as_ref()
                .map(|endpoint| match endpoint {
                    Endpoint::Url { url } => ServiceChannelActionAddress::PublicUrl(url.clone()),
                    Endpoint::Socket { .. } => {
                        ServiceChannelActionAddress::PublicSocket(endpoint.to_string())
                    }
                });
            let authorization = publication
                .auth_url
                .as_ref()
                .map(|url| ServiceChannelActionAddress::AuthorizationUrl(url.clone()));
            match publication.phase {
                ChannelPublicationPhase::Authorizing => authorization.or(public),
                ChannelPublicationPhase::Ready => public.or(authorization),
                ChannelPublicationPhase::Connecting | ChannelPublicationPhase::Error => {
                    public.or(authorization)
                }
            }
        });

        Some(ServiceChannelActions {
            service_name: service_name.to_string(),
            channel_index: index,
            channel_id: channel.id.clone(),
            published: channel.publication.is_some(),
            address,
        })
    }

    pub fn selected_service_channel_actions(
        &self,
        service_name: &str,
    ) -> Option<ServiceChannelActions> {
        let dialog = self
            .service_dialog
            .as_ref()
            .filter(|dialog| dialog.service.name == service_name && dialog.selected_field == 6)?;
        self.service_channel_actions(service_name, dialog.selected_channel)
    }

    pub(super) async fn run_service_channel_action(
        &mut self,
        service_name: &str,
        index: usize,
        action: ServiceChannelAction,
    ) {
        let Some(actions) = self.service_channel_actions(service_name, index) else {
            return;
        };
        match action {
            ServiceChannelAction::TogglePublication => {
                self.toggle_channel_publication(service_name, index).await;
            }
            ServiceChannelAction::OpenAddress => {
                let Some(address) = actions.address.filter(|address| address.can_open()) else {
                    return;
                };
                match open_url(address.value()) {
                    Ok(()) => self.set_status(format!("opened {}", address.noun())),
                    Err(error) => {
                        self.set_status(format!("open {} failed: {error}", address.noun()))
                    }
                }
            }
            ServiceChannelAction::CopyAddress => {
                let Some(address) = actions.address else {
                    return;
                };
                match copy_to_clipboard(address.value()) {
                    Ok(outcome) => self.set_status(format!(
                        "{} · {}",
                        address.noun(),
                        outcome.status(address.value().chars().count())
                    )),
                    Err(error) => {
                        self.set_status(format!("copy {} failed: {error}", address.noun()))
                    }
                }
            }
        }
    }

    fn edit_service_channel_text(&mut self, mut edit: impl FnMut(&mut String)) {
        let Some(dialog) = self.service_dialog.as_mut() else {
            return;
        };
        let Some(channel) = dialog.channel_editor.as_mut() else {
            return;
        };
        match channel.selected_field {
            0 if channel.mode == ServiceChannelDialogMode::Create => edit(&mut channel.channel.id),
            2 if channel.channel.kind == "http" => {
                let mut value = channel
                    .channel
                    .port
                    .map(|port| port.to_string())
                    .unwrap_or_default();
                edit(&mut value);
                channel.channel.port = value.parse::<u16>().ok().filter(|port| *port > 0);
            }
            2 if channel.channel.kind == "slack" => edit(&mut channel.app_token),
            3 if channel.channel.kind == "slack" => edit(&mut channel.bot_token),
            4 if channel.channel.kind == "slack" => {
                let mut value = channel.channel.allowed_workspaces.join(",");
                edit(&mut value);
                channel.channel.allowed_workspaces = split_allowlist(&value);
                channel.channel.allowed_workspace_count = channel.channel.allowed_workspaces.len();
            }
            5 if channel.channel.kind == "slack" => {
                let mut value = channel.channel.allowed_channels.join(",");
                edit(&mut value);
                channel.channel.allowed_channels = split_allowlist(&value);
                channel.channel.allowed_channel_count = channel.channel.allowed_channels.len();
            }
            _ => return,
        }
        channel.note = None;
        channel.new_secret = None;
        channel.confirm_delete = false;
    }

    async fn save_service_channel(&mut self, rotate_secret: bool) {
        let Some(parent) = self.service_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let editor_snapshot = editor.clone();
        let valid_id = valid_service_name(&editor_snapshot.channel.id);
        let validation_error = if !valid_id {
            Some("Channel ID must be 1–32 lowercase letters, digits, or interior hyphens.")
        } else if editor_snapshot.channel.kind == "http" && editor_snapshot.channel.port.is_none() {
            Some("HTTP port must be between 1 and 65535.")
        } else if editor_snapshot.channel.kind == "slack"
            && editor_snapshot.mode == ServiceChannelDialogMode::Create
            && !editor_snapshot.app_token.starts_with("xapp-")
        {
            Some("Slack app token must start with xapp-.")
        } else if editor_snapshot.channel.kind == "slack"
            && editor_snapshot.mode == ServiceChannelDialogMode::Create
            && !editor_snapshot.bot_token.starts_with("xoxb-")
        {
            Some("Slack bot token must start with xoxb-.")
        } else {
            None
        };
        if let Some(message) = validation_error {
            if let Some(parent) = self.service_dialog.as_mut() {
                if let Some(editor) = parent.channel_editor.as_mut() {
                    editor.note = Some(message.to_string());
                }
            }
            return;
        }
        match self
            .client
            .put_service_channel(construct_protocol::ServiceChannelPutParams {
                service_name: editor_snapshot.service_name.clone(),
                channel: construct_protocol::ServiceChannelPut {
                    id: editor_snapshot.channel.id,
                    kind: editor_snapshot.channel.kind,
                    enabled: editor_snapshot.channel.enabled,
                    port: editor_snapshot.channel.port,
                    app_token: (!editor_snapshot.app_token.is_empty())
                        .then_some(editor_snapshot.app_token),
                    bot_token: (!editor_snapshot.bot_token.is_empty())
                        .then_some(editor_snapshot.bot_token),
                    allowed_workspaces: editor_snapshot.channel.allowed_workspaces,
                    allowed_channels: editor_snapshot.channel.allowed_channels,
                },
                rotate_secret,
            })
            .await
        {
            Ok(result) => {
                self.refresh_services().await;
                let refreshed_service = self
                    .services
                    .iter()
                    .find(|service| service.name == editor_snapshot.service_name)
                    .cloned();
                let applied = result.applied.summary();
                let new_secret = result.new_secret;
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(service) = refreshed_service {
                        parent.service = service;
                    }
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.mode = ServiceChannelDialogMode::Edit;
                        editor.channel = result.channel;
                        editor.app_token.clear();
                        editor.bot_token.clear();
                        let has_new_secret = new_secret.is_some();
                        editor.new_secret = new_secret;
                        editor.confirm_delete = false;
                        editor.note = Some(if has_new_secret {
                            format!("{applied}. Copy the credential now; it is shown only once.")
                        } else {
                            applied
                        });
                    }
                }
            }
            Err(error) => {
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Channel save failed: {error}"));
                    }
                }
            }
        }
    }

    async fn delete_service_channel(&mut self) {
        let Some(parent) = self.service_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let service_name = editor.service_name.clone();
        let channel_id = editor.channel.id.clone();
        match self
            .client
            .delete_service_channel(&service_name, &channel_id)
            .await
        {
            Ok(()) => {
                self.refresh_services().await;
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(service) = self
                        .services
                        .iter()
                        .find(|service| service.name == service_name)
                        .cloned()
                    {
                        parent.service = service;
                    }
                    parent.channel_editor = None;
                    parent.note = Some(format!("Channel `{channel_id}` deleted and withdrawn."));
                }
            }
            Err(error) => {
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Channel delete failed: {error}"));
                        editor.confirm_delete = false;
                    }
                }
            }
        }
    }

    async fn rotate_service_channel_secret(&mut self) {
        let Some(parent) = self.service_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let service_name = editor.service_name.clone();
        let channel_id = editor.channel.id.clone();
        match self
            .client
            .rotate_service_channel_secret(&service_name, &channel_id)
            .await
        {
            Ok(result) => {
                self.refresh_services().await;
                let applied = result.applied.summary();
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(service) = self
                        .services
                        .iter()
                        .find(|service| service.name == service_name)
                        .cloned()
                    {
                        parent.service = service;
                    }
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.channel = result.channel;
                        editor.new_secret = result.new_secret;
                        editor.note = Some(format!(
                            "Credential rotated. Copy it now; it is shown only once. {}.",
                            applied
                        ));
                    }
                }
            }
            Err(error) => {
                if let Some(parent) = self.service_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Credential rotation failed: {error}"));
                    }
                }
            }
        }
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
            6 => return,
            _ => {}
        }
        dialog.note = None;
        dialog.confirm_delete = false;
    }

    pub(super) fn insert_service_dialog_text(&mut self, text: &str) -> bool {
        if self.service_dialog.is_none() {
            return false;
        }
        let sanitized = text.replace(['\r', '\n'], " ");
        if self
            .service_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.channel_editor.is_some())
        {
            self.edit_service_channel_text(|value| value.push_str(&sanitized));
        } else {
            self.edit_service_dialog_text(|value| value.push_str(&sanitized));
        }
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

    async fn save_service_dialog(&mut self) {
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
            .put_service(construct_protocol::ServicePutParams { service })
            .await
        {
            Ok(result) => {
                self.refresh_services().await;
                let applied = result.applied.summary();
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.mode = ServiceDialogMode::Edit;
                    dialog.service = result.service;
                    dialog.confirm_delete = false;
                    dialog.note = Some(applied);
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
                self.set_status(format!("{name} deleted; its endpoint is withdrawn"));
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
        if snapshot.channel_editor.is_some() {
            return self.handle_service_channel_dialog_key(key).await;
        }
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
                    self.save_service_dialog().await;
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
                        if dialog.selected_field == 6 && !self.service_channel_catalog.is_empty() {
                            dialog.selected_channel = dialog
                                .selected_channel
                                .checked_sub(1)
                                .unwrap_or(self.service_channel_catalog.len() - 1);
                        } else {
                            dialog.selected_field = dialog
                                .selected_field
                                .checked_sub(1)
                                .unwrap_or(FIELD_COUNT - 1);
                        }
                    }
                    return true;
                }
                KeyCode::Char('n') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if dialog.selected_field == 6 && !self.service_channel_catalog.is_empty() {
                            dialog.selected_channel =
                                (dialog.selected_channel + 1) % self.service_channel_catalog.len();
                        } else {
                            dialog.selected_field = (dialog.selected_field + 1) % FIELD_COUNT;
                        }
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
                6 => {
                    if self.service_channel_catalog.is_empty() {
                        self.open_new_service_channel();
                    } else if self
                        .service_channel_catalog
                        .get(snapshot.selected_channel)
                        .is_some_and(|channel| {
                            channel.attached_to.as_deref() == Some(snapshot.service.name.as_str())
                        })
                    {
                        self.open_edit_service_channel(snapshot.selected_channel);
                    } else if let Some(channel) =
                        self.service_channel_catalog.get(snapshot.selected_channel)
                    {
                        let message = match channel.attached_to.as_deref() {
                            Some(owner) => format!(
                                "Channel `{}` is attached to `{owner}`; press Space on an available channel.",
                                channel.id
                            ),
                            None => format!(
                                "Channel `{}` is available; press Space to attach it.",
                                channel.id
                            ),
                        };
                        if let Some(dialog) = self.service_dialog.as_mut() {
                            dialog.note = Some(message);
                        }
                    }
                }
                _ => self.save_service_dialog().await,
            },
            KeyCode::Char(' ') if snapshot.selected_field == 6 => {
                self.toggle_service_channel(snapshot.selected_channel).await;
            }
            KeyCode::Char('a') if snapshot.selected_field == 6 => {
                self.open_new_service_channel();
            }
            KeyCode::Char('e') if snapshot.selected_field == 6 => {
                self.open_edit_service_channel(snapshot.selected_channel);
            }
            KeyCode::Char('d') if snapshot.selected_field == 6 => {
                if self.open_edit_service_channel(snapshot.selected_channel) {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if let Some(editor) = dialog.channel_editor.as_mut() {
                            editor.confirm_delete = true;
                            editor.note = Some(
                                "Delete this channel? Enter/y confirms; Esc/n cancels.".to_string(),
                            );
                        }
                    }
                }
            }
            KeyCode::Char('r') if snapshot.selected_field == 6 => {
                if self.open_edit_service_channel(snapshot.selected_channel) {
                    self.rotate_service_channel_secret().await;
                }
            }
            KeyCode::Char('p') if snapshot.selected_field == 6 => {
                self.run_service_channel_action(
                    &snapshot.service.name,
                    snapshot.selected_channel,
                    ServiceChannelAction::TogglePublication,
                )
                .await;
            }
            KeyCode::Char('o') if snapshot.selected_field == 6 => {
                self.run_service_channel_action(
                    &snapshot.service.name,
                    snapshot.selected_channel,
                    ServiceChannelAction::OpenAddress,
                )
                .await;
            }
            KeyCode::Char('y') if snapshot.selected_field == 6 => {
                self.run_service_channel_action(
                    &snapshot.service.name,
                    snapshot.selected_channel,
                    ServiceChannelAction::CopyAddress,
                )
                .await;
            }
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

    async fn handle_service_channel_dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(snapshot) = self
            .service_dialog
            .as_ref()
            .and_then(|dialog| dialog.channel_editor.clone())
        else {
            return false;
        };
        if snapshot.confirm_delete {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.delete_service_channel().await,
                KeyCode::Esc | KeyCode::Char('n') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if let Some(editor) = dialog.channel_editor.as_mut() {
                            editor.confirm_delete = false;
                            editor.note = Some("Delete cancelled.".to_string());
                        }
                    }
                }
                _ => {}
            }
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_service_channel(false).await;
                    return true;
                }
                KeyCode::Char('r')
                    if snapshot.mode == ServiceChannelDialogMode::Edit
                        && snapshot.channel.kind == "http" =>
                {
                    self.rotate_service_channel_secret().await;
                    return true;
                }
                KeyCode::Char('d') if snapshot.mode == ServiceChannelDialogMode::Edit => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if let Some(editor) = dialog.channel_editor.as_mut() {
                            editor.confirm_delete = true;
                            editor.note = Some(
                                "Delete this channel? Enter/y confirms; Esc/n cancels.".to_string(),
                            );
                        }
                    }
                    return true;
                }
                KeyCode::Char('x') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        dialog.channel_editor = None;
                    }
                    return false;
                }
                KeyCode::Char('p') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if let Some(editor) = dialog.channel_editor.as_mut() {
                            editor.selected_field = editor
                                .selected_field
                                .checked_sub(1)
                                .unwrap_or(channel_field_count(editor) - 1);
                        }
                    }
                    return true;
                }
                KeyCode::Char('n') => {
                    if let Some(dialog) = self.service_dialog.as_mut() {
                        if let Some(editor) = dialog.channel_editor.as_mut() {
                            editor.selected_field =
                                (editor.selected_field + 1) % channel_field_count(editor);
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    dialog.channel_editor = None;
                }
            }
            KeyCode::Enter => self.save_service_channel(false).await,
            KeyCode::Tab | KeyCode::Down => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.selected_field =
                            (editor.selected_field + 1) % channel_field_count(editor);
                    }
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.selected_field = editor
                            .selected_field
                            .checked_sub(1)
                            .unwrap_or(channel_field_count(editor) - 1);
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if snapshot.selected_field == 1
                    && snapshot.mode == ServiceChannelDialogMode::Create =>
            {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.channel.kind = if editor.channel.kind == "http" {
                            "slack"
                        } else {
                            "http"
                        }
                        .to_string();
                        editor.channel.port = (editor.channel.kind == "http").then_some(8787);
                        editor.selected_field = 1;
                        editor.note = None;
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if (snapshot.channel.kind == "http" && snapshot.selected_field == 3)
                    || (snapshot.channel.kind == "slack" && snapshot.selected_field == 6) =>
            {
                if let Some(dialog) = self.service_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.channel.enabled = !editor.channel.enabled;
                    }
                }
            }
            KeyCode::Backspace => self.edit_service_channel_text(|value| {
                value.pop();
            }),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.edit_service_channel_text(|value| value.push(ch));
            }
            _ => {}
        }
        true
    }
}

fn split_allowlist(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}
