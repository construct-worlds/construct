//! XDG-style path conventions shared between daemon and client.
//!
//! Each layer respects `CONSTRUCT_*_DIR` env overrides, then `CONSTRUCT_HOME`,
//! then `XDG_*_HOME`, falling back to standard `$HOME/.config|.local/state|.local/share/construct`.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let home = home_dir();
        let construct_home = env_dir("CONSTRUCT_HOME");

        let config_dir = env_dir("CONSTRUCT_CONFIG_DIR").unwrap_or_else(|| {
            if let Some(ref ch) = construct_home {
                ch.join("config")
            } else {
                env_dir("XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("construct")
            }
        });
        let state_dir = env_dir("CONSTRUCT_STATE_DIR").unwrap_or_else(|| {
            if let Some(ref ch) = construct_home {
                ch.join("state")
            } else {
                env_dir("XDG_STATE_HOME")
                    .unwrap_or_else(|| home.join(".local").join("state"))
                    .join("construct")
            }
        });
        let data_dir = env_dir("CONSTRUCT_DATA_DIR").unwrap_or_else(|| {
            if let Some(ref ch) = construct_home {
                ch.join("data")
            } else {
                env_dir("XDG_DATA_HOME")
                    .unwrap_or_else(|| home.join(".local").join("share"))
                    .join("construct")
            }
        });
        let runtime_dir = env_dir("CONSTRUCT_RUNTIME_DIR").unwrap_or_else(|| {
            if let Some(ref ch) = construct_home {
                ch.join("run")
            } else {
                env_dir("XDG_RUNTIME_DIR")
                    .map(|p| p.join("construct"))
                    .unwrap_or_else(|| state_dir.clone())
            }
        });

        Self {
            config_dir,
            state_dir,
            data_dir,
            runtime_dir,
        }
    }

    /// Resolve the legacy `agentd` layout so startup can offer a migration
    /// message when existing `~/.config|.local|XDG_*` directories are still
    /// using pre-rename names.
    pub fn discover_legacy() -> Self {
        let home = home_dir();

        let config_dir = env_dir("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("agentd");
        let state_dir = env_dir("XDG_STATE_HOME")
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join("agentd");
        let data_dir = env_dir("XDG_DATA_HOME")
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join("agentd");
        let runtime_dir = env_dir("XDG_RUNTIME_DIR")
            .map(|p| p.join("agentd"))
            .unwrap_or_else(|| state_dir.clone());

        Self {
            config_dir,
            state_dir,
            data_dir,
            runtime_dir,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("construct.sock")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_dir.join("daemon.pid")
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn config_template_file(&self) -> PathBuf {
        self.config_dir.join("config.toml.template")
    }

    pub fn services_dir(&self) -> PathBuf {
        self.config_dir.join("services")
    }

    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join("keymap.toml")
    }

    pub fn midi_file(&self) -> PathBuf {
        self.config_dir.join("midi.toml")
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_root().join(id)
    }

    pub fn tui_state_file(&self) -> PathBuf {
        self.state_dir.join("tui-state.json")
    }

    /// Path to the learned per-model token-limit table — smith
    /// adapts this at runtime when providers reject requests as
    /// over-budget and bumps it on successful probe calls.
    pub fn smith_model_limits_file(&self) -> PathBuf {
        self.state_dir.join("smith-model-limits.json")
    }

    /// Last successfully bound model-router port for this home.
    ///
    /// Written after the daemon binds the router listener so a later
    /// restart of the *same* home reclaims the port live harnesses are
    /// still dialing. Distinct homes (distinct runtime dirs) get
    /// distinct files, so two daemons no longer fight over 8917.
    pub fn router_port_file(&self) -> PathBuf {
        self.runtime_dir.join("router.port")
    }

    /// Last successfully bound localhost web-UI port for this home.
    ///
    /// Same reclaim story as [`Self::router_port_file`]: keeps
    /// `construct paths` and browser bookmarks stable across restarts
    /// of one home, while letting a second home pick a free port.
    pub fn webui_port_file(&self) -> PathBuf {
        self.runtime_dir.join("webui.port")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Default port for the localhost-only browser UI. Override with the
/// `CONSTRUCT_WEBUI_PORT` env var, or let the daemon auto-pick and persist
/// under [`Paths::webui_port_file`]. The daemon binds `127.0.0.1:<port>`;
/// the CLI's `construct paths` prints the resolved URL.
pub const DEFAULT_WEBUI_PORT: u16 = 5746;

/// Default port for the model-route proxy. Override with `[router] port`
/// in config.toml, or let the daemon auto-pick and persist under
/// [`Paths::router_port_file`].
pub const DEFAULT_ROUTER_PORT: u16 = 8917;

/// Env var that pins the localhost web-UI port (no auto-fallback).
pub const WEBUI_PORT_ENV: &str = "CONSTRUCT_WEBUI_PORT";

/// Read a port written by a previous bind of this home, if any.
pub fn read_persisted_port(path: &std::path::Path) -> Option<u16> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse::<u16>().ok().filter(|p| *p != 0)
}

/// Persist the port a listener actually bound so the next daemon start
/// for this home reclaims it. Best-effort: a write failure only means
/// the next boot falls back to the compiled default.
pub fn write_persisted_port(path: &std::path::Path, port: u16) -> std::io::Result<()> {
    if port == 0 {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("port.tmp");
    std::fs::write(&tmp, format!("{port}\n"))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Preferred localhost web-UI port for this process, without binding.
///
/// Precedence: `CONSTRUCT_WEBUI_PORT` env (explicit pin) → persisted
/// [`Paths::webui_port_file`] → [`DEFAULT_WEBUI_PORT`].
pub fn local_webui_port() -> u16 {
    local_webui_port_for(&Paths::discover())
}

/// Like [`local_webui_port`] against an already-resolved [`Paths`].
pub fn local_webui_port_for(paths: &Paths) -> u16 {
    if let Some(port) = std::env::var(WEBUI_PORT_ENV)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
    {
        return port;
    }
    read_persisted_port(&paths.webui_port_file()).unwrap_or(DEFAULT_WEBUI_PORT)
}

/// True when the user pinned the web-UI port via env — the daemon
/// must not auto-fallback or overwrite the persisted file.
pub fn local_webui_port_explicit() -> bool {
    std::env::var(WEBUI_PORT_ENV)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .is_some()
}

/// The resolved localhost web-UI URL (`http://127.0.0.1:<port>/`).
pub fn local_webui_url() -> String {
    local_webui_url_for(&Paths::discover())
}

/// Like [`local_webui_url`] against an already-resolved [`Paths`].
pub fn local_webui_url_for(paths: &Paths) -> String {
    format!("http://127.0.0.1:{}/", local_webui_port_for(paths))
}

/// Preferred router port before binding.
///
/// Precedence: explicit `configured` value → persisted
/// [`Paths::router_port_file`] → [`DEFAULT_ROUTER_PORT`].
/// A configured `Some(0)` means "ask the OS" (tests) and skips the file.
pub fn preferred_router_port(paths: &Paths, configured: Option<u16>) -> u16 {
    match configured {
        Some(port) => port,
        None => read_persisted_port(&paths.router_port_file()).unwrap_or(DEFAULT_ROUTER_PORT),
    }
}

/// Resolve a sibling binary (an adapter, `construct-mcp`, etc.) by name.
/// Search order: absolute path → next to the current executable → `$PATH`.
/// Returns `None` if not found. Used by the daemon to find adapter
/// binaries and by adapters to find auxiliary tools like `construct-mcp`.
pub fn locate_sibling_binary(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(name);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&p);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Mutex to ensure env var mutation is serialized
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn lock(vars: &[&'static str]) -> Self {
            let lock = ENV_MUTEX.lock().unwrap();
            let mut saved = Vec::new();
            for var in vars {
                saved.push((*var, std::env::var_os(var)));
                std::env::remove_var(var);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (var, val) in &self.saved {
                if let Some(ref v) = val {
                    std::env::set_var(var, v);
                } else {
                    std::env::remove_var(var);
                }
            }
        }
    }

    #[test]
    fn test_construct_home_defaults() {
        let _guard = EnvGuard::lock(&[
            "CONSTRUCT_HOME",
            "CONSTRUCT_CONFIG_DIR",
            "CONSTRUCT_STATE_DIR",
            "CONSTRUCT_DATA_DIR",
            "CONSTRUCT_RUNTIME_DIR",
        ]);

        std::env::set_var("CONSTRUCT_HOME", "/test/home");

        let paths = Paths::discover();
        assert_eq!(paths.config_dir, PathBuf::from("/test/home/config"));
        assert_eq!(paths.state_dir, PathBuf::from("/test/home/state"));
        assert_eq!(paths.data_dir, PathBuf::from("/test/home/data"));
        assert_eq!(paths.runtime_dir, PathBuf::from("/test/home/run"));
    }

    #[test]
    fn test_construct_home_with_overrides() {
        let _guard = EnvGuard::lock(&[
            "CONSTRUCT_HOME",
            "CONSTRUCT_CONFIG_DIR",
            "CONSTRUCT_STATE_DIR",
            "CONSTRUCT_DATA_DIR",
            "CONSTRUCT_RUNTIME_DIR",
        ]);

        std::env::set_var("CONSTRUCT_HOME", "/test/home");
        std::env::set_var("CONSTRUCT_CONFIG_DIR", "/override/config");
        std::env::set_var("CONSTRUCT_RUNTIME_DIR", "/override/run");

        let paths = Paths::discover();
        assert_eq!(paths.config_dir, PathBuf::from("/override/config"));
        assert_eq!(paths.state_dir, PathBuf::from("/test/home/state"));
        assert_eq!(paths.data_dir, PathBuf::from("/test/home/data"));
        assert_eq!(paths.runtime_dir, PathBuf::from("/override/run"));
    }

    #[test]
    fn persisted_port_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("router.port");
        write_persisted_port(&path, 9123).unwrap();
        assert_eq!(read_persisted_port(&path), Some(9123));
        // Zero is never a real bound port; refuse to write it.
        write_persisted_port(&path, 0).unwrap();
        assert_eq!(read_persisted_port(&path), Some(9123));
        // Junk is ignored.
        std::fs::write(&path, "not-a-port\n").unwrap();
        assert_eq!(read_persisted_port(&path), None);
    }

    #[test]
    fn preferred_router_port_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            runtime_dir: dir.path().to_path_buf(),
        };
        // No file, no pin → compiled default.
        assert_eq!(preferred_router_port(&paths, None), DEFAULT_ROUTER_PORT);
        // Persisted file wins over the default.
        write_persisted_port(&paths.router_port_file(), 9333).unwrap();
        assert_eq!(preferred_router_port(&paths, None), 9333);
        // Explicit config pin wins over the file.
        assert_eq!(preferred_router_port(&paths, Some(9444)), 9444);
        // Explicit 0 (tests) is honored literally.
        assert_eq!(preferred_router_port(&paths, Some(0)), 0);
    }

    #[test]
    fn local_webui_port_prefers_env_then_file() {
        let _guard = EnvGuard::lock(&[WEBUI_PORT_ENV]);
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            runtime_dir: dir.path().to_path_buf(),
        };
        assert_eq!(local_webui_port_for(&paths), DEFAULT_WEBUI_PORT);
        write_persisted_port(&paths.webui_port_file(), 6001).unwrap();
        assert_eq!(local_webui_port_for(&paths), 6001);
        std::env::set_var(WEBUI_PORT_ENV, "6002");
        assert_eq!(local_webui_port_for(&paths), 6002);
        assert!(local_webui_port_explicit());
    }
}
