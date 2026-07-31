//! Loopback-only v1 service ingress.
//!
//! Service definitions live in `services/<name>.toml` in the config dir. This
//! module intentionally owns no public exposure: tunnels and non-HTTP
//! channels are separate capabilities, so enabling a service cannot make a
//! machine reachable from the internet by accident.

use crate::session::SessionManager;
use anyhow::{anyhow, Context, Result};
use construct_protocol::{CreateSessionParams, SessionKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

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
    pub channels: BTreeMap<String, ServiceChannelConfig>,
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
    pub port: Option<u16>,
    pub token: Option<String>,
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
        let Some(channel) = service.channels.get("http").cloned() else {
            continue;
        };
        if channel.kind.as_deref().is_some_and(|kind| kind != "http") {
            tracing::warn!(service = %name, "http channel has a non-http kind; skipping");
            continue;
        }
        let Some(port) = channel.port else {
            tracing::warn!(service = %name, "http service channel has no port; skipping");
            continue;
        };
        let Some(token) = channel.token.filter(|token| !token.is_empty()) else {
            tracing::warn!(service = %name, "http service channel has no token; skipping");
            continue;
        };
        let runtime = ServiceRuntime::load(
            name.clone(),
            service,
            token,
            manager.clone(),
            data_dir.clone(),
        );
        tokio::spawn(async move {
            if let Err(error) = serve(runtime, port).await {
                tracing::error!(service = %name, %error, "service endpoint stopped");
            }
        });
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    sessions: HashMap<String, String>,
}

struct ServiceRuntime {
    name: String,
    config: ServiceConfig,
    token: String,
    manager: Arc<SessionManager>,
    state_path: PathBuf,
    state: Mutex<PersistedState>,
    seen_requests: Mutex<(VecDeque<String>, std::collections::HashSet<String>)>,
}

impl ServiceRuntime {
    fn load(
        name: String,
        config: ServiceConfig,
        token: String,
        manager: Arc<SessionManager>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let state_path = data_dir.join("services").join(format!("{name}.json"));
        let state = std::fs::read(&state_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        Arc::new(Self {
            name,
            config,
            token,
            manager,
            state_path,
            state: Mutex::new(state),
            seen_requests: Mutex::new(Default::default()),
        })
    }

    async fn route(&self, body: ServiceRequest) -> Result<String> {
        if body.message.trim().is_empty() {
            return Err(anyhow!("message must not be empty"));
        }
        if let Some(request_id) = body.request_id.as_deref() {
            let mut seen = self.seen_requests.lock().await;
            if seen.1.contains(request_id) {
                return Err(anyhow!("duplicate request_id"));
            }
            seen.0.push_back(request_id.to_string());
            seen.1.insert(request_id.to_string());
            if seen.0.len() > REQUEST_DEDUP_CAP {
                if let Some(old) = seen.0.pop_front() {
                    seen.1.remove(&old);
                }
            }
        }
        let key = match self.config.routing {
            ServiceRouting::PerEvent => None,
            ServiceRouting::Single => Some("__single__".to_string()),
            ServiceRouting::SessionKey => Some(
                body.session_key
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| anyhow!("session_key is required for session-key routing"))?,
            ),
        };
        if let Some(key) = key {
            // Keep lookup + creation atomic for this service. Without this,
            // two concurrent first deliveries for the same key would each
            // create a conversation and one would become orphaned.
            let mut state = self.state.lock().await;
            let existing = state.sessions.get(&key).cloned();
            if let Some(id) = existing {
                drop(state);
                self.manager.send_input(&id, body.message).await?;
                return Ok(id);
            }
            let id = self
                .create(body.message, Some(format!("service:{}:{key}", self.name)))
                .await?;
            state.sessions.insert(key, id.clone());
            let snapshot = serde_json::to_vec_pretty(&*state)?;
            drop(state);
            if let Some(parent) = self.state_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&self.state_path, snapshot).await?;
            Ok(id)
        } else {
            self.create(body.message, Some(format!("service:{}", self.name)))
                .await
        }
    }

    async fn create(&self, message: String, title: Option<String>) -> Result<String> {
        let prompt = if self.config.instruction.trim().is_empty() {
            message
        } else {
            format!("{}\n\n{}", self.config.instruction.trim(), message)
        };
        self.manager
            .create(CreateSessionParams {
                harness: self.config.harness.clone(),
                cwd: self.config.cwd.clone(),
                prompt: Some(prompt),
                model: self.config.model.clone(),
                title,
                mode: Some("headless".to_string()),
                pty_size: None,
                worktree: false,
                env: HashMap::new(),
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
    tracing::info!(service = %runtime.name, port, "service http endpoint ready (loopback only)");
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
    if request_line != format!("POST /svc/{} HTTP/1.1", runtime.name) {
        return respond(&mut stream, 405, "POST required").await;
    }
    let authorized = lines
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization")
                && value.trim() == format!("Bearer {}", runtime.token)
        });
    if !authorized {
        return respond(&mut stream, 401, "unauthorized").await;
    }
    let result = match serde_json::from_slice::<ServiceRequest>(&bytes[end..]) {
        Ok(request) => runtime.route(request).await,
        Err(_) => Err(anyhow!("invalid JSON")),
    };
    match result {
        Ok(session) => {
            json_response(
                &mut stream,
                202,
                &serde_json::json!({"accepted": true, "service": runtime.name, "session": session}),
            )
            .await
        }
        Err(error) => respond(&mut stream, 400, &error.to_string()).await,
    }
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
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
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
    #[test]
    fn header_boundary_and_content_length() {
        let request = b"POST / HTTP/1.1\r\nContent-Length: 12\r\n\r\nhello world!";
        let end = find_headers_end(request).unwrap();
        assert_eq!(content_length(&request[..end]).unwrap(), 12);
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
}
