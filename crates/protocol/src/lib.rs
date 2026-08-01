//! agentd wire protocol.
//!
//! Two protocols share JSON-RPC 2.0 over line-delimited JSON:
//!
//! - **AHP** (Agent Harness Protocol): daemon ⇄ adapter, over the adapter's stdio.
//! - **IPC**: client ⇄ daemon, over a Unix socket.
//!
//! Both reuse the envelope types in [`jsonrpc`] and the same [`transport`] helpers.

pub mod adapter;
pub mod agent_context;
pub mod dialect;
pub mod jsonrpc;
pub mod osc11;
pub mod paths;
pub mod published_model;
pub mod slash;
pub mod transport;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use jsonrpc::{ErrorObject, Notification, Request, Response};

/// Current AHP protocol version. Bump on breaking wire changes.
pub const AHP_VERSION: &str = "0.1.0";

/// Current IPC protocol version.
pub const IPC_VERSION: &str = "0.1.0";

// ============================================================================
// AHP: methods the adapter implements (daemon → adapter)
// ============================================================================

pub mod ahp_method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_START: &str = "session.start";
    pub const SESSION_INPUT: &str = "session.input";
    pub const SESSION_PTY_INPUT: &str = "session.pty_input";
    pub const SESSION_PTY_RESIZE: &str = "session.pty_resize";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_TOOL_DECISION: &str = "session.tool_decision";
    pub const SESSION_TOOL_ACTION: &str = "session.tool_action";
    pub const SESSION_SET_APPROVAL_MODE: &str = "session.set_approval_mode";
    pub const SHUTDOWN: &str = "shutdown";
}

pub mod ahp_notif {
    /// Adapter → daemon: a [`super::SessionEvent`] occurred.
    pub const EVENT: &str = "session/event";
    /// Adapter → daemon: free-form log line for the daemon's log.
    pub const LOG: &str = "log";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    #[serde(default)]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub supports_input: bool,
    #[serde(default)]
    pub supports_interrupt: bool,
    #[serde(default)]
    pub supports_diff: bool,
    #[serde(default)]
    pub supports_cost: bool,
    /// Adapter owns a PTY for this session: emits [`SessionEvent::Pty`]
    /// events and accepts `session.pty_input` / `session.pty_resize`.
    #[serde(default)]
    pub supports_pty: bool,
    /// Adapter emits no startup escapes (banner, screen-clear, cursor
    /// reset) on resume — so the daemon can safely keep the prior
    /// incarnation's PTY ring + on-disk `pty.log` instead of clearing
    /// them. Without this guarantee, mixing old bytes with a fresh
    /// adapter's start-up sequence leaves vt100 in a half-rendered
    /// state, so the daemon defaults to clearing on respawn.
    #[serde(default)]
    pub supports_silent_resume: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartParams {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Adapter-defined session mode (e.g. `"interactive"` / `"headless"`).
    /// Each adapter chooses its own default if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Initial PTY size — set by the client when starting in PTY mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_size: Option<PtySize>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPtyInputParams {
    pub session_id: String,
    /// Base64-encoded raw bytes to write to the child's PTY.
    pub data: String,
    /// Whether this input is an explicit user-engagement claim for the
    /// session's single PTY geometry. Pointer motion may be forwarded to a
    /// mouse-tracking child with this set to false.
    #[serde(default = "default_pty_activity_claim")]
    pub claim: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPtyResizeParams {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    /// Whether this resize is an explicit user-engagement claim for the
    /// session's single PTY geometry. Passive viewport reports set this to
    /// false so they update the connection's remembered size without stealing
    /// ownership from another client.
    #[serde(default = "default_pty_activity_claim")]
    pub claim: bool,
}

fn default_pty_activity_claim() -> bool {
    // Compatibility with clients predating explicit ownership: their resize
    // and input calls historically claimed the PTY, so an omitted field keeps
    // that behavior. New passive reporters must send `claim: false`.
    true
}

#[cfg(test)]
#[test]
fn pty_activity_claim_is_legacy_true_but_can_be_explicitly_passive() {
    let legacy: SessionPtyResizeParams = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "cols": 80,
        "rows": 24
    }))
    .expect("legacy resize params");
    assert!(legacy.claim);

    let passive: SessionPtyResizeParams = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "cols": 80,
        "rows": 24,
        "claim": false
    }))
    .expect("passive resize params");
    assert!(!passive.claim);

    let legacy_input: SessionPtyInputParams = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "data": "aGVsbG8="
    }))
    .expect("legacy input params");
    assert!(legacy_input.claim);

    let passive_input: SessionPtyInputParams = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "data": "aGVsbG8=",
        "claim": false
    }))
    .expect("passive input params");
    assert!(!passive_input.claim);
}

