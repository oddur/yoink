//! `yoink fs` — laptop-mount a running container's filesystem.
//!
//! Same shape as `yoink pf` but for files. The mechanism:
//!
//! 1. Pick a target container (replica + host filter, like `pf`).
//! 2. Inspect it; read `GraphDriver.Data["MergedDir"]` — the host
//!    overlay2 path that mirrors the container's full filesystem
//!    (rw layer + image layers, unioned).
//! 3. `sshfs <user>@<host>:<MergedDir> <local-mountpoint>` over the
//!    same SSH credentials yoink already uses.
//! 4. Hold the foreground sshfs subprocess until Ctrl-C; on shutdown,
//!    force-unmount the local mountpoint (FUSE-managed kernel state)
//!    and reap the child.
//!
//! Read-only by default. Writes through `merged` mutate the running
//! container's filesystem and can race with the app (file locks,
//! truncation, in-flight log rotation) — `--rw` opts in.
//!
//! Overlay2-only for v1. Other storage drivers (btrfs, zfs, vfs) put
//! the container's rootfs in driver-specific places that don't map
//! to a single host path; we fail loudly with an actionable error
//! rather than mount the wrong thing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context as _;
use thiserror::Error;
use tokio::process::Command;

use crate::config::Config;
use crate::docker_ops::{ContainerDetail, DockerOps, GraphDriverInfo, Host};

/// Default mountpoint root on the operator's machine. Each `yoink fs`
/// invocation gets a per-service subdir so two services don't collide.
pub const DEFAULT_MOUNT_ROOT: &str = "/tmp/yoink-fs";

/// Hard cap on how long we wait for `umount`/`fusermount` to clean up
/// during shutdown. Real unmounts complete in milliseconds; this is
/// the safety valve for "ssh died, FUSE is in a weird state."
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum FsError {
    #[error("service {0:?} not found in config")]
    ServiceNotFound(String),
    #[error("service {service:?} has no running container on {host} — was `yoink up` run?")]
    NoRunningContainer { service: String, host: String },
    #[error(
        "service {service:?} has {found} running container(s) on {host}; \
         --replica {replica} is out of range"
    )]
    ReplicaOutOfRange {
        service: String,
        host: String,
        replica: usize,
        found: usize,
    },
    #[error(
        "container {container:?} on {host} is using docker storage driver {driver:?} — \
         `yoink fs` only supports overlay2 today (other drivers don't expose a single host path \
         that mirrors the container's rootfs)"
    )]
    UnsupportedStorageDriver {
        container: String,
        host: String,
        driver: String,
    },
    #[error(
        "container {container:?} on {host} reports overlay2 but no MergedDir — \
         daemon may be too old, or the container is in an unusual state"
    )]
    NoMergedDir { container: String, host: String },
    #[error(
        "`sshfs` not found on PATH — `brew install --cask fuse-t-sshfs` on macOS, \
         `apt install sshfs` (or distro equivalent) on Linux"
    )]
    SshfsMissing,
    #[error("sshfs exited with status {status}{stderr}", stderr = if .stderr.is_empty() { String::new() } else { format!(":\n{}", .stderr) })]
    SshfsFailed { status: String, stderr: String },
    #[error(
        "mountpoint {0:?} already in use — unmount it (`umount {0}` or `fusermount -u {0}`) and retry"
    )]
    MountpointBusy(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Docker(#[from] crate::docker_ops::DockerError),
}

/// Settings the CLI / future TUI shape passes in. Kept as a single
/// struct so adding flags is a one-line schema change instead of a
/// signature shuffle.
#[derive(Debug, Clone)]
pub struct FsOptions {
    pub host_filter: Option<String>,
    pub replica: usize,
    /// Mountpoint on the operator's machine. `None` → defaults to
    /// `/tmp/yoink-fs/<service>/`.
    pub to: Option<PathBuf>,
    /// Mount read-write. Default is read-only — writes through
    /// the merged view race with the running app's writes (file
    /// locks, truncation, log rotation), so opting in is loud.
    pub rw: bool,
}

/// Pick the running container that matches `(service, host, replica)`.
/// Used by `cmd_fs` and reachable from tests via the `DockerOps`
/// trait.
pub async fn resolve_container(
    ops: &dyn DockerOps,
    host: &Host,
    service: &str,
    replica: usize,
) -> Result<String, FsError> {
    let label = format!("yoink.service={service}");
    let mut containers = ops.list_containers_by_label(host, &label).await?;
    // Sort for deterministic replica indexing — list_containers_by_label
    // ordering is daemon-defined, which would otherwise make `--replica 1`
    // non-deterministic across calls on a multi-replica deploy.
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    let running: Vec<_> = containers
        .into_iter()
        .filter(|c| c.state == "running")
        .collect();
    if running.is_empty() {
        return Err(FsError::NoRunningContainer {
            service: service.to_string(),
            host: host.address.clone(),
        });
    }
    let found = running.len();
    running
        .into_iter()
        .nth(replica)
        .map(|c| c.name)
        .ok_or(FsError::ReplicaOutOfRange {
            service: service.to_string(),
            host: host.address.clone(),
            replica,
            found,
        })
}

/// Extract the host-side path that mirrors the container's running
/// rootfs. Errors loudly when the storage driver isn't overlay2 or
/// when the daemon didn't populate `MergedDir` (rare; would need an
/// older daemon or a transient inspect race).
pub fn merged_dir<'d>(
    detail: &'d ContainerDetail,
    container: &str,
    host_addr: &str,
) -> Result<&'d str, FsError> {
    let GraphDriverInfo { name, data } =
        detail
            .graph_driver
            .as_ref()
            .ok_or_else(|| FsError::NoMergedDir {
                container: container.to_string(),
                host: host_addr.to_string(),
            })?;
    if name != "overlay2" {
        return Err(FsError::UnsupportedStorageDriver {
            container: container.to_string(),
            host: host_addr.to_string(),
            driver: name.clone(),
        });
    }
    data.get("MergedDir")
        .map(String::as_str)
        .ok_or_else(|| FsError::NoMergedDir {
            container: container.to_string(),
            host: host_addr.to_string(),
        })
}

