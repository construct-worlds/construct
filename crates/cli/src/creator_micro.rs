//! Native Work Louder Creator Micro 2 control-surface support.
//!
//! The device carries a small JSON-RPC server on HID report 6. Construct opens
//! that report pipe non-exclusively, so the board remains a normal keyboard and
//! works over either USB or Bluetooth. The integration is opt-in through
//! `creator-micro.toml`; this avoids competing with Work Louder Input or another
//! agent-status host for the device-wide thread LEDs.

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Subcommand;
use construct_protocol::paths::Paths;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::midi::MidiAction;

const VENDOR_ID: u16 = 0x303a;
const PRODUCT_IDS: [u16; 3] = [0x8298, 0x8297, 0x8360];
const VENDOR_USAGE_PAGE: u16 = 0xff00;
const VENDOR_USAGE: u16 = 0x0001;
const REPORT_ID: u8 = 0x06;
const CHANNEL_RPC: u8 = 2;
const REPORT_SIZE: usize = 64;
const MAX_PAYLOAD: usize = 61;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const FEEDBACK_HEARTBEAT: Duration = Duration::from_secs(10);
const LIGHTING_WRITE_COOLDOWN: Duration = Duration::from_millis(50);
const SESSION_KEY_COUNT: usize = 6;
const PANE_KEY_COUNT: usize = 4;
const THREAD_KEY_COUNT: usize = 13;

