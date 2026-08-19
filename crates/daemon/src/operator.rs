//! Daemon-owned operator definitions and channel lifecycle.
//!
//! Operator definitions live in `operators/<name>.toml` in the config dir.
//! HTTP remains loopback-only. Transport adapters are separate from the shared
//! ingress router so adding a channel does not fork session semantics.

mod harness_error;
mod http;
mod ingress;
mod mcp;
mod slack;
mod slack_personal;

use anyhow::{anyhow, Context, Result};
// The accepted values for a channel's behavior options are published by the
// protocol so that what a client offers and what the daemon accepts cannot
// drift apart.
use construct_protocol::{
    SLACK_FOLLOW_UP_VALUES as FOLLOW_UP_VALUES, SLACK_PERSONAL_POLL_DEFAULT_SECS,
    SLACK_PERSONAL_POLL_MIN_SECS, SLACK_PERSONAL_RESPONSE_VALUES, SLACK_PERSONAL_TRIGGER_VALUES,
    SLACK_PROGRESS_VALUES as PROGRESS_VALUES, SLACK_THREAD_CONTEXT_DEFAULT,
    SLACK_THREAD_CONTEXT_MAX as THREAD_CONTEXT_MAX,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorConfig {
    /// Stable position among operator rows. This is display metadata only;
    /// operator runtime behavior does not depend on it.
    #[serde(default)]
    pub position: u64,
    #[serde(default)]
    pub instruction: String,
    #[serde(default = "default_operator_harness")]
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_mode: OperatorSessionMode,
    #[serde(default = "default_operator_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub routing: OperatorRouting,
    #[serde(default)]
    pub paused: bool,
    /// Seconds to hold a turn stopped at an approval before denying it on the
    /// caller's behalf. `0` waits indefinitely, which keeps the user as
    /// the only one who can decide.
    #[serde(default)]
    pub approval_timeout_secs: u64,
    /// Slot in the top-level list flow when the row has been moved out of the
    /// leading operator block (display metadata only; `None` = leading block).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<construct_protocol::OperatorPlacement>,
    #[serde(default)]
    pub sandbox: OperatorSandboxConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, OperatorChannelConfig>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorSessionMode {
    #[default]
    Headless,
    Interactive,
}

impl OperatorSessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Interactive => "interactive",
        }
    }
}

/// Capability limits applied to every session a operator creates.
///
/// A operator session is prompted by a third party, so it is confined by
/// default and widened only by explicit configuration. Filesystem and network
/// confinement are not represented here: the harness sandbox already limits
/// writes to the session's working directory and denies egress, and a operator
/// must not be able to relax that from its own definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct OperatorSandboxConfig {
    /// Allow tools that reach other sessions or the daemon itself.
    #[serde(default)]
    pub fleet_control: bool,
    /// Allow the construct MCP server to be injected into harnesses that
    /// take their fleet access that way.
    #[serde(default)]
    pub mcp: bool,
    /// Allow the harness to load skills.
    #[serde(default = "default_sandbox_skills")]
    pub skills: bool,
}

fn default_sandbox_skills() -> bool {
    true
}

impl Default for OperatorSandboxConfig {
    fn default() -> Self {
        Self {
            fleet_control: false,
            mcp: false,
            skills: default_sandbox_skills(),
        }
    }
}

impl OperatorSandboxConfig {
    /// Environment applied at session creation. Each entry withholds a
    /// capability; an allowed capability adds nothing, so a operator session
    /// with everything enabled is indistinguishable from an ordinary one.
    pub fn session_env(&self) -> HashMap<String, String> {
        let mut env = HashMap::new();
        if !self.fleet_control {
            env.insert("CONSTRUCT_SMITH_FLEET_TOOLS".to_string(), "off".to_string());
        }
        if !self.mcp {
            env.insert("CONSTRUCT_INJECT_MCP".to_string(), "0".to_string());
        }
        if !self.skills {
            env.insert("CONSTRUCT_SMITH_SKILLS".to_string(), "off".to_string());
        }
        env
    }
}

fn default_operator_harness() -> String {
    "smith".to_string()
}

fn default_operator_cwd() -> String {
    ".".to_string()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperatorRouting {
    PerEvent,
    #[default]
    SessionKey,
    Single,
}

/// What a Slack channel shows while a turn it accepted is still running.
///
/// A long turn is indistinguishable from a dropped one when the channel stays
/// silent, so the user picks how visible the wait should be. `Reaction`
/// and `Both` call `reactions.add`, which needs the `reactions:write` scope —
/// an app the user has not reinstalled since granting it will log the
/// refusal and keep answering normally.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SlackProgress {
    /// Say nothing until the answer is ready.
    Off,
    /// A thread message that later becomes the answer itself.
    #[default]
    Placeholder,
    /// An emoji reaction on the message that triggered the turn.
    Reaction,
    Both,
}

impl SlackProgress {
    pub(crate) fn posts_placeholder(self) -> bool {
        matches!(self, Self::Placeholder | Self::Both)
    }

    pub(crate) fn reacts(self) -> bool {
        matches!(self, Self::Reaction | Self::Both)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Placeholder => "placeholder",
            Self::Reaction => "reaction",
            Self::Both => "both",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "placeholder" => Ok(Self::Placeholder),
            "reaction" => Ok(Self::Reaction),
            "both" => Ok(Self::Both),
            other => Err(unknown_option("progress", other, PROGRESS_VALUES)),
        }
    }
}

/// Where a Slack channel keeps listening after it has been addressed.
///
/// A bot that must be `@`-mentioned for every turn cannot hold a conversation:
/// the person asking has to keep re-addressing an participant that is visibly
/// already in the room. Once engaged, the bot behaves like a participant —
/// within a boundary the user sets, because "answers everything in this
/// channel" is right for a dedicated channel and wrong for a busy shared one.
///
/// Anything past `Off` needs the `message.channels` event subscription (plus
/// `message.groups` for private channels). Without it Slack never sends the
/// untagged messages, and every mode behaves like `Off`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SlackFollowUp {
    /// Only direct mentions and DMs.
    Off,
    /// Keep answering inside a thread the bot was mentioned in.
    #[default]
    Thread,
    /// Keep answering anywhere in a channel the bot has been mentioned in.
    Channel,
}

impl SlackFollowUp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Thread => "thread",
            Self::Channel => "channel",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "thread" => Ok(Self::Thread),
            "channel" => Ok(Self::Channel),
            other => Err(unknown_option("follow_up", other, FOLLOW_UP_VALUES)),
        }
    }
}

/// What may enter a slack-personal operator (spec 0202). The channel has no
/// bot to mention: DMs are addressed to the user by construction, while a
/// channel message needs the trigger widened and the channel allowlisted.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SlackPersonalTrigger {
    /// Direct messages only.
    #[default]
    Dm,
    /// DMs plus every message in an allowlisted channel.
    All,
}

impl SlackPersonalTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::All => "all",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "dm" => Ok(Self::Dm),
            "all" => Ok(Self::All),
            other => Err(unknown_option("trigger", other, SLACK_PERSONAL_TRIGGER_VALUES)),
        }
    }
}

/// How visibly a slack-personal operator may answer (spec 0202). Everything
/// it posts appears as the user, so the safe default leaves the send to them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SlackPersonalResponse {
    /// Compose the reply as a Slack draft; the human reviews and sends it.
    #[default]
    Draft,
    /// Post directly, disclosed by default.
    Auto,
    /// Wait for the configured grace period, then post only if the user has
    /// not answered the thread themself (spec 0202).
    AutoAfter,
}

impl SlackPersonalResponse {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Auto => "auto",
            Self::AutoAfter => "auto-after",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(Self::Draft),
            "auto" => Ok(Self::Auto),
            "auto-after" => Ok(Self::AutoAfter),
            other => Err(unknown_option(
                "response_mode",
                other,
                SLACK_PERSONAL_RESPONSE_VALUES,
            )),
        }
    }
}

fn unknown_option(field: &str, value: &str, accepted: &[&str]) -> anyhow::Error {
    anyhow::anyhow!(
        "unknown {field} value {value:?}; expected one of {}",
        accepted.join(", ")
    )
}

fn default_thread_context() -> usize {
    SLACK_THREAD_CONTEXT_DEFAULT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorChannelConfig {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_channel_enabled")]
    pub enabled: bool,
    pub port: Option<u16>,
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_workspaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_channels: Vec<String>,
    /// Slack only. Omitted definitions keep the default affordance.
    #[serde(default)]
    pub progress: SlackProgress,
    /// Slack only. Where the bot keeps answering once it has been addressed.
    #[serde(default)]
    pub follow_up: SlackFollowUp,
    /// Slack only. How many earlier messages of a thread to read when first
    /// pulled into one. `0` reads none. Needs `channels:history`.
    #[serde(default = "default_thread_context")]
    pub thread_context: usize,
    /// slack-personal only. Shell command that starts the channel's MCP
    /// backend on stdio (spec 0201). Stored as `Option` — like the fields
    /// above being Slack-only — so unrelated kinds serialize without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<SlackPersonalTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<SlackPersonalResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_after_secs: Option<u64>,
    /// Whether an auto-sent reply carries the agent marker. `None` reads as
    /// on: undisclosed impersonation is the failure mode (spec 0202), so it
    /// must be an explicit opt-out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u64>,
}

fn default_channel_enabled() -> bool {
    true
}

/// Matches the serde defaults, so a config built from `..Default::default()`
/// reads the same as one parsed from a TOML that omits those fields.
impl Default for OperatorChannelConfig {
    fn default() -> Self {
        Self {
            kind: None,
            enabled: default_channel_enabled(),
            port: None,
            token: None,
            app_token: None,
            bot_token: None,
            allowed_workspaces: Vec::new(),
            allowed_channels: Vec::new(),
            progress: SlackProgress::default(),
            follow_up: SlackFollowUp::default(),
            thread_context: default_thread_context(),
            mcp_command: None,
            trigger: None,
            response_mode: None,
            auto_after_secs: None,
            disclosure: None,
            poll_interval_secs: None,
        }
    }
}

pub fn load_definitions(dir: &std::path::Path) -> Result<BTreeMap<String, OperatorConfig>> {
    let mut operators = BTreeMap::new();
    if !dir.exists() {
        return Ok(operators);
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        validate_operator_name(name)?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read operator definition {}", path.display()))?;
        let definition = toml::from_str(&raw)
            .with_context(|| format!("parse operator definition {}", path.display()))?;
        operators.insert(name.to_string(), definition);
    }
    Ok(operators)
}

/// Names of every defined operator, tolerating malformed definition files.
/// Session reordering only needs to know which `operator:<name>` title
/// prefixes are claimed, so a definition whose TOML fails to parse still
/// counts — its routed sessions nest under the operator row the moment the
/// file is fixed, and hiding them from the flat reorder region either way
/// keeps a swap from pairing a visible row with an invisible one.
pub fn known_operator_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("toml") {
                return None;
            }
            let name = path.file_stem()?.to_str()?;
            validate_operator_name(name).ok()?;
            Some(name.to_string())
        })
        .collect();
    names.sort();
    names
}

