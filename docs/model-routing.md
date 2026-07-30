# Model routing

A session runs on one harness — Claude Code, Codex, Grok, … — but its model
traffic doesn't have to go where that harness would send it. Construct's
daemon carries a per-session proxy that can serve a session's model requests
from a different provider: run Claude Code on a Kimi subscription, let a
Codex subagent answer from Claude, move a session off a provider mid-outage
without restarting it.

There are two ways to use it, and they compose:

- **Redirect** — you point the session's model traffic somewhere else.
  Transparent to the harness; it keeps believing it talks to its own model.
- **Native picker entries (gateway)** — Construct publishes its route
  targets *into the harness's own model picker*, so the harness (and its
  native subagents) can pick them itself.

Both are on by default. `[router] enabled = false` opts out of everything;
`publish_models = false` keeps redirects but stops augmenting native
pickers.

## Route targets

A target is somewhere the router can send a model request:

- **Subscription logins already on this machine** (Claude, ChatGPT/Codex,
  Grok, Kimi) are offered automatically — nothing to declare. The router
  reads those credentials from the owning CLI's store and never refreshes
  them; an expired login is reported with the command to renew it.
- **Declared endpoints** are the `[smith.models.*]` profiles in
  `config.toml` — declare an endpoint once and it is reachable from both
  smith and a routed session:

  ```toml
  [smith.models.kimi]
  provider = "openai"            # wire dialect
  base_url = "https://api.moonshot.ai/v1"
  api_key_env = "MOONSHOT_API_KEY"
  model = "kimi-k2.5"
  ```

When the target speaks a different wire dialect than the harness, the
router translates through a canonical form: Anthropic Messages, OpenAI
Chat Completions, OpenAI Responses (including Azure), and Google Gemini
are supported. Targets with no translator are still listed in the picker,
with the reason they can't be selected.

A target appears only when it is actually usable — a credential the router
can read, or a configured endpoint with its key present. A fresh machine
with no logins and no profiles has nothing to offer, so pickers stay
native-only until a login or profile exists.

### Summary of route targets

| Target | Type | Wire Dialect | Backend Endpoint | Credential Source |
| :--- | :--- | :--- | :--- | :--- |
| `claude-oauth` | Subscription Login | Anthropic Messages | `https://api.anthropic.com/v1/messages` | Auto-discovered from Claude CLI store (read-only token) |
| `codex-oauth` | Subscription Login | OpenAI Responses | `https://chatgpt.com/backend-api/codex/responses` | Auto-discovered from Codex CLI store (read-only token) |
| `grok-oauth` | Subscription Login | OpenAI Chat Completions | `https://api.x.ai/v1/chat/completions` | Auto-discovered from Grok CLI store (read-only token) |
| `kimi-oauth` | Subscription Login | Anthropic Messages | `https://api.kimi.com/coding/v1/messages` | Auto-discovered from Kimi CLI store (read-only token) |
| `[smith.models.<name>]` | Declared Endpoint | Configured (`openai`, `anthropic`, `responses`, `gemini`, `azure`) | Configured `base_url` | Declared in `config.toml` (`api_key_env` / `api_key`) |

*Note: Antigravity OAuth logins are not offered as route targets because their backend uses a Gemini-shaped protocol with no proxy translator.*

## Redirect

Click the model name in the modeline (the status bar above the input) to
open the **model redirect** menu: pick a target, then one of its models.
The top row, **No redirect**, is the off switch and always works.

Semantics:

- **Transparent to the harness.** The harness is not restarted and never
  learns about the substitution. Its own context accounting and displayed
  model reflect its belief; Construct's modeline shows the truth as
  `harness-model → routed-model`.
- **Applies from the next request.** A request already in flight is never
  re-aimed. Connections made stale by a redirect change are closed between
  turns so the harness reconnects and picks up the new disposition.
- **Durable.** A redirect survives session restarts and daemon restarts —
  a resumed session comes back redirected to where you left it
  (spec 0114).
