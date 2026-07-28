//! `construct plugin` subcommands (spec 0152): install, link, list,
//! enable/disable, uninstall. These mutate the on-disk plugin registry only;
//! the daemon reads it at startup, so every mutation ends with a restart
//! hint (`construct daemon restart` preserves sessions).

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use construct_daemon::plugins::{self, PluginManifest, Registry};
use construct_protocol::paths::Paths;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Install a plugin from GitHub: `owner/repo[/subdir]`. Clones the
    /// repo, shows what the plugin contributes, asks for consent, runs its
    /// build steps, and registers it.
    Install {
        /// `owner/repo` or `owner/repo/subdir`.
        spec: String,
        /// Git ref (branch, tag, or commit) to install; default branch
        /// otherwise. Pin this for supply-chain hygiene.
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Skip the consent prompt (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Register a local plugin directory for development, without cloning
    /// or running build steps.
    Link {
        dir: PathBuf,
        /// Skip the consent prompt (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// List installed plugins and their status.
    List,
    /// Enable an installed plugin.
    Enable { id: String },
    /// Disable an installed plugin without removing it.
    Disable { id: String },
    /// Remove a plugin's registration (and its managed checkout for GitHub
    /// installs; linked directories are left alone).
    Uninstall { id: String },
}

pub fn run(cmd: PluginCommand) -> Result<()> {
    let paths = Paths::discover();
    match cmd {
        PluginCommand::Install { spec, git_ref, yes } => install(&paths, &spec, git_ref, yes),
        PluginCommand::Link { dir, yes } => link(&paths, dir, yes),
        PluginCommand::List => list(&paths),
        PluginCommand::Enable { id } => {
            plugins::set_enabled(&paths, &id, true)?;
            println!("enabled plugin `{id}`");
            restart_hint();
            Ok(())
        }
        PluginCommand::Disable { id } => {
            plugins::set_enabled(&paths, &id, false)?;
            println!("disabled plugin `{id}`");
            restart_hint();
            Ok(())
        }
        PluginCommand::Uninstall { id } => {
            let entry = plugins::uninstall(&paths, &id)?;
            println!("uninstalled plugin `{id}` (was {})", entry.source);
            restart_hint();
            Ok(())
        }
    }
}

fn install(paths: &Paths, spec: &str, git_ref: Option<String>, yes: bool) -> Result<()> {
    let (url, repo, subdir) = plugins::parse_github_spec(spec)?;
    let tmp = plugins::clone_to_temp(paths, &url, git_ref.as_deref())?;
    // Everything between clone and finalize cleans up the temp checkout on
    // failure, so an aborted install leaves nothing behind.
    let result = (|| -> Result<(PluginManifest, PathBuf)> {
        let manifest_root = match subdir.as_deref() {
            Some(s) => tmp.join(s),
            None => tmp.clone(),
        };
        let manifest = PluginManifest::load(&manifest_root)?;
        manifest.check_compatible(env!("CARGO_PKG_VERSION"))?;
        confirm(&manifest, yes)?;
        let root = plugins::finalize_checkout(paths, &tmp, &manifest.plugin.id, subdir.as_deref())?;
        Ok((manifest, root))
    })();
    let (manifest, root) = match result {
        Ok(v) => v,
        Err(e) => {
            std::fs::remove_dir_all(&tmp).ok();
            return Err(e);
        }
    };
    if let Err(e) = plugins::run_build_steps(&root, &manifest) {
        // A failed build removes the managed checkout so the install is
        // all-or-nothing.
        std::fs::remove_dir_all(plugins::plugins_root(paths).join(&manifest.plugin.id)).ok();
        return Err(e).context("plugin build failed; install rolled back");
    }
    plugins::register(paths, &manifest, &root, &format!("github:{repo}"))?;
    println!(
        "installed plugin `{}` v{} from {repo}",
        manifest.plugin.id, manifest.plugin.version
    );
    restart_hint();
    Ok(())
}

fn link(paths: &Paths, dir: PathBuf, yes: bool) -> Result<()> {
    let dir = std::fs::canonicalize(&dir)
        .with_context(|| format!("resolve plugin dir {}", dir.display()))?;
    let manifest = PluginManifest::load(&dir)?;
    manifest.check_compatible(env!("CARGO_PKG_VERSION"))?;
    confirm(&manifest, yes)?;
    plugins::register(paths, &manifest, &dir, "link")?;
    println!(
        "linked plugin `{}` -> {}",
        manifest.plugin.id,
        dir.display()
    );
    restart_hint();
    Ok(())
}

fn list(paths: &Paths) -> Result<()> {
    let registry = Registry::load(paths)?;
    if registry.plugins.is_empty() {
        println!("(no plugins installed)");
        return Ok(());
    }
    for (id, entry) in &registry.plugins {
        let state = if entry.enabled { "enabled" } else { "disabled" };
        let status = match PluginManifest::load(&entry.root) {
            Ok(m) => match m.check_compatible(env!("CARGO_PKG_VERSION")) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("incompatible: {e}"),
            },
            Err(e) => format!("broken: {e:#}"),
        };
        println!(
            "{id:<20} v{version:<10} [{state}]  {status}\n{pad:20} {source}  {root}",
            version = entry.version,
            pad = "",
            source = entry.source,
            root = entry.root.display(),
        );
    }
    Ok(())
}

/// Show the manifest's capability summary and ask for consent. `--yes`
/// skips the prompt; a non-terminal stdin without `--yes` refuses rather
/// than silently installing.
fn confirm(manifest: &PluginManifest, yes: bool) -> Result<()> {
    println!("{}", manifest.describe());
    println!(
        "Plugin code runs as your user, with your environment, and can use the\n\
         full construct CLI/IPC surface. Review the repository before trusting it."
    );
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("stdin is not a terminal; re-run with --yes to confirm");
    }
    eprint!("Proceed? [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("aborted");
    }
    Ok(())
}

fn restart_hint() {
    println!("Restart the daemon to apply: `construct daemon restart` (sessions are preserved).");
}
