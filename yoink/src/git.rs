//! Git interaction. Shells out to the local `git` binary for the repo's
//! short SHA and dirty-tree detection. Pure parsing helpers are split out
//! for testability.

use std::path::Path;
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git command failed (status {status:?}): {stderr}")]
    Failed { status: Option<i32>, stderr: String },
    #[error("dirty working tree (use --allow-dirty to override)")]
    Dirty,
    #[error("could not parse git output: {0}")]
    Parse(String),
}

/// Returns the current commit's short SHA — 7 hex chars by convention.
/// Errors if not in a git repo or git isn't on PATH.
pub fn current_short_sha(repo: &Path) -> Result<String, GitError> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        return Err(GitError::Failed {
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_short_sha(&stdout)
}

/// Returns true if there are uncommitted changes (staged or unstaged).
pub fn is_dirty(repo: &Path) -> Result<bool, GitError> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        return Err(GitError::Failed {
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(is_dirty_from_status(&stdout))
}

/// Resolve the version yoink should deploy. By default, the current short
/// SHA. If the tree is dirty and `allow_dirty` is false, errors loudly.
pub fn version(repo: &Path, allow_dirty: bool) -> Result<String, GitError> {
    if !allow_dirty && is_dirty(repo)? {
        return Err(GitError::Dirty);
    }
    current_short_sha(repo)
}

/// Parse `git rev-parse --short=7 HEAD` output. Trims trailing newline,
/// validates length and hex.
pub fn parse_short_sha(stdout: &str) -> Result<String, GitError> {
    let trimmed = stdout.trim();
    if trimmed.len() < 7 {
        return Err(GitError::Parse(format!(
            "expected at least 7 hex chars, got {trimmed:?}"
        )));
    }
    if !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(GitError::Parse(format!(
            "expected hex SHA, got {trimmed:?}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Parse `git status --porcelain` output. Empty (after trim) means clean.
#[must_use]
pub fn is_dirty_from_status(stdout: &str) -> bool {
    !stdout.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_short_sha_trims_newline() {
        assert_eq!(parse_short_sha("a1b2c3d\n").unwrap(), "a1b2c3d");
    }

    #[test]
    fn parse_short_sha_accepts_longer_hash() {
        assert_eq!(parse_short_sha("a1b2c3d4e5f6\n").unwrap(), "a1b2c3d4e5f6");
    }

    #[test]
    fn parse_short_sha_rejects_too_short() {
        let err = parse_short_sha("abc").unwrap_err();
        assert!(matches!(err, GitError::Parse(_)));
    }

    #[test]
    fn parse_short_sha_rejects_non_hex() {
        let err = parse_short_sha("xyzzz12").unwrap_err();
        assert!(matches!(err, GitError::Parse(_)));
    }

    #[test]
    fn parse_short_sha_rejects_empty() {
        let err = parse_short_sha("\n").unwrap_err();
        assert!(matches!(err, GitError::Parse(_)));
    }

    #[test]
    fn is_dirty_from_status_true_when_modified() {
        assert!(is_dirty_from_status(" M src/main.rs\n"));
    }

    #[test]
    fn is_dirty_from_status_true_when_untracked() {
        assert!(is_dirty_from_status("?? new-file.txt\n"));
    }

    #[test]
    fn is_dirty_from_status_false_when_clean() {
        assert!(!is_dirty_from_status(""));
        assert!(!is_dirty_from_status("\n"));
        assert!(!is_dirty_from_status("   \n"));
    }
}