pub fn list_summaries(dir: &std::path::Path) -> Result<Vec<construct_protocol::OperatorSummary>> {
    let mut operators: Vec<_> = load_definitions(dir)?
        .into_iter()
        .map(|(name, config)| summary(name, &config))
        .collect();
    operators.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(operators)
}

pub fn put_definition(
    dir: &std::path::Path,
    params: construct_protocol::OperatorPutParams,
) -> Result<construct_protocol::OperatorPutResult> {
    validate_operator_name(&params.operator.name)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.toml", params.operator.name));
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| toml::from_str::<OperatorConfig>(&raw).ok());
    let channels = existing
        .as_ref()
        .map(|config| config.channels.clone())
        .unwrap_or_default();
    // Sandbox limits are not part of the edit surface, so an edit must carry
    // the stored ones forward. Rebuilding them from defaults would silently
    // re-grant (or revoke) capabilities on an unrelated field change.
    let sandbox = existing
        .as_ref()
        .map(|config| config.sandbox.clone())
        .unwrap_or_default();
    let routing = match params.operator.routing.as_str() {
        "per-event" => OperatorRouting::PerEvent,
        "session-key" => OperatorRouting::SessionKey,
        "single" => OperatorRouting::Single,
        other => return Err(anyhow!("invalid routing mode `{other}`")),
    };
    let session_mode =
        parse_operator_session_mode(&params.operator.harness, &params.operator.session_mode)?;
    let position = match existing.as_ref() {
        Some(config) => config.position,
        None => load_definitions(dir)?
            .values()
            .map(|config| config.position)
            .max()
            .map(|position| position.saturating_add(1))
            .unwrap_or_default(),
    };
    let config = OperatorConfig {
        position,
        instruction: params.operator.instruction,
        harness: params.operator.harness,
        model: params.operator.model,
        session_mode,
        cwd: params.operator.cwd,
        routing,
        paused: params.operator.paused,
        approval_timeout_secs: existing
            .as_ref()
            .map(|config| config.approval_timeout_secs)
            .unwrap_or_default(),
        // Like `position`, a row's slot in the list is not part of the edit
        // surface: an unrelated field change must not snap the row back to
        // the leading operator block.
        placement: existing.as_ref().and_then(|config| config.placement),
        sandbox,
        channels,
    };
    write_definition(dir, &params.operator.name, &config)?;
    Ok(construct_protocol::OperatorPutResult {
        operator: summary(params.operator.name, &config),
        applied: Default::default(),
    })
}

/// One row of the top-level list flow a operator row can step across: another
/// operator, an ungrouped session, or a whole project block (operators never
/// nest inside a project, so a project is a single hop).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlowRow {
    Operator(String),
    Session { position: i64 },
    Project { position: i64 },
}

/// Whether a session renders as a top-level ungrouped row in every
/// first-party list client. Routed operator sessions nest under their operator
/// row, so they are matched by title key the same way clients match them.
fn is_top_level_ungrouped_session(
    session: &construct_protocol::SessionSummary,
    operator_names: &[&str],
) -> bool {
    session.kind == construct_protocol::SessionKind::User
        && !session.archived
        && session.group_id.is_none()
        && session.forked_from.is_none()
        && !operator_names.iter().any(|name| {
            let Some(title) = session.title.as_deref() else {
                return false;
            };
            let prefix = format!("operator:{name}");
            title == prefix || title.starts_with(&format!("{prefix}:"))
        })
}

/// Current top-level display flow: the leading operator block, then the
/// ungrouped-session run with placed operators interleaved, then the projects
/// run likewise. Mirrors the order every list client renders.
fn top_level_flow(
    operators: &BTreeMap<String, OperatorConfig>,
    sessions: &[construct_protocol::SessionSummary],
    groups: &[construct_protocol::GroupSummary],
) -> Vec<FlowRow> {
    use construct_protocol::OperatorPlacementRegion as Region;

    let operator_order = |name: &str| {
        let config = &operators[name];
        (config.position, name.to_string())
    };
    let mut block: Vec<&String> = operators
        .iter()
        .filter(|(_, config)| config.placement.is_none())
        .map(|(name, _)| name)
        .collect();
    block.sort_by_key(|name| operator_order(name));

    let operator_names: Vec<&str> = operators.keys().map(String::as_str).collect();
    let mut ungrouped: Vec<&construct_protocol::SessionSummary> = sessions
        .iter()
        .filter(|session| is_top_level_ungrouped_session(session, &operator_names))
        .collect();
    ungrouped.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| b.created_at.cmp(&a.created_at))
    });
    let mut projects: Vec<&construct_protocol::GroupSummary> = groups.iter().collect();
    projects.sort_by_key(|group| group.position);

    // Placed operators merge into a region by (position, row-first, operator
    // order): at equal positions the session/project row they were dropped
    // after renders first.
    let placed_in = |region: Region| {
        let mut placed: Vec<(&String, i64)> = operators
            .iter()
            .filter_map(|(name, config)| {
                config
                    .placement
                    .filter(|placement| placement.region == region)
                    .map(|placement| (name, placement.position))
            })
            .collect();
        placed.sort_by_key(|(name, position)| (*position, operator_order(name)));
        placed
    };

    let mut rows: Vec<FlowRow> = block
        .into_iter()
        .map(|name| FlowRow::Operator(name.clone()))
        .collect();
    let mut sessions_region = placed_in(Region::Sessions).into_iter().peekable();
    for session in ungrouped {
        while sessions_region
            .peek()
            .is_some_and(|(_, position)| *position < session.position)
        {
            let (name, _) = sessions_region.next().unwrap();
            rows.push(FlowRow::Operator(name.clone()));
        }
        rows.push(FlowRow::Session {
            position: session.position,
        });
    }
    for (name, _) in sessions_region {
        rows.push(FlowRow::Operator(name.clone()));
    }
    let mut projects_region = placed_in(Region::Projects).into_iter().peekable();
    for project in projects {
        while projects_region
            .peek()
            .is_some_and(|(_, position)| *position < project.position)
        {
            let (name, _) = projects_region.next().unwrap();
            rows.push(FlowRow::Operator(name.clone()));
        }
        rows.push(FlowRow::Project {
            position: project.position,
        });
    }
    for (name, _) in projects_region {
        rows.push(FlowRow::Operator(name.clone()));
    }
    rows
}

/// Move one operator row past its adjacent top-level row: another operator, an
/// ungrouped session, or a whole project block. The step is applied to the
/// current display flow, then every operator's persisted order is re-derived
/// from the result — contiguous operator positions in flow order, and each
/// interleaved operator re-pinned to the position value of the session or
/// project row it now renders after (`None` placement = leading block). That
/// normalization also heals placements whose pinned row has since vanished.
pub fn move_definition(
    dir: &std::path::Path,
    name: &str,
    direction: construct_protocol::MoveDirection,
    sessions: &[construct_protocol::SessionSummary],
    groups: &[construct_protocol::GroupSummary],
) -> Result<()> {
    use construct_protocol::OperatorPlacement;
    use construct_protocol::OperatorPlacementRegion as Region;

    let mut operators = load_definitions(dir)?;
    if !operators.contains_key(name) {
        return Err(anyhow!("operator not found: {name}"));
    }
    let mut rows = top_level_flow(&operators, sessions, groups);
    let index = rows
        .iter()
        .position(|row| matches!(row, FlowRow::Operator(candidate) if candidate == name))
        .expect("every operator has a flow row");
    let neighbor = match direction {
        construct_protocol::MoveDirection::Up if index > 0 => index - 1,
        construct_protocol::MoveDirection::Down if index + 1 < rows.len() => index + 1,
        _ => return Ok(()),
    };
    rows.swap(index, neighbor);

    // Re-derive persisted state from the desired order. A operator row's
    // placement is the position value of the nearest preceding session or
    // project row; with none it belongs to the leading block.
    let mut preceding: Option<OperatorPlacement> = None;
    let mut next_position: u64 = 0;
    for row in &rows {
        match row {
            FlowRow::Session { position } => {
                preceding = Some(OperatorPlacement {
                    region: Region::Sessions,
                    position: *position,
                });
            }
            FlowRow::Project { position } => {
                preceding = Some(OperatorPlacement {
                    region: Region::Projects,
                    position: *position,
                });
            }
            FlowRow::Operator(operator_name) => {
                let config = operators
                    .get_mut(operator_name)
                    .ok_or_else(|| anyhow!("operator disappeared while reordering: {operator_name}"))?;
                let position = next_position;
                next_position += 1;
                if config.position != position || config.placement != preceding {
                    config.position = position;
                    config.placement = preceding;
                    write_definition(dir, operator_name, config)?;
                }
            }
        }
    }
    Ok(())
}

fn parse_operator_session_mode(harness: &str, mode: &str) -> Result<OperatorSessionMode> {
    match mode {
        "headless" => Ok(OperatorSessionMode::Headless),
        "interactive" if matches!(harness, "codex" | "claude") => {
            Ok(OperatorSessionMode::Interactive)
        }
        "interactive" => {
            return Err(anyhow!(
                "interactive operator sessions currently require the `codex` or `claude` harness"
            ))
        }
        other => return Err(anyhow!("invalid operator session mode `{other}`")),
    }
}

