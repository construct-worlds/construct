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

#[derive(Debug, Clone, Subcommand)]
pub enum CreatorMicroCommand {
    /// Show whether native Creator Micro control is enabled and reachable.
    Status,
    /// List connected compatible Work Louder devices.
    Devices,
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
        CreatorMicroCommand::Enable => {
            CreatorMicroConfig { enabled: true }.save(&path)?;
            println!("Creator Micro control enabled in {}", path.display());
            println!("Open a new Construct TUI to connect.");
            println!();
            println!("The active Work Louder layer must use these Input keycodes:");
            println!("  keys 1-6: KV_OAI_AG00 through KV_OAI_AG05");
            println!("  action row: KV_OAI_ACT06 through KV_OAI_ACT12");
            println!("  encoder: KV_OAI_ENC_CC / KV_OAI_ENC_CW / KV_OAI_ENC_CLK");
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorMicroEvent {
    Session(usize),
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
fn run_connection(
    feedback_rx: &std_mpsc::Receiver<CreatorMicroSnapshot>,
    event_tx: &mpsc::UnboundedSender<CreatorMicroEvent>,
    link_tx: &mpsc::UnboundedSender<CreatorMicroLinkStatus>,
    stop: &AtomicBool,
    snapshot: &mut CreatorMicroSnapshot,
) -> Result<()> {
    use hidapi::HidApi;

    let api = HidApi::new().context("initialize HID")?;
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
        .context("open Creator Micro 2 non-exclusively")?;
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
    let _ = link_tx.send(CreatorMicroLinkStatus::Connected { product, transport });

    let mut request_id = 1u16;
    send_feedback(&device, *snapshot, &mut request_id)?;
    let mut last_feedback = Instant::now();
    let mut reassembler = JsonReassembler::default();
    let mut wide_pressed_at: Option<Instant> = None;
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
            if let Some(event) = event_from_message(&message, &mut wide_pressed_at) {
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
    let params = (0..6)
        .map(|slot| {
            let bit = 1 << slot;
            if snapshot.attention & bit != 0 {
                json!({"id": slot, "c": 0x00c853, "b": 1.0, "e": 6, "s": 0.75})
            } else if snapshot.active & bit != 0 {
                json!({"id": slot, "c": 0xffc400, "b": 0.9, "e": 4, "s": 0.6})
            } else if snapshot.assigned & bit != 0 {
                json!({"id": slot, "c": 0x2d7ff9, "b": 0.22, "e": 1, "s": 0.0})
            } else {
                json!({"id": slot, "c": 0, "b": 0.0, "e": 0, "s": 0.0})
            }
        })
        .collect::<Vec<_>>();
    let message = serde_json::to_vec(&json!({
        "method": "v.oai.thstatus",
        "params": params,
        "id": *request_id,
    }))?;
    *request_id = (*request_id + 1) % 999;

    let framed = [b"\r\n".as_slice(), message.as_slice(), b"\r\n".as_slice()].concat();
    for chunk in framed.chunks(MAX_PAYLOAD) {
        let mut packet = [0u8; REPORT_SIZE];
        packet[0] = REPORT_ID;
        packet[1] = CHANNEL_RPC;
        packet[2] = chunk.len() as u8;
        packet[3..3 + chunk.len()].copy_from_slice(chunk);
        device
            .write(&packet)
            .context("write Creator Micro feedback")?;
    }
    Ok(())
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

fn event_from_message(
    message: &Value,
    wide_pressed_at: &mut Option<Instant>,
) -> Option<CreatorMicroEvent> {
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
        .filter(|slot| *slot < 6)
    {
        return Some(CreatorMicroEvent::Session(slot));
    }
    Some(match key {
        "ACT06" => CreatorMicroEvent::Enter,
        "ACT07" => CreatorMicroEvent::Approve,
        "ACT08" => CreatorMicroEvent::Reject,
        "ACT09" => CreatorMicroEvent::Action(MidiAction::Interrupt),
        // One wide physical cap can press ACT10 and ACT11 together. Treat the
        // pair as one New Session gesture instead of opening two dialogs.
        "ACT10" | "ACT11" => {
            let now = Instant::now();
            if wide_pressed_at
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(80))
            {
                return None;
            }
            *wide_pressed_at = Some(now);
            CreatorMicroEvent::Action(MidiAction::NewSession)
        }
        "ACT12" => CreatorMicroEvent::Action(MidiAction::CommandPalette),
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
        let mut wide = None;
        let event = |key: &str| json!({"m":"v.oai.hid","p":{"k":key,"act":1}});
        assert_eq!(
            event_from_message(&event("AG05"), &mut wide),
            Some(CreatorMicroEvent::Session(5))
        );
        assert_eq!(
            event_from_message(&event("ACT07"), &mut wide),
            Some(CreatorMicroEvent::Approve)
        );
        assert_eq!(
            event_from_message(&event("ENC_CW"), &mut wide),
            Some(CreatorMicroEvent::Action(MidiAction::ScrollDown))
        );
    }

    #[test]
    fn releases_and_second_wide_switch_are_suppressed() {
        let mut wide = None;
        let release = json!({"m":"v.oai.hid","p":{"k":"AG00","act":0}});
        assert_eq!(event_from_message(&release, &mut wide), None);
        let first = json!({"m":"v.oai.hid","p":{"k":"ACT10","act":1}});
        let second = json!({"m":"v.oai.hid","p":{"k":"ACT11","act":1}});
        assert_eq!(
            event_from_message(&first, &mut wide),
            Some(CreatorMicroEvent::Action(MidiAction::NewSession))
        );
        assert_eq!(event_from_message(&second, &mut wide), None);
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
