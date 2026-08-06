//! Daemon-owned service definitions and channel lifecycle.
//!
//! Service definitions live in `services/<name>.toml` in the config dir.
//! HTTP remains loopback-only. Transport adapters are separate from the shared
//! ingress router so adding a channel does not fork session semantics.

mod harness_error;
mod http;
mod ingress;
mod slack;

use anyhow::{anyhow, Context, Result};
// The accepted values for a channel's behavior options are published by the
// protocol so that what a client offers and what the daemon accepts cannot
// drift apart.
use construct_protocol::{
    SLACK_FOLLOW_UP_VALUES as FOLLOW_UP_VALUES, SLACK_PROGRESS_VALUES as PROGRESS_VALUES,
    SLACK_THREAD_CONTEXT_DEFAULT, SLACK_THREAD_CONTEXT_MAX as THREAD_CONTEXT_MAX,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub instruction: String,
    #[serde(default = "default_service_harness")]
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_mode: ServiceSessionMode,
    #[serde(default = "default_service_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub routing: ServiceRouting,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub position: i64,
    /// Seconds to hold a turn stopped at an approval before denying it on the
    /// caller's behalf. `0` waits indefinitely, which keeps the operator as
    /// the only one who can decide.
    #[serde(default)]
    pub approval_timeout_secs: u64,
    #[serde(default)]
    pub sandbox: ServiceSandboxConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, ServiceChannelConfig>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceSessionMode {
    #[default]
    Headless,
    Interactive,
}

impl ServiceSessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::Interactive => "interactive",
        }
    }
}

/// Capability limits applied to every session a service creates.
///
/// A service session is prompted by a third party, so it is confined by
/// default and widened only by explicit configuration. Filesystem and network
/// confinement are not represented here: the harness sandbox already limits
/// writes to the session's working directory and denies egress, and a service
/// must not be able to relax that from its own definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ServiceSandboxConfig {
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

impl Default for ServiceSandboxConfig {
    fn default() -> Self {
        Self {
            fleet_control: false,
            mcp: false,
            skills: default_sandbox_skills(),
        }
    }
}