pub fn delete_definition(dir: &std::path::Path, name: &str) -> Result<()> {
    validate_operator_name(name)?;
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ChannelCatalog {
    #[serde(default)]
    channels: BTreeMap<String, OperatorChannelConfig>,
}

fn channel_catalog_path(dir: &std::path::Path) -> PathBuf {
    dir.parent().unwrap_or(dir).join("channels.toml")
}

fn load_channel_catalog(dir: &std::path::Path) -> Result<ChannelCatalog> {
    let path = channel_catalog_path(dir);
    if !path.exists() {
        return Ok(ChannelCatalog::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read channel catalog {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parse channel catalog {}", path.display()))
}

fn write_channel_catalog(dir: &std::path::Path, catalog: &ChannelCatalog) -> Result<()> {
    let path = channel_catalog_path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let encoded = toml::to_string_pretty(catalog)?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn channel_owners(operators: &BTreeMap<String, OperatorConfig>) -> BTreeMap<String, Vec<String>> {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (operator_name, operator) in operators {
        for channel_id in operator.channels.keys() {
            owners
                .entry(channel_id.clone())
                .or_default()
                .push(operator_name.clone());
        }
    }
    owners
}

fn owner_label(owners: &BTreeMap<String, Vec<String>>, channel_id: &str) -> Option<String> {
    owners.get(channel_id).map(|operators| operators.join(", "))
}

fn migrate_legacy_channels(
    dir: &std::path::Path,
    operators: &BTreeMap<String, OperatorConfig>,
    catalog: &mut ChannelCatalog,
) -> Result<()> {
    let mut changed = false;
    for operator in operators.values() {
        for (id, config) in &operator.channels {
            if !catalog.channels.contains_key(id) {
                catalog.channels.insert(id.clone(), config.clone());
                changed = true;
            }
        }
    }
    if changed {
        write_channel_catalog(dir, catalog)?;
    }
    Ok(())
}

pub fn list_channel_summaries(
    dir: &std::path::Path,
    operator_name: &str,
) -> Result<Vec<construct_protocol::OperatorChannelSummary>> {
    validate_operator_name(operator_name)?;
    let operators = load_definitions(dir)?;
    let operator = operators
        .get(operator_name)
        .ok_or_else(|| anyhow!("operator `{operator_name}` not found"))?;
    Ok(operator
        .channels
        .iter()
        .map(|(id, channel)| channel_summary(id.clone(), channel, Some(operator_name.to_string())))
        .collect())
}

pub fn list_channel_catalog(
    dir: &std::path::Path,
) -> Result<Vec<construct_protocol::OperatorChannelSummary>> {
    let operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    let owners = channel_owners(&operators);
    Ok(catalog
        .channels
        .iter()
        .map(|(id, channel)| channel_summary(id.clone(), channel, owner_label(&owners, id)))
        .collect())
}

pub fn put_channel(
    dir: &std::path::Path,
    params: construct_protocol::OperatorChannelPutParams,
) -> Result<construct_protocol::OperatorChannelPutResult> {
    validate_operator_name(&params.operator_name)?;
    validate_channel_id(&params.channel.id)?;
    if !matches!(
        params.channel.kind.as_str(),
        "http" | "slack" | "slack-personal"
    ) {
        return Err(anyhow!(
            "unsupported channel kind `{}`",
            params.channel.kind
        ));
    }
    let port = match params.channel.kind.as_str() {
        "http" => Some(
            params
                .channel
                .port
                .filter(|port| *port > 0)
                .ok_or_else(|| anyhow!("HTTP channel port must be between 1 and 65535"))?,
        ),
        _ => None,
    };
    let mut operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    let owners = channel_owners(&operators);
    if let Some(owner) = owner_label(&owners, &params.channel.id) {
        if owner != params.operator_name {
            return Err(anyhow!(
                "channel `{}` is already attached to operator `{owner}`",
                params.channel.id
            ));
        }
    }
    if let Some(port) = port {
        if catalog.channels.iter().any(|(id, channel)| {
            id != &params.channel.id
                && channel_kind(id, channel) == "http"
                && channel.port == Some(port)
        }) {
            return Err(anyhow!("HTTP port {port} is already used by this operator"));
        }
    }
    let operator = operators
        .get_mut(&params.operator_name)
        .ok_or_else(|| anyhow!("operator `{}` not found", params.operator_name))?;
    let existing = operator
        .channels
        .get(&params.channel.id)
        .cloned()
        .or_else(|| catalog.channels.get(&params.channel.id).cloned());
    if let Some(existing) = &existing {
        let existing_kind = channel_kind(&params.channel.id, existing);
        if existing_kind != params.channel.kind {
            return Err(anyhow!(
                "channel `{}` cannot change kind from `{existing_kind}` to `{}`",
                params.channel.id,
                params.channel.kind
            ));
        }
    }
    let new_secret = if params.channel.kind == "http"
        && (params.rotate_secret
            || existing
                .as_ref()
                .and_then(|channel| channel.token.as_deref())
                .is_none())
    {
        Some(generate_channel_secret())
    } else {
        None
    };
    let token = (params.channel.kind == "http")
        .then(|| {
            new_secret
                .clone()
                .or_else(|| existing.as_ref().and_then(|channel| channel.token.clone()))
        })
        .flatten();
    let slack_tokens_provided =
        params.channel.app_token.is_some() || params.channel.bot_token.is_some();
    let app_token = params
        .channel
        .app_token
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|channel| channel.app_token.clone())
        });
    let bot_token = params
        .channel
        .bot_token
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|channel| channel.bot_token.clone())
        });
    // Refuse options a kind does not read (spec 0180): accepting one would
    // store a value nothing reads, and report it back as though in effect.
    match params.channel.kind.as_str() {
        "slack" => {
            validate_slack_token("app", app_token.as_deref(), "xapp-")?;
            validate_slack_token("bot", bot_token.as_deref(), "xoxb-")?;
            if params.channel.mcp_command.is_some()
                || params.channel.trigger.is_some()
                || params.channel.response_mode.is_some()
                || params.channel.auto_after_secs.is_some()
                || params.channel.disclosure.is_some()
                || params.channel.poll_interval_secs.is_some()
            {
                return Err(anyhow!(
                    "mcp_command, trigger, response_mode, auto_after_secs, disclosure, and poll_interval_secs apply to slack-personal channels only"
                ));
            }
        }
        "slack-personal" => {
            if slack_tokens_provided {
                return Err(anyhow!(
                    "app_token and bot_token apply to Slack bot channels only; slack-personal acts through its MCP backend"
                ));
            }
            if params.channel.progress.is_some() || params.channel.follow_up.is_some() {
                // The bot's progress affordances post as the bot; posting
                // "working on it" as the user is not an option this channel
                // offers, and follow-up is subsumed by its trigger policy.
                return Err(anyhow!(
                    "progress and follow_up apply to Slack bot channels only"
                ));
            }
            if let Some(interval) = params.channel.poll_interval_secs {
                if interval < SLACK_PERSONAL_POLL_MIN_SECS {
                    return Err(anyhow!(
                        "poll_interval_secs must be at least {SLACK_PERSONAL_POLL_MIN_SECS}"
                    ));
                }
            }
            if let Some(delay) = params.channel.auto_after_secs {
                if delay < construct_protocol::SLACK_PERSONAL_AUTO_AFTER_MIN_SECS {
                    return Err(anyhow!(
                        "auto_after_secs must be at least {}",
                        construct_protocol::SLACK_PERSONAL_AUTO_AFTER_MIN_SECS
                    ));
                }
            }
        }
        _ => {
            if params.channel.progress.is_some()
                || params.channel.follow_up.is_some()
                || params.channel.thread_context.is_some()
                || params.channel.mcp_command.is_some()
                || params.channel.trigger.is_some()
                || params.channel.response_mode.is_some()
                || params.channel.auto_after_secs.is_some()
                || params.channel.disclosure.is_some()
                || params.channel.poll_interval_secs.is_some()
            {
                return Err(anyhow!(
                    "progress, follow_up, thread_context, and slack-personal options apply to Slack channels only"
                ));
            }
        }
    }
    // Preserve-on-omit, like the token fields: a client that does not offer
    // these fields must not reset them by saving an unrelated one.
    let mcp_command = params
        .channel
        .mcp_command
        .filter(|command| !command.trim().is_empty())
        .or_else(|| existing.as_ref().and_then(|channel| channel.mcp_command.clone()));
    let trigger = match params.channel.trigger.as_deref() {
        Some(value) => Some(SlackPersonalTrigger::parse(value)?),
        None => existing.as_ref().and_then(|channel| channel.trigger),
    };
    let response_mode = match params.channel.response_mode.as_deref() {
        Some(value) => Some(SlackPersonalResponse::parse(value)?),
        None => existing.as_ref().and_then(|channel| channel.response_mode),
    };
    let auto_after_secs = params.channel.auto_after_secs.or_else(|| {
        existing
            .as_ref()
            .and_then(|channel| channel.auto_after_secs)
    });
    let disclosure = params
        .channel
        .disclosure
        .or_else(|| existing.as_ref().and_then(|channel| channel.disclosure));
    let poll_interval_secs = params
        .channel
        .poll_interval_secs
        .or_else(|| existing.as_ref().and_then(|channel| channel.poll_interval_secs));
    if params.channel.kind == "slack-personal"
        && mcp_command.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        return Err(anyhow!(
            "slack-personal channels need an mcp_command that starts their MCP backend"
        ));
    }
    // An omitted option keeps what is stored: a client that does not offer
    // these fields must not reset them by saving an unrelated one.
    let progress = match params.channel.progress.as_deref() {
        Some(value) => SlackProgress::parse(value)?,
        None => existing
            .as_ref()
            .map(|channel| channel.progress)
            .unwrap_or_default(),
    };
    let follow_up = match params.channel.follow_up.as_deref() {
        Some(value) => SlackFollowUp::parse(value)?,
        None => existing
            .as_ref()
            .map(|channel| channel.follow_up)
            .unwrap_or_default(),
    };
    let thread_context = match params.channel.thread_context {
        Some(value) if value > THREAD_CONTEXT_MAX => {
            return Err(anyhow!(
                "thread_context must be at most {THREAD_CONTEXT_MAX}; Slack returns no more in one page"
            ));
        }
        Some(value) => value,
        None => existing
            .as_ref()
            .map(|channel| channel.thread_context)
            .unwrap_or_else(default_thread_context),
    };
    let config = OperatorChannelConfig {
        kind: Some(params.channel.kind),
        enabled: params.channel.enabled,
        port,
        token,
        app_token,
        bot_token,
        allowed_workspaces: normalize_allowlist(params.channel.allowed_workspaces),
        allowed_channels: normalize_allowlist(params.channel.allowed_channels),
        progress,
        follow_up,
        thread_context,
        mcp_command,
        trigger,
        response_mode,
        auto_after_secs,
        disclosure,
        poll_interval_secs,
    };
    operator
        .channels
        .insert(params.channel.id.clone(), config.clone());
    catalog
        .channels
        .insert(params.channel.id.clone(), config.clone());
    let summary = channel_summary(
        params.channel.id,
        &config,
        Some(params.operator_name.clone()),
    );
    write_definition(dir, &params.operator_name, operator)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::OperatorChannelPutResult {
        channel: summary,
        new_secret,
        applied: Default::default(),
    })
}

