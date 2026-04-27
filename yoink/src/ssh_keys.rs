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

    #[test]
    fn prepare_returns_none_when_no_host_declares_secret() {
        let cfg = config_with_hosts(vec![HostConfig {
            address: "h1".into(),
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
                user: "deploy".into(),
                ssh_key_secret: Some("h1_key".into()),
            },
            HostConfig {
                address: "h2".into(),
                user: "deploy".into(),
                ssh_key_secret: None,
            },
        ]);
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "h1_key".to_string(),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----".to_string(),
        );
        let bundle = SecretsBundle::new(values);
        let mgr = prepare(&cfg, Some(&bundle)).expect("prepare ok").expect("some");
        let h1 = Host {
            address: "h1".into(),
            user: "deploy".into(),
        };
        let h2 = Host {
            address: "h2".into(),
            user: "deploy".into(),
        };
        assert!(mgr.key_for(&h1).is_some());
        assert!(mgr.key_for(&h2).is_none());
        // Round-trip the bytes.
        let written = std::fs::read_to_string(mgr.key_for(&h1).unwrap()).unwrap();
        assert!(written.contains("BEGIN OPENSSH PRIVATE KEY"));
    }
}
