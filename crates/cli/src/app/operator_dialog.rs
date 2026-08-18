//! Inline operator-view editor and operator definition lifecycle.

use super::*;

/// Definition rows in the operator view: Name, Instruction, Harness, Model,
/// Session mode, Working dir, Routing, State. Channels and routed sessions are not fields —
/// they are their own navigable sections below the definition (spec 0175).
pub const FIELD_COUNT: usize = 8;
/// Re-exported name for consumers outside this module (renderers, hit tests).
pub const OPERATOR_FIELD_COUNT: usize = FIELD_COUNT;
pub const OPERATOR_PICKER_VISIBLE_ROWS: usize = 5;

/// Where the operator editor's cursor sits. The view is one continuous list —
/// definition fields, then the channel catalog, then the routed sessions — so
/// the selection is modelled explicitly instead of overloading a field index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorDialogFocus {
    Field(usize),
    Channel(usize),
    Session(usize),
}

impl OperatorDialogFocus {
    pub fn field(self) -> Option<usize> {
        match self {
            Self::Field(index) => Some(index),
            _ => None,
        }
    }

    pub fn channel(self) -> Option<usize> {
        match self {
            Self::Channel(index) => Some(index),
            _ => None,
        }
    }

    pub fn session(self) -> Option<usize> {
        match self {
            Self::Session(index) => Some(index),
            _ => None,
        }
    }

    pub fn is_field(self, index: usize) -> bool {
        self.field() == Some(index)
    }

    /// Pull a stale row selection back in range after the catalog or the
    /// routed-session list changed underneath it.
    fn clamp(self, channels: usize, sessions: usize) -> Self {
        match self {
            Self::Field(index) if index >= FIELD_COUNT => Self::Field(FIELD_COUNT - 1),
            Self::Channel(index) if index >= channels => {
                if channels == 0 {
                    Self::Field(FIELD_COUNT - 1)
                } else {
                    Self::Channel(channels - 1)
                }
            }
            Self::Session(index) if index >= sessions => {
                if sessions == 0 {
                    Self::Field(FIELD_COUNT - 1)
                } else {
                    Self::Session(sessions - 1)
                }
            }
            other => other,
        }
    }

    /// Down/Tab/`C-n`: definition fields → channel rows → session rows → wrap.
    pub fn next(self, channels: usize, sessions: usize) -> Self {
        match self.clamp(channels, sessions) {
            Self::Field(index) if index + 1 < FIELD_COUNT => Self::Field(index + 1),
            Self::Field(_) => Self::first_channel(channels, sessions),
            Self::Channel(index) if index + 1 < channels => Self::Channel(index + 1),
            Self::Channel(_) => Self::first_session(sessions),
            Self::Session(index) if index + 1 < sessions => Self::Session(index + 1),
            Self::Session(_) => Self::Field(0),
        }
    }

    /// Up/BackTab/`C-p`: the exact mirror of [`Self::next`].
    pub fn prev(self, channels: usize, sessions: usize) -> Self {
        match self.clamp(channels, sessions) {
            Self::Field(0) => {
                if sessions > 0 {
                    Self::Session(sessions - 1)
                } else if channels > 0 {
                    Self::Channel(channels - 1)
                } else {
                    Self::Field(FIELD_COUNT - 1)
                }
            }
            Self::Field(index) => Self::Field(index - 1),
            Self::Channel(0) => Self::Field(FIELD_COUNT - 1),
            Self::Channel(index) => Self::Channel(index - 1),
            Self::Session(0) => {
                if channels > 0 {
                    Self::Channel(channels - 1)
                } else {
                    Self::Field(FIELD_COUNT - 1)
                }
            }
            Self::Session(index) => Self::Session(index - 1),
        }
    }

    fn first_channel(channels: usize, sessions: usize) -> Self {
        if channels > 0 {
            Self::Channel(0)
        } else {
            Self::first_session(sessions)
        }
    }

