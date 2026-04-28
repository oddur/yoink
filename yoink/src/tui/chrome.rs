//! Helpers for the TUI top chrome: detect when the loaded config
//! lives in a git repo and whether it's dirty, so the header can
//! show "<repo>" / "<repo>(*)" — answering "which version of the
//! config am I looking at" without leaving the TUI.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Display name for the git repo containing the config file (when one
/// exists), with `(*)` appended if the config file has uncommitted
/// edits relative to HEAD. `None` when the file isn't in a git
/// working tree, or `git` isn't on PATH.
#[must_use]
pub fn config_source(config_path: &Path) -> Option<String> {
    let workdir = config_path.parent()?;
    let repo_root_raw = run_git(workdir, &["rev-parse", "--show-toplevel"])?;
    let repo_root = PathBuf::from(repo_root_raw.trim());
    let name = repo_root
        .file_name()
        .map_or_else(|| "git".into(), |s| s.to_string_lossy().into_owned());
    // Limit the dirty check to the config file itself — operators
    // almost always have unrelated work-in-progress in the same repo
    // and we only care whether *this config* differs from HEAD.
    let status = run_git(
        &repo_root,
        &[OsStr::new("status"), OsStr::new("--porcelain"), OsStr::new("--"), config_path.as_os_str()],
    );
    if status
        .as_deref()
        .is_some_and(crate::git::is_dirty_from_status)
    {
        Some(format!("{name}(*)"))
    } else {
        Some(name)
    }
}

fn run_git<S: AsRef<OsStr>>(workdir: &Path, args: &[S]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}
