//! TUI app state and event loop.

use crate::client::Client;
use crate::keymap::{self, ChordState, KeyAction, Keymap, KeymapResult, Profile};
use crate::ui;
use agentd_protocol::{
    EventNotificationPayload, HarnessInfo, SessionSummary, StateNotificationPayload,
    TimestampedEvent,
};
use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Transcript,
}

#[derive(Debug, Clone)]
pub enum MinibufferIntent {
    SendInput { session_id: String },
    NewSessionHarness,
    NewSessionPrompt { harness: String, cwd: String },
    KillConfirm { session_id: String },
    CommandPalette,
}

#[derive(Debug, Clone)]
pub struct Minibuffer {
    pub prompt: String,
    pub input: String,
    pub cursor: usize,
    pub intent: MinibufferIntent,
}

pub struct App {
    pub client: Arc<Client>,
    pub sessions: Vec<SessionSummary>,
    pub selected: usize,
    pub focus: Focus,
    pub transcript: Vec<TimestampedEvent>,
    pub transcript_session: Option<String>,
    pub transcript_scroll: u16,
    pub minibuffer: Option<Minibuffer>,
    pub harnesses: Vec<HarnessInfo>,
    pub help_visible: bool,
    pub profile: Profile,
    pub keymap: Keymap,
    pub chord_state: ChordState,
    pub chord_label: String,
    pub status: Option<(String, Instant)>,
    pub last_diff: Option<String>,
    pub should_quit: bool,
    pub connected: bool,
}

pub async fn run(client: Arc<Client>) -> Result<()> {
    let profile = Profile::from_env();
    let keymap = keymap::default_for(profile);

    // Initial fetches.
    let sessions = client.list().await.unwrap_or_default();
    let harnesses = client.harnesses().await.unwrap_or_default();

    let mut app = App {
        client: client.clone(),
        sessions,
        selected: 0,
        focus: Focus::List,
        transcript: Vec::new(),
        transcript_session: None,
        transcript_scroll: 0,
        minibuffer: None,
        harnesses,
        help_visible: false,
        profile,
        keymap,
        chord_state: ChordState::default(),
        chord_label: String::new(),
        status: None,
        last_diff: None,
        should_quit: false,
        connected: true,
    };

    // Subscribe to all session events.
    if let Err(e) = client.subscribe(None).await {
        app.status = Some((format!("subscribe failed: {e}"), Instant::now()));
    }
    // Load transcript for the first session if any.
    app.refresh_selected_transcript().await;

    // Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(&mut terminal, &mut app).await;

    // Teardown — best effort.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    terminal.show_cursor().ok();
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut input_stream = EventStream::new();
    let mut notifications = app
        .client
        .take_notifications()
        .await
        .context("notifications channel already taken")?;
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    while !app.should_quit {
        terminal.draw(|f| ui::render(f, app))?;
        tokio::select! {
            ev = input_stream.next() => {
                match ev {
                    Some(Ok(ev)) => app.on_term_event(ev).await,
                    Some(Err(e)) => {
                        app.set_status(format!("input error: {e}"));
                    }
                    None => break,
                }
            }
            notif = notifications.recv() => {
                match notif {
                    Some(n) => app.on_notification(n).await,
                    None => {
                        app.connected = false;
                        app.set_status("daemon disconnected".to_string());
                    }
                }
            }
            _ = tick.tick() => {
                // Clear expired status, redraw.
                if let Some((_, at)) = &app.status {
                    if at.elapsed() > Duration::from_secs(5) {
                        app.status = None;
                    }
                }
            }
        }
    }
    Ok(())
}

