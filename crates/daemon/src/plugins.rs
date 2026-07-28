//! Plugin system (spec 0151): manifest parsing, the installed-plugin
//! registry, and the merge of plugin contributions into the daemon's
//! existing extension seams (adapters, program verbs, program templates,
//! injected MCP servers).
//!
//! A plugin is a directory containing a `construct-plugin.toml` manifest.
//! Plugins never link into the binary — every contribution lands on a seam
//! that already exists for out-of-process or data-file extensions. The
//! daemon reads the registry once at startup; `construct plugin …` CLI
//! commands mutate the registry on disk and tell the user to restart the
//! daemon (sessions survive a restart, so applying is cheap).

use crate::config::{AdapterConfig, Config};
use anyhow::{bail, Context, Result};
use construct_protocol::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Manifest file name at a plugin's root.
pub const MANIFEST_FILE: &str = "construct-plugin.toml";

/// Env var carrying plugin-contributed MCP servers to adapters as a JSON
/// array (see `construct_protocol::adapter::PLUGIN_MCP_SERVERS_ENV` for the
/// consumer side; the two constants must stay equal).
pub const PLUGIN_MCP_SERVERS_ENV: &str = construct_protocol::adapter::PLUGIN_MCP_SERVERS_ENV;

/// Env vars injected into plugin-owned processes (plugin adapters now;
/// actions/hooks in later phases).
pub const ENV_PLUGIN_ID: &str = "CONSTRUCT_PLUGIN_ID";
pub const ENV_PLUGIN_ROOT: &str = "CONSTRUCT_PLUGIN_ROOT";
pub const ENV_PLUGIN_CONFIG_DIR: &str = "CONSTRUCT_PLUGIN_CONFIG_DIR";
pub const ENV_PLUGIN_STATE_DIR: &str = "CONSTRUCT_PLUGIN_STATE_DIR";