- **Scoped to native-addressed requests.** A redirect applies to requests
  addressed to the harness's own model. A request that explicitly names a
  Construct catalog entry (next section) goes where it is addressed —
  exactly like mail forwarding, which forwards what's addressed to the old
  address and doesn't touch mail addressed directly to the new one.

Only probe-verified harnesses can be redirected (spec 0115): a harness is
route-capable when a probe has shown Construct's proxy injection actually
reaches it, never because documentation says it should. Sessions started
before routing was enabled can't be redirected; the menu says so instead
of showing an empty list. An armed redirect that has not yet served a
request is reported as unproven ("routing inert") rather than silently
claimed to be working.

## Native picker entries (the gateway)

For supported harnesses, `construct new <harness>` also publishes every
usable target into the harness's *own* model picker, so model choice can
stay in the tool you're already using:

- **Codex** gets a session-local generated model catalog. Routed entries
  appear in `/model` and in the native subagent scheduler's model list as
  `<model> · <route>`. The generated catalog pins the session to Codex's
  v1 multi-agent surface (v2 may encrypt child tasks for the ChatGPT
  backend, which a routed provider cannot read).
- **Claude Code** gets a loopback, session-scoped Anthropic-compatible
  gateway, and Claude's native gateway model discovery is enabled for that
  child process only. Routes appear in `/model` as
  `<model> · <route> · Construct` gateway entries. An existing
  `ANTHROPIC_BASE_URL` you configured is left untouched, and a claude.ai
  login stays authoritative.

Each published entry carries a stable id in Construct's namespace
(`construct-<route>/<model>`, or `claude-construct-…` for Claude's id
filter). The harness sends that id on every request that uses the entry —
including requests made by its native subagents — and the proxy resolves
it per request. That is what makes the selection *request-scoped*: a Codex
parent can stay on its native model while one subagent answers from Kimi
and another from Claude, concurrently.

Construct's own surfaces never show the raw encoded id: the modeline,
session list, and web UI display it decoded as `model · route`
(spec 0158).

Publication is session-local and never edits the harness's persistent
configuration. Turning it off (`publish_models = false`) affects new
sessions only.

## Harness routing support

The table below summarizes router capability across all supported harness adapters: whether Construct can redirect model traffic (transparent proxy interception) and whether Construct publishes route targets into the harness's native model picker.

| Harness | Adapter | Redirect Support | Native Picker Integration | Intercept Hosts & Trust Channel | Integration Details |
| :--- | :--- | :---: | :---: | :--- | :--- |
| **Claude Code** | `claude` | ✅ Yes | ✅ Yes | `api.anthropic.com`<br>(`NODE_EXTRA_CA_CERTS`, additive) | Publishes Anthropic loopback gateway; entries appear in `/model` as `<model> · <route> · Construct`. |
| **Codex** | `codex` | ✅ Yes | ✅ Yes | `chatgpt.com`, `api.openai.com`<br>(`SSL_CERT_FILE`, replacing bundle) | Publishes session-local model catalog; entries appear in `/model` and native subagent scheduler as `<model> · <route>`. |
| **Grok** | `grok` | ✅ Yes | ❌ No | `cli-chat-proxy.grok.com`<br>(`SSL_CERT_FILE`, additive) | Probe-verified interception of native Grok CLI traffic when redirected. |
| **Pi** | `pi` | ✅ Yes | ❌ No | `chatgpt.com`<br>(`NODE_EXTRA_CA_CERTS`, additive) | Probe-verified interception of native Pi CLI traffic when redirected. |
| **Hermes** | `hermes` | ✅ Yes | ❌ No | `inference-api.nousresearch.com`<br>(`SSL_CERT_FILE`, replacing bundle) | Probe-verified interception of native Hermes CLI traffic when redirected. |
| **OpenCode** | `opencode` | ❌ No | ❌ No | Pass-through only | Endpoint host varies per user configuration (no fixed intercept host). |
| **Kimi** | `kimi` | ❌ No | ❌ No | None | No proxy routing probe or native picker catalog injection. |
| **Antigravity** | `antigravity` | ❌ No | ❌ No | None | Backend uses Gemini-shaped protocol with no proxy translator. |
| **Smith** | `smith` | N/A | N/A | Direct provider calls | Built-in multi-provider harness; selects models directly via `config.toml` profiles or `/model`. |
| **Shell** | `shell` | N/A | N/A | N/A | Non-LLM interactive terminal session. |