impl SessionPtyInputParams {
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(&self.data)
    }

    pub fn from_bytes(session_id: impl Into<String>, bytes: &[u8]) -> Self {
        Self::from_bytes_with_claim(session_id, bytes, true)
    }

    pub fn from_bytes_with_claim(session_id: impl Into<String>, bytes: &[u8], claim: bool) -> Self {
        use base64::Engine;
        Self {
            session_id: session_id.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            claim,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetFocusedParams {
    pub session_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInputParams {
    pub session_id: String,
    pub text: String,
}

/// Per-connection report of the background color the client paints over the
/// terminal, or `None` when the client's theme leaves the terminal background
/// visible (spec 0073). The daemon uses the most recent report among live
/// connections to answer child OSC 11 background probes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetTerminalBackgroundParams {
    pub background: Option<[u8; 3]>,
}

/// Which surface a client is currently showing a session through. Reported by
/// clients via `session.set_view` so the daemon knows whether a given session
/// is being watched in the structured chat view (where Claude's native PTY
/// widgets aren't usable) or the raw terminal view (where they are). Drives the
/// `AskUserQuestion` chat-gate: when a chat viewer is active, the injected
/// `PreToolUse` hook degrades the picker to a plain-text question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientView {
    /// Structured chat rendering (no usable native PTY widget).
    Chat,
    /// Raw terminal rendering (native PTY widgets work).
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetViewParams {
    pub session_id: String,
    pub view: ClientView,
}

/// Result of `session.chat_viewer_active`: whether any connected client is
/// currently watching the session in [`ClientView::Chat`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatViewerActiveResult {
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachClipboardParams {
    pub session_id: String,
    /// Base64-encoded clipboard/file bytes.
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachClipboardResult {
    /// Absolute path written on the daemon host.
    pub path: String,
    /// Short text clients can insert into the prompt.
    pub reference: String,
}

/// Read one file back from a session's attachments directory (spec 0099:
/// web-client attachment previews). Only bare filenames are accepted — the
/// method can never read outside the attachments dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReadAttachmentParams {
    pub session_id: String,
    pub filename: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReadAttachmentResult {
    /// Base64-encoded file bytes.
    pub data: String,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEmitEventParams {
    pub session_id: String,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWidgetDeleteParams {
    pub session_id: String,
    pub panel_id: String,
}

/// Payload carried by the [`ahp_notif::EVENT`] notification from adapter → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub session_id: String,
    pub event: SessionEvent,
}

/// Live, adapter-owned turn status shown near the input editor while
/// an agent is working. Inactive statuses are ephemeral completion
/// notices; clients may render them locally without persisting them
/// into transcript or PTY data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub active: bool,
    #[serde(default)]
    pub started_at_ms: i64,
    pub status: String,
}

/// Browser preview image for UI-only overlays. This is emitted by
/// browser-aware tools so clients can show what the agent is viewing
/// without stuffing screenshots into the textual tool result that the
/// model consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserPreview {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// PNG bytes encoded as base64.
    pub image: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// Declarative, adapter-owned session UI panel. This is intentionally a
/// small semantic vocabulary for the first dynamic-UI pass: renderers own
/// layout/resize/focus while harnesses own the task state being displayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPanel {
    pub id: String,
    /// Source filename for file-backed widgets. Used as title fallback and
    /// omitted for ephemeral event-backed panels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Filesystem creation time for file-backed widgets, in Unix milliseconds.
    /// Renderers use this to keep widget positions stable across restart while
    /// appending newly-created widgets after existing ones. Ephemeral panels may
    /// leave it unset.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_at_ms: u64,
    #[serde(default)]
    pub placement: UiPlacement,
    /// Safe agentd-markdown: normal markdown plus semantic links such as
    /// `[Run checks](agentd:action/run-checks)`. Renderers parse the subset
    /// they understand and degrade the rest as text.
    #[serde(default)]
    pub markdown: String,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiPlacement {
    Inline,
    #[default]
    Sticky,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiAction {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Close the containing widget after dispatch. Parsed from action links
    /// such as `[OK](agentd:action/ok?close=1)`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
}

/// A structured event emitted by an adapter while running a session.
///
/// Adapters whose underlying CLI is plain text can lean on
/// [`SessionEvent::Message`] for everything; richer harnesses should emit the
/// more specific variants so the UI can render them distinctively.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Authoritative set of harness-native children currently retained by the
    /// wrapped harness. The daemon archives mirrors owned by this session when
    /// their native id is absent, and later upserts may unarchive them.
    NativeSubagentSnapshot {
        ids: Vec<String>,
    },
    /// The harness removed a child from its native active-agent view. The
    /// daemon archives the corresponding mirror without deleting its history.
    NativeSubagentRemoved {
        id: String,
    },
    /// A child agent created and owned by the wrapped harness (for example a
    /// Claude Code or Codex native subagent). The daemon projects these as
    /// read-only virtual sessions; it must not spawn, resume, or directly
    /// control a second adapter for them.
    NativeSubagent {
        /// Stable harness-native child identifier.
        id: String,
        /// Harness-native parent identifier. `None` means the Construct
        /// session receiving this event is the direct parent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        state: SessionState,
        /// Optional semantic transcript event belonging to the native child.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<Box<SessionEvent>>,
        /// Deterministic per-child ordinal for emissions derived from the
        /// child's own transcript file, starting at 0 in file order. An
        /// adapter re-scanning the file from the top regenerates the same
        /// ordinals, so the daemon can drop already-projected replays by
        /// comparing against [`NativeSubagentRef::projected_seq`] — which is
        /// what lets adapters ALWAYS backfill a child's full history instead
        /// of skipping pre-existing lines (and leaving empty mirrors) on
        /// resume/restart. `None` for emissions not derived from the child's
        /// file (discovery, root-transcript state updates): those are
        /// processed unconditionally.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    Message {
        role: MessageRole,
        text: String,
    },
    /// Free-form "thinking" / "reasoning" text streamed from the
    /// model — separate from `Message` so the TUI can render it
    /// distinctively (dim italic) and so harnesses that don't surface
    /// reasoning don't see it in their normal message stream.
    /// Adapters that don't have access to reasoning content can
    /// simply never emit this.
    Reasoning {
        text: String,
    },
    /// A tool invocation by the model.
    ///
    /// `call_id` is the canonical key correlating a [`SessionEvent::ToolResult`]
    /// (or a `TaskStart`) back to this `ToolUse`. Historically the `tool` field
    /// on `ToolResult` carried the call id by "smith convention"; that is now
    /// superseded by `call_id`, and `tool` should hold the actual tool name.
    /// When `call_id` is `None` (legacy transcripts), consumers fall back to the
    /// `tool` field for correlation.
    ToolUse {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// The result of a tool invocation.
    ///
    /// `call_id` is the canonical key correlating this result back to its
    /// [`SessionEvent::ToolUse`] (or `TaskStart`). Historically the `tool` field
    /// carried the call id by "smith convention"; that is now superseded by
    /// `call_id`, and `tool` should hold the actual tool name. When `call_id` is
    /// `None` (legacy transcripts), consumers fall back to the `tool` field for
    /// correlation.
    ToolResult {
        tool: String,
        ok: bool,
        #[serde(default)]
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    AwaitingInput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    Status {
        state: SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Live or just-completed agent turn status. The TUI renders this
    /// above queued input while active and may render inactive statuses
    /// as display-only history rows.
    AgentStatus(AgentStatus),
    /// UI-only browser preview. Clients may render this as an overlay;
    /// it is not intended to be fed back to the model as tool output.
    BrowserPreview(BrowserPreview),
    /// Declarative session UI panel created or replaced by an adapter.
    UiPanel(UiPanel),
    /// Delete a previously-created session UI panel by id.
    UiDelete {
        id: String,
    },
    Cost {
        #[serde(default)]
        usd: f64,
        #[serde(default)]
        tokens_in: u64,
        #[serde(default)]
        tokens_out: u64,
        /// Cached input tokens (subset of `tokens_in` served from the provider's
        /// prompt cache). 0 when unknown or unsupported by the provider.
        #[serde(default)]
        tokens_cached: u64,
        /// Model that consumed these tokens, spelled exactly as this adapter
        /// spells it in [`ModelChanged`](Self::ModelChanged) — read from the
        /// same report when the harness states one there, else the adapter's
        /// currently-tracked model from the harness's own model records (spec
        /// 0167). `None` when the harness never stated a model; clients then
        /// attribute the sample to the session's current model rather than
        /// dropping it. The two spellings must not diverge — a label that
        /// disagrees with `ModelChanged` splits one model into two series
        /// wherever usage is grouped by model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Live context-window fill (spec 0104): the prompt-side token count of
    /// the session's most recent model call, and the model's context-window
    /// size when the harness states it. A gauge, not a counter — each report
    /// replaces the previous one, and a context reset clears it.
    ContextUsage {
        used_tokens: u64,
        /// `None` when the harness doesn't report its window; clients then
        /// show the bare usage rather than a percentage against a guess.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_tokens: Option<u64>,
    },
    /// Labeled per-component breakdown of what occupies the context window
    /// (spec 0156): system prompt, guides, tools, conversation, … Same gauge
    /// semantics as [`ContextUsage`](Self::ContextUsage) — latest report
    /// replaces the previous one, a context reset clears it — and adapters
    /// emit it at the same cadence (on change, alongside the gauge). Segment
    /// counts derived from char-length heuristics set
    /// [`ContextSegment::estimated`]; clients mark those visually. Adapters
    /// only report components their harness's own data surface makes
    /// derivable — clients synthesize "free space" and "unaccounted" rows
    /// themselves.
    ContextBreakdown {
        segments: Vec<ContextSegment>,
    },
    Diff {
        patch: String,
    },
    Error {
        message: String,
    },
    /// Adapter requests that the daemon clear this session's persisted
    /// transcript and PTY replay history. Used by interactive adapters for
    /// slash commands such as `/reset`.
    Reset,
    Done {
        #[serde(default)]
        exit_code: i32,
    },
    /// Raw byte chunk from the session's PTY. `data` is base64-encoded so the
    /// JSON transport doesn't have to deal with arbitrary byte sequences.
    Pty {
        data: String,
    },
    /// The session's single PTY was resized to `cols` x `rows` (by whichever
    /// client last claimed geometry — TUI or web). UI-only and transient
    /// (never persisted/replayed): lets a passive viewer whose viewport is
    /// narrower render at the real width instead of wrapping the output.
    PtyResize {
        cols: u16,
        rows: u16,
        /// True only for the client connection whose explicit engagement
        /// currently owns this resize. The daemon personalizes this transient
        /// event per subscriber; older servers omit it and deserialize as
        /// false.
        #[serde(default)]
        owner: bool,
    },
    /// Adapter is asking the user to approve (or deny) a pending tool call.
    /// The adapter parks the agent loop until a [`SessionToolDecisionParams`]
    /// arrives on the inbox referencing the same `call_id`. The `risk` field
    /// lets the UI render a badge; `args_summary` is pre-formatted by the
    /// adapter so the UI doesn't have to interpret each tool's schema.
    ToolApprovalRequest {
        call_id: String,
        tool: String,
        #[serde(default)]
        args_summary: String,
        risk: ToolRisk,
        /// Whether the UI should offer an auto-review retry action. Adapters
        /// set this to false when auto-review already vetted this call and
        /// deferred to the user; showing the same action again is redundant.
        #[serde(default = "default_true")]
        allow_auto_review: bool,
    },
    /// The pending tool approval identified by `call_id` is no longer
    /// waiting — it was approved, denied, auto-reviewed, or the turn was
    /// interrupted. UI-only and transient (never persisted/replayed): lets
    /// passive viewers that mirrored the [`ToolApprovalRequest`] (the web
    /// approval dialog, the TUI minibuffer prompt) dismiss it when another
    /// client answered the prompt.
    ToolApprovalResolved {
        call_id: String,
    },
    /// Adapter changed the session's approval mode internally, typically
    /// because the user answered an inline PTY approval prompt with an
    /// action that changes future approval behavior.
    ApprovalModeChanged {
        mode: ApprovalMode,
    },
    /// Adapter toggled the operator ambient loop (`/operator enable|disable`).
    /// Durable per-session state like [`ApprovalModeChanged`]: never written
    /// to the transcript; the daemon persists it so the choice survives restart.
    OperatorLoopChanged {
        enabled: bool,
    },
    /// Adapter switched the session's active model internally (e.g. the
    /// smith `/model` slash command). `model` is a canonical spec string the
    /// adapter can re-resolve on resume (`provider:model`, or `@profile:model`
    /// for a named-endpoint profile). The daemon records it as the session's
    /// model so the choice survives restart and the UI label tracks the
    /// switch. Durable per-session state, like
    /// [`ApprovalModeChanged`](Self::ApprovalModeChanged): never written to
    /// the transcript.
    ModelChanged {
        model: String,
    },
    /// Adapter detected that the wrapped harness minted a fresh native
    /// conversation id mid-session (Claude `/clear`/`/branch`/in-session
    /// `/resume`, Codex `/clear`/`/new`, and equivalent flows in Antigravity
    /// and Grok — see spec 0079). The daemon synthesizes a real, archived
    /// child session holding a copy of the transcript up to this point,
    /// forked from this session, so the pre-reset conversation stays
    /// addressable through the ordinary fork/archive machinery (spec 0085).
    /// Durable per-session state, like [`ModelChanged`](Self::ModelChanged):
    /// never written to the transcript.
    NativeIdChanged {
        prior_native_id: String,
        new_native_id: String,
    },
    /// Adapter detected the session's active reasoning-effort tier (e.g.
    /// `"high"` / `"medium"` / `"low"`). Best-effort, like `ModelChanged` for
    /// the harnesses that don't self-report (grok, codex): scraped from
    /// on-disk transcript state the adapter already tails, not guaranteed to
    /// track every live change (see each adapter's own doc comments for its
    /// specific caveats). Durable per-session state, like
    /// [`ModelChanged`](Self::ModelChanged): never written to the transcript.
    EffortChanged {
        effort: String,
    },
    /// Tool lifecycle: adapter started running a tool. Carries the
    /// canonical `call_id` (unlike [`ToolUse`] which doesn't) so the
    /// daemon's per-session task registry can match this against the
    /// later [`TaskBackgrounded`] / [`TaskEnd`] events. Emitted in
    /// addition to `ToolUse`, not in place of it — the structured
    /// transcript and the model's tool-call stream still rely on
    /// `ToolUse` / `ToolResult`.
    TaskStart {
        call_id: String,
        tool: String,
        #[serde(default)]
        args_summary: String,
    },
    /// The tool's foreground budget elapsed (auto-bg at
    /// `CONSTRUCT_TOOL_BG_AFTER_MS`) or the user clicked `[bg]` /
    /// invoked `session.tool_action { action: "background" }`. The
    /// adapter detached the join handle into its background pool;
    /// the agent's conversation got a placeholder result and moved
    /// on. A [`TaskEnd`] event will follow once the detached task
    /// actually completes.
    TaskBackgrounded {
        call_id: String,
    },
    /// The tool finished (whichever path it took). `ok` and
    /// `output_preview` mirror the corresponding `ToolResult` event
    /// — `output_preview` is truncated for `/tasks` display; the
    /// full output is in the transcript via `ToolResult`.
    TaskEnd {
        call_id: String,
        ok: bool,
        #[serde(default)]
        output_preview: String,
    },
    /// Adapter compacted older conversation turns into an
    /// LLM-generated summary so the rolling context stays under the
    /// model's input-token budget without silently dropping history.
    /// Emitted by interactive adapters (currently smith) when either
    /// the user runs `/compact` or auto-compact fires near the budget
    /// ceiling. Carries enough info for the TUI to render a banner
    /// card; full summary text is also in the transcript as the new
    /// synthetic user turn the adapter inserted at the head.
    ContextCompacted {
        /// Number of turn pairs preserved verbatim after the summary.
        #[serde(default)]
        kept_turns: u32,
        /// Number of turn pairs that were folded into the summary.
        #[serde(default)]
        dropped_turns: u32,
        /// Approximate token count of the conversation *before* the
        /// compaction ran (chars/3.5 heuristic, same as the rolling
        /// budget check).
        #[serde(default)]
        tokens_before: u64,
        /// Approximate token count *after* the compaction. Always
        /// less than `tokens_before` on a successful compact.
        #[serde(default)]
        tokens_after: u64,
        /// First ~160 chars of the summary, for the TUI banner. Full
        /// text lives in the inserted user turn.
        #[serde(default)]
        summary_preview: String,
    },
    /// State of an adapter's input editor, rendered by the TUI in a
    /// fixed pane below the chat scrollback. Emitted by the adapter
    /// (currently smith interactive) whenever the editor buffer,
    /// cursor, or pending-input queue changes. Lets the client paint
    /// a true bottom-anchored prompt that doesn't compete with the
    /// agent's stream for PTY rows.
    EditorState {
        /// Pending user submissions waiting for the agent — each
        /// shown as a "queued" row above the active prompt. The
        /// adapter coalesces consecutive submissions but exposes the
        /// per-submission strings so the TUI can render one line per
        /// pending entry if it likes.
        #[serde(default)]
        queued: Vec<String>,
        /// Current text in the editor buffer.
        #[serde(default)]
        buf: String,
        /// Character index of the cursor within `buf` (0 = before
        /// first char).
        #[serde(default)]
        cursor: usize,
        /// Completion suggestions for the current editor buffer.
        /// Rendered above the fixed prompt by clients so completions
        /// stay out of PTY scrollback.
        #[serde(default)]
        completions: Vec<String>,
    },
    /// A client-routed slash command, dispatched by an adapter for the
    /// attached client to execute (`/zoom`, `/new`, `/remote-control`, …).
    ///
    /// Replaces the old `tui` `ToolUse` convention: instead of encoding the
    /// command as a fake tool call (which forced every consumer to
    /// string-sniff `"tui"` and accidentally leaked UI noise into
    /// `agentd_get_transcript`), it carries the typed [`slash::CommandId`].
    /// The daemon and the transcript-read path look the command's
    /// [`slash::TranscriptPolicy`] / [`slash::ModelVisibility`] up in
    /// [`slash::COMMANDS`] and honor it — persistence and model-visibility
    /// become a property of the command, not a special case.
    ClientCommand {
        id: slash::CommandId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// Generated next-prompt suggestions for the turn that just ended
    /// (spec 0109). Produced daemon-side from the session's normalized
    /// transcript tail — never by adapters — and delivered like
    /// [`AgentStatus`](Self::AgentStatus): broadcast to live clients
    /// only, never persisted to the transcript. Clients render the hand
    /// however fits their surface (TUI corner orb, web fan); accepting a
    /// card sends its text through the ordinary session-input path. A
    /// hand is only valid while the session still awaits input — any
    /// later event for the session invalidates it client-side.
    Suggestions(SuggestionHand),
}

/// A dealt "hand" of generated next-prompt suggestions: one top pick
/// (the single most likely next prompt, cheap to accept) plus a few
/// generated verbs — under-specified directions, each holding its own
/// concrete prompt cards. All text is model-generated per turn; nothing
/// in the hand is a fixed vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionHand {
    /// The single best next prompt — the one-keypress case.
    pub top: SuggestionCard,
    /// Generated directions, each expanding to concrete cards.
    #[serde(default)]
    pub verbs: Vec<SuggestionVerb>,
}

/// A generated direction ("wrap up", "dig deeper", …) grouping cards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionVerb {
    /// Short label shown on the verb chip. Generated per turn.
    pub label: String,
    #[serde(default)]
    pub cards: Vec<SuggestionCard>,
}

/// One concrete suggested prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionCard {
    /// Full prompt text sent verbatim when the card is accepted.
    pub text: String,
}

/// Ack for [`ipc_method::SESSION_SUGGEST`]: whether generation was
/// actually kicked off (false when disabled, the session isn't awaiting
/// input, or another generation is already in flight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResult {
    pub started: bool,
}

/// Params for [`ipc_method::SESSION_SUGGEST`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSuggestParams {
    pub session_id: String,
    /// Optional user guidance for a deliberate regeneration. Keywords
    /// steer the next hand but do not become transcript evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
}

/// One remembered prompt in the global prompt history (spec 0155).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHistoryEntry {
    /// The prompt text, verbatim as sent.
    pub text: String,
    /// When the prompt was last sent (dedupe moves a repeat to the front
    /// and refreshes this), unix millis.
    pub at_ms: i64,
    /// Session the prompt was last sent to, if still known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Harness of that session, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

/// Params for [`ipc_method::PROMPT_HISTORY_LIST`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptHistoryListParams {
    /// Maximum entries to return, newest first. Omitted → the full
    /// retained history (itself FIFO-capped by the daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Result of [`ipc_method::PROMPT_HISTORY_LIST`]: newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHistoryListResult {
    pub entries: Vec<PromptHistoryEntry>,
}

impl SuggestionHand {
    /// Loose-in, strict-out parse of a model's reply into a hand:
    /// accepts the JSON with or without code fences or prose around it,
    /// then clamps every field to UI-safe sizes. Shared by the smith
    /// `--suggest-mode` one-shot and the daemon's same-harness probe
    /// path so every generator obeys the same contract. Errors only
    /// when no usable top pick survives.
    pub fn parse_loose(raw: &str) -> Result<Self, String> {
        const MAX_CARD_CHARS: usize = 240;
        const MAX_LABEL_CHARS: usize = 28;
        const MAX_VERBS: usize = 4;
        const MAX_CARDS_PER_VERB: usize = 4;

        #[derive(Deserialize)]
        struct RawVerb {
            #[serde(default)]
            label: String,
            #[serde(default)]
            cards: Vec<String>,
        }
        #[derive(Deserialize)]
        struct RawHand {
            #[serde(default)]
            top: String,
            #[serde(default)]
            verbs: Vec<RawVerb>,
        }

        let start = raw.find('{').ok_or("no JSON object in output")?;
        let end = raw.rfind('}').ok_or("no JSON object in output")?;
        if end < start {
            return Err("malformed JSON output".to_string());
        }
        let parsed: RawHand = serde_json::from_str(&raw[start..=end])
            .map_err(|e| format!("suggestion JSON parse: {e}"))?;

        let clamp_card = |text: &str| -> Option<SuggestionCard> {
            let t = text.trim();
            if t.is_empty() {
                return None;
            }
            Some(SuggestionCard {
                text: t
                    .chars()
                    .take(MAX_CARD_CHARS)
                    .collect::<String>()
                    .trim()
                    .to_string(),
            })
        };

        let top = clamp_card(&parsed.top).ok_or("empty top suggestion")?;
        let verbs = parsed
            .verbs
            .into_iter()
            .filter_map(|v| {
                let label: String = v
                    .label
                    .trim()
                    .chars()
                    .take(MAX_LABEL_CHARS)
                    .collect::<String>()
                    .trim()
                    .to_string();
                if label.is_empty() {
                    return None;
                }
                let cards: Vec<SuggestionCard> = v
                    .cards
                    .iter()
                    .filter_map(|c| clamp_card(c))
                    .take(MAX_CARDS_PER_VERB)
                    .collect();
                if cards.is_empty() {
                    return None;
                }
                Some(SuggestionVerb { label, cards })
            })
            .take(MAX_VERBS)
            .collect();
        Ok(SuggestionHand { top, verbs })
    }

    /// The generation instructions shared by every suggestion generator
    /// (spec 0109). Generators append the transcript context after this
    /// and parse the reply with [`Self::parse_loose`].
    pub const PROMPT_INSTRUCTIONS: &'static str = r#"You predict the user's next prompt in a coding-agent session. Below is the tail of the transcript: the user's prompts, the agent's replies, and tool activity. The agent just finished a turn and is waiting for input.

Reply with ONLY a JSON object, no markdown fences, no commentary:
{"top": "...", "verbs": [{"label": "...", "cards": ["...", "..."]}]}

Rules:
- "top": the single most likely next prompt, written in the user's own voice and style. Terse, imperative, immediately sendable.
- "verbs": exactly 3 or 4 distinct directions the user might take instead. Each label is 1-3 lowercase words generated from THIS conversation (e.g. "ship it", "dig deeper", "change course" — but derive from context, never a fixed menu). No two verbs may mean the same thing, and none may restate "top".
- Each verb has 2-4 "cards": complete, self-contained prompts under 140 characters, concrete enough to send unedited. Reference actual files, tests, errors, and names from the transcript.
- Mirror the user's phrasing habits and any workflow conventions visible in the transcript (tests before PR, worktrees, etc.).
- A "Recent prompts from this user across sessions" section may follow the transcript. Use it only to mirror the user's voice, phrasing, and recurring workflows — suggestions must stay grounded in THIS session's transcript, and never copy an old prompt that doesn't fit the current state.
- A "User guidance for this regeneration" section may also follow. Bias the hand toward its keywords when they fit the transcript, but treat them as intent guidance rather than evidence and never invent state to satisfy them.
- Never invent state: only reference things the transcript shows. Do not run tools; just reply with the JSON."#;
}

/// Lifecycle state surfaced by `session.list_tasks`. Derived by the
/// daemon from `SessionEvent::TaskStart` / `TaskBackgrounded` /
/// `TaskEnd` flowing from the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Backgrounded,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub call_id: String,
    pub tool: String,
    #[serde(default)]
    pub args_summary: String,
    pub state: TaskState,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backgrounded_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    /// Truncated end-state preview (only meaningful for terminal
    /// states; `None` while still running / backgrounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    /// `true` for a successful terminal state — distinguishes
    /// `Completed { ok: true }` from `Completed { ok: false }`.
    #[serde(default)]
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksResult {
    pub tasks: Vec<TaskInfo>,
}

/// Conventional tool name that adapters use to forward client-targeted
/// slash commands. The adapter emits a [`SessionEvent::ToolUse`] with
/// this name and an `args` object shaped like
/// `{"command": "<verb>", "args": "<rest>"}` (the `args` field is
/// omitted when the command was bare); it immediately follows up with
/// a synthetic [`SessionEvent::ToolResult`] so the transcript stays
/// balanced. The client TUI / CLI subscribes to `ToolUse` events,
/// recognizes this tool name, and dispatches its own slash table.
/// Defining it as a protocol constant (instead of a separate event
/// variant) keeps the wire format small and lets the LLM-initiated
/// path use the same tool when we later register it in smith's
/// catalog for natural-language UI actions.
pub const TUI_DISPATCH_TOOL: &str = "tui";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Manual,
    AutoReview,
    UnsafeAuto,
}

impl ApprovalMode {
    pub fn badge(self) -> Option<&'static str> {
        match self {
            ApprovalMode::Manual => None,
            ApprovalMode::AutoReview => Some("auto-review"),
            ApprovalMode::UnsafeAuto => Some("always-approve"),
        }
    }
}

/// Coarse risk classification used by adapters that gate tool calls behind
/// user approval. Two tiers are intentional; finer-grained policies can be
/// layered without changing the protocol (the `decision` field on
/// [`SessionToolDecisionParams`] is an open string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// Read-only / observation. Adapter runs these without prompting.
    Safe,
    /// Mutates filesystem / sessions / external state. Adapter gates these
    /// according to the session's approval mode.
    Risky,
}

impl SessionEvent {
    /// Build a [`SessionEvent::Pty`] from raw bytes.
    pub fn pty(bytes: &[u8]) -> Self {
        use base64::Engine;
        SessionEvent::Pty {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// Decode a [`SessionEvent::Pty`] payload back to raw bytes.
    pub fn pty_bytes(&self) -> Option<Vec<u8>> {
        match self {
            SessionEvent::Pty { data } => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(data).ok()
            }
            _ => None,
        }
    }
}

/// Helper to determine if a PTY byte slice contains actual visible activity,
/// ignoring ignorable styling (SGR) and synchronized update escape sequences.
pub fn is_pty_active_payload(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI
                let start = i;
                i += 2;
                let mut is_ignorable = false;
                while i < bytes.len() {
                    let c = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        let seq = &bytes[start..i];
                        if c == b'm' {
                            is_ignorable = true;
                        } else if (c == b'h' || c == b'l') && seq.starts_with(b"\x1b[?2026") {
                            is_ignorable = true;
                        }
                        break;
                    }
                }
                if is_ignorable {
                    continue;
                } else {
                    return true;
                }
            } else {
                return true;
            }
        } else if b == 0 {
            i += 1;
            continue;
        } else {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Pending,
    Running,
    AwaitingInput,
    Paused,
    Done,
    Errored,
}

impl SessionState {
    pub fn glyph(self) -> &'static str {
        match self {
            SessionState::Pending => "○",
            SessionState::Running => "●",
            SessionState::AwaitingInput => "●",
            SessionState::Paused => "⏸",
            SessionState::Done => "✓",
            SessionState::Errored => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionState::Pending => "pending",
            SessionState::Running => "running",
            SessionState::AwaitingInput => "awaiting input",
            SessionState::Paused => "paused",
            SessionState::Done => "done",
            SessionState::Errored => "errored",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, SessionState::Done | SessionState::Errored)
    }
}

// ============================================================================
// IPC: methods the daemon exposes to clients
// ============================================================================