// ── Manifest ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    /// Commands run once at install time, in order, with the plugin root as
    /// cwd. Never run at daemon startup.
    #[serde(default)]
    pub build: Vec<BuildStep>,
    /// AHP harnesses this plugin contributes.
    #[serde(default)]
    pub adapters: Vec<PluginAdapterDecl>,
    /// Directory of program-verb definition files, relative to the root.
    #[serde(default)]
    pub verbs: Option<PluginDirSection>,
    /// Directory of program-template files, relative to the root.
    #[serde(default)]
    pub templates: Option<PluginDirSection>,
    /// MCP tool servers injected into harness sessions alongside the
    /// construct MCP server.
    #[serde(default)]
    pub mcp_servers: Vec<PluginMcpServerDecl>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMeta {
    /// Stable identifier; namespaces everything the plugin contributes.
    /// ASCII letters, digits, dot, underscore, hyphen.
    pub id: String,
    pub name: String,
    pub version: String,
    /// Oldest construct version the plugin works with. Install/link refuse
    /// a plugin whose minimum is newer than the running binary.
    pub min_construct_version: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Supported platforms (`macos`, `linux`, `windows`). Empty = all.
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildStep {
    /// Argv array; no shell interpretation.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginAdapterDecl {
    /// Adapter name within the plugin. Exposed as harness `<plugin-id>`
    /// when it equals the plugin id, `<plugin-id>:<name>` otherwise.
    pub name: String,
    /// Binary path; relative paths resolve against the plugin root.
    pub binary: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginDirSection {
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMcpServerDecl {
    /// Server name. Registered as MCP key `<plugin-id>` when it equals the
    /// plugin id, `<plugin-id>-<name>` otherwise.
    pub name: String,
    /// Argv array; `command[0]` resolves against the plugin root when
    /// relative.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl PluginManifest {
    /// Read and validate `construct-plugin.toml` under `root`.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let manifest: PluginManifest =
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        validate_id(&self.plugin.id)?;
        if self.plugin.name.trim().is_empty() {
            bail!("plugin name must not be empty");
        }
        if parse_version(&self.plugin.min_construct_version).is_none() {
            bail!(
                "min_construct_version `{}` is not a dotted version",
                self.plugin.min_construct_version
            );
        }
        for a in &self.adapters {
            if a.name.trim().is_empty() || a.name.contains(':') {
                bail!("adapter name `{}` must be non-empty and contain no `:`", a.name);
            }
            if a.binary.trim().is_empty() {
                bail!("adapter `{}` needs a binary", a.name);
            }
        }
        for s in &self.mcp_servers {
            if s.name.trim().is_empty() || s.name.contains(':') || s.name.contains('.') {
                bail!(
                    "mcp server name `{}` must be non-empty and contain no `:` or `.`",
                    s.name
                );
            }
            if s.command.is_empty() {
                bail!("mcp server `{}` needs a command", s.name);
            }
        }
        for b in &self.build {
            if b.command.is_empty() {
                bail!("build step with empty command");
            }
        }
        Ok(())
    }

    /// Human-readable capability summary shown before install/link consent.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let meta = &self.plugin;
        out.push_str(&format!(
            "{} ({}) v{} — requires construct >= {}\n",
            meta.name, meta.id, meta.version, meta.min_construct_version
        ));
        if let Some(desc) = meta.description.as_deref() {
            out.push_str(&format!("  {desc}\n"));
        }
        for b in &self.build {
            out.push_str(&format!("  build: {}\n", b.command.join(" ")));
        }
        for a in &self.adapters {
            out.push_str(&format!(
                "  adapter: {} ({}) — runs `{}` as your user\n",
                harness_name(&meta.id, &a.name),
                a.description.as_deref().unwrap_or("no description"),
                a.binary
            ));
        }
        if let Some(v) = &self.verbs {
            out.push_str(&format!("  verbs: {}/ (prompt definitions)\n", v.dir));
        }
        if let Some(t) = &self.templates {
            out.push_str(&format!("  templates: {}/\n", t.dir));
        }
        for s in &self.mcp_servers {
            out.push_str(&format!(
                "  mcp server: {} — `{}` injected into every harness session\n",
                mcp_key(&meta.id, &s.name),
                s.command.join(" ")
            ));
        }
        out
    }

    /// Refuse when this construct is older than the plugin's minimum, or
    /// the platform is unsupported.
    pub fn check_compatible(&self, current_version: &str) -> Result<()> {
        let min = parse_version(&self.plugin.min_construct_version)
            .context("invalid min_construct_version")?;
        let cur = parse_version(current_version)
            .with_context(|| format!("invalid current version {current_version}"))?;
        if min > cur {
            bail!(
                "plugin {} requires construct >= {}, but this is {}",
                self.plugin.id,
                self.plugin.min_construct_version,
                current_version
            );
        }
        if !self.plugin.platforms.is_empty() {
            let this = current_platform();
            if !self.plugin.platforms.iter().any(|p| p == this) {
                bail!(
                    "plugin {} supports [{}], not {}",
                    self.plugin.id,
                    self.plugin.platforms.join(", "),
                    this
                );
            }
        }
        Ok(())
    }
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// Plugin ids namespace harness names, verb names, and template ids, so the
/// charset is restrictive: ASCII letters, digits, dot, underscore, hyphen.
/// `:` is excluded because it is the namespace separator.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("plugin id must be 1–64 characters");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("plugin id `{id}` may only contain ASCII letters, digits, `.`, `_`, `-`");
    }
    if id.starts_with('.') {
        bail!("plugin id `{id}` must not start with `.`");
    }
    Ok(())
}

/// Parse a leading `X[.Y[.Z]]` triple, ignoring any suffix after the third
/// component (so `0.16.1`, `0.16`, and `1.0.0-rc1` all parse).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut nums = [0u64; 3];
    for (i, part) in v.trim().splitn(3, '.').enumerate() {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        nums[i] = digits.parse().ok()?;
    }
    Some((nums[0], nums[1], nums[2]))
}