/// Where to mount on the operator's machine when `--to` is unset.
#[must_use]
pub fn default_mountpoint(service: &str) -> PathBuf {
    Path::new(DEFAULT_MOUNT_ROOT).join(service)
}

/// Spawn `sshfs` in the foreground and return the child handle. The
/// returned process owns the FUSE mount; killing it (Ctrl-C, drop)
/// tears the mount down. Caller is responsible for ensuring
/// `mountpoint` exists as an empty dir.
pub async fn spawn_sshfs(
    user: &str,
    host_addr: &str,
    remote_dir: &str,
    mountpoint: &Path,
    keyfile: Option<&Path>,
    rw: bool,
) -> Result<tokio::process::Child, FsError> {
    let endpoint = format!("{user}@{host_addr}:{remote_dir}");
    let mut cmd = Command::new("sshfs");
    cmd.arg(&endpoint).arg(mountpoint);
    // -f: foreground (no daemonize) — the process IS the FUSE handler;
    //     killing it propagates to the kernel and unmounts cleanly.
    // -o BatchMode=yes / ConnectTimeout=10 / idmap=user — same posture
    //     as the SshTunnel (`transport/tunnel.rs`), with idmap
    //     remapping container-side UIDs onto the operator's so files
    //     don't show as nobody:nogroup.
    let mut opts: Vec<String> = vec![
        "BatchMode=yes".into(),
        "ConnectTimeout=10".into(),
        "idmap=user".into(),
        "reconnect".into(),
    ];
    if !rw {
        opts.push("ro".into());
    }
    if let Some(key) = keyfile {
        opts.push(format!("IdentityFile={}", key.display()));
        opts.push("IdentitiesOnly=yes".into());
    }
    cmd.arg("-f");
    cmd.arg("-o").arg(opts.join(","));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FsError::SshfsMissing
        } else {
            FsError::Io(e)
        }
    })?;
    Ok(child)
}

/// Tear down a mountpoint. Picks the right tool for the OS:
/// `fusermount -u` on Linux (no root needed; the user that mounted
/// is the user that can unmount), `umount` on macOS / BSD. Best-effort
/// — caller already SIGKILL'd the sshfs child, this is the explicit
/// cleanup belt to its suspenders.
pub async fn unmount(mountpoint: &Path) -> Result<(), FsError> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "linux") {
        ("fusermount", vec!["-u", mountpoint.to_str().unwrap_or("")])
    } else {
        ("umount", vec![mountpoint.to_str().unwrap_or("")])
    };
    let result = tokio::time::timeout(
        UNMOUNT_TIMEOUT,
        Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => {
            // Non-zero exit usually means "already unmounted" (sshfs
            // child died and propagated the umount). Log and shrug.
            tracing::debug!(target: "yoink::fs", %status, ?mountpoint, "unmount returned non-zero");
            Ok(())
        }
        Ok(Err(e)) => Err(FsError::Io(e)),
        Err(_) => {
            tracing::warn!(target: "yoink::fs", ?mountpoint, "unmount timed out — try `umount -f` manually");
            Ok(())
        }
    }
}

