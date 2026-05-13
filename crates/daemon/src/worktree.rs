//! Thin wrappers around `git worktree` and `git diff` for session isolation.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Check whether `dir` looks like a git repo or working tree.
pub async fn is_git_repo(dir: &Path) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    matches!(out, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
}

/// Create a fresh worktree of `repo_dir` at `worktree_dir` on a new branch.
pub async fn create_worktree(
    repo_dir: &Path,
    worktree_dir: &Path,
    branch: &str,
) -> Result<PathBuf> {
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // base off HEAD of the source repo
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(branch)
        .arg(worktree_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn git worktree add")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(worktree_dir.to_path_buf())
}

/// Remove a worktree that was created with [`create_worktree`]. Tries
/// `git worktree remove` first (so the linked repo's worktree registry is
/// kept clean); falls back to a plain recursive delete if git can't help.
/// Idempotent: missing directory is not an error.
pub async fn remove_worktree(worktree_dir: &Path) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }
    let attempted = Command::new("git")
        .arg("-C")
        .arg(worktree_dir)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if matches!(&attempted, Ok(s) if s.success()) && !worktree_dir.exists() {
        return Ok(());
    }
    // Fallback — git refused (e.g. the worktree drifted, the linked repo is
    // gone, etc.). Drop the directory directly. The linked repo will still
    // know about a stale worktree entry; users can `git worktree prune` later.
    if worktree_dir.exists() {
        tokio::fs::remove_dir_all(worktree_dir)
            .await
            .context("remove_dir_all worktree")?;
    }
    Ok(())
}

/// `git diff HEAD` against the worktree, returning the patch as a string.
pub async fn diff_worktree(work_dir: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .arg("--no-pager")
        .arg("diff")
        .arg("HEAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn git diff")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