/// Harness name a plugin adapter is exposed under.
pub fn harness_name(plugin_id: &str, adapter_name: &str) -> String {
    if plugin_id == adapter_name {
        plugin_id.to_string()
    } else {
        format!("{plugin_id}:{adapter_name}")
    }
}

/// MCP server key a plugin server is registered under (`mcpServers.<key>`
/// in injected harness configs). `.`/`:` are excluded by validation so the
/// key stays a bare TOML key for codex's dotted `-c` overrides.
pub fn mcp_key(plugin_id: &str, server_name: &str) -> String {
    if plugin_id == server_name {
        plugin_id.to_string()
    } else {
        format!("{plugin_id}-{server_name}")
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// `data_dir/plugins/registry.toml`: which plugins are installed, where
/// their roots are, and whether they are enabled. Managed checkouts live in
/// `data_dir/plugins/<id>/`; linked plugins point wherever the developer's
/// working copy is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub plugins: BTreeMap<String, RegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Directory containing the manifest.
    pub root: PathBuf,
    /// `github:<owner>/<repo>` or `link`.
    pub source: String,
    pub enabled: bool,
    /// Manifest version at install/link time (informational).
    pub version: String,
}

pub fn plugins_root(paths: &Paths) -> PathBuf {
    paths.data_dir.join("plugins")
}

pub fn registry_path(paths: &Paths) -> PathBuf {
    plugins_root(paths).join("registry.toml")
}

impl Registry {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = registry_path(paths);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let path = registry_path(paths);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serialize plugin registry")?;
        std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))
    }
}

// ── Loaded set ──────────────────────────────────────────────────────────────

/// One enabled plugin whose manifest parsed and passed the compatibility
/// gate at load time.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub id: String,
    pub root: PathBuf,
    pub manifest: PluginManifest,
}

impl LoadedPlugin {
    /// Env vars every process this plugin owns receives. Injected by the
    /// daemon after its own `CONSTRUCT_*` scrub, so nested sessions do not
    /// inherit another plugin's identity.
    pub fn env(&self, paths: &Paths) -> HashMap<String, String> {
        let config_dir = paths.config_dir.join("plugins").join(&self.id);
        let state_dir = paths.state_dir.join("plugins").join(&self.id);
        std::fs::create_dir_all(&config_dir).ok();
        std::fs::create_dir_all(&state_dir).ok();
        HashMap::from([
            (ENV_PLUGIN_ID.to_string(), self.id.clone()),
            (
                ENV_PLUGIN_ROOT.to_string(),
                self.root.to_string_lossy().to_string(),
            ),
            (
                ENV_PLUGIN_CONFIG_DIR.to_string(),
                config_dir.to_string_lossy().to_string(),
            ),
            (
                ENV_PLUGIN_STATE_DIR.to_string(),
                state_dir.to_string_lossy().to_string(),
            ),
        ])
    }

    /// Resolve a manifest-relative path against the plugin root.
    pub fn resolve(&self, rel: &str) -> PathBuf {
        let p = PathBuf::from(rel);
        if p.is_absolute() {
            p
        } else {
            self.root.join(p)
        }
    }
}

/// Every enabled, loadable plugin. Broken plugins are skipped with a
/// warning — one bad manifest must never take the daemon down.
#[derive(Debug, Clone, Default)]
pub struct PluginSet {
    pub plugins: Vec<LoadedPlugin>,
}

impl PluginSet {
    pub fn load(paths: &Paths) -> Self {
        Self::load_for_version(paths, env!("CARGO_PKG_VERSION"))
    }