/// Remove a channel from the catalog for good, detaching it from the caller's
/// operator first when it is attached there. Deletion is honest — unlike
/// [`detach_channel`], the channel does not survive as an available catalog
/// entry. A channel owned by another operator is refused rather than stolen.
pub fn delete_channel(
    dir: &std::path::Path,
    params: construct_protocol::OperatorChannelNameParams,
) -> Result<()> {
    validate_operator_name(&params.operator_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    if !catalog.channels.contains_key(&params.channel_id) {
        return Err(anyhow!(
            "channel `{}` not found in catalog",
            params.channel_id
        ));
    }
    let owners = channel_owners(&operators);
    if let Some(owner) = owner_label(&owners, &params.channel_id) {
        if owner != params.operator_name {
            return Err(anyhow!(
                "channel `{}` is attached to operator `{owner}`; delete it from there",
                params.channel_id
            ));
        }
        let operator = operators
            .get_mut(&params.operator_name)
            .ok_or_else(|| anyhow!("operator `{}` not found", params.operator_name))?;
        operator.channels.remove(&params.channel_id);
        write_definition(dir, &params.operator_name, operator)?;
    }
    catalog.channels.remove(&params.channel_id);
    write_channel_catalog(dir, &catalog)?;
    Ok(())
}

pub fn attach_channel(
    dir: &std::path::Path,
    params: construct_protocol::OperatorChannelAttachParams,
) -> Result<construct_protocol::OperatorChannelPutResult> {
    validate_operator_name(&params.operator_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    let owners = channel_owners(&operators);
    if let Some(owner) = owner_label(&owners, &params.channel_id) {
        if owner != params.operator_name {
            return Err(anyhow!(
                "channel `{}` is already attached to operator `{owner}`",
                params.channel_id
            ));
        }
    }
    let config = catalog
        .channels
        .get(&params.channel_id)
        .cloned()
        .ok_or_else(|| anyhow!("channel `{}` not found in catalog", params.channel_id))?;
    let operator = operators
        .get_mut(&params.operator_name)
        .ok_or_else(|| anyhow!("operator `{}` not found", params.operator_name))?;
    if channel_kind(&params.channel_id, &config) == "http" {
        if operator.channels.iter().any(|(id, channel)| {
            id != &params.channel_id
                && channel_kind(id, channel) == "http"
                && channel.port == config.port
        }) {
            return Err(anyhow!(
                "HTTP port {:?} is already used by this operator",
                config.port
            ));
        }
    }
    operator
        .channels
        .insert(params.channel_id.clone(), config.clone());
    write_definition(dir, &params.operator_name, operator)?;
    Ok(construct_protocol::OperatorChannelPutResult {
        channel: channel_summary(params.channel_id, &config, Some(params.operator_name)),
        new_secret: None,
        applied: Default::default(),
    })
}

pub fn detach_channel(
    dir: &std::path::Path,
    params: construct_protocol::OperatorChannelAttachParams,
) -> Result<construct_protocol::OperatorChannelPutResult> {
    validate_operator_name(&params.operator_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    let operator = operators
        .get_mut(&params.operator_name)
        .ok_or_else(|| anyhow!("operator `{}` not found", params.operator_name))?;
    let config = operator.channels.remove(&params.channel_id).ok_or_else(|| {
        anyhow!(
            "channel `{}` is not attached to operator `{}`",
            params.channel_id,
            params.operator_name
        )
    })?;
    catalog
        .channels
        .entry(params.channel_id.clone())
        .or_insert_with(|| config.clone());
    write_definition(dir, &params.operator_name, operator)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::OperatorChannelPutResult {
        channel: channel_summary(params.channel_id, &config, None),
        new_secret: None,
        applied: Default::default(),
    })
}

pub fn rotate_channel_secret(
    dir: &std::path::Path,
    params: construct_protocol::OperatorChannelNameParams,
) -> Result<construct_protocol::OperatorChannelPutResult> {
    validate_operator_name(&params.operator_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut operators = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &operators, &mut catalog)?;
    let operator = operators
        .get_mut(&params.operator_name)
        .ok_or_else(|| anyhow!("operator `{}` not found", params.operator_name))?;
    let channel = operator
        .channels
        .get_mut(&params.channel_id)
        .ok_or_else(|| {
            anyhow!(
                "channel `{}` not found on operator `{}`",
                params.channel_id,
                params.operator_name
            )
        })?;
    if channel_kind(&params.channel_id, channel) != "http" {
        return Err(anyhow!(
            "only HTTP channel credentials can be rotated in v1"
        ));
    }
    let secret = generate_channel_secret();
    channel.token = Some(secret.clone());
    catalog
        .channels
        .insert(params.channel_id.clone(), channel.clone());
    let config = channel.clone();
    let summary = channel_summary(
        params.channel_id.clone(),
        &config,
        Some(params.operator_name.clone()),
    );
    write_definition(dir, &params.operator_name, operator)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::OperatorChannelPutResult {
        channel: summary,
        new_secret: Some(secret),
        applied: Default::default(),
    })
}

fn write_definition(dir: &std::path::Path, name: &str, config: &OperatorConfig) -> Result<()> {
    let path = dir.join(format!("{name}.toml"));
    let encoded = toml::to_string_pretty(config)?;
    let temporary = dir.join(format!(".{name}.toml.tmp"));
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn channel_kind(id: &str, config: &OperatorChannelConfig) -> String {
    config
        .kind
        .clone()
        .unwrap_or_else(|| if id == "http" { "http" } else { "unknown" }.to_string())
}

fn validate_channel_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(anyhow!("invalid channel id `{id}`"))
    }
}

fn generate_channel_secret() -> String {
    format!("cst_{}", Uuid::new_v4().simple())
}

fn validate_slack_token(label: &str, token: Option<&str>, prefix: &str) -> Result<()> {
    match token {
        Some(token) if token.starts_with(prefix) => Ok(()),
        _ => Err(anyhow!("Slack {label} token must start with `{prefix}`")),
    }
}

fn normalize_allowlist(values: Vec<String>) -> Vec<String> {
    let mut values: Vec<_> = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort();
    values.dedup();
    values
}

fn channel_summary(
    id: String,
    config: &OperatorChannelConfig,
    attached_to: Option<String>,
) -> construct_protocol::OperatorChannelSummary {
    // Behavior options belong to their kind, and a client that cannot see the
    // stored value cannot show what an omitted field is preserving.
    let kind = channel_kind(&id, config);
    let slack = kind == "slack";
    let personal = kind == "slack-personal";
    construct_protocol::OperatorChannelSummary {
        id,
        kind: kind.clone(),
        enabled: config.enabled,
        port: config.port,
        has_credential: match kind.as_str() {
            "slack" => {
                config
                    .app_token
                    .as_ref()
                    .is_some_and(|token| !token.is_empty())
                    && config
                        .bot_token
                        .as_ref()
                        .is_some_and(|token| !token.is_empty())
            }
            "slack-personal" => config
                .mcp_command
                .as_ref()
                .is_some_and(|command| !command.trim().is_empty()),
            _ => config.token.as_ref().is_some_and(|token| !token.is_empty()),
        },
        has_app_token: config
            .app_token
            .as_ref()
            .is_some_and(|token| !token.is_empty()),
        has_bot_token: config
            .bot_token
            .as_ref()
            .is_some_and(|token| !token.is_empty()),
        allowed_workspace_count: config.allowed_workspaces.len(),
        allowed_channel_count: config.allowed_channels.len(),
        allowed_workspaces: config.allowed_workspaces.clone(),
        allowed_channels: config.allowed_channels.clone(),
        progress: slack.then(|| config.progress.as_str().to_string()),
        follow_up: slack.then(|| config.follow_up.as_str().to_string()),
        // Both Slack kinds read thread context when first pulled into a
        // conversation already in progress.
        thread_context: (slack || personal).then_some(config.thread_context),
        mcp_command: personal.then(|| config.mcp_command.clone().unwrap_or_default()),
        trigger: personal.then(|| config.trigger.unwrap_or_default().as_str().to_string()),
        response_mode: personal.then(|| {
            config
                .response_mode
                .unwrap_or_default()
                .as_str()
                .to_string()
        }),
        auto_after_secs: personal.then(|| {
            config
                .auto_after_secs
                .unwrap_or(construct_protocol::SLACK_PERSONAL_AUTO_AFTER_DEFAULT_SECS)
        }),
        disclosure: personal.then(|| config.disclosure.unwrap_or(true)),
        poll_interval_secs: personal
            .then(|| config.poll_interval_secs.unwrap_or(SLACK_PERSONAL_POLL_DEFAULT_SECS)),
        attached_to,
        publication: None,
    }
}

fn summary(name: String, config: &OperatorConfig) -> construct_protocol::OperatorSummary {
    let operator_name = name.clone();
    construct_protocol::OperatorSummary {
        name,
        position: config.position,
        placement: config.placement,
        instruction: config.instruction.clone(),
        harness: config.harness.clone(),
        model: config.model.clone(),
        session_mode: config.session_mode.as_str().to_string(),
        cwd: config.cwd.clone(),
        routing: match config.routing {
            OperatorRouting::PerEvent => "per-event",
            OperatorRouting::SessionKey => "session-key",
            OperatorRouting::Single => "single",
        }
        .to_string(),
        paused: config.paused,
        channels: config
            .channels
            .iter()
            .map(|(id, channel)| channel_summary(id.clone(), channel, Some(operator_name.clone())))
            .collect(),
    }
}

fn validate_operator_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(anyhow!("invalid operator name `{name}`"))
    }
}

pub(crate) use ingress::{OperatorIngress as OperatorRuntime, OperatorIngressShared as OperatorShared};
pub(crate) use slack::SlackConfig;
pub(crate) use slack_personal::SlackPersonalConfig;

/// Build the transport-neutral ingress runtime for one channel.
pub(crate) fn channel_runtime(
    shared: Arc<OperatorShared>,
    channel_id: String,
) -> Arc<OperatorRuntime> {
    Arc::new(OperatorRuntime::new(channel_id, shared))
}

/// Whether a channel is one this daemon knows how to bind, logging the reason
/// when it is not. Used by the supervisor to build the desired listener set.
pub(crate) fn bindable_port(
    operator: &str,
    channel_id: &str,
    channel: &OperatorChannelConfig,
) -> Option<u16> {
    if !channel.enabled {
        return None;
    }
    let kind = channel_kind(channel_id, channel);
    if kind == "slack" || kind == "slack-personal" {
        return None;
    }
    if kind != "http" {
        tracing::warn!(operator = %operator, channel = %channel_id, "unsupported operator channel kind; skipping");
        return None;
    }
    let Some(port) = channel.port else {
        tracing::warn!(operator = %operator, channel = %channel_id, "HTTP channel has no port; skipping");
        return None;
    };
    if channel.token.as_deref().unwrap_or("").is_empty() {
        tracing::warn!(operator = %operator, channel = %channel_id, "HTTP channel has no token; skipping");
        return None;
    }
    Some(port)
}

/// Describe the local ingress this channel adapter owns. Publication code
/// consumes this typed endpoint and never inspects `OperatorChannelConfig`.
/// A future channel kind adds its adapter mapping here (or in a registry)
/// without adding protocol branches to the tunnel supervisor.
pub(crate) fn ingress_endpoint(
    operator: &str,
    channel_id: &str,
    channel: &OperatorChannelConfig,
) -> Option<crate::channel_publication::ChannelIngressEndpoint> {
    bindable_port(operator, channel_id, channel).map(|port| {
        crate::channel_publication::ChannelIngressEndpoint::loopback_http(
            port,
            format!("/svc/{operator}"),
        )
    })
}

/// Drive one supervisor-owned HTTP listener until it is cancelled.
pub(crate) async fn serve(
    runtime: Arc<OperatorRuntime>,
    listener: tokio::net::TcpListener,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    http::serve(runtime, listener, cancel).await
}

/// Validate and snapshot one Slack channel without exposing its credentials.
pub(crate) fn slack_config(
    operator: &str,
    channel_id: &str,
    channel: &OperatorChannelConfig,
) -> Option<slack::SlackConfig> {
    if !channel.enabled || channel_kind(channel_id, channel) != "slack" {
        return None;
    }
    let app_token = channel
        .app_token
        .clone()
        .filter(|token| token.starts_with("xapp-"));
    let bot_token = channel
        .bot_token
        .clone()
        .filter(|token| token.starts_with("xoxb-"));
    let (Some(app_token), Some(bot_token)) = (app_token, bot_token) else {
        tracing::warn!(operator = %operator, channel = %channel_id, "Slack channel credentials are missing or invalid; skipping");
        return None;
    };
    Some(slack::SlackConfig {
        app_token,
        bot_token,
        allowed_workspaces: channel.allowed_workspaces.clone(),
        allowed_channels: channel.allowed_channels.clone(),
        progress: channel.progress,
        follow_up: channel.follow_up,
        thread_context: channel.thread_context,
    })
}

pub(crate) async fn serve_slack(
    runtime: Arc<OperatorRuntime>,
    config: slack::SlackConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    slack::serve(runtime, config, cancel).await
}

