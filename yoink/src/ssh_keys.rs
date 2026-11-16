//! Per-host SSH keyfile manager for `hosts[].ssh_key_secret:`.
//!
//! When a host declares an `ssh_key_secret:`, yoink decrypts the named
//! sealed-secret value (PEM-formatted SSH private key) to a tempfile
//! with mode `0o600` for the duration of the deploy. Bollard's
//! `connect_with_ssh` takes a keypair path natively (via the
//! `openssh` crate); `ssh_probe` adds `-i <path>` to its `ssh true`
//! probe. Either way, the operator's personal ssh-agent is untouched.
//!
//! On Drop, the tempfiles are removed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use tempfile::NamedTempFile;

use crate::config::Config;
use crate::docker_ops::Host;
use crate::secrets::SecretsBundle;

/// Holds tempfiles for every host that declared an `ssh_key_secret:`.
/// Keyed by host `address` (the same address bollard's ssh URL uses).
/// Drop removes the tempfiles.
#[derive(Debug)]
pub struct KeyManager {
    by_address: BTreeMap<String, NamedTempFile>,
}

impl KeyManager {
    /// Tempfile path for `host`'s key, or `None` if this host doesn't
    /// declare a `ssh_key_secret`. Pass into bollard's
    /// `connect_with_ssh` as `keypair_path` and into `ssh_probe::probe`
    /// as the `-i` arg.
    #[must_use]
    pub fn key_for(&self, host: &Host) -> Option<&Path> {
        self.by_address.get(&host.address).map(NamedTempFile::path)
    }
}

/// Build the subprocess that runs `script` on `host`'s shell, for the
/// call sites that shell out to `ssh` instead of going through bollard
/// (`files::upload`, the host audit sink). The `local` sentinel host
/// has nothing to ssh to, so it runs `sh -c <script>` on the operator
/// machine. Every other host runs `ssh -o BatchMode=yes [-i keyfile]
/// [-p port] user@address <script>`; `keyfile` is the path from
/// `DockerOps::ssh_keyfile` (`KeyManager::key_for`), so the shell-out
/// authenticates with the same `ssh_key_secret:` key as the bollard
/// connection and `ssh_probe`. Stdio is left for the caller to set.
#[must_use]
pub fn host_shell_command(
    host: &Host,
    keyfile: Option<&Path>,
    script: &str,
) -> tokio::process::Command {
    if host.is_local() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        return cmd;
    }
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-o").arg("BatchMode=yes");
    if let Some(key) = keyfile {
        // `IdentitiesOnly=yes` stops ssh from trying every agent key
        // first (which can exhaust the server's auth attempts before
        // it reaches `-i`).
        cmd.arg("-o").arg("IdentitiesOnly=yes").arg("-i").arg(key);
    }
    // `[-p <port>] user@address`, so a host with `port:` is reached
    // the same way as by every other ssh shell-out.
    cmd.args(host.ssh_args()).arg(script);
    cmd
}

/// Walk `config.hosts`, decrypt every named `ssh_key_secret` from the
/// bundle into a 0600 tempfile, and return a manager. The manager
/// holds the tempfiles alive for the duration of the deploy.
///
/// `Ok(None)` when no host declares `ssh_key_secret` — the common
/// path; the operator's existing ssh auth keeps working.
pub fn prepare(config: &Config, bundle: Option<&SecretsBundle>) -> Result<Option<KeyManager>> {
    let mut by_address: BTreeMap<String, NamedTempFile> = BTreeMap::new();
    for host in &config.hosts {
        let Some(secret_name) = host.ssh_key_secret.as_deref() else {
            continue;
        };
        let bundle = bundle.ok_or_else(|| {
            anyhow::anyhow!(
                "host `{}` declares ssh_key_secret={secret_name:?} but no [secrets] block is configured",
                host.address,
            )
        })?;
        let key_pem = bundle.get(secret_name).ok_or_else(|| {
            anyhow::anyhow!(
                "host `{}` declares ssh_key_secret={secret_name:?} but the secrets bundle has no such key",
                host.address,
            )
        })?;
        let tempfile = write_keyfile(secret_name, key_pem)?;
        by_address.insert(host.address.clone(), tempfile);
    }
    if by_address.is_empty() {
        return Ok(None);
    }
    Ok(Some(KeyManager { by_address }))
}

/// Write `key_pem` to a `0o600` tempfile in the OS temp dir. The
/// `NamedTempFile` returned is the lifetime owner — the file is
/// removed on Drop.
fn write_keyfile(name: &str, key_pem: &str) -> Result<NamedTempFile> {
    let tempfile = tempfile::Builder::new()
        .prefix(&format!("yoink-key-{name}-"))
        .suffix(".pem")
        .tempfile()
        .context("creating tempfile for ssh private key")?;
    let path: PathBuf = tempfile.path().to_path_buf();
    write_with_mode(&path, key_pem)?;
    Ok(tempfile)
}

