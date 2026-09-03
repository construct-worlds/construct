# smith built-in agent

`smith` is the built-in agent that ships with construct. It talks to OpenAI,
Anthropic, Google Gemini, Meta Model API, xAI Grok, DeepSeek, OpenRouter, or a
local Ollama directly, and can also draw on your Codex, Claude Code, Grok, or
Kimi Code subscription. Smith runs its own agent loop with shell + patch editing
+ browser automation + construct-control tools. Many PRs for the construct
repository have already been made from smith sessions running inside construct.

### Quick start

```sh
# Pick a provider — only one of these needs to be set:
export ANTHROPIC_API_KEY=sk-ant-...
# or  export OPENAI_API_KEY=sk-...
# or  export GEMINI_API_KEY=...        # (or GOOGLE_API_KEY)
# or  export META_API_KEY=...          # (or MODEL_API_KEY)
# or  export GROK_API_KEY=...          # (or XAI_API_KEY)
# or  export DEEPSEEK_API_KEY=...
# or  export OPENROUTER_API_KEY=sk-or-...
# or  codex login, then use --model codex-oauth:gpt-5.4-mini
# or  claude login, then use --model claude-oauth:sonnet
# or  grok login, then use --model grok-oauth:grok-4.6
# or  kimi login, then use --model kimi-oauth:k3
# or  run a local ollama (default http://localhost:11434)

construct new --prompt "list the rust files in this repo and summarize what each crate does" smith
```

### Model selection

Pass `--model <spec>` on `construct new` (or set `CONSTRUCT_SMITH_MODEL`).
The spec is one of:

- `openai:<name>` — e.g. `openai:gpt-5-mini`
- `anthropic:<name>` — e.g. `anthropic:claude-haiku-4-5`
- `claude-oauth:<name>` — e.g. `claude-oauth:sonnet` (alias: `claude-code-oauth:`)
- `gemini:<name>` — e.g. `gemini:gemini-2.5-pro`
- `meta:<name>` — e.g. `meta:muse-spark-1.1` using `META_API_KEY` or
  `MODEL_API_KEY`