    fn load_for_version(paths: &Paths, current_version: &str) -> Self {
        let registry = match Registry::load(paths) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "plugin registry unreadable; loading no plugins");
                return Self::default();
            }
        };
        let mut plugins = Vec::new();
        for (id, entry) in registry.plugins {
            if !entry.enabled {
                tracing::debug!(plugin = %id, "plugin disabled; skipping");
                continue;
            }
            let manifest = match PluginManifest::load(&entry.root) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(plugin = %id, error = %format!("{e:#}"), "plugin manifest failed to load; skipping");
                    continue;
                }
            };
            if manifest.plugin.id != id {
                tracing::warn!(
                    plugin = %id,
                    manifest_id = %manifest.plugin.id,
                    "plugin manifest id does not match registry entry; skipping"
                );
                continue;
            }
            if let Err(e) = manifest.check_compatible(current_version) {
                tracing::warn!(plugin = %id, error = %format!("{e:#}"), "plugin incompatible; skipping");
                continue;
            }
            plugins.push(LoadedPlugin {
                id,
                root: entry.root,
                manifest,
            });
        }
        Self { plugins }
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Merge plugin adapters into the adapter map and stamp the plugin MCP
    /// env onto every adapter entry. Field-level merge mirrors the built-in
    /// layering in `Config::load_or_default`: an `[adapters."<name>"]` block
    /// the user wrote to tweak one field keeps the plugin's binary/args.
    /// A name that fully collides with a built-in or user-declared adapter
    /// is left alone (existing config wins) with a warning.
    pub fn apply_to_config(&self, cfg: &mut Config, paths: &Paths) {
        for plugin in &self.plugins {
            let plugin_env = plugin.env(paths);
            for decl in &plugin.manifest.adapters {
                let name = harness_name(&plugin.id, &decl.name);
                let binary = plugin.resolve(&decl.binary).to_string_lossy().to_string();
                let entry = cfg.adapters.entry(name.clone()).or_default();
                if entry.binary.is_some() && !name.contains(':') && plugin.id != name {
                    // Unreachable by construction (names either contain the
                    // plugin id or a colon), kept as a guard for clarity.
                    tracing::warn!(harness = %name, "adapter name collision; existing config wins");
                    continue;
                }
                if entry.binary.is_none() {
                    entry.binary = Some(binary);
                }
                if entry.args.is_empty() {
                    entry.args = decl.args.clone();
                }
                if entry.description.is_none() {
                    entry.description = Some(
                        decl.description
                            .clone()
                            .unwrap_or_else(|| format!("{} (plugin)", plugin.manifest.plugin.name)),
                    );
                }
                for (k, v) in decl.env.iter().chain(plugin_env.iter()) {
                    entry.env.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        if let Some(json) = self.mcp_servers_json(paths) {
            for entry in cfg.adapters.values_mut() {
                entry
                    .env
                    .entry(PLUGIN_MCP_SERVERS_ENV.to_string())
                    .or_insert_with(|| json.clone());
            }
        }
    }

    /// Plugin verb directories as `(namespace, dir)` pairs; only existing
    /// directories are returned.
    pub fn verb_dirs(&self) -> Vec<(String, PathBuf)> {
        self.dir_sections(|m| m.verbs.as_ref())
    }

    /// Plugin template directories as `(namespace, dir)` pairs.
    pub fn template_dirs(&self) -> Vec<(String, PathBuf)> {
        self.dir_sections(|m| m.templates.as_ref())
    }

    fn dir_sections(
        &self,
        pick: impl Fn(&PluginManifest) -> Option<&PluginDirSection>,
    ) -> Vec<(String, PathBuf)> {
        self.plugins
            .iter()
            .filter_map(|p| {
                let section = pick(&p.manifest)?;
                let dir = p.resolve(&section.dir);
                if dir.is_dir() {
                    Some((p.id.clone(), dir))
                } else {
                    tracing::warn!(plugin = %p.id, dir = %dir.display(), "declared plugin directory missing");
                    None
                }
            })
            .collect()
    }

    /// JSON payload for [`PLUGIN_MCP_SERVERS_ENV`]: every plugin MCP server,
    /// command resolved against its plugin root, plugin identity env merged
    /// in. `None` when no plugin declares a server.
    pub fn mcp_servers_json(&self, paths: &Paths) -> Option<String> {
        let mut servers = Vec::new();
        for plugin in &self.plugins {
            let plugin_env = plugin.env(paths);
            for decl in &plugin.manifest.mcp_servers {
                let mut command = decl.command.clone();
                if let Some(first) = command.first_mut() {
                    *first = plugin.resolve(first).to_string_lossy().to_string();
                }
                let mut env: BTreeMap<String, String> = plugin_env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (k, v) in &decl.env {
                    env.insert(k.clone(), v.clone());
                }
                servers.push(serde_json::json!({
                    "name": mcp_key(&plugin.id, &decl.name),
                    "command": command[0],
                    "args": command[1..].to_vec(),
                    "env": env,
                }));
            }
        }
        if servers.is_empty() {
            None
        } else {
            serde_json::to_string(&servers).ok()
        }
    }
}

// ── Install / link operations (driven by the CLI) ───────────────────────────

/// `owner/repo[/subdir…]` → (clone URL, owner/repo, optional subdir).
pub fn parse_github_spec(spec: &str) -> Result<(String, String, Option<String>)> {
    let spec = spec.trim().trim_start_matches("https://github.com/");
    let mut parts = spec.splitn(3, '/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        bail!("expected `owner/repo[/subdir]`, got `{spec}`");
    }
    let subdir = parts.next().map(|s| s.trim_matches('/').to_string()).filter(|s| !s.is_empty());
    Ok((
        format!("https://github.com/{owner}/{repo}.git"),
        format!("{owner}/{repo}"),
        subdir,
    ))
}

/// Clone `url` (at optional `git_ref`) into a temp dir under the plugins
/// root, so the final move into place is a same-filesystem rename.
pub fn clone_to_temp(paths: &Paths, url: &str, git_ref: Option<&str>) -> Result<PathBuf> {
    let root = plugins_root(paths);
    std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let tmp = root.join(format!(".tmp-install-{}", std::process::id()));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).ok();
    }
    let mut clone = std::process::Command::new("git");
    clone.arg("clone");
    if git_ref.is_none() {
        clone.args(["--depth", "1"]);
    }
    clone.arg(url).arg(&tmp);
    let status = clone.status().context("run git clone")?;
    if !status.success() {
        std::fs::remove_dir_all(&tmp).ok();
        bail!("git clone {url} failed");
    }
    if let Some(r) = git_ref {
        let status = std::process::Command::new("git")
            .args(["checkout", r])
            .current_dir(&tmp)
            .status()
            .context("run git checkout")?;
        if !status.success() {
            std::fs::remove_dir_all(&tmp).ok();
            bail!("git checkout {r} failed");
        }
    }
    Ok(tmp)
}