#[derive(Debug, Clone, Subcommand)]
pub enum CreatorMicroCommand {
    /// Show whether native Creator Micro control is enabled and reachable.
    Status,
    /// List connected compatible Work Louder devices.
    Devices,
    /// Open the vendor channel and query the firmware without changing the device.
    Probe,
    /// Enable the control surface for subsequently opened TUIs.
    Enable,
    /// Stop Construct from opening or lighting the control surface.
    Disable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct CreatorMicroConfig {
    enabled: bool,
}

impl Default for CreatorMicroConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

impl CreatorMicroConfig {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).with_context(|| format!("parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("Creator Micro config path has no parent directory")?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let raw = toml::to_string_pretty(self).context("serialize Creator Micro config")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary file in {}", parent.display()))?;
        use std::io::Write as _;
        temp.write_all(raw.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

pub fn run(command: Option<CreatorMicroCommand>) -> Result<()> {
    let path = Paths::discover().creator_micro_file();
    match command.unwrap_or(CreatorMicroCommand::Status) {
        CreatorMicroCommand::Status => {
            let config = CreatorMicroConfig::load(&path)?;
            println!("config: {}", path.display());
            println!("enabled: {}", config.enabled);
            print_devices()
        }
        CreatorMicroCommand::Devices => print_devices(),
        CreatorMicroCommand::Probe => probe_device(),
        CreatorMicroCommand::Enable => {
            CreatorMicroConfig { enabled: true }.save(&path)?;
            println!("Creator Micro control enabled in {}", path.display());
            println!("Open a new Construct TUI to connect.");
            println!();
            println!("The active Work Louder layer must use these Input keycodes:");
            println!("  all 13 keys: KV_OAI_AG00 through KV_OAI_AG12");
            println!("  encoder: KV_OAI_ENC_CC / KV_OAI_ENC_CW / KV_OAI_ENC_CLK");
            println!("  legacy KV_OAI_ACT06 through KV_OAI_ACT12 inputs still work,");
            println!("  but AG06 through AG12 are required for individual lighting.");
            Ok(())
        }
        CreatorMicroCommand::Disable => {
            CreatorMicroConfig { enabled: false }.save(&path)?;
            println!("Creator Micro control disabled in {}", path.display());
            println!("A currently open TUI releases it when that TUI exits.");
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CreatorMicroSnapshot {
    /// Low six bits say which physical session keys currently have a session.
    pub assigned: u8,
    pub active: u8,
    pub attention: u8,
    /// Number of visible split panes, capped at the four hardware pane keys.
    pub pane_count: u8,
    /// Zero-based visible pane ordinal when a split pane owns keyboard focus.
    pub focused_pane: Option<u8>,
    /// Aggregate fleet state drives the device underglow.
    pub fleet_active: bool,
    pub fleet_attention: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorMicroEvent {
    Session(usize),
    Pane(usize),
    Enter,
    Approve,
    Reject,
    Action(MidiAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CreatorMicroLinkStatus {
    Connected { product: String, transport: String },
    Disconnected,
}

pub(crate) struct CreatorMicroSurface {
    feedback_tx: std_mpsc::Sender<CreatorMicroSnapshot>,
    last_snapshot: Cell<CreatorMicroSnapshot>,
    stop: Arc<AtomicBool>,
}

impl CreatorMicroSurface {
    pub(crate) fn update(&self, snapshot: CreatorMicroSnapshot) {
        if self.last_snapshot.replace(snapshot) != snapshot {
            let _ = self.feedback_tx.send(snapshot);
        }
    }
}

impl Drop for CreatorMicroSurface {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

type EventReceiver = mpsc::UnboundedReceiver<CreatorMicroEvent>;
type LinkReceiver = mpsc::UnboundedReceiver<CreatorMicroLinkStatus>;

pub(crate) fn start_surface() -> Result<Option<(CreatorMicroSurface, EventReceiver, LinkReceiver)>>
{
    let config = CreatorMicroConfig::load(&Paths::discover().creator_micro_file())?;
    if !config.enabled {
        return Ok(None);
    }
    start_surface_platform()
}

#[cfg(target_os = "macos")]
fn start_surface_platform() -> Result<Option<(CreatorMicroSurface, EventReceiver, LinkReceiver)>> {
    let (feedback_tx, feedback_rx) = std_mpsc::channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (link_tx, link_rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("construct-creator-micro".into())
        .spawn(move || worker_loop(feedback_rx, event_tx, link_tx, worker_stop))
        .context("spawn Creator Micro thread")?;
    Ok(Some((
        CreatorMicroSurface {
            feedback_tx,
            last_snapshot: Cell::new(CreatorMicroSnapshot::default()),
            stop,
        },
        event_rx,
        link_rx,
    )))
}

#[cfg(not(target_os = "macos"))]
fn start_surface_platform() -> Result<Option<(CreatorMicroSurface, EventReceiver, LinkReceiver)>> {
    anyhow::bail!("native Creator Micro control is currently supported on macOS")
}

#[cfg(target_os = "macos")]
fn worker_loop(
    feedback_rx: std_mpsc::Receiver<CreatorMicroSnapshot>,
    event_tx: mpsc::UnboundedSender<CreatorMicroEvent>,
    link_tx: mpsc::UnboundedSender<CreatorMicroLinkStatus>,
    stop: Arc<AtomicBool>,
) {
    let mut snapshot = CreatorMicroSnapshot::default();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(next) = feedback_rx.try_recv() {
            snapshot = next;
        }
        match run_connection(&feedback_rx, &event_tx, &link_tx, &stop, &mut snapshot) {
            Ok(()) => break,
            Err(error) => tracing::debug!(%error, "Creator Micro disconnected"),
        }
        // Emit the initial waiting state too, so an enabled but sleeping
        // wireless board has an honest hollow indicator in the modeline.
        let _ = link_tx.send(CreatorMicroLinkStatus::Disconnected);
        let deadline = Instant::now() + RECONNECT_DELAY;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(target_os = "macos")]
struct OpenedCreatorMicro {
    device: hidapi::HidDevice,
    product: String,
    transport: String,
}

#[cfg(target_os = "macos")]
fn open_creator_micro() -> Result<OpenedCreatorMicro> {
    use hidapi::HidApi;

    let api = HidApi::new().context("initialize HID")?;
    // Be explicit even though the dependency feature sets this during first
    // initialization. hidapi's setting is process-global, so another caller
    // could otherwise change it before a reconnect attempt.
    api.set_open_exclusive(false);
    let mut candidates = api
        .device_list()
        .filter(|device| {
            device.vendor_id() == VENDOR_ID && PRODUCT_IDS.contains(&device.product_id())
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|device| {
        let vendor_collection =
            device.usage_page() == VENDOR_USAGE_PAGE && device.usage() == VENDOR_USAGE;
        let wired = matches!(device.bus_type(), hidapi::BusType::Usb);
        (
            !vendor_collection,
            !wired,
            device.path().to_bytes().to_vec(),
        )
    });
    let info = candidates
        .first()
        .context("no Creator Micro 2 found; wake it or connect USB-C")?;
    let device = info
        .open_device(&api)
        .context("open Creator Micro 2 vendor channel non-exclusively")?;
    let product = info
        .product_string()
        .unwrap_or("Creator Micro 2")
        .to_string();
    let transport = match info.bus_type() {
        hidapi::BusType::Usb => "USB",
        hidapi::BusType::Bluetooth => "Bluetooth",
        hidapi::BusType::I2c => "I2C",
        hidapi::BusType::Spi => "SPI",
        hidapi::BusType::Unknown => "unknown",
    }
    .to_string();
    Ok(OpenedCreatorMicro {
        device,
        product,
        transport,
    })
}

#[cfg(target_os = "macos")]
fn run_connection(
    feedback_rx: &std_mpsc::Receiver<CreatorMicroSnapshot>,
    event_tx: &mpsc::UnboundedSender<CreatorMicroEvent>,
    link_tx: &mpsc::UnboundedSender<CreatorMicroLinkStatus>,
    stop: &AtomicBool,
    snapshot: &mut CreatorMicroSnapshot,
) -> Result<()> {
    let OpenedCreatorMicro {
        device,
        product,
        transport,
    } = open_creator_micro()?;
    let _ = link_tx.send(CreatorMicroLinkStatus::Connected { product, transport });

    let mut request_id = 1u16;
    send_feedback(&device, *snapshot, &mut request_id)?;
    let mut last_feedback = Instant::now();
    let mut reassembler = JsonReassembler::default();
    let mut buffer = [0u8; REPORT_SIZE];

    while !stop.load(Ordering::Relaxed) {
        let mut changed = false;
        while let Ok(next) = feedback_rx.try_recv() {
            if *snapshot != next {
                *snapshot = next;
                changed = true;
            }
        }
        if changed || last_feedback.elapsed() >= FEEDBACK_HEARTBEAT {
            send_feedback(&device, *snapshot, &mut request_id)?;
            last_feedback = Instant::now();
        }

        let count = device
            .read_timeout(&mut buffer, 100)
            .context("read Creator Micro report")?;
        if count < 3 || buffer[0] != REPORT_ID || buffer[1] != CHANNEL_RPC {
            continue;
        }
        let payload_len = usize::from(buffer[2]);
        if payload_len > MAX_PAYLOAD || 3 + payload_len > count {
            continue;
        }
        for message in reassembler.push(&buffer[3..3 + payload_len]) {
            if let Some(event) = event_from_message(&message) {
                let _ = event_tx.send(event);
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn send_feedback(
    device: &hidapi::HidDevice,
    snapshot: CreatorMicroSnapshot,
    request_id: &mut u16,
) -> Result<()> {
    let preview = lighting_preview_message(snapshot, *request_id);
    *request_id = (*request_id + 1) % 999;
    write_rpc(device, &preview).context("write Creator Micro underglow")?;
    std::thread::sleep(LIGHTING_WRITE_COOLDOWN);

    let message = thread_status_message(snapshot, *request_id);
    *request_id = (*request_id + 1) % 999;
    write_rpc(device, &message).context("write Creator Micro key feedback")
}

fn thread_status_message(snapshot: CreatorMicroSnapshot, request_id: u16) -> Value {
    let params = (0..THREAD_KEY_COUNT)
        .map(|slot| {
            let session_bit = if slot < SESSION_KEY_COUNT {
                1u8 << slot
            } else {
                0
            };
            if slot < SESSION_KEY_COUNT && snapshot.attention & session_bit != 0 {
                json!({"id": slot, "c": 0x00c853, "b": 1.0, "e": 6, "s": 0.75})
            } else if slot < SESSION_KEY_COUNT && snapshot.active & session_bit != 0 {
                json!({"id": slot, "c": 0xffc400, "b": 0.9, "e": 4, "s": 0.6})
            } else if slot < SESSION_KEY_COUNT && snapshot.assigned & session_bit != 0 {
                json!({"id": slot, "c": 0x2d7ff9, "b": 0.22, "e": 1, "s": 0.0})
            } else if (SESSION_KEY_COUNT..SESSION_KEY_COUNT + PANE_KEY_COUNT).contains(&slot) {
                let pane = (slot - SESSION_KEY_COUNT) as u8;
                if snapshot.focused_pane == Some(pane) {
                    json!({"id": slot, "c": 0x5ce1ff, "b": 1.0, "e": 1, "s": 0.0})
                } else if pane < snapshot.pane_count {
                    json!({"id": slot, "c": 0x2d7ff9, "b": 0.28, "e": 1, "s": 0.0})
                } else {
                    json!({"id": slot, "c": 0, "b": 0.0, "e": 0, "s": 0.0})
                }
            } else if slot == 10 {
                json!({"id": slot, "c": 0x00c853, "b": 0.5, "e": 1, "s": 0.0})
            } else if slot == 11 {
                json!({"id": slot, "c": 0xff4d5f, "b": 0.5, "e": 1, "s": 0.0})
            } else if slot == 12 {
                json!({"id": slot, "c": 0xffffff, "b": 0.6, "e": 1, "s": 0.0})
            } else {
                json!({"id": slot, "c": 0, "b": 0.0, "e": 0, "s": 0.0})
            }
        })
        .collect::<Vec<_>>();
    json!({
        "method": "v.oai.thstatus",
        "params": params,
        "id": request_id,
    })
}

fn lighting_preview_message(snapshot: CreatorMicroSnapshot, request_id: u16) -> Value {
    let (effect, brightness, speed, color) = if snapshot.fleet_attention {
        ("breath", 0.65, 0.6, 0x00c853)
    } else if snapshot.fleet_active {
        ("breath", 0.5, 0.6, 0xffc400)
    } else {
        ("solid", 0.18, 0.0, 0x2d7ff9)
    };
    json!({
        "method": "lights.preview",
        "params": {
            "backlight": {
                "effect": "off", "brightness": 0.0, "speed": 0.0,
                "magic": 0.0, "color": 0
            },
            "underglow": {
                "effect": effect, "brightness": brightness, "speed": speed,
                "magic": 0.0, "color": color
            }
        },
        "id": request_id,
    })
}

#[cfg(target_os = "macos")]
fn write_rpc(device: &hidapi::HidDevice, message: &Value) -> Result<()> {
    let message = serde_json::to_vec(message)?;
    let framed = [b"\r\n".as_slice(), message.as_slice(), b"\r\n".as_slice()].concat();
    for chunk in framed.chunks(MAX_PAYLOAD) {
        let mut packet = [0u8; REPORT_SIZE];
        packet[0] = REPORT_ID;
        packet[1] = CHANNEL_RPC;
        packet[2] = chunk.len() as u8;
        packet[3..3 + chunk.len()].copy_from_slice(chunk);
        device
            .write(&packet)
            .context("write Creator Micro vendor report")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn probe_device() -> Result<()> {
    const PROBE_ID: u64 = 991;
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

    let OpenedCreatorMicro {
        device,
        product,
        transport,
    } = open_creator_micro()?;
    println!("opened: {product} over {transport}");
    println!(
        "access: {}",
        if device.is_open_exclusive()? {
            "exclusive"
        } else {
            "shared"
        }
    );
    write_rpc(
        &device,
        &json!({"method": "sys.version", "params": Value::Null, "id": PROBE_ID}),
    )
    .context("send read-only firmware query")?;

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut reassembler = JsonReassembler::default();
    let mut buffer = [0u8; REPORT_SIZE];
    while Instant::now() < deadline {
        let count = device
            .read_timeout(&mut buffer, 100)
            .context("read firmware query response")?;
        if count < 3 || buffer[0] != REPORT_ID || buffer[1] != CHANNEL_RPC {
            continue;
        }
        let payload_len = usize::from(buffer[2]);
        if payload_len > MAX_PAYLOAD || 3 + payload_len > count {
            continue;
        }
        for message in reassembler.push(&buffer[3..3 + payload_len]) {
            if message.get("id").and_then(Value::as_u64) != Some(PROBE_ID) {
                continue;
            }
            if let Some(error) = message.get("error") {
                anyhow::bail!("firmware query failed: {error}");
            }
            println!(
                "firmware: {}",
                message.get("result").unwrap_or(&Value::Null)
            );
            println!("vendor channel: ready");
            return Ok(());
        }
    }
    anyhow::bail!("timed out waiting for the firmware response")
}

#[cfg(not(target_os = "macos"))]
fn probe_device() -> Result<()> {
    anyhow::bail!("native Creator Micro control is currently supported on macOS")
}

#[derive(Default)]
struct JsonReassembler {
    bytes: Vec<u8>,
}

impl JsonReassembler {
    fn push(&mut self, fragment: &[u8]) -> Vec<Value> {
        self.bytes.extend_from_slice(fragment);
        let mut messages = Vec::new();
        while let Some(end) = self
            .bytes
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let line = self.bytes.drain(..end).collect::<Vec<_>>();
            self.bytes.remove(0);
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                messages.push(value);
            }
        }
        // Accept an unframed complete message as well. This is useful for
        // older firmware and keeps the parser tolerant of missing final CRLF.
        if let Ok(value) = serde_json::from_slice::<Value>(&self.bytes) {
            messages.push(value);
            self.bytes.clear();
        } else if self.bytes.len() > 64 * 1024 {
            self.bytes.clear();
        }
        messages
    }
}

fn event_from_message(message: &Value) -> Option<CreatorMicroEvent> {
    if message.get("m")?.as_str()? != "v.oai.hid" {
        return None;
    }
    let params = message.get("p")?;
    let key = params.get("k")?.as_str()?;
    let action = params.get("act")?.as_u64()?;
    if action == 0 {
        return None;
    }
    if let Some(slot) = key
        .strip_prefix("AG")
        .and_then(|slot| slot.parse::<usize>().ok())
    {
        return Some(match slot {
            0..=5 => CreatorMicroEvent::Session(slot),
            6..=9 => CreatorMicroEvent::Pane(slot - 5),
            10 => CreatorMicroEvent::Approve,
            11 => CreatorMicroEvent::Reject,
            12 => CreatorMicroEvent::Enter,
            _ => return None,
        });
    }
    Some(match key {
        "ACT06" => CreatorMicroEvent::Pane(1),
        "ACT07" => CreatorMicroEvent::Pane(2),
        "ACT08" => CreatorMicroEvent::Pane(3),
        "ACT09" => CreatorMicroEvent::Pane(4),
        "ACT10" => CreatorMicroEvent::Approve,
        "ACT11" => CreatorMicroEvent::Reject,
        "ACT12" => CreatorMicroEvent::Enter,
        "ENC_CC" => CreatorMicroEvent::Action(MidiAction::ScrollUp),
        "ENC_CW" => CreatorMicroEvent::Action(MidiAction::ScrollDown),
        "ENC_CLK" => CreatorMicroEvent::Action(MidiAction::SwitchFocus),
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
fn print_devices() -> Result<()> {
    let api = hidapi::HidApi::new().context("initialize HID")?;
    let devices = api
        .device_list()
        .filter(|device| {
            device.vendor_id() == VENDOR_ID && PRODUCT_IDS.contains(&device.product_id())
        })
        .collect::<Vec<_>>();
    if devices.is_empty() {
        println!("(no Creator Micro found; wake it or connect USB-C)");
        return Ok(());
    }
    for device in devices {
        let transport = match device.bus_type() {
            hidapi::BusType::Usb => "USB",
            hidapi::BusType::Bluetooth => "Bluetooth",
            hidapi::BusType::I2c => "I2C",
            hidapi::BusType::Spi => "SPI",
            hidapi::BusType::Unknown => "unknown",
        };
        println!(
            "{} ({:04x}:{:04x}, {transport}, usage {:04x}/{:04x})",
            device.product_string().unwrap_or("Creator Micro"),
            device.vendor_id(),
            device.product_id(),
            device.usage_page(),
            device.usage(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn print_devices() -> Result<()> {
    anyhow::bail!("native Creator Micro control is currently supported on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_json_is_reassembled() {
        let mut reassembler = JsonReassembler::default();
        assert!(reassembler.push(b"\r\n{\"m\":\"v.oai").is_empty());
        assert_eq!(
            reassembler.push(b".hid\",\"p\":{\"k\":\"AG00\",\"act\":1}}\r\n"),
            vec![json!({"m":"v.oai.hid","p":{"k":"AG00","act":1}})]
        );
    }

    #[test]
    fn multiple_framed_messages_are_reassembled() {
        let mut reassembler = JsonReassembler::default();
        assert_eq!(
            reassembler.push(
                b"{\"id\":1,\"result\":{\"ok\":1}}\r\n{\"m\":\"v.oai.hid\",\"p\":{\"k\":\"ACT07\",\"act\":1}}\r\n"
            ),
            vec![
                json!({"id":1,"result":{"ok":1}}),
                json!({"m":"v.oai.hid","p":{"k":"ACT07","act":1}}),
            ]
        );
    }

    #[test]
    fn agent_and_action_keys_map_to_construct_semantics() {
        let event = |key: &str| json!({"m":"v.oai.hid","p":{"k":key,"act":1}});
        assert_eq!(
            event_from_message(&event("AG05")),
            Some(CreatorMicroEvent::Session(5))
        );
        assert_eq!(
            event_from_message(&event("AG07")),
            Some(CreatorMicroEvent::Pane(2))
        );
        assert_eq!(
            event_from_message(&event("AG10")),
            Some(CreatorMicroEvent::Approve)
        );
        assert_eq!(
            event_from_message(&event("AG11")),
            Some(CreatorMicroEvent::Reject)
        );
        assert_eq!(
            event_from_message(&event("AG12")),
            Some(CreatorMicroEvent::Enter)
        );
        assert_eq!(
            event_from_message(&event("ACT07")),
            Some(CreatorMicroEvent::Pane(2))
        );
        assert_eq!(
            event_from_message(&event("ACT10")),
            Some(CreatorMicroEvent::Approve)
        );
        assert_eq!(
            event_from_message(&event("ACT11")),
            Some(CreatorMicroEvent::Reject)
        );
        assert_eq!(
            event_from_message(&event("ACT12")),
            Some(CreatorMicroEvent::Enter)
        );
        assert_eq!(
            event_from_message(&event("ENC_CW")),
            Some(CreatorMicroEvent::Action(MidiAction::ScrollDown))
        );
    }

    #[test]
    fn feedback_lights_all_thirteen_keys_and_aggregate_underglow() {
        let snapshot = CreatorMicroSnapshot {
            assigned: 0b0000_0001,
            active: 0,
            attention: 0,
            pane_count: 2,
            focused_pane: Some(1),
            fleet_active: true,
            fleet_attention: false,
        };
        let thread_status = thread_status_message(snapshot, 7);
        let keys = thread_status["params"].as_array().unwrap();
        assert_eq!(thread_status["method"], "v.oai.thstatus");
        assert_eq!(thread_status["id"], 7);
        assert_eq!(keys.len(), 13);
        assert_eq!(keys[0]["c"], 0x2d7ff9);
        assert_eq!(keys[6]["c"], 0x2d7ff9);
        assert_eq!(keys[7]["c"], 0x5ce1ff);
        assert_eq!(keys[8]["b"], 0.0);
        assert_eq!(keys[10]["c"], 0x00c853);
        assert_eq!(keys[11]["c"], 0xff4d5f);
        assert_eq!(keys[12]["c"], 0xffffff);

        let preview = lighting_preview_message(snapshot, 8);
        assert_eq!(preview["method"], "lights.preview");
        assert_eq!(preview["id"], 8);
        assert_eq!(preview["params"]["backlight"]["effect"], "off");
        assert_eq!(preview["params"]["underglow"]["effect"], "breath");
        assert_eq!(preview["params"]["underglow"]["color"], 0xffc400);

        let attention = lighting_preview_message(
            CreatorMicroSnapshot {
                fleet_attention: true,
                ..snapshot
            },
            9,
        );
        assert_eq!(attention["params"]["underglow"]["color"], 0x00c853);
    }

    #[test]
    fn releases_are_suppressed() {
        let release = json!({"m":"v.oai.hid","p":{"k":"AG00","act":0}});
        assert_eq!(event_from_message(&release), None);
    }

    #[test]
    fn config_defaults_disabled_and_round_trips() {
        assert!(!toml::from_str::<CreatorMicroConfig>("").unwrap().enabled);
        let enabled = CreatorMicroConfig { enabled: true };
        let encoded = toml::to_string(&enabled).unwrap();
        assert_eq!(
            toml::from_str::<CreatorMicroConfig>(&encoded).unwrap(),
            enabled
        );
    }
}