/// Snapshot one slack-personal channel for its outbound polling task.
pub(crate) fn slack_personal_config(
    operator: &str,
    channel_id: &str,
    channel: &OperatorChannelConfig,
) -> Option<slack_personal::SlackPersonalConfig> {
    if !channel.enabled || channel_kind(channel_id, channel) != "slack-personal" {
        return None;
    }
    let mcp_command = channel
        .mcp_command
        .clone()
        .filter(|command| !command.trim().is_empty());
    let Some(mcp_command) = mcp_command else {
        tracing::warn!(operator = %operator, channel = %channel_id, "slack-personal channel has no MCP command; skipping");
        return None;
    };
    let poll_secs = channel
        .poll_interval_secs
        .unwrap_or(SLACK_PERSONAL_POLL_DEFAULT_SECS)
        .max(SLACK_PERSONAL_POLL_MIN_SECS);
    Some(slack_personal::SlackPersonalConfig {
        mcp_command,
        poll_interval: std::time::Duration::from_secs(poll_secs),
        trigger: channel.trigger.unwrap_or_default(),
        response: channel.response_mode.unwrap_or_default(),
        auto_after: std::time::Duration::from_secs(
            channel
                .auto_after_secs
                .unwrap_or(construct_protocol::SLACK_PERSONAL_AUTO_AFTER_DEFAULT_SECS),
        ),
        disclosure: channel.disclosure.unwrap_or(true),
        allowed_workspaces: channel.allowed_workspaces.clone(),
        allowed_channels: channel.allowed_channels.clone(),
        thread_context: channel.thread_context,
    })
}

pub(crate) async fn serve_slack_personal(
    runtime: Arc<OperatorRuntime>,
    config: slack_personal::SlackPersonalConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    slack_personal::serve(runtime, config, cancel).await
}

#[cfg(test)]
mod tests {
    use super::http::{content_length, find_headers_end, parse_http_route, HttpRoute};
    use super::ingress::{latest_assistant_reply, pending_approval, PersistedState};
    use super::*;
    use construct_protocol::{MessageRole, SessionEvent, SessionState};

    /// A live operator plus one channel runtime, so the tests below read
    /// configuration the way a request does rather than inspecting the struct.
    async fn live_operator(config: OperatorConfig) -> (Arc<OperatorShared>, Arc<OperatorRuntime>) {
        let tmp = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let (manager, _remote_rx, _restart_rx) = crate::session::SessionManager::new(
            storage,
            Arc::new(crate::config::Config::default()),
            tmp.path().join("run"),
        )
        .await
        .expect("session manager");
        let shared = OperatorShared::load(
            "svc".to_string(),
            config,
            Arc::new(manager),
            tmp.path().join("data"),
        );
        let runtime = channel_runtime(shared.clone(), "http1".to_string());
        (shared, runtime)
    }

    fn config_with_channel(token: &str, enabled: bool, paused: bool) -> OperatorConfig {
        OperatorConfig {
            position: 0,
            placement: None,
            instruction: String::new(),
            harness: "smith".into(),
            model: None,
            session_mode: OperatorSessionMode::Headless,
            cwd: ".".into(),
            routing: OperatorRouting::SessionKey,
            paused,
            approval_timeout_secs: 0,
            sandbox: OperatorSandboxConfig::default(),
            channels: BTreeMap::from([(
                "http1".to_string(),
                OperatorChannelConfig {
                    kind: Some("http".into()),
                    enabled,
                    port: Some(9000),
                    token: Some(token.to_string()),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: Default::default(),
                    follow_up: Default::default(),
                    thread_context: default_thread_context(),
                    ..Default::default()
                },
            )]),
        }
    }

    #[test]
    fn interactive_mode_is_limited_to_native_agent_harnesses() {
        assert_eq!(
            parse_operator_session_mode("codex", "interactive").unwrap(),
            OperatorSessionMode::Interactive
        );
        assert_eq!(
            parse_operator_session_mode("claude", "interactive").unwrap(),
            OperatorSessionMode::Interactive
        );
        assert!(parse_operator_session_mode("smith", "interactive")
            .unwrap_err()
            .to_string()
            .contains("codex` or `claude"));
        assert!(parse_operator_session_mode("codex", "unknown").is_err());
    }

    #[tokio::test]
    async fn a_rotated_credential_takes_effect_without_rebinding() {
        // The listener never moves for a rotation, so the credential has to be
        // read per request. Before this, the old secret kept working until the
        // daemon restarted.
        let (shared, runtime) = live_operator(config_with_channel("first", true, false)).await;
        assert_eq!(http::token(&runtime).as_deref(), Some("first"));

        shared.set_config(config_with_channel("second", true, false));
        assert_eq!(
            http::token(&runtime).as_deref(),
            Some("second"),
            "the next request authenticates against the rotated secret"
        );

        // A channel detached out from under a live listener can authenticate
        // nobody, rather than falling back to its previous secret.
        let mut detached = config_with_channel("second", true, false);
        detached.channels.clear();
        shared.set_config(detached);
        assert_eq!(http::token(&runtime), None);
    }

    #[tokio::test]
    async fn pausing_or_disabling_stops_a_channel_serving() {
        let (shared, runtime) = live_operator(config_with_channel("t", true, false)).await;
        assert!(http::serving(&runtime));

        shared.set_config(config_with_channel("t", true, true));
        assert!(
            !http::serving(&runtime),
            "a paused operator refuses requests"
        );

        shared.set_config(config_with_channel("t", false, false));
        assert!(
            !http::serving(&runtime),
            "a disabled channel refuses requests"
        );

        shared.set_config(config_with_channel("t", true, false));
        assert!(http::serving(&runtime), "resuming serves again");
    }

    #[tokio::test]
    async fn a_definition_change_keeps_routed_sessions_and_delivery_history() {
        // Editing a definition must not lose which session serves a key, nor
        // which deliveries were already handled — the first would strand live
        // conversations, the second would let a retry open a duplicate.
        let (shared, _runtime) = live_operator(config_with_channel("t", true, false)).await;
        shared
            .state
            .lock()
            .await
            .sessions
            .insert("http1:customer".into(), "s-existing".into());
        {
            let mut seen = shared.seen_requests.lock().await;
            seen.0.push_back("http1:req-1".into());
            seen.1.insert("http1:req-1".into());
        }

        let mut edited = config_with_channel("t", true, false);
        edited.routing = OperatorRouting::PerEvent;
        shared.set_config(edited);

        assert_eq!(
            shared.state.lock().await.sessions.get("http1:customer"),
            Some(&"s-existing".to_string()),
            "the routed session survives the edit"
        );
        assert!(
            shared.seen_requests.lock().await.1.contains("http1:req-1"),
            "an already-handled delivery is still recognized after the edit"
        );
        assert_eq!(shared.config().routing, OperatorRouting::PerEvent);
    }

    fn msg(role: MessageRole, text: &str) -> SessionEvent {
        SessionEvent::Message {
            role,
            text: text.to_string(),
        }
    }

    fn status(state: SessionState) -> SessionEvent {
        SessionEvent::Status {
            state,
            detail: None,
        }
    }

    #[test]
    fn reply_joins_streamed_assistant_deltas() {
        // Transcript shape observed from a live smith turn: the answer arrives
        // as one Message event per token delta, wrapped in bookkeeping events.
        let events = vec![
            msg(MessageRole::User, "What is 2 plus 2?"),
            status(SessionState::Running),
            msg(MessageRole::Assistant, "2"),
            msg(MessageRole::Assistant, " plus"),
            msg(MessageRole::Assistant, " "),
            msg(MessageRole::Assistant, "2"),
            msg(MessageRole::Assistant, " is"),
            msg(MessageRole::Assistant, " "),
            msg(MessageRole::Assistant, "4"),
            msg(MessageRole::Assistant, "."),
            status(SessionState::AwaitingInput),
        ];
        assert_eq!(
            latest_assistant_reply(events.iter()),
            Some("2 plus 2 is 4.".to_string())
        );
    }

    #[test]
    fn reply_covers_only_the_latest_turn() {
        let events = vec![
            msg(MessageRole::User, "first"),
            msg(MessageRole::Assistant, "old answer"),
            msg(MessageRole::User, "second"),
            msg(MessageRole::Assistant, "new "),
            msg(MessageRole::Assistant, "answer"),
        ];
        assert_eq!(
            latest_assistant_reply(events.iter()),
            Some("new answer".to_string())
        );
    }

    #[test]
    fn reply_after_tool_use_excludes_preceding_narration() {
        let events = vec![
            msg(MessageRole::User, "check the disk"),
            msg(MessageRole::Assistant, "Let me look."),
            SessionEvent::ToolUse {
                tool: "bash".into(),
                args: serde_json::Value::Null,
                call_id: None,
            },
            SessionEvent::ToolResult {
                tool: "bash".into(),
                ok: true,
                output: "42%".into(),
                call_id: None,
            },
            msg(MessageRole::Assistant, "Disk is "),
            msg(MessageRole::Assistant, "42% full."),
        ];
        assert_eq!(
            latest_assistant_reply(events.iter()),
            Some("Disk is 42% full.".to_string())
        );
    }

    #[test]
    fn reply_survives_a_tool_result_recorded_after_the_answer() {
        // Transcript shape observed from a live interactive codex operator
        // turn: codex flushed the reply tool's result *after* the assistant
        // text it produced. Treating that trailing result as a turn boundary
        // hid the answer entirely — the poll endpoint reported `ready` with a
        // null reply, and a waiting caller blocked to its timeout.
        let events = vec![
            msg(MessageRole::User, "say CHARLIE"),
            SessionEvent::ToolUse {
                tool: "construct_operator_reply".into(),
                args: serde_json::Value::Null,
                call_id: Some("call-1".into()),
            },
            msg(MessageRole::Assistant, "CHARLIE"),
            SessionEvent::ToolResult {
                tool: "construct_operator_reply".into(),
                ok: true,
                output: String::new(),
                call_id: Some("call-1".into()),
            },
            status(SessionState::AwaitingInput),
        ];
        assert_eq!(
            latest_assistant_reply(events.iter()),
            Some("CHARLIE".to_string())
        );
    }

    #[test]
    fn a_turn_that_ends_inside_a_tool_call_reports_no_reply() {
        // The trailing-result skip must not reach back past the tool call
        // itself and serve pre-tool narration as the final answer.
        let events = vec![
            msg(MessageRole::User, "check the disk"),
            msg(MessageRole::Assistant, "Let me look."),
            SessionEvent::ToolUse {
                tool: "bash".into(),
                args: serde_json::Value::Null,
                call_id: None,
            },
            SessionEvent::ToolResult {
                tool: "bash".into(),
                ok: true,
                output: "42%".into(),
                call_id: None,
            },
            status(SessionState::AwaitingInput),
        ];
        assert_eq!(latest_assistant_reply(events.iter()), None);
    }

    #[test]
    fn reply_is_absent_before_the_assistant_speaks() {
        let events = vec![
            msg(MessageRole::User, "hello"),
            status(SessionState::Running),
        ];
        assert_eq!(latest_assistant_reply(events.iter()), None);
        assert_eq!(latest_assistant_reply([].iter()), None);
    }