pub mod ipc_method {
    pub const PING: &str = "ping";
    pub const HARNESS_LIST: &str = "harness.list";
    /// User-invocable actions contributed by installed plugins (spec 0152
    /// phase 2). Clients populate their palette/slash surfaces from this at
    /// runtime — plugin actions are data, never compiled-in commands.
    pub const PLUGIN_LIST_ACTIONS: &str = "plugin.list_actions";
    /// Run one plugin action: the daemon spawns the action's command with
    /// plugin identity env and the optional session context, fire-and-forget.
    pub const PLUGIN_RUN_ACTION: &str = "plugin.run_action";
    /// Per-method smith auth detection, powering the `/configure` dialog's
    /// smith-auth tab (spec 0069): which auth methods smith supports, whether
    /// each is currently usable, and which (if any) `CONSTRUCT_SMITH_MODEL`
    /// currently pins.
    pub const SMITH_AUTH_STATUS: &str = "smith.auth_status";
    /// Pin (or clear, with `method: "auto"`) smith's default model by writing
    /// `[adapters.smith.env] CONSTRUCT_SMITH_MODEL` in the daemon's
    /// `config.toml`. Takes effect for sessions started after the write —
    /// see `SmithSetAuthMethodResult::note`.
    pub const SMITH_SET_AUTH_METHOD: &str = "smith.set_auth_method";
    /// Live status of the daemon's ambient features (auto-naming,
    /// suggestions, operator — spec 0151): whether each is working,
    /// degraded, or off, with a human-readable reason. Lets clients
    /// connect a silently-missing convenience back to its cause instead
    /// of leaving the user guessing.
    pub const FEATURES_STATUS: &str = "features.status";
    pub const PLAYBOOK_GET: &str = "playbook.get";
    pub const PLAYBOOK_UPDATE: &str = "playbook.update";
    pub const PLAYBOOK_EDIT: &str = "playbook.edit";
    pub const PLAYBOOK_CURSOR: &str = "playbook.cursor";
    pub const PLAYBOOK_EXECUTE: &str = "playbook.execute";
    pub const PLAYBOOK_LIST_TEMPLATES: &str = "playbook.list_templates";
    /// List available Playbook selection verbs (spec 0089): built-in plus any
    /// user-defined `verbs/*.md` overrides/additions.
    pub const PLAYBOOK_LIST_VERBS: &str = "playbook.list_verbs";
    /// Run a Playbook selection verb (spec 0089): spawns a result-returning
    /// subagent scoped to the selection; the daemon merges its structured
    /// result back into the document once the subagent completes.
    pub const PLAYBOOK_VERB_EXECUTE: &str = "playbook.verb_execute";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_CREATE: &str = "session.create";
    pub const SESSION_GET: &str = "session.get";
    pub const SESSION_INPUT: &str = "session.input";
    pub const SESSION_ATTACH_CLIPBOARD: &str = "session.attach_clipboard";
    pub const SESSION_READ_ATTACHMENT: &str = "session.read_attachment";
    pub const SESSION_PTY_INPUT: &str = "session.pty_input";
    pub const SESSION_PTY_RESIZE: &str = "session.pty_resize";
    pub const SESSION_PTY_REPLAY: &str = "session.pty_replay";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_KILL: &str = "session.kill";
    pub const SESSION_DELETE: &str = "session.delete";
    /// Terminate a session's adapter and mark it archived: it keeps its
    /// transcript/worktree but is hidden from the list by default and is
    /// not auto-resumed on daemon startup. Reversed by `SESSION_RESTART`.
    pub const SESSION_ARCHIVE: &str = "session.archive";
    pub const SESSION_MERGE: &str = "session.merge";
    pub const SESSION_WIDGET_DELETE: &str = "session.widget.delete";
    /// Respawn a session's adapter — typically used to bring a `Done`
    /// session back to life so the user can continue typing. The
    /// adapter is launched with `CONSTRUCT_RESUME=1` so harnesses that
    /// persist conversation state (e.g. smith) can pick up where
    /// they left off.
    pub const SESSION_RESTART: &str = "session.restart";
    pub const SESSION_SET_PINNED: &str = "session.set_pinned";
    /// Clear a session's `needs_attention` marker and record it as the focused
    /// session (so a concurrent non-`Running` transition won't re-raise it).
    pub const SESSION_MARK_SEEN: &str = "session.mark_seen";
    pub const SESSION_SET_FOCUSED: &str = "session.set_focused";
    /// Report the connection's painted terminal background (spec 0073).
    /// `background: [r, g, b]` when the client's theme paints the frame,
    /// `null` for background-aware themes that leave the terminal visible.
    pub const CLIENT_SET_TERMINAL_BACKGROUND: &str = "client.set_terminal_background";
    pub const SESSION_SET_TITLE: &str = "session.set_title";
    pub const SESSION_SET_APPROVAL_MODE: &str = "session.set_approval_mode";
    /// Arm, change, or clear a session's model route (spec 0114). Takes
    /// effect on the running session without restarting it.
    pub const SESSION_SET_ROUTE: &str = "session.set_route";
    /// Enumerate the routes configured on this daemon, plus whether the
    /// named session can currently be routed and why not (spec 0115).
    pub const ROUTER_LIST_ROUTES: &str = "router.list_routes";
    /// Start a sign-in for a subscription route: spawns a shell session
    /// running the owning CLI's login command, then watches for the
    /// credential to land and archives the session automatically
    /// (spec 0117).
    pub const ROUTER_LOGIN: &str = "router.login";
    pub const SESSION_TOOL_DECISION: &str = "session.tool_decision";
    pub const SESSION_TOOL_ACTION: &str = "session.tool_action";
    pub const SESSION_LIST_TASKS: &str = "session.list_tasks";
    /// Append/broadcast a structured event for a session. Intended for trusted
    /// local helpers such as construct-mcp that run outside an adapter but need to
    /// surface UI-only state (for example browser previews) in the caller's
    /// session.
    pub const SESSION_EMIT_EVENT: &str = "session.emit_event";
    pub const LOOP_CREATE: &str = "loop.create";
    pub const LOOP_LIST: &str = "loop.list";
    pub const LOOP_UPDATE: &str = "loop.update";
    pub const LOOP_REMOVE: &str = "loop.remove";
    pub const SESSION_MOVE: &str = "session.move";
    pub const SESSION_SET_GROUP: &str = "session.set_group";
    pub const SESSION_SET_PROJECT: &str = "session.set_project";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_CREATE: &str = "project.create";
    pub const PROJECT_RENAME: &str = "project.rename";
    pub const PROJECT_DELETE: &str = "project.delete";
    pub const PROJECT_SET_COLLAPSED: &str = "project.set_collapsed";
    pub const PROJECT_MOVE: &str = "project.move";
    pub const SESSION_DIFF: &str = "session.diff";
    pub const SESSION_TRANSCRIPT: &str = "session.transcript";
    /// Request next-prompt suggestion generation for a session (spec
    /// 0106). Fire-and-forget: the ack only says whether generation
    /// started; the hand itself arrives later as a broadcast
    /// [`SessionEvent::Suggestions`](crate::SessionEvent::Suggestions).
    pub const SESSION_SUGGEST: &str = "session.suggest";
    /// Read the global prompt history: the FIFO of prompts the user has
    /// sent to any user-kind session, newest first (spec 0155). See
    /// [`crate::PromptHistoryListParams`] / [`crate::PromptHistoryListResult`].
    pub const PROMPT_HISTORY_LIST: &str = "prompt_history.list";
    /// Substring search across session name/metadata, stored playbook
    /// contents, and transcript history — see [`crate::SearchParams`] /
    /// [`crate::SearchResult`].
    pub const SESSION_SEARCH: &str = "session.search";
    /// A client reports which surface (chat vs terminal) it is currently
    /// showing a session through. The daemon tracks this per connection so it
    /// can answer `session.chat_viewer_active`.
    pub const SESSION_SET_VIEW: &str = "session.set_view";
    /// Query whether any connected client is watching the session in the chat
    /// view. Used by the `construct ask-gate` hook to decide whether to degrade
    /// `AskUserQuestion` to a plain-text question.
    pub const SESSION_CHAT_VIEWER_ACTIVE: &str = "session.chat_viewer_active";
    /// Read the shared split layout: the window tree every client renders
    /// its panes from, plus the version to pass back to [`LAYOUT_SET`].
    pub const LAYOUT_GET: &str = "layout.get";
    /// Replace the shared split layout wholesale. `base_version` makes the
    /// write last-writer-wins against a known state: a stale base is
    /// rejected rather than silently clobbering a concurrent edit. There is
    /// no merge semantics for a split tree — an edit is always a whole-tree
    /// replace.
    ///
    /// Only clients wide enough to *render* the layout may call this. A
    /// client that has clamped the tree down to a single pane for its
    /// viewport must never write its clamped view back, or opening the web
    /// UI on a phone would collapse the layout on every other client.
    pub const LAYOUT_SET: &str = "layout.set";
    pub const SUBSCRIBE_EVENTS: &str = "subscribe.events";
    pub const UNSUBSCRIBE_EVENTS: &str = "unsubscribe.events";
    /// Bind the remote WS listener (idempotent) and return a URL + QR
    /// the caller can show the user. With `provider: None` — the
    /// default — this exposes nothing past the local network: no
    /// tunnel subprocess is spawned, and the reply describes the LAN
    /// and loopback addresses. Naming a provider additionally starts
    /// that tunnel and waits for its URL.
    pub const REMOTE_START: &str = "remote.start";
    /// Probe every tunnel provider the daemon knows about and report
    /// which ones could start right now. Read-only: spawns no tunnel
    /// and changes no state, so the dialog can call it freely to
    /// decide which buttons to offer.
    pub const REMOTE_PROVIDERS: &str = "remote.providers";
    /// Stop remote control, or just its tunnel (see `tunnel_only` in
    /// `RemoteStopParams`). Full stop kills the tunnel subprocess, drops
    /// the listener, and clears the daemon-side `RemoteState` so the
    /// next `/remote-control` mints a fresh password. Tunnel-only stop
    /// kills the tunnel but leaves the LAN listener and its password
    /// running. Idempotent — calling stop when nothing is running
    /// returns Ok with `was_running: false`.
    pub const REMOTE_STOP: &str = "remote.stop";
    /// Restart the daemon process in place — persist remote state
    /// (token, password, port, provider, tunnel URL) to disk, exec()
    /// the current executable so any on-disk binary upgrade is picked
    /// up. The tunnel subprocess is left running (detached process
    /// group) so its URL survives the restart; the new daemon binds
    /// the same local port and adopts the still-alive tunnel without
    /// re-spawning. Returns the new exe path and
    /// timestamp the new daemon will report on startup; the IPC
    /// connection is closed by the kernel during exec(), so clients
    /// detect the restart as a socket close and reconnect.
    pub const DAEMON_RESTART: &str = "daemon.restart";
    /// Ask the running daemon to shut down gracefully: stop every
    /// session's adapter (leaving session state resumable on the next
    /// start) and exit. The IPC connection is closed by the kernel as
    /// the process exits, so clients detect the shutdown as a socket
    /// close — same as `daemon.restart`, minus the re-exec.
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    /// Dev tooling: point the running daemon at a directory of web-UI
    /// assets (`index.html`, `static/*`) to serve from disk instead of
    /// the binary's embedded copies, and inject a live-reload poller so
    /// edits show up on browser refresh / automatically. `dir: None`
    /// reverts to the embedded assets. Lets you iterate on `index.html`
    /// in a worktree against an already-running daemon — no rebuild or
    /// restart.
    pub const DEV_SET_ASSETS: &str = "dev.set_assets";
    /// Query (and optionally trigger a background refresh of) the cached
    /// harness usage-probe snapshot for one harness (spec 0086). Read-mostly:
    /// never blocks on the probe itself — when a refresh is warranted it is
    /// spawned in the background and this call returns immediately with
    /// `refreshing: true`. Backs the TUI's hover tooltip over a harness name.
    pub const USAGE_QUERY: &str = "usage.query";
    pub const TOKEN_HISTORY: &str = "usage.token_history";
}

pub mod ipc_notif {
    pub const EVENT: &str = "session/event";
    pub const STATE: &str = "session/state";
    pub const PLAYBOOK_STATE: &str = "playbook/state";
    pub const PLAYBOOK_CURSOR: &str = "playbook/cursor";
    pub const DELETED: &str = "session/deleted";
    pub const PROJECT_STATE: &str = "project/state";
    pub const PROJECT_DELETED: &str = "project/deleted";
    /// The shared split layout changed — either because a client wrote it
    /// or because the daemon pruned a pane whose session went away.
    /// Carries the whole tree; there are no incremental layout deltas.
    pub const LAYOUT_STATE: &str = "layout/state";
    /// Aggregate state for the daemon's remote WebSocket transport:
    /// how many remote clients are currently attached. Broadcast on
    /// every connect / disconnect so the local TUI can render a
    /// "● remote attached" badge as a visible signal that another
    /// surface is also driving the daemon.
    pub const REMOTE_STATE: &str = "remote/state";
    /// Ambient-feature status changed (spec 0151). Carries a full
    /// `FeaturesStatusResult`; emitted the first time an ambient feature
    /// actually skips work for lack of a smith credential, so clients can
    /// surface the degradation without polling.
    pub const FEATURES_STATE: &str = "features/state";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybookUpdateActor {
    Human,
    Agent,
}

impl Default for PlaybookUpdateActor {
    fn default() -> Self {
        Self::Human
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookDocument {
    pub session_id: String,
    pub markdown: String,
    pub version: u64,
    pub updated_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
}

/// A playbook "block": the unit of playbook-run shimmer (see specs 0042 and
/// 0053). A run of non-blank Markdown lines is split at heading and list-item
/// boundaries, so each heading, each list item, and each plain paragraph is its
/// own block — letting an individual task card shimmer or settle independently
/// of its siblings even when written without blank lines between them. Wrapped
/// continuation lines stay with the item or paragraph they belong to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookBlockSpan {
    /// Source-line range `[start_line, end_line)` into `markdown.lines()`.
    pub start_line: usize,
    pub end_line: usize,
    /// Normalized content: each line trimmed, joined by `\n`. Equal-content
    /// blocks share a signature (and therefore an id) and settle together.
    pub signature: String,
    /// Legacy content-derived block id (spec 0053): a hash of `signature`.
    pub id: String,
    /// Raw block text: source lines `[start_line, end_line)` joined by `\n`
    /// (original indentation preserved), usable directly as an edit anchor.
    pub text: String,
}

/// One block of a playbook with its current shimmer state — the per-block
/// projection returned by playbook get/edit/update so an agent reads and
/// declares shimmer by stable block ref (spec 0053).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookBlockView {
    /// Stable daemon-owned block-instance reference for shimmer declarations.
    /// Prefer this (or `id`, which mirrors it for compatibility with existing
    /// agent instructions) over content-derived ids.
    pub id: String,
    /// Stable block-instance id, independent of content and position.
    #[serde(default)]
    pub block_id: String,
    /// Incremented when this block instance's semantic content changes.
    #[serde(default)]
    pub content_epoch: u64,
    /// `block_id:content_epoch`; the authoritative shimmer key.
    #[serde(default)]
    pub block_ref: String,
    /// Legacy content-derived id for compatibility and diagnostics.
    #[serde(default)]
    pub content_id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub shimmer: bool,
    /// Concise (≤10-word) run-status tooltip stored alongside this block's
    /// shimmer state (spec 0057). `Some` only for a pending block whose shimmer
    /// was declared with a tooltip; `None` for settled blocks and for blocks
    /// shimmering without a stored tooltip (optimistic/legacy/in-flight), where
    /// a renderer falls back to a hardcoded label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// A declaration that a playbook block (addressed by its stable ref/id) is pending
/// (`shimmer: true`) or settled (`shimmer: false`) — the unit of the per-block
/// shimmer declaration carried by playbook edits (spec 0053). When declaring a
/// block pending, a concise `tooltip` describing its run status is required of
/// agent callers (spec 0057) and stored alongside the shimmer state; it is
/// ignored when settling a block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookShimmerDecl {
    pub id: String,
    pub shimmer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tooltip: Option<String>,
}

/// Hardcoded fallback tooltip for a block that is shimmering without a stored
/// tooltip — optimistic client-side shimmer before an agent supplies one,
/// legacy run state, or a block kept pending across an edit (spec 0057).
pub const PLAYBOOK_SHIMMER_FALLBACK_TOOLTIP: &str = "Working…";

pub const PLAYBOOK_SHIMMER_STATUS_QUEUED: &str = "Queued behind current turn";
pub const PLAYBOOK_SHIMMER_STATUS_DELIVERED: &str = "Delivered, waiting for agent";
pub const PLAYBOOK_SHIMMER_STATUS_AGENT_WORKING: &str = "Agent working, no status yet";

/// Maximum word count for a playbook-shimmer tooltip (spec 0057). Longer
/// tooltips are gracefully truncated rather than rejected.
pub const PLAYBOOK_SHIMMER_TOOLTIP_MAX_WORDS: usize = 10;

/// Normalize a playbook-shimmer tooltip to its stored form (spec 0057): trim,
/// collapse internal whitespace to single spaces, and truncate to at most
/// [`PLAYBOOK_SHIMMER_TOOLTIP_MAX_WORDS`] words (appending `…` when truncated).
/// Returns `None` for an empty/whitespace-only string so an absent tooltip is
/// never stored as an empty label.
pub fn normalize_playbook_tooltip(raw: &str) -> Option<String> {
    let words: Vec<&str> = raw.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    if words.len() <= PLAYBOOK_SHIMMER_TOOLTIP_MAX_WORDS {
        Some(words.join(" "))
    } else {
        Some(format!(
            "{}…",
            words[..PLAYBOOK_SHIMMER_TOOLTIP_MAX_WORDS].join(" ")
        ))
    }
}

/// Legacy content-derived id for a playbook block (spec 0053). Derived from the
/// block's identity signature with a dependency-free FNV-1a hash so the daemon
/// and every client compute the same fallback id for the same content. Stable
/// block refs from `PlaybookBlockView::id` are authoritative when available.
pub fn playbook_block_id(signature: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in signature.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Signature used for block identity. It intentionally ignores smart-clip
/// instance ids (`clip_id=...`) because those are client-assigned references to
/// a specific rendered clip, not task content. Adding or repairing clip ids must
/// not settle a block whose work is still pending.
fn playbook_block_identity_signature(signature: &str) -> String {
    let mut out = String::with_capacity(signature.len());
    let mut rest = signature;
    loop {
        let Some(start) = rest.find("@{") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            out.push_str(&rest[start..]);
            break;
        };
        let body = &after_start[..end];
        out.push_str("@{");
        out.push_str(&playbook_smart_clip_body_without_instance_id(body));
        out.push('}');
        rest = &after_start[end + 1..];
    }
    out
}

fn playbook_smart_clip_body_without_instance_id(raw_clip: &str) -> String {
    raw_clip
        .split_whitespace()
        .filter(|part| !part.starts_with("clip_id="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One inline `@{type:target ...}` smart-clip occurrence found while scanning
/// playbook text, with the byte span (`start` is the index of `@`, `end` is
/// one past the closing `}`) so a caller can remove or replace it in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookSmartClipOccurrence {
    pub type_name: String,
    pub target: String,
    pub start: usize,
    pub end: usize,
}

/// Scan `text` for every inline `@{type:target ...}` smart-clip occurrence, in
/// source order. A clip body with no `:` is reported with `type_name` `"clip"`
/// and `target` set to the whole body. Does not descend into fenced
/// `:::clip ... :::` blocks. Used by the daemon's instant-dispatch fast path
/// (spec 0066) to find and strip a list item's harness clip without a full
/// Markdown parser.
pub fn playbook_scan_smart_clips(text: &str) -> Vec<PlaybookSmartClipOccurrence> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some(rel_start) = text[idx..].find("@{") {
        let start = idx + rel_start;
        let after = start + 2;
        let Some(rel_end) = text[after..].find('}') else {
            break;
        };
        let end = after + rel_end + 1;
        let body = &text[after..after + rel_end];
        let first = body.split_whitespace().next().unwrap_or(body);
        let (type_name, target) = first.split_once(':').unwrap_or(("clip", first));
        out.push(PlaybookSmartClipOccurrence {
            type_name: type_name.to_string(),
            target: target.to_string(),
            start,
            end,
        });
        idx = end;
    }
    out
}

/// True if a trimmed line is a Markdown ATX heading (`#`..`######` then a space).
fn playbook_is_heading(trimmed: &str) -> bool {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ')
}

/// Byte length of the list-item marker at the start of `trimmed` — the bullet
/// or ordered prefix plus its single separating space (e.g. 2 for `"- "`, 4
/// for `"12. "`). `None` for a bare/empty bullet (`"-"` alone) or a line that
/// is not a list item.
fn playbook_list_item_marker_len(trimmed: &str) -> Option<usize> {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return Some(2);
    }
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return Some(digits + 2);
        }
    }
    None
}