    fn first_session(sessions: usize) -> Self {
        if sessions > 0 {
            Self::Session(0)
        } else {
            Self::Field(0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorDialogMode {
    Create,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorDialogPickerKind {
    Harness,
    Model,
    SessionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorChannelDialogMode {
    Create,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorDialogPickerOption {
    pub value: String,
    pub label: String,
    pub detail: String,
    pub available: bool,
}

#[derive(Debug, Clone)]
pub struct OperatorDialog {
    pub mode: OperatorDialogMode,
    pub operator: OperatorSummary,
    /// The definition as the daemon last confirmed it. Compared against
    /// `operator` to decide whether the pane title carries the unsaved marker
    /// and whether Esc has edits to revert.
    pub saved: OperatorSummary,
    pub focus: OperatorDialogFocus,
    pub note: Option<String>,
    pub picker: Option<OperatorDialogPickerKind>,
    pub picker_selected: usize,
    pub picker_scroll: usize,
    pub channel_editor: Option<OperatorChannelDialog>,
}

#[derive(Debug, Clone)]
pub struct OperatorChannelDialog {
    pub mode: OperatorChannelDialogMode,
    pub operator_name: String,
    pub channel: OperatorChannelSummary,
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
pub enum OperatorChannelActionAddress {
    AuthorizationUrl(String),
    PublicUrl(String),
    PublicSocket(String),
}

impl OperatorChannelActionAddress {
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
pub struct OperatorChannelActions {
    pub operator_name: String,
    pub channel_index: usize,
    pub channel_id: String,
    pub published: bool,
    pub address: Option<OperatorChannelActionAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorChannelAction {
    TogglePublication,
    OpenAddress,
    CopyAddress,
}

/// Field indexes shared by the Slack channel editor's navigation, rendering,
/// and key handling. State stays last so it sits where an HTTP channel's does.
pub(crate) const SLACK_FIELD_PROGRESS: usize = 6;
pub(crate) const SLACK_FIELD_FOLLOW_UP: usize = 7;
pub(crate) const SLACK_FIELD_THREAD_CONTEXT: usize = 8;
pub(crate) const SLACK_FIELD_STATE: usize = 9;
/// The slack-personal editor's field indexes, after ID (0), Kind (1),
/// MCP command (2), Workspaces (3), and Channels (4).
pub(crate) const PERSONAL_FIELD_TRIGGER: usize = 5;
pub(crate) const PERSONAL_FIELD_RESPONSE: usize = 6;
pub(crate) const PERSONAL_FIELD_DISCLOSURE: usize = 7;
pub(crate) const PERSONAL_FIELD_POLL: usize = 8;
pub(crate) const PERSONAL_FIELD_THREAD_CONTEXT: usize = 9;
pub(crate) const PERSONAL_FIELD_STATE: usize = 10;
const HTTP_FIELD_STATE: usize = 3;

fn channel_field_count(editor: &OperatorChannelDialog) -> usize {
    channel_state_field(&editor.channel.kind) + 1
}

/// The two Slack kinds share the allowlist fields but at different indexes,
/// because the bot kind spends 2–3 on its tokens and slack-personal spends 2
/// on its MCP command.
fn workspace_field(kind: &str) -> usize {
    if kind == "slack" {
        4
    } else {
        3
    }
}

fn channel_allowlist_field(kind: &str) -> usize {
    if kind == "slack" {
        5
    } else {
        4
    }
}

fn thread_context_field(kind: &str) -> usize {
    if kind == "slack" {
        SLACK_FIELD_THREAD_CONTEXT
    } else {
        PERSONAL_FIELD_THREAD_CONTEXT
    }
}

fn channel_state_field(kind: &str) -> usize {
    match kind {
        "slack" => SLACK_FIELD_STATE,
        "slack-personal" => PERSONAL_FIELD_STATE,
        _ => HTTP_FIELD_STATE,
    }
}

/// Step through a fixed list of option values, wrapping in both directions.
/// The list is the protocol's, so the editor can only offer what the daemon
/// will accept.
fn cycle_option(values: &[&str], current: Option<&str>, forward: bool) -> String {
    let index = current
        .and_then(|current| values.iter().position(|value| *value == current))
        .unwrap_or(0);
    let next = if forward {
        (index + 1) % values.len()
    } else {
        index.checked_sub(1).unwrap_or(values.len() - 1)
    };
    values[next].to_string()
}

fn canonical_operator_model(model: &str) -> String {
    construct_protocol::published_model::decode_published_model_id(model)
        .ok()
        .flatten()
        .map(|(route, model)| {
            construct_protocol::published_model::published_model_id(&route, &model)
        })
        .unwrap_or_else(|| model.to_string())
}

/// Compare the user-editable half of two definitions. Channels are
/// attached and detached straight against the daemon, so they are deliberately
/// excluded: toggling one must never leave the editor looking dirty.
fn same_editable_definition(left: &OperatorSummary, right: &OperatorSummary) -> bool {
    left.name == right.name
        && left.instruction == right.instruction
        && left.harness == right.harness
        && left.model == right.model
        && left.session_mode == right.session_mode
        && left.cwd == right.cwd
        && left.routing == right.routing
        && left.paused == right.paused
}

impl OperatorDialog {
    /// Open the editor for a definition the daemon already knows about.
    pub fn editing(operator: OperatorSummary) -> Self {
        Self {
            mode: OperatorDialogMode::Edit,
            saved: operator.clone(),
            operator,
            focus: OperatorDialogFocus::Field(1),
            note: Some("Saved edits apply live — see each field for when.".to_string()),
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
            channel_editor: None,
        }
    }

    /// A operator the user has changed since it was last saved. A operator
    /// being created has never been saved, so it is unsaved by definition.
    pub fn is_dirty(&self) -> bool {
        self.mode == OperatorDialogMode::Create
            || !same_editable_definition(&self.operator, &self.saved)
    }

    /// Adopt a definition the daemon just confirmed: it is now both what the
    /// editor shows and the baseline that "unsaved" is measured against.
    pub fn adopt_saved(&mut self, operator: OperatorSummary) {
        self.saved = operator.clone();
        self.operator = operator;
    }

    pub fn field_value(&self, field: usize) -> String {
        match field {
            0 => self.operator.name.clone(),
            1 => self.operator.instruction.replace('\n', " ↵ "),
            2 => self.operator.harness.clone(),
            3 => self
                .operator
                .model
                .as_deref()
                .and_then(|model| {
                    construct_protocol::published_model::decode_published_model_id(model)
                        .ok()
                        .flatten()
                })
                .map(|(route, model)| format!("{model} · {route}"))
                .or_else(|| self.operator.model.clone())
                .unwrap_or_default(),
            4 => self.operator.session_mode.clone(),
            5 => self.operator.cwd.clone(),
            6 => self.operator.routing.clone(),
            7 => {
                if self.operator.paused {
                    "paused".to_string()
                } else {
                    "serving".to_string()
                }
            }
            _ => String::new(),
        }
    }

    pub fn picker_options(&self, app: &App) -> Vec<OperatorDialogPickerOption> {
        match self.picker {
            Some(OperatorDialogPickerKind::Harness) => {
                let mut options: Vec<_> = app
                    .harnesses
                    .iter()
                    .map(|harness| OperatorDialogPickerOption {
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
                    .any(|option| option.value == self.operator.harness)
                {
                    options.insert(
                        0,
                        OperatorDialogPickerOption {
                            value: self.operator.harness.clone(),
                            label: self.operator.harness.clone(),
                            detail: "current value; no daemon probe available".to_string(),
                            available: false,
                        },
                    );
                }
                options
            }
            Some(OperatorDialogPickerKind::Model) => {
                let mut options = vec![OperatorDialogPickerOption {
                    value: String::new(),
                    label: "Default".to_string(),
                    detail: "let the selected harness choose its default model".to_string(),
                    available: true,
                }];
                if matches!(self.operator.harness.as_str(), "codex" | "claude")
                    && !app.operator_route_catalog.is_empty()
                {
                    for route in &app.operator_route_catalog {
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
                            options.push(OperatorDialogPickerOption {
                                value: construct_protocol::published_model::published_model_id(
                                    &route.name,
                                    &model,
                                ),
                                label: format!("{} / {model}", route.name),
                                detail,
                                available: route.unavailable_reason.is_none(),
                            });
                        }
                    }
                } else if let Some(harness) = app
                    .harnesses
                    .iter()
                    .find(|harness| harness.name == self.operator.harness)
                {
                    options.extend(harness.capabilities.models.iter().map(|model| {
                        OperatorDialogPickerOption {
                            value: model.clone(),
                            label: model.clone(),
                            detail: "advertised by this harness".to_string(),
                            available: true,
                        }
                    }));
                }
                if let Some(current) = self.operator.model.as_deref() {
                    let current = canonical_operator_model(current);
                    if !options.iter().any(|option| option.value == current) {
                        options.push(OperatorDialogPickerOption {
                            value: current.clone(),
                            label: current,
                            detail: "current value; not advertised by this harness".to_string(),
                            available: false,
                        });
                    }
                }
                options
            }
            Some(OperatorDialogPickerKind::SessionMode) => vec![
                OperatorDialogPickerOption {
                    value: "headless".to_string(),
                    label: "Headless / structured".to_string(),
                    detail: "structured events; automatic reply extraction".to_string(),
                    available: true,
                },
                OperatorDialogPickerOption {
                    value: "interactive".to_string(),
                    label: "Interactive / native PTY".to_string(),
                    detail: if matches!(self.operator.harness.as_str(), "codex" | "claude") {
                        "native UI; replies through the bound operator tool".to_string()
                    } else {
                        "currently requires codex or claude".to_string()
                    },
                    available: matches!(self.operator.harness.as_str(), "codex" | "claude"),
                },
            ],
            None => Vec::new(),
        }
    }
}

fn default_operator(app: &App, suggested: String) -> OperatorSummary {
    let selected = app.selected_session();
    OperatorSummary {
        name: suggested,
        position: app
            .operators
            .iter()
            .map(|operator| operator.position)
            .max()
            .map(|position| position.saturating_add(1))
            .unwrap_or_default(),
        placement: None,
        instruction: String::new(),
        harness: selected
            .map(|session| session.harness.clone())
            .unwrap_or_else(|| "smith".to_string()),
        model: selected.and_then(|session| session.model.clone()),
        session_mode: "headless".to_string(),
        cwd: selected
            .map(|session| session.cwd.clone())
            .unwrap_or_else(|| ".".to_string()),
        routing: "session-key".to_string(),
        paused: false,
        channels: Vec::new(),
    }
}

fn valid_operator_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

impl App {
    pub async fn refresh_operators(&mut self) {
        match self.client.list_operators().await {
            Ok(mut operators) => {
                operators.sort_by(|a, b| {
                    a.position
                        .cmp(&b.position)
                        .then_with(|| a.name.cmp(&b.name))
                });
                self.operator_token_meters
                    .retain(|name, _| operators.iter().any(|operator| operator.name == *name));
                self.operators = operators;
            }
            Err(error) => self.set_status(format!("operators refresh failed: {error}")),
        }
        match self.client.list_operator_channel_catalog().await {
            Ok(mut channels) => {
                channels.sort_by(|a, b| a.id.cmp(&b.id));
                self.operator_channel_catalog = channels;
                self.clamp_operator_dialog_focus();
            }
            Err(error) => self.set_status(format!("channel catalog refresh failed: {error}")),
        }
    }

    /// Sessions this operator has routed a request into, in list order.
    pub fn routed_operator_sessions(&self, name: &str) -> Vec<&SessionSummary> {
        let prefix = format!("operator:{name}");
        let nested = format!("{prefix}:");
        self.sessions
            .iter()
            .filter(|session| {
                session
                    .title
                    .as_deref()
                    .is_some_and(|title| title == prefix || title.starts_with(&nested))
            })
            .collect()
    }

    /// Operator whose routing namespace owns this session or one of its
    /// ancestors. Native subagents and forks contribute to the operator that
    /// owns their routed root even when their own title has no operator prefix.
    pub fn routed_operator_name<'a>(&'a self, session: &SessionSummary) -> Option<&'a str> {
        let mut current = session;
        // Session ancestry is acyclic by contract. The bound is a defensive
        // stop for malformed summaries so meter attribution can never loop.
        for _ in 0..=self.sessions.len() {
            if let Some(operator_name) = current
                .title
                .as_deref()
                .and_then(|title| title.strip_prefix("operator:"))
                .and_then(|suffix| suffix.split(':').next())
            {
                if let Some(operator) = self
                    .operators
                    .iter()
                    .find(|operator| operator.name == operator_name)
                {
                    return Some(operator.name.as_str());
                }
            }
            let parent_id = current
                .native_subagent
                .as_ref()
                .map(|native| native.owner_session_id.as_str())
                .or(current.parent_session_id.as_deref())
                .or_else(|| current.forked_from.as_ref().map(|fork| fork.session_id.as_str()))?;
            current = self.sessions.iter().find(|candidate| candidate.id == parent_id)?;
        }
        None
    }

    /// Navigable row counts below the definition fields. The channel section
    /// always offers one row: with an empty catalog it is the "create one"
    /// affordance, which is the only way to reach channel creation by keyboard.
    fn operator_dialog_row_counts(&self) -> (usize, usize) {
        let channels = self.operator_channel_catalog.len().max(1);
        let sessions = self
            .operator_dialog
            .as_ref()
            .map(|dialog| self.routed_operator_sessions(&dialog.operator.name).len())
            .unwrap_or(0);
        (channels, sessions)
    }

    fn clamp_operator_dialog_focus(&mut self) {
        let (channels, sessions) = self.operator_dialog_row_counts();
        if let Some(dialog) = self.operator_dialog.as_mut() {
            dialog.focus = dialog.focus.clamp(channels, sessions);
        }
    }

    fn move_operator_dialog_focus(&mut self, forward: bool) {
        let (channels, sessions) = self.operator_dialog_row_counts();
        if let Some(dialog) = self.operator_dialog.as_mut() {
            dialog.focus = if forward {
                dialog.focus.next(channels, sessions)
            } else {
                dialog.focus.prev(channels, sessions)
            };
        }
        // Moving the selection can scroll the section (the render pass brings
        // the focused row back into view), so reveal the bar the same way the
        // session view reveals its scrollback bar on a scroll.
        self.show_terminal_scrollbar();
    }

    pub fn open_new_operator_view(&mut self, suggested: impl Into<String>) {
        let suggested = suggested.into();
        self.dismiss_surfaces_over_operator_view();
        self.select_operator(suggested.clone());
        let operator = default_operator(self, suggested);
        self.operator_dialog = Some(OperatorDialog {
            mode: OperatorDialogMode::Create,
            saved: operator.clone(),
            operator,
            focus: OperatorDialogFocus::Field(0),
            note: Some("Enter saves this operator as its own TOML file.".to_string()),
            picker: None,
            picker_selected: 0,
            picker_scroll: 0,
            channel_editor: None,
        });
    }

    /// Open an unsaved operator using the first free conventional name. The
    /// name field has focus, so the generated suffix is only a collision-safe
    /// starting point and can be replaced immediately.
    pub(super) fn open_new_operator_view_with_default_name(&mut self) {
        let mut suggested = "operator".to_string();
        let mut suffix = 2_u32;
        while self
            .operators
            .iter()
            .any(|operator| operator.name == suggested)
        {
            suggested = format!("operator-{suffix}");
            suffix = suffix.saturating_add(1);
        }
        self.open_new_operator_view(suggested);
    }

    pub fn open_edit_operator_view(&mut self, name: &str) -> bool {
        if !self.operators.iter().any(|operator| operator.name == name) {
            self.set_status(format!("operator {name} not found"));
            return false;
        }
        self.dismiss_surfaces_over_operator_view();
        // Selecting the operator is what opens its editor — focusing a operator
        // view is edit mode (spec 0175).
        self.select_operator(name.to_string());
        true
    }

    /// Transient surfaces that would otherwise keep swallowing keystrokes
    /// after an action hands the user a freshly focused operator view.
    fn dismiss_surfaces_over_operator_view(&mut self) {
        self.configure_popup = None;
        self.session_picker = None;
        self.prompt = None;
    }

    /// Keep the inline editor attached to whatever the active pane shows:
    /// every path that focuses a operator leaves its editor open, and moving
    /// off a operator closes it. Called from selection and pane-focus changes.
    pub(super) fn sync_operator_editor_with_selection(&mut self) {
        let Some(name) = self.selection.operator_name().map(str::to_owned) else {
            self.operator_dialog = None;
            return;
        };
        if self
            .operator_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.operator.name == name)
        {
            return;
        }
        // A different operator brings its own channels and sessions, so the
        // section starts at the top again.
        self.operator_view_scroll = 0;
        match self
            .operators
            .iter()
            .find(|operator| operator.name == name)
            .cloned()
        {
            // A operator still being created has no daemon-side definition to
            // reopen; `open_new_operator_view` installs that editor itself, and
            // the selection follows the name field so this stays in step.
            None => self.operator_dialog = None,
            Some(operator) => self.operator_dialog = Some(OperatorDialog::editing(operator)),
        }
    }

    fn suggested_channel_id(&self, _operator: &OperatorSummary) -> String {
        if !self
            .operator_channel_catalog
            .iter()
            .any(|channel| channel.id == "http")
        {
            return "http".to_string();
        }
        (2..=99)
            .map(|index| format!("http-{index}"))
            .find(|id| {
                !self
                    .operator_channel_catalog
                    .iter()
                    .any(|channel| channel.id == *id)
            })
            .unwrap_or_else(|| format!("http-{}", self.operator_channel_catalog.len() + 1))
    }

    fn suggested_channel_port(&self, operator: &OperatorSummary) -> u16 {
        let mut used: std::collections::HashSet<u16> = operator
            .channels
            .iter()
            .filter_map(|channel| channel.port)
            .collect();
        used.extend(
            self.operator_channel_catalog
                .iter()
                .filter_map(|channel| channel.port),
        );
        (8787..=u16::MAX)
            .find(|port| !used.contains(port))
            .unwrap_or(8787)
    }

    pub fn open_new_operator_channel(&mut self) -> bool {
        let Some(operator) = self
            .operator_dialog
            .as_ref()
            .map(|dialog| dialog.operator.clone())
        else {
            return false;
        };
        let id = self.suggested_channel_id(&operator);
        let port = self.suggested_channel_port(&operator);
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return false;
        };
        dialog.focus = OperatorDialogFocus::Channel(dialog.focus.channel().unwrap_or(0));
        dialog.channel_editor = Some(OperatorChannelDialog {
            mode: OperatorChannelDialogMode::Create,
            operator_name: dialog.operator.name.clone(),
            channel: OperatorChannelSummary {
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
                // Filled in when the kind is switched to a Slack kind; an
                // HTTP channel has no behavior options to show.
                progress: None,
                follow_up: None,
                thread_context: None,
                mcp_command: None,
                trigger: None,
                response_mode: None,
                disclosure: None,
                poll_interval_secs: None,
                attached_to: Some(dialog.operator.name.clone()),
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

    pub fn open_edit_operator_channel(&mut self, index: usize) -> bool {
        self.open_operator_channel_editor(index, false)
    }

    /// Open the channel editor on catalog row `index`. Editing is limited to
    /// channels attached to this operator; `for_delete` also admits unattached
    /// catalog rows, since dropping one takes nothing away from any operator.
    fn open_operator_channel_editor(&mut self, index: usize, for_delete: bool) -> bool {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return false;
        };
        let Some(channel) = self.operator_channel_catalog.get(index).cloned() else {
            return false;
        };
        let attached_here = channel.attached_to.as_deref() == Some(dialog.operator.name.as_str());
        if !attached_here && !(for_delete && channel.attached_to.is_none()) {
            return false;
        }
        dialog.focus = OperatorDialogFocus::Channel(index);
        dialog.channel_editor = Some(OperatorChannelDialog {
            mode: OperatorChannelDialogMode::Edit,
            operator_name: dialog.operator.name.clone(),
            channel,
            selected_field: 2,
            note: Some(if attached_here {
                "Channel changes bind or unbind the listener immediately.".to_string()
            } else {
                "This channel is not attached to any operator.".to_string()
            }),
            new_secret: None,
            confirm_delete: false,
            app_token: String::new(),
            bot_token: String::new(),
        });
        true
    }

    /// Arm the delete-confirm prompt for the selected catalog row. A channel
    /// owned by another operator is refused: deleting it here would pull the
    /// endpoint out from under that operator.
    fn confirm_delete_operator_channel(&mut self, index: usize) {
        let Some(dialog) = self.operator_dialog.as_ref() else {
            return;
        };
        let operator_name = dialog.operator.name.clone();
        let Some(channel) = self.operator_channel_catalog.get(index).cloned() else {
            return;
        };
        if let Some(owner) = channel.attached_to.as_deref() {
            if owner != operator_name {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    dialog.note = Some(format!(
                        "Channel `{}` is already attached to operator `{owner}`.",
                        channel.id
                    ));
                }
                return;
            }
        }
        let attached_here = channel.attached_to.is_some();
        if !self.open_operator_channel_editor(index, true) {
            return;
        }
        if let Some(editor) = self
            .operator_dialog
            .as_mut()
            .and_then(|dialog| dialog.channel_editor.as_mut())
        {
            editor.confirm_delete = true;
            editor.note = Some(if attached_here {
                "Delete this channel? Enter/y confirms; Esc/n cancels.".to_string()
            } else {
                format!(
                    "Delete unattached channel `{}` from the catalog? Enter/y confirms; Esc/n cancels.",
                    editor.channel.id
                )
            });
        }
    }

    async fn toggle_operator_channel(&mut self, index: usize) {
        let Some(dialog) = self.operator_dialog.as_ref() else {
            return;
        };
        let operator_name = dialog.operator.name.clone();
        let Some(channel) = self.operator_channel_catalog.get(index).cloned() else {
            return;
        };
        let operation = match channel.attached_to.as_deref() {
            None => Some((true, channel.id.clone())),
            Some(owner) if owner == operator_name => Some((false, channel.id.clone())),
            Some(owner) => {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    dialog.note = Some(format!(
                        "Channel `{}` is already attached to operator `{owner}`.",
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
                .attach_operator_channel(&operator_name, &channel_id)
                .await
        } else {
            self.client
                .detach_operator_channel(&operator_name, &channel_id)
                .await
        };
        match result {
            Ok(result) => {
                self.refresh_operators().await;
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(operator) = self
                        .operators
                        .iter()
                        .find(|operator| operator.name == operator_name)
                        .cloned()
                    {
                        dialog.adopt_saved(operator);
                    }
                    dialog.note = Some(format!(
                        "Channel `{channel_id}` {}: {}.",
                        if attach { "attached" } else { "detached" },
                        result.applied.summary()
                    ));
                }
            }
            Err(error) => {
                if let Some(dialog) = self.operator_dialog.as_mut() {
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
        for channel in &mut self.operator_channel_catalog {
            if channel.id == payload.channel_id
                && channel.attached_to.as_deref() == Some(payload.operator_name.as_str())
            {
                channel.publication = payload.publication.clone();
            }
        }
        for operator in &mut self.operators {
            if operator.name != payload.operator_name {
                continue;
            }
            for channel in &mut operator.channels {
                if channel.id == payload.channel_id {
                    channel.publication = payload.publication.clone();
                }
            }
        }
        if let Some(dialog) = self.operator_dialog.as_mut() {
            if dialog.operator.name == payload.operator_name {
                for channel in &mut dialog.operator.channels {
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

    async fn toggle_channel_publication(&mut self, operator_name: &str, index: usize) {
        let operator_name = operator_name.to_string();
        let Some(channel) = self.operator_channel_catalog.get(index).cloned() else {
            return;
        };
        if channel.attached_to.as_deref() != Some(operator_name.as_str()) {
            if let Some(dialog) = self.operator_dialog.as_mut() {
                dialog.note = Some("Attach the channel before publishing it.".to_string());
            }
            return;
        }

        if channel.publication.is_some() {
            match self
                .client
                .unpublish_operator_channel(&operator_name, &channel.id)
                .await
            {
                Ok(_) => self.apply_channel_publication(
                    construct_protocol::ChannelPublicationNotificationPayload {
                        operator_name,
                        channel_id: channel.id,
                        publication: None,
                    },
                ),
                Err(error) => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
                        dialog.note = Some(format!("Unpublish failed: {error}"));
                    }
                }
            }
        } else {
            match self
                .client
                .publish_operator_channel(&operator_name, &channel.id, "construct")
                .await
            {
                Ok(publication) => self.apply_channel_publication(
                    construct_protocol::ChannelPublicationNotificationPayload {
                        operator_name,
                        channel_id: channel.id,
                        publication: Some(publication),
                    },
                ),
                Err(error) => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
    pub fn operator_channel_actions(
        &self,
        operator_name: &str,
        index: usize,
    ) -> Option<OperatorChannelActions> {
        let channel = self.operator_channel_catalog.get(index)?;
        if channel.attached_to.as_deref() != Some(operator_name)
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
                    Endpoint::Url { url } => OperatorChannelActionAddress::PublicUrl(url.clone()),
                    Endpoint::Socket { .. } => {
                        OperatorChannelActionAddress::PublicSocket(endpoint.to_string())
                    }
                });
            let authorization = publication
                .auth_url
                .as_ref()
                .map(|url| OperatorChannelActionAddress::AuthorizationUrl(url.clone()));
            match publication.phase {
                ChannelPublicationPhase::Authorizing => authorization.or(public),
                ChannelPublicationPhase::Ready => public.or(authorization),
                ChannelPublicationPhase::Connecting | ChannelPublicationPhase::Error => {
                    public.or(authorization)
                }
            }
        });

        Some(OperatorChannelActions {
            operator_name: operator_name.to_string(),
            channel_index: index,
            channel_id: channel.id.clone(),
            published: channel.publication.is_some(),
            address,
        })
    }

    pub fn selected_operator_channel_actions(
        &self,
        operator_name: &str,
    ) -> Option<OperatorChannelActions> {
        let index = self
            .operator_dialog
            .as_ref()
            .filter(|dialog| dialog.operator.name == operator_name)
            .and_then(|dialog| dialog.focus.channel())?;
        self.operator_channel_actions(operator_name, index)
    }

    pub(super) async fn run_operator_channel_action(
        &mut self,
        operator_name: &str,
        index: usize,
        action: OperatorChannelAction,
    ) {
        let Some(actions) = self.operator_channel_actions(operator_name, index) else {
            return;
        };
        match action {
            OperatorChannelAction::TogglePublication => {
                self.toggle_channel_publication(operator_name, index).await;
            }
            OperatorChannelAction::OpenAddress => {
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
            OperatorChannelAction::CopyAddress => {
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

    fn edit_operator_channel_text(&mut self, mut edit: impl FnMut(&mut String)) {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return;
        };
        let Some(channel) = dialog.channel_editor.as_mut() else {
            return;
        };
        let kind = channel.channel.kind.clone();
        let slack_kind = matches!(kind.as_str(), "slack" | "slack-personal");
        let field = channel.selected_field;
        if field == 0 && channel.mode == OperatorChannelDialogMode::Create {
            edit(&mut channel.channel.id);
        } else if kind == "http" && field == 2 {
            let mut value = channel
                .channel
                .port
                .map(|port| port.to_string())
                .unwrap_or_default();
            edit(&mut value);
            channel.channel.port = value.parse::<u16>().ok().filter(|port| *port > 0);
        } else if kind == "slack" && field == 2 {
            edit(&mut channel.app_token);
        } else if kind == "slack" && field == 3 {
            edit(&mut channel.bot_token);
        } else if kind == "slack-personal" && field == 2 {
            let mut value = channel.channel.mcp_command.clone().unwrap_or_default();
            edit(&mut value);
            channel.channel.mcp_command = Some(value);
        } else if slack_kind && field == workspace_field(&kind) {
            let mut value = channel.channel.allowed_workspaces.join(",");
            edit(&mut value);
            channel.channel.allowed_workspaces = split_allowlist(&value);
            channel.channel.allowed_workspace_count = channel.channel.allowed_workspaces.len();
        } else if slack_kind && field == channel_allowlist_field(&kind) {
            let mut value = channel.channel.allowed_channels.join(",");
            edit(&mut value);
            channel.channel.allowed_channels = split_allowlist(&value);
            channel.channel.allowed_channel_count = channel.channel.allowed_channels.len();
        } else if kind == "slack-personal" && field == PERSONAL_FIELD_POLL {
            let mut value = channel
                .channel
                .poll_interval_secs
                .map(|secs| secs.to_string())
                .unwrap_or_default();
            edit(&mut value);
            channel.channel.poll_interval_secs = if value.is_empty() {
                None
            } else {
                value
                    .parse::<u64>()
                    .ok()
                    .or(channel.channel.poll_interval_secs)
            };
        } else if slack_kind && field == thread_context_field(&kind) {
            let mut value = channel
                .channel
                .thread_context
                .map(|count| count.to_string())
                .unwrap_or_default();
            edit(&mut value);
            // An emptied field reads as none rather than as "unchanged":
            // the user is typing a number, and 0 is a real setting.
            channel.channel.thread_context = if value.is_empty() {
                Some(0)
            } else {
                value
                    .parse::<usize>()
                    .ok()
                    .map(|count| count.min(construct_protocol::SLACK_THREAD_CONTEXT_MAX))
                    .or(channel.channel.thread_context)
            };
        } else {
            return;
        }
        channel.note = None;
        channel.new_secret = None;
        channel.confirm_delete = false;
    }

    async fn save_operator_channel(&mut self, rotate_secret: bool) {
        let Some(parent) = self.operator_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let editor_snapshot = editor.clone();
        let slack = editor_snapshot.channel.kind == "slack";
        let personal = editor_snapshot.channel.kind == "slack-personal";
        let valid_id = valid_operator_name(&editor_snapshot.channel.id);
        let validation_error = if !valid_id {
            Some("Channel ID must be 1–32 lowercase letters, digits, or interior hyphens.")
        } else if editor_snapshot.channel.kind == "http" && editor_snapshot.channel.port.is_none() {
            Some("HTTP port must be between 1 and 65535.")
        } else if editor_snapshot.channel.kind == "slack"
            && editor_snapshot.mode == OperatorChannelDialogMode::Create
            && !editor_snapshot.app_token.starts_with("xapp-")
        {
            Some("Slack app token must start with xapp-.")
        } else if editor_snapshot.channel.kind == "slack"
            && editor_snapshot.mode == OperatorChannelDialogMode::Create
            && !editor_snapshot.bot_token.starts_with("xoxb-")
        {
            Some("Slack bot token must start with xoxb-.")
        } else if personal
            && editor_snapshot
                .channel
                .mcp_command
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            Some("slack-personal channels need an MCP command that starts their backend.")
        } else if personal
            && editor_snapshot
                .channel
                .poll_interval_secs
                .is_some_and(|secs| secs < construct_protocol::SLACK_PERSONAL_POLL_MIN_SECS)
        {
            Some("Poll interval must be at least 5 seconds.")
        } else {
            None
        };
        if let Some(message) = validation_error {
            if let Some(parent) = self.operator_dialog.as_mut() {
                if let Some(editor) = parent.channel_editor.as_mut() {
                    editor.note = Some(message.to_string());
                }
            }
            return;
        }
        match self
            .client
            .put_operator_channel(construct_protocol::OperatorChannelPutParams {
                operator_name: editor_snapshot.operator_name.clone(),
                channel: construct_protocol::OperatorChannelPut {
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
                    // Kind-specific: sending an option to a kind that does not
                    // read it is refused, and the editor never shows it there.
                    progress: slack.then_some(editor_snapshot.channel.progress).flatten(),
                    follow_up: slack.then_some(editor_snapshot.channel.follow_up).flatten(),
                    thread_context: (slack || personal)
                        .then_some(editor_snapshot.channel.thread_context)
                        .flatten(),
                    mcp_command: personal
                        .then_some(editor_snapshot.channel.mcp_command)
                        .flatten(),
                    trigger: personal.then_some(editor_snapshot.channel.trigger).flatten(),
                    response_mode: personal
                        .then_some(editor_snapshot.channel.response_mode)
                        .flatten(),
                    disclosure: personal
                        .then_some(editor_snapshot.channel.disclosure)
                        .flatten(),
                    poll_interval_secs: personal
                        .then_some(editor_snapshot.channel.poll_interval_secs)
                        .flatten(),
                },
                rotate_secret,
            })
            .await
        {
            Ok(result) => {
                self.refresh_operators().await;
                let refreshed_operator = self
                    .operators
                    .iter()
                    .find(|operator| operator.name == editor_snapshot.operator_name)
                    .cloned();
                let applied = result.applied.summary();
                let new_secret = result.new_secret;
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(operator) = refreshed_operator {
                        parent.adopt_saved(operator);
                    }
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.mode = OperatorChannelDialogMode::Edit;
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
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Channel save failed: {error}"));
                    }
                }
            }
        }
    }

    async fn delete_operator_channel(&mut self) {
        let Some(parent) = self.operator_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let operator_name = editor.operator_name.clone();
        let channel_id = editor.channel.id.clone();
        // An unattached channel has no live endpoint to withdraw; say so
        // instead of claiming a withdrawal that never happened.
        let was_attached = editor.channel.attached_to.is_some();
        match self
            .client
            .delete_operator_channel(&operator_name, &channel_id)
            .await
        {
            Ok(()) => {
                self.refresh_operators().await;
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(operator) = self
                        .operators
                        .iter()
                        .find(|operator| operator.name == operator_name)
                        .cloned()
                    {
                        parent.adopt_saved(operator);
                    }
                    parent.channel_editor = None;
                    parent.note = Some(if was_attached {
                        format!("Channel `{channel_id}` deleted and withdrawn.")
                    } else {
                        format!("Channel `{channel_id}` deleted from the catalog.")
                    });
                }
            }
            Err(error) => {
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Channel delete failed: {error}"));
                        editor.confirm_delete = false;
                    }
                }
            }
        }
    }

    async fn rotate_operator_channel_secret(&mut self) {
        let Some(parent) = self.operator_dialog.as_ref() else {
            return;
        };
        let Some(editor) = parent.channel_editor.as_ref() else {
            return;
        };
        let operator_name = editor.operator_name.clone();
        let channel_id = editor.channel.id.clone();
        match self
            .client
            .rotate_operator_channel_secret(&operator_name, &channel_id)
            .await
        {
            Ok(result) => {
                self.refresh_operators().await;
                let applied = result.applied.summary();
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(operator) = self
                        .operators
                        .iter()
                        .find(|operator| operator.name == operator_name)
                        .cloned()
                    {
                        parent.adopt_saved(operator);
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
                if let Some(parent) = self.operator_dialog.as_mut() {
                    if let Some(editor) = parent.channel_editor.as_mut() {
                        editor.note = Some(format!("Credential rotation failed: {error}"));
                    }
                }
            }
        }
    }

    fn open_operator_picker(&mut self, kind: OperatorDialogPickerKind) {
        let Some(mut dialog) = self.operator_dialog.clone() else {
            return;
        };
        dialog.picker = Some(kind);
        dialog.picker_scroll = 0;
        let options = dialog.picker_options(self);
        let current = match kind {
            OperatorDialogPickerKind::Harness => dialog.operator.harness.clone(),
            OperatorDialogPickerKind::Model => dialog
                .operator
                .model
                .as_deref()
                .map(canonical_operator_model)
                .unwrap_or_default(),
            OperatorDialogPickerKind::SessionMode => dialog.operator.session_mode.clone(),
        };
        dialog.picker_selected = options
            .iter()
            .position(|option| option.value == current)
            .unwrap_or(0);
        self.operator_dialog = Some(dialog);
        self.ensure_operator_picker_visible();
    }

    fn ensure_operator_picker_visible(&mut self) {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return;
        };
        if dialog.picker_selected < dialog.picker_scroll {
            dialog.picker_scroll = dialog.picker_selected;
        } else if dialog.picker_selected >= dialog.picker_scroll + OPERATOR_PICKER_VISIBLE_ROWS {
            dialog.picker_scroll = dialog
                .picker_selected
                .saturating_sub(OPERATOR_PICKER_VISIBLE_ROWS - 1);
        }
    }

    fn move_operator_picker(&mut self, delta: isize) {
        let Some(snapshot) = self.operator_dialog.clone() else {
            return;
        };
        if snapshot.picker.is_none() {
            return;
        }
        let options = snapshot.picker_options(self);
        if options.is_empty() {
            return;
        }
        if let Some(dialog) = self.operator_dialog.as_mut() {
            dialog.picker_selected = (dialog.picker_selected as isize + delta)
                .rem_euclid(options.len() as isize) as usize;
        }
        self.ensure_operator_picker_visible();
    }

    fn choose_operator_picker(&mut self) {
        let Some(snapshot) = self.operator_dialog.clone() else {
            return;
        };
        let Some(kind) = snapshot.picker else {
            return;
        };
        let options = snapshot.picker_options(self);
        let Some(option) = options.get(snapshot.picker_selected) else {
            return;
        };
        if !option.available {
            if let Some(dialog) = self.operator_dialog.as_mut() {
                dialog.note = Some(option.detail.clone());
            }
            return;
        }
        let value = option.value.clone();
        if let Some(dialog) = self.operator_dialog.as_mut() {
            match kind {
                OperatorDialogPickerKind::Harness => dialog.operator.harness = value,
                OperatorDialogPickerKind::Model => {
                    dialog.operator.model = (!value.is_empty()).then_some(value)
                }
                OperatorDialogPickerKind::SessionMode => dialog.operator.session_mode = value,
            }
            dialog.picker = None;
            dialog.picker_selected = 0;
            dialog.picker_scroll = 0;
            dialog.note = None;
        }
    }

    fn edit_operator_dialog_text(&mut self, mut edit: impl FnMut(&mut String)) {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return;
        };
        // Channel and session rows are not text: they act on daemon-side
        // objects, so their keys never reach the definition fields.
        let Some(field) = dialog.focus.field() else {
            return;
        };
        if dialog.mode == OperatorDialogMode::Edit && field == 0 {
            dialog.note = Some("Operator names cannot be changed after creation.".to_string());
            return;
        }
        match field {
            0 => edit(&mut dialog.operator.name),
            1 => edit(&mut dialog.operator.instruction),
            // Harness, model, and session mode are catalog-backed fields. They deliberately
            // do not accept typed or pasted text: Enter opens the picker and
            // the picker is the only way to change either value.
            2 | 3 | 4 => return,
            5 => edit(&mut dialog.operator.cwd),
            _ => {}
        }
        dialog.note = None;
        // A operator being named doesn't exist yet, so the pane is bound to it
        // by the name in the editor. Keep the selection following the field or
        // the next selection sync finds no such operator and drops the draft.
        if field == 0 {
            let name = dialog.operator.name.clone();
            if self.selection.operator_name() != Some(name.as_str()) {
                self.selection = Selection::Operator(name);
                self.sync_active_window_selection();
            }
        }
    }

    pub(super) fn insert_operator_dialog_text(&mut self, text: &str) -> bool {
        if self.operator_dialog.is_none() {
            return false;
        }
        let sanitized = text.replace(['\r', '\n'], " ");
        if self
            .operator_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.channel_editor.is_some())
        {
            self.edit_operator_channel_text(|value| value.push_str(&sanitized));
        } else {
            self.edit_operator_dialog_text(|value| value.push_str(&sanitized));
        }
        true
    }

    fn cycle_operator_dialog_value(&mut self, reverse: bool) {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return;
        };
        match dialog.focus.field() {
            Some(6) => {
                const ROUTING: [&str; 3] = ["session-key", "per-event", "single"];
                let current = ROUTING
                    .iter()
                    .position(|value| *value == dialog.operator.routing)
                    .unwrap_or(0);
                let next = if reverse {
                    current.checked_sub(1).unwrap_or(ROUTING.len() - 1)
                } else {
                    (current + 1) % ROUTING.len()
                };
                dialog.operator.routing = ROUTING[next].to_string();
            }
            Some(7) => dialog.operator.paused = !dialog.operator.paused,
            _ => {}
        }
        dialog.note = None;
    }

    async fn save_operator_dialog(&mut self) {
        let Some(dialog) = self.operator_dialog.clone() else {
            return;
        };
        let operator = dialog.operator;
        let validation_error = if !valid_operator_name(&operator.name) {
            Some("Name must be 1–32 lowercase letters, digits, or interior hyphens.")
        } else if operator.harness.trim().is_empty() {
            Some("Harness cannot be empty.")
        } else if operator.cwd.trim().is_empty() {
            Some("Working directory cannot be empty.")
        } else if operator.session_mode == "interactive"
            && !matches!(operator.harness.as_str(), "codex" | "claude")
        {
            Some("Interactive sessions currently require the codex or claude harness.")
        } else if !matches!(operator.session_mode.as_str(), "headless" | "interactive") {
            Some("Session mode must be headless or interactive.")
        } else if !matches!(
            operator.routing.as_str(),
            "session-key" | "per-event" | "single"
        ) {
            Some("Routing must be session-key, per-event, or single.")
        } else {
            None
        };
        if let Some(message) = validation_error {
            if let Some(dialog) = self.operator_dialog.as_mut() {
                dialog.note = Some(message.to_string());
            }
            return;
        }

        match self
            .client
            .put_operator(construct_protocol::OperatorPutParams { operator })
            .await
        {
            Ok(result) => {
                self.refresh_operators().await;
                let applied = result.applied.summary();
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    dialog.mode = OperatorDialogMode::Edit;
                    dialog.adopt_saved(result.operator);
                    dialog.note = Some(applied);
                }
            }
            Err(error) => {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    dialog.note = Some(format!("Save failed: {error}"));
                }
            }
        }
    }

    /// Delete a saved operator definition (C-x k / title-menu delete / palette).
    pub(super) async fn delete_operator_by_name(&mut self, name: String) {
        match self.client.delete_operator(name.clone()).await {
            Ok(()) => {
                let was_selected = self.selection.operator_name() == Some(name.as_str());
                if self.main_windows.clear_operator(&name) {
                    self.push_layout();
                }
                if self
                    .operator_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.operator.name == name)
                {
                    self.operator_dialog = None;
                }
                self.refresh_operators().await;
                if was_selected {
                    self.selection = Selection::None;
                    self.ensure_selection_valid();
                    self.sync_active_window_selection();
                }
                self.set_status(format!("{name} deleted; its endpoint is withdrawn"));
            }
            Err(error) => self.set_status(format!("operator delete failed: {error}")),
        }
    }

    /// Esc in a operator view. The editor is the view, so there is no
    /// view-only state to fall back to: Esc first throws away unsaved edits,
    /// and once there is nothing to discard it hands keyboard focus back to
    /// the session list (spec 0175).
    pub(super) fn escape_operator_dialog(&mut self) {
        let Some(dialog) = self.operator_dialog.as_mut() else {
            return;
        };
        if dialog.mode == OperatorDialogMode::Create {
            // Nothing has been written to disk yet, so the draft is all there
            // is — dropping it leaves no operator to keep showing.
            let name = dialog.operator.name.clone();
            self.operator_dialog = None;
            if self.selection.operator_name() == Some(name.as_str())
                && !self.operators.iter().any(|operator| operator.name == name)
            {
                self.selection = Selection::None;
                self.ensure_selection_valid();
                self.sync_active_window_selection();
            }
            self.focus = PaneFocus::List;
            self.set_status(format!("{name} discarded"));
            return;
        }
        if dialog.is_dirty() {
            let saved = dialog.saved.clone();
            // Channels move independently of the definition, so keep the live
            // attachment list and revert only what the user typed.
            dialog.operator = OperatorSummary {
                channels: dialog.operator.channels.clone(),
                ..saved
            };
            dialog.picker = None;
            dialog.picker_selected = 0;
            dialog.picker_scroll = 0;
            dialog.note = Some("Unsaved edits reverted.".to_string());
            return;
        }
        self.focus = PaneFocus::List;
    }

    pub(super) async fn handle_operator_dialog_key(&mut self, key: KeyEvent) -> bool {
        // A `C-x` chord already in flight belongs to the global keymap. The
        // editor no longer closes itself to make room for one, so it has to
        // stand aside for the continuation key explicitly.
        if !self.chord_state.is_empty() {
            return false;
        }
        self.clamp_operator_dialog_focus();
        let Some(snapshot) = self.operator_dialog.clone() else {
            return false;
        };
        if snapshot.channel_editor.is_some() {
            return self.handle_operator_channel_dialog_key(key).await;
        }
        if snapshot.picker.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
                        dialog.picker = None;
                        dialog.picker_selected = 0;
                        dialog.picker_scroll = 0;
                    }
                }
                KeyCode::Enter => self.choose_operator_picker(),
                KeyCode::Up => self.move_operator_picker(-1),
                KeyCode::Down => self.move_operator_picker(1),
                KeyCode::Char('p') if ctrl => self.move_operator_picker(-1),
                KeyCode::Char('n') if ctrl => self.move_operator_picker(1),
                KeyCode::Char('x') if ctrl => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
                    self.save_operator_dialog().await;
                    return true;
                }
                KeyCode::Char('x') => {
                    // Global C-x chords keep working over an open editor: the
                    // editor stays put and the chord continuation is let
                    // through by the guard at the top of this handler.
                    return false;
                }
                KeyCode::Char('p') => {
                    self.move_operator_dialog_focus(false);
                    return true;
                }
                KeyCode::Char('n') => {
                    self.move_operator_dialog_focus(true);
                    return true;
                }
                _ => {}
            }
        }
        let channel_row = snapshot.focus.channel();
        match key.code {
            KeyCode::Esc => self.escape_operator_dialog(),
            KeyCode::Enter => match snapshot.focus {
                OperatorDialogFocus::Field(2) => {
                    self.open_operator_picker(OperatorDialogPickerKind::Harness)
                }
                OperatorDialogFocus::Field(3) => {
                    self.open_operator_picker(OperatorDialogPickerKind::Model)
                }
                OperatorDialogFocus::Field(4) => {
                    self.open_operator_picker(OperatorDialogPickerKind::SessionMode)
                }
                OperatorDialogFocus::Field(_) => self.save_operator_dialog().await,
                OperatorDialogFocus::Channel(index) => {
                    if self.operator_channel_catalog.is_empty() {
                        self.open_new_operator_channel();
                    } else if self
                        .operator_channel_catalog
                        .get(index)
                        .is_some_and(|channel| {
                            channel.attached_to.as_deref() == Some(snapshot.operator.name.as_str())
                        })
                    {
                        self.open_edit_operator_channel(index);
                    } else if let Some(channel) = self.operator_channel_catalog.get(index) {
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
                        if let Some(dialog) = self.operator_dialog.as_mut() {
                            dialog.note = Some(message);
                        }
                    }
                }
                // A routed session row is a jump target, exactly like clicking
                // it: Enter leaves the operator view for that session.
                OperatorDialogFocus::Session(index) => {
                    if let Some(id) = self
                        .routed_operator_sessions(&snapshot.operator.name)
                        .get(index)
                        .map(|session| session.id.clone())
                    {
                        self.select_session(id);
                    }
                }
            },
            KeyCode::Char(' ') if channel_row.is_some() => {
                self.toggle_operator_channel(channel_row.unwrap_or_default())
                    .await;
            }
            KeyCode::Char('a') if channel_row.is_some() => {
                self.open_new_operator_channel();
            }
            KeyCode::Char('e') if channel_row.is_some() => {
                self.open_edit_operator_channel(channel_row.unwrap_or_default());
            }
            KeyCode::Char('d') if channel_row.is_some() => {
                self.confirm_delete_operator_channel(channel_row.unwrap_or_default());
            }
            KeyCode::Char('r') if channel_row.is_some() => {
                if self.open_edit_operator_channel(channel_row.unwrap_or_default()) {
                    self.rotate_operator_channel_secret().await;
                }
            }
            KeyCode::Char('p') if channel_row.is_some() => {
                self.run_operator_channel_action(
                    &snapshot.operator.name,
                    channel_row.unwrap_or_default(),
                    OperatorChannelAction::TogglePublication,
                )
                .await;
            }
            KeyCode::Char('o') if channel_row.is_some() => {
                self.run_operator_channel_action(
                    &snapshot.operator.name,
                    channel_row.unwrap_or_default(),
                    OperatorChannelAction::OpenAddress,
                )
                .await;
            }
            KeyCode::Char('y') if channel_row.is_some() => {
                self.run_operator_channel_action(
                    &snapshot.operator.name,
                    channel_row.unwrap_or_default(),
                    OperatorChannelAction::CopyAddress,
                )
                .await;
            }
            KeyCode::Tab | KeyCode::Down => self.move_operator_dialog_focus(true),
            KeyCode::BackTab | KeyCode::Up => self.move_operator_dialog_focus(false),
            KeyCode::Left => self.cycle_operator_dialog_value(true),
            KeyCode::Right | KeyCode::Char(' ')
                if matches!(snapshot.focus, OperatorDialogFocus::Field(6 | 7)) =>
            {
                self.cycle_operator_dialog_value(false)
            }
            KeyCode::Backspace => self.edit_operator_dialog_text(|value| {
                value.pop();
            }),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.edit_operator_dialog_text(|value| value.push(ch));
            }
            _ => {}
        }
        true
    }

