//! Suspend the TUI and jump into `$EDITOR` at the YAML line of the
//! object the operator is currently looking at. Returning from the
//! editor restores the alt-screen and triggers the next config
//! reload tick — the existing reload path picks up edits without
//! any new plumbing.
//!
//! Line-finding is intentionally a dumb line scanner, not a YAML
//! parser, so it can point at the *exact* `name:` / `address:` line
//! the user typed (a re-serialized AST would lose comments and
//! ordering). The yoink config file is line-oriented yaml with no
//! flow-style maps in the wild, so this is fine in practice.

use std::path::PathBuf;
use std::process::Command;

/// Where to drop the operator on `$EDITOR` resume.
#[derive(Debug, Clone)]
pub struct EditorTarget {
    pub path: PathBuf,
    /// 1-indexed line, or `None` to open at the top of the file.
    pub line: Option<usize>,
}

/// 1-indexed line of the `name: <service>` block in the yaml text.
/// Matches any indentation and a leading `- ` (yaml list syntax).
/// Strips matching quotes around the value so `name: "api"` and
/// `name: api` both match.
#[must_use]
pub fn find_service_line(yaml: &str, service_name: &str) -> Option<usize> {
    find_kv_line(yaml, "name", service_name)
}

/// Same idea as [`find_service_line`] but scans for `address:`.
#[must_use]
pub fn find_host_line(yaml: &str, address: &str) -> Option<usize> {
    find_kv_line(yaml, "address", address)
}

fn find_kv_line(yaml: &str, key: &str, value: &str) -> Option<usize> {
    let needle = format!("{key}:");
    for (idx, line) in yaml.lines().enumerate() {
        let trimmed = line.trim_start();
        let after_dash = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        let Some(rest) = after_dash.strip_prefix(&needle) else {
            continue;
        };
        let candidate = rest
            .trim()
            // Drop a trailing inline comment if any.
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        if candidate == value {
            return Some(idx + 1);
        }
    }
    None
}

/// Suspend the TUI, run the operator's editor on `target`, return
/// when they exit. The caller is responsible for tearing down the
/// terminal (alt-screen + raw mode) before this and re-entering
/// after — we don't touch the terminal here so the call site can
/// keep its existing setup/restore helpers.
///
/// `$EDITOR` (then `$VISUAL`, then `vi`) decides which editor to
/// run. We pass `+<line>` as the first argument when a line is
/// known — every popular terminal editor (`vi`/`vim`/`nvim`,
/// `nano`, `emacs -nw`, `helix`, `kakoune`, `micro`) treats `+N`
/// as "open at line N". Editors that don't (`code`, `subl`) are
/// the GUI ones an operator is unlikely to set as `$EDITOR` in a
/// terminal session anyway.
pub fn run_editor(target: &EditorTarget) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("EDITOR")
        .ok()
        .or_else(|| std::env::var("VISUAL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    // Allow `EDITOR="code -w"` etc. — split on whitespace so the
    // shell-style "command + flags" form works.
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or("vi");
    let mut cmd = Command::new(prog);
    for arg in parts {
        cmd.arg(arg);
    }
    if let Some(line) = target.line {
        cmd.arg(format!("+{line}"));
    }
    cmd.arg(&target.path);
    cmd.status()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
hosts:
  - address: host-a
    user: deploy
  - address: host-b
    user: deploy
services:
  - name: api
    image: ghcr.io/example/api
  - name: \"web\"
    image: ghcr.io/example/web
  - name: app-a
    image: ghcr.io/example/app-a
";

    #[test]
    fn finds_service_by_name() {
        assert_eq!(find_service_line(SAMPLE, "api"), Some(7));
    }

    #[test]
    fn finds_service_with_quoted_name() {
        assert_eq!(find_service_line(SAMPLE, "web"), Some(9));
    }

    #[test]
    fn finds_service_with_dash_in_name() {
        assert_eq!(find_service_line(SAMPLE, "app-a"), Some(11));
    }

    #[test]
    fn finds_host_by_address() {
        assert_eq!(find_host_line(SAMPLE, "host-a"), Some(2));
        assert_eq!(find_host_line(SAMPLE, "host-b"), Some(4));
    }

    #[test]
    fn missing_target_returns_none() {
        assert_eq!(find_service_line(SAMPLE, "ghost"), None);
        assert_eq!(find_host_line(SAMPLE, "nope"), None);
    }
}