/// True if a trimmed line begins a Markdown list item: a `-`/`*`/`+` bullet
/// (with content or as a bare empty bullet) or an ordered `N.`/`N)` marker.
pub fn playbook_is_list_item(trimmed: &str) -> bool {
    if trimmed == "-" || trimmed == "*" || trimmed == "+" {
        return true;
    }
    if playbook_list_item_marker_len(trimmed).is_some() {
        return true;
    }
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && matches!(&trimmed[digits..], "." | ")")
}

/// The text following a list item's marker — e.g. `"task"` for both `"- task"`
/// and `"12. task"` — or `None` if `trimmed` is not a list item with content
/// (including a bare empty bullet). Used by the daemon's instant-dispatch
/// fast path (spec 0066) to derive a subagent prompt from a playbook list item.
pub fn playbook_list_item_text(trimmed: &str) -> Option<&str> {
    playbook_list_item_marker_len(trimmed).map(|len| &trimmed[len..])
}

/// Split Markdown into ordered blocks, finer than paragraphs: a run of non-blank
/// lines is broken at heading and list-item boundaries so each heading, list
/// item, and paragraph is its own block (spec 0053). Wrapped continuation lines
/// (non-blank, non-heading, non-item) stay with the item/paragraph above them.
/// The daemon uses this to compute the shimmer pending set and the per-block
/// projection; clients use it to map shimmer back onto source lines. Keeping a
/// single shared parser guarantees daemon and clients agree on block identity.
pub fn playbook_block_spans(markdown: &str) -> Vec<PlaybookBlockSpan> {
    let raw_lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut start: Option<usize> = None;
    let mut norm: Vec<String> = Vec::new();
    let push = |start: usize, end: usize, norm: &[String], blocks: &mut Vec<PlaybookBlockSpan>| {
        let signature = norm.join("\n");
        let id = playbook_block_id(&playbook_block_identity_signature(&signature));
        let text = raw_lines[start..end].join("\n");
        blocks.push(PlaybookBlockSpan {
            start_line: start,
            end_line: end,
            signature,
            id,
            text,
        });
    };
    for (i, line) in raw_lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Blank line ends the current block.
            if let Some(s) = start.take() {
                push(s, i, &norm, &mut blocks);
                norm.clear();
            }
            continue;
        }
        if playbook_is_heading(trimmed) {
            // A heading ends the current block and is a single-line block.
            if let Some(s) = start.take() {
                push(s, i, &norm, &mut blocks);
                norm.clear();
            }
            push(i, i + 1, &[trimmed.to_string()], &mut blocks);
            continue;
        }
        if playbook_is_list_item(trimmed) {
            // A list item ends the current block and begins a new one.
            if let Some(s) = start.take() {
                push(s, i, &norm, &mut blocks);
                norm.clear();
            }
            start = Some(i);
            norm.push(trimmed.to_string());
            continue;
        }
        // Continuation / paragraph line: extend the current block (or open one).
        if start.is_none() {
            start = Some(i);
        }
        norm.push(trimmed.to_string());
    }
    if let Some(s) = start {
        push(s, raw_lines.len(), &norm, &mut blocks);
    }
    blocks
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaybookRunStage {
    /// Client-local optimistic stage immediately after Run is pressed, before
    /// the execute call returns. Daemon snapshots normally advance to Delivered
    /// because shared run state is published after delivery succeeds.
    Pressed,
    Delivered,
    FirstOutput,
    PlanningPassDone,
    Settling,
}