    async fn handle_operator_channel_dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(snapshot) = self
            .operator_dialog
            .as_ref()
            .and_then(|dialog| dialog.channel_editor.clone())
        else {
            return false;
        };
        if snapshot.confirm_delete {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.delete_operator_channel().await,
                KeyCode::Esc | KeyCode::Char('n') => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
                    self.save_operator_channel(false).await;
                    return true;
                }
                KeyCode::Char('r')
                    if snapshot.mode == OperatorChannelDialogMode::Edit
                        && snapshot.channel.kind == "http" =>
                {
                    self.rotate_operator_channel_secret().await;
                    return true;
                }
                KeyCode::Char('d') if snapshot.mode == OperatorChannelDialogMode::Edit => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
                    if let Some(dialog) = self.operator_dialog.as_mut() {
                        dialog.channel_editor = None;
                    }
                    return false;
                }
                KeyCode::Char('p') => {
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
                    if let Some(dialog) = self.operator_dialog.as_mut() {
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
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    dialog.channel_editor = None;
                }
            }
            KeyCode::Enter => self.save_operator_channel(false).await,
            KeyCode::Tab | KeyCode::Down => {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.selected_field =
                            (editor.selected_field + 1) % channel_field_count(editor);
                    }
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(dialog) = self.operator_dialog.as_mut() {
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
                    && snapshot.mode == OperatorChannelDialogMode::Create =>
            {
                let forward = key.code != KeyCode::Left;
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.channel.kind =
                            cycle_option(&["http", "slack", "slack-personal"], Some(&editor.channel.kind), forward);
                        let slack = editor.channel.kind == "slack";
                        let personal = editor.channel.kind == "slack-personal";
                        editor.channel.port = (!slack && !personal).then_some(8787);
                        // Show a new channel the values it will be saved with
                        // rather than a column of blanks.
                        editor.channel.progress =
                            slack.then(|| construct_protocol::SLACK_PROGRESS_DEFAULT.to_string());
                        editor.channel.follow_up =
                            slack.then(|| construct_protocol::SLACK_FOLLOW_UP_DEFAULT.to_string());
                        editor.channel.thread_context = (slack || personal)
                            .then_some(construct_protocol::SLACK_THREAD_CONTEXT_DEFAULT);
                        editor.channel.mcp_command = personal.then(String::new);
                        editor.channel.trigger = personal
                            .then(|| construct_protocol::SLACK_PERSONAL_TRIGGER_DEFAULT.to_string());
                        editor.channel.response_mode = personal
                            .then(|| construct_protocol::SLACK_PERSONAL_RESPONSE_DEFAULT.to_string());
                        editor.channel.disclosure = personal.then_some(true);
                        editor.channel.poll_interval_secs =
                            personal.then_some(construct_protocol::SLACK_PERSONAL_POLL_DEFAULT_SECS);
                        editor.selected_field = 1;
                        editor.note = None;
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if snapshot.selected_field == channel_state_field(&snapshot.channel.kind) =>
            {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.channel.enabled = !editor.channel.enabled;
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if snapshot.channel.kind == "slack-personal"
                    && snapshot.selected_field == PERSONAL_FIELD_DISCLOSURE =>
            {
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        editor.channel.disclosure =
                            Some(!editor.channel.disclosure.unwrap_or(true));
                        editor.note = None;
                    }
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if (snapshot.channel.kind == "slack"
                    && matches!(
                        snapshot.selected_field,
                        SLACK_FIELD_PROGRESS | SLACK_FIELD_FOLLOW_UP
                    ))
                    || (snapshot.channel.kind == "slack-personal"
                        && matches!(
                            snapshot.selected_field,
                            PERSONAL_FIELD_TRIGGER | PERSONAL_FIELD_RESPONSE
                        )) =>
            {
                // More than two values, so Left has to mean the other way.
                let forward = key.code != KeyCode::Left;
                if let Some(dialog) = self.operator_dialog.as_mut() {
                    if let Some(editor) = dialog.channel_editor.as_mut() {
                        if editor.channel.kind == "slack" {
                            if editor.selected_field == SLACK_FIELD_PROGRESS {
                                editor.channel.progress = Some(cycle_option(
                                    construct_protocol::SLACK_PROGRESS_VALUES,
                                    editor.channel.progress.as_deref(),
                                    forward,
                                ));
                            } else {
                                editor.channel.follow_up = Some(cycle_option(
                                    construct_protocol::SLACK_FOLLOW_UP_VALUES,
                                    editor.channel.follow_up.as_deref(),
                                    forward,
                                ));
                            }
                        } else if editor.selected_field == PERSONAL_FIELD_TRIGGER {
                            editor.channel.trigger = Some(cycle_option(
                                construct_protocol::SLACK_PERSONAL_TRIGGER_VALUES,
                                editor.channel.trigger.as_deref(),
                                forward,
                            ));
                        } else {
                            editor.channel.response_mode = Some(cycle_option(
                                construct_protocol::SLACK_PERSONAL_RESPONSE_VALUES,
                                editor.channel.response_mode.as_deref(),
                                forward,
                            ));
                        }
                        editor.note = None;
                    }
                }
            }
            KeyCode::Backspace => self.edit_operator_channel_text(|value| {
                value.pop();
            }),
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.edit_operator_channel_text(|value| value.push(ch));
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
