//! Single long-running task that owns reloading `config.toml` (spec 0190).
//!
//! `config.toml` is a documented, hand-editable surface, so an edit made in a
//! text editor has to reach the running daemon on the same terms an edit to a
//! service definition beside it already does. This task is what makes that
//! safe.
//!
//! It sits behind an mpsc channel for the same two reasons
//! `service_supervisor` does, in the same order:
//!
//! 1. **Reloads must be serialized.** Two concurrent reloads would each read
//!    the file, then race to install their derivation — and because a reload
//!    swaps four separate things (the daemon environment, storage
//!    directories, the plugin runtime, the router), interleaving two of them
//!    could leave the daemon running a mix of both. One owning task makes a
//!    reload atomic by construction, with no lock for a future caller to
//!    forget.
//! 2. **Insurance against the Send-inference cycle** documented in
//!    `remote_supervisor`. Reload reaches deep into `SessionManager`; a future
//!    edge from there back into `crate::server` would make rustc infer Send
//!    across the whole recursive component. The channel breaks the static edge
//!    before that can happen — which is why this module must never import
//!    `crate::server`.
//!
//! Reload is all-or-nothing: if the file fails to parse, nothing changes. That
//! is also what makes the file watcher safe, since it can catch an editor
//! mid-write and simply retry on the next tick.
//!
//! A reload **re-derives** rather than patches. It reads the config file, the
//! plugin registry, and the plugin manifests and rebuilds the running
//! configuration from all three, exactly as startup does. Patching the config
//! already in hand would be wrong in a way that is easy to miss:
//! `PluginSet::apply_to_config` is additive, so a harness contributed by a
//! plugin that has since been disabled would survive every reload.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use construct_protocol::paths::Paths;
use construct_protocol::ConfigApplyResult;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::session::SessionManager;

/// Why a reload is happening. Purely for logs and for the text a caller sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadReason {
    /// An IPC caller asked, including a client that just wrote the file.
    Ipc,
    /// The watcher saw the file change under a hand edit.
    FileChanged,
}

impl ReloadReason {
    fn label(self) -> &'static str {
        match self {
            ReloadReason::Ipc => "requested",
            ReloadReason::FileChanged => "file changed",
        }
    }
}

pub enum ConfigMsg {
    Reload {
        reason: ReloadReason,
        respond: Option<oneshot::Sender<ConfigApplyResult>>,
    },
}

/// Handle held by `SessionManager` so any caller can request a reload without
/// depending on this module's internals.
#[derive(Clone)]
pub struct ConfigHandle(mpsc::UnboundedSender<ConfigMsg>);