impl App {
    pub fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.sessions.get(self.selected)
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
    }

    async fn refresh_selected_transcript(&mut self) {
        let Some(id) = self.selected_id() else {
            self.transcript.clear();
            self.transcript_session = None;
            return;
        };
        if self.transcript_session.as_deref() == Some(&id) {
            return;
        }
        match self.client.transcript(&id, 0, None).await {
            Ok(t) => {
                self.transcript = t.events;
                self.transcript_session = Some(id);
                self.transcript_scroll = u16::MAX; // sentinel = bottom
            }
            Err(e) => {
                self.set_status(format!("load transcript: {e}"));
            }
        }
    }

    async fn refresh_sessions(&mut self) {
        match self.client.list().await {
            Ok(list) => {
                let prev_id = self.selected_id();
                self.sessions = list;
                if let Some(pid) = prev_id {
                    if let Some(i) = self.sessions.iter().position(|s| s.id == pid) {
                        self.selected = i;
                    } else if self.selected >= self.sessions.len() {
                        self.selected = self.sessions.len().saturating_sub(1);
                    }
                }
            }
            Err(e) => self.set_status(format!("list failed: {e}")),
        }
    }

    async fn on_notification(&mut self, n: agentd_protocol::Notification) {
        match n.method.as_str() {
            m if m == agentd_protocol::ipc_notif::EVENT => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<EventNotificationPayload>(p) {
                        if Some(payload.session_id.as_str())
                            == self.transcript_session.as_deref()
                        {
                            self.transcript.push(TimestampedEvent {
                                seq: payload.seq,
                                at: payload.at,
                                event: payload.event.clone(),
                            });
                            self.transcript_scroll = u16::MAX;
                        }
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<StateNotificationPayload>(p) {
                        let id = payload.session.id.clone();
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.sessions[i] = payload.session;
                        } else {
                            self.sessions.push(payload.session);
                            self.sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn on_term_event(&mut self, ev: CtEvent) {
        match ev {
            CtEvent::Key(k) => self.on_key(k).await,
            CtEvent::Resize(_, _) => {}
            _ => {}
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        // Minibuffer captures all input when open.
        if self.minibuffer.is_some() {
            self.handle_minibuffer_key(key).await;
            return;
        }
        if self.help_visible {
            // Any key closes help.
            self.help_visible = false;
            return;
        }
        match self.keymap_clone().bindings.iter().count() {
            _ => {}
        }
        let res = self.chord_state.handle(key, &self.keymap);
        self.chord_label = self.chord_state.label();
        match res {
            KeymapResult::Action(a) => self.run_action(a).await,
            KeymapResult::Pending(label) => self.chord_label = label,
            KeymapResult::Unhandled => {}
        }
    }

    // tiny helper to avoid borrow issues
    fn keymap_clone(&self) -> &Keymap {
        &self.keymap
    }

    async fn run_action(&mut self, action: KeyAction) {
        use KeyAction::*;
        match action {
            Quit => self.should_quit = true,
            NextSession => {
                if !self.sessions.is_empty() {
                    self.selected = (self.selected + 1) % self.sessions.len();
                    self.refresh_selected_transcript().await;
                }
            }
            PrevSession => {
                if !self.sessions.is_empty() {
                    if self.selected == 0 {
                        self.selected = self.sessions.len() - 1;
                    } else {
                        self.selected -= 1;
                    }
                    self.refresh_selected_transcript().await;
                }
            }
            Refresh => {
                self.refresh_sessions().await;
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
            }
            OpenSendInput => {
                if let Some(id) = self.selected_id() {
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!("Send to {}: ", short_id(&id)),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::SendInput { session_id: id },
                    });
                } else {
                    self.set_status("no session selected".to_string());
                }
            }
            OpenNewSession => {
                if self.harnesses.is_empty() {
                    self.harnesses = self.client.harnesses().await.unwrap_or_default();
                }
                let hint = self
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join("|");
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("Harness [{hint}]: "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::NewSessionHarness,
                });
            }
            OpenKillConfirm => {
                if let Some(id) = self.selected_id() {
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!("Kill {}? (y/N): ", short_id(&id)),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::KillConfirm { session_id: id },
                    });
                }
            }
            OpenDiff => {
                if let Some(id) = self.selected_id() {
                    match self.client.diff(&id).await {
                        Ok(r) => {
                            if r.patch.is_empty() {
                                self.set_status("(no diff)".to_string());
                                self.last_diff = None;
                            } else {
                                self.last_diff = Some(r.patch);
                            }
                        }
                        Err(e) => self.set_status(format!("diff failed: {e}")),
                    }
                }
            }
            Interrupt => {
                if let Some(id) = self.selected_id() {
                    match self.client.interrupt(&id).await {
                        Ok(()) => self.set_status("interrupt sent".to_string()),
                        Err(e) => self.set_status(format!("interrupt failed: {e}")),
                    }
                }
            }
            OpenCommandPalette => {
                self.minibuffer = Some(Minibuffer {
                    prompt: "M-x ".to_string(),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::CommandPalette,
                });
            }
            TogglePane => {
                self.focus = match self.focus {
                    Focus::List => Focus::Transcript,
                    Focus::Transcript => Focus::List,
                };
            }
            ScrollUp => {
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                }
            }
            ScrollDown => {
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(1);
                }
            }
            ScrollPageUp => {
                if self.transcript_scroll == u16::MAX {
                    self.transcript_scroll = 0;
                } else {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(10);
                }
            }
            ScrollPageDown => {
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(10);
                }
            }
            ScrollTop => {
                self.transcript_scroll = 0;
            }
            ScrollBottom => {
                self.transcript_scroll = u16::MAX;
            }
            ToggleHelp => {
                self.help_visible = !self.help_visible;
            }
        }
    }

    async fn handle_minibuffer_key(&mut self, key: KeyEvent) {
        let Some(mb) = self.minibuffer.as_mut() else { return; };
        match key.code {
            KeyCode::Esc => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Enter => {
                let intent = mb.intent.clone();
                let input = std::mem::take(&mut mb.input);
                self.minibuffer = None;
                self.run_minibuffer_submit(intent, input).await;
                return;
            }
            KeyCode::Backspace => {
                if mb.cursor > 0 {
                    let prev = mb.cursor - 1;
                    mb.input.remove(prev);
                    mb.cursor = prev;
                }
            }
            KeyCode::Left => {
                mb.cursor = mb.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Home => mb.cursor = 0,
            KeyCode::End => mb.cursor = mb.input.chars().count(),
            KeyCode::Char(c) => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.insert(pos, c);
                mb.cursor += 1;
            }
            _ => {}
        }
    }

    async fn run_minibuffer_submit(&mut self, intent: MinibufferIntent, input: String) {
        match intent {
            MinibufferIntent::SendInput { session_id } => {
                if input.is_empty() {
                    return;
                }
                match self.client.send_input(&session_id, input).await {
                    Ok(()) => self.set_status("input sent".to_string()),
                    Err(e) => self.set_status(format!("send failed: {e}")),
                }
            }
            MinibufferIntent::NewSessionHarness => {
                let harness = input.trim().to_string();
                if harness.is_empty() {
                    return;
                }
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("Prompt for {} in {}: ", harness, cwd),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::NewSessionPrompt { harness, cwd },
                });
            }
            MinibufferIntent::NewSessionPrompt { harness, cwd } => {
                let prompt = input.trim().to_string();
                let params = agentd_protocol::CreateSessionParams {
                    harness: harness.clone(),
                    cwd,
                    prompt: if prompt.is_empty() { None } else { Some(prompt) },
                    model: None,
                    title: None,
                    worktree: false,
                    env: HashMap::new(),
                    args: Vec::new(),
                };
                match self.client.create(params).await {
                    Ok(id) => {
                        self.set_status(format!("created {}", short_id(&id)));
                        self.refresh_sessions().await;
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.selected = i;
                            self.refresh_selected_transcript().await;
                        }
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
                }
            }
            MinibufferIntent::KillConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("kill cancelled".to_string());
                    return;
                }
                match self.client.kill(&session_id).await {
                    Ok(()) => self.set_status(format!("killed {}", short_id(&session_id))),
                    Err(e) => self.set_status(format!("kill failed: {e}")),
                }
            }
            MinibufferIntent::CommandPalette => {
                let cmd = input.trim();
                self.run_palette_command(cmd).await;
            }
        }
    }

    async fn run_palette_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
            "" => {}
            "quit" | "exit" => self.should_quit = true,
            "refresh" => {
                self.refresh_sessions().await;
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
            }
            "new" | "new-session" => self.run_action(KeyAction::OpenNewSession).await,
            "send" | "send-input" => self.run_action(KeyAction::OpenSendInput).await,
            "kill" => self.run_action(KeyAction::OpenKillConfirm).await,
            "diff" => self.run_action(KeyAction::OpenDiff).await,
            "interrupt" => self.run_action(KeyAction::Interrupt).await,
            "help" | "?" => self.help_visible = true,
            "harnesses" => {
                self.harnesses = self.client.harnesses().await.unwrap_or_default();
                let names: Vec<String> = self
                    .harnesses
                    .iter()
                    .map(|h| {
                        let mark = if h.available { "ok" } else { "missing" };
                        format!("{} ({})", h.name, mark)
                    })
                    .collect();
                self.set_status(format!("harnesses: {}", names.join(", ")));
            }
            other => self.set_status(format!("unknown command: {other}")),
        }
    }
}

pub fn short_id(id: &str) -> &str {
    let n = id.len().min(10);
    &id[..n]
}

fn byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}
