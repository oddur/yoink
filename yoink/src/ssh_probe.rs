//! Pre-bollard ssh connectivity probe with operator-readable error
//! classification.
//!
//! Bollard's SSH transport spawns its own `ssh` client and swallows
//! stderr — so when ssh prompts for an interactive auth check (e.g.
//! Tailscale's "additional check required"), the connection just
//! hangs with no UI feedback. Running ssh ourselves first lets us
//! catch and surface those patterns before falling through to
//! bollard.
//!
//! Used by `yoink preflight` today; TUI Hosts pane is a candidate
//! consumer for the same flow.

use crate::docker_ops::Host;

/// Probe the SSH connection. Returns `Ok(())` when `ssh user@host
/// true` succeeds with `BatchMode=yes` (no interactive prompts);
/// otherwise an operator-readable error string with both a
/// classified hint and the raw stderr.
pub async fn probe(host: &Host) -> Result<(), String> {
    let target = format!("{}@{}", host.user, host.address);
    let output = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
            "true",
        ])
        .output()
        .await
        .map_err(|e| format!("could not invoke ssh: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let translated = classify(&stderr);
    Err(format!(
        "ssh probe failed: {translated}\n\nraw ssh stderr:\n{}",
        stderr.trim()
    ))
}

/// Pattern-match ssh stderr against the failure modes operators hit
/// most often, returning a one-line actionable hint.
#[must_use]
pub fn classify(stderr: &str) -> String {
    if let Some(url) = extract_tailscale_check_url(stderr) {
        return format!(
            "Tailscale SSH requires an additional check — open this URL in a browser, then re-run:\n  {url}"
        );
    }
    if stderr.contains("Permission denied") {
        return "permission denied — check the ssh key (`ssh-add -l`) and that the deploy user is configured on the host".into();
    }
    if stderr.contains("Connection timed out") || stderr.contains("Operation timed out") {
        return "connection timed out — host unreachable. If on Tailscale, run `tailscale up` and check `tailscale status`".into();
    }
    if stderr.contains("Host key verification failed") {
        return "host key verification failed — the host's key changed. Inspect ~/.ssh/known_hosts and re-add if expected".into();
    }
    if stderr.contains("Could not resolve hostname")
        || stderr.contains("Name or service not known")
    {
        return "could not resolve hostname — typo in `address:`, or DNS / Tailscale magicDNS not reachable".into();
    }
    if stderr.contains("Connection refused") {
        return "connection refused — sshd not listening on the target port".into();
    }
    "ssh failed (see raw stderr below for details)".into()
}

/// Pull the Tailscale auth URL out of stderr if present.
/// Tailscale SSH's check-mode prints `# To authenticate, visit:
/// <https://login.tailscale.com/a/TOKEN>` — note the leading `# `.
#[must_use]
pub fn extract_tailscale_check_url(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        let trimmed = line.trim().trim_start_matches('#').trim();
        if let Some(rest) = trimmed.strip_prefix("To authenticate, visit:") {
            let url = rest.trim();
            if url.starts_with("https://login.tailscale.com/") {
                return Some(url.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_tailscale_check_surfaces_url() {
        let stderr = "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/abc123\n";
        let msg = classify(stderr);
        assert!(msg.contains("Tailscale SSH requires an additional check"));
        assert!(msg.contains("https://login.tailscale.com/a/abc123"));
    }

    #[test]
    fn classify_permission_denied_points_at_ssh_key() {
        let msg = classify("Permission denied (publickey).\r\n");
        assert!(msg.contains("permission denied"));
        assert!(msg.contains("ssh-add"));
    }

    #[test]
    fn classify_timeout_mentions_tailscale_up() {
        let msg = classify("ssh: connect to host x port 22: Operation timed out\n");
        assert!(msg.contains("connection timed out"));
        assert!(msg.contains("tailscale up"));
    }

    #[test]
    fn classify_unknown_falls_back_to_generic() {
        let msg = classify("ssh: something weird went wrong\n");
        assert!(msg.contains("ssh failed"));
    }

    #[test]
    fn extract_tailscale_url_picks_login_tailscale_only() {
        let s = "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/zzz\n";
        assert_eq!(
            extract_tailscale_check_url(s).as_deref(),
            Some("https://login.tailscale.com/a/zzz")
        );
        // Non-tailscale URL — refuse to surface (don't help phishing).
        let s2 = "# To authenticate, visit: https://evil.example.com/a/zzz\n";
        assert_eq!(extract_tailscale_check_url(s2), None);
    }

    #[test]
    fn extract_tailscale_url_handles_user_reported_format() {
        // Exact text the operator pasted (including the # prefix
        // Tailscale uses).
        let s = "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/l4ea83873a21a2\n";
        assert_eq!(
            extract_tailscale_check_url(s).as_deref(),
            Some("https://login.tailscale.com/a/l4ea83873a21a2")
        );
    }
}
