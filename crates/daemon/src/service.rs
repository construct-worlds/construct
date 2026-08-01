//! Loopback-only v1 service ingress.
//!
//! Service definitions live in `services/<name>.toml` in the config dir. This
//! module intentionally owns no public exposure: tunnels and non-HTTP
//! channels are separate capabilities, so enabling a service cannot make a
//! machine reachable from the internet by accident.

use crate::session::SessionManager;
use anyhow::{anyhow, Context, Result};
use construct_protocol::{
    CreateSessionParams, MessageRole, SessionEvent, SessionKind, SessionState,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_HTTP_BYTES: usize = 1024 * 1024;
const REQUEST_DEDUP_CAP: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub instruction: String,
    #[serde(default = "default_service_harness")]
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_service_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub routing: ServiceRouting,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub sandbox: ServiceSandboxConfig,
    #[serde(default)]
    pub channels: BTreeMap<String, ServiceChannelConfig>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceChannelConfig {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_channel_enabled")]
    pub enabled: bool,
    pub port: Option<u16>,
    pub token: Option<String>,
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
    Ok(load_definitions(dir)?
        .into_iter()
        .map(|(name, config)| summary(name, &config))
        .collect())
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
    let config = ServiceConfig {
        instruction: params.service.instruction,
        harness: params.service.harness,
        model: params.service.model,
        cwd: params.service.cwd,
        routing,
        paused: params.service.paused,
        sandbox,
        channels,
    };
    write_definition(dir, &params.service.name, &config)?;
    Ok(construct_protocol::ServicePutResult {
        service: summary(params.service.name, &config),
        restart_required: true,
    })
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
    dir.parent()
        .unwrap_or(dir)
        .join("channels.toml")
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

fn channel_owners(
    services: &BTreeMap<String, ServiceConfig>,
) -> BTreeMap<String, Vec<String>> {
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
    if params.channel.kind != "http" {
        return Err(anyhow!(
            "unsupported channel kind `{}`; v1 supports `http`",
            params.channel.kind
        ));
    }
    let port = params
        .channel
        .port
        .filter(|port| *port > 0)
        .ok_or_else(|| anyhow!("HTTP channel port must be between 1 and 65535"))?;
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
    if catalog.channels.iter().any(|(id, channel)| {
        id != &params.channel.id && channel.port == Some(port)
    }) {
        return Err(anyhow!("HTTP port {port} is already used by this service"));
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
                params.channel.id, params.channel.kind
            ));
        }
    }
    let new_secret = if params.rotate_secret
        || existing
            .as_ref()
            .and_then(|channel| channel.token.as_deref())
            .is_none()
    {
        Some(generate_channel_secret())
    } else {
        None
    };
    let token = new_secret.clone().or_else(|| {
        existing
            .as_ref()
            .and_then(|channel| channel.token.clone())
    });
    let config = ServiceChannelConfig {
        kind: Some(params.channel.kind),
        enabled: params.channel.enabled,
        port: Some(port),
        token,
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
        restart_required: true,
    })
}