- `grok:<name>` — e.g. `grok:grok-4.6` using `GROK_API_KEY` or `XAI_API_KEY`
- `deepseek:<name>` — e.g. `deepseek:deepseek-v4-pro` using `DEEPSEEK_API_KEY`
- `openrouter:<name>` — e.g. `openrouter:openrouter/auto` using `OPENROUTER_API_KEY`
- `grok-oauth:<name>` — e.g. `grok-oauth:grok-4.6` using the Grok CLI auth file
- `kimi-oauth:<name>` — e.g. `kimi-oauth:k3` using the Kimi Code CLI login
- `ollama:<name>` — e.g. `ollama:llama3.1`
- `codex-oauth:<name>` — e.g. `codex-oauth:gpt-5.4-mini`
- `@<name>` — a named endpoint profile (see [Model profiles](#model-profiles)),
  e.g. `@work-gateway` or `@work-gateway:deepseek-v4-flash` to override its model

Bare names auto-detect: `gpt-*` / `o[1-5]*` → OpenAI, `claude-*` →
Anthropic, `gemini-*` → Gemini, `grok*` → Grok, `deepseek*` → DeepSeek,
anything else → Ollama.
Use the explicit `meta:` prefix for Muse Spark; a bare `muse-spark-1.1`
continues to mean an Ollama model. When in doubt, use the explicit prefix.

`openrouter:` calls the OpenRouter aggregator (`openrouter.ai/api/v1`) using
`OPENROUTER_API_KEY`. Model IDs follow OpenRouter's `vendor/model` format (e.g.
`openrouter:openrouter/auto` or `openrouter:stealth/ox-alpha`). Because slash
IDs overlap with Ollama paths and routing through an aggregator is an explicit
billing choice (spec 0028), bare names never route to OpenRouter.

`meta:` calls Meta's Responses API directly. Smith streams assistant text,
supports parallel function calls and tool-result replay, and sends
`store: false` because construct persists and replays the conversation locally.

`codex-oauth:` uses your Codex CLI subscription login: run `codex login`
once so the credentials are stored, and smith reads them from
`$CODEX_HOME/auth.json` (falling back to `~/.codex/auth.json`) and calls
the OpenAI-compatible API directly with the subscription OAuth token. The
`codex` CLI does not need to stay on `PATH` at runtime. Smith passes its own
tools natively, so construct's normal tool approvals and transcript persistence
apply. This path uses your subscription, not `OPENAI_API_KEY` (that's the
separate `openai:` path).

`claude-oauth:` (or `claude-code-oauth:`) uses your Claude Code subscription login:
run `claude login` once, and smith reads credentials from the macOS Keychain
(`Claude Code-credentials`) or `~/.claude/.credentials.json` (overridable via
`CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS`), calling Anthropic's Messages API directly
with OAuth bearer tokens and native tools.

`grok-oauth:` uses the same OpenAI-compatible xAI API endpoint as `grok:`, but
loads a bearer token from the Grok CLI auth file instead of `GROK_API_KEY` /
`XAI_API_KEY`. Run `grok login` first. Smith reads
`$GROK_HOME/.grok/auth.json` when `GROK_HOME` is set, otherwise
`~/.grok/auth.json`, and chooses the newest unexpired `key` entry.

`kimi-oauth:` uses your Kimi Code subscription login: run `kimi login` once,
and smith reads `$KIMI_CODE_HOME/credentials/kimi-code.json` (default
`~/.kimi-code/credentials/kimi-code.json`) and calls Moonshot's
Anthropic-compatible coding backend directly with the OAuth bearer token.
Kimi access tokens are short-lived, so smith refreshes them through Kimi's
own token endpoint and writes the rotated tokens back to the same file the
CLI uses. Models: `k3`, `k3-256k`, `kimi-for-coding`,
`kimi-for-coding-highspeed`.

If you don't pass a model and `CONSTRUCT_SMITH_MODEL` isn't set, smith
picks: `ANTHROPIC_API_KEY` → `claude-opus-4-8`, else `OPENAI_API_KEY`
→ `gpt-5`, else `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) →
`gemini-2.5-pro`, else `META_API_KEY` (or `MODEL_API_KEY`) →
`muse-spark-1.1`, else `DEEPSEEK_API_KEY` → `deepseek-v4-pro`,
else `OPENROUTER_API_KEY` → `openrouter/auto`,
else **smith fails to start** with an error explaining
what's missing. The initial Status event records the chosen `provider:model`
so you can verify.

Earlier versions fell through to `ollama:llama3.1` here unconditionally, so a
machine with no Ollama server running got a session that looked healthy and
then died mid-turn with a raw transport error instead of failing loudly at
start. OAuth subscriptions (`claude-oauth:`, `codex-oauth:`, `grok-oauth:`,
`kimi-oauth:`) and Ollama are still fully supported — pass one of the explicit prefixes
above (or `CONSTRUCT_SMITH_MODEL`) rather than relying on auto-detect to
guess them. In the construct TUI, run `/configure` (or `M-x configure`) to
see every auth method smith supports, its live-detected status, and — when
you pick one — persist it as smith's default via `CONSTRUCT_SMITH_MODEL`.

### Model profiles

The base-URL env vars below bind one endpoint per wire protocol. To use
**several** endpoints of the same protocol in one session — e.g. first-party
OpenAI plus two OpenAI-compatible vendors — declare named profiles in
`config.toml` and switch between them at runtime with `/model @<name>`.

Each `[smith.models.<name>]` entry sets:

- `provider` — wire protocol to speak: `openai`, `anthropic`, `gemini`,
  `meta`, `grok`, `deepseek`, `openrouter`, or `ollama`. (OAuth providers can't be profiled
  — use their prefixes directly.)
- `base_url` — endpoint URL (defaults to the protocol's public endpoint).
- `api_key_env` — name of the env var holding the key (preferred). Or
  `api_key = "..."` inline (discouraged). If neither is set, the protocol's
  standard key env var is used (`OPENAI_API_KEY`, etc.).
- `model` — default model name; override per call with `@<name>:<model>`.

None of the direct-API-key providers needs a profile — the key plus the
`<provider>:` prefix already reaches its public endpoint, and the same key
makes it a route target for other harnesses (spec 0179). Declare one only
for an endpoint Construct can't know: a private gateway, a reseller, a
second account.

```toml
[smith.models.work-gateway]
provider    = "deepseek"
base_url    = "https://deepseek.internal/v1"
api_key_env = "WORK_DEEPSEEK_KEY"
model       = "deepseek-v4-pro"

[smith.models.groq-llama]
provider    = "openai"
base_url    = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model       = "llama-3.3-70b-versatile"

[smith.models.xai]
provider    = "grok"
api_key_env = "XAI_API_KEY"
model       = "grok-4.6"

[smith.models.meta]
provider    = "meta"
api_key_env = "META_API_KEY"
model       = "muse-spark-1.1"
```

```text
construct new --model @work-gateway --prompt "..." smith  # start on a profile
/model openai:gpt-5                            # first-party OpenAI
/model deepseek:deepseek-v4-pro                # DeepSeek's public endpoint
/model @work-gateway                           # DeepSeek via a private gateway
/model @groq-llama:llama-3.1-8b-instant        # Groq, one-off model override
/model                                         # shows current + lists @profiles
```

Profiles are always referenced with the explicit `@` prefix; bare names never
resolve to a profile. The status line shows `@<name>:<model>` so you can tell
which endpoint is active.

### Tools

Smith registers three tool sets: local development tools, Chrome DevTools browser automation, and daemon/fleet control tools (including subagents).

#### Local coding tools

Smith provides a minimal, Codex-style tool surface rather than separate read/list/find primitives:

- `shell`: run commands in a shell (`command`, optional `timeout_secs`, `interactive`, `read_only`). File reads, search, listing, and repo inspection go through standard tools (`cat`, `rg`, `ls`, `git`, `sed -n`) instead of dedicated file-reading tools.
- `write_stdin`: write text lines or send EOF to the standard input of an interactive process spawned by `shell` (addressed by `pid`).
- `edit_file`: atomic find-and-replace editor. Supports single edit (`path`, `find`, `replace` with unique match required) or multi-hunk / multi-file batch edits (`edits: [{path?, find, replace}]`). Creating a new file uses an empty `find` and non-existent `path` with contents in `replace`. All hunks are validated first and applied atomically; if any match fails or matches more than once, nothing is written.

#### Browser tools

Native tools drive Chrome through DevTools remote debugging and emit the browser preview thumbnail that the TUI renders above the session:

- `browser_open`: open a URL in Chrome (starts Chrome with remote debugging on port 9222 if needed) and emit a preview overlay.
- `browser_inspect`: list open tabs (with no args) or inspect tab content (title, URL, visible body text, links) selected by `tab_id` or `url_contains`.
- `browser_screenshot`: capture a screenshot (viewport or `full_page`) of a Chrome tab and emit preview overlay.
- `browser_eval`: evaluate JavaScript / async expressions in a Chrome tab for browser automation and DOM extraction.

These tools are native to smith and are also exposed through construct's MCP server for other harnesses.

#### Fleet & construct-control tools

Unless withheld via `CONSTRUCT_SMITH_FLEET_TOOLS=off`, smith sessions have full read and write access to other sessions on the daemon and can orchestrate child subagents (specs 0089, 0171):

- **Session inspection (Safe)**: `agentd_context` (structured memory, widgets, environment, playbook, system reference), `agentd_whoami` (returns the calling session ID), `agentd_list_sessions` (lists every session with state, cwd, harness, approval mode, and activity timestamps), `agentd_get_session` (full summary and structured transcript), `agentd_get_transcript` (event log slice by `from` sequence and `limit`), `agentd_get_output` (recent PTY scrollback text), `agentd_get_diff` (`git diff HEAD` for a session's worktree), `agentd_list_harnesses` (lists available harnesses).
- **Session control (Risky)**: `agentd_create_session` (spawn a session on any harness), `agentd_send_input` (send input line), `agentd_send_keys` (send raw base64 bytes for control/arrow keys), `agentd_interrupt_session` (send `C-c`), `agentd_stop_session` (graceful stop), `agentd_kill_session` (SIGKILL adapter), `agentd_delete_session` (drop transcript and worktree), `agentd_pin_session` (toggle pin strip status), `agentd_rename_session` (set/clear session title), `agentd_set_session_group` (group assignment and position), `agentd_move_session` (reorder session up/down).
- **Recurring loops (scheduler)**: `agentd_loop_create` (Risky; recurring prompt injected at interval), `agentd_loop_list` (Safe; list loops and next fire times), `agentd_loop_update` (Risky; change interval, prompt, or expiry), `agentd_loop_remove` (Risky; stop recurring loop).
- **Playbook**: `agentd_playbook_get` (Safe; read playbook markdown, version, and shimmer state), `agentd_playbook_edit` (Risky; apply anchored find/replace edits).
- **Subagents**: Smith-owned child backing sessions nested under the parent session: `agentd_subagent_create` (Risky; spawn subagent), `agentd_subagent_list` (Safe; list owned subagents and summaries), `agentd_subagent_peek` (Safe; inspect scrollback or structured event tail), `agentd_subagent_enqueue` (Risky; send follow-up prompt), `agentd_subagent_cancel` (Risky; interrupt subagent), `agentd_subagent_delete` (Risky; remove subagent).

### Approval modes and transitions

Tool calls run with your permissions. Smith classifies each tool call by risk:

- **Safe** (runs immediately without prompting, fans out concurrently in the agent loop):
  - Browser inspection: `browser_inspect`, `browser_screenshot`.
  - Fleet and daemon reads: `agentd_context`, `agentd_whoami`, `agentd_list_sessions`, `agentd_get_session`, `agentd_get_transcript`, `agentd_get_output`, `agentd_get_diff`, `agentd_list_harnesses`, `agentd_loop_list`, `agentd_playbook_get`, `agentd_subagent_list`, `agentd_subagent_peek`.
  - **Dynamic downgrades**:
    - A `shell` call explicitly marked `read_only: true` (and `interactive: false`/omitted) is downgraded to Safe, allowing read commands (e.g. search, inspection) to run concurrently without a gate.
    - An `edit_file` call whose targets all fall inside paths allowed by the auto-approval policy (such as session widget directories configured via `CONSTRUCT_AUTO_APPROVE_PATHS`) is downgraded to Safe.
- **Risky** (governed by the session's approval mode):
  - Local/browser mutations: `shell` (mutating/default), `write_stdin`, `edit_file` (outside auto-approved paths), `browser_open`, `browser_eval`.
  - Fleet mutations: all session-modifying `agentd_*` tools, loop creation/updates/removal, playbook edits, and subagent creation/lifecycle tools.

#### The three approval modes

Smith implements three explicit per-session approval modes (spec 0015):

1. **`manual` (default)**: Safe tools run immediately. Risky tools pause and display an inline approval prompt in the session. The modeline badge displays `[manual]`.
2. **`auto_review`**: Safe tools run immediately. Risky tools are first evaluated by an automated reviewer prompt (or fast-path approved if recognized as a routine development shell command).
   - If the reviewer approves the action as routine, bounded, and task-relevant development work inside the git worktree, the tool runs automatically.
   - If the action is ambiguous, broad, touches secrets, mutates outside the worktree, or the reviewer is uncertain, it defers to the user (`ask_user`).
   - The reviewer *never* denies on its own — only a human makes the final rejection.
   - The modeline badge displays `[auto-review]`.
3. **`always-approve`** (`unsafe_auto` on wire): Both Safe and Risky tools run automatically without asking the user. The modeline badge displays `[always-approve]`.

#### Approval prompt actions

When a Risky tool requires human decision in `manual` mode (or when `auto_review` defers):

- `y` / Enter: **Approve** the current call.
- `n` / Esc / `C-g` / `C-c`: **Deny** the current call. Returns a synthetic "user denied" error to the model so it can adapt its approach.
- `a` / `A`: Switch the session to **`auto_review`** mode and vet this pending call immediately. If the reviewer approves, execution continues; if uncertain, it prompts again.
- `f` / `F`: Switch the session to **`always-approve`** (`unsafe_auto`), approving the current call and running all future calls without prompts.

When an approval prompt changes the approval mode, Smith emits an `ApprovalModeChanged` event so the daemon and all connected clients update immediately.

#### Mode cycling and automode behavior

- **Cycle modes anytime**: Press `C-x A` (Emacs) or `A` (Vim normal mode), or click the `[manual]` / `[auto-review]` / `[always-approve]` badge on the TUI modeline. Cycling follows the order: `manual` → `auto-review` (`auto_review`) → `always-approve` (`unsafe_auto`) → `manual`.
- **Initial automode override**: Set `CONSTRUCT_SMITH_AUTOMODE=1` in the environment to start the session directly in `always-approve` (`unsafe_auto`) mode rather than `manual` (ideal for non-interactive or batch runs).

### Long output handling & context management

The full tool output goes to the transcript (you see everything). The
agent's context only gets a truncated head + `[… N bytes elided …]` + tail
(8 KiB budget per call), so large command outputs don't overwhelm the context
window.

Smith automatically manages context budget through two layers:

- **Auto-compaction**: When estimated tokens reach 65% of the model's effective
  context window (`AUTO_COMPACT_RATIO = 0.65`), Smith requests a structured
  summary of older history and prepends a `[Compacted earlier context]` turn,
  preserving recent turn pairs verbatim (`DEFAULT_KEEP_PAIRS = 4`). You can also
  trigger manual compaction anytime with `/compact [N]`. Auto-compaction is enabled
  by default; disable with `CONSTRUCT_SMITH_AUTO_COMPACT=off` (or `0`/`false`).
- **Rolling prune**: If context exceeds 70% utilization (`UTILIZATION = 0.70`),
  the oldest turn pairs are pruned, always keeping at least the two most-recent
  turn pairs. Smith learns and persists runtime limits when providers report
  overflow errors (spec 0070).

### Ambient features that use smith

Beyond running its own sessions, smith powers several daemon-level
conveniences. If none of the credentials above exist, these degrade — and
the daemon tells you how (spec 0151):

- **Session auto-naming** — every new session's title is generated from its
  first prompt via a cheap smith one-shot. Without a smith credential,
  sessions on model harnesses (claude, codex, …) fall back to a hidden
  one-shot on their *own* harness; smith and shell sessions keep their
  default hash names.
- **Next-prompt suggestions** — smith and shell sessions generate
  suggestions via smith; other harnesses always use their own model.
- **The minibuffer session** — the bottom input strip is a smith session by
  default. Without a credential it still handles slash commands but cannot
  act as an agent (spec 0071).

Inspect the live status with `construct harnesses` (the `ambient features:`
block) or the `/configure` dialog's **Features** tab, which maps each
feature to its cause and fix. The first time a feature actually skips work
for lack of a credential, the TUI shows a clickable `smith: no credential`
notice in the status bar that opens `/configure`.

### Opt-out / customization

- `CONSTRUCT_SMITH_AUTOMODE=1` — start the session in `always-approve`
  (`unsafe_auto`) mode instead of `manual`.
- `CONSTRUCT_SMITH_MODEL=<spec>` — default model when `--model` is
  omitted.
- `CONSTRUCT_SMITH_FLEET_TOOLS=off` — withhold tools that reach outside the
  session (daemon-control and subagents), exposing only local coding and
  browser tools.
- `CONSTRUCT_SMITH_AUTO_COMPACT=off` — disable auto-compaction before rolling
  prune.
- `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` (or `GOOGLE_API_KEY`),
  `META_API_KEY` (or `MODEL_API_KEY`), `GROK_API_KEY` (or `XAI_API_KEY`),
  `DEEPSEEK_API_KEY`, `OPENROUTER_API_KEY` — API keys for each supported
  direct provider.
  Each key set in the daemon's environment also makes its provider a **route target**
  for other harnesses with no further config (spec 0179) — see
  [Model routing](model-routing.md#route-targets).
- `CODEX_HOME` — override the base directory used for `codex-oauth:` auth lookup
  (reads `$CODEX_HOME/auth.json` instead of `~/.codex/auth.json`).
- `CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS` — override the credentials file path used
  by `claude-oauth:` (default reads macOS Keychain item `Claude Code-credentials`
  or `~/.claude/.credentials.json`). Note that `CONSTRUCT_CLAUDE_BIN` /
  `CONSTRUCT_CLAUDE_CMD` configure the standalone `claude` CLI adapter harness,
  not Smith's direct `claude-oauth:` provider.
- `GROK_HOME` — override the base directory used by `grok-oauth:` token lookup;
  Smith reads `$GROK_HOME/.grok/auth.json` instead of `~/.grok/auth.json`.
- `KIMI_CODE_HOME` — override the base directory used by `kimi-oauth:`
  credential lookup (default `~/.kimi-code`);
  `CONSTRUCT_KIMI_OAUTH_CREDENTIALS` points at an exact credentials file.
- `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` / `GEMINI_BASE_URL` /
  `META_BASE_URL` / `OLLAMA_HOST` — point at alternate endpoints. Pointing
  `OPENAI_BASE_URL` at an OpenAI-compatible vendor (OpenRouter, DeepSeek, Groq, xAI,
  Mistral, …) reuses the `openai:` path with no extra config. These bind
  one endpoint per protocol; to switch between several at runtime, use
  [Model profiles](#model-profiles) instead.
