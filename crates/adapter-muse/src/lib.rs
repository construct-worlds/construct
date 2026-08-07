//! Meta Muse Code CLI adapter.
//!
//! Muse provides both a native TUI and `muse exec --json`. Its durable log
//! records the owning process id, so interactive sessions bind to their native
//! UUID by matching the actual PTY child rather than guessing from global file
//! modification order. The UUID is persisted in `muse_session_id.txt` and
//! resumed with `muse resume <uuid>`. Headless sessions mint the UUID before
//! their first `muse exec --session-id <uuid>` turn.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use construct_adapter_common::{
    drive_turn, emit_launch_failure_if_silent, spawn_stderr_tail, TurnOutcome,
};
use construct_protocol::adapter::pty::{run_session_with_pid as run_pty_with_pid, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

const SESSION_ID_FILE: &str = "muse_session_id.txt";

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "muse".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
        match resolve_mode(&params) {
            Mode::Interactive => run_interactive(params, ctx).await,
            Mode::Headless => run_headless(params, ctx).await,
        }
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Headless,
}

fn resolve_mode(params: &SessionStartParams) -> Mode {
    if let Ok(mode) = std::env::var("CONSTRUCT_MUSE_MODE") {
        match mode.as_str() {
            "interactive" => return Mode::Interactive,
            "headless" => return Mode::Headless,
            _ => {}
        }
    }
    match params.mode.as_deref() {
        Some("interactive") => Mode::Interactive,
        Some("headless") => Mode::Headless,
        _ if params.pty_size.is_some() => Mode::Interactive,
        _ => Mode::Headless,
    }
}

fn default_command() -> construct_protocol::adapter::CommandOverride {
    let default_bin = construct_protocol::adapter::default_cli_bin_with_home_fallback(
        "muse",
        Path::new(".local/bin/muse"),
    );
    construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_MUSE_CMD",
        "CONSTRUCT_MUSE_BIN",
        &default_bin,
    )
}

fn session_data_dir() -> Option<PathBuf> {
    std::env::var_os("CONSTRUCT_SESSION_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn muse_sessions_dir(params: &SessionStartParams) -> Option<PathBuf> {
    params
        .env
        .get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .map(|dir| dir.join("muse/sessions"))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/share/muse/sessions"))
        })
}

fn session_id_path() -> Option<PathBuf> {
    Some(session_data_dir()?.join(SESSION_ID_FILE))
}

fn valid_session_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .ok()
        .is_some_and(|id| id.hyphenated().to_string() == value)
}

fn read_session_id() -> Option<String> {
    std::fs::read_to_string(session_id_path()?)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| valid_session_id(value))
}

fn write_session_id(id: &str) {
    if let Some(path) = session_id_path() {
        let _ = std::fs::write(path, id);
    }
}

fn mint_session_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

fn child_env(params: &SessionStartParams, construct_session_id: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), construct_session_id.into()));
    env
}