impl Default for PlaybookRunStage {
    fn default() -> Self {
        Self::Pressed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookRunProgress {
    pub run_id: String,
    pub started_at_ms: i64,
    pub expires_at_ms: i64,
    /// Daemon-derived run-level fallback status for shimmering blocks whose
    /// agent-authored tooltip is missing (optimistic/legacy/keep_pending).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_status: Option<String>,
    /// Legacy content-derived ids of the blocks still pending in this run.
    /// New payloads prefer `pending_block_refs`; this is kept for older clients
    /// and dirty-buffer fallback rendering.
    #[serde(default)]
    pub pending_block_ids: Vec<String>,
    /// Stable block refs (`block_id:content_epoch`) still pending in this run.
    /// New clients and agents should use this; `pending_block_ids` remains as a
    /// legacy content-id projection for compatibility.
    #[serde(default)]
    pub pending_block_refs: Vec<String>,
    /// Per-block run-status tooltips keyed by stable block ref when available,
    /// or by legacy content id for fallback/older runs (spec 0057). Settling a
    /// block, or dropping it from the pending set, removes its entry. A pending
    /// block with no entry (optimistic/legacy/keep_pending) renders the
    /// hardcoded fallback.
    #[serde(default)]
    pub pending_block_tooltips: HashMap<String, String>,
    #[serde(default)]
    pub seen_running: bool,
    #[serde(default)]
    pub first_output_seen: bool,
    /// Internal daemon fact: true when Run was dispatched while the owning
    /// session was already in a turn. This is used to derive `system_status`;
    /// the projected status string is the client-facing contract.
    #[serde(default, skip_serializing)]
    pub queued_behind_current_turn: bool,
    /// True once an in-run playbook declaration/edit has narrowed this run —
    /// i.e. the run is actively managed via per-block declarations rather than
    /// riding the untouched optimistic full-playbook shimmer. A managed run is
    /// cleared by its pending set emptying, a terminal owning-session state, or
    /// the inactivity backstop — NOT by the owning session merely returning to
    /// awaiting-input (a self-scheduling agent goes idle while delegated or
    /// background work is still in flight). An unmanaged run that no
    /// declaration has narrowed still clears when the owning session goes idle
    /// after being seen running. See `specs/0042-playbook-run-progress-affordance.md`.
    #[serde(default)]
    pub agent_managed: bool,
    /// Derived compact stage for clients to render next to the Run control.
    /// It is computed from the run lifecycle facts above; clients should not
    /// use it as a stop signal.
    #[serde(default)]
    pub stage: PlaybookRunStage,
    /// Number of blocks from this run's initial pending set that have settled.
    #[serde(default)]
    pub settled_block_count: usize,
    /// Number of blocks in this run's initial pending set.
    #[serde(default)]
    pub total_block_count: usize,
}

impl PlaybookRunProgress {
    pub fn pending_block_count(&self) -> usize {
        if !self.pending_block_refs.is_empty() {
            self.pending_block_refs.len()
        } else {
            self.pending_block_ids.len()
        }
    }

    pub fn refresh_stage(&mut self) {
        let pending = self.pending_block_count();
        if self.total_block_count == 0 {
            self.total_block_count = pending;
        }
        self.total_block_count = self.total_block_count.max(pending);
        self.settled_block_count = self.total_block_count.saturating_sub(pending);
        self.stage = if self.agent_managed {
            if self.settled_block_count > 0 {
                PlaybookRunStage::Settling
            } else {
                PlaybookRunStage::PlanningPassDone
            }
        } else if self.first_output_seen {
            PlaybookRunStage::FirstOutput
        } else {
            PlaybookRunStage::Delivered
        };
    }

    pub fn with_refreshed_stage(mut self) -> Self {
        self.refresh_stage();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookRevision {
    pub version: u64,
    pub actor: PlaybookUpdateActor,
    pub at_ms: i64,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookTemplate {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub markdown: String,
    #[serde(default)]
    pub built_in: bool,
}

// Smart-clip reference types are entries in the shared construct Markdown
// dialect registry (spec 0074): see `dialect::CONSTRUCT_MARKDOWN_EXTENSIONS`.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookGetParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookGetResult {
    pub playbook: PlaybookDocument,
    #[serde(default)]
    pub revisions: Vec<PlaybookRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<PlaybookRunProgress>,
    /// Ordered per-block projection with each block's stable ref/id and current
    /// shimmer state (spec 0053). Derived from the live markdown; not persisted.
    #[serde(default)]
    pub blocks: Vec<PlaybookBlockView>,
    /// Ephemeral remote cursor/presence entries currently editing this Playbook
    /// (spec 0065). Not persisted; clients render them as advisory UI only.
    #[serde(default)]
    pub collaborators: Vec<PlaybookCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookUpdateParams {
    pub session_id: String,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u64>,
    #[serde(default)]
    pub actor: PlaybookUpdateActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Complete shimmer declaration over the blocks of `markdown`, in document
    /// order: `shimmer[i]` is the pending state of the i-th block (spec 0053).
    /// `None` leaves any active run's shimmer to narrow by content change only
    /// (the co-editing human-save path); the MCP tool requires it for agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shimmer: Option<Vec<bool>>,
    /// Per-block run-status tooltips parallel to `shimmer`, in document order
    /// (spec 0057): `shimmer_tooltips[i]` is the tooltip for the i-th block,
    /// used only when `shimmer[i]` is `true`. `None`, or a `None` entry, stores
    /// no tooltip for that block (the renderer falls back). When present its
    /// length must equal `shimmer`'s; the MCP tool requires a tooltip for every
    /// block declared pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shimmer_tooltips: Option<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookUpdateResult {
    pub playbook: PlaybookDocument,
    /// Fresh per-block projection after the write, so the caller rides the echo
    /// instead of re-reading (spec 0053).
    #[serde(default)]
    pub blocks: Vec<PlaybookBlockView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<PlaybookRunProgress>,
}

/// One anchored edit: replace `old_string` with `new_string` in the playbook
/// Markdown. An empty `old_string` appends `new_string` to the end of the
/// document. Anchored edits apply to the *latest* document content, so
/// concurrent edits to other regions merge without a version conflict; the
/// only failures are a vanished anchor (`old_string` not found) or an
/// ambiguous one (multiple matches without `replace_all`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookEdit {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
    /// Keep the block this edit produces in the playbook-run shimmer set, in the
    /// same call (spec 0053). Editing a block changes its text and therefore its
    /// id, so its prior shimmer does not carry over; setting this re-adds the
    /// resulting block's new id atomically — so a move/annotate of a still-
    /// pending block never transiently empties the pending set. Use it whenever
    /// an edit changes a block whose work is still in flight.
    #[serde(default)]
    pub keep_pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookEditParams {
    pub session_id: String,
    #[serde(default)]
    pub edits: Vec<PlaybookEdit>,
    #[serde(default)]
    pub actor: PlaybookUpdateActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Partial shimmer declaration applied after the edits (spec 0053): each
    /// entry sets the pending state of a block addressed by its stable ref/id, and
    /// may target any block, not only blocks this edit changed. Ids that match
    /// no post-edit block are dropped (the block changed underneath the caller).
    #[serde(default)]
    pub shimmer: Vec<PlaybookShimmerDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookCursor {
    pub session_id: String,
    /// Daemon-scoped connection identity, stable until disconnect.
    pub client_id: String,
    /// Short human-readable source label such as "TUI" or "Web".
    pub label: String,
    /// Client surface/kind, e.g. "tui" or "web", or "agent" for the owning
    /// agent's own presence cursor (spec 0065 agent presence).
    pub kind: String,
    /// Caret offset in Unicode scalar values within the current Playbook
    /// markdown. This matches the TUI's existing Playbook cursor units.
    pub cursor: usize,
    /// For a human cursor (`kind` "tui"/"web"), the bounds of that client's
    /// real text selection. For the agent's own presence cursor
    /// (`kind == "agent"`), these instead bound the span its last edit just
    /// wrote — there is no real selection to report — so renderers use them
    /// to briefly reveal-highlight where the edit landed rather than to draw
    /// a selection. A future selection-rendering feature must branch on
    /// `kind` before treating these as a genuine selection range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_anchor: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_head: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub color_index: u8,
    pub updated_at_ms: i64,
    /// `false` is a best-effort tombstone sent when a client leaves or
    /// disconnects; it should clear any rendered cursor for `client_id`.
    #[serde(default = "playbook_cursor_default_active")]
    pub active: bool,
}

fn playbook_cursor_default_active() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookCursorParams {
    pub session_id: String,
    pub cursor: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_anchor: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_head: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When true, clears this connection's cursor for the Playbook.
    #[serde(default)]
    pub clear: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookCursorResult {
    pub cursor: PlaybookCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookCursorNotificationPayload {
    pub cursor: PlaybookCursor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookExecuteParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u64>,
    /// Optional one-line instruction supplied with a Run gesture. The daemon
    /// appends it to the generated playbook-run prompt; the playbook body and
    /// shimmer scope remain unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Optional initial pending set over the executed body's blocks, in order
    /// (spec 0053). `None` keeps the optimistic default: the whole executed
    /// region shimmers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shimmer: Option<Vec<bool>>,
    /// For a selection Run, the stable block ref/id of every real document
    /// block the client's selection overlaps — computed via the same
    /// containment logic as the TUI's `selected_playbook_block_ids` (overlap
    /// of the selection's character range with each block's line range), not
    /// by re-hashing the selected substring's own text. Lets the daemon trust
    /// the client's block identity instead of re-parsing the raw selected
    /// text as its own standalone document and hash-matching, which only
    /// works when the selection exactly spans one or more whole blocks and
    /// otherwise fabricates a phantom block for a partial-line/partial-block
    /// selection. `None`/empty preserves today's substring-matching fallback,
    /// for older clients and callers (e.g. the MCP tool) that don't send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_block_ids: Option<Vec<String>>,
    /// Execute in a new interactive fork of the Playbook-owning session
    /// instead of the owner itself. This is the explicit Shift-modified
    /// override in the clients (spec 0137); omitting it runs on the owner.
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookExecuteResult {
    pub playbook: PlaybookDocument,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<PlaybookRunProgress>,
    /// Per-block projection of the playbook after the run was seeded (spec 0053).
    #[serde(default)]
    pub blocks: Vec<PlaybookBlockView>,
    /// The session that received the Run prompt. Present for both forked and
    /// owner-session execution so clients can identify the worker uniformly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookListTemplatesResult {
    pub templates: Vec<PlaybookTemplate>,
}

/// What a Playbook selection verb does with its subagent's structured result
/// (spec 0089): `Annotate` inserts new content adjacent to the selection and
/// leaves the selected text untouched; `Rewrite` replaces the selected
/// markdown with the returned content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaybookVerbEffect {
    Annotate,
    Rewrite,
}

/// Whether a Playbook selection verb's subagent runs to completion unattended
/// (`SingleShot`) or may hold a dialogue with the user inside its own session
/// before producing a result (`Interactive`) (spec 0089).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaybookVerbInteraction {
    SingleShot,
    Interactive,
}

/// A typed refinement action offered on a Playbook selection alongside Run
/// (spec 0089). Loaded from a markdown definition file: built-in verbs ship
/// embedded; user files under a `verbs/` config directory add to or, by
/// matching `name`, override them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookVerb {
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub effect: PlaybookVerbEffect,
    pub interaction: PlaybookVerbInteraction,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub built_in: bool,
    /// The purpose prompt (definition body) handed to the verb's subagent.
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookListVerbsResult {
    pub verbs: Vec<PlaybookVerb>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookVerbExecuteParams {
    pub session_id: String,
    /// A verb's `name` from `playbook.list_verbs`.
    pub verb: String,
    /// The selected markdown the verb operates on — a verb's entire
    /// jurisdiction. Unlike `playbook.execute`, there is no whole-document
    /// fallback: a verb always targets an explicit selection.
    pub selection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u64>,
    /// Optional one-line instruction composed onto the verb's purpose prompt,
    /// same convention as `playbook.execute`'s `comment` (spec 0137).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_block_ids: Option<Vec<String>>,
    /// Deliver the verb prompt to the Playbook owner instead of creating a
    /// fork. Owner delivery always implies a direct edit — there is no fork
    /// to merge a structured result back from — so the daemon treats this as
    /// `direct_edit` regardless of the flag below. Interactive clients set it
    /// for the unmodified gesture and clear it for the Shift-modified one
    /// (spec 0137); it stays default-off so API callers keep forking.
    #[serde(default)]
    pub run_on_owner: bool,
    /// Let the executing fork edit the owner's Playbook directly instead
    /// of returning a structured result for daemon-side merge. Ignored when
    /// `run_on_owner` is set.
    #[serde(default)]
    pub direct_edit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookVerbExecuteResult {
    pub playbook: PlaybookDocument,
    /// The subagent spawned to run the verb; its session clip is what the
    /// provisional edit annotates onto the selection.
    pub subagent_session_id: String,
    pub verb: String,
    #[serde(default)]
    pub blocks: Vec<PlaybookBlockView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookStateNotificationPayload {
    pub playbook: PlaybookDocument,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<PlaybookRunProgress>,
    /// Ordered per-block projection, included so clients can map stable
    /// block refs to the rendered Markdown without re-deriving identity.
    #[serde(default)]
    pub blocks: Vec<PlaybookBlockView>,
}

/// One user-invocable action contributed by an installed plugin (spec 0152
/// phase 2), as returned by `plugin.list_actions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginActionInfo {
    pub plugin_id: String,
    pub id: String,
    pub label: String,
    /// `session` (runs with the selected session's context) or `fleet`.
    #[serde(default)]
    pub context: String,
    /// The palette/slash token that invokes it — `<plugin-id>:<id>`, or the
    /// bare plugin id when the action id equals it.
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginListActionsResult {
    pub actions: Vec<PluginActionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRunActionParams {
    pub plugin_id: String,
    pub action_id: String,
    /// Session context for `context = "session"` actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub name: String,
    pub available: bool,
    /// Short human-readable reason for `available` — e.g. "ready", "ready
    /// (Claude subscription)", or "`claude` CLI not found on daemon PATH".
    /// `#[serde(default)]` so older daemons that predate this field still
    /// deserialize cleanly on newer clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// One auth method the built-in `smith` harness can use, as detected by the
/// daemon (spec 0069). Powers the `/configure` dialog's smith-auth tab: each
/// method reports whether its credential currently exists, so the TUI can
/// list them with live status and let the user pin one as smith's default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmithAuthMethodInfo {
    /// Stable id, e.g. `"anthropic_api_key"` or `"auto"`. Sent back verbatim
    /// in `SmithSetAuthMethodParams::method` to pick this method.
    pub id: String,
    /// Human label for display, e.g. "Anthropic API key".
    pub label: String,
    pub available: bool,
    /// Short human-readable reason, e.g. "ANTHROPIC_API_KEY is set".
    pub detail: String,
}

/// Result of `smith.auth_status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmithAuthStatusResult {
    pub methods: Vec<SmithAuthMethodInfo>,
    /// Which method id the daemon's config currently pins via
    /// `CONSTRUCT_SMITH_MODEL` — `Some("auto")` when nothing is pinned,
    /// `Some(id)` when the pin's prefix matches a known method, or `None`
    /// when it's pinned to something the dialog doesn't recognize (e.g. an
    /// `@profile` or a hand-edited spec).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// Params for `smith.set_auth_method`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmithSetAuthMethodParams {
    /// One of `SmithAuthMethodInfo::id`, or `"auto"` to clear the pin.
    pub method: String,
}

/// Result of `smith.set_auth_method`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmithSetAuthMethodResult {
    /// The `CONSTRUCT_SMITH_MODEL` value now written to config.toml, or
    /// `None` when `method: "auto"` cleared the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_spec: Option<String>,
    /// Guidance shown in the dialog — always notes that already-running
    /// adapters keep their current model and only sessions started after a
    /// `construct daemon restart` pick up the new pin.
    pub note: String,
}

/// Health of one ambient daemon feature (spec 0151). Serialized lowercase
/// so clients can string-match without importing the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureStatus {
    /// Fully working.
    Ok,
    /// Working for some sessions / via a fallback, but not at full fidelity.
    Degraded,
    /// Not working at all (unusable dependency or disabled in config).
    Off,
}

/// One ambient feature's live status, as reported by `features.status`
/// (spec 0151). Ambient features are the daemon-driven conveniences that
/// depend on the built-in smith harness having a usable model credential —
/// session auto-naming, next-prompt suggestions, the operator session — and
/// whose failure would otherwise be invisible to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureInfo {
    /// Stable id: `"auto_title"`, `"suggestions"`, `"operator"`.
    pub id: String,
    /// Human label for display, e.g. "Session auto-naming".
    pub label: String,
    pub status: FeatureStatus,
    /// Human-readable explanation of the status — what works, what doesn't,
    /// and why (e.g. "no smith credential — shell sessions get no
    /// suggestions").
    pub detail: String,
}

/// Result of `features.status`, also the payload of the `features/state`
/// notification (spec 0151).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesStatusResult {
    pub features: Vec<FeatureInfo>,
    /// True once any ambient feature has actually skipped work this daemon
    /// run because no smith credential was available — the signal clients
    /// use to surface a visible degradation notice instead of nagging on
    /// every credential-less machine that never hit the gap.
    #[serde(default)]
    pub degradation_observed: bool,
}

/// Clean a model-generated session title: first line only, quotes/markdown
/// stripped, length capped so a misbehaving model can't blow out the
/// modeline. Shared by smith's `--title-mode` one-shot and the daemon's
/// same-harness title-probe fallback so both produce identically shaped
/// titles.
pub fn sanitize_auto_title(raw: &str) -> String {
    let line = raw.lines().next().unwrap_or("");
    let mut s = line
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*' || c == '#');
    // Strip a single pair of surrounding quotes after the trim-match above
    // catches the easy cases.
    while let (Some('"'), Some('"')) = (s.chars().next(), s.chars().last()) {
        s = &s[1..s.len() - 1];
        s = s.trim();
    }
    let truncated: String = s.chars().take(48).collect();
    truncated.trim().to_string()
}

/// One labeled component of a session's occupied context window (spec
/// 0156), as reported by [`SessionEvent::ContextBreakdown`]. Segments are
/// additive parts of the *used* context, ordered fixed-prefix first;
/// "free space" and any unaccounted remainder are derived by clients from
/// the gauge (spec 0104) rather than reported here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSegment {
    /// Short lowercase display label chosen by the adapter, rendered
    /// verbatim — e.g. `"system prompt"`, `"tools"`, `"messages"`.
    pub label: String,
    pub tokens: u64,
    /// True when `tokens` comes from a char-length heuristic rather than a
    /// harness-/provider-reported count. Clients must render estimated
    /// segments visually distinct (a `~` prefix) — part of the spec 0156
    /// contract, not styling.
    #[serde(default)]
    pub estimated: bool,
}

impl ContextSegment {
    pub fn new(label: impl Into<String>, tokens: u64, estimated: bool) -> Self {
        Self {
            label: label.into(),
            tokens,
            estimated,
        }
    }
}

/// One cached harness usage-probe capture, as sent over the wire (spec
/// 0085). `bytes` is the raw PTY output the harness's own usage/status
/// slash command rendered — base64-encoded, deliberately unparsed (no token
/// counts or other structured fields are extracted). Clients feed it
/// through their own vt100 parser at `cols`x`rows` to redisplay it verbatim,
/// including color/layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshotInfo {
    /// Base64-encoded raw PTY bytes captured from the probe session.
    pub bytes: String,
    pub cols: u16,
    pub rows: u16,
    /// Unix epoch ms when the snapshot was captured. Clients use this (plus
    /// their own notion of "now") to decide whether to show the snapshot as
    /// possibly-stale while a background refresh is in flight.
    pub captured_at_ms: i64,
}

/// Params for `usage.query`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageQueryParams {
    /// Harness name, e.g. `"claude"`, `"codex"`, `"agy"`, `"grok"`.
    pub harness: String,
    /// When `true` and the cached snapshot is missing/stale and no probe is
    /// already in flight for this harness, the daemon spawns a background
    /// probe and returns immediately with `refreshing: true` rather than
    /// blocking the IPC dispatch loop for the several seconds a probe
    /// takes. Callers (the TUI, while hovering) poll again on their normal
    /// refresh cadence to pick up the result.
    #[serde(default)]
    pub allow_refresh: bool,
}

/// One token-usage sample in the daemon's fleet-wide rolling window
/// (spec 0167): when it was reported, what consumed it, and how much.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSample {
    /// Report time, Unix epoch ms — the same instant the transcript records,
    /// so a client binning these into a history graph places them where they
    /// actually happened rather than where it happened to learn of them.
    pub at_ms: i64,
    /// Model this usage is attributed to, already resolved by the daemon:
    /// the model the report named, else the one the session had in effect at
    /// that moment. `None` only when the session had never named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompt-side plus output tokens. Cached input is not added on top — it
    /// is already inside the prompt side, and adding it would double-count.
    pub tokens: u64,
    /// The part of `tokens` the provider served from its prompt cache rather
    /// than processing fresh. A subset of `tokens`, so `tokens - cached` is
    /// the new work in this sample (spec 0167). 0 when the harness reports no
    /// cache figure, and additive on the wire: records written before it
    /// existed load as 0.
    #[serde(default)]
    pub cached: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHistoryParams {
    /// How far back to reach, in seconds. Clamped to the daemon's own
    /// retention.
    pub window_secs: i64,
    /// Cap on returned samples. The daemon keeps the newest when it bites.
    pub max_samples: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHistoryResult {
    /// Oldest first.
    pub samples: Vec<TokenSample>,
    /// Daemon time when the window was taken, so a client can convert
    /// `at_ms` into an age without trusting the two clocks to agree.
    pub now_ms: i64,
}

/// Result of `usage.query`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageQueryResult {
    /// The most recently cached snapshot, if any — regardless of whether it
    /// is still fresh (the daemon's 5-minute TTL only governs whether a
    /// new probe is warranted, not whether the last snapshot is returned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<UsageSnapshotInfo>,
    /// Whether a probe is currently in flight for this harness (either
    /// triggered by this call or already running from an earlier one).
    pub refreshing: bool,
    /// Whether the usage probe is configured for this harness at all (see
    /// `usage_probe` in `config.toml`). `false` means `snapshot` will never
    /// populate and callers should stop polling.
    pub enabled: bool,
}

/// Distinguishes the implicit orchestrator session — created by the
/// daemon at startup as the user's always-available command surface —
/// from ordinary user-created sessions. The orchestrator session is
/// otherwise identical to a user session; clients use this flag to
/// render it differently (hidden from the session list, surfaced in
/// the TUI's minibuffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    User,
    Orchestrator,
    Subagent,
    /// A short-lived, daemon-internal session spun up to run a harness's
    /// own usage/status slash command and capture what it renders (spec
    /// 0085). Hidden from every session list via the same `User`-only
    /// allowlist that hides `Orchestrator`/`Subagent`; deleted (along with
    /// the native transcript file the harness CLI wrote for it) as soon as
    /// the probe finishes.
    UsageProbe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub harness: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// True while `title` is a system-provided fork placeholder that the first
    /// substantive prompt may replace through normal auto-title generation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_title_pending: bool,
    pub state: SessionState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the most recent chat `Message` event persisted — unlike
    /// `last_event_at`, which every transcript event refreshes (status rows,
    /// tool blocks, usage reports, resume plumbing), this only moves on
    /// actual conversation. Restored from the transcript at load so it
    /// survives daemon restarts and self-heals for sessions recorded before
    /// the field existed. `None` for sessions with no messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Best-effort reasoning-effort tier (e.g. `"high"`/`"medium"`/`"low"`),
    /// set by [`SessionEvent::EffortChanged`]. `None` when the harness has
    /// no such concept or none has been observed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Active model route (spec 0114). `None` — the overwhelmingly common
    /// case — means the session's model traffic passes through untouched.
    /// Note `model` keeps reporting what the *harness* believes it is
    /// running: the harness is not restarted by a route change and never
    /// learns of the substitution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<SessionRoute>,
    /// True when the daemon injected routing transport into this session
    /// at spawn, i.e. a route can be armed on it later. Sessions started
    /// before routing was enabled, or on a harness that is not
    /// route-capable, are `false` and can never be routed (spec 0115).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub route_capable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default)]
    pub pending_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub event_count: u64,
    /// True if the session's adapter owns a PTY (clients should default the
    /// view to terminal mode).
    #[serde(default)]
    pub has_pty: bool,
    /// The session's mode, as reported when it was started (e.g. "interactive"
    /// or "headless"). Adapter-defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// User-marked: should this session always be shown as a live tile in
    /// the TUI's pin strip, regardless of which session is currently selected?
    #[serde(default)]
    pub pinned: bool,
    /// Sort key for the list view. Sessions are ordered by `position`
    /// ascending; the daemon assigns `-created_at_ms` at creation so newer
    /// sessions appear at the top, and reorder operations swap the values.
    /// For a grouped session this is the position *within* its group.
    #[serde(default)]
    pub position: i64,
    /// Group membership. `None` = ungrouped (rendered at the top of the
    /// list); `Some(id)` = belongs to that group (rendered indented under
    /// the group's header, below the ungrouped region).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Parent session for internal child sessions such as Smith subagents.
    /// Clients can render these under the owning user session instead of as
    /// ordinary top-level sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Provenance for a read-only child session projected from a harness's
    /// own subagent mechanism. Presence means Construct does not own this
    /// child's process or lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_subagent: Option<NativeSubagentRef>,
    /// Unix epoch ms of the most recent PTY byte received from the adapter,
    /// or `None` if this session has never produced PTY output. Clients use
    /// `now - last_pty_at_ms < quiescence_window` as a "session looks busy"
    /// heuristic (drives the TUI's spinner; useful for MCP-driven agents to
    /// avoid sending input while the agent is mid-turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pty_at_ms: Option<i64>,
    /// Total accumulated COMPUTE time, ms: the sum of every completed span
    /// this session spent in `SessionState::Running`, maintained by the
    /// daemon on state transitions. Excludes the in-flight span — combine
    /// with `busy_running_since_ms` (see [`SessionSummary::busy_ms_at`]).
    /// `0` for sessions recorded before this field existed.
    #[serde(default)]
    pub busy_ms: u64,
    /// Unix epoch ms when the current `Running` span began, if the session
    /// is computing right now; `None` while idle. Set/cleared by the daemon
    /// alongside `busy_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_running_since_ms: Option<i64>,
    /// Count of chat `Message` events persisted to the transcript —
    /// unlike `event_count` (the raw transcript sequence, which also
    /// advances on tool blocks, status rows, and PTY ordering markers),
    /// this counts only actual messages. Maintained by the daemon as
    /// events persist and recounted from the transcript at load, so it
    /// self-heals for sessions recorded before the field existed.
    #[serde(default)]
    pub message_count: u64,
    /// Lifetime token consumption, accumulated from `SessionEvent::Cost`
    /// reports as they persist and recounted from the transcript at load
    /// (spec 0103), so it self-heals like `message_count`. All-zero for
    /// harnesses that never report usage.
    #[serde(default, skip_serializing_if = "TokenTally::is_zero")]
    pub tokens: TokenTally,
    /// Latest context-window fill report (spec 0104): prompt-side tokens of
    /// the most recent model call. `None` until a harness reports (and after
    /// a context reset). Restored from the transcript's last report at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    /// The model's context-window size, when the harness states it alongside
    /// its usage reports. `None` = unknown; clients show bare usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Latest per-component context breakdown report (spec 0156). Empty for
    /// harnesses that report none; cleared on context reset; restored from
    /// the transcript's last report at load, like the gauge above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_segments: Vec<ContextSegment>,
    /// How adapters that gate tools handle Risky tool calls.
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    /// Distinguishes the orchestrator session (daemon-created, hidden
    /// from the session list) from ordinary user sessions. Persisted
    /// in `meta.json` so the daemon recognizes the orchestrator
    /// across restarts.
    #[serde(default)]
    pub kind: SessionKind,
    /// Archived sessions have had their adapter terminated and are hidden
    /// from the session list by default, but keep their transcript/worktree
    /// and can be restarted. Persisted in `meta.json`; cleared on restart.
    /// The daemon does not auto-resume archived sessions on startup.
    #[serde(default)]
    pub archived: bool,
    /// Operator ambient loop is disabled (`/operator disable`). Only meaningful
    /// for the orchestrator session; true (disabled) by default on fresh create,
    /// false for all other session kinds.
    #[serde(default)]
    pub operator_loop_disabled: bool,
    /// Sticky "this session needs you" marker. The daemon raises it when the
    /// session leaves `Running` for a non-running state (awaiting input / done
    /// / errored) while it isn't the focused session, and clears it when the
    /// operator switches to it or it returns to `Running`. Persisted so it
    /// survives daemon/client restart. Orthogonal to `state` — not a run state.
    #[serde(default)]
    pub needs_attention: bool,
    /// Fork lineage. Normal sessions and subagents leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ForkedFrom>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<ForkMerge>,
}

impl SessionSummary {
    /// Total compute time as of `now_ms`: every completed `Running` span
    /// plus the in-flight one, if the session is computing right now.
    pub fn busy_ms_at(&self, now_ms: i64) -> u64 {
        let mut busy = self.busy_ms;
        if let Some(since) = self.busy_running_since_ms {
            busy = busy.saturating_add(now_ms.saturating_sub(since).max(0) as u64);
        }
        busy
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSubagentRef {
    /// Construct session whose adapter owns the native child tree.
    pub owner_session_id: String,
    /// Stable identifier assigned by the wrapped harness.
    pub native_id: String,
    /// High-water mark over the adapter's deterministic per-child emission
    /// ordinals (`NativeSubagent::seq`): the count already projected into
    /// this mirror's transcript. Emissions with `seq < projected_seq` are
    /// replays of lines the mirror already has and are dropped, so adapters
    /// can re-scan child files from the top on every (re)start.
    #[serde(default)]
    pub projected_seq: u64,
}

/// A session's token consumption tally (spec 0103). Mirrors the fields of
/// `SessionEvent::Cost`: `input` is the total prompt-side count *including*
/// cache reads, `cached` the subset of `input` served from the provider's
/// prompt cache. Used both as a lifetime accumulator on `SessionSummary`
/// and as a boundary stamp on fork/merge records, so lineage windows get
/// their token deltas by plain subtraction.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenTally {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cached: u64,
}

impl TokenTally {
    pub fn is_zero(&self) -> bool {
        *self == TokenTally::default()
    }

    /// Model-work volume: input + output. What lineage labels display.
    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    pub fn add(&mut self, input: u64, output: u64, cached: u64) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.cached = self.cached.saturating_add(cached);
    }

    /// Field-wise saturating subtraction — a window's delta between two
    /// checkpoint stamps.
    pub fn saturating_sub(&self, other: &TokenTally) -> TokenTally {
        TokenTally {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cached: self.cached.saturating_sub(other.cached),
        }
    }

    /// Field-wise max — checkpoint advancement (counters only grow).
    pub fn max(&self, other: &TokenTally) -> TokenTally {
        TokenTally {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cached: self.cached.max(other.cached),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkedFrom {
    pub session_id: String,
    pub transcript_seq: u64,
    pub at_ms: i64,
    /// The parent's accumulated compute time (`busy_ms_at`) at the moment
    /// this fork branched — the busy-time counterpart to `transcript_seq`,
    /// letting lineage windows report summed compute time instead of
    /// wall-clock spans. `#[serde(default)]` for records predating it.
    #[serde(default)]
    pub parent_busy_ms: u64,
    /// The parent's `message_count` at the moment this fork branched —
    /// the message-only counterpart to `transcript_seq`, letting lineage
    /// windows count actual chat messages instead of raw transcript
    /// events. `#[serde(default)]` for records predating it.
    #[serde(default)]
    pub parent_message_count: u64,
    /// The parent's token tally at the moment this fork branched — the
    /// token counterpart to `parent_message_count` (spec 0103). All-zero
    /// for records predating it (lineage falls back to message counts).
    #[serde(default, skip_serializing_if = "TokenTally::is_zero")]
    pub parent_tokens: TokenTally,
    /// Set when this fork was synthesized automatically by a harness-native
    /// context reset (`/clear` and equivalents, spec 0085) rather than
    /// created by a user picking a harness and forking on purpose. Lineage
    /// rendering uses this to show a distinct edge glyph (`↺` vs `⑂`) so the
    /// two are never confused at a glance.
    #[serde(default)]
    pub is_reset_snapshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkMerge {
    pub mode: ForkMergeMode,
    pub at_ms: i64,
    /// The parent's `event_count` (transcript sequence counter) at the
    /// moment this fork merged back — the same counter scale as
    /// `ForkedFrom::transcript_seq`, so lineage rendering can carve the
    /// parent's timeline into segments (fork-out to merge-back, merge-back
    /// to the next fork-out, ...) using plain arithmetic, no extra fetch.
    /// `#[serde(default)]` so merge records persisted before this field
    /// existed still deserialize (as `0`) rather than fail to load.
    #[serde(default)]
    pub merged_seq: u64,
    /// The parent's accumulated compute time (`busy_ms_at`) at the moment
    /// this fork merged back — the busy-time counterpart to `merged_seq`.
    #[serde(default)]
    pub merged_busy_ms: u64,
    /// The parent's `message_count` at the moment this fork merged back —
    /// the message-only counterpart to `merged_seq`.
    #[serde(default)]
    pub merged_message_count: u64,
    /// The parent's token tally at the moment this fork merged back — the
    /// token counterpart to `merged_message_count` (spec 0103).
    #[serde(default, skip_serializing_if = "TokenTally::is_zero")]
    pub merged_tokens: TokenTally,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForkMergeMode {
    Result,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMoveParams {
    pub session_id: String,
    pub direction: MoveDirection,
}

/// `false` means the session was already at the edge of its reorder
/// region (top/bottom of the list, or — for a forked session — the edge
/// of its sibling forks) and nothing moved.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SessionMoveResult {
    pub moved: bool,
}

/// Move a session into a group (or ungroup it by passing
/// `group_id: None`). `position` controls where in the target region
/// the session lands — `Top` of the list or `Bottom`. Default is
/// `Bottom` so a newly-grouped session appears at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionGroupPosition {
    Top,
    Bottom,
}

impl Default for SessionGroupPosition {
    fn default() -> Self {
        Self::Bottom
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetGroupParams {
    pub session_id: String,
    /// `None` ungroups the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default)]
    pub position: SessionGroupPosition,
}

/// Project-named compatibility shape for `session.set_project`.
/// Internally this still maps to the session's persisted `group_id`
/// until the storage model is renamed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetProjectParams {
    pub session_id: String,
    /// `None` removes the session from its project.
    #[serde(default, alias = "group_id", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub position: SessionGroupPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetPinnedParams {
    pub session_id: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetTitleParams {
    pub session_id: String,
    /// `None` clears any user-set title (display falls back to the hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetApprovalModeParams {
    pub session_id: String,
    pub mode: ApprovalMode,
}

/// A session's active model route (spec 0114). Present on
/// [`SessionSummary`] only while a route is armed; absent means the
/// session is passing through to whatever endpoint its harness resolved
/// on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRoute {
    /// Name of the configured route (the `[router.routes.<name>]` key).
    pub name: String,
    /// Model the route substitutes into outbound requests.
    pub model: String,
    /// Optional pin-chosen reasoning effort applied on pin-routed requests
    /// when the target advertises a real effort scale (spec 0160/0165).
    /// Absent when the pin did not choose one, or the target has no
    /// selectable scale. Distinct from [`SessionSummary::effort`], which is
    /// the harness-observed live tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The model the harness itself last reported, captured when the
    /// route was armed. Lets a client render `<origin> → <routed>` even
    /// though the harness keeps reporting its own model (spec 0114).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_model: Option<String>,
    /// Set once the proxy has served at least one intercepted request for
    /// this session. Until then the route is armed but unproven: the
    /// harness may resolve its endpoint through a channel that ignores our
    /// injection (spec 0115).
    #[serde(default)]
    pub observed: bool,
}

/// Params for `session.set_route`. `route: None` clears the route and
/// returns the session to pass-through, which can never fail (spec 0114).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetRouteParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Which of the target's models to send. `None` takes the target's
    /// configured or default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Pin-chosen reasoning effort for this route. `None` leaves the
    /// harness request body as the source of effort (or the target
    /// default). Rejected when the target does not advertise that level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Params for `router.login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLoginParams {
    /// Name of the subscription route to sign in to (a route whose
    /// [`RouteOption::login_command`] was present).
    pub route: String,
    /// Where the login session runs. Defaults to the daemon's `$HOME` —
    /// login flows do not care, and a project directory would only leak
    /// into the session title's context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_size: Option<PtySize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterListRoutesParams {
    /// Session the routes would be applied to. Determines
    /// [`RouterListRoutesResult::unavailable_reason`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// One selectable route, as offered to a client's picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteOption {
    pub name: String,
    /// Wire dialect the target speaks (`"anthropic"`, `"openai"`, ...).
    pub dialect: String,
    /// The model armed when none is chosen explicitly.
    pub model: String,
    /// Models selectable for this target. Always contains `model`; a
    /// client offers these as a second step after picking the target.
    #[serde(default)]
    pub models: Vec<String>,
    /// Selectable reasoning-effort levels keyed by model name (spec 0160 /
    /// 0165). A missing key or empty list means the target has no real
    /// effort scale for that model — the picker omits the third column.
    /// Single-level "provider default" advertisements are not listed here.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub efforts: std::collections::BTreeMap<String, Vec<String>>,
    /// Endpoint host, for display. Never carries a credential.
    pub base_url: String,
    /// `None` when the route is selectable. `Some(reason)` when it is
    /// shown but not selectable — a missing API key, or a dialect the
    /// session's harness does not speak. Rendered rather than hidden
    /// (spec 0115).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// When the blocker is a missing or expired subscription login, the
    /// owning CLI's command that renews it. The user fixes it by running
    /// this command themselves — clients may offer to run it in a new
    /// session, but the owning tool stays the credential's only writer
    /// (spec 0117).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterListRoutesResult {
    pub routes: Vec<RouteOption>,
    /// `None` when the session can be routed. `Some(reason)` when routing
    /// is impossible for this session as a whole — the harness is not
    /// route-capable, the session started before routing was enabled, or
    /// the router is disabled. Clients show the reason instead of an
    /// empty picker (spec 0115).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// The route currently armed on the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// True when this session was launched with Construct's routes
    /// published into the harness's own model picker (spec 0157). The
    /// harness may then be carrying a Construct model id per request,
    /// which selects its own route and makes an armed pin inert until the
    /// harness returns to a native model — pickers surface that state
    /// rather than presenting the pin as the whole truth (spec 0158).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub native_catalog: bool,
}

/// Params for `session.tool_action` — the client asks the adapter
/// to take an action on a running tool call (the user clicked a
/// `[kill]` / `[bg]` button, or pressed an override key). The
/// adapter looks up `call_id` in its running-tasks registry and
/// either aborts the task (`"kill"`) or moves it to the
/// background pool (`"background"`); unknown actions are ignored
/// with a warning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolActionParams {
    pub session_id: String,
    pub call_id: String,
    /// One of `"kill"`, `"background"`. Open string so finer
    /// actions can be added without a protocol break.
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolDecisionParams {
    pub session_id: String,
    /// Matches the `call_id` in the originating
    /// [`SessionEvent::ToolApprovalRequest`].
    pub call_id: String,
    /// One of `"approve"`, `"deny"`, `"auto_review"`, `"unsafe_auto"`.
    /// Open string so finer decisions can be added without a protocol break.
    pub decision: String,
}

/// Schedule for a recurring prompt injection. v1 supports interval
/// only; the wire format is open so future cron expression support
/// is additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopSpec {
    /// Fire every `seconds` seconds.
    Interval { seconds: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    pub id: String,
    pub session_id: String,
    pub spec: LoopSpec,
    pub prompt: String,
    pub created_at_ms: i64,
    pub next_fire_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at_ms: Option<i64>,
    #[serde(default)]
    pub fire_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopCreateParams {
    pub session_id: String,
    pub spec: LoopSpec,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopListParams {
    /// `None` = list every session's loops; `Some` = scope to one
    /// session. The MCP / CLI surface uses the unscoped form; the
    /// smith tool defaults to the calling session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopListResult {
    pub loops: Vec<Loop>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopUpdateParams {
    pub loop_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<LoopSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRemoveParams {
    pub loop_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyReplayResult {
    /// Base64-encoded raw bytes representing the requested PTY log range.
    pub data: String,
    /// Absolute byte offset where `data` starts in `pty.log`.
    #[serde(default)]
    pub start_offset: u64,
    /// Absolute byte offset where `data` ends in `pty.log`.
    #[serde(default)]
    pub end_offset: u64,
    /// Current total byte length of `pty.log`.
    #[serde(default)]
    pub total_bytes: u64,
    /// Most recent known PTY size for the session, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PtySize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyReplayParams {
    pub session_id: String,
    /// Return up to this many bytes before `before_offset`. When omitted,
    /// defaults to the historical replay cap from the current log tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
    /// Exclusive end offset for the requested range. When omitted, uses the
    /// current end of `pty.log`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub events: Vec<TimestampedEvent>,
    /// Current durable, file-backed UI widgets for this session. Kept separate
    /// from `events` so widgets persist without entering model-facing history.
    #[serde(default)]
    pub ui_panels: Vec<UiPanel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedEvent {
    pub seq: u64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionParams {
    pub harness: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Adapter-defined mode. Conventional values: `"interactive"` (PTY) or
    /// `"headless"` (structured stream). Adapters pick their own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Initial PTY size if the adapter is going to allocate a PTY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_size: Option<PtySize>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Marks an internal daemon caller as creating the orchestrator
    /// session. Public IPC clients should leave this as
    /// `SessionKind::User`; daemon-internal `ensure_orchestrator`
    /// passes `Orchestrator`.
    #[serde(default)]
    pub kind: SessionKind,
    /// Parent session for internal child sessions such as Smith subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Group to file the new session under. `None` (default) creates
    /// an ungrouped session. The TUI uses this to auto-join the new
    /// session to whichever group is currently selected (or contains
    /// the selected session) so creating a session from inside a
    /// group keeps the user's mental grouping intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Optional placement hint for clients that are creating a related
    /// session and want it to render immediately after an existing
    /// visible session in the same group/project region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_after_session_id: Option<String>,
    /// Optional fork ancestry, persisted on the new top-level session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ForkedFrom>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMergeParams {
    pub session_id: String,
    pub mode: ForkMergeMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResult {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptParams {
    pub session_id: String,
    #[serde(default)]
    pub from: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Return up to `limit` events immediately before this sequence number.
    /// Results remain chronological. When set, `from` is ignored. This lets
    /// readers page upward from a recent tail without scanning or rendering
    /// the transcript from its beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Return the most-recent `tail` events instead of paginating forward
    /// from `from`. When set, `from`, `before`, and `limit` are ignored. Used
    /// by the webui to render the live tail of long histories immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptResult {
    pub events: Vec<TimestampedEvent>,
    pub total: u64,
}

/// Which corner of a session's stored data `session.search` looks in.
/// `Name` is the same instant, in-memory match the TUI's `C-x b` picker
/// already does (title/id/short-id/harness); `Playbook` and `Transcript`
/// scan on-disk files and are what the daemon-side search engine adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    Name,
    Playbook,
    Transcript,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchParams {
    pub query: String,
    /// Which scopes to search. `None` searches all three.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<SearchScope>>,
    /// Restrict the search to these session ids. `None` searches every
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ids: Option<Vec<String>>,
    /// Global cap on the number of hits returned across all sessions and
    /// scopes. Defaults to 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Cap on hits contributed by a single session for each scope.
    /// Defaults to 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_session_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    /// `session_switch`-style label: the session's title, or its short id
    /// when untitled.
    pub title: String,
    pub harness: String,
    pub scope: SearchScope,
    /// Transcript hits only: the event's sequence number, usable as
    /// [`TranscriptParams::from`] to read the surrounding context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// Context around the match, trimmed to ~200 chars.
    pub snippet: String,
    /// Byte offsets of the match within `snippet` (not within the source
    /// document), for highlighting.
    pub match_start: usize,
    pub match_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// True if any per-session or global budget/limit cut coverage short —
    /// there may be more matches than what's returned.
    pub truncated: bool,
    /// Number of sessions the search actually looked at (may be less than
    /// the total session count when the global hit limit was reached
    /// early).
    pub sessions_scanned: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventNotificationPayload {
    pub session_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub event: SessionEvent,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateNotificationPayload {
    pub session: SessionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedNotificationPayload {
    pub session_id: String,
}

/// Payload of the `remote/state` notification — number of remote WS
/// clients currently attached to the daemon. Local clients (Unix
/// socket) don't count; this is specifically the "is someone else
/// Remote-control listener availability and attached-client count for the
/// local TUI's persistent status-bar affordance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStateNotificationPayload {
    #[serde(default)]
    pub enabled: bool,
    pub clients: u32,
}

/// How the remote listener is published beyond this machine.
///
/// The listener itself always binds every interface and is gated by
/// HTTP Basic auth, so it is reachable from the local network with no
/// provider at all. A provider adds reach *past* the LAN:
///
/// - `Cloudflare` — a `cloudflared` quick tunnel. Public internet,
///   ephemeral URL that nobody can guess. Needs no account.
/// - `Construct` — Construct's authenticated first-party service,
///   with a user-chosen stable name under `tunnel.zarvis.ai`.
///
/// The enum keeps a `None` variant and room for more providers because
/// the daemon models "reach the LAN" and "reach past it" as one axis;
/// the tunnel backends sit behind a seam so another provider is a
/// backend plus a variant, not a reshape.
///
/// `None` is not a failure state — it is the resting state of the
/// dialog before the user picks, and it is a perfectly good way to
/// use remote control from a phone on the same Wi-Fi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProvider {
    #[default]
    None,
    Cloudflare,
    Construct,
}

impl TunnelProvider {
    /// Stable u8 encoding so the daemon can keep the live provider in
    /// an atomic alongside the rest of the remote state.
    pub fn as_u8(self) -> u8 {
        match self {
            TunnelProvider::None => 0,
            TunnelProvider::Cloudflare => 1,
            TunnelProvider::Construct => 2,
        }
    }

    /// Inverse of [`TunnelProvider::as_u8`]. Unknown values decode to
    /// `None` rather than panicking — a forward-compatible snapshot
    /// written by a newer daemon degrades to "no tunnel", which is
    /// safe.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => TunnelProvider::Cloudflare,
            2 => TunnelProvider::Construct,
            _ => TunnelProvider::None,
        }
    }

    /// Human-facing name, used in dialogs, hints, and log lines.
    pub fn label(self) -> &'static str {
        match self {
            TunnelProvider::None => "local network",
            TunnelProvider::Cloudflare => "Cloudflare",
            TunnelProvider::Construct => "tunnel.zarvis.ai",
        }
    }
}

/// Params for the `remote.start` IPC method.
///
/// `provider` defaults to `None`: bind the listener and return the
/// local/LAN URL without exposing anything past this network. That is
/// what opening the `/remote-control` dialog does — a tunnel is only
/// created once the user explicitly picks a provider, so merely
/// *looking* at the dialog never publishes the daemon.
///
/// `password` is the optional user-supplied override for the HTTP
/// Basic auth gate. When unset, the daemon auto-generates a
/// memorable 3-token password (`swift.fox.42` shape). The chosen
/// password is returned in `RemoteStartResult` so the popup can
/// display it. Only honored on the first start of a listener's life.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteStartParams {
    #[serde(default)]
    pub provider: TunnelProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Stable first label requested from providers that support named
    /// tunnels. Ignored by providers with provider-assigned names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
    /// Tunnel mode normally waits for the provider's URL before
    /// replying. Interactive clients can set this false to get an
    /// immediate local result, paint a progress dialog, then poll
    /// again with waiting enabled in the background. Ignored when
    /// `provider` is `None` (there is nothing to wait for).
    #[serde(default = "default_true")]
    pub wait_for_tunnel: bool,
}

fn default_true() -> bool {
    true
}

/// Params for `dev.set_assets`. `dir: None` reverts to the embedded
/// web-UI assets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevSetAssetsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Result of `dev.set_assets`: the directory now in effect (`None` =
/// embedded assets).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevAssetsResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Result of the `remote.start` IPC method. Always reflects what
/// the daemon was *asked* for (via `RemoteStartParams`), never a
/// "best effort fallback". With a provider set, the daemon waits up
/// to ~15s for that provider to publish its URL and returns a
/// JSON-RPC *error* if it doesn't — it never silently degrades to
/// the local URL, because a user who asked to be reachable from a
/// coffee shop must not be told "ready" when they are not.
///
/// With `provider: None`, the reply is immediate and describes what
/// is reachable without any tunnel: the no-auth loopback web UI
/// always, plus the authenticated LAN URL when this machine has one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStartResult {
    /// The URL to show most prominently and encode in `qr`. The
    /// provider's URL once a tunnel is up; otherwise the LAN URL,
    /// falling back to loopback on a machine with no usable LAN
    /// address.
    pub url: String,
    /// Multi-line Unicode QR rendering of `url`. Empty when the
    /// encoder rejected the input (rare); callers should fall back
    /// to printing the URL alone.
    pub qr: String,
    /// True iff `url` is a provider URL reachable beyond this
    /// network. False whenever `url` is the LAN or loopback address.
    pub tunnel_ready: bool,
    /// HTTP Basic auth password for LAN and applicable tunnel access.
    /// The loopback-only `local_url` does not require it. Either the
    /// caller's override (from `RemoteStartParams.password`) or the
    /// daemon's auto-generated 3-token string. The username is fixed
    /// (see `REMOTE_USERNAME` in the daemon).
    pub password: String,
    /// Optional user-readable hint. Reserved for unusual states —
    /// e.g. "no LAN address found", or "starting tunnel…" on the
    /// non-waiting first leg of an interactive start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Which provider is actually publishing `url`. `None` means the
    /// listener is only reachable from this machine + the LAN.
    #[serde(default)]
    pub provider: TunnelProvider,
    /// The daemon's loopback-only web UI URL. Always populated, whatever
    /// the provider, and does not require Basic auth.
    #[serde(default)]
    pub local_url: String,
    /// LAN URL (`http://<private-ip>:<port>/`), when this machine has
    /// a private-range IPv4 address. `None` on a machine with no LAN
    /// (CI containers, VPN-only routing), in which case the dialog
    /// says so rather than showing an address that cannot work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_url: Option<String>,
    /// The tunnel provider currently live on this listener, if any —
    /// independent of what *this* call requested. A `provider: None`
    /// (LAN) reply can still report `active_provider: Cloudflare` when a
    /// tunnel from an earlier start is running, so the dialog can badge
    /// that provider's button as active instead of implying nothing is
    /// exposed.
    #[serde(default)]
    pub active_provider: TunnelProvider,
    /// Short-lived browser login URL while an interactive provider is
    /// waiting for authorization. It is a one-time request identifier,
    /// not an owner credential. Never persisted by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
}

/// Params for the `remote.providers` IPC method. No options — the
/// daemon probes every provider it knows about.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteProvidersParams {}

/// One provider's readiness, as probed by the daemon.
///
/// The dialog lists a provider even when it cannot run, so the user
/// sees a greyed button with the reason rather than a button that does
/// nothing — a provider silently absent from the list teaches nobody
/// anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProviderInfo {
    pub provider: TunnelProvider,
    /// True iff starting this provider right now would be expected to
    /// work — e.g. its binary is present.
    pub available: bool,
    /// Why it is unavailable, phrased as something the user can act on
    /// ("cloudflared is not on PATH — install with `brew install
    /// cloudflared`"). `None` when `available` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Result of `remote.providers`: one entry per known provider, in a
/// stable order suitable for rendering as a row of buttons.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteProvidersResult {
    pub providers: Vec<RemoteProviderInfo>,
}

/// Params for the `remote.stop` IPC method.
///
/// `tunnel_only` is the difference between "stop the tunnel" and "stop
/// remote control". With it set, the tunnel process is torn down but
/// the LAN listener, its password, and any LAN-connected clients are
/// left untouched — the dialog's `stop` button, which drops the public
/// URL and returns you to the local-network view. Left false (the
/// `/remote-control stop` command), everything comes down and the next
/// start mints fresh credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteStopParams {
    #[serde(default)]
    pub tunnel_only: bool,
}

/// Result of the `remote.stop` IPC method. `was_running: false` is
/// not an error — it means the caller invoked stop while nothing
/// was running, which is the natural state after fresh daemon
/// boot or a prior stop. The CLI surfaces this distinction so the
/// status line can say "remote stopped" vs "remote wasn't
/// running".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStopResult {
    pub was_running: bool,
    /// Provider that was active immediately before the stop. Clients use
    /// this to perform provider-specific logout without guessing from UI state.
    #[serde(default)]
    pub provider: TunnelProvider,
}

/// Params for `daemon.restart`. `exe: None` re-execs the daemon's own
/// binary (the upgrade-in-place case); `Some(path)` execs a different
/// binary instead — e.g. a freshly-built one from a worktree. The path
/// is validated (must exist + be executable) before the restart fires,
/// so a typo returns an error rather than bricking the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonRestartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// When true, also bounce every session's adapter as part of the
    /// restart: each adapter is gracefully stopped before the daemon
    /// re-execs, so the new daemon respawns a fresh adapter process
    /// (and its session-scoped `construct-mcp` child) for each session.
    /// Sessions are preserved/resumed from on-disk state — neither
    /// archived nor deleted. When false (the default) adapters survive
    /// the exec and reattach, so MCP children are *not* restarted.
    #[serde(default)]
    pub restart_sessions: bool,
}

/// Result of `daemon.restart`. Echoed back to the caller right
/// before the daemon exec()s itself — clients see this reply, then
/// observe the IPC socket close as the new process replaces the
/// old. `exe` is the path the new process will load from (typically
/// the same `current_exe()`, but useful for the CLI to show "now
/// running build X" once it reconnects).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRestartResult {
    pub exe: String,
    pub pid: u32,
    /// True iff a still-running tunnel subprocess was found and the
    /// new daemon will adopt it rather than spawn a fresh one. Lets
    /// the CLI emit "remote URL preserved" vs "remote URL will
    /// rotate" in the status line.
    pub tunnel_preserved: bool,
}

/// Params for `daemon.shutdown`. No options today — the daemon always
/// stops adapters gracefully (leaving sessions resumable) and exits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonShutdownParams {}

/// Result of `daemon.shutdown`. Echoed back to the caller right before
/// the daemon stops its adapters and exits; clients see this reply,
/// then observe the IPC socket close as the process goes away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonShutdownResult {
    /// PID of the daemon that is shutting down.
    pub pid: u32,
}

/// A user-created group used to organize sessions in the list view.
/// Sessions can belong to at most one group; ungrouped sessions render at
/// the top of the list, groups render below them in `position` order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSummary {
    pub id: String,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Sort key among groups (smaller = nearer the top of the groups
    /// region). Groups never sort above ungrouped sessions.
    #[serde(default)]
    pub position: i64,
    /// When true, the group's members are hidden from the list view —
    /// only the header is shown. Toggled by `Space` in the TUI.
    #[serde(default)]
    pub collapsed: bool,
}

/// Product name for the same persisted organizer still stored under the
/// historical `GroupSummary` / `group_id` fields (storage rename is a
/// separate migration). Wire IPC uses `project.*` only.
pub type ProjectSummary = GroupSummary;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCreateParams {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCreateResult {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIdParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeleteParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    /// When true, cascade-delete every member session before removing
    /// the project. When false (default), members are orphaned —
    /// `group_id` on their summary clears to `None` and they survive.
    #[serde(default)]
    pub delete_members: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRenameParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSetCollapsedParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMoveParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub direction: MoveDirection,
}

/// Which way a [`LayoutNode::Split`] divides its area. Mirrors the TUI's
/// window-split directions so the same tree renders identically in a
/// character grid and a pixel viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutSplitDirection {
    Below,
    Right,
}

/// The shared split layout: a recursive binary tree of panes.
///
/// A leaf is a *pane*, and a pane shows at most one session. Ratios are
/// percentages rather than absolute sizes precisely so the tree is
/// client-agnostic — the same 60/40 split is meaningful in an 80-column
/// terminal and in a 3440px browser window.
///
/// `session_id` is optional because a pane can legitimately be empty: a TUI
/// window whose selection is a group header or an archived-row disclosure
/// has no session to show, and a freshly split pane may not have picked one
/// yet. Clients must treat `None` as "empty pane", never as an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutNode {
    Leaf {
        /// Stable per-pane id. Clients key per-pane local state (scrollback,
        /// view mode, focus) off this, so it must survive a round trip
        /// through the daemon unchanged.
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    Split {
        direction: LayoutSplitDirection,
        /// Share of the split's area given to `first`, 1..=99.
        ratio_percent: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl Default for LayoutNode {
    fn default() -> Self {
        Self::Leaf {
            id: 1,
            session_id: None,
        }
    }
}

impl LayoutNode {
    /// Every leaf id, in layout order (first before second). Layout order is
    /// the shared pane ordering clients use for "pane N" — see spec 0061.
    pub fn leaf_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_leaf_ids(&mut out);
        out
    }

    fn collect_leaf_ids(&self, out: &mut Vec<u64>) {
        match self {
            Self::Leaf { id, .. } => out.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_leaf_ids(out);
                second.collect_leaf_ids(out);
            }
        }
    }

    /// Session ids of every pane, in layout order, skipping empty panes.
    pub fn session_ids(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_session_ids(&mut out);
        out
    }

    fn collect_session_ids<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Leaf { session_id, .. } => {
                if let Some(id) = session_id.as_deref() {
                    out.push(id);
                }
            }
            Self::Split { first, second, .. } => {
                first.collect_session_ids(out);
                second.collect_session_ids(out);
            }
        }
    }

    pub fn max_leaf_id(&self) -> u64 {
        match self {
            Self::Leaf { id, .. } => *id,
            Self::Split { first, second, .. } => first.max_leaf_id().max(second.max_leaf_id()),
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// The first leaf in layout order — the fallback pane a client focuses
    /// when it has no remembered focus of its own.
    pub fn first_leaf_id(&self) -> u64 {
        match self {
            Self::Leaf { id, .. } => *id,
            Self::Split { first, .. } => first.first_leaf_id(),
        }
    }

    pub fn session_for_leaf(&self, target: u64) -> Option<&str> {
        match self {
            Self::Leaf { id, session_id } if *id == target => session_id.as_deref(),
            Self::Leaf { .. } => None,
            Self::Split { first, second, .. } => first
                .session_for_leaf(target)
                .or_else(|| second.session_for_leaf(target)),
        }
    }

    /// Clear every pane showing `session_id`, leaving the pane in place but
    /// empty. Used by the daemon when a session is deleted: the *shape* of
    /// the layout is the user's, so a vanished session empties its pane
    /// rather than collapsing the split out from under them.
    pub fn clear_session(&mut self, session_id: &str) -> bool {
        match self {
            Self::Leaf {
                session_id: sid, ..
            } => {
                if sid.as_deref() == Some(session_id) {
                    *sid = None;
                    true
                } else {
                    false
                }
            }
            Self::Split { first, second, .. } => {
                // Not `||` — both halves must be visited, and `||`
                // short-circuits as soon as the first one matches.
                let a = first.clear_session(session_id);
                let b = second.clear_session(session_id);
                a || b
            }
        }
    }

    /// Clear panes whose session is not in `live`. Runs daemon-side on load
    /// and after deletion so every client sees the same repaired tree
    /// instead of each repairing it independently.
    pub fn retain_sessions<F>(&mut self, live: &F) -> bool
    where
        F: Fn(&str) -> bool,
    {
        match self {
            Self::Leaf { session_id, .. } => match session_id.as_deref() {
                Some(id) if !live(id) => {
                    *session_id = None;
                    true
                }
                _ => false,
            },
            Self::Split { first, second, .. } => {
                let a = first.retain_sessions(live);
                let b = second.retain_sessions(live);
                a || b
            }
        }
    }

    /// Clamp `ratio_percent` into 1..=99 and de-duplicate leaf ids, so a
    /// malformed or hand-edited tree can't wedge a client's renderer. Leaf
    /// ids must be unique because clients key per-pane state off them.
    pub fn normalize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut next = self.max_leaf_id().saturating_add(1);
        self.normalize_inner(&mut seen, &mut next);
    }

    fn normalize_inner(&mut self, seen: &mut std::collections::HashSet<u64>, next: &mut u64) {
        match self {
            Self::Leaf { id, .. } => {
                if !seen.insert(*id) {
                    *id = *next;
                    seen.insert(*next);
                    *next = next.saturating_add(1);
                }
            }
            Self::Split {
                ratio_percent,
                first,
                second,
                ..
            } => {
                *ratio_percent = (*ratio_percent).clamp(1, 99);
                first.normalize_inner(seen, next);
                second.normalize_inner(seen, next);
            }
        }
    }
}

/// The shared layout plus its version. `version` is bumped on every accepted
/// write and passed back as `base_version` to detect a stale writer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutDocument {
    pub tree: LayoutNode,
    pub version: u64,
}

impl Default for LayoutDocument {
    fn default() -> Self {
        Self {
            tree: LayoutNode::default(),
            version: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutSetParams {
    pub tree: LayoutNode,
    /// Version this write was composed against. `None` forces the write
    /// through — for a client that deliberately has no prior state, not as
    /// a way to win a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutStateNotificationPayload {
    pub layout: LayoutDocument,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStateNotificationPayload {
    pub project: ProjectSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeletedNotificationPayload {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
}

#[cfg(test)]
mod project_compat_tests {
    use super::*;

    #[test]
    fn project_params_accept_legacy_group_id_fields() {
        let p: SessionSetProjectParams = serde_json::from_value(serde_json::json!({
            "session_id": "s1",
            "group_id": "g1"
        }))
        .unwrap();
        assert_eq!(p.project_id.as_deref(), Some("g1"));

        let p: ProjectRenameParams = serde_json::from_value(serde_json::json!({
            "group_id": "g1",
            "name": "Project"
        }))
        .unwrap();
        assert_eq!(p.project_id, "g1");
    }

    #[test]
    fn project_results_use_project_id_on_the_wire() {
        let v = serde_json::to_value(ProjectCreateResult {
            project_id: "p1".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "project_id": "p1" }));

        let v = serde_json::to_value(ProjectDeletedNotificationPayload {
            project_id: "p1".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "project_id": "p1" }));
    }
}

#[cfg(test)]
mod playbook_block_tests {
    use super::*;

    #[test]
    fn block_id_is_stable_against_position_and_changes_with_content() {
        let a = playbook_block_id("- item");
        // Same content, different surrounding document position → same id.
        let doc1 = playbook_block_spans("- item\n\n- other\n");
        let doc2 = playbook_block_spans("- other\n\n- different\n\n- item\n");
        let id1 = &doc1.iter().find(|b| b.signature == "- item").unwrap().id;
        let id2 = &doc2.iter().find(|b| b.signature == "- item").unwrap().id;
        assert_eq!(&a, id1);
        assert_eq!(id1, id2, "id is position-independent");
        // Editing the block's text changes its id.
        assert_ne!(a, playbook_block_id("- item done"));
    }

    #[test]
    fn block_id_ignores_smart_clip_instance_ids() {
        let without_clip_id = playbook_block_spans("* task — @{session:s1}\n")
            .pop()
            .unwrap();
        let with_clip_id = playbook_block_spans("* task — @{session:s1 clip_id=clip_4}\n")
            .pop()
            .unwrap();
        let changed_clip_id = playbook_block_spans("* task — @{session:s1 clip_id=clip_9}\n")
            .pop()
            .unwrap();
        assert_eq!(without_clip_id.signature, "* task — @{session:s1}");
        assert_eq!(
            with_clip_id.signature,
            "* task — @{session:s1 clip_id=clip_4}"
        );
        assert_eq!(without_clip_id.id, with_clip_id.id);
        assert_eq!(with_clip_id.id, changed_clip_id.id);

        let changed_target = playbook_block_spans("* task — @{session:s2 clip_id=clip_4}\n")
            .pop()
            .unwrap();
        assert_ne!(
            with_clip_id.id, changed_target.id,
            "changing the smart-clip target is still semantic content"
        );
    }

    #[test]
    fn identical_blocks_share_one_id() {
        let spans = playbook_block_spans("- dup\n\n- dup\n");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].id, spans[1].id, "equal content → equal id");
    }

    #[test]
    fn spans_split_heading_and_items_into_separate_blocks() {
        // A heading glued to two items (no blank lines) splits into the heading
        // plus one block per item — so each card shimmers independently.
        let spans = playbook_block_spans("## In progress\n  * one\n* two\n\n## Done\n");
        assert_eq!(spans.len(), 4);
        assert_eq!((spans[0].start_line, spans[0].end_line), (0, 1));
        assert_eq!(spans[0].signature, "## In progress");
        // Signature trims each line; raw text keeps original indentation.
        assert_eq!((spans[1].start_line, spans[1].end_line), (1, 2));
        assert_eq!(spans[1].signature, "* one");
        assert_eq!(spans[1].text, "  * one");
        assert_eq!((spans[2].start_line, spans[2].end_line), (2, 3));
        assert_eq!(spans[2].signature, "* two");
        assert_eq!(spans[3].signature, "## Done");
        assert_eq!(spans[3].start_line, 4);
        // Distinct items have distinct ids.
        assert_ne!(spans[1].id, spans[2].id);
        // Empty / whitespace-only input has no blocks.
        assert!(playbook_block_spans("  \n\n").is_empty());
    }

    #[test]
    fn spans_keep_wrapped_continuation_with_its_item_and_paragraph() {
        // A wrapped continuation line stays with the item above it; a multi-line
        // paragraph stays whole; an ordered marker also starts an item.
        let spans = playbook_block_spans(
            "intro paragraph\nsecond line\n\n1. first step\n   wrapped detail\n2. second step\n",
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].signature, "intro paragraph\nsecond line");
        assert_eq!(spans[1].signature, "1. first step\nwrapped detail");
        assert_eq!(spans[2].signature, "2. second step");
    }

    #[test]
    fn scan_smart_clips_finds_type_target_and_span() {
        let text = "- do X @{harness:codex} and @{session:s1 clip_id=clip_2}";
        let clips = playbook_scan_smart_clips(text);
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].type_name, "harness");
        assert_eq!(clips[0].target, "codex");
        assert_eq!(&text[clips[0].start..clips[0].end], "@{harness:codex}");
        assert_eq!(clips[1].type_name, "session");
        assert_eq!(clips[1].target, "s1");
        assert_eq!(
            &text[clips[1].start..clips[1].end],
            "@{session:s1 clip_id=clip_2}"
        );
    }

    #[test]
    fn scan_smart_clips_empty_for_no_clips() {
        assert!(playbook_scan_smart_clips("- plain item, no clips here").is_empty());
    }

    #[test]
    fn list_item_text_strips_bullet_and_ordered_markers() {
        assert_eq!(playbook_list_item_text("- task"), Some("task"));
        assert_eq!(playbook_list_item_text("* task"), Some("task"));
        assert_eq!(playbook_list_item_text("+ task"), Some("task"));
        assert_eq!(playbook_list_item_text("12. task"), Some("task"));
        assert_eq!(playbook_list_item_text("3) task"), Some("task"));
        // Bare/empty bullets and non-list-item lines have no item text.
        assert_eq!(playbook_list_item_text("-"), None);
        assert_eq!(playbook_list_item_text("plain paragraph"), None);
    }

    #[test]
    fn normalize_tooltip_trims_collapses_and_truncates() {
        // Empty / whitespace-only → None (never stored as an empty label).
        assert_eq!(normalize_playbook_tooltip("   "), None);
        assert_eq!(normalize_playbook_tooltip(""), None);
        // Trims and collapses internal whitespace.
        assert_eq!(
            normalize_playbook_tooltip("  Building   the   PR \n"),
            Some("Building the PR".to_string())
        );
        // Exactly the max word count is kept verbatim.
        let ten = "one two three four five six seven eight nine ten";
        assert_eq!(normalize_playbook_tooltip(ten), Some(ten.to_string()));
        // Over the max is truncated to the first N words with an ellipsis.
        let eleven = "one two three four five six seven eight nine ten eleven";
        assert_eq!(
            normalize_playbook_tooltip(eleven),
            Some("one two three four five six seven eight nine ten…".to_string())
        );
    }

    fn run_progress_with_pending(pending: &[&str]) -> PlaybookRunProgress {
        PlaybookRunProgress {
            run_id: "r1".into(),
            started_at_ms: 10,
            expires_at_ms: 20,
            pending_block_ids: Vec::new(),
            pending_block_refs: pending.iter().map(|id| (*id).to_string()).collect(),
            pending_block_tooltips: HashMap::new(),
            system_status: None,
            seen_running: false,
            first_output_seen: false,
            queued_behind_current_turn: false,
            agent_managed: false,
            stage: PlaybookRunStage::Pressed,
            settled_block_count: 0,
            total_block_count: pending.len(),
        }
    }

    #[test]
    fn playbook_run_stage_derives_pipeline_progress() {
        let mut run = run_progress_with_pending(&["a", "b", "c"]);
        run.refresh_stage();
        assert_eq!(run.stage, PlaybookRunStage::Delivered);
        assert_eq!(run.settled_block_count, 0);
        assert_eq!(run.total_block_count, 3);

        run.first_output_seen = true;
        run.refresh_stage();
        assert_eq!(run.stage, PlaybookRunStage::FirstOutput);

        run.agent_managed = true;
        run.refresh_stage();
        assert_eq!(run.stage, PlaybookRunStage::PlanningPassDone);

        run.pending_block_refs = vec!["b".into(), "c".into()];
        run.refresh_stage();
        assert_eq!(run.stage, PlaybookRunStage::Settling);
        assert_eq!(run.settled_block_count, 1);
        assert_eq!(run.total_block_count, 3);
    }

    #[test]
    fn playbook_run_stage_defaults_total_for_legacy_payloads() {
        let mut run = run_progress_with_pending(&["a", "b"]);
        run.total_block_count = 0;
        run.refresh_stage();
        assert_eq!(run.total_block_count, 2);
        assert_eq!(run.settled_block_count, 0);
        assert_eq!(run.stage, PlaybookRunStage::Delivered);
    }
}

#[cfg(test)]
mod pty_activity_tests {
    use super::*;