pub fn delete_channel(
    dir: &std::path::Path,
    params: construct_protocol::ServiceChannelNameParams,
) -> Result<()> {
    detach_channel(
        dir,
        construct_protocol::ServiceChannelAttachParams {
            service_name: params.service_name,
            channel_id: params.channel_id,
        },
    )
    .map(|_| ())
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
    if service
        .channels
        .iter()
        .any(|(id, channel)| id != &params.channel_id && channel.port == config.port)
    {
        return Err(anyhow!(
            "HTTP port {:?} is already used by this service",
            config.port
        ));
    }
    service
        .channels
        .insert(params.channel_id.clone(), config.clone());
    write_definition(dir, &params.service_name, service)?;
    Ok(construct_protocol::ServiceChannelPutResult {
        channel: channel_summary(
            params.channel_id,
            &config,
            Some(params.service_name),
        ),
        new_secret: None,
        restart_required: true,
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
    let config = service
        .channels
        .remove(&params.channel_id)
        .ok_or_else(|| {
            anyhow!(
                "channel `{}` is not attached to service `{}`",
                params.channel_id, params.service_name
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
        restart_required: true,
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
                params.channel_id, params.service_name
            )
        })?;
    if channel_kind(&params.channel_id, channel) != "http" {
        return Err(anyhow!("only HTTP channel credentials can be rotated in v1"));
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
        restart_required: true,
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

fn channel_kind(id: &str, config: &ServiceChannelConfig) -> String {
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

fn channel_summary(
    id: String,
    config: &ServiceChannelConfig,
    attached_to: Option<String>,
) -> construct_protocol::ServiceChannelSummary {
    construct_protocol::ServiceChannelSummary {
        id: id.clone(),
        kind: channel_kind(&id, config),
        enabled: config.enabled,
        port: config.port,
        has_credential: config.token.as_ref().is_some_and(|token| !token.is_empty()),
        attached_to,
    }
}

fn summary(name: String, config: &ServiceConfig) -> construct_protocol::ServiceSummary {
    let service_name = name.clone();
    construct_protocol::ServiceSummary {
        name,
        instruction: config.instruction.clone(),
        harness: config.harness.clone(),
        model: config.model.clone(),
        cwd: config.cwd.clone(),
        routing: match config.routing {
            ServiceRouting::PerEvent => "per-event",
            ServiceRouting::SessionKey => "session-key",
            ServiceRouting::Single => "single",
        }
        .to_string(),
        paused: config.paused,
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

pub fn spawn_all(
    manager: Arc<SessionManager>,
    services: BTreeMap<String, ServiceConfig>,
    data_dir: PathBuf,
) {
    for (name, service) in services {
        if service.paused {
            continue;
        }
        let shared = ServiceShared::load(name.clone(), service.clone(), manager.clone(), data_dir.clone());
        for (channel_id, channel) in service.channels {
            if !channel.enabled {
                continue;
            }
            if channel_kind(&channel_id, &channel) != "http" {
                tracing::warn!(service = %name, channel = %channel_id, "unsupported service channel kind; skipping");
                continue;
            }
            let Some(port) = channel.port else {
                tracing::warn!(service = %name, channel = %channel_id, "HTTP channel has no port; skipping");
                continue;
            };
            let Some(token) = channel.token.filter(|token| !token.is_empty()) else {
                tracing::warn!(service = %name, channel = %channel_id, "HTTP channel has no token; skipping");
                continue;
            };
            let runtime = Arc::new(ServiceRuntime {
                channel_id: channel_id.clone(),
                token,
                shared: shared.clone(),
            });
            let service_name = name.clone();
            tokio::spawn(async move {
                if let Err(error) = serve(runtime, port).await {
                    tracing::error!(service = %service_name, channel = %channel_id, %error, "service endpoint stopped");
                }
            });
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    sessions: HashMap<String, String>,
    /// Every session this service is allowed to expose through its result
    /// endpoint. This is broader than `sessions`: per-event sessions have no
    /// routing key but still need to remain queryable.
    #[serde(default)]
    owned_sessions: HashSet<String>,
}

impl PersistedState {
    fn normalize_legacy_ownership(&mut self) {
        self.owned_sessions.extend(self.sessions.values().cloned());
    }
}

struct ServiceShared {
    name: String,
    config: ServiceConfig,
    manager: Arc<SessionManager>,
    state_path: PathBuf,
    state: Mutex<PersistedState>,
    seen_requests: Mutex<(VecDeque<String>, std::collections::HashSet<String>)>,
}

impl ServiceShared {
    fn load(
        name: String,
        config: ServiceConfig,
        manager: Arc<SessionManager>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let state_path = data_dir.join("services").join(format!("{name}.json"));
        let mut state: PersistedState = std::fs::read(&state_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        state.normalize_legacy_ownership();
        Arc::new(Self {
            name,
            config,
            manager,
            state_path,
            state: Mutex::new(state),
            seen_requests: Mutex::new(Default::default()),
        })
    }
}

struct ServiceRuntime {
    channel_id: String,
    token: String,
    shared: Arc<ServiceShared>,
}

impl ServiceRuntime {
    fn name(&self) -> &str {
        &self.shared.name
    }

    async fn route(&self, body: ServiceRequest) -> Result<String> {
        if body.message.trim().is_empty() {
            return Err(anyhow!("message must not be empty"));
        }
        if let Some(request_id) = body.request_id.as_deref() {
            let mut seen = self.shared.seen_requests.lock().await;
            let request_key = format!("{}:{request_id}", self.channel_id);
            if seen.1.contains(&request_key) {
                return Err(anyhow!("duplicate request_id"));
            }
            seen.0.push_back(request_key.clone());
            seen.1.insert(request_key);
            if seen.0.len() > REQUEST_DEDUP_CAP {
                if let Some(old) = seen.0.pop_front() {
                    seen.1.remove(&old);
                }
            }
        }
        let key = match self.shared.config.routing {
            ServiceRouting::PerEvent => None,
            ServiceRouting::Single => Some("__single__".to_string()),
            ServiceRouting::SessionKey => Some(
                body.session_key
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| anyhow!("session_key is required for session-key routing"))?,
            ),
        };
        if let Some(key) = key {
            let lookup_key = format!("{}:{key}", self.channel_id);
            // Keep lookup + creation atomic for this service. Without this,
            // two concurrent first deliveries for the same key would each
            // create a conversation and one would become orphaned.
            let mut state = self.shared.state.lock().await;
            let existing = state.sessions.get(&lookup_key).cloned().or_else(|| {
                // State written by the original single-channel v1 runtime used
                // the bare session key. Preserve those conversations when the
                // legacy channel id is still `http`.
                (self.channel_id == "http")
                    .then(|| state.sessions.get(&key).cloned())
                    .flatten()
            });
            if let Some(id) = existing {
                drop(state);
                self.shared.manager.send_input(&id, body.message).await?;
                return Ok(id);
            }
            let id = self
                .create(body.message, Some(format!("service:{}:{}:{key}", self.shared.name, self.channel_id)))
                .await?;
            state.sessions.insert(lookup_key, id.clone());
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(id)
        } else {
            let id = self
                .create(body.message, Some(format!("service:{}:{}", self.shared.name, self.channel_id)))
                .await?;
            let mut state = self.shared.state.lock().await;
            state.owned_sessions.insert(id.clone());
            self.persist_state(&state).await?;
            Ok(id)
        }
    }

    async fn persist_state(&self, state: &PersistedState) -> Result<()> {
        let snapshot = serde_json::to_vec_pretty(state)?;
        if let Some(parent) = self.shared.state_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&self.shared.state_path, snapshot).await?;
        Ok(())
    }

    async fn session_result(&self, session_id: &str) -> Result<Option<serde_json::Value>> {
        let owned = self
            .shared
            .state
            .lock()
            .await
            .owned_sessions
            .contains(session_id);
        if !owned {
            return Ok(None);
        }
        let Ok(detail) = self.shared.manager.detail(session_id).await else {
            return Ok(None);
        };
        let reply = latest_assistant_reply(detail.events.iter().map(|event| &event.event));
        let ready = matches!(
            detail.summary.state,
            SessionState::AwaitingInput | SessionState::Done | SessionState::Errored
        );
        Ok(Some(serde_json::json!({
            "service": self.shared.name,
            "channel": self.channel_id,
            "session": session_id,
            "status": detail.summary.state,
            "ready": ready,
            "reply": reply,
        })))
    }

    async fn create(&self, message: String, title: Option<String>) -> Result<String> {
        let prompt = if self.shared.config.instruction.trim().is_empty() {
            message
        } else {
            format!("{}\n\n{}", self.shared.config.instruction.trim(), message)
        };
        self.shared.manager
            .create(CreateSessionParams {
                harness: self.shared.config.harness.clone(),
                cwd: self.shared.config.cwd.clone(),
                prompt: Some(prompt),
                model: self.shared.config.model.clone(),
                title,
                mode: Some("headless".to_string()),
                pty_size: None,
                worktree: false,
                env: self.shared.config.sandbox.session_env(),
                args: Vec::new(),
                kind: SessionKind::User,
                parent_session_id: None,
                group_id: None,
                position_after_session_id: None,
                forked_from: None,
            })
            .await
    }
}

#[derive(Deserialize)]
struct ServiceRequest {
    message: String,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
}

async fn serve(runtime: Arc<ServiceRuntime>, port: u16) -> Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context("bind loopback service endpoint")?;
    tracing::info!(service = %runtime.name(), channel = %runtime.channel_id, port, "service http endpoint ready (loopback only)");
    loop {
        let (stream, _) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, runtime).await {
                tracing::debug!(%error, "service request failed");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, runtime: Arc<ServiceRuntime>) -> Result<()> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > MAX_HTTP_BYTES {
            return respond(&mut stream, 413, "request too large").await;
        }
        if let Some(end) = find_headers_end(&bytes) {
            let length = content_length(&bytes[..end])?;
            while bytes.len() < end + length {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return respond(&mut stream, 400, "truncated request").await;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            break;
        }
    }
    let end = find_headers_end(&bytes).unwrap();
    let headers = std::str::from_utf8(&bytes[..end]).map_err(|_| anyhow!("invalid headers"))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let route = match parse_http_route(runtime.name(), request_line) {
        Ok(route) => route,
        Err((status, message)) => return respond(&mut stream, status, message).await,
    };
    let authorized = lines
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization")
                && value.trim() == format!("Bearer {}", runtime.token)
        });
    if !authorized {
        return respond(&mut stream, 401, "unauthorized").await;
    }
    match route {
        HttpRoute::Submit => {
            let result = match serde_json::from_slice::<ServiceRequest>(&bytes[end..]) {
                Ok(request) => runtime.route(request).await,
                Err(_) => Err(anyhow!("invalid JSON")),
            };
            match result {
                Ok(session) => {
                    json_response(
                        &mut stream,
                        202,
                        &serde_json::json!({
                            "accepted": true,
                "service": runtime.name(),
                "channel": runtime.channel_id,
                            "session": session,
                        }),
                    )
                    .await
                }
                Err(error) => respond(&mut stream, 400, &error.to_string()).await,
            }
        }
        HttpRoute::Session(session_id) => match runtime.session_result(&session_id).await? {
            Some(result) => json_response(&mut stream, 200, &result).await,
            None => respond(&mut stream, 404, "session not found").await,
        },
    }
}

/// Reconstruct the caller-facing reply from a session transcript.
///
/// Harnesses stream an assistant turn as many small `Message` events (one per
/// token delta), so the reply is the *concatenation* of the trailing assistant
/// run, not its last element. Walking backwards, transcript bookkeeping
/// (status, cost, usage, reasoning) is skipped, and collection stops at the
/// first boundary that ends the turn's final answer — the user's own message,
/// or a tool call whose narration precedes it — so a tool-using turn reports
/// the answer rather than the commentary leading up to it.
fn latest_assistant_reply<'a>(
    events: impl DoubleEndedIterator<Item = &'a SessionEvent>,
) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for event in events.rev() {
        match event {
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text,
            } => parts.push(text.as_str()),
            SessionEvent::Message { .. }
            | SessionEvent::ToolUse { .. }
            | SessionEvent::ToolResult { .. } => break,
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.concat())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HttpRoute {
    Submit,
    Session(String),
}

fn parse_http_route(
    service_name: &str,
    request_line: &str,
) -> std::result::Result<HttpRoute, (u16, &'static str)> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if parts.next().is_some() || !version.starts_with("HTTP/") {
        return Err((400, "invalid request line"));
    }
    let submit = format!("/svc/{service_name}");
    if target == submit {
        return if method == "POST" {
            Ok(HttpRoute::Submit)
        } else {
            Err((405, "POST required"))
        };
    }
    let session_prefix = format!("{submit}/sessions/");
    if let Some(session_id) = target.strip_prefix(&session_prefix) {
        if session_id.is_empty() || session_id.contains('/') {
            return Err((404, "not found"));
        }
        return if method == "GET" {
            Ok(HttpRoute::Session(session_id.to_string()))
        } else {
            Err((405, "GET required"))
        };
    }
    Err((404, "not found"))
}

fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|x| x == b"\r\n\r\n")
        .map(|i| i + 4)
}
fn content_length(headers: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(headers)?;
    Ok(text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse().ok())
        })
        .unwrap_or(0))
}
async fn respond(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    json_response(stream, status, &serde_json::json!({"error": message})).await
}
async fn json_response(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    stream.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(env.get("CONSTRUCT_INJECT_MCP").map(String::as_str), Some("0"));
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
            cwd: ".".into(),
            routing: ServiceRouting::SessionKey,
            paused: false,
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
    fn service_put_preserves_channels_and_channel_crud_rotates_credentials() {
        let config = tempfile::tempdir().unwrap();
        let services = config.path().join("services");
        std::fs::create_dir_all(&services).unwrap();
        let service = construct_protocol::ServiceSummary {
            name: "alerts".into(),
            instruction: "triage".into(),
            harness: "smith".into(),
            model: None,
            cwd: ".".into(),
            routing: "session-key".into(),
            paused: false,
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
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
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
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
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

        delete_channel(
            &services,
            construct_protocol::ServiceChannelNameParams {
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
                    cwd: ".".into(),
                    routing: "session-key".into(),
                    paused: false,
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
}