fn append_root_args(args: &mut Vec<String>, params: &SessionStartParams) {
    args.extend(["--workspace".into(), params.cwd.clone()]);
    if let Some(model) = params.model.as_ref() {
        args.extend(["--model".into(), model.clone()]);
    }
    args.extend(params.args.clone());
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = default_command();
    let store = muse_sessions_dir(&params);
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let native_id = resuming.then(read_session_id).flatten();

    let mut args = command.args.clone();
    if let Some(id) = native_id.as_ref() {
        args.extend(["resume".into(), id.clone()]);
        append_root_args(&mut args, &params);
    } else {
        append_root_args(&mut args, &params);
        if let Some(prompt) = params
            .prompt
            .as_ref()
            .filter(|prompt| !resuming && !prompt.trim().is_empty())
        {
            args.push(prompt.clone());
        }
        if resuming {
            ctx.emit
                .log("muse respawn: no captured native session id; starting a fresh conversation");
        }
    }

    let (pid_tx, pid_rx) = oneshot::channel();
    if let Some(store) = store {
        spawn_session_watcher(store, native_id, resuming, pid_rx, ctx.emit.clone());
    } else {
        ctx.emit.log(
            "muse: no XDG_DATA_HOME/HOME; native resume and structured transcript unavailable",
        );
    }

    let label = command.argv_preview();
    let spec = PtySpec {
        bin: command.bin,
        args,
        cwd: PathBuf::from(&params.cwd),
        env: child_env(&params, &ctx.session_id),
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty_with_pid(spec, ctx, pid_tx).await;
}

async fn run_headless(params: SessionStartParams, mut ctx: AdapterContext) {
    let command = default_command();
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let native_id = read_session_id().unwrap_or_else(|| {
        let id = mint_session_id();
        write_session_id(&id);
        id
    });
    let mut pending = VecDeque::new();
    if !resuming {
        if let Some(prompt) = params
            .prompt
            .as_ref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            pending.push_back(prompt.clone());
        }
    }
    let meta = Arc::new(StdMutex::new(MetaState {
        last_model: params.model.clone(),
        last_effort: None,
    }));

    let exit_code = loop {
        let prompt = match pending.pop_front() {
            Some(prompt) => prompt,
            None => {
                ctx.emit.emit(SessionEvent::AwaitingInput { prompt: None });
                match ctx.inbox.recv().await {
                    Some(AdapterInboxMsg::Input(text)) => text,
                    Some(AdapterInboxMsg::Stop) | None => break 0,
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::PtyInput(_))
                    | Some(AdapterInboxMsg::PtyResize { .. })
                    | Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => continue,
                }
            }
        };
        if prompt.trim().is_empty() {
            continue;
        }

        let mut args = command.args.clone();
        args.extend([
            "exec".into(),
            "--json".into(),
            "--session-id".into(),
            native_id.clone(),
        ]);
        append_root_args(&mut args, &params);
        args.push(prompt);

        let mut child = Command::new(&command.bin);
        child
            .args(&args)
            .current_dir(&params.cwd)
            .envs(child_env(&params, &ctx.session_id))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(error) => {
                ctx.emit.emit(SessionEvent::Error {
                    message: construct_protocol::adapter::missing_bin_hint(
                        &command.argv_preview(),
                        &error,
                    ),
                });
                break 127;
            }
        };

        let stdout_task = spawn_headless_stdout(
            child.stdout.take().expect("piped"),
            ctx.emit.clone(),
            meta.clone(),
        );
        let (stderr_task, stderr_tail) =
            spawn_stderr_tail(child.stderr.take().expect("piped"), ctx.emit.clone());
        ctx.emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: Some(format!("{} exec", command.argv_preview())),
        });
        let events_before_turn = ctx.emit.events_emitted();

        let outcome = drive_turn(&mut child, &mut ctx.inbox, &ctx.emit, &mut pending).await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let status = child.wait().await.ok();
        match outcome {
            TurnOutcome::Stopped => break 0,
            TurnOutcome::Interrupted => {
                ctx.emit.log("muse turn interrupted; awaiting next input");
            }
            TurnOutcome::Completed => {
                let reported = emit_launch_failure_if_silent(
                    &ctx.emit,
                    events_before_turn,
                    status.as_ref(),
                    &stderr_tail.snapshot(),
                );
                if !reported {
                    if let Some(status) = status.filter(|status| !status.success()) {
                        ctx.emit.emit(SessionEvent::Error {
                            message: format!("muse exec exited with {status}"),
                        });
                    }
                }
            }
        }
    };
    ctx.emit.emit(SessionEvent::Done { exit_code });
}

#[derive(Debug, Default)]
struct MetaState {
    last_model: Option<String>,
    last_effort: Option<String>,
}

fn usage_events(event: &Value, model: Option<String>) -> Vec<SessionEvent> {
    let Some(usage) = event.get("usage") else {
        return Vec::new();
    };
    let tokens_in = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens_cached = usage
        .get("cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(tokens_in);
    let tokens_out = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            usage
                .get("reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    if tokens_in == 0 && tokens_out == 0 && tokens_cached == 0 {
        return Vec::new();
    }
    vec![
        SessionEvent::Cost {
            usd: 0.0,
            tokens_in,
            tokens_out,
            tokens_cached,
            model,
        },
        SessionEvent::ContextUsage {
            used_tokens: tokens_in,
            window_tokens: None,
        },
    ]
}

fn metadata_events(event: &Value, meta: &mut MetaState) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    if let Some(model) = event
        .get("model")
        .or_else(|| event.get("model_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if meta.last_model.as_deref() != Some(model) {
            meta.last_model = Some(model.into());
            events.push(SessionEvent::ModelChanged {
                model: model.into(),
            });
        }
    }
    if let Some(effort) = event
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        if meta.last_effort.as_deref() != Some(effort) {
            meta.last_effort = Some(effort.into());
            events.push(SessionEvent::EffortChanged {
                effort: effort.into(),
            });
        }
    }
    events
}