impl ConfigHandle {
    pub async fn reload(&self, reason: ReloadReason) -> Result<ConfigApplyResult> {
        let (tx, rx) = oneshot::channel();
        self.0
            .send(ConfigMsg::Reload {
                reason,
                respond: Some(tx),
            })
            .map_err(|_| anyhow::anyhow!("config supervisor is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("config supervisor dropped the request"))
    }

    /// Fire-and-forget reload, for the file watcher, which has nobody to
    /// report to.
    pub fn reload_detached(&self, reason: ReloadReason) {
        let _ = self.0.send(ConfigMsg::Reload {
            reason,
            respond: None,
        });
    }
}

/// Create the supervisor's channel and handle. The caller spawns [`run`].
pub fn channel() -> (ConfigHandle, mpsc::UnboundedReceiver<ConfigMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ConfigHandle(tx), rx)
}

/// Run the supervisor until the channel closes at daemon shutdown.
pub async fn run(
    manager: Arc<SessionManager>,
    paths: Paths,
    socket_path: PathBuf,
    mut rx: mpsc::UnboundedReceiver<ConfigMsg>,
) {
    while let Some(msg) = rx.recv().await {
        let ConfigMsg::Reload {
            mut reason,
            mut respond,
        } = msg;
        // Coalesce a burst — an editor that writes three times, or a client
        // save racing the watcher — into one reload, keeping the last caller
        // waiting for an answer.
        while let Ok(ConfigMsg::Reload {
            reason: next_reason,
            respond: next_respond,
        }) = rx.try_recv()
        {
            if let Some(stale) = respond.take() {
                let _ = stale.send(ConfigApplyResult::default());
            }
            reason = next_reason;
            respond = next_respond;
        }

        let result = reload(&manager, &paths, &socket_path, reason).await;
        if let Some(respond) = respond {
            let _ = respond.send(result);
        }
    }
}

/// Re-derive the running configuration from disk and install it.
///
/// The order below is not arbitrary. Storage, the plugin runtime, and the
/// router are swapped *before* the config itself, so anything that reads the
/// new config is guaranteed the subsystems behind it are at least as new. The
/// reverse order would leave a window where a caller sees a harness the new
/// config declares while the plugin runtime backing it is still the old one.
pub(crate) async fn reload(
    manager: &Arc<SessionManager>,
    paths: &Paths,
    socket_path: &std::path::Path,
    reason: ReloadReason,
) -> ConfigApplyResult {
    // All-or-nothing starts here: a file caught mid-write fails to parse, and
    // we return having touched nothing.
    let mut next = match Config::load_or_default(paths) {
        Ok(next) => next,
        Err(error) => {
            let error = format!("{error:#}");
            tracing::warn!(
                reason = reason.label(),
                error = %error,
                "config reload refused; the running configuration is unchanged"
            );
            let result = ConfigApplyResult {
                reloaded: false,
                error: Some(error),
                // Whatever was already waiting on a restart still is.
                restart_required: manager.config_restart_required(),
                applied: Vec::new(),
            };
            manager.broadcast_config_state(&result);
            return result;
        }
    };

    let previous = manager.config();

    // Re-derived, never patched: a plugin disabled since the last reload must
    // lose the harnesses it contributed, and a merge cannot subtract.
    let plugin_set = crate::plugins::PluginSet::load(paths);
    plugin_set.apply_to_config(&mut next, paths);

    // Before `route_profiles()` below, which probes the environment to decide
    // which built-in route targets exist — so a credential added to
    // `[daemon.env]` makes its provider reachable in this same reload.
    crate::daemon_env::install(next.daemon.env.clone());

    manager
        .storage()
        .set_overrides(crate::storage::StorageOverrides {
            playbook_templates_dir: crate::config::resolve_playbook_templates_dir(
                &next,
                std::env::var("CONSTRUCT_PLAYBOOK_TEMPLATES_DIR").ok().as_deref(),
            ),
            playbook_verbs_dir: Some(paths.config_dir.join("verbs")),
            plugin_verb_dirs: plugin_set.verb_dirs(),
            plugin_template_dirs: plugin_set.template_dirs(),
        });

    manager.set_plugin_runtime(Arc::new(crate::plugins::PluginRuntime::new(
        plugin_set,
        paths.clone(),
        socket_path.to_path_buf(),
    )));

    let restart_required = manager
        .router
        .apply_config(&next.router, next.smith.route_profiles());

    let applied = describe_applied(&previous, &next);
    manager.set_config(Arc::new(next));

    // The off→on transition the router can take live. Done after the swap and
    // outside every lock: `apply_config` is sync precisely so this await is
    // not held under it.
    if manager.router.wants_start() {
        if let Err(error) = manager.router.start().await {
            tracing::error!(error = %format!("{error:#}"), "router failed to start after config reload");
        }
    }

    // `suggest.enabled` and the minibuffer harness both feed the ambient
    // feature status, so a reload that moved either must republish it or
    // clients keep rendering the old answer.
    manager.broadcast_features_state().await;

    let result = ConfigApplyResult {
        reloaded: true,
        applied,
        restart_required,
        error: None,
    };
    if result.applied.is_empty() && result.restart_required.is_empty() {
        tracing::debug!(reason = reason.label(), "config reloaded; nothing changed");
    } else {
        tracing::info!(
            reason = reason.label(),
            summary = %result.summary(),
            "config reloaded"
        );
    }
    manager.set_config_restart_required(result.restart_required.clone());
    manager.broadcast_config_state(&result);
    result
}

/// Name what changed, for a user. Deliberately coarse — this is the text
/// beside a status line, not a diff. An empty result is what keeps a
/// comment-only edit from announcing itself.
fn describe_applied(previous: &Config, next: &Config) -> Vec<String> {
    let mut applied = Vec::new();

    let before: Vec<&String> = previous.adapters.keys().collect();
    let after: Vec<&String> = next.adapters.keys().collect();
    if before != after {
        applied.push("harnesses".to_string());
    } else if previous
        .adapters
        .iter()
        .zip(next.adapters.values())
        .any(|((_, a), b)| a != b)
    {
        applied.push("harness settings".to_string());
    }

    if previous.daemon.env != next.daemon.env {
        applied.push("[daemon.env]".to_string());
    }
    if previous.defaults.worktree != next.defaults.worktree {
        applied.push("[defaults]".to_string());
    }
    if previous.minibuffer.effective_harness() != next.minibuffer.effective_harness() {
        applied.push("[minibuffer]".to_string());
    }
    if previous.playbook.templates_dir != next.playbook.templates_dir {
        applied.push("[playbook]".to_string());
    }
    if previous.suggest.enabled != next.suggest.enabled {
        applied.push("[suggest]".to_string());
    }
    if previous.smith.models != next.smith.models {
        applied.push("[smith.models]".to_string());
    }
    if previous.router.publish_models != next.router.publish_models
        || previous.router.featured_models != next.router.featured_models
        || previous.router.oauth != next.router.oauth
        || previous.router.enabled != next.router.enabled
    {
        applied.push("[router]".to_string());
    }

    applied
}

/// How often a hand edit is noticed. Configuration changes on human
/// timescales, so this trades a little latency for far fewer wakeups. Matches
/// the service watcher deliberately: the two files sit in the same directory
/// and a user editing both should not see them apply on visibly
/// different schedules.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Notice `config.toml` edited outside the daemon.
///
/// Polling an mtime fingerprint is deliberate, for the reasons the service
/// watcher gives: a filesystem-notification dependency would have to cope with
/// editors that write in place versus write-and-rename, and with event
/// coalescing, to learn the same thing this learns by looking.
///
/// A torn read is harmless because reload is all-or-nothing — the parse fails,
/// nothing changes, and the next tick picks up the finished file.
pub fn spawn_watcher(paths: Paths, handle: ConfigHandle) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(WATCH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Seeded from the file as it is now, so the first tick after boot is
        // a no-op: startup already applied this exact content.
        let mut fingerprint = config_fingerprint(&paths);
        loop {
            interval.tick().await;
            let current = config_fingerprint(&paths);
            if current != fingerprint {
                fingerprint = current;
                handle.reload_detached(ReloadReason::FileChanged);
            }
        }
    });
}

