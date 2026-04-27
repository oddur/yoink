//! Unregistry-style image push: spin up an ephemeral
//! `ghcr.io/psviderski/unregistry` sidecar on the target host (backed
//! by the host's own docker image store via the mounted socket), open
//! an SSH-tunnelled local port to it, then `docker push` over the
//! standard registry protocol — registry-protocol layer dedup means
//! only layers the host doesn't already have cross the wire.
//!
//! Massive win on redeploys vs. the tarball transport (which re-streams
//! the whole image every time). The sidecar is `--rm` and is also
//! force-removed by name in [`cleanup`] / on Drop in case the push
//! crashes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use thiserror::Error;
use tracing::warn;

use super::oci_push::{self, OciPushError};
use super::tunnel::{SshTunnel, TunnelError};
use crate::docker_ops::{DockerError, DockerOps, Host, parse_published_port, rand_hex};
use crate::ssh_probe;

/// Image used for the ephemeral on-host registry sidecar.
///
/// TODO: pin by content digest (`@sha256:…`) before merge — tags are
/// mutable. Bump deliberately when upstream cuts a release we want.
pub const UNREGISTRY_IMAGE: &str = "ghcr.io/psviderski/unregistry";
pub const UNREGISTRY_TAG: &str = "0.4.2";

/// Container port the unregistry server listens on inside the sidecar.
const UNREGISTRY_CONTAINER_PORT: u16 = 5000;

/// Label applied to every ephemeral sidecar so a startup sweep can find
/// and reap leaked instances from a prior crash.
pub const SIDECAR_LABEL: &str = "yoink.role";
pub const SIDECAR_LABEL_VALUE: &str = "unregistry-ephemeral";