fn durable_record_events(value: &Value, meta: &mut MetaState) -> Vec<SessionEvent> {
    if value.get("payload_type").and_then(Value::as_str) != Some("runtime.session") {
        return Vec::new();
    }
    let Some(event) = value.pointer("/payload/event") else {
        return Vec::new();
    };
    let mut events = metadata_events(event, meta);
    match event.get("kind").and_then(Value::as_str) {
        Some("assistant_message_committed") => {
            if let Some(text) = event
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
            {
                events.push(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: text.into(),
                });
            }
        }
        Some("model_completed") => {
            events.extend(usage_events(event, meta.last_model.clone()));
        }
        _ => {}
    }
    events
}

fn headless_record_events(value: &Value, meta: &mut MetaState) -> Vec<SessionEvent> {
    if value.get("payload_type").and_then(Value::as_str) == Some("runtime.session") {
        let Some(event) = value.pointer("/payload/event") else {
            return Vec::new();
        };
        let mut events = metadata_events(event, meta);
        if event.get("kind").and_then(Value::as_str) == Some("model_completed") {
            events.extend(usage_events(event, meta.last_model.clone()));
        }
        return events;
    }

    let payload_type = value.get("payload_type").and_then(Value::as_str);
    let payload = value.get("payload").unwrap_or(&Value::Null);
    match payload_type {
        Some("run.terminal.completed") => payload
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                vec![SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: text.into(),
                }]
            })
            .unwrap_or_default(),
        Some("run.terminal.failed") => {
            let message = payload
                .get("reason")
                .or_else(|| payload.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("Muse turn failed")
                .to_string();
            vec![SessionEvent::Error { message }]
        }
        _ => Vec::new(),
    }
}

fn spawn_headless_stdout<R>(
    reader: R,
    emit: EventEmitter,
    meta: Arc<StdMutex<MetaState>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                emit.log(format!("muse stdout: {line}"));
                continue;
            };
            let events = {
                let mut meta = meta.lock().unwrap();
                headless_record_events(&value, &mut meta)
            };
            for event in events {
                emit.emit(event);
            }
        }
    })
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn collect_root_session_files(dir: &Path, depth: usize, files: &mut Vec<(String, PathBuf)>) {
    if depth == 4 {
        let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let path = dir.join("session.jsonl");
        if valid_session_id(id) && path.is_file() {
            files.push((id.into(), path));
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_root_session_files(&path, depth + 1, files);
        }
    }
}

fn root_session_files(store: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    collect_root_session_files(store, 0, &mut files);
    files
}

fn session_file_for_id(store: &Path, id: &str) -> Option<PathBuf> {
    root_session_files(store)
        .into_iter()
        .find_map(|(candidate, path)| (candidate == id).then_some(path))
}

fn session_file_for_pid(
    store: &Path,
    pid: u32,
    not_before: SystemTime,
) -> Option<(String, PathBuf)> {
    root_session_files(store).into_iter().find(|(_, path)| {
        let recent_enough = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map(|modified| modified >= not_before)
            .unwrap_or(false);
        recent_enough && log_names_pid(path, pid)
    })
}

fn log_names_pid(path: &Path, pid: u32) -> bool {
    use std::io::BufRead;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    std::io::BufReader::new(file)
        .lines()
        .take(256)
        .flatten()
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .any(|value| {
            value.get("payload_type").and_then(Value::as_str) == Some("runtime.session.route_facts")
                && value.pointer("/payload/record/pid").and_then(Value::as_u64)
                    == Some(u64::from(pid))
        })
}