/// Size-and-mtime fingerprint over everything a reload re-derives from.
///
/// Individual paths, never a directory walk: the daemon rewrites
/// `config.toml.template` in this same directory on every boot, and a walk
/// would read that as a user edit.
fn config_fingerprint(paths: &Paths) -> Vec<(String, u64, u128)> {
    let mut parts = vec![file_stamp(&paths.config_file())];
    // Plugins are part of the same derivation, so installing, disabling, or
    // editing one must reach the running daemon on the same schedule as a
    // config edit. The registry alone is not enough: a linked plugin's
    // manifest can change without the registry being rewritten.
    parts.push(file_stamp(&crate::plugins::registry_path(paths)));
    let entries = std::fs::read_dir(crate::plugins::plugins_root(paths))
        .into_iter()
        .flatten()
        .flatten();
    for entry in entries {
        parts.push(file_stamp(&entry.path().join("plugin.toml")));
    }
    parts.sort();
    parts
}

fn file_stamp(path: &std::path::Path) -> (String, u64, u128) {
    let name = path.to_string_lossy().to_string();
    let Ok(meta) = std::fs::metadata(path) else {
        return (name, 0, 0);
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|delta| delta.as_millis())
        .unwrap_or(0);
    (name, meta.len(), modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use std::sync::Arc;

    fn tmp_paths(tmp: &std::path::Path) -> Paths {
        Paths {
            config_dir: tmp.join("config"),
            state_dir: tmp.join("state"),
            data_dir: tmp.join("data"),
            runtime_dir: tmp.join("run"),
        }
    }

    /// A manager wired the way `lib.rs` wires one, minus the supervisor task —
    /// tests drive `reload` directly so they never wait on a watch interval.
    async fn manager_for(paths: &Paths) -> Arc<SessionManager> {
        for dir in [
            &paths.config_dir,
            &paths.state_dir,
            &paths.data_dir,
            &paths.runtime_dir,
        ] {
            std::fs::create_dir_all(dir).expect("create dir");
        }
        let storage =
            Arc::new(crate::storage::Storage::new(paths.data_dir.clone()).expect("storage"));
        let config = Arc::new(Config::load_or_default(paths).expect("initial config"));
        let (manager, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, paths.runtime_dir.clone())
                .await
                .expect("session manager");
        Arc::new(manager)
    }

    async fn reload_now(manager: &Arc<SessionManager>, paths: &Paths) -> ConfigApplyResult {
        reload(
            manager,
            paths,
            &paths.runtime_dir.join("construct.sock"),
            ReloadReason::Ipc,
        )
        .await
    }

    fn write_config(paths: &Paths, body: &str) {
        // Callers write a starting config before building the manager, so the
        // directory may not exist yet.
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), body).expect("write config");
    }

    #[tokio::test]
    async fn a_new_harness_becomes_available_without_a_restart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        let manager = manager_for(&paths).await;
        assert!(
            !manager.config().adapters.contains_key("demo"),
            "the harness does not exist yet"
        );

        write_config(
            &paths,
            "[adapters.demo]\nbinary = \"/bin/sh\"\ndescription = \"demo\"\n",
        );
        let result = reload_now(&manager, &paths).await;

        assert!(result.reloaded, "the reload should have been accepted");
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(
            manager.config().adapters.contains_key("demo"),
            "the new harness is in force"
        );
        assert!(
            result.applied.iter().any(|a| a == "harnesses"),
            "the change should be named to the user: {:?}",
            result.applied
        );
    }

    #[tokio::test]
    async fn a_harness_removed_from_config_stops_being_offered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        write_config(&paths, "[adapters.demo]\nbinary = \"/bin/sh\"\n");
        let manager = manager_for(&paths).await;
        assert!(manager.config().adapters.contains_key("demo"));

        write_config(&paths, "");
        reload_now(&manager, &paths).await;

        assert!(
            !manager.config().adapters.contains_key("demo"),
            "a removed harness must not survive the reload"
        );
    }

    /// REGRESSION: all-or-nothing. A file caught mid-write — or simply
    /// wrong — must leave the running configuration exactly as it was, which
    /// is what makes polling the file safe.
    #[tokio::test]
    async fn a_config_that_does_not_parse_changes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        write_config(&paths, "[adapters.demo]\nbinary = \"/bin/sh\"\n");
        let manager = manager_for(&paths).await;

        write_config(&paths, "[adapters.demo]\nbinary = ");
        let result = reload_now(&manager, &paths).await;

        assert!(!result.reloaded, "a torn file is not an edit");
        assert!(result.error.is_some(), "the user is told why");
        assert!(
            manager.config().adapters.contains_key("demo"),
            "the running configuration is untouched"
        );

        // ...and the corrected file is picked up on the next tick.
        write_config(&paths, "[adapters.other]\nbinary = \"/bin/sh\"\n");
        let result = reload_now(&manager, &paths).await;
        assert!(result.reloaded, "recovery on the next reload");
        assert!(manager.config().adapters.contains_key("other"));
    }

    /// REGRESSION: pins the re-derive decision (spec 0190).
    /// `PluginSet::apply_to_config` is additive — it can add a harness but
    /// never subtract one — so a reload that patched the config already in
    /// hand would leave a disabled plugin's harness in place forever.
    #[tokio::test]
    async fn a_harness_from_a_disabled_plugin_is_dropped_on_reload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        std::fs::create_dir_all(&paths.data_dir).expect("data dir");
        let root = tmp.path().join("plug");
        std::fs::create_dir_all(&root).expect("plugin root");
        std::fs::write(
            root.join(crate::plugins::MANIFEST_FILE),
            format!(
                r#"
                [plugin]
                id = "demo-plug"
                name = "Demo"
                version = "1.0.0"
                min_construct_version = "0.1.0"

                [[adapters]]
                name = "plugged"
                binary = "{}"
                "#,
                "/bin/sh"
            ),
        )
        .expect("write manifest");
        let manifest =
            crate::plugins::PluginManifest::load(&root).expect("manifest parses");
        crate::plugins::register(&paths, &manifest, &root, "link").expect("register");

        let manager = manager_for(&paths).await;
        reload_now(&manager, &paths).await;
        assert!(
            manager.config().adapters.contains_key("demo-plug:plugged"),
            "the plugin's harness should be offered while it is enabled: {:?}",
            manager.config().adapters.keys().collect::<Vec<_>>()
        );

        crate::plugins::set_enabled(&paths, "demo-plug", false).expect("disable");
        reload_now(&manager, &paths).await;

        assert!(
            !manager.config().adapters.contains_key("demo-plug:plugged"),
            "a disabled plugin's harness must not survive a reload: {:?}",
            manager.config().adapters.keys().collect::<Vec<_>>()
        );
    }

    /// A comment-only edit still bumps the file's mtime and reloads. It must
    /// not announce itself, or the status bar chatters on every save.
    #[tokio::test]
    async fn an_edit_that_changes_nothing_applies_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        write_config(&paths, "[adapters.demo]\nbinary = \"/bin/sh\"\n");
        let manager = manager_for(&paths).await;

        write_config(
            &paths,
            "# a note to self\n[adapters.demo]\nbinary = \"/bin/sh\"\n",
        );
        let result = reload_now(&manager, &paths).await;

        assert!(result.reloaded);
        assert!(
            result.applied.is_empty(),
            "nothing semantic changed: {:?}",
            result.applied
        );
        assert_eq!(result.summary(), "config reloaded; nothing changed");
    }

    #[tokio::test]
    async fn a_restart_residue_is_reported_and_sticks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        let manager = manager_for(&paths).await;

        write_config(&paths, "[router]\nport = 8999\n");
        let result = reload_now(&manager, &paths).await;

        assert!(
            result.restart_required.iter().any(|r| r.contains("[router] port")),
            "a port change is restart-only: {:?}", result.restart_required
        );
        assert_eq!(
            manager.config_restart_required(),
            result.restart_required,
            "the daemon holds the residue so a reconnecting client still sees it"
        );
        assert!(
            !manager.config_state_payload().restart_required.is_empty(),
            "the replayed payload carries it too"
        );
        assert!(
            manager.config_state_payload().applied.is_empty(),
            "a replay is not a reload and must not re-announce an apply"
        );
    }

    /// The daemon rewrites `config.toml.template` in this directory on every
    /// boot. A fingerprint that walked the directory would read that as an
    /// minibuffer edit and reload on a loop.
    #[test]
    fn the_fingerprint_ignores_the_config_template() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("write config");
        let before = config_fingerprint(&paths);

        std::fs::write(paths.config_template_file(), "# template\n").expect("write template");

        assert_eq!(
            before,
            config_fingerprint(&paths),
            "the template is not the configuration"
        );
    }

    #[test]
    fn the_fingerprint_notices_a_config_edit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = tmp_paths(tmp.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("write config");
        let before = config_fingerprint(&paths);

        std::fs::write(paths.config_file(), "[adapters.demo]\n").expect("edit config");

        assert_ne!(before, config_fingerprint(&paths), "an edit is noticed");
    }
}