    #[test]
    fn operators_withhold_fleet_access_unless_asked() {
        // The default matters on its own: a operator is prompted by whoever can
        // reach its channel, so an omitted [sandbox] section must not hand that
        // caller the fleet.
        let env = OperatorSandboxConfig::default().session_env();
        assert_eq!(
            env.get("CONSTRUCT_SMITH_FLEET_TOOLS").map(String::as_str),
            Some("off")
        );
        assert_eq!(
            env.get("CONSTRUCT_INJECT_MCP").map(String::as_str),
            Some("0")
        );
        assert!(!env.contains_key("CONSTRUCT_SMITH_SKILLS"));

        let opened = OperatorSandboxConfig {
            fleet_control: true,
            mcp: true,
            skills: false,
        };
        let env = opened.session_env();
        assert!(!env.contains_key("CONSTRUCT_SMITH_FLEET_TOOLS"));
        assert!(!env.contains_key("CONSTRUCT_INJECT_MCP"));
        assert_eq!(
            env.get("CONSTRUCT_SMITH_SKILLS").map(String::as_str),
            Some("off")
        );
    }

    #[test]
    fn sandbox_limits_survive_an_unrelated_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = OperatorConfig {
            position: 0,
            placement: None,
            instruction: "hi".into(),
            harness: "smith".into(),
            model: None,
            session_mode: OperatorSessionMode::Headless,
            cwd: ".".into(),
            routing: OperatorRouting::SessionKey,
            paused: false,
            approval_timeout_secs: 0,
            sandbox: OperatorSandboxConfig::default(),
            channels: BTreeMap::new(),
        };
        config.sandbox.fleet_control = true;
        write_definition(dir.path(), "svc", &config).unwrap();

        // An edit that never mentions the sandbox must not re-confine (or
        // re-open) the operator behind the user's back.
        put_definition(
            dir.path(),
            construct_protocol::OperatorPutParams {
                operator: construct_protocol::OperatorSummary {
                    name: "svc".into(),
                    position: 0,
                    placement: None,
                    instruction: "changed".into(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();

        let stored = load_definitions(dir.path()).unwrap();
        assert!(stored["svc"].sandbox.fleet_control);
        assert_eq!(stored["svc"].instruction, "changed");
    }

    fn at(seq: u64, event: SessionEvent) -> construct_protocol::TimestampedEvent {
        construct_protocol::TimestampedEvent {
            seq,
            at: chrono::Utc::now(),
            event,
        }
    }

    fn approval_request(call_id: &str, tool: &str) -> SessionEvent {
        SessionEvent::ToolApprovalRequest {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            args_summary: "…".to_string(),
            risk: construct_protocol::ToolRisk::Risky,
            allow_auto_review: true,
        }
    }

    #[test]
    fn a_trailing_approval_request_is_pending() {
        let events = vec![
            at(0, msg(MessageRole::User, "do it")),
            at(
                1,
                SessionEvent::ToolUse {
                    tool: "shell".into(),
                    args: serde_json::Value::Null,
                    call_id: Some("c1".into()),
                },
            ),
            at(2, approval_request("c1", "shell")),
            at(3, status(SessionState::Running)),
        ];
        let pending = pending_approval(&events).expect("pending");
        assert_eq!(pending.call_id, "c1");
        assert_eq!(pending.tool, "shell");
    }

    #[test]
    fn an_answered_approval_is_not_pending() {
        // Resolutions are never written to the transcript, so what marks this
        // one answered is the work that followed it.
        let events = vec![
            at(0, msg(MessageRole::User, "do it")),
            at(1, approval_request("c1", "shell")),
            at(
                2,
                SessionEvent::ToolResult {
                    tool: "shell".into(),
                    ok: true,
                    output: "done".into(),
                    call_id: Some("c1".into()),
                },
            ),
            at(3, msg(MessageRole::Assistant, "Done.")),
        ];
        assert!(pending_approval(&events).is_none());
    }

    #[test]
    fn a_turn_with_no_approval_is_not_pending() {
        let events = vec![
            at(0, msg(MessageRole::User, "hi")),
            at(1, msg(MessageRole::Assistant, "hello")),
        ];
        assert!(pending_approval(&events).is_none());
        assert!(pending_approval(&[]).is_none());
    }

    #[test]
    fn approval_timeout_defaults_to_waiting_forever() {
        // The user stays the only one who can approve unless they opt into
        // a bound; turning this on by default would deny work nobody refused.
        let raw = "instruction = \"x\"\nharness = \"smith\"\ncwd = \".\"\n";
        let config: OperatorConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.approval_timeout_secs, 0);

        let bounded: OperatorConfig =
            toml::from_str(&format!("{raw}approval_timeout_secs = 120\n")).unwrap();
        assert_eq!(bounded.approval_timeout_secs, 120);
    }

    #[test]
    fn header_boundary_and_content_length() {
        let request = b"POST / HTTP/1.1\r\nContent-Length: 12\r\n\r\nhello world!";
        let end = find_headers_end(request).unwrap();
        assert_eq!(content_length(&request[..end]).unwrap(), 12);
    }

    #[test]
    fn http_routes_distinguish_submit_result_method_and_wrong_operator() {
        assert_eq!(
            parse_http_route("alerts", "POST /svc/alerts HTTP/1.1"),
            Ok(HttpRoute::Submit)
        );
        assert_eq!(
            parse_http_route("alerts", "GET /svc/alerts/sessions/s123 HTTP/1.1"),
            Ok(HttpRoute::Session("s123".to_string()))
        );
        assert_eq!(
            parse_http_route("alerts", "GET /svc/alerts HTTP/1.1"),
            Err((405, "POST required"))
        );
        assert_eq!(
            parse_http_route("alerts", "POST /svc/alerts/sessions/s123 HTTP/1.1"),
            Err((405, "GET required"))
        );
        assert_eq!(
            parse_http_route("alerts", "POST /svc/other HTTP/1.1"),
            Err((404, "not found"))
        );
    }

    #[test]
    fn legacy_keyed_sessions_become_operator_owned() {
        let mut state: PersistedState =
            serde_json::from_str(r#"{"sessions":{"incident-1":"s123"}}"#).unwrap();
        assert!(state.owned_sessions.is_empty());
        state.normalize_legacy_ownership();
        assert!(state.owned_sessions.contains("s123"));
    }

    #[test]
    fn operator_config_accepts_v1_routing_mode() {
        let operator: OperatorConfig = toml::from_str(
            r#"
            instruction = "triage alert"
            harness = "smith"
            routing = "session-key"
            [channels.http]
            port = 8787
            token = "secret"
            "#,
        )
        .unwrap();
        assert_eq!(operator.routing, OperatorRouting::SessionKey);
        assert_eq!(operator.channels["http"].port, Some(8787));
    }

    #[test]
    fn loads_one_toml_document_per_operator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("alerts.toml"),
            "harness = \"smith\"\n[channels.http]\nport = 8787\ntoken = \"secret\"\n",
        )
        .unwrap();
        let operators = load_definitions(dir.path()).unwrap();
        assert_eq!(operators.len(), 1);
        assert_eq!(operators["alerts"].channels["http"].port, Some(8787));
    }

    #[test]
    fn operator_reorder_materializes_and_preserves_positions() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["alpha", "bravo", "charlie"] {
            std::fs::write(
                dir.path().join(format!("{name}.toml")),
                "harness = \"smith\"\n",
            )
            .unwrap();
        }

        let names = || {
            list_summaries(dir.path())
                .unwrap()
                .into_iter()
                .map(|operator| operator.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(), ["alpha", "bravo", "charlie"]);

        move_definition(
            dir.path(),
            "charlie",
            construct_protocol::MoveDirection::Up,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(names(), ["alpha", "charlie", "bravo"]);
        assert_eq!(
            list_summaries(dir.path())
                .unwrap()
                .iter()
                .map(|operator| operator.position)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );

        let mut edited = list_summaries(dir.path()).unwrap()[1].clone();
        edited.instruction = "changed".into();
        edited.position = 99;
        put_definition(
            dir.path(),
            construct_protocol::OperatorPutParams { operator: edited },
        )
        .unwrap();
        assert_eq!(names(), ["alpha", "charlie", "bravo"]);
    }

    fn flow_session(id: &str, position: i64) -> construct_protocol::SessionSummary {
        construct_protocol::SessionSummary {
            id: id.to_string(),
            harness: "smith".to_string(),
            cwd: "/tmp".to_string(),
            title: None,
            auto_title_pending: false,
            state: construct_protocol::SessionState::Running,
            created_at: "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            last_event_at: None,
            last_message_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            route: None,
            route_capable: false,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            last_message_role: None,
            last_message: None,
            last_error: None,
            event_count: 0,
            has_pty: true,
            mode: Some("interactive".to_string()),
            pinned: false,
            position,
            group_id: None,
            parent_session_id: None,
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: Default::default(),
            context_used: None,
            context_window: None,
            context_segments: Vec::new(),
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind: construct_protocol::SessionKind::User,
            archived: false,
            minibuffer_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    fn flow_project(id: &str, position: i64) -> construct_protocol::GroupSummary {
        construct_protocol::GroupSummary {
            id: id.to_string(),
            name: id.to_string(),
            created_at: "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            position,
            collapsed: false,
        }
    }

    /// The full display flow, as reconstructed from persisted state, so the
    /// assertions cover exactly what a list client would render.
    fn flow_names(
        dir: &std::path::Path,
        sessions: &[construct_protocol::SessionSummary],
        groups: &[construct_protocol::GroupSummary],
    ) -> Vec<String> {
        let operators = load_definitions(dir).unwrap();
        super::top_level_flow(&operators, sessions, groups)
            .into_iter()
            .map(|row| match row {
                super::FlowRow::Operator(name) => format!("svc:{name}"),
                super::FlowRow::Session { position } => format!("sess@{position}"),
                super::FlowRow::Project { position } => format!("proj@{position}"),
            })
            .collect()
    }

    #[test]
    fn operator_reorders_across_sessions_and_projects_at_top_level() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["alpha", "bravo"] {
            std::fs::write(
                dir.path().join(format!("{name}.toml")),
                "harness = \"smith\"\n",
            )
            .unwrap();
        }
        let sessions = vec![flow_session("s1", 10), flow_session("s2", 20)];
        let groups = vec![flow_project("p1", 5), flow_project("p2", 6)];
        let down = |name: &str, sessions: &_, groups: &_| {
            move_definition(
                dir.path(),
                name,
                construct_protocol::MoveDirection::Down,
                sessions,
                groups,
            )
            .unwrap()
        };
        let up = |name: &str, sessions: &_, groups: &_| {
            move_definition(
                dir.path(),
                name,
                construct_protocol::MoveDirection::Up,
                sessions,
                groups,
            )
            .unwrap()
        };

        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "svc:bravo", "sess@10", "sess@20", "proj@5", "proj@6"]
        );

        // Walk bravo all the way to the bottom: past each session, then over
        // each whole project block.
        down("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "sess@10", "svc:bravo", "sess@20", "proj@5", "proj@6"]
        );
        down("bravo", &sessions, &groups);
        down("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "sess@10", "sess@20", "proj@5", "svc:bravo", "proj@6"]
        );
        down("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "sess@10", "sess@20", "proj@5", "proj@6", "svc:bravo"]
        );
        // At the bottom edge the move is a no-op.
        down("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "sess@10", "sess@20", "proj@5", "proj@6", "svc:bravo"]
        );

        // And back up: over the projects, past each session, rejoining the
        // leading block above alpha only after one more step.
        up("bravo", &sessions, &groups);
        up("bravo", &sessions, &groups);
        up("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "sess@10", "svc:bravo", "sess@20", "proj@5", "proj@6"]
        );
        up("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:alpha", "svc:bravo", "sess@10", "sess@20", "proj@5", "proj@6"]
        );
        up("bravo", &sessions, &groups);
        assert_eq!(
            flow_names(dir.path(), &sessions, &groups),
            ["svc:bravo", "svc:alpha", "sess@10", "sess@20", "proj@5", "proj@6"]
        );
    }