#[cfg(unix)]
fn write_with_mode(path: &Path, key_pem: &str) -> Result<()> {
    // Re-open with explicit `0o600` — ssh refuses world-/group-readable
    // private keys, and tempfile's default mode varies by platform.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .context("setting 0600 mode on key tempfile")?;
    let bytes = key_pem.as_bytes();
    file.write_all(bytes).context("writing key bytes")?;
    if !bytes.ends_with(b"\n") {
        file.write_all(b"\n").context("writing trailing newline")?;
    }
    file.sync_all().context("syncing key tempfile")?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_mode(path: &Path, key_pem: &str) -> Result<()> {
    // Windows: file ACLs aren't a unix-mode story; rely on the
    // per-user temp dir's default ACL. This whole feature is
    // unix-leaning anyway (target hosts are Linux) so the platform
    // mismatch is acceptable.
    let mut file = std::fs::File::create(path).context("creating key tempfile")?;
    let bytes = key_pem.as_bytes();
    file.write_all(bytes).context("writing key bytes")?;
    if !bytes.ends_with(b"\n") {
        file.write_all(b"\n").context("writing trailing newline")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;

    fn config_with_hosts(hosts: Vec<HostConfig>) -> Config {
        // Round-trip a minimal valid config; hosts are patched in
        // after so we don't depend on Config impl-ing Default.
        let mut cfg = Config::parse_str(
            r#"
deploy:
  networks: [n]
hosts:
  - { address: placeholder, user: deploy }
services:
  - name: svc
    image: img
    tag: t
    networks: [n]
    run: { port: 8080 }
"#,
        )
        .expect("minimal config parses");
        cfg.hosts = hosts;
        cfg
    }

    fn argv(cmd: &tokio::process::Command) -> Vec<String> {
        let std_cmd = cmd.as_std();
        std::iter::once(std_cmd.get_program())
            .chain(std_cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn remote_host() -> Host {
        Host {
            user: "deploy".into(),
            address: "h1".into(),
            port: None,
        }
    }

    #[test]
    fn host_shell_command_passes_both_the_keyfile_and_the_port() {
        // `files::upload` and the host audit sink on a host with both
        // `ssh_key_secret:` and `port:`.
        let host = Host {
            port: Some(2200),
            ..remote_host()
        };
        let cmd = host_shell_command(&host, Some(Path::new("/tmp/yoink-key-h1.pem")), "true");
        assert_eq!(
            argv(&cmd),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
                "/tmp/yoink-key-h1.pem",
                "-p",
                "2200",
                "deploy@h1",
                "true",
            ],
        );
    }

    #[test]
    fn host_shell_command_passes_the_host_keyfile_to_ssh() {
        // Regression: `files::upload` and the host audit sink ran a
        // bare `ssh user@addr`, so a host whose only credential is
        // `ssh_key_secret:` got "Permission denied (publickey)".
        let cmd = host_shell_command(
            &remote_host(),
            Some(Path::new("/tmp/yoink-key-h1.pem")),
            "true",
        );
        assert_eq!(
            argv(&cmd),
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-i",
                "/tmp/yoink-key-h1.pem",
                "deploy@h1",
                "true",
            ],
        );
    }

    #[test]
    fn host_shell_command_without_keyfile_uses_operator_ssh_auth() {
        let cmd = host_shell_command(&remote_host(), None, "true");
        assert_eq!(
            argv(&cmd),
            ["ssh", "-o", "BatchMode=yes", "deploy@h1", "true"]
        );
    }

    #[test]
    fn host_shell_command_runs_local_host_without_ssh() {
        // Regression: `address: local` used to run `ssh @local ...`,
        // which fails with "Could not resolve hostname local".
        let cmd = host_shell_command(&Host::local(), None, "true");
        assert_eq!(argv(&cmd), ["sh", "-c", "true"]);
    }

    #[test]
    fn prepare_returns_none_when_no_host_declares_secret() {
        let cfg = config_with_hosts(vec![HostConfig {
            address: "h1".into(),
            port: None,
            address_secret: None,
            user: "deploy".into(),
            ssh_key_secret: None,
        }]);
        let mgr = prepare(&cfg, None).expect("prepare ok");
        assert!(mgr.is_none());
    }

    #[test]
    fn prepare_errors_when_secret_missing_from_bundle() {
        let cfg = config_with_hosts(vec![HostConfig {
            address: "h1".into(),
            port: None,
            address_secret: None,
            user: "deploy".into(),
            ssh_key_secret: Some("h1_key".into()),
        }]);
        let bundle = SecretsBundle::default();
        let err = prepare(&cfg, Some(&bundle)).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("h1_key") && msg.contains("secrets bundle has no such key"),
            "unexpected msg: {msg}",
        );
    }

    #[test]
    fn prepare_errors_when_no_secrets_block() {
        let cfg = config_with_hosts(vec![HostConfig {
            address: "h1".into(),
            port: None,
            address_secret: None,
            user: "deploy".into(),
            ssh_key_secret: Some("h1_key".into()),
        }]);
        let err = prepare(&cfg, None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no [secrets] block"), "unexpected msg: {msg}");
    }

    #[test]
    fn prepare_writes_keyfile_for_declared_host() {
        let cfg = config_with_hosts(vec![
            HostConfig {
                address: "h1".into(),
                port: None,
                address_secret: None,
                user: "deploy".into(),
                ssh_key_secret: Some("h1_key".into()),
            },
            HostConfig {
                address: "h2".into(),
                port: None,
                address_secret: None,
                user: "deploy".into(),
                ssh_key_secret: None,
            },
        ]);
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "h1_key".to_string(),
            zeroize::Zeroizing::new(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                    .to_string(),
            ),
        );
        let bundle = SecretsBundle::new(values);
        let mgr = prepare(&cfg, Some(&bundle))
            .expect("prepare ok")
            .expect("some");
        let h1 = Host {
            address: "h1".into(),
            port: None,
            user: "deploy".into(),
        };
        let h2 = Host {
            address: "h2".into(),
            port: None,
            user: "deploy".into(),
        };
        assert!(mgr.key_for(&h1).is_some());
        assert!(mgr.key_for(&h2).is_none());
        // Round-trip the bytes.
        let written = std::fs::read_to_string(mgr.key_for(&h1).unwrap()).unwrap();
        assert!(written.contains("BEGIN OPENSSH PRIVATE KEY"));
    }
}
