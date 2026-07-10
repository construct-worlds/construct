use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &PathBuf, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    Some(value.trim().to_string())
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let repo = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.clone());

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
    let hash = git(&repo, &["rev-parse", "--short", "HEAD"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&repo, &["status", "--porcelain"])
        .map(|s| if s.is_empty() { "" } else { "-dirty" })
        .unwrap_or("");

    println!("cargo:rerun-if-changed={}", repo.join(".git").display());
    if let Some(git_dir) = git(&repo, &["rev-parse", "--git-dir"]) {
        let git_dir = Path::new(&git_dir);
        let git_dir = if git_dir.is_absolute() {
            git_dir.to_path_buf()
        } else {
            repo.join(git_dir)
        };
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo.join("Cargo.lock").display()
    );
    println!("cargo:rerun-if-changed={}", repo.join("crates").display());
    println!("cargo:rustc-env=CONSTRUCT_BUILD_ID={version}+{hash}{dirty}");
}