impl ServiceSandboxConfig {
    /// Environment applied at session creation. Each entry withholds a
    /// capability; an allowed capability adds nothing, so a service session
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

fn default_service_harness() -> String {
    "smith".to_string()
}

fn default_service_cwd() -> String {
    ".".to_string()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceRouting {
    PerEvent,
    #[default]
    SessionKey,
    Single,
}

/// What a Slack channel shows while a turn it accepted is still running.
///
/// A long turn is indistinguishable from a dropped one when the channel stays
/// silent, so the operator picks how visible the wait should be. `Reaction`
/// and `Both` call `reactions.add`, which needs the `reactions:write` scope —
/// an app the operator has not reinstalled since granting it will log the
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
/// within a boundary the operator sets, because "answers everything in this
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
pub struct ServiceChannelConfig {
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
}

fn default_channel_enabled() -> bool {
    true
}

pub fn load_definitions(dir: &std::path::Path) -> Result<BTreeMap<String, ServiceConfig>> {
    let mut services = BTreeMap::new();
    if !dir.exists() {
        return Ok(services);
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
        validate_service_name(name)?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read service definition {}", path.display()))?;
        let definition = toml::from_str(&raw)
            .with_context(|| format!("parse service definition {}", path.display()))?;
        services.insert(name.to_string(), definition);
    }
    Ok(services)
}

pub fn list_summaries(dir: &std::path::Path) -> Result<Vec<construct_protocol::ServiceSummary>> {
    let mut out: Vec<construct_protocol::ServiceSummary> = load_definitions(dir)?
        .into_iter()
        .map(|(name, config)| summary(name, &config))
        .collect();
    out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

pub fn move_service(dir: &std::path::Path, name: &str, direction: construct_protocol::MoveDirection) -> Result<()> {
    validate_service_name(name)?;
    let mut services = load_definitions(dir)?;
    let mut sorted: Vec<(String, i64)> = services
        .iter()
        .map(|(n, c)| (n.clone(), c.position))
        .collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let idx = sorted
        .iter()
        .position(|(n, _)| n == name)
        .ok_or_else(|| anyhow!("service `{name}` not found"))?;
    let neighbor_idx = match direction {
        construct_protocol::MoveDirection::Up => {
            if idx == 0 {
                return Ok(());
            }
            idx - 1
        }
        construct_protocol::MoveDirection::Down => {
            if idx + 1 >= sorted.len() {
                return Ok(());
            }
            idx + 1
        }
    };
    let a_name = sorted[idx].0.clone();
    let b_name = sorted[neighbor_idx].0.clone();
    let a_pos = services.get(&a_name).map(|c| c.position).unwrap_or(0);
    let b_pos = services.get(&b_name).map(|c| c.position).unwrap_or(0);
    if a_pos == b_pos {
        for (i, (n, _)) in sorted.iter().enumerate() {
            if let Some(cfg) = services.get_mut(n) {
                cfg.position = i as i64;
            }
        }
        let a_pos = services.get(&a_name).map(|c| c.position).unwrap_or(0);
        let b_pos = services.get(&b_name).map(|c| c.position).unwrap_or(0);
        if let Some(cfg) = services.get_mut(&a_name) {
            cfg.position = b_pos;
        }
        if let Some(cfg) = services.get_mut(&b_name) {
            cfg.position = a_pos;
        }
    } else {
        if let Some(cfg) = services.get_mut(&a_name) {
            cfg.position = b_pos;
        }
        if let Some(cfg) = services.get_mut(&b_name) {
            cfg.position = a_pos;
        }
    }
    write_definition(dir, &a_name, services.get(&a_name).unwrap())?;
    write_definition(dir, &b_name, services.get(&b_name).unwrap())?;
    Ok(())
}

pub fn put_definition(
    dir: &std::path::Path,
    params: construct_protocol::ServicePutParams,
) -> Result<construct_protocol::ServicePutResult> {
    validate_service_name(&params.service.name)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.toml", params.service.name));
    let existing = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| toml::from_str::<ServiceConfig>(&raw).ok());
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
    let routing = match params.service.routing.as_str() {
        "per-event" => ServiceRouting::PerEvent,
        "session-key" => ServiceRouting::SessionKey,
        "single" => ServiceRouting::Single,
        other => return Err(anyhow!("invalid routing mode `{other}`")),
    };
    let session_mode =
        parse_service_session_mode(&params.service.harness, &params.service.session_mode)?;
    let position = existing
        .as_ref()
        .map(|config| config.position)
        .unwrap_or_else(|| {
            let mut max_pos: Option<i64> = None;
            if let Ok(defs) = load_definitions(dir) {
                for cfg in defs.values() {
                    max_pos = Some(max_pos.map_or(cfg.position, |m| m.max(cfg.position)));
                }
            }
            max_pos.map(|p| p + 1).unwrap_or(0)
        });
    let config = ServiceConfig {
        instruction: params.service.instruction,
        harness: params.service.harness,
        model: params.service.model,
        session_mode,
        cwd: params.service.cwd,
        routing,
        paused: params.service.paused,
        position,
        approval_timeout_secs: existing
            .as_ref()
            .map(|config| config.approval_timeout_secs)
            .unwrap_or_default(),
        sandbox,
        channels,
    };
    write_definition(dir, &params.service.name, &config)?;
    Ok(construct_protocol::ServicePutResult {
        service: summary(params.service.name, &config),
        applied: Default::default(),
    })
}

fn parse_service_session_mode(harness: &str, mode: &str) -> Result<ServiceSessionMode> {
    match mode {
        "headless" => Ok(ServiceSessionMode::Headless),
        "interactive" if matches!(harness, "codex" | "claude") => {
            Ok(ServiceSessionMode::Interactive)
        }
        "interactive" => {
            return Err(anyhow!(
                "interactive service sessions currently require the `codex` or `claude` harness"
            ))
        }
        other => return Err(anyhow!("invalid service session mode `{other}`")),
    }
}

pub fn delete_definition(dir: &std::path::Path, name: &str) -> Result<()> {
    validate_service_name(name)?;
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ChannelCatalog {
    #[serde(default)]
    channels: BTreeMap<String, ServiceChannelConfig>,
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

fn channel_owners(services: &BTreeMap<String, ServiceConfig>) -> BTreeMap<String, Vec<String>> {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (service_name, service) in services {
        for channel_id in service.channels.keys() {
            owners
                .entry(channel_id.clone())
                .or_default()
                .push(service_name.clone());
        }
    }
    owners
}

fn owner_label(owners: &BTreeMap<String, Vec<String>>, channel_id: &str) -> Option<String> {
    owners.get(channel_id).map(|services| services.join(", "))
}

fn migrate_legacy_channels(
    dir: &std::path::Path,
    services: &BTreeMap<String, ServiceConfig>,
    catalog: &mut ChannelCatalog,
) -> Result<()> {
    let mut changed = false;
    for service in services.values() {
        for (id, config) in &service.channels {
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
    service_name: &str,
) -> Result<Vec<construct_protocol::ServiceChannelSummary>> {
    validate_service_name(service_name)?;
    let services = load_definitions(dir)?;
    let service = services
        .get(service_name)
        .ok_or_else(|| anyhow!("service `{service_name}` not found"))?;
    Ok(service
        .channels
        .iter()
        .map(|(id, channel)| channel_summary(id.clone(), channel, Some(service_name.to_string())))
        .collect())
}

pub fn list_channel_catalog(
    dir: &std::path::Path,
) -> Result<Vec<construct_protocol::ServiceChannelSummary>> {
    let services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    let owners = channel_owners(&services);
    Ok(catalog
        .channels
        .iter()
        .map(|(id, channel)| channel_summary(id.clone(), channel, owner_label(&owners, id)))
        .collect())
}

pub fn put_channel(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelPutParams,
) -> Result<construct_protocol::ServiceChannelPutResult> {
    validate_service_name(&params.service_name)?;
    validate_channel_id(&params.channel.id)?;
    if !matches!(params.channel.kind.as_str(), "http" | "slack") {
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
    let mut services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    let owners = channel_owners(&services);
    if let Some(owner) = owner_label(&owners, &params.channel.id) {
        if owner != params.service_name {
            return Err(anyhow!(
                "channel `{}` is already attached to service `{owner}`",
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
            return Err(anyhow!("HTTP port {port} is already used by this service"));
        }
    }
    let service = services
        .get_mut(&params.service_name)
        .ok_or_else(|| anyhow!("service `{}` not found", params.service_name))?;
    let existing = service
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
    if params.channel.kind == "slack" {
        validate_slack_token("app", app_token.as_deref(), "xapp-")?;
        validate_slack_token("bot", bot_token.as_deref(), "xoxb-")?;
    } else if params.channel.progress.is_some()
        || params.channel.follow_up.is_some()
        || params.channel.thread_context.is_some()
    {
        // Accepting these on an HTTP channel would store an option nothing
        // reads, and report it back as though it were in effect.
        return Err(anyhow!(
            "progress, follow_up, and thread_context apply to Slack channels only"
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
    let config = ServiceChannelConfig {
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
    };
    service
        .channels
        .insert(params.channel.id.clone(), config.clone());
    catalog
        .channels
        .insert(params.channel.id.clone(), config.clone());
    let summary = channel_summary(
        params.channel.id,
        &config,
        Some(params.service_name.clone()),
    );
    write_definition(dir, &params.service_name, service)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::ServiceChannelPutResult {
        channel: summary,
        new_secret,
        applied: Default::default(),
    })
}

/// Remove a channel from the catalog for good, detaching it from the caller's
/// service first when it is attached there. Deletion is honest — unlike
/// [`detach_channel`], the channel does not survive as an available catalog
/// entry. A channel owned by another service is refused rather than stolen.
pub fn delete_channel(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelNameParams,
) -> Result<()> {
    validate_service_name(&params.service_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    if !catalog.channels.contains_key(&params.channel_id) {
        return Err(anyhow!(
            "channel `{}` not found in catalog",
            params.channel_id
        ));
    }
    let owners = channel_owners(&services);
    if let Some(owner) = owner_label(&owners, &params.channel_id) {
        if owner != params.service_name {
            return Err(anyhow!(
                "channel `{}` is attached to service `{owner}`; delete it from there",
                params.channel_id
            ));
        }
        let service = services
            .get_mut(&params.service_name)
            .ok_or_else(|| anyhow!("service `{}` not found", params.service_name))?;
        service.channels.remove(&params.channel_id);
        write_definition(dir, &params.service_name, service)?;
    }
    catalog.channels.remove(&params.channel_id);
    write_channel_catalog(dir, &catalog)?;
    Ok(())
}

pub fn attach_channel(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelAttachParams,
) -> Result<construct_protocol::ServiceChannelPutResult> {
    validate_service_name(&params.service_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    let owners = channel_owners(&services);
    if let Some(owner) = owner_label(&owners, &params.channel_id) {
        if owner != params.service_name {
            return Err(anyhow!(
                "channel `{}` is already attached to service `{owner}`",
                params.channel_id
            ));
        }
    }
    let config = catalog
        .channels
        .get(&params.channel_id)
        .cloned()
        .ok_or_else(|| anyhow!("channel `{}` not found in catalog", params.channel_id))?;
    let service = services
        .get_mut(&params.service_name)
        .ok_or_else(|| anyhow!("service `{}` not found", params.service_name))?;
    if channel_kind(&params.channel_id, &config) == "http" {
        if service.channels.iter().any(|(id, channel)| {
            id != &params.channel_id
                && channel_kind(id, channel) == "http"
                && channel.port == config.port
        }) {
            return Err(anyhow!(
                "HTTP port {:?} is already used by this service",
                config.port
            ));
        }
    }
    service
        .channels
        .insert(params.channel_id.clone(), config.clone());
    write_definition(dir, &params.service_name, service)?;
    Ok(construct_protocol::ServiceChannelPutResult {
        channel: channel_summary(params.channel_id, &config, Some(params.service_name)),
        new_secret: None,
        applied: Default::default(),
    })
}

pub fn detach_channel(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelAttachParams,
) -> Result<construct_protocol::ServiceChannelPutResult> {
    validate_service_name(&params.service_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    let service = services
        .get_mut(&params.service_name)
        .ok_or_else(|| anyhow!("service `{}` not found", params.service_name))?;
    let config = service.channels.remove(&params.channel_id).ok_or_else(|| {
        anyhow!(
            "channel `{}` is not attached to service `{}`",
            params.channel_id,
            params.service_name
        )
    })?;
    catalog
        .channels
        .entry(params.channel_id.clone())
        .or_insert_with(|| config.clone());
    write_definition(dir, &params.service_name, service)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::ServiceChannelPutResult {
        channel: channel_summary(params.channel_id, &config, None),
        new_secret: None,
        applied: Default::default(),
    })
}

pub fn rotate_channel_secret(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelNameParams,
) -> Result<construct_protocol::ServiceChannelPutResult> {
    validate_service_name(&params.service_name)?;
    validate_channel_id(&params.channel_id)?;
    let mut services = load_definitions(dir)?;
    let mut catalog = load_channel_catalog(dir)?;
    migrate_legacy_channels(dir, &services, &mut catalog)?;
    let service = services
        .get_mut(&params.service_name)
        .ok_or_else(|| anyhow!("service `{}` not found", params.service_name))?;
    let channel = service
        .channels
        .get_mut(&params.channel_id)
        .ok_or_else(|| {
            anyhow!(
                "channel `{}` not found on service `{}`",
                params.channel_id,
                params.service_name
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
        Some(params.service_name.clone()),
    );
    write_definition(dir, &params.service_name, service)?;
    write_channel_catalog(dir, &catalog)?;
    Ok(construct_protocol::ServiceChannelPutResult {
        channel: summary,
        new_secret: Some(secret),
        applied: Default::default(),
    })
}

fn write_definition(dir: &std::path::Path, name: &str, config: &ServiceConfig) -> Result<()> {
    let path = dir.join(format!("{name}.toml"));
    let encoded = toml::to_string_pretty(config)?;
    let temporary = dir.join(format!(".{name}.toml.tmp"));
    std::fs::write(&temporary, encoded)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn channel_kind(id: &str, config: &ServiceChannelConfig) -> String {
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
    config: &ServiceChannelConfig,
    attached_to: Option<String>,
) -> construct_protocol::ServiceChannelSummary {
    // Behavior options belong to Slack, and a client that cannot see the stored
    // value cannot show what an omitted field is preserving.
    let slack = channel_kind(&id, config) == "slack";
    construct_protocol::ServiceChannelSummary {
        id: id.clone(),
        kind: channel_kind(&id, config),
        enabled: config.enabled,
        port: config.port,
        has_credential: match channel_kind(&id, config).as_str() {
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
        thread_context: slack.then_some(config.thread_context),
        attached_to,
        publication: None,
    }
}

fn summary(name: String, config: &ServiceConfig) -> construct_protocol::ServiceSummary {
    let service_name = name.clone();
    construct_protocol::ServiceSummary {
        name,
        instruction: config.instruction.clone(),
        harness: config.harness.clone(),
        model: config.model.clone(),
        session_mode: config.session_mode.as_str().to_string(),
        cwd: config.cwd.clone(),
        routing: match config.routing {
            ServiceRouting::PerEvent => "per-event",
            ServiceRouting::SessionKey => "session-key",
            ServiceRouting::Single => "single",
        }
        .to_string(),
        paused: config.paused,
        position: config.position,
        channels: config
            .channels
            .iter()
            .map(|(id, channel)| channel_summary(id.clone(), channel, Some(service_name.clone())))
            .collect(),
    }
}

fn validate_service_name(name: &str) -> Result<()> {
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
        Err(anyhow!("invalid service name `{name}`"))
    }
}

pub(crate) use ingress::{ServiceIngress as ServiceRuntime, ServiceIngressShared as ServiceShared};
pub(crate) use slack::SlackConfig;

/// Build the transport-neutral ingress runtime for one channel.
pub(crate) fn channel_runtime(
    shared: Arc<ServiceShared>,
    channel_id: String,
) -> Arc<ServiceRuntime> {
    Arc::new(ServiceRuntime::new(channel_id, shared))
}

/// Whether a channel is one this daemon knows how to bind, logging the reason
/// when it is not. Used by the supervisor to build the desired listener set.
pub(crate) fn bindable_port(
    service: &str,
    channel_id: &str,
    channel: &ServiceChannelConfig,
) -> Option<u16> {
    if !channel.enabled {
        return None;
    }
    let kind = channel_kind(channel_id, channel);
    if kind == "slack" {
        return None;
    }
    if kind != "http" {
        tracing::warn!(service = %service, channel = %channel_id, "unsupported service channel kind; skipping");
        return None;
    }
    let Some(port) = channel.port else {
        tracing::warn!(service = %service, channel = %channel_id, "HTTP channel has no port; skipping");
        return None;
    };
    if channel.token.as_deref().unwrap_or("").is_empty() {
        tracing::warn!(service = %service, channel = %channel_id, "HTTP channel has no token; skipping");
        return None;
    }
    Some(port)
}

/// Describe the local ingress this channel adapter owns. Publication code
/// consumes this typed endpoint and never inspects `ServiceChannelConfig`.
/// A future channel kind adds its adapter mapping here (or in a registry)
/// without adding protocol branches to the tunnel supervisor.
pub(crate) fn ingress_endpoint(
    service: &str,
    channel_id: &str,
    channel: &ServiceChannelConfig,
) -> Option<crate::channel_publication::ChannelIngressEndpoint> {
    bindable_port(service, channel_id, channel).map(|port| {
        crate::channel_publication::ChannelIngressEndpoint::loopback_http(
            port,
            format!("/svc/{service}"),
        )
    })
}

/// Drive one supervisor-owned HTTP listener until it is cancelled.
pub(crate) async fn serve(
    runtime: Arc<ServiceRuntime>,
    listener: tokio::net::TcpListener,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    http::serve(runtime, listener, cancel).await
}

/// Validate and snapshot one Slack channel without exposing its credentials.
pub(crate) fn slack_config(
    service: &str,
    channel_id: &str,
    channel: &ServiceChannelConfig,
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
        tracing::warn!(service = %service, channel = %channel_id, "Slack channel credentials are missing or invalid; skipping");
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
    runtime: Arc<ServiceRuntime>,
    config: slack::SlackConfig,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    slack::serve(runtime, config, cancel).await
}

#[cfg(test)]
mod tests {
    use super::http::{content_length, find_headers_end, parse_http_route, HttpRoute};
    use super::ingress::{latest_assistant_reply, pending_approval, PersistedState};
    use super::*;
    use construct_protocol::{MessageRole, SessionEvent, SessionState};

    /// A live service plus one channel runtime, so the tests below read
    /// configuration the way a request does rather than inspecting the struct.
    async fn live_service(config: ServiceConfig) -> (Arc<ServiceShared>, Arc<ServiceRuntime>) {
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
        let shared = ServiceShared::load(
            "svc".to_string(),
            config,
            Arc::new(manager),
            tmp.path().join("data"),
        );
        let runtime = channel_runtime(shared.clone(), "http1".to_string());
        (shared, runtime)
    }

    fn config_with_channel(token: &str, enabled: bool, paused: bool) -> ServiceConfig {
        ServiceConfig {
            instruction: String::new(),
            harness: "smith".into(),
            model: None,
            session_mode: ServiceSessionMode::Headless,
            cwd: ".".into(),
            routing: ServiceRouting::SessionKey,
            paused,
            approval_timeout_secs: 0,
            sandbox: ServiceSandboxConfig::default(),
            channels: BTreeMap::from([(
                "http1".to_string(),
                ServiceChannelConfig {
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
                },
            )]),
        }
    }

    #[test]
    fn interactive_mode_is_limited_to_native_agent_harnesses() {
        assert_eq!(
            parse_service_session_mode("codex", "interactive").unwrap(),
            ServiceSessionMode::Interactive
        );
        assert_eq!(
            parse_service_session_mode("claude", "interactive").unwrap(),
            ServiceSessionMode::Interactive
        );
        assert!(parse_service_session_mode("smith", "interactive")
            .unwrap_err()
            .to_string()
            .contains("codex` or `claude"));
        assert!(parse_service_session_mode("codex", "unknown").is_err());
    }

    #[tokio::test]
    async fn a_rotated_credential_takes_effect_without_rebinding() {
        // The listener never moves for a rotation, so the credential has to be
        // read per request. Before this, the old secret kept working until the
        // daemon restarted.
        let (shared, runtime) = live_service(config_with_channel("first", true, false)).await;
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
        let (shared, runtime) = live_service(config_with_channel("t", true, false)).await;
        assert!(http::serving(&runtime));

        shared.set_config(config_with_channel("t", true, true));
        assert!(
            !http::serving(&runtime),
            "a paused service refuses requests"
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
        let (shared, _runtime) = live_service(config_with_channel("t", true, false)).await;
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
        edited.routing = ServiceRouting::PerEvent;
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
        assert_eq!(shared.config().routing, ServiceRouting::PerEvent);
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
        // Transcript shape observed from a live interactive codex service
        // turn: codex flushed the reply tool's result *after* the assistant
        // text it produced. Treating that trailing result as a turn boundary
        // hid the answer entirely — the poll endpoint reported `ready` with a
        // null reply, and a waiting caller blocked to its timeout.
        let events = vec![
            msg(MessageRole::User, "say CHARLIE"),
            SessionEvent::ToolUse {
                tool: "construct_service_reply".into(),
                args: serde_json::Value::Null,
                call_id: Some("call-1".into()),
            },
            msg(MessageRole::Assistant, "CHARLIE"),
            SessionEvent::ToolResult {
                tool: "construct_service_reply".into(),
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
    fn services_withhold_fleet_access_unless_asked() {
        // The default matters on its own: a service is prompted by whoever can
        // reach its channel, so an omitted [sandbox] section must not hand that
        // caller the fleet.
        let env = ServiceSandboxConfig::default().session_env();
        assert_eq!(
            env.get("CONSTRUCT_SMITH_FLEET_TOOLS").map(String::as_str),
            Some("off")
        );
        assert_eq!(
            env.get("CONSTRUCT_INJECT_MCP").map(String::as_str),
            Some("0")
        );
        assert!(!env.contains_key("CONSTRUCT_SMITH_SKILLS"));

        let opened = ServiceSandboxConfig {
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
        let mut config = ServiceConfig {
            instruction: "hi".into(),
            harness: "smith".into(),
            model: None,
            session_mode: ServiceSessionMode::Headless,
            cwd: ".".into(),
            routing: ServiceRouting::SessionKey,
            paused: false,
            approval_timeout_secs: 0,
            sandbox: ServiceSandboxConfig::default(),
            channels: BTreeMap::new(),
        };
        config.sandbox.fleet_control = true;
        write_definition(dir.path(), "svc", &config).unwrap();

        // An edit that never mentions the sandbox must not re-confine (or
        // re-open) the service behind the operator's back.
        put_definition(
            dir.path(),
            construct_protocol::ServicePutParams {
                service: construct_protocol::ServiceSummary {
                    name: "svc".into(),
                    instruction: "changed".into(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    position: 0,
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
        // The operator stays the only one who can approve unless they opt into
        // a bound; turning this on by default would deny work nobody refused.
        let raw = "instruction = \"x\"\nharness = \"smith\"\ncwd = \".\"\n";
        let config: ServiceConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.approval_timeout_secs, 0);

        let bounded: ServiceConfig =
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
    fn http_routes_distinguish_submit_result_method_and_wrong_service() {
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
    fn legacy_keyed_sessions_become_service_owned() {
        let mut state: PersistedState =
            serde_json::from_str(r#"{"sessions":{"incident-1":"s123"}}"#).unwrap();
        assert!(state.owned_sessions.is_empty());
        state.normalize_legacy_ownership();
        assert!(state.owned_sessions.contains("s123"));
    }

    #[test]
    fn service_config_accepts_v1_routing_mode() {
        let service: ServiceConfig = toml::from_str(
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
        assert_eq!(service.routing, ServiceRouting::SessionKey);
        assert_eq!(service.channels["http"].port, Some(8787));
    }

    #[test]
    fn loads_one_toml_document_per_service() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("alerts.toml"),
            "harness = \"smith\"\n[channels.http]\nport = 8787\ntoken = \"secret\"\n",
        )
        .unwrap();
        let services = load_definitions(dir.path()).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services["alerts"].channels["http"].port, Some(8787));
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
    fn service_put_preserves_channels_and_channel_crud_rotates_credentials() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        let service = construct_protocol::ServiceSummary {
            name: "alerts".into(),
            instruction: "triage".into(),
            harness: "smith".into(),
            model: None,
            session_mode: "headless".into(),
            cwd: ".".into(),
            routing: "session-key".into(),
            paused: false,
            position: 0,
            channels: Vec::new(),
        };
        let first = put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: service.clone(),
            },
        )
        .unwrap();
        assert!(first.service.channels.is_empty());
        let first_channel = put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let original = first_channel.new_secret.unwrap();
        let second = put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: service.clone(),
            },
        )
        .unwrap();
        assert_eq!(second.service.channels.len(), 1);
        let stored = load_definitions(&services).unwrap();
        assert_eq!(
            stored["alerts"].channels["http"].token.as_deref(),
            Some(original.as_str())
        );
        let rotated = rotate_channel_secret(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_ne!(rotated.new_secret.as_deref(), Some(original.as_str()));
        delete_channel(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert!(load_definitions(&services)
            .unwrap()
            .get("alerts")
            .unwrap()
            .channels
            .is_empty());
        assert!(list_channel_catalog(&services).unwrap().is_empty());
    }

    #[test]
    fn channel_ports_are_unique_within_a_service() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: construct_protocol::ServiceSummary {
                    name: "alerts".into(),
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    position: 0,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let duplicate = put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        );
        assert!(duplicate.unwrap_err().to_string().contains("already used"));
    }

    #[test]
    fn channel_catalog_migrates_and_controls_exclusive_attachments() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(
            services.join("alerts.toml"),
            "harness = \"smith\"\n[channels.http]\nport = 8787\ntoken = \"secret\"\n",
        )
        .unwrap();
        put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: construct_protocol::ServiceSummary {
                    name: "backup".into(),
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    position: 0,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();

        let catalog = list_channel_catalog(&services).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].attached_to.as_deref(), Some("alerts"));
        assert!(config.path().join("channels.toml").exists());

        let rejected = attach_channel(
            &services,
            construct_protocol::ServiceChannelAttachParams {
                service_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(rejected.to_string().contains("already attached"));

        detach_channel(
            &services,
            construct_protocol::ServiceChannelAttachParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_eq!(
            list_channel_catalog(&services).unwrap()[0].attached_to,
            None
        );

        attach_channel(
            &services,
            construct_protocol::ServiceChannelAttachParams {
                service_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_eq!(
            list_channel_catalog(&services).unwrap()[0]
                .attached_to
                .as_deref(),
            Some("backup")
        );
    }

    #[test]
    fn deleting_a_channel_removes_it_from_the_catalog() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        for name in ["alerts", "backup"] {
            put_definition(
                &services,
                construct_protocol::ServicePutParams {
                    service: construct_protocol::ServiceSummary {
                        name: name.into(),
                        instruction: String::new(),
                        harness: "smith".into(),
                        model: None,
                        session_mode: "headless".into(),
                        cwd: ".".into(),
                        routing: "session-key".into(),
                        paused: false,
                        position: 0,
                        channels: Vec::new(),
                    },
                },
            )
            .unwrap();
        }
        put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap();

        // Another service may not delete a channel out from under its owner.
        let stolen = delete_channel(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(stolen.to_string().contains("attached to service `alerts`"));
        assert_eq!(list_channel_catalog(&services).unwrap().len(), 1);

        // An unattached channel is deleted from the catalog outright.
        detach_channel(
            &services,
            construct_protocol::ServiceChannelAttachParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        delete_channel(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "backup".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert!(list_channel_catalog(&services).unwrap().is_empty());

        // A channel that is not in the catalog at all cannot be deleted.
        let missing = delete_channel(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap_err();
        assert!(missing.to_string().contains("not found in catalog"));
    }

    #[test]
    fn rotating_an_attached_channel_updates_the_catalog_credential() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: construct_protocol::ServiceSummary {
                    name: "alerts".into(),
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    position: 0,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        let created = put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap();
        let original = created.new_secret.unwrap();
        let rotated = rotate_channel_secret(
            &services,
            construct_protocol::ServiceChannelNameParams {
                service_name: "alerts".into(),
                channel_id: "http".into(),
            },
        )
        .unwrap();
        assert_ne!(rotated.new_secret.as_deref(), Some(original.as_str()));
        assert!(list_channel_catalog(&services).unwrap()[0].has_credential);
    }

    #[test]
    fn an_omitted_option_keeps_the_value_the_channel_was_given() {
        // Absent means unchanged, never default: a client that does not offer
        // these fields must be able to save an allowlist without resetting an
        // operator's choice behind their back.
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(
            services.join("chat.toml"),
            "harness = \"codex\"\n\
             [channels.bot]\nkind = \"slack\"\nprogress = \"reaction\"\n\
             follow_up = \"channel\"\nthread_context = 7\n\
             app_token = \"xapp-1\"\nbot_token = \"xoxb-1\"\n",
        )
        .unwrap();

        put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "chat".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap();

        let stored = &load_definitions(&services).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::Reaction);
        assert_eq!(stored.follow_up, SlackFollowUp::Channel);
        assert_eq!(stored.thread_context, 7);
        assert_eq!(stored.allowed_workspaces, vec!["T9".to_string()]);
    }

    /// A Slack channel with every behavior option left at its default.
    fn slack_channel_fixture() -> (tempfile::TempDir, PathBuf) {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(
            services.join("chat.toml"),
            "harness = \"codex\"\n\
             [channels.bot]\nkind = \"slack\"\n\
             app_token = \"xapp-1\"\nbot_token = \"xoxb-1\"\n",
        )
        .unwrap();
        (config, services)
    }

    fn slack_option_put(
        progress: Option<&str>,
        follow_up: Option<&str>,
        thread_context: Option<usize>,
    ) -> construct_protocol::ServiceChannelPutParams {
        construct_protocol::ServiceChannelPutParams {
            service_name: "chat".into(),
            channel: construct_protocol::ServiceChannelPut {
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
            },
            rotate_secret: false,
        }
    }

    #[test]
    fn a_channel_put_sets_the_slack_options_and_reports_them_back() {
        let (_config, services) = slack_channel_fixture();

        let result = put_channel(
            &services,
            slack_option_put(Some("both"), Some("channel"), Some(12)),
        )
        .unwrap();

        // Reported back, so a client can show what it is preserving.
        assert_eq!(result.channel.progress.as_deref(), Some("both"));
        assert_eq!(result.channel.follow_up.as_deref(), Some("channel"));
        assert_eq!(result.channel.thread_context, Some(12));

        let stored = &load_definitions(&services).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::Both);
        assert_eq!(stored.follow_up, SlackFollowUp::Channel);
        assert_eq!(stored.thread_context, 12);
    }

    #[test]
    fn an_unknown_option_value_is_refused_rather_than_defaulted() {
        let (_config, services) = slack_channel_fixture();

        for params in [
            slack_option_put(Some("loud"), None, None),
            slack_option_put(None, Some("everywhere"), None),
        ] {
            let error = put_channel(&services, params).unwrap_err().to_string();
            assert!(error.contains("expected one of"), "unexpected: {error}");
        }
        // A refused edit leaves the stored definition untouched.
        let stored = &load_definitions(&services).unwrap()["chat"].channels["bot"];
        assert_eq!(stored.progress, SlackProgress::default());
        assert_eq!(stored.follow_up, SlackFollowUp::default());
    }

    #[test]
    fn a_thread_context_past_slacks_own_page_limit_is_refused() {
        let (_config, services) = slack_channel_fixture();

        let error = put_channel(
            &services,
            slack_option_put(None, None, Some(THREAD_CONTEXT_MAX + 1)),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("at most"), "unexpected: {error}");
        assert_eq!(
            put_channel(&services, slack_option_put(None, None, Some(0)))
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
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        std::fs::write(services.join("alerts.toml"), "harness = \"codex\"\n").unwrap();

        let error = put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "alerts".into(),
                channel: construct_protocol::ServiceChannelPut {
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
                },
                rotate_secret: false,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("Slack channels only"), "unexpected: {error}");
        // An HTTP channel reports no options rather than defaults it ignores.
        let summary = &list_channel_catalog(&services).unwrap();
        assert!(summary.is_empty(), "the refused put must not have stored");
    }

    #[test]
    fn the_published_option_defaults_match_the_ones_a_definition_gets() {
        // Clients seed a new channel from the published defaults, so a drift
        // here would show the operator a value the daemon would not store.
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
    }

    #[test]
    fn slack_credentials_are_persisted_but_never_returned_in_summaries() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        put_definition(
            &services,
            construct_protocol::ServicePutParams {
                service: construct_protocol::ServiceSummary {
                    name: "chat".into(),
                    instruction: String::new(),
                    harness: "smith".into(),
                    model: None,
                    session_mode: "headless".into(),
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
                    position: 0,
                    channels: Vec::new(),
                },
            },
        )
        .unwrap();
        let result = put_channel(
            &services,
            construct_protocol::ServiceChannelPutParams {
                service_name: "chat".into(),
                channel: construct_protocol::ServiceChannelPut {
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
        let encoded = std::fs::read_to_string(services.join("chat.toml")).unwrap();
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
}