/// Move a validated temp clone into `data_dir/plugins/<id>` and return the
/// manifest root (the checkout joined with `subdir` when present).
pub fn finalize_checkout(
    paths: &Paths,
    tmp: &Path,
    id: &str,
    subdir: Option<&str>,
) -> Result<PathBuf> {
    let dest = plugins_root(paths).join(id);
    if dest.exists() {
        std::fs::remove_dir_all(tmp).ok();
        bail!("plugin `{id}` is already installed; `construct plugin uninstall {id}` first");
    }
    std::fs::rename(tmp, &dest)
        .with_context(|| format!("move {} -> {}", tmp.display(), dest.display()))?;
    Ok(match subdir {
        Some(s) => dest.join(s),
        None => dest,
    })
}

/// Run the manifest's build steps with the plugin root as cwd, inheriting
/// stdio so the user sees compiler output. Any failing step aborts.
pub fn run_build_steps(root: &Path, manifest: &PluginManifest) -> Result<()> {
    for step in &manifest.build {
        let (program, args) = step
            .command
            .split_first()
            .context("build step with empty command")?;
        println!("[{}] build: {}", manifest.plugin.id, step.command.join(" "));
        let status = std::process::Command::new(program)
            .args(args)
            .current_dir(root)
            .status()
            .with_context(|| format!("run build step `{}`", step.command.join(" ")))?;
        if !status.success() {
            bail!("build step `{}` failed ({status})", step.command.join(" "));
        }
    }
    Ok(())
}

