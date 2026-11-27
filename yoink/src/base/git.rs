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

/// What `commits_behind_upstream_for_path` returns when there's
/// drift to warn about. `upstream` is the branch ref `git` reports
/// (typically `origin/<branch>`); `commits_behind` is the count of
/// upstream commits that touch the path but aren't in HEAD yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamLag {
    pub upstream: String,
    pub commits_behind: u32,
}

/// Best-effort: report whether the file at `path` has commits on its
/// branch's upstream tracking ref that aren't yet in HEAD. Returns
/// `None` on any failure mode that isn't actionable from the operator's
/// seat — not a git repo, no upstream tracking branch, detached HEAD,
/// `git` binary missing, etc. We deliberately do **not** `git fetch`
/// (it's slow, may prompt for creds, and may have privacy implications);
/// the lag is measured against whatever the caller's most recent fetch
/// produced, which is typically what they want.
///
/// Path-scoped: only counts commits whose diff actually touches `path`.
/// A config file that hasn't changed upstream produces `None` even if
/// the branch as a whole is behind.
#[must_use]
pub fn commits_behind_upstream_for_path(path: &Path) -> Option<UpstreamLag> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let basename = path.file_name()?.to_str()?;
    let upstream = run_git_string(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])?;
    // Pass the file name as a workdir-relative path so the filter
    // matches the repo's tree regardless of how `path` was rendered
    // by the caller (canonicalize-via-symlinks on macOS would have
    // produced `/private/tmp/...` which `rev-list -- <path>` won't
    // resolve against the repo at `/tmp/...`).
    let raw = run_git_string(
        dir,
        &["rev-list", "--count", "HEAD..@{upstream}", "--", basename],
    )?;
    let commits_behind: u32 = raw.parse().ok()?;
    if commits_behind == 0 {
        return None;
    }
    Some(UpstreamLag {
        upstream,
        commits_behind,
    })
}

/// Run `git <args>` in `dir` and return stdout trimmed. Returns
/// `None` if the command fails or stdout is empty — used for
/// best-effort probes where the caller doesn't want a structured
/// error.
fn run_git_string(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
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

    #[test]
    fn upstream_lag_none_outside_git_repo() {
        // tempfile in /tmp lives outside any git repo (the operator's
        // homedir is the closest one and macOS /tmp is its own
        // filesystem on most setups). Even when that's not true, the
        // file we check is freshly-created so it has no upstream
        // history — `rev-list --count … -- <path>` returns 0.
        let dir = tempfile::tempdir_in("/tmp").expect("tempdir");
        let path = dir.path().join("yoink.yaml");
        std::fs::write(&path, "hello: world\n").unwrap();
        assert_eq!(commits_behind_upstream_for_path(&path), None);
    }

    #[test]
    fn upstream_lag_detects_committed_upstream_change() {
        // Build a tiny "remote" repo, clone it, commit a config file
        // change on the remote, fetch on the clone. The clone's HEAD
        // is now behind `origin/main` for that path, which is exactly
        // what `commits_behind_upstream_for_path` is meant to catch.
        let scratch = tempfile::tempdir_in("/tmp").expect("tempdir");
        let remote = scratch.path().join("remote.git");
        let work = scratch.path().join("work");
        let edit = scratch.path().join("edit");

        // Bare upstream pinned to `main` so the test isn't sensitive
        // to the host's `init.defaultBranch` (older git defaults to
        // `master`, newer defaults vary). The bare repo has no HEAD
        // commit yet, so we can't `clone -b main` it — first do a
        // throwaway clone, push the initial commit, then clone for
        // real for the operator-side workdir.
        run_or_skip(
            scratch.path(),
            &["init", "--bare", "-b", "main", "remote.git"],
        );
        run_or_skip(scratch.path(), &["clone", remote.to_str().unwrap(), "edit"]);
        run_git_str(&edit, &["config", "user.email", "test@example.invalid"]);
        run_git_str(&edit, &["config", "user.name", "Test"]);
        run_git_str(&edit, &["checkout", "-b", "main"]);
        std::fs::write(edit.join("yoink.yaml"), "v: 1\n").unwrap();
        run_git_str(&edit, &["add", "yoink.yaml"]);
        run_git_str(&edit, &["commit", "-m", "init"]);
        run_git_str(&edit, &["push", "-u", "origin", "main"]);

        // Operator's clone — at the initial commit only.
        run_or_skip(scratch.path(), &["clone", remote.to_str().unwrap(), "work"]);
        let local_path = work.join("yoink.yaml");

        // Upstream lands a change to the config file.
        std::fs::write(edit.join("yoink.yaml"), "v: 2\n").unwrap();
        run_git_str(&edit, &["commit", "-am", "bump"]);
        run_git_str(&edit, &["push"]);

        // Operator hasn't pulled but has run `git fetch`.
        run_git_str(&work, &["fetch"]);

        let lag = commits_behind_upstream_for_path(&local_path).expect("expected upstream lag");
        assert_eq!(lag.commits_behind, 1);
        assert!(
            lag.upstream.contains("main"),
            "upstream label should reference main: {:?}",
            lag.upstream
        );
    }

    /// `git` invocation helper for tests. Panics on failure so a
    /// broken precondition surfaces immediately rather than as a
    /// silent skip.
    fn run_git_str(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Like `run_git_str` but also tolerates `git` not being
    /// installed (skip the whole test) — only used for the *first*
    /// command since later ones are guaranteed to find git too.
    fn run_or_skip(dir: &Path, args: &[&str]) {
        let out = Command::new("git").args(args).current_dir(dir).output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => panic!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => panic!("git not installed (skipping): {e}"),
        }
    }
}