## Precedence

For each request the proxy sees, in order:

1. A valid Construct catalog id selects its encoded route and model — the
   redirect is not consulted.
2. A native model id follows the session's redirect, if one is armed.
3. Otherwise the request goes to the exact origin the harness named, with
   its native credential intact.
4. A malformed or no-longer-published id in Construct's namespace fails
   closed: it is never leaked to the native provider as an accidental
   paid request.

Because of rule 1, arming a redirect while the harness is currently on a
catalog entry changes nothing until the harness returns to a native
model — the status line says so ("redirect armed … idle while the harness
addresses … directly"), and the redirect menu marks the harness's own
pick with `»` alongside the redirect's `*`.

## Configuration reference

Everything lives under `[router]` in `config.toml` (see
`config.toml.template` next to it for the full annotated version):

```toml
[router]
enabled        = true    # default; false opts out of routing and publication
publish_models = true    # default; false keeps redirects, stops augmenting pickers
# port         = 8917    # optional pin. When omitted the daemon reclaims the
                         # port last bound by this home (runtime_dir/router.port),
                         # falling back to 8917 and then a free port if busy.
                         # Harness processes outlive the daemon and keep dialing
                         # the port they were given at spawn, so the persisted
                         # file is what makes restart safe without config.

# Optional picker/subagent ordering, `<route>/<model>` selectors.
# Codex exposes only its five lowest-priority catalog entries to
# spawn_agent; this list chooses that roster.
featured_models = [
  "claude-oauth/opus",
  "codex-oauth/gpt-5.6-sol",
]

[router.oauth]
# Models each subscription login offers. Optional — the default list is
# the same curated one behind smith's /model completion. One string pins
# a single model; a list becomes the picker's second step.
claude-oauth = ["opus", "sonnet"]
codex-oauth  = ["gpt-5.6-sol", "gpt-5.5"]
grok-oauth   = "grok-4.5"
```

## Troubleshooting

- **The menu says a session can't be redirected** — it opened before
  routing was enabled, or its harness has no routing probe. New sessions
  on route-capable harnesses will work.
- **A target is listed but not selectable** — the reason is shown in
  place of its models: a missing API key, an expired login, or a dialect
  with no translator. When the fix is signing in, the menu offers it
  directly: press Enter (or click the reason) and Construct opens a new
  shell session running the owning CLI's login command — sign in there
  (the CLI opens its own browser page). Construct detects the credential
  landing and closes and archives the login session automatically; a
  login that fails stays open so you can read what went wrong.
- **A redirect is armed but reported unproven** — the harness resolved
  its endpoint through a channel that ignores the proxy environment. The
  session keeps working exactly as before; Construct reports what it
  observed rather than pretending the redirect took.
- **No Construct entries in the harness's `/model`** — publication needs
  at least one usable target at session spawn, and (for Codex) a native
  catalog baseline from a prior Codex run. Check logins/profiles, then
  start a new session.

## Design records

Specs [0113](../specs/0113-model-routing-is-proxy-transported.md),
[0114](../specs/0114-session-route-is-durable-session-state.md),
[0115](../specs/0115-routing-injection-is-probe-verified.md),
[0157](../specs/0157-native-model-catalog-routing.md), and
[0158](../specs/0158-native-catalog-selection-is-visible-state.md) record
the decisions behind this behavior.