/// Insert or replace a registry entry.
pub fn register(paths: &Paths, manifest: &PluginManifest, root: &Path, source: &str) -> Result<()> {
    let mut registry = Registry::load(paths)?;
    registry.plugins.insert(
        manifest.plugin.id.clone(),
        RegistryEntry {
            root: root.to_path_buf(),
            source: source.to_string(),
            enabled: true,
            version: manifest.plugin.version.clone(),
        },
    );
    registry.save(paths)
}

pub fn set_enabled(paths: &Paths, id: &str, enabled: bool) -> Result<()> {
    let mut registry = Registry::load(paths)?;
    let entry = registry
        .plugins
        .get_mut(id)
        .with_context(|| format!("plugin `{id}` is not installed"))?;
    entry.enabled = enabled;
    registry.save(paths)
}

/// Remove a plugin's registry entry and (for managed installs) its
/// checkout under the plugins root. Linked plugins keep their directory —
/// it is the developer's working copy, not ours.
pub fn uninstall(paths: &Paths, id: &str) -> Result<RegistryEntry> {
    let mut registry = Registry::load(paths)?;
    let entry = registry
        .plugins
        .remove(id)
        .with_context(|| format!("plugin `{id}` is not installed"))?;
    registry.save(paths)?;
    if entry.source != "link" {
        let checkout = plugins_root(paths).join(id);
        if checkout.exists() {
            std::fs::remove_dir_all(&checkout)
                .with_context(|| format!("remove {}", checkout.display()))?;
        }
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths(tmp: &Path) -> Paths {
        Paths {
            config_dir: tmp.join("config"),
            state_dir: tmp.join("state"),
            data_dir: tmp.join("data"),
            runtime_dir: tmp.join("run"),
        }
    }

    const FULL_MANIFEST: &str = r#"
        [plugin]
        id = "diff-review"
        name = "Diff Review"
        version = "0.3.0"
        min_construct_version = "0.16.0"
        description = "Rich diff review workflow"
        platforms = ["macos", "linux"]

        [[build]]
        command = ["cargo", "build", "--release"]

        [[adapters]]
        name = "reviewer"
        binary = "target/release/reviewer-adapter"
        description = "Headless review harness"

        [verbs]
        dir = "verbs"

        [templates]
        dir = "templates"

        [[mcp_servers]]
        name = "review"
        command = ["target/release/review-mcp", "serve"]
    "#;

    fn write_plugin(root: &Path, manifest: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join(MANIFEST_FILE), manifest).unwrap();
    }

    fn registered(paths: &Paths, root: &Path, manifest: &PluginManifest) {
        register(paths, manifest, root, "link").unwrap();
    }

    #[test]
    fn full_manifest_parses_and_validates() {
        let m: PluginManifest = toml::from_str(FULL_MANIFEST).unwrap();
        m.validate().unwrap();
        assert_eq!(m.plugin.id, "diff-review");
        assert_eq!(m.adapters.len(), 1);
        assert_eq!(m.mcp_servers.len(), 1);
        assert_eq!(m.build.len(), 1);
        assert!(m.verbs.is_some() && m.templates.is_some());
    }

    #[test]
    fn minimal_manifest_parses() {
        let m: PluginManifest = toml::from_str(
            r#"
            [plugin]
            id = "tiny"
            name = "Tiny"
            version = "1.0.0"
            min_construct_version = "0.1"
        "#,
        )
        .unwrap();
        m.validate().unwrap();
        assert!(m.adapters.is_empty() && m.build.is_empty());
    }

    #[test]
    fn id_validation_rejects_bad_ids() {
        for bad in ["", "has:colon", "has/slash", ".hidden", "sp ace"] {
            assert!(validate_id(bad).is_err(), "id `{bad}` should be rejected");
        }
        for good in ["a", "diff-review", "org.tool_2"] {
            assert!(validate_id(good).is_ok(), "id `{good}` should pass");
        }
    }

    #[test]
    fn version_gate_refuses_newer_minimum() {
        let m: PluginManifest = toml::from_str(FULL_MANIFEST).unwrap();
        assert!(m.check_compatible("0.16.1").is_ok());
        assert!(m.check_compatible("0.16.0").is_ok());
        assert!(m.check_compatible("0.15.9").is_err());
        assert!(m.check_compatible("1.0.0").is_ok());
    }

    #[test]
    fn parse_version_handles_partials_and_suffixes() {
        assert_eq!(parse_version("0.16"), Some((0, 16, 0)));
        assert_eq!(parse_version("1.0.0-rc1"), Some((1, 0, 0)));
        assert_eq!(parse_version("junk"), None);
    }

    #[test]
    fn namespacing_rules() {
        assert_eq!(harness_name("aider", "aider"), "aider");
        assert_eq!(harness_name("diff-review", "reviewer"), "diff-review:reviewer");
        assert_eq!(mcp_key("review", "review"), "review");
        assert_eq!(mcp_key("diff-review", "review"), "diff-review-review");
    }

    #[test]
    fn github_spec_parses_with_and_without_subdir() {
        let (url, repo, subdir) = parse_github_spec("owner/repo").unwrap();
        assert_eq!(url, "https://github.com/owner/repo.git");
        assert_eq!(repo, "owner/repo");
        assert_eq!(subdir, None);
        let (_, _, subdir) = parse_github_spec("owner/repo/plugins/foo").unwrap();
        assert_eq!(subdir.as_deref(), Some("plugins/foo"));
        assert!(parse_github_spec("just-a-name").is_err());
    }

    #[test]
    fn registry_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let m: PluginManifest = toml::from_str(FULL_MANIFEST).unwrap();
        register(&paths, &m, Path::new("/x/y"), "github:owner/repo").unwrap();
        let reg = Registry::load(&paths).unwrap();
        let entry = reg.plugins.get("diff-review").unwrap();
        assert_eq!(entry.root, PathBuf::from("/x/y"));
        assert!(entry.enabled);
        set_enabled(&paths, "diff-review", false).unwrap();
        assert!(!Registry::load(&paths).unwrap().plugins["diff-review"].enabled);
        let removed = uninstall(&paths, "diff-review").unwrap();
        assert_eq!(removed.source, "github:owner/repo");
        assert!(Registry::load(&paths).unwrap().plugins.is_empty());
    }

    #[test]
    fn load_skips_disabled_and_broken_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let good = tmp.path().join("good");
        write_plugin(
            &good,
            r#"
            [plugin]
            id = "good"
            name = "Good"
            version = "1.0.0"
            min_construct_version = "0.1"
        "#,
        );
        let broken = tmp.path().join("broken");
        write_plugin(&broken, "not valid toml [[");
        let disabled = tmp.path().join("disabled");
        write_plugin(
            &disabled,
            r#"
            [plugin]
            id = "disabled"
            name = "Disabled"
            version = "1.0.0"
            min_construct_version = "0.1"
        "#,
        );
        let too_new = tmp.path().join("too-new");
        write_plugin(
            &too_new,
            r#"
            [plugin]
            id = "too-new"
            name = "Too New"
            version = "1.0.0"
            min_construct_version = "999.0.0"
        "#,
        );
        let mut registry = Registry::default();
        for (id, root, enabled) in [
            ("good", &good, true),
            ("broken", &broken, true),
            ("disabled", &disabled, false),
            ("too-new", &too_new, true),
        ] {
            registry.plugins.insert(
                id.to_string(),
                RegistryEntry {
                    root: root.clone(),
                    source: "link".to_string(),
                    enabled,
                    version: "1.0.0".to_string(),
                },
            );
        }
        registry.save(&paths).unwrap();
        let set = PluginSet::load_for_version(&paths, "0.16.1");
        assert_eq!(
            set.plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["good"]
        );
    }

    #[test]
    fn apply_to_config_registers_namespaced_adapter_with_plugin_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let root = tmp.path().join("diff-review");
        write_plugin(&root, FULL_MANIFEST);
        let m = PluginManifest::load(&root).unwrap();
        registered(&paths, &root, &m);
        let set = PluginSet::load_for_version(&paths, "0.16.1");
        assert_eq!(set.plugins.len(), 1);

        let mut cfg = Config::default();
        set.apply_to_config(&mut cfg, &paths);
        let adapter = cfg
            .adapters
            .get("diff-review:reviewer")
            .expect("plugin adapter registered");
        let bin = adapter.binary.as_deref().unwrap();
        assert!(
            bin.ends_with("target/release/reviewer-adapter") && PathBuf::from(bin).is_absolute(),
            "relative binary resolves against the plugin root, got {bin}"
        );
        assert_eq!(adapter.env.get(ENV_PLUGIN_ID).map(String::as_str), Some("diff-review"));
        assert!(adapter.env.contains_key(ENV_PLUGIN_ROOT));
        // Plugin config/state dirs are created eagerly.
        assert!(paths.config_dir.join("plugins/diff-review").is_dir());
        assert!(paths.state_dir.join("plugins/diff-review").is_dir());
        // Every adapter (here: just the plugin's own) carries the MCP env.
        assert!(adapter.env.contains_key(PLUGIN_MCP_SERVERS_ENV));
    }

    #[test]
    fn apply_to_config_lets_user_config_override_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let root = tmp.path().join("diff-review");
        write_plugin(&root, FULL_MANIFEST);
        let m = PluginManifest::load(&root).unwrap();
        registered(&paths, &root, &m);
        let set = PluginSet::load_for_version(&paths, "0.16.1");

        let mut cfg: Config = toml::from_str(
            r#"
            [adapters."diff-review:reviewer"]
            binary = "/custom/reviewer"
        "#,
        )
        .unwrap();
        set.apply_to_config(&mut cfg, &paths);
        let adapter = &cfg.adapters["diff-review:reviewer"];
        assert_eq!(adapter.binary.as_deref(), Some("/custom/reviewer"));
        // Missing fields are still filled from the plugin declaration.
        assert_eq!(adapter.description.as_deref(), Some("Headless review harness"));
    }

    #[test]
    fn mcp_servers_json_resolves_command_and_merges_env() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let root = tmp.path().join("diff-review");
        write_plugin(&root, FULL_MANIFEST);
        let m = PluginManifest::load(&root).unwrap();
        registered(&paths, &root, &m);
        let set = PluginSet::load_for_version(&paths, "0.16.1");
        let json = set.mcp_servers_json(&paths).expect("one server");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "diff-review-review");
        let cmd = parsed[0]["command"].as_str().unwrap();
        assert!(cmd.ends_with("target/release/review-mcp") && PathBuf::from(cmd).is_absolute());
        assert_eq!(parsed[0]["args"], serde_json::json!(["serve"]));
        assert_eq!(parsed[0]["env"][ENV_PLUGIN_ID], "diff-review");
    }

    #[test]
    fn dir_sections_only_report_existing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = tmp_paths(tmp.path());
        let root = tmp.path().join("diff-review");
        write_plugin(&root, FULL_MANIFEST);
        std::fs::create_dir_all(root.join("verbs")).unwrap();
        // templates dir deliberately missing
        let m = PluginManifest::load(&root).unwrap();
        registered(&paths, &root, &m);
        let set = PluginSet::load_for_version(&paths, "0.16.1");
        let verbs = set.verb_dirs();
        assert_eq!(verbs.len(), 1);
        assert_eq!(verbs[0].0, "diff-review");
        assert!(set.template_dirs().is_empty());
    }
}