    #[test]
    fn test_is_pty_active_payload() {
        // Purely synchronized updates + style resets -> inactive
        assert!(!is_pty_active_payload(
            b"\x1b[?2026h\x1b[39m\x1b[49m\x1b[59m\x1b[0m\x1b[?2026l"
        ));
        // Purely style resets -> inactive
        assert!(!is_pty_active_payload(b"\x1b[31m\x1b[0m"));
        // Empty -> inactive
        assert!(!is_pty_active_payload(b""));
        // Null bytes -> inactive
        assert!(!is_pty_active_payload(b"\0\0"));

        // Text character -> active
        assert!(is_pty_active_payload(b"a"));
        // Space -> active
        assert!(is_pty_active_payload(b" "));
        // Newline -> active
        assert!(is_pty_active_payload(b"\n"));
        // Cursor home -> active
        assert!(is_pty_active_payload(b"\x1b[H"));
        // Text after ignorable sequence -> active
        assert!(is_pty_active_payload(b"\x1b[?2026hhello\x1b[?2026l"));
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn parse_loose_accepts_fenced_json() {
        let raw = "```json\n{\"top\":\"run the tests\",\"verbs\":[{\"label\":\"dig deeper\",\"cards\":[\"why did X fail?\"]}]}\n```";
        let hand = SuggestionHand::parse_loose(raw).unwrap();
        assert_eq!(hand.top.text, "run the tests");
        assert_eq!(hand.verbs.len(), 1);
        assert_eq!(hand.verbs[0].label, "dig deeper");
        assert_eq!(hand.verbs[0].cards[0].text, "why did X fail?");
    }

    #[test]
    fn parse_loose_clamps_counts_and_lengths() {
        let long = "x".repeat(1000);
        let verbs: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    "{{\"label\":\"verb {i}\",\"cards\":[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\"]}}"
                )
            })
            .collect();
        let raw = format!("{{\"top\":\"{long}\",\"verbs\":[{}]}}", verbs.join(","));
        let hand = SuggestionHand::parse_loose(&raw).unwrap();
        assert_eq!(hand.top.text.chars().count(), 240);
        assert_eq!(hand.verbs.len(), 4);
        assert_eq!(hand.verbs[0].cards.len(), 4);
    }

    #[test]
    fn parse_loose_drops_empty_verbs_and_requires_top() {
        assert!(SuggestionHand::parse_loose("{\"top\":\"  \",\"verbs\":[]}").is_err());
        let hand = SuggestionHand::parse_loose(
            "{\"top\":\"ok\",\"verbs\":[{\"label\":\"\",\"cards\":[\"a\"]},{\"label\":\"real\",\"cards\":[]}]}",
        )
        .unwrap();
        assert!(hand.verbs.is_empty());
    }

    #[test]
    fn parse_loose_ignores_prose_around_json() {
        let raw =
            "Sure! Here's my prediction:\n{\"top\":\"merge it\",\"verbs\":[]}\nHope that helps.";
        let hand = SuggestionHand::parse_loose(raw).unwrap();
        assert_eq!(hand.top.text, "merge it");
    }

    #[test]
    fn suggestions_event_roundtrips() {
        let ev = SessionEvent::Suggestions(SuggestionHand {
            top: SuggestionCard {
                text: "run tests".into(),
            },
            verbs: vec![SuggestionVerb {
                label: "dig deeper".into(),
                cards: vec![SuggestionCard {
                    text: "why?".into(),
                }],
            }],
        });
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"suggestions\""));
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        match back {
            SessionEvent::Suggestions(h) => {
                assert_eq!(h.top.text, "run tests");
                assert_eq!(h.verbs[0].cards[0].text, "why?");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn suggest_params_keep_keyword_guidance_optional_for_existing_clients() {
        let params: SessionSuggestParams =
            serde_json::from_str(r#"{"session_id":"s1"}"#).unwrap();
        assert_eq!(params.session_id, "s1");
        assert!(params.keywords.is_none());

        let guided: SessionSuggestParams =
            serde_json::from_str(r#"{"session_id":"s1","keywords":"tests docs"}"#).unwrap();
        assert_eq!(guided.keywords.as_deref(), Some("tests docs"));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn leaf(id: u64, session: Option<&str>) -> LayoutNode {
        LayoutNode::Leaf {
            id,
            session_id: session.map(str::to_string),
        }
    }

    fn split(first: LayoutNode, second: LayoutNode, ratio: u16) -> LayoutNode {
        LayoutNode::Split {
            direction: LayoutSplitDirection::Right,
            ratio_percent: ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn tree_round_trips_through_json() {
        let tree = split(leaf(1, Some("s1")), leaf(2, None), 60);
        let json = serde_json::to_string(&tree).unwrap();
        assert!(json.contains("\"kind\":\"split\""));
        assert!(json.contains("\"direction\":\"right\""));
        // An empty pane omits the key entirely rather than sending null.
        assert!(!json.contains("null"));
        let back: LayoutNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn leaf_order_is_layout_order() {
        let tree = split(leaf(7, None), split(leaf(3, None), leaf(9, None), 50), 50);
        assert_eq!(tree.leaf_ids(), vec![7, 3, 9]);
        assert_eq!(tree.first_leaf_id(), 7);
        assert_eq!(tree.leaf_count(), 3);
        assert_eq!(tree.max_leaf_id(), 9);
    }

    #[test]
    fn session_ids_skip_empty_panes() {
        let tree = split(
            leaf(1, Some("a")),
            split(leaf(2, None), leaf(3, Some("b")), 50),
            50,
        );
        assert_eq!(tree.session_ids(), vec!["a", "b"]);
    }

    #[test]
    fn clearing_a_session_empties_every_pane_showing_it() {
        // Both halves must be visited: a short-circuiting `||` would leave
        // the second copy of the session behind.
        let mut tree = split(leaf(1, Some("dup")), leaf(2, Some("dup")), 50);
        assert!(tree.clear_session("dup"));
        assert_eq!(tree.session_ids(), Vec::<&str>::new());
        assert!(!tree.clear_session("dup"));
    }

    #[test]
    fn pruning_empties_dead_panes_but_keeps_the_shape() {
        let mut tree = split(leaf(1, Some("live")), leaf(2, Some("dead")), 70);
        let changed = tree.retain_sessions(&|id: &str| id == "live");
        assert!(changed);
        assert_eq!(tree.leaf_count(), 2, "shape is the user's, not ours");
        assert_eq!(tree.session_ids(), vec!["live"]);
        assert_eq!(tree.session_for_leaf(2), None);
    }

    #[test]
    fn normalize_clamps_ratios_and_dedupes_leaf_ids() {
        let mut tree = split(leaf(1, Some("a")), leaf(1, Some("b")), 0);
        tree.normalize();
        match &tree {
            LayoutNode::Split { ratio_percent, .. } => assert_eq!(*ratio_percent, 1),
            _ => panic!("expected a split"),
        }
        let ids = tree.leaf_ids();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "clients key per-pane state off leaf ids");
    }

    #[test]
    fn a_bare_leaf_is_the_default_layout() {
        let doc = LayoutDocument::default();
        assert_eq!(doc.version, 0);
        assert_eq!(doc.tree.leaf_count(), 1);
        assert_eq!(doc.tree.session_ids(), Vec::<&str>::new());
    }
}

#[cfg(test)]
mod auto_title_tests {
    use super::sanitize_auto_title;

    #[test]
    fn sanitize_strips_quotes_and_markdown() {
        assert_eq!(
            sanitize_auto_title("\"Refactor Adapter Spawning\""),
            "Refactor Adapter Spawning"
        );
        assert_eq!(sanitize_auto_title("`Add Pty Logging`"), "Add Pty Logging");
        assert_eq!(
            sanitize_auto_title("**Plan The Refactor**"),
            "Plan The Refactor"
        );
    }

    #[test]
    fn sanitize_first_line_only() {
        assert_eq!(
            sanitize_auto_title("Title Here\nextra explanation"),
            "Title Here"
        );
    }

    #[test]
    fn sanitize_caps_length() {
        let huge = "Word ".repeat(50);
        let out = sanitize_auto_title(&huge);
        assert!(out.len() <= 48);
    }
}

#[cfg(test)]
mod cost_model_tests {
    use super::*;

    /// A transcript written before `Cost` carried a model must still load —
    /// the field is additive and absence means "harness never said" (spec
    /// 0167), never a parse failure.
    #[test]
    fn legacy_cost_without_model_deserializes() {
        let legacy =
            r#"{"type":"cost","usd":0.5,"tokens_in":100,"tokens_out":20,"tokens_cached":80}"#;
        match serde_json::from_str::<SessionEvent>(legacy).expect("legacy cost parses") {
            SessionEvent::Cost {
                usd,
                tokens_in,
                tokens_out,
                tokens_cached,
                model,
            } => {
                assert!((usd - 0.5).abs() < f64::EPSILON);
                assert_eq!(tokens_in, 100);
                assert_eq!(tokens_out, 20);
                assert_eq!(tokens_cached, 80);
                assert_eq!(model, None);
            }
            other => panic!("expected Cost, got {other:?}"),
        }
    }

    /// An absent model stays absent on the wire, so records written by this
    /// version keep the same shape older readers already accept.
    #[test]
    fn absent_model_is_omitted_when_serialized() {
        let event = SessionEvent::Cost {
            usd: 0.0,
            tokens_in: 1,
            tokens_out: 2,
            tokens_cached: 0,
            model: None,
        };
        let json = serde_json::to_string(&event).expect("serializes");
        assert!(!json.contains("model"), "{json}");
    }

    #[test]
    fn model_roundtrips_when_stated() {
        let event = SessionEvent::Cost {
            usd: 0.0,
            tokens_in: 1,
            tokens_out: 2,
            tokens_cached: 0,
            model: Some("claude-opus-5".into()),
        };
        let json = serde_json::to_string(&event).expect("serializes");
        match serde_json::from_str::<SessionEvent>(&json).expect("parses") {
            SessionEvent::Cost { model, .. } => {
                assert_eq!(model.as_deref(), Some("claude-opus-5"))
            }
            other => panic!("expected Cost, got {other:?}"),
        }
    }
}