/// How long to wait for the sidecar's published port to become reachable
/// over the SSH tunnel. Pulling the unregistry image dominates first-run
/// time; the actual server startup is sub-second.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Error)]
pub enum UnregistryError {
    #[error("docker op against {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error("ssh tunnel setup: {0}")]
    Tunnel(#[from] TunnelError),
    #[error(
        "inspect of sidecar {name} returned no published port for {UNREGISTRY_CONTAINER_PORT}/tcp"
    )]
    NoPublishedPort { name: String },
    #[error("OCI push to registry failed: {0}")]
    OciPush(#[from] OciPushError),
    #[error("failed to talk to local docker daemon (operator-side): {0}")]
    LocalDocker(#[source] bollard::errors::Error),
    #[error("could not parse image reference {image_ref:?} into <repo>:<tag>")]
    InvalidImageRef { image_ref: String },
    #[error("registry endpoint at 127.0.0.1:{port} did not respond within {timeout:?}")]
    RegistryNotReady { port: u16, timeout: Duration },
    #[error("ssh pre-flight failed: {0}")]
    SshProbe(String),
}

fn docker_err(host: &Host, source: DockerError) -> UnregistryError {
    UnregistryError::Docker {
        host: host.address.clone(),
        source,
    }
}

/// Push `image_ref` (already present in the operator's local docker
/// daemon as `<image>:<tag>`) to `host` via an ephemeral unregistry
/// sidecar. On success the image is registered in the host's docker
/// store under the same `<image>:<tag>` reference, ready for the
/// reconcile loop to use.
///
/// On any error (sidecar create, tunnel, push) the sidecar and any
/// local re-tag are cleaned up best-effort before returning.
pub async fn push(
    ops: &dyn DockerOps,
    host: &Host,
    image_ref: &str,
) -> Result<(), UnregistryError> {
    if host.is_local() {
        // Pushing to the operator's own daemon is nonsensical — the
        // image is already there. Caller should never select unregistry
        // transport for the local host. Treat as a no-op rather than
        // erroring; matches the tarball transport's same-daemon
        // behavior (save → load is a tautology that succeeds trivially).
        return Ok(());
    }

    ensure_sidecar_image(ops, host).await?;

    let sidecar_name = format!("yoink-unregistry-{}", rand_hex());
    let body = sidecar_create_body();

    ops.create_container(host, &sidecar_name, body)
        .await
        .map_err(|e| docker_err(host, e))?;
    let cleanup = SidecarCleanup::new(ops, host, &sidecar_name);

    ops.start_container(host, &sidecar_name)
        .await
        .map_err(|e| docker_err(host, e))?;

    let detail = ops
        .inspect_container(host, &sidecar_name)
        .await
        .map_err(|e| docker_err(host, e))?;
    tracing::debug!(
        "unregistry sidecar {sidecar_name} ports from inspect: {:?}",
        detail.ports
    );
    let host_port =
        parse_published_port(&detail.ports, UNREGISTRY_CONTAINER_PORT).ok_or_else(|| {
            UnregistryError::NoPublishedPort {
                name: sidecar_name.clone(),
            }
        })?;

    // If this host uses `ssh_key_secret:`, route both the probe and
    // the tunnel's ssh through the same key bollard is using. Without
    // this, the probe would hit the operator's default identity (or
    // none) and the tunnel would silently auth-fail.
    let keyfile = ops.ssh_keyfile(host);
    // Pre-flight ssh so we surface classified errors (Tailscale auth,
    // permission denied, …) instead of the tunnel's generic "didn't
    // become reachable" timeout. Same probe `yoink preflight` uses.
    ssh_probe::probe(host, keyfile.as_deref().and_then(|p| p.to_str()))
        .await
        .map_err(UnregistryError::SshProbe)?;
    let tunnel = SshTunnel::open(
        &host.user,
        &host.address,
        host_port,
        READY_TIMEOUT,
        keyfile.as_deref(),
    )
    .await?;

    // End-to-end probe: a TCP-level readiness check on the SSH tunnel
    // returns Ok the instant ssh starts listening locally, BEFORE the
    // remote forward is wired (ssh hasn't finished auth yet on a cold
    // tunnel). Poll the registry's `/v2/` until it responds with an
    // HTTP status — at that point we know the tunnel is fully wired
    // through to the unregistry container.
    wait_for_registry_ready(tunnel.local_port(), Duration::from_secs(20)).await?;

    // Bypass the operator's docker daemon for the push step. On
    // macOS Docker Desktop the daemon lives in a Linux VM, so a
    // `docker push 127.0.0.1:<tunnel_port>/...` from the daemon would
    // hit the VM's loopback (nothing there) instead of the operator's
    // loopback (where the SSH tunnel lives). Reading the image bytes
    // via the daemon (no network) and pushing via reqwest from the
    // operator process works on every platform uniformly.
    let (repo, tag) = split_image_ref(image_ref)?;
    let base_url = format!("http://127.0.0.1:{}", tunnel.local_port());

    let tar_bytes = export_local_image(image_ref).await?;
    let summary = oci_push::push_image(&base_url, &repo, &tag, tar_bytes).await?;
    tracing::info!(
        "unregistry: pushed {image_ref} to {} ({} blobs uploaded, {} skipped, {} bytes)",
        host.address,
        summary.blobs_uploaded,
        summary.blobs_skipped,
        summary.bytes_uploaded
    );

    // Tunnel drops here (kills ssh child); cleanup runs explicitly so
    // we surface the result rather than swallowing in Drop.
    drop(tunnel);
    cleanup.run_now().await;
    Ok(())
}

/// `<image>:<tag>` → ("<image>", "<tag>"). Handles the digest form
/// (`<image>@sha256:…`) by treating the digest as the tag — unregistry
/// doesn't index by digest at the registry level for our use case
/// (we always `image:tag` from the operator), so this is a defensive
/// path for the rare `--tag sha256:…` rollback.
fn split_image_ref(image_ref: &str) -> Result<(String, String), UnregistryError> {
    if let Some((repo, digest)) = image_ref.split_once('@') {
        return Ok((repo.to_string(), digest.to_string()));
    }
    image_ref
        .rsplit_once(':')
        .map(|(r, t)| (r.to_string(), t.to_string()))
        .ok_or_else(|| UnregistryError::InvalidImageRef {
            image_ref: image_ref.to_string(),
        })
}

/// Export an image from the operator's local docker daemon as a
/// docker-save tarball. Bollard's `export_image` returns a stream of
/// `Bytes` chunks — collect into a single `Bytes` so the OCI push
/// module can do random-access tar parsing.
///
/// For typical yoink-built service images (a few hundred MB at most)
/// the in-memory hit is fine. If we ever need to support multi-GB
/// images we can switch to a tempfile-backed two-pass.
async fn export_local_image(image_ref: &str) -> Result<Bytes, UnregistryError> {
    let docker = Docker::connect_with_local_defaults().map_err(UnregistryError::LocalDocker)?;
    let mut stream = docker.export_image(image_ref);
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(UnregistryError::LocalDocker)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Pull the unregistry image to `host` if it isn't already cached.
/// Errors here are the most common "fall back to tarball" trigger
/// (host can't reach ghcr, pull-rate limited, etc.).
async fn wait_for_registry_ready(
    local_port: u16,
    timeout: Duration,
) -> Result<(), UnregistryError> {
    let url = format!("http://127.0.0.1:{local_port}/v2/");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client builder with default config");
    let deadline = Instant::now() + timeout;
    loop {
        match client.get(&url).send().await {
            // Any HTTP response (200, 401, 404…) means the tunnel is
            // wired end-to-end and the registry is up.
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            Err(_) => {
                return Err(UnregistryError::RegistryNotReady {
                    port: local_port,
                    timeout,
                });
            }
        }
    }
}

async fn ensure_sidecar_image(ops: &dyn DockerOps, host: &Host) -> Result<(), UnregistryError> {
    // `pull_image` is idempotent — a separate `image_present` check
    // would only add a round-trip for the same answer.
    ops.pull_image(host, UNREGISTRY_IMAGE, UNREGISTRY_TAG, None)
        .await
        .map_err(|e| docker_err(host, e))?;
    Ok(())
}

fn sidecar_create_body() -> ContainerCreateBody {
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    let key = format!("{UNREGISTRY_CONTAINER_PORT}/tcp");
    port_bindings.insert(
        key.clone(),
        Some(vec![PortBinding {
            // Publish on loopback only — we tunnel to it from the
            // operator and never want the sidecar reachable from the
            // host's external interface.
            host_ip: Some("127.0.0.1".to_string()),
            // "0" → docker picks an ephemeral host port; we read it
            // back from inspect.
            host_port: Some("0".to_string()),
        }]),
    );

    let exposed_ports: Vec<String> = vec![key];

    let labels: HashMap<String, String> = [
        (SIDECAR_LABEL.to_string(), SIDECAR_LABEL_VALUE.to_string()),
        ("yoink.managed".to_string(), "true".to_string()),
    ]
    .into_iter()
    .collect();

    ContainerCreateBody {
        image: Some(format!("{UNREGISTRY_IMAGE}:{UNREGISTRY_TAG}")),
        exposed_ports: Some(exposed_ports),
        labels: Some(labels),
        host_config: Some(HostConfig {
            // unregistry reads/writes the host's image store via the
            // CONTAINERD socket (not docker.sock — it talks to
            // containerd directly). Default path on a modern Linux
            // docker host is `/run/containerd/containerd.sock`.
            // Privileged mount; document in --help.
            binds: Some(vec![
                "/run/containerd/containerd.sock:/run/containerd/containerd.sock".to_string(),
            ]),
            port_bindings: Some(port_bindings),
            // Fire-and-forget: docker reaps when we stop it.
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}



/// RAII guard that force-removes the sidecar if Drop runs before
/// [`SidecarCleanup::run_now`] consumes it. `run_now` is the happy-path
/// async cleanup that surfaces removal errors to the caller; the Drop
/// path covers panics and early returns, where the sidecar is left to
/// `auto_remove: true` plus the next deploy's label sweep.
struct SidecarCleanup<'a> {
    ops: &'a dyn DockerOps,
    host: Host,
    name: String,
}

impl<'a> SidecarCleanup<'a> {
    fn new(ops: &'a dyn DockerOps, host: &Host, name: &str) -> Self {
        Self {
            ops,
            host: host.clone(),
            name: name.to_string(),
        }
    }

    async fn run_now(self) {
        let result = self
            .ops
            .force_remove_container(&self.host, &self.name)
            .await;
        // Skip the Drop log — we already ran cleanup synchronously.
        std::mem::forget(self);
        if let Err(e) = result {
            warn!("failed to remove unregistry sidecar: {e}");
        }
    }
}

impl Drop for SidecarCleanup<'_> {
    fn drop(&mut self) {
        warn!(
            "unregistry sidecar {} on {} not cleaned up synchronously; \
             will be reaped on the next deploy via label sweep",
            self.name, self.host.address
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_published_port_picks_matching_container_port() {
        let entries = vec!["32768:5000/tcp".to_string(), "32769:9090/tcp".to_string()];
        assert_eq!(parse_published_port(&entries, 5000), Some(32768));
        assert_eq!(parse_published_port(&entries, 9090), Some(32769));
        assert_eq!(parse_published_port(&entries, 80), None);
    }

    #[test]
    fn parse_published_port_ignores_unpublished_lines() {
        let entries = vec![
            "(unpublished) 5000/tcp".to_string(),
            "32770:5000/tcp".to_string(),
        ];
        assert_eq!(parse_published_port(&entries, 5000), Some(32770));
    }

    #[test]
    fn split_image_ref_handles_common_shapes() {
        assert_eq!(
            split_image_ref("bt-api:v1").unwrap(),
            ("bt-api".into(), "v1".into())
        );
        assert_eq!(
            split_image_ref("ghcr.io/owner/bt-api:abc").unwrap(),
            ("ghcr.io/owner/bt-api".into(), "abc".into())
        );
        assert_eq!(
            split_image_ref("repo@sha256:deadbeef").unwrap(),
            ("repo".into(), "sha256:deadbeef".into())
        );
    }
}