    #[test]
    fn placed_operator_stays_put_when_its_pinned_neighbor_vanishes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.toml"), "harness = \"smith\"\n").unwrap();
        let sessions = vec![
            flow_session("s1", 10),
            flow_session("s2", 20),
            flow_session("s3", 30),
        ];
        for _ in 0..2 {
            move_definition(
                dir.path(),
                "alpha",
                construct_protocol::MoveDirection::Down,
                &sessions,
                &[],
            )
            .unwrap();
        }
        assert_eq!(
            flow_names(dir.path(), &sessions, &[]),
            ["sess@10", "sess@20", "svc:alpha", "sess@30"]
        );

        // The row alpha was dropped after disappears (archived / deleted /
        // moved into a project): alpha keeps its slot between the remaining
        // neighbors instead of snapping back to the leading block.
        let remaining = vec![flow_session("s1", 10), flow_session("s3", 30)];
        assert_eq!(
            flow_names(dir.path(), &remaining, &[]),
            ["sess@10", "svc:alpha", "sess@30"]
        );
    }

    #[test]
    fn routed_operator_sessions_are_not_reorder_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.toml"), "harness = \"smith\"\n").unwrap();
        let mut routed = flow_session("routed", 10);
        routed.title = Some("operator:alpha:key".to_string());
        let plain = flow_session("s1", 20);
        let sessions = vec![routed, plain];

        move_definition(
            dir.path(),
            "alpha",
            construct_protocol::MoveDirection::Down,
            &sessions,
            &[],
        )
        .unwrap();
        // The routed session nests under the operator row in every client, so
        // one step down lands past the first *top-level* session.
        assert_eq!(
            flow_names(dir.path(), &sessions, &[]),
            ["sess@20", "svc:alpha"]
        );
    }

    #[test]
    fn a_slack_channel_chooses_its_progress_affordance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bot.toml"),
            "harness = \"codex\"\n\
             [channels.a]\nkind = \"slack\"\nprogress = \"reaction\"\n\
             [channels.b]\nkind = \"slack\"\nprogress = \"off\"\n\
             [channels.c]\nkind = \"slack\"\n",
        )
        .unwrap();

        let channels = &load_definitions(dir.path()).unwrap()["bot"].channels;
        assert_eq!(channels["a"].progress, SlackProgress::Reaction);
        assert_eq!(channels["b"].progress, SlackProgress::Off);
        // A definition written before this option existed keeps working and
        // gets the default rather than an unset/failed parse.
        assert_eq!(channels["c"].progress, SlackProgress::Placeholder);
    }

    #[test]
    fn a_slack_channel_chooses_where_it_keeps_listening() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bot.toml"),
            "harness = \"codex\"\n\
             [channels.a]\nkind = \"slack\"\nfollow_up = \"channel\"\nthread_context = 10\n\
             [channels.b]\nkind = \"slack\"\nfollow_up = \"off\"\nthread_context = 0\n\
             [channels.c]\nkind = \"slack\"\n",
        )
        .unwrap();

        let channels = &load_definitions(dir.path()).unwrap()["bot"].channels;
        assert_eq!(channels["a"].follow_up, SlackFollowUp::Channel);
        assert_eq!(channels["a"].thread_context, 10);
        assert_eq!(channels["b"].follow_up, SlackFollowUp::Off);
        assert_eq!(channels["b"].thread_context, 0);
        // A definition written before these options existed keeps working.
        assert_eq!(channels["c"].follow_up, SlackFollowUp::Thread);
        assert_eq!(channels["c"].thread_context, default_thread_context());
    }

    #[test]
    fn operator_put_preserves_channels_and_channel_crud_rotates_credentials() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        let operator = construct_protocol::OperatorSummary {
            name: "alerts".into(),
            position: 0,
            placement: None,
            instruction: "triage".into(),
            harness: "smith".into(),
            model: None,
            session_mode: "headless".into(),
            cwd: ".".into(),
            routing: "session-key".into(),
            paused: false,
            channels: Vec::new(),
        };
        let first = put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: operator.clone(),
            },
        )
        .unwrap();
        assert!(first.operator.channels.is_empty());
        let first_channel = put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "http".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let original = first_channel.new_secret.unwrap();
        let second = put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: operator.clone(),
            },
        )
        .unwrap();
        assert_eq!(second.operator.channels.len(), 1);
        let stored = load_definitions(&operators).unwrap();
        assert_eq!(
            stored["alerts"].channels["http"].token.as_deref(),
            Some(original.as_str())
        );
        let rotated = rotate_channel_secret(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_ne!(rotated.new_secret.as_deref(), Some(original.as_str()));
        delete_channel(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert!(load_definitions(&operators)
            .unwrap()
            .get("alerts")
            .unwrap()
            .channels
            .is_empty());
        assert!(list_channel_catalog(&operators).unwrap().is_empty());
    }

    #[test]
    fn channel_ports_are_unique_within_a_operator() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: construct_protocol::OperatorSummary {
                    name: "alerts".into(),
                    position: 0,
                    placement: None,
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "http".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let duplicate = put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "monitoring".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        );
        assert!(duplicate.unwrap_err().to_string().contains("already used"));
    }

    #[test]
    fn channel_catalog_migrates_and_controls_exclusive_attachments() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        std::fs::write(
            operators.join("alerts.toml"),
            "harness = \"smith\"\n[channels.http]\nport = 8787\ntoken = \"secret\"\n",
        )
        .unwrap();
        put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: construct_protocol::OperatorSummary {
                    name: "backup".into(),
                    position: 0,
                    placement: None,
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();

        let catalog = list_channel_catalog(&operators).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].attached_to.as_deref(), Some("alerts"));
        assert!(config.path().join("channels.toml").exists());

        let rejected = attach_channel(
            &operators,
            construct_protocol::OperatorChannelAttachParams {
                operator_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(rejected.to_string().contains("already attached"));

        detach_channel(
            &operators,
            construct_protocol::OperatorChannelAttachParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_eq!(
            list_channel_catalog(&operators).unwrap()[0].attached_to,
            None
        );

        attach_channel(
            &operators,
            construct_protocol::OperatorChannelAttachParams {
                operator_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_eq!(
            list_channel_catalog(&operators).unwrap()[0]
                .attached_to
                .as_deref(),
            Some("backup")
        );
    }

    #[test]
    fn deleting_a_channel_removes_it_from_the_catalog() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        for name in ["alerts", "backup"] {
            put_definition(
                &operators,
                construct_protocol::OperatorPutParams {
                    operator: construct_protocol::OperatorSummary {
                        name: name.into(),
                        position: 0,
                        placement: None,
                        instruction: String::new(),
                        harness: "smith".into(),
                        model: None,
                        session_mode: "headless".into(),
                        cwd: ".".into(),
                        routing: "session-key".into(),
                        paused: false,
                        channels: Vec::new(),
                    },
                },
            )
            .unwrap();
        }
        put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "http".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();

        // Another operator may not delete a channel out from under its owner.
        let stolen = delete_channel(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(stolen.to_string().contains("attached to operator `alerts`"));
        assert_eq!(list_channel_catalog(&operators).unwrap().len(), 1);

        // An unattached channel is deleted from the catalog outright.
        detach_channel(
            &operators,
            construct_protocol::OperatorChannelAttachParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        delete_channel(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert!(list_channel_catalog(&operators).unwrap().is_empty());

        // A channel that is not in the catalog at all cannot be deleted.
        let missing = delete_channel(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(missing.to_string().contains("not found in catalog"));
    }

    #[test]
    fn rotating_an_attached_channel_updates_the_catalog_credential() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: construct_protocol::OperatorSummary {
                    name: "alerts".into(),
                    position: 0,
                    placement: None,
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        let created = put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "http".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let original = created.new_secret.unwrap();
        let rotated = rotate_channel_secret(
            &operators,
            construct_protocol::OperatorChannelNameParams {
                operator_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_ne!(rotated.new_secret.as_deref(), Some(original.as_str()));
        assert!(list_channel_catalog(&operators).unwrap()[0].has_credential);
    }

    #[test]
    fn an_omitted_option_keeps_the_value_the_channel_was_given() {
        // Absent means unchanged, never default: a client that does not offer
        // these fields must be able to save an allowlist without resetting an
        // minibuffer's choice behind their back.
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        std::fs::write(
            operators.join("chat.toml"),
            "harness = \"codex\"\n\
             [channels.bot]\nkind = \"slack\"\nprogress = \"reaction\"\n\
             follow_up = \"channel\"\nthread_context = 7\n\
             app_token = \"xapp-1\"\nbot_token = \"xoxb-1\"\n",
        )
        .unwrap();

        put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "chat".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "bot".into(),
                    kind: "slack".into(),
                    enabled: true,
                    port: None,
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: vec!["T9".into()],
                    allowed_channels: Vec::new(),
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();

        let stored = &load_definitions(&operators).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::Reaction);
        assert_eq!(stored.follow_up, SlackFollowUp::Channel);
        assert_eq!(stored.thread_context, 7);
        assert_eq!(stored.allowed_workspaces, vec!["T9".to_string()]);
    }

    /// A Slack channel with every behavior option left at its default.
    fn slack_channel_fixture() -> (tempfile::TempDir, PathBuf) {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        std::fs::write(
            operators.join("chat.toml"),
            "harness = \"codex\"\n\
             [channels.bot]\nkind = \"slack\"\n\
             app_token = \"xapp-1\"\nbot_token = \"xoxb-1\"\n",
        )
        .unwrap();
        (config, operators)
    }

    fn slack_option_put(
        progress: Option<&str>,
        follow_up: Option<&str>,
        thread_context: Option<usize>,
    ) -> construct_protocol::OperatorChannelPutParams {
        construct_protocol::OperatorChannelPutParams {
            operator_name: "chat".into(),
            channel: construct_protocol::OperatorChannelPut {
                id: "bot".into(),
                kind: "slack".into(),
                enabled: true,
                port: None,
                app_token: None,
                bot_token: None,
                allowed_workspaces: Vec::new(),
                allowed_channels: Vec::new(),
                progress: progress.map(ToString::to_string),
                follow_up: follow_up.map(ToString::to_string),
                thread_context,
                ..Default::default()
            },
            rotate_secret: false,
        }
    }

    #[test]
    fn a_channel_put_sets_the_slack_options_and_reports_them_back() {
        let (_config, operators) = slack_channel_fixture();

        let result = put_channel(
            &operators,
            slack_option_put(Some("both"), Some("channel"), Some(12)),
        )
        .unwrap();

        // Reported back, so a client can show what it is preserving.
        assert_eq!(result.channel.progress.as_deref(), Some("both"));
        assert_eq!(result.channel.follow_up.as_deref(), Some("channel"));
        assert_eq!(result.channel.thread_context, Some(12));

        let stored = &load_definitions(&operators).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::Both);
        assert_eq!(stored.follow_up, SlackFollowUp::Channel);
        assert_eq!(stored.thread_context, 12);
    }

    #[test]
    fn an_unknown_option_value_is_refused_rather_than_defaulted() {
        let (_config, operators) = slack_channel_fixture();

        for params in [
            slack_option_put(Some("loud"), None, None),
            slack_option_put(None, Some("everywhere"), None),
        ] {
            let error = put_channel(&operators, params).unwrap_err().to_string();
            assert!(error.contains("expected one of"), "unexpected: {error}");
        }
        // A refused edit leaves the stored definition untouched.
        let stored = &load_definitions(&operators).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::default());
        assert_eq!(stored.follow_up, SlackFollowUp::default());
    }

    #[test]
    fn a_thread_context_past_slacks_own_page_limit_is_refused() {
        let (_config, operators) = slack_channel_fixture();

        let error = put_channel(
            &operators,
            slack_option_put(None, None, Some(THREAD_CONTEXT_MAX + 1)),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("at most"), "unexpected: {error}");
        assert_eq!(
            put_channel(&operators, slack_option_put(None, None, Some(0)))
                .unwrap()
                .channel
                .thread_context,
            Some(0),
            "0 is a real setting, not an absent one"
        );
    }

    #[test]
    fn slack_options_are_refused_on_a_channel_that_cannot_read_them() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        std::fs::write(operators.join("alerts.toml"), "harness = \"codex\"\n").unwrap();

        let error = put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "alerts".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "http".into(),
                    kind: "http".into(),
                    enabled: true,
                    port: Some(8787),
                    app_token: None,
                    bot_token: None,
                    allowed_workspaces: Vec::new(),
                    allowed_channels: Vec::new(),
                    follow_up: Some("channel".into()),
                    progress: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Slack channels only"), "unexpected: {error}");
        // An HTTP channel reports no options rather than defaults it ignores.
        let summary = &list_channel_catalog(&operators).unwrap();
        assert!(summary.is_empty(), "the refused put must not have stored");
    }

    #[test]
    fn the_published_option_defaults_match_the_ones_a_definition_gets() {
        // Clients seed a new channel from the published defaults, so a drift
        // here would show the user a value the daemon would not store.
        assert_eq!(
            SlackProgress::default().as_str(),
            construct_protocol::SLACK_PROGRESS_DEFAULT
        );
        assert_eq!(
            SlackFollowUp::default().as_str(),
            construct_protocol::SLACK_FOLLOW_UP_DEFAULT
        );
        assert_eq!(default_thread_context(), SLACK_THREAD_CONTEXT_DEFAULT);
        // Every value a client can offer is one the daemon accepts.
        for value in PROGRESS_VALUES {
            assert!(SlackProgress::parse(value).is_ok(), "{value}");
        }
        for value in FOLLOW_UP_VALUES {
            assert!(SlackFollowUp::parse(value).is_ok(), "{value}");
        }
        for value in construct_protocol::SLACK_PERSONAL_RESPONSE_VALUES {
            assert!(SlackPersonalResponse::parse(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn slack_credentials_are_persisted_but_never_returned_in_summaries() {
        let config = tempfile::tempdir().unwrap();
        let operators = config.path().join("operators");
        std::fs::create_dir_all(&operators).unwrap();
        put_definition(
            &operators,
            construct_protocol::OperatorPutParams {
                operator: construct_protocol::OperatorSummary {
                    name: "chat".into(),
                    position: 0,
                    placement: None,
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        let result = put_channel(
            &operators,
            construct_protocol::OperatorChannelPutParams {
                operator_name: "chat".into(),
                channel: construct_protocol::OperatorChannelPut {
                    id: "slack".into(),
                    kind: "slack".into(),
                    enabled: true,
                    port: None,
                    app_token: Some("xapp-secret".into()),
                    bot_token: Some("xoxb-secret".into()),
                    allowed_workspaces: vec![" T2 ".into(), "T1".into(), "T1".into()],
                    allowed_channels: vec!["C1".into()],
                    progress: None,
                    follow_up: None,
                    thread_context: None,
                    ..Default::default()
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        assert!(result.new_secret.is_none());
        assert!(result.channel.has_credential);
        assert!(result.channel.has_app_token);
        assert!(result.channel.has_bot_token);
        assert_eq!(result.channel.allowed_workspaces, vec!["T1", "T2"]);
        let encoded = std::fs::read_to_string(operators.join("chat.toml")).unwrap();
        assert!(encoded.contains("xapp-secret"));
        assert!(encoded.contains("xoxb-secret"));
        let summary = serde_json::to_string(&result.channel).unwrap();
        assert!(!summary.contains("xapp-secret"));
        assert!(!summary.contains("xoxb-secret"));
    }

    #[test]
    fn slack_token_prefixes_are_validated() {
        assert!(validate_slack_token("app", Some("xapp-good"), "xapp-").is_ok());
        assert!(validate_slack_token("bot", Some("xoxp-user"), "xoxb-").is_err());
        assert!(validate_slack_token("bot", None, "xoxb-").is_err());
    }

    fn personal_put(
        channel: construct_protocol::OperatorChannelPut,
    ) -> construct_protocol::OperatorChannelPutParams {
        construct_protocol::OperatorChannelPutParams {
            operator_name: "chat".into(),
            channel,
            rotate_secret: false,
        }
    }

    #[test]
    fn a_slack_personal_channel_saves_and_reports_its_options() {
        let (_config, operators) = slack_channel_fixture();
        let result = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                mcp_command: Some("npx my-slack-mcp".into()),
                trigger: Some("all".into()),
                response_mode: Some("auto-after".into()),
                auto_after_secs: Some(45),
                disclosure: Some(false),
                poll_interval_secs: Some(30),
                thread_context: Some(10),
                allowed_channels: vec!["C1".into()],
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(result.channel.has_credential);
        assert_eq!(result.channel.kind, "slack-personal");
        assert_eq!(result.channel.mcp_command.as_deref(), Some("npx my-slack-mcp"));
        assert_eq!(result.channel.trigger.as_deref(), Some("all"));
        assert_eq!(result.channel.response_mode.as_deref(), Some("auto-after"));
        assert_eq!(result.channel.auto_after_secs, Some(45));
        assert_eq!(result.channel.disclosure, Some(false));
        assert_eq!(result.channel.poll_interval_secs, Some(30));
        assert_eq!(result.channel.thread_context, Some(10));

        // A later edit that omits every option preserves the stored values.
        let edited = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                allowed_channels: vec!["C1".into()],
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(edited.channel.mcp_command.as_deref(), Some("npx my-slack-mcp"));
        assert_eq!(edited.channel.trigger.as_deref(), Some("all"));
        assert_eq!(edited.channel.response_mode.as_deref(), Some("auto-after"));
        assert_eq!(edited.channel.auto_after_secs, Some(45));
        assert_eq!(edited.channel.disclosure, Some(false));
        assert_eq!(edited.channel.poll_interval_secs, Some(30));
    }

    #[test]
    fn a_slack_personal_channel_without_a_backend_command_is_refused() {
        let (_config, operators) = slack_channel_fixture();
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("mcp_command"), "{error}");
    }

    #[test]
    fn options_are_refused_where_their_kind_does_not_read_them() {
        let (_config, operators) = slack_channel_fixture();

        // Bot-only affordances on a slack-personal channel: this channel may
        // not post progress as the user, so storing the option would lie.
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                mcp_command: Some("cmd".into()),
                progress: Some("both".into()),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Slack bot channels only"), "{error}");

        // slack-personal options on the bot kind.
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "bot".into(),
                kind: "slack".into(),
                app_token: Some("xapp-1".into()),
                bot_token: Some("xoxb-1".into()),
                trigger: Some("dm".into()),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("slack-personal channels only"),
            "{error}"
        );

        // Bot tokens on a slack-personal channel.
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                mcp_command: Some("cmd".into()),
                app_token: Some("xapp-1".into()),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("MCP backend"), "{error}");
    }

    #[test]
    fn a_too_fast_poll_interval_is_refused() {
        let (_config, operators) = slack_channel_fixture();
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                mcp_command: Some("cmd".into()),
                poll_interval_secs: Some(1),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("at least"), "{error}");
    }

    #[test]
    fn auto_after_delay_is_parsed_and_must_be_positive() {
        let parsed: OperatorChannelConfig = toml::from_str(
            r#"
kind = "slack-personal"
mcp_command = "backend"
response_mode = "auto-after"
auto_after_secs = 17
"#,
        )
        .expect("parse delayed response mode");
        assert_eq!(parsed.response_mode, Some(SlackPersonalResponse::AutoAfter));
        assert_eq!(parsed.auto_after_secs, Some(17));

        let (_config, operators) = slack_channel_fixture();
        let error = put_channel(
            &operators,
            personal_put(construct_protocol::OperatorChannelPut {
                id: "me".into(),
                kind: "slack-personal".into(),
                mcp_command: Some("cmd".into()),
                response_mode: Some("auto-after".into()),
                auto_after_secs: Some(0),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    #[test]
    fn slack_personal_config_snapshots_the_channel_with_safe_defaults() {
        let channel = OperatorChannelConfig {
            kind: Some("slack-personal".into()),
            mcp_command: Some("npx my-slack-mcp".into()),
            ..Default::default()
        };
        let config = slack_personal_config("chat", "me", &channel).expect("config");
        assert_eq!(config.mcp_command, "npx my-slack-mcp");
        // The safe posture is the default posture: DMs only, drafts only,
        // disclosure on (spec 0202).
        assert_eq!(config.trigger, SlackPersonalTrigger::Dm);
        assert_eq!(config.response, SlackPersonalResponse::Draft);
        assert_eq!(config.auto_after, std::time::Duration::from_secs(60));
        assert!(config.disclosure);
        assert_eq!(config.poll_interval, std::time::Duration::from_secs(20));

        // Disabled channels and channels without a command produce no task.
        let disabled = OperatorChannelConfig {
            enabled: false,
            ..channel.clone()
        };
        assert!(slack_personal_config("chat", "me", &disabled).is_none());
        let missing = OperatorChannelConfig {
            kind: Some("slack-personal".into()),
            ..Default::default()
        };
        assert!(slack_personal_config("chat", "me", &missing).is_none());

        // A hand-edited interval below the floor is clamped, not obeyed.
        let fast = OperatorChannelConfig {
            poll_interval_secs: Some(1),
            ..channel
        };
        let config = slack_personal_config("chat", "me", &fast).expect("config");
        assert_eq!(config.poll_interval, std::time::Duration::from_secs(5));
    }
}