/// `true` when `path` looks like a stale FUSE mount: the dir exists
/// but `metadata` returns "transport endpoint not connected" (errno
/// 107 / `ENOTCONN`). Caller can offer to reap it before remounting.
pub fn is_stale_fuse_mount(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => false,
        Err(e) => {
            // ENOTCONN on Linux, ENXIO on macOS — both surface as
            // io::Error::raw_os_error() values that aren't NotFound.
            // Heuristic: any non-`NotFound` error on a path that
            // exists as a directory entry suggests "broken mount".
            !matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) && path.exists()
        }
    }
}

/// Make sure `mountpoint` is an empty, ready-to-use directory.
/// Reaps a stale FUSE mount in place. Errors when the dir is non-
/// empty for non-stale-mount reasons (looks like the operator put
/// real files there).
pub async fn prepare_mountpoint(mountpoint: &Path) -> Result<(), FsError> {
    if is_stale_fuse_mount(mountpoint) {
        tracing::info!(target: "yoink::fs", ?mountpoint, "reaping stale FUSE mount before remount");
        unmount(mountpoint).await?;
    }
    if !mountpoint.exists() {
        std::fs::create_dir_all(mountpoint)?;
        return Ok(());
    }
    if !mountpoint.is_dir() {
        return Err(FsError::MountpointBusy(mountpoint.to_path_buf()));
    }
    let entries = std::fs::read_dir(mountpoint)?.next();
    match entries {
        None => Ok(()),
        Some(_) => Err(FsError::MountpointBusy(mountpoint.to_path_buf())),
    }
}

