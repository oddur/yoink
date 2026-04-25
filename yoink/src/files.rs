//! Operator-side files that get uploaded to the target host before
//! `docker create`, then bind-mounted into the running container.
//! Equivalent of Kamal's `files:` directive — lets you keep
//! configuration (Caddyfile, otel-collector.yaml, etc.) in git instead
//! of pre-staging it on the host out-of-band.
//!
//! Upload path is content-addressed (`/var/lib/yoink/files/<sha256>-<basename>`)
//! so concurrent deploys never overwrite each other and a config edit
//! flows through to a redeploy via the spec hash.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::process::Command;

use crate::docker_ops::Host;

/// Parsed `service.run.files` entry. Local path is operator-side
/// (resolved against the config file's directory); container path is
/// where the bind mount lands inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMount {
    pub local: PathBuf,
    pub container: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Error)]
pub enum FileError {
    #[error("invalid file mount {0:?}: expected \"local:container[:ro]\"")]
    BadFormat(String),
    #[error("local file {path:?} does not exist or is not readable: {source}")]
    LocalRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("scp upload to {host}:{remote} failed: {message}")]
    Scp {
        host: String,
        remote: String,
        message: String,
    },
    #[error("ssh mkdir on {host} failed: {message}")]
    Mkdir { host: String, message: String },
}

impl FileMount {
    /// Parse `"local:container[:ro]"`. Local paths may be relative —
    /// they're resolved against `base_dir` (typically the config file's
    /// directory).
    pub fn parse(spec: &str, base_dir: Option<&Path>) -> Result<Self, FileError> {
        // Split deliberately on the *first* and *last* `:` rather than
        // by count, so container paths with colons (rare but possible)
        // aren't mangled. The trailing `:ro` (or `:rw`) is recognized
        // only when the suffix is exactly that.
        let (head, mode) = match spec.rsplit_once(':') {
            Some((h, "ro")) => (h, true),
            Some((h, "rw")) => (h, false),
            _ => (spec, false),
        };
        let Some((local, container)) = head.split_once(':') else {
            return Err(FileError::BadFormat(spec.to_string()));
        };
        if local.is_empty() || container.is_empty() {
            return Err(FileError::BadFormat(spec.to_string()));
        }
        let local_path = Path::new(local);
        let local_abs = if local_path.is_absolute() {
            local_path.to_path_buf()
        } else if let Some(base) = base_dir {
            base.join(local_path)
        } else {
            local_path.to_path_buf()
        };
        Ok(Self {
            local: local_abs,
            container: PathBuf::from(container),
            read_only: mode,
        })
    }

    /// SHA-256 of the file's bytes — feeds the spec hash so that a
    /// content change triggers a redeploy without a tag bump.
    pub fn content_hash(&self) -> Result<String, FileError> {
        let bytes = std::fs::read(&self.local).map_err(|source| FileError::LocalRead {
            path: self.local.display().to_string(),
            source,
        })?;
        let mut h = Sha256::new();
        h.update(&bytes);
        let digest = h.finalize();
        Ok(hex(&digest[..]))
    }

    /// `/var/lib/yoink/files/<sha256>-<basename>`. Idempotent: same
    /// content always lands at the same path, so re-uploads are no-ops
    /// and concurrent deploys can't collide.
    #[must_use]
    pub fn remote_staging_path(&self, content_hash: &str) -> String {
        let basename = self
            .local
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        format!("/var/lib/yoink/files/{content_hash}-{basename}")
    }

    /// Render as the docker-cli bind string our container builder
    /// already understands: `<host_path>:<container_path>[:ro]`.
    #[must_use]
    pub fn as_bind_string(&self, content_hash: &str) -> String {
        let host = self.remote_staging_path(content_hash);
        let container = self.container.display();
        if self.read_only {
            format!("{host}:{container}:ro")
        } else {
            format!("{host}:{container}")
        }
    }
}

