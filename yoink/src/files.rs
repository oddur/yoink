//! Operator-side files that get uploaded to the target host before
//! `docker create`, then bind-mounted into the running container.
//! Equivalent of Kamal's `files:` directive — lets you keep
//! configuration (Caddyfile, otel-collector.yaml, etc.) in git instead
//! of pre-staging it on the host out-of-band.
//!
//! The upload is content-addressed (`<sha256>-<basename>`) so concurrent
//! deploys never overwrite each other and a config edit flows through
//! to a redeploy via the spec hash. Files land in
//! `~/.local/state/yoink/files/` of the SSH user, which that user can
//! create without root on a server yoink has never seen.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::host::Host;

/// Parsed `service.run.files` entry. Local path is operator-side
/// (resolved against the config file's directory); container path is
/// where the bind mount lands inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMount {
    pub local: PathBuf,
    pub container: PathBuf,
    /// The spec carried an explicit `:ro`. A `files:` mount is always
    /// read-only in the container: `false` only means the bind string
    /// stays bare, and the container builder defaults a bare bind to
    /// `:ro`. There is no writable state (`parse` rejects `:rw`).
    pub read_only: bool,
}

#[derive(Debug, Error)]
pub enum FileError {
    #[error(
        "invalid file mount {0:?}: expected \"local:container[:ro]\" \
         (`files:` mounts are always read-only; `:rw` is rejected)"
    )]
    BadFormat(String),
    #[error(
        "invalid file mount {0:?}: `:rw` is not allowed, `files:` mounts are always \
         read-only; use `binds:` or a volume for data the container must write"
    )]
    ReadWrite(String),
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
    /// Parse `"local:container[:ro]"`. The mount is always read-only:
    /// `:ro` is an explicit no-op, and `:rw` is an error
    /// (`FileError::ReadWrite`). A writable `files:` mount would lose
    /// its writes on the next deploy (the local copy is uploaded
    /// again), share one staged host file between replicas, and be
    /// unwritable for the default non-root container user. Local paths
    /// may be relative — they're resolved against `base_dir` (typically
    /// the config file's directory).
    pub fn parse(spec: &str, base_dir: Option<&Path>) -> Result<Self, FileError> {
        if crate::config::has_rw_mode(spec) {
            return Err(FileError::ReadWrite(spec.to_string()));
        }
        // Split deliberately on the *first* and *last* `:` rather than
        // by count, so container paths with colons (rare but possible)
        // aren't mangled. The trailing `:ro` is recognized only when
        // the suffix is exactly that.
        let (head, mode) = match spec.rsplit_once(':') {
            Some((h, "ro")) => (h, true),
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

    /// Read the file's bytes once and return them alongside their
    /// SHA-256 hex digest. The hash feeds the spec hash so a content
    /// change triggers a redeploy without a tag bump.
    ///
    /// Returning the bytes (rather than re-reading at upload time)
    /// closes a TOCTOU window: if the operator edits the file
    /// between hash compute and upload, the spec-hash would reference
    /// one byte stream and the host would receive another, then the
    /// next reconcile would see matching `yoink.spec_hash` and
    /// silently keep the stale-on-disk content. Reading once
    /// eliminates the gap.
    pub fn read_and_hash(&self) -> Result<(Vec<u8>, String), FileError> {
        let bytes = std::fs::read(&self.local).map_err(|source| FileError::LocalRead {
            path: self.local.display().to_string(),
            source,
        })?;
        let mut h = Sha256::new();
        h.update(&bytes);
        let digest = h.finalize();
        Ok((bytes, hex(&digest[..])))
    }

    /// `<sha256>-<basename>`: the name of the staged file on the host.
    /// Idempotent: same content always lands at the same name, so
    /// re-uploads are no-ops and concurrent deploys can't collide.
    #[must_use]
    pub fn staging_file_name(&self, content_hash: &str) -> String {
        let basename = self
            .local
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        format!("{content_hash}-{basename}")
    }

    /// The path that stands for this file in the spec hash:
    /// `/var/lib/yoink/files/<sha256>-<basename>`.
    ///
    /// This is NOT where the file is staged. yoink staged files there
    /// before v0.21, and only root can create that directory. The real
    /// directory is under the home of the SSH user and differs between
    /// hosts and users, so it cannot feed the hash: `status`, the TUI
    /// and `--dry-run` compute the hash without a connection to the
    /// host. The old path stays as the hash input so that no service
    /// changes its `spec_hash` because of the move.
    #[must_use]
    pub fn remote_staging_path(&self, content_hash: &str) -> String {
        format!(
            "{LEGACY_FILES_DIR}/{}",
            self.staging_file_name(content_hash)
        )
    }

    /// The bind string for the container: the file as staged in
    /// `staging_dir` (the directory that [`upload`] returned), mounted
    /// at the container path. Same shape as [`Self::as_bind_string`].
    #[must_use]
    pub fn bind_string_at(&self, staging_dir: &str, content_hash: &str) -> String {
        let name = self.staging_file_name(content_hash);
        let container = self.container.display();
        let mode = if self.read_only { ":ro" } else { "" };
        format!("{staging_dir}/{name}:{container}{mode}")
    }

    /// Render as the docker-cli bind string our container builder
    /// already understands: `<host_path>:<container_path>[:ro]`.
    /// This form feeds the spec hash only; the container is created
    /// with [`Self::bind_string_at`]. See [`Self::remote_staging_path`].
    /// A spec with no mode renders bare, and
    /// `run_args::default_bind_to_ro` then appends `:ro`. Do not
    /// normalise the two forms: the string feeds the spec hash, so a
    /// change here rerolls every service that has a `files:` entry.
    #[must_use]
    pub fn as_bind_string(&self, content_hash: &str) -> String {
        let host = self.remote_staging_path(content_hash);
        let container = self.container.display();
        let mode = if self.read_only { ":ro" } else { "" };
        format!("{host}:{container}{mode}")
    }
}

/// Hash-input directory for staged files. See
/// [`FileMount::remote_staging_path`]. yoink does not write here.
const LEGACY_FILES_DIR: &str = "/var/lib/yoink/files";

/// Shell expression for the directory that holds staged files on a
/// host. It is under the home directory of the SSH user, so a deploy
/// user without root can create it. The host shell expands it.
const HOST_FILES_DIR_SH: &str = "\"${XDG_STATE_HOME:-$HOME/.local/state}/yoink/files\"";

/// Shell script that writes stdin to the staged file `name` and
/// prints the absolute staging directory.
///
/// Single ssh roundtrip: mkdir, then write the bytes to a tempfile and
/// atomic-rename into place. Atomic rename means a partial transfer
/// (network drop) never leaves the staged file half-written for a
/// concurrent reader. The name goes through a single-quoted shell var
/// so spaces / metachars in the basename can't break parsing or inject.
fn upload_script(name: &str) -> String {
    format!(
        "set -e; \
         P={HOST_FILES_DIR_SH}; N={n}; \
         mkdir -p \"$P\"; \
         tmp=\"$P/$N.tmp.$$\"; \
         cat > \"$tmp\"; \
         chmod 0644 \"$tmp\"; \
         mv \"$tmp\" \"$P/$N\"; \
         printf '%s' \"$P\"",
        n = shell_single_quote(name),
    )
}

/// Upload a single `FileMount` to the target host and return the
/// absolute directory it was staged in, for
/// [`FileMount::bind_string_at`]. Idempotent: the staged name is
/// content-addressed, so re-uploads of the same bytes land at the same
/// path. `bytes` is the in-memory content captured at `read_and_hash`
/// time; piping it to `ssh ... 'cat > file'` closes the
/// hash-vs-upload TOCTOU window.
///
/// `keyfile` is `DockerOps::ssh_keyfile(host)`: the decrypted
/// `ssh_key_secret:` key, so the upload authenticates the same way as
/// the docker connection. The `local` sentinel host skips ssh and
/// stages the file on the operator machine, where its daemon runs.
pub async fn upload(
    host: &Host,
    keyfile: Option<&Path>,
    mount: &FileMount,
    content_hash: &str,
    bytes: &[u8],
) -> Result<String, FileError> {
    let name = mount.staging_file_name(content_hash);
    // For error messages only: the real directory is known after the
    // script runs.
    let remote = format!("~/.local/state/yoink/files/{name}");
    let script = upload_script(&name);
    let mut child = crate::ssh_keys::host_shell_command(host, keyfile, &script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| FileError::Scp {
            host: host.address.clone(),
            remote: remote.clone(),
            message: e.to_string(),
        })?;
    let mut stdin = child
        .stdin
        .take()
        .expect("Stdio::piped() guarantees Some on spawn");
    stdin.write_all(bytes).await.map_err(|e| FileError::Scp {
        host: host.address.clone(),
        remote: remote.clone(),
        message: e.to_string(),
    })?;
    stdin.shutdown().await.map_err(|e| FileError::Scp {
        host: host.address.clone(),
        remote: remote.clone(),
        message: e.to_string(),
    })?;
    // Close the pipe's write end BEFORE awaiting the child. `shutdown()`
    // flushes but does NOT close the fd — the `ChildStdin` owns it until
    // dropped, and `wait_with_output` doesn't take it. Leaving it open means
    // the remote `cat > tmp` never sees EOF, never exits, and the wait blocks
    // forever. openssh tolerates the lingering half-open stdin; Tailscale SSH
    // does not, so this deadlocks every `files:` upload to a tailnet host.
    drop(stdin);
    let output = child.wait_with_output().await.map_err(|e| FileError::Scp {
        host: host.address.clone(),
        remote: remote.clone(),
        message: e.to_string(),
    })?;
    if !output.status.success() {
        return Err(FileError::Scp {
            host: host.address.clone(),
            remote,
            message: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    // The bind source must be an absolute path. Anything else means
    // the host shell printed something unexpected (a login banner, an
    // unset HOME), and a relative bind would name a docker volume.
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !dir.starts_with('/') || dir.contains('\n') {
        return Err(FileError::Scp {
            host: host.address.clone(),
            remote,
            message: format!(
                "the host reported the staging directory {dir:?}, not an absolute path"
            ),
        });
    }
    Ok(dir)
}

/// POSIX-shell single-quote a string. Wraps in `'…'` and escapes any
/// embedded single quotes via `'\''`. Used to interpolate paths into
/// the upload script safely; without this, a basename containing a
/// space, `;`, `$`, backtick, glob, etc. would break parsing or
/// inject. The result is never empty (always at least `''`).
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
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
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(
            shell_single_quote("with space"),
            "'with space'",
            "spaces stay literal inside single quotes"
        );
        assert_eq!(
            shell_single_quote("$VAR;`backtick`"),
            "'$VAR;`backtick`'",
            "shell metachars stay literal inside single quotes"
        );
        assert_eq!(
            shell_single_quote("it's"),
            "'it'\\''s'",
            "embedded single quote escapes correctly"
        );
    }

    /// Staged files used to go to `/var/lib/yoink/files`, which a
    /// deploy user without root cannot create. They now go under the
    /// home directory of that user, and the script reports where.
    #[test]
    fn upload_script_stages_the_file_under_the_user_home() {
        use std::io::Write as _;
        let home = tempfile::tempdir().unwrap();
        let name = "abc123-it's a file.yaml";
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(upload_script(name))
            .env("HOME", home.path())
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"key: value\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let dir = home.path().join(".local/state/yoink/files");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            dir.to_str().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(name)).unwrap(),
            "key: value\n"
        );
        // No tempfile left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// The container binds the real staging directory. The spec hash
    /// keeps the old fixed path, so the move changes no `spec_hash`.
    #[test]
    fn bind_string_at_uses_the_staging_dir_and_hash_form_is_unchanged() {
        let m = FileMount {
            local: PathBuf::from("/tmp/foo.yaml"),
            container: PathBuf::from("/etc/foo.yaml"),
            read_only: true,
        };
        assert_eq!(
            m.bind_string_at("/home/deploy/.local/state/yoink/files", "abc123"),
            "/home/deploy/.local/state/yoink/files/abc123-foo.yaml:/etc/foo.yaml:ro"
        );
        assert_eq!(
            m.as_bind_string("abc123"),
            "/var/lib/yoink/files/abc123-foo.yaml:/etc/foo.yaml:ro"
        );
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

    /// An explicit `:rw` used to parse and then mount read-only. It is
    /// now an error, so a `FileMount` never stands for a writable mount.
    #[test]
    fn parse_rejects_rw() {
        let err = FileMount::parse("/tmp/data.json:/app/data.json:rw", None).unwrap_err();
        assert!(matches!(&err, FileError::ReadWrite(s) if s == "/tmp/data.json:/app/data.json:rw"));
        let msg = err.to_string();
        assert!(msg.contains("always read-only"), "{msg}");
        assert!(msg.contains("`binds:` or a volume"), "{msg}");
        // The mode is rejected before the shape is checked.
        assert!(matches!(
            FileMount::parse("nocolon:rw", None),
            Err(FileError::ReadWrite(_))
        ));
    }

    #[test]
    fn bad_format_error_says_rw_is_rejected() {
        let msg = FileMount::parse("nocolon", None).unwrap_err().to_string();
        assert!(msg.contains("local:container[:ro]"), "{msg}");
        assert!(msg.contains("`:rw` is rejected"), "{msg}");
    }

    /// Pins the bind strings for a spec with no mode and a spec with
    /// `:ro`. They feed the spec hash, so they must stay byte-identical
    /// across releases or every service with a `files:` entry rerolls.
    #[test]
    fn bind_strings_for_no_mode_and_ro_are_pinned() {
        let bare = FileMount::parse("/tmp/data.json:/app/data.json", None).unwrap();
        assert_eq!(
            bare.as_bind_string("abc123"),
            "/var/lib/yoink/files/abc123-data.json:/app/data.json"
        );
        assert_eq!(
            crate::run_args::default_bind_to_ro(&bare.as_bind_string("abc123")),
            "/var/lib/yoink/files/abc123-data.json:/app/data.json:ro"
        );
        let ro = FileMount::parse("/tmp/data.json:/app/data.json:ro", None).unwrap();
        assert_eq!(
            ro.as_bind_string("abc123"),
            "/var/lib/yoink/files/abc123-data.json:/app/data.json:ro"
        );
        assert_eq!(
            crate::run_args::default_bind_to_ro(&ro.as_bind_string("abc123")),
            "/var/lib/yoink/files/abc123-data.json:/app/data.json:ro"
        );
    }
}