fn spawn_session_watcher(
    store: PathBuf,
    initial_id: Option<String>,
    resuming: bool,
    child_pid: oneshot::Receiver<Option<u32>>,
    emit: EventEmitter,
) {
    tokio::spawn(async move {
        let mut current = initial_id.and_then(|id| {
            let path = session_file_for_id(&store, &id)?;
            Some((id, path))
        });
        let mut cursor = if resuming {
            current
                .as_ref()
                .map(|(_, path)| count_lines(path))
                .unwrap_or(0)
        } else {
            0
        };
        let child_pid = match child_pid.await {
            Ok(Some(pid)) => pid,
            _ => return,
        };
        let not_before = SystemTime::now()
            .checked_sub(Duration::from_secs(5))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut meta = MetaState::default();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Some((id, path)) = session_file_for_pid(&store, child_pid, not_before) {
                if current.as_ref().is_none_or(|(_, current)| current != &path) {
                    if let Some((prior, _)) = current.as_ref() {
                        emit.emit(SessionEvent::NativeIdChanged {
                            prior_native_id: prior.clone(),
                            new_native_id: id.clone(),
                        });
                    }
                    write_session_id(&id);
                    cursor = 0;
                    current = Some((id, path));
                }
            }

            let Some((_, path)) = current.as_ref() else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if index < cursor || line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                for event in durable_record_events(&value, &mut meta) {
                    emit.emit(event);
                }
            }
            cursor = text.lines().count();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Record literals are from Muse Code 0.1.0-R708.1 `--provider echo`
    // session logs captured on this machine on 2026-08-05, with ids shortened.

    #[test]
    fn mode_defaults_follow_pty_presence_and_allow_override() {
        let mut params: SessionStartParams = serde_json::from_value(serde_json::json!({
            "session_id":"s1", "cwd":"/tmp"
        }))
        .unwrap();
        assert_eq!(resolve_mode(&params), Mode::Headless);
        params.pty_size = Some(PtySize { cols: 80, rows: 24 });
        assert_eq!(resolve_mode(&params), Mode::Interactive);
        params.mode = Some("headless".into());
        assert_eq!(resolve_mode(&params), Mode::Headless);
    }

    #[test]
    fn discovery_excludes_nested_subagent_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let root_id = "11111111-2222-4333-8444-555555555555";
        let child_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let root = tmp.path().join("2026/08/05").join(root_id);
        let child = root.join("subagent").join(child_id);
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.join("session.jsonl"), "{}\n").unwrap();
        std::fs::write(child.join("session.jsonl"), "{}\n").unwrap();

        assert_eq!(root_session_files(tmp.path()).len(), 1);
        assert_eq!(
            session_file_for_id(tmp.path(), root_id),
            Some(root.join("session.jsonl"))
        );
        assert_eq!(session_file_for_id(tmp.path(), child_id), None);
    }

    #[test]
    fn process_id_selects_the_owning_root_session() {
        let tmp = tempfile::tempdir().unwrap();
        let own_id = "11111111-2222-4333-8444-555555555555";
        let other_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let own = tmp.path().join("2026/08/05").join(own_id);
        let other = tmp.path().join("2026/08/05").join(other_id);
        std::fs::create_dir_all(&own).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let record = |pid| {
            format!(
                "{{\"payload_type\":\"runtime.session.route_facts\",\"payload\":{{\"record\":{{\"pid\":{pid}}}}}}}\n"
            )
        };
        std::fs::write(own.join("session.jsonl"), record(4242)).unwrap();
        std::fs::write(other.join("session.jsonl"), record(9000)).unwrap();

        assert_eq!(
            session_file_for_pid(tmp.path(), 4242, SystemTime::UNIX_EPOCH),
            Some((own_id.into(), own.join("session.jsonl")))
        );
    }

    #[test]
    fn durable_records_emit_assistant_text_and_usage() {
        let assistant = serde_json::json!({
            "payload_type":"runtime.session",
            "payload":{"event":{"kind":"assistant_message_committed","text":"echo: hello"}}
        });
        let usage = serde_json::json!({
            "payload_type":"runtime.session",
            "payload":{"event":{"kind":"model_completed","usage":{
                "input_tokens":1200,"output_tokens":80,"cached_tokens":400,
                "reasoning_tokens":20
            }}}
        });
        let mut meta = MetaState {
            last_model: Some("meta/example".into()),
            ..Default::default()
        };

        assert!(matches!(
            durable_record_events(&assistant, &mut meta).as_slice(),
            [SessionEvent::Message { role: MessageRole::Assistant, text }] if text == "echo: hello"
        ));
        let events = durable_record_events(&usage, &mut meta);
        assert!(matches!(
            events.as_slice(),
            [
                SessionEvent::Cost { tokens_in: 1200, tokens_out: 100, tokens_cached: 400, model: Some(model), .. },
                SessionEvent::ContextUsage { used_tokens: 1200, window_tokens: None }
            ] if model == "meta/example"
        ));
    }

    #[test]
    fn headless_terminal_uses_final_text_once() {
        let delta = serde_json::json!({
            "payload_type":"run.output.delta",
            "payload":{"text":"echo: hel"}
        });
        let terminal = serde_json::json!({
            "payload_type":"run.terminal.completed",
            "payload":{"terminal":"completed","text":"echo: hello","reason":null}
        });
        let mut meta = MetaState::default();
        assert!(headless_record_events(&delta, &mut meta).is_empty());
        assert!(matches!(
            headless_record_events(&terminal, &mut meta).as_slice(),
            [SessionEvent::Message { text, .. }] if text == "echo: hello"
        ));
    }

    #[test]
    fn uuid_validation_requires_canonical_lowercase() {
        assert!(valid_session_id("11111111-2222-4333-8444-555555555555"));
        assert!(!valid_session_id("11111111-2222-4333-8444-55555555555A"));
        assert!(!valid_session_id("not-a-uuid"));
    }
}