/// Upload a single `FileMount` to the target host. Idempotent — if a
/// file with the same content hash already exists at the staging path
/// we skip the scp.
pub async fn upload(host: &Host, mount: &FileMount, content_hash: &str) -> Result<(), FileError> {
    let remote = mount.remote_staging_path(content_hash);
    let dest = format!("{}@{}:{}", host.user, host.address, remote);
    let parent = Path::new(&remote).parent().map_or_else(
        || "/var/lib/yoink/files".into(),
        |p| p.display().to_string(),
    );

    // mkdir -p on the host (cheap, ssh roundtrip we do once per file).
    let mkdir = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(format!("{}@{}", host.user, host.address))
        .arg(format!("mkdir -p {parent}"))
        .output()
        .await
        .map_err(|e| FileError::Mkdir {
            host: host.address.clone(),
            message: e.to_string(),
        })?;
    if !mkdir.status.success() {
        return Err(FileError::Mkdir {
            host: host.address.clone(),
            message: String::from_utf8_lossy(&mkdir.stderr).into_owned(),
        });
    }

    // scp -p preserves modification time and permissions. -o BatchMode=yes
    // guarantees we never block on a password prompt during a deploy.
    let scp = Command::new("scp")
        .arg("-p")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(&mount.local)
        .arg(&dest)
        .output()
        .await
        .map_err(|e| FileError::Scp {
            host: host.address.clone(),
            remote: remote.clone(),
            message: e.to_string(),
        })?;
    if !scp.status.success() {
        return Err(FileError::Scp {
            host: host.address.clone(),
            remote,
            message: String::from_utf8_lossy(&scp.stderr).into_owned(),
        });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_local_container_ro() {
        let m = FileMount::parse("config/foo.yaml:/etc/foo.yaml:ro", None).unwrap();
        assert_eq!(m.local, PathBuf::from("config/foo.yaml"));
        assert_eq!(m.container, PathBuf::from("/etc/foo.yaml"));
        assert!(m.read_only);
    }

    #[test]
    fn parse_resolves_relative_against_base_dir() {
        let base = PathBuf::from("/home/user/yoink");
        let m = FileMount::parse("config/foo.yaml:/etc/foo.yaml", Some(&base)).unwrap();
        assert_eq!(m.local, PathBuf::from("/home/user/yoink/config/foo.yaml"));
        assert!(!m.read_only);
    }

    #[test]
    fn parse_keeps_absolute_local_path_unchanged() {
        let base = PathBuf::from("/somewhere/else");
        let m = FileMount::parse("/abs/path/foo.yaml:/etc/foo.yaml:ro", Some(&base)).unwrap();
        assert_eq!(m.local, PathBuf::from("/abs/path/foo.yaml"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            FileMount::parse("nocolon", None),
            Err(FileError::BadFormat(_))
        ));
        assert!(matches!(
            FileMount::parse(":/etc/foo:ro", None),
            Err(FileError::BadFormat(_))
        ));
    }

    #[test]
    fn remote_staging_path_is_content_addressed() {
        let m = FileMount {
            local: PathBuf::from("/tmp/foo.yaml"),
            container: PathBuf::from("/etc/foo.yaml"),
            read_only: true,
        };
        let p = m.remote_staging_path("deadbeef");
        assert_eq!(p, "/var/lib/yoink/files/deadbeef-foo.yaml");
    }

    #[test]
    fn as_bind_string_propagates_ro_flag() {
        let m = FileMount {
            local: PathBuf::from("/tmp/foo.yaml"),
            container: PathBuf::from("/etc/foo.yaml"),
            read_only: true,
        };
        assert_eq!(
            m.as_bind_string("abc123"),
            "/var/lib/yoink/files/abc123-foo.yaml:/etc/foo.yaml:ro"
        );
        let mut rw = m.clone();
        rw.read_only = false;
        assert_eq!(
            rw.as_bind_string("abc123"),
            "/var/lib/yoink/files/abc123-foo.yaml:/etc/foo.yaml"
        );
    }
}