/// `cmd_fs` resolves the target, inspects it, prepares the
/// mountpoint, spawns sshfs, prints the URL-equivalent message,
/// and holds open until Ctrl-C. On shutdown: kill the sshfs child,
/// run `unmount` to be sure.
#[allow(clippy::too_many_lines)] // single linear flow, mirroring cmd_pf
pub async fn cmd_fs(
    config: &Config,
    service_name: &str,
    opts: FsOptions,
    ops: std::sync::Arc<dyn DockerOps>,
) -> anyhow::Result<()> {
    let service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| FsError::ServiceNotFound(service_name.to_string()))?;

    let applicable: Vec<_> = service
        .applicable_hosts(&config.hosts)
        .into_iter()
        .filter(|h| opts.host_filter.as_deref().is_none_or(|f| f == h.address))
        .collect();
    let host_cfg = match applicable.as_slice() {
        [] => anyhow::bail!(
            "service {service_name:?} has no applicable host{}",
            opts.host_filter
                .as_deref()
                .map_or(String::new(), |f| format!(" matching --host={f}")),
        ),
        [single] => *single,
        many if opts.host_filter.is_some() => many[0],
        many => anyhow::bail!(
            "service {service_name:?} runs on {} hosts; pin one with --host:\n{}",
            many.len(),
            many.iter()
                .map(|h| format!("  {}", h.address))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };

    let host = Host::from(host_cfg);
    if host.address == Host::LOCAL_ADDRESS {
        anyhow::bail!(
            "service {service_name:?} runs on `address: local` (the local docker socket), \
             which has no SSH endpoint to mount through. `yoink fs` is for remote hosts; \
             use `docker cp` or bind-mount the path locally instead."
        );
    }
    let keyfile = ops.ssh_keyfile(&host);
    crate::ssh_probe::probe(&host, keyfile.as_deref().and_then(|p| p.to_str()))
        .await
        .map_err(|e| anyhow::anyhow!("ssh probe to {}: {e}", host.address))?;

    let container = resolve_container(&*ops, &host, service_name, opts.replica).await?;
    let detail = ops.inspect_container(&host, &container).await?;
    let merged = merged_dir(&detail, &container, &host.address)?;

    let mountpoint = opts
        .to
        .clone()
        .unwrap_or_else(|| default_mountpoint(service_name));
    prepare_mountpoint(&mountpoint).await?;

    if opts.rw {
        eprintln!(
            "⚠ mounting {} read-write — writes can race with the running app's writes \
             (file locks, truncation, log rotation). Ctrl-C to abort.",
            container
        );
    }

    let mut child = spawn_sshfs(
        &host.user,
        &host.address,
        merged,
        &mountpoint,
        keyfile.as_deref(),
        opts.rw,
    )
    .await?;

    eprintln!(
        "→ {service_name} (replica {}) on {} ⇆ {}{}\n  Ctrl-C to unmount",
        opts.replica,
        host.address,
        mountpoint.display(),
        if opts.rw { " [rw]" } else { " [ro]" },
    );

    // Race: sshfs failure (network drop, permission denied) vs
    // operator Ctrl-C. Whichever fires first wins; both paths run
    // the cleanup.
    tokio::select! {
        status = child.wait() => {
            let exit = status.context("waiting on sshfs")?;
            if exit.success() {
                // sshfs exited cleanly on its own (rare; usually means
                // the daemon was killed by something else). Treat as
                // graceful shutdown.
                eprintln!("\n✓ unmounted (sshfs exited)");
            } else {
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut stderr).await;
                }
                let _ = unmount(&mountpoint).await;
                return Err(FsError::SshfsFailed {
                    status: exit.to_string(),
                    stderr,
                }
                .into());
            }
        }
        _ = tokio::signal::ctrl_c() => {
            // Order: SIGTERM the sshfs child (its SIGINT handler
            // should umount and exit), then explicit umount as the
            // belt to that suspenders, then drop the child. The
            // explicit umount survives the case where the child was
            // already wedged.
            let _ = child.start_kill();
            let _ = unmount(&mountpoint).await;
            let _ = child.wait().await;
            eprintln!("\n✓ unmounted");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_detail(driver: &str, merged_dir: Option<&str>) -> ContainerDetail {
        let mut data = BTreeMap::new();
        if let Some(m) = merged_dir {
            data.insert("MergedDir".into(), m.into());
            data.insert("UpperDir".into(), format!("{m}/../upper"));
        }
        ContainerDetail {
            name: "api-aaa".into(),
            graph_driver: Some(GraphDriverInfo {
                name: driver.into(),
                data,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn merged_dir_overlay2_returns_path() {
        let d = make_detail("overlay2", Some("/var/lib/docker/overlay2/abc/merged"));
        let path = merged_dir(&d, "api-aaa", "host-1").expect("ok");
        assert_eq!(path, "/var/lib/docker/overlay2/abc/merged");
    }

    #[test]
    fn merged_dir_rejects_btrfs() {
        let d = make_detail("btrfs", None);
        let err = merged_dir(&d, "api-aaa", "host-1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("btrfs"), "got: {msg}");
        assert!(msg.contains("overlay2 today"), "got: {msg}");
    }

    #[test]
    fn merged_dir_overlay2_without_mergeddir_errors() {
        let d = make_detail("overlay2", None);
        let err = merged_dir(&d, "api-aaa", "host-1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no MergedDir"), "got: {msg}");
    }

    #[test]
    fn merged_dir_no_graph_driver_errors() {
        let d = ContainerDetail {
            name: "api-aaa".into(),
            graph_driver: None,
            ..Default::default()
        };
        let err = merged_dir(&d, "api-aaa", "host-1").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no MergedDir"), "got: {msg}");
    }

    #[test]
    fn default_mountpoint_under_tmp() {
        let mp = default_mountpoint("api");
        assert_eq!(mp, Path::new("/tmp/yoink-fs/api"));
    }

    #[tokio::test]
    async fn prepare_mountpoint_creates_missing_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mp = tmp.path().join("nested/api");
        prepare_mountpoint(&mp).await.expect("prepare ok");
        assert!(mp.is_dir());
    }

    #[tokio::test]
    async fn prepare_mountpoint_rejects_non_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mp = tmp.path().join("api");
        std::fs::create_dir(&mp).unwrap();
        std::fs::write(mp.join("oops"), b"x").unwrap();
        let err = prepare_mountpoint(&mp).await.unwrap_err();
        assert!(matches!(err, FsError::MountpointBusy(_)));
    }

    fn make_container(name: &str, state: &str) -> crate::docker_ops::ContainerInfo {
        crate::docker_ops::ContainerInfo {
            host: "h1".into(),
            name: name.into(),
            image: String::new(),
            state: state.into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: Some("api".into()),
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn resolve_container_picks_replica_by_sorted_name() {
        use crate::docker_ops::FakeDockerOps;
        let ops = FakeDockerOps::new();
        let host = Host {
            user: "u".into(),
            address: "h1".into(),
        };
        ops.push_list_containers(Ok(vec![
            make_container("api-bbb", "running"),
            make_container("api-aaa", "running"),
            make_container("api-ccc", "exited"),
        ]));
        // Sorted: aaa, bbb (ccc filtered as exited). Replica 0 → aaa.
        let name = resolve_container(&ops, &host, "api", 0)
            .await
            .expect("resolve ok");
        assert_eq!(name, "api-aaa");
    }

    #[tokio::test]
    async fn resolve_container_replica_out_of_range() {
        use crate::docker_ops::FakeDockerOps;
        let ops = FakeDockerOps::new();
        let host = Host {
            user: "u".into(),
            address: "h1".into(),
        };
        ops.push_list_containers(Ok(vec![make_container("api-aaa", "running")]));
        let err = resolve_container(&ops, &host, "api", 5).await.unwrap_err();
        assert!(matches!(err, FsError::ReplicaOutOfRange { .. }));
    }
}
