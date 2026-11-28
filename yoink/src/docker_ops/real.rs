//! `RealDockerOps`: the Docker Engine API over bollard, one cached
//! client per host, reached through the ssh connector.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, NetworkCreateRequest, NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, EventsOptionsBuilder,
    ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;

use super::{
    BUILD_TIMEOUT, ContainerDetail, ContainerInfo, ContainerStats, DockerError, DockerEvent,
    DockerEventKind, DockerOps, DockerVersion, ExecKind, ExecSession, HostInfo, ImageInfo,
    ImageTarStream, LogLine, LogStream, NetworkDetail, NetworkInfo, OneShotResult, ProcessTable,
    PruneReport, StoppedRemoval, VolumeInfo, rand_hex,
};
use crate::base::host::Host;

// ───── RealDockerOps ────────────────────────────────────────────────────

const HEALTHCHECK_CURL_IMAGE: &str = "curlimages/curl:8.10.1";
/// Tiny image with busybox `nc -z` for TCP-only probes — curl's
/// `--connect-only` is HTTP-scheme-bound and `telnet://` reads
/// after connect, both of which break against servers that don't
/// speak first (caddy with strict-SNI, redis, postgres). Busybox
/// is ~5 MB and doesn't change per docker host.
const HEALTHCHECK_TCP_IMAGE: &str = "busybox:1.37";

/// Maximum age of a cached bollard `Docker` handle. Past this, the
/// next `client_for` call drops it and reconnects.
///
/// Why: bollard's ssh transport spawns a fresh openssh master per
/// hyper pool entry; over a long session the pool — and the openssh
/// process / fd footprint — only grows. Periodic eviction tears down
/// the old hyper Client (and therefore every openssh master it
/// pooled), bounding fd usage and the staleness window. Long enough
/// that an active `yoink up` reuses the same handle; short enough
/// that an idle TUI cycles state in single-digit minutes.
const CLIENT_MAX_AGE: Duration = Duration::from_secs(120);

struct CachedClient {
    docker: Docker,
    created: Instant,
}

/// True if `e` looks like a transport-level failure (stale pooled
/// connection, link reset, ssh stdio session closed) rather than a
/// real Docker API error. Used by `with_retry` to decide whether
/// retrying with a freshly-dialed `Docker` is appropriate.
fn is_stale_transport(e: &bollard::errors::Error) -> bool {
    matches!(
        e,
        bollard::errors::Error::HyperLegacyError { .. }
            | bollard::errors::Error::HyperResponseError { .. }
    )
}

/// Real implementation. One `bollard::Docker` per host, cached for the
/// lifetime of the process (subject to `CLIENT_MAX_AGE` eviction).
#[derive(Default)]
pub struct RealDockerOps {
    clients: tokio::sync::Mutex<HashMap<String, CachedClient>>,
    timeout_secs: u64,
    /// Per-host private-key tempfiles for hosts declaring
    /// `ssh_key_secret:`. Built by `crate::ssh::keys::prepare` from the
    /// secrets bundle; passed into bollard `connect_with_ssh` as the
    /// `keypair_path` arg, and added to `ssh_probe`'s `ssh -i`.
    key_manager: Option<std::sync::Arc<crate::ssh::keys::KeyManager>>,
}

impl RealDockerOps {
    #[must_use]
    pub fn new() -> Self {
        Self::with_key_manager(None)
    }

    #[must_use]
    pub fn with_key_manager(km: Option<std::sync::Arc<crate::ssh::keys::KeyManager>>) -> Self {
        Self {
            clients: tokio::sync::Mutex::new(HashMap::new()),
            timeout_secs: 120,
            key_manager: km,
        }
    }

    async fn client_for(&self, host: &Host) -> Result<Docker, DockerError> {
        let key = host.ssh_url();

        // Cache hit: cheap. Local-socket transport doesn't share the
        // ssh-pool staleness / fd-leak failure modes, so its entry
        // never ages out.
        {
            let mut clients = self.clients.lock().await;
            if let Some(entry) = clients.get(&key) {
                if host.is_local() || entry.created.elapsed() < CLIENT_MAX_AGE {
                    return Ok(entry.docker.clone());
                }
                clients.remove(&key);
            }
        }

        // Cache miss: dial without holding the cache lock so a slow
        // ssh probe / connect on one host doesn't stall calls to any
        // other host. Two concurrent cache-miss callers for the same
        // key may both dial — last writer wins, the loser's `Docker`
        // drops harmlessly.
        let docker = if host.is_local() {
            // The `local` sentinel host routes to the operator's
            // docker socket via bollard's platform-default unix-
            // socket / npipe transport, bypassing ssh entirely.
            Docker::connect_with_local_defaults().map_err(|source| DockerError::Connect {
                host: host.address.clone(),
                source,
            })?
        } else {
            // Probe ssh first. Without this, bollard's SSH transport
            // happily blocks for `timeout_secs` (120 by default)
            // waiting on interactive prompts (Tailscale auth check,
            // password, host-key TOFU) that yoink can't see — the
            // TUI just hangs on "(loading…)". Probing gives us a
            // classified, actionable error string before bollard
            // ever opens the long-lived connection.
            let keyfile = self
                .key_manager
                .as_ref()
                .and_then(|km| km.key_for(host))
                .map(|p| p.to_string_lossy().into_owned());
            crate::ssh::probe::probe(host, keyfile.as_deref())
                .await
                .map_err(|detail| DockerError::SshProbe {
                    host: host.address.clone(),
                    detail,
                })?;
            // bollard's own ssh connector cannot dial a non-default
            // port (see `crate::ssh::bollard`), so a host with `port:`
            // goes through yoink's copy of it.
            match host.ssh_port() {
                Some(port) => crate::ssh::bollard::connect(host, port, keyfile, self.timeout_secs),
                None => Docker::connect_with_ssh(
                    &key,
                    self.timeout_secs,
                    bollard::API_DEFAULT_VERSION,
                    keyfile,
                ),
            }
            .map_err(|source| DockerError::Connect {
                host: host.address.clone(),
                source,
            })?
        };

        self.clients.lock().await.insert(
            key,
            CachedClient {
                docker: docker.clone(),
                created: Instant::now(),
            },
        );
        Ok(docker)
    }

    fn err(host: &Host, source: bollard::errors::Error) -> DockerError {
        DockerError::Bollard {
            host: host.address.clone(),
            source,
        }
    }

    /// Run `op(docker)` against the cached `Docker` for `host` and
    /// retry once with a freshly-dialed handle if it fails with a
    /// transport-level error.
    ///
    /// Why retry: bollard's ssh transport keeps an idle hyper pool of
    /// openssh-mux'd `docker system dial-stdio` sessions. Pooled
    /// connections that idle out (server-side timeout, NAT eviction,
    /// path flap, sleep/wake) surface as `client error (Connect)` or
    /// `connection closed before message completed` on first reuse,
    /// even though the next call would succeed. Evicting the cached
    /// `Docker` (and therefore its hyper pool + underlying ssh
    /// master) and dialing fresh papers over the race transparently.
    /// Real Docker API errors propagate as before.
    async fn with_retry<T, F, Fut>(&self, host: &Host, op: F) -> Result<T, DockerError>
    where
        F: Fn(Docker) -> Fut,
        Fut: std::future::Future<Output = Result<T, bollard::errors::Error>>,
    {
        let docker = self.client_for(host).await?;
        match op(docker).await {
            Ok(v) => Ok(v),
            Err(e) if is_stale_transport(&e) => {
                tracing::debug!(
                    host = %host.address,
                    error = %e,
                    "stale docker transport; reconnecting and retrying once",
                );
                self.clients.lock().await.remove(&host.ssh_url());
                let docker = self.client_for(host).await?;
                op(docker).await.map_err(|s| Self::err(host, s))
            }
            Err(e) => Err(Self::err(host, e)),
        }
    }

    /// `docker rm`, with or without `-f`. A missing container (404) is
    /// idempotent success.
    async fn remove_container(
        &self,
        host: &Host,
        name: &str,
        force: bool,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = RemoveContainerOptionsBuilder::new().force(force).build();
            match docker.remove_container(name, Some(opts)).await {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    /// Make sure a healthcheck probe image (`repo:tag`) is in the
    /// daemon's store. Best-effort: a failure here surfaces on the
    /// probe's `create_container` instead. The `image_present` check
    /// is a local daemon stat; without it every probe attempt asks
    /// the registry for the manifest, which is slow and counts
    /// against Docker Hub's pull rate limit.
    async fn ensure_probe_image(&self, host: &Host, reference: &str) {
        let (image, tag) = reference.split_once(':').unwrap_or((reference, "latest"));
        if self.image_present(host, image, tag).await.unwrap_or(false) {
            return;
        }
        let _ = self.pull_image(host, image, tag, None).await;
    }

    /// Create + start a container, then wait for it to exit. Returns
    /// the exit code. The caller owns inspecting the container (logs)
    /// and the final `force_remove_container` — this helper only
    /// reaps on the *error* path so partial state never lingers.
    /// Shared between the HTTP healthcheck, the TCP healthcheck, and
    /// `run_one_shot` — they all do the same dance.
    async fn create_start_wait(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<i64, DockerError> {
        let docker = self.client_for(host).await?;
        let create_opts = CreateContainerOptionsBuilder::new().name(name).build();
        docker
            .create_container(Some(create_opts), body)
            .await
            .map_err(|e| Self::err(host, e))?;
        if let Err(e) = docker.start_container(name, None).await {
            let _ = self.force_remove_container(host, name).await;
            return Err(Self::err(host, e));
        }
        let wait_opts = WaitContainerOptionsBuilder::new()
            .condition("not-running")
            .build();
        let mut wait_stream = docker.wait_container(name, Some(wait_opts));
        match wait_stream.next().await {
            Some(Ok(resp)) => Ok(resp.status_code),
            Some(Err(bollard::errors::Error::DockerContainerWaitError { error: _, code })) => {
                Ok(code)
            }
            Some(Err(other)) => {
                let _ = self.force_remove_container(host, name).await;
                Err(Self::err(host, other))
            }
            None => {
                let _ = self.force_remove_container(host, name).await;
                Err(DockerError::Invalid(
                    "wait_container yielded no event".into(),
                ))
            }
        }
    }
}

#[async_trait]
impl DockerOps for RealDockerOps {
    fn ssh_keyfile(&self, host: &Host) -> Option<std::path::PathBuf> {
        self.key_manager
            .as_ref()
            .and_then(|km| km.key_for(host))
            .map(std::path::Path::to_path_buf)
    }

    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError> {
        let v = self
            .with_retry(host, |docker| async move { docker.version().await })
            .await?;
        Ok(DockerVersion {
            server_version: v.version,
            api_version: v.api_version,
            os: v.os,
            arch: v.arch,
        })
    }

    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError> {
        let i = self
            .with_retry(host, |docker| async move { docker.info().await })
            .await?;
        Ok(HostInfo {
            n_cpu: i.ncpu,
            mem_total: i.mem_total,
            containers: i.containers,
            containers_running: i.containers_running,
            images: i.images,
            kernel: i.kernel_version,
            operating_system: i.operating_system,
        })
    }

    async fn container_stats(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<ContainerStats, DockerError> {
        let stats = self
            .with_retry(host, |docker| async move {
                let opts = bollard::query_parameters::StatsOptionsBuilder::new()
                    .stream(false)
                    .one_shot(false)
                    .build();
                let mut stream = docker.stats(name, Some(opts));
                stream.next().await.transpose()
            })
            .await?;
        let stats =
            stats.ok_or_else(|| DockerError::Invalid("stats stream yielded no item".into()))?;
        Ok(parse_stats(&stats))
    }

    async fn inspect_container(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<ContainerDetail, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                docker.inspect_container(name, None).await
            })
            .await?;
        Ok(parse_inspect(name, &resp))
    }

    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError> {
        self.with_retry(host, |docker| async move {
            match docker.inspect_network(network, None).await {
                Ok(_) => Ok(false),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {
                    let req = NetworkCreateRequest {
                        name: network.to_string(),
                        ..Default::default()
                    };
                    match docker.create_network(req).await {
                        Ok(_) => Ok(true),
                        // 409 = raced by another operator; treat as idempotent.
                        Err(bollard::errors::Error::DockerResponseServerError {
                            status_code: 409,
                            ..
                        }) => Ok(false),
                        Err(other) => Err(other),
                    }
                }
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn inspect_network(
        &self,
        host: &Host,
        network: &str,
    ) -> Result<Option<NetworkDetail>, DockerError> {
        self.with_retry(host, |docker| async move {
            match docker.inspect_network(network, None).await {
                Ok(n) => Ok(Some(NetworkDetail {
                    labels: n.labels.unwrap_or_default().into_iter().collect(),
                    internal: n.internal.unwrap_or(false),
                    containers: n
                        .containers
                        .unwrap_or_default()
                        .into_values()
                        .filter_map(|c| c.name)
                        .collect(),
                })),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(None),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn create_network(
        &self,
        host: &Host,
        network: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<bool, DockerError> {
        self.with_retry(host, |docker| async move {
            let req = NetworkCreateRequest {
                name: network.to_string(),
                labels: Some(labels.clone().into_iter().collect()),
                ..Default::default()
            };
            match docker.create_network(req).await {
                Ok(_) => Ok(true),
                // 409 = raced by another operator.
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 409, ..
                }) => Ok(false),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn connect_container_network(
        &self,
        host: &Host,
        container: &str,
        network: &str,
        endpoint: EndpointSettings,
    ) -> Result<(), DockerError> {
        let endpoint = &endpoint;
        self.with_retry(host, |docker| async move {
            let req = bollard::models::NetworkConnectRequest {
                container: container.to_string(),
                endpoint_config: Some(endpoint.clone()),
            };
            match docker.connect_network(network, req).await {
                // 403 = "endpoint already exists in network" — already
                // connected, treat as success.
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 403, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn disconnect_container_network(
        &self,
        host: &Host,
        container: &str,
        network: &str,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let req = bollard::models::NetworkDisconnectRequest {
                container: container.to_string(),
                force: Some(true),
            };
            match docker.disconnect_network(network, req).await {
                // 404 = no such container or network. 403 = the
                // container is not on the network. Both leave the
                // container off the network, which is the goal.
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 403 | 404,
                    ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn list_networks(&self, host: &Host) -> Result<Vec<NetworkInfo>, DockerError> {
        let nets = self
            .with_retry(host, |docker| async move {
                docker
                    .list_networks(None::<bollard::query_parameters::ListNetworksOptions>)
                    .await
            })
            .await?;
        Ok(nets
            .into_iter()
            .map(|n| NetworkInfo {
                host: host.address.clone(),
                name: n.name.unwrap_or_default(),
                driver: n.driver.unwrap_or_default(),
                scope: n.scope.unwrap_or_default(),
                internal: n.internal.unwrap_or(false),
                // bollard 0.20 dropped the `containers` map from the
                // list response (it was only populated when the daemon
                // returned a verbose-mode response). Container counts
                // require a per-network inspect; skip for now.
                container_count: 0,
            })
            .collect())
    }

    async fn list_volumes(&self, host: &Host) -> Result<Vec<VolumeInfo>, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                docker
                    .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
                    .await
            })
            .await?;
        Ok(resp
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|v| VolumeInfo {
                host: host.address.clone(),
                name: v.name,
                driver: v.driver,
                mountpoint: v.mountpoint,
                created: v.created_at.as_ref().map(ToString::to_string),
            })
            .collect())
    }

    async fn list_images(&self, host: &Host) -> Result<Vec<ImageInfo>, DockerError> {
        let images = self
            .with_retry(host, |docker| async move {
                let opts = bollard::query_parameters::ListImagesOptionsBuilder::new()
                    .all(false)
                    .build();
                docker.list_images(Some(opts)).await
            })
            .await?;
        Ok(images
            .into_iter()
            .map(|img| {
                let mut id = img.id;
                if let Some(rest) = id.strip_prefix("sha256:") {
                    id = rest.chars().take(12).collect();
                }
                let dangling = img
                    .repo_tags
                    .iter()
                    .all(|t| t == "<none>:<none>" || t.is_empty());
                ImageInfo {
                    host: host.address.clone(),
                    id,
                    repo_tags: img.repo_tags,
                    size_bytes: img.size,
                    created_unix: Some(img.created),
                    dangling,
                }
            })
            .collect())
    }

    async fn remove_image(&self, host: &Host, name: &str, force: bool) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = bollard::query_parameters::RemoveImageOptionsBuilder::new()
                .force(force)
                .noprune(false)
                .build();
            // 404 is idempotent: image already gone.
            match docker.remove_image(name, Some(opts), None).await {
                Ok(_)
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn prune_images(
        &self,
        host: &Host,
        dangling_only: bool,
    ) -> Result<PruneReport, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                // `dangling=true` → only `<none>` images. `dangling=false` →
                // every image with no live container reference (matches `-a`).
                let mut filters: HashMap<String, Vec<String>> = HashMap::new();
                filters.insert(
                    "dangling".to_string(),
                    vec![if dangling_only { "true" } else { "false" }.to_string()],
                );
                let opts = bollard::query_parameters::PruneImagesOptionsBuilder::new()
                    .filters(&filters)
                    .build();
                docker.prune_images(Some(opts)).await
            })
            .await?;
        let reclaimed = resp
            .images_deleted
            .unwrap_or_default()
            .into_iter()
            .filter_map(|d| d.deleted.or(d.untagged))
            .collect();
        Ok(PruneReport {
            reclaimed,
            space_reclaimed_bytes: resp.space_reclaimed.unwrap_or(0),
        })
    }

    async fn remove_volume(&self, host: &Host, name: &str, force: bool) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = bollard::query_parameters::RemoveVolumeOptionsBuilder::new()
                .force(force)
                .build();
            match docker.remove_volume(name, Some(opts)).await {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn prune_volumes(&self, host: &Host) -> Result<PruneReport, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                docker
                    .prune_volumes(None::<bollard::query_parameters::PruneVolumesOptions>)
                    .await
            })
            .await?;
        Ok(PruneReport {
            reclaimed: resp.volumes_deleted.unwrap_or_default(),
            space_reclaimed_bytes: resp.space_reclaimed.unwrap_or(0),
        })
    }

    async fn remove_network(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            match docker.remove_network(name).await {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn prune_networks(&self, host: &Host) -> Result<PruneReport, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                docker
                    .prune_networks(None::<bollard::query_parameters::PruneNetworksOptions>)
                    .await
            })
            .await?;
        Ok(PruneReport {
            reclaimed: resp.networks_deleted.unwrap_or_default(),
            space_reclaimed_bytes: 0,
        })
    }

    async fn restart_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = bollard::query_parameters::RestartContainerOptionsBuilder::new()
                .t(i32::try_from(drain.as_secs()).unwrap_or(i32::MAX))
                .build();
            docker.restart_container(name, Some(opts)).await
        })
        .await
    }

    async fn top_container(&self, host: &Host, name: &str) -> Result<ProcessTable, DockerError> {
        let resp = self
            .with_retry(host, |docker| async move {
                let opts = bollard::query_parameters::TopOptionsBuilder::new()
                    .ps_args("-ef")
                    .build();
                docker.top_processes(name, Some(opts)).await
            })
            .await?;
        Ok(ProcessTable {
            titles: resp.titles.unwrap_or_default(),
            processes: resp.processes.unwrap_or_default(),
        })
    }

    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        // Digest pulls go through `from_image=repo@sha256:...` with no
        // separate tag. Tag-pulls keep the conventional split.
        let opts = if tag.starts_with("sha256:") {
            CreateImageOptionsBuilder::new()
                .from_image(&format!("{image}@{tag}"))
                .build()
        } else {
            CreateImageOptionsBuilder::new()
                .from_image(image)
                .tag(tag)
                .build()
        };
        let mut stream = docker.create_image(Some(opts), None, credentials);
        // Track each layer's last-printed status so we don't spam the
        // log with "Downloading 53%" every chunk — only when the
        // layer crosses to a new status (e.g. → "Download complete").
        let mut last: HashMap<String, String> = HashMap::new();
        while let Some(item) = stream.next().await {
            let info = item.map_err(|s| Self::err(host, s))?;
            if let (Some(id), Some(status)) = (info.id.as_ref(), info.status.as_ref())
                && last.get(id) != Some(status)
            {
                tracing::info!(host = %host.address, image = %id, %status, "pull");
                last.insert(id.clone(), status.clone());
            }
        }
        Ok(())
    }

    async fn image_present(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
    ) -> Result<bool, DockerError> {
        let reference = crate::base::image::image_reference(image, tag);
        self.with_retry(host, |docker| {
            let reference = reference.clone();
            async move {
                match docker.inspect_image(&reference).await {
                    Ok(_) => Ok(true),
                    // bollard surfaces "no such image" as DockerResponseServerError
                    // with status 404. Anything else is a real error worth
                    // surfacing — but for an "is it cached" check, treating ALL
                    // errors as "not present" is also safe (worst case: we
                    // re-pull, which is the existing behavior). Pick the safe
                    // form so a flaky daemon doesn't hide pull failures.
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Ok(false),
                    Err(source) => Err(source),
                }
            }
        })
        .await
    }

    async fn load_image(&self, host: &Host, body: ImageTarStream) -> Result<(), DockerError> {
        use bollard::query_parameters::ImportImageOptions;
        let docker = self.client_for(host).await?;
        let mut stream = docker.import_image(
            ImportImageOptions {
                quiet: false,
                platform: None,
            },
            bollard::body_try_stream(body),
            None,
        );
        // Drain the progress stream — surface errors but ignore the
        // "Loaded image: ..." status messages.
        while let Some(item) = stream.next().await {
            item.map_err(|s| Self::err(host, s))?;
        }
        Ok(())
    }

    async fn build_image(
        &self,
        host: &Host,
        tag: &str,
        context_tar: bytes::Bytes,
    ) -> Result<(), DockerError> {
        use bollard::query_parameters::BuildImageOptionsBuilder;
        let docker = self.client_for(host).await?;
        let opts = BuildImageOptionsBuilder::default()
            .dockerfile("Dockerfile")
            .t(tag)
            .rm(true)
            .build();
        let mut stream = docker.build_image(opts, None, Some(bollard::body_full(context_tar)));
        // xcaddy compiles emit hundreds of progress lines — log at DEBUG
        // so `--verbose` (INFO) stays focused on deploy milestones.
        // bollard maps daemon `errorDetail` chunks to stream errors, so
        // most failures arrive as `Err(_)` from `next()`; the explicit
        // `error_detail` branch catches the rare detail-only case.
        // The whole drain is wrapped in a 15-min timeout: a network
        // stall during `go mod download` would otherwise sit forever
        // (the deploy lock heartbeat keeps the lock alive but neither
        // operator-side Ctrl-C nor a CI-runner hard timeout is a great
        // failure mode).
        let drain = async {
            while let Some(item) = stream.next().await {
                let info = item.map_err(|s| Self::err(host, s))?;
                if let Some(line) = info.stream.as_ref().filter(|s| !s.trim().is_empty()) {
                    tracing::debug!(host = %host.address, %tag, build = %line.trim_end(), "build");
                }
                if let Some(detail) = info.error_detail.as_ref() {
                    let msg = detail
                        .message
                        .clone()
                        .unwrap_or_else(|| "build failed".into());
                    return Err(DockerError::Invalid(format!(
                        "build_image {tag} on {}: {msg}",
                        host.address
                    )));
                }
            }
            Ok(())
        };
        match tokio::time::timeout(BUILD_TIMEOUT, drain).await {
            Ok(result) => result,
            Err(_) => Err(DockerError::Invalid(format!(
                "build_image {tag} on {}: timed out after {}s — \
                 network stall during `go mod download`? \
                 (configurable later if anyone needs longer)",
                host.address,
                BUILD_TIMEOUT.as_secs(),
            ))),
        }
    }

    async fn list_containers_by_label(
        &self,
        host: &Host,
        label: &str,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let summaries = self
            .with_retry(host, |docker| async move {
                let mut filters = HashMap::new();
                filters.insert("label".to_string(), vec![label.to_string()]);
                let opts = ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build();
                docker.list_containers(Some(opts)).await
            })
            .await?;
        Ok(summaries
            .into_iter()
            .map(|s| ContainerInfo::from_summary(&host.address, s))
            .collect())
    }

    async fn list_running_containers(
        &self,
        host: &Host,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let summaries = self
            .with_retry(host, |docker| async move {
                let mut filters = HashMap::new();
                filters.insert("status".to_string(), vec!["running".to_string()]);
                let opts = ListContainersOptionsBuilder::new()
                    .all(false)
                    .filters(&filters)
                    .build();
                docker.list_containers(Some(opts)).await
            })
            .await?;
        Ok(summaries
            .into_iter()
            .map(|s| ContainerInfo::from_summary(&host.address, s))
            .collect())
    }

    async fn create_container(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<String, DockerError> {
        let resp = self
            .with_retry(host, |docker| {
                let body = body.clone();
                async move {
                    let opts = CreateContainerOptionsBuilder::new().name(name).build();
                    docker.create_container(Some(opts), body).await
                }
            })
            .await?;
        Ok(resp.id)
    }

    async fn start_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            docker.start_container(name, None).await
        })
        .await
    }

    async fn stop_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = StopContainerOptionsBuilder::new()
                .t(i32::try_from(drain.as_secs()).unwrap_or(i32::MAX))
                .build();
            docker.stop_container(name, Some(opts)).await
        })
        .await
    }

    async fn force_remove_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        self.remove_container(host, name, true).await
    }

    async fn remove_stopped_container(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<StoppedRemoval, DockerError> {
        self.with_retry(host, |docker| async move {
            let conflict = match docker.remove_container(name, None).await {
                Ok(()) => return Ok(StoppedRemoval::Removed),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(StoppedRemoval::Absent),
                Err(
                    e @ bollard::errors::Error::DockerResponseServerError {
                        status_code: 409, ..
                    },
                ) => e,
                Err(other) => return Err(other),
            };
            // The daemon answers 409 for a running container, but also
            // for a removal that is already in progress. Inspect to
            // tell them apart instead of matching on the message text.
            match docker.inspect_container(name, None).await {
                Ok(resp) => {
                    // `running` stays true while a container is paused
                    // or waits for a restart.
                    let state = resp.state.unwrap_or_default();
                    let live =
                        [state.running, state.paused, state.restarting].contains(&Some(true));
                    if live {
                        Ok(StoppedRemoval::Running)
                    } else {
                        Err(conflict)
                    }
                }
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(StoppedRemoval::Absent),
                Err(_) => Err(conflict),
            }
        })
        .await
    }

    async fn kill_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            // Default signal is SIGKILL — that's exactly what we want here.
            match docker.kill_container(name, None).await {
                Ok(())
                // 409 = "container not running"; treat as idempotent.
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 409, ..
                }) => Ok(()),
                Err(other) => Err(other),
            }
        })
        .await
    }

    async fn healthcheck(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
        path: &str,
    ) -> Result<u16, DockerError> {
        let url = format!("http://{target}:{port}{path}");
        let probe_name = format!("yoink-probe-{}-{}", target, rand_hex());
        // Without this the first probe on a fresh daemon (a
        // just-provisioned host) 404s on create_container.
        self.ensure_probe_image(host, HEALTHCHECK_CURL_IMAGE).await;
        let body = probe_body(
            network,
            vec![
                "-fsS".into(),
                "-o".into(),
                "/dev/null".into(),
                "-w".into(),
                "%{http_code}".into(),
                "--max-time".into(),
                "5".into(),
                url,
            ],
        );
        let exit_status = self.create_start_wait(host, &probe_name, body).await?;
        // Reap the probe before `?` so a failed log fetch can't leak it.
        let logs = self.fetch_recent_logs(host, &probe_name, 32).await;
        let _ = self.force_remove_container(host, &probe_name).await;
        let logs = logs?;

        // curl with `-w "%{http_code}"` writes the 3-digit code (no newline)
        // before any `-S` error text. Take the leading digits we see.
        let code = logs
            .iter()
            .flat_map(|line| line.chars())
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if code.len() == 3
            && let Ok(n) = code.parse::<u16>()
        {
            return Ok(n);
        }
        // Couldn't parse — fall back to exit code interpretation.
        if exit_status == 0 { Ok(200) } else { Ok(0) }
    }

    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<OneShotResult, DockerError> {
        let docker = self.client_for(host).await?;
        let exec = docker
            .create_exec(
                container,
                bollard::exec::CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|s| Self::err(host, s))?;

        // Non-detached: bollard returns a stream of frame-tagged log
        // chunks that we drain into stdout/stderr buffers. The lock
        // heartbeat callers ignore the output; `yoink exec` prints it.
        let exec_results = docker
            .start_exec(
                &exec.id,
                Some(bollard::exec::StartExecOptions {
                    detach: false,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|s| Self::err(host, s))?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let bollard::exec::StartExecResults::Attached { mut output, .. } = exec_results {
            while let Some(item) = output.next().await {
                match item {
                    Ok(
                        bollard::container::LogOutput::StdOut { message }
                        | bollard::container::LogOutput::Console { message },
                    ) => stdout.push_str(&String::from_utf8_lossy(&message)),
                    Ok(bollard::container::LogOutput::StdErr { message }) => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(bollard::container::LogOutput::StdIn { .. }) => {}
                    Err(e) => return Err(Self::err(host, e)),
                }
            }
        }

        let inspect = docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|s| Self::err(host, s))?;
        Ok(OneShotResult {
            exit_code: inspect.exit_code.unwrap_or(0),
            stdout,
            stderr,
        })
    }

    async fn exec_interactive(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
        rows: u16,
        cols: u16,
    ) -> Result<ExecSession, DockerError> {
        let docker = self.client_for(host).await?;
        let exec = docker
            .create_exec(
                container,
                bollard::exec::CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|s| Self::err(host, s))?;

        let res = docker
            .start_exec(
                &exec.id,
                Some(bollard::exec::StartExecOptions {
                    detach: false,
                    tty: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|s| Self::err(host, s))?;

        let bollard::exec::StartExecResults::Attached { output, input } = res else {
            return Err(DockerError::Invalid(
                "start_exec returned Detached for an attached request".into(),
            ));
        };

        // Initial PTY size — best-effort; the daemon may reject if the exec
        // hasn't fully wired up, but the next render-driven resize will
        // catch up almost immediately.
        let _ = docker
            .resize_exec(
                &exec.id,
                bollard::exec::ResizeExecOptions {
                    height: rows,
                    width: cols,
                },
            )
            .await;

        let host_for_err = host.clone();
        let mapped = output.map(move |item| match item {
            Ok(
                bollard::container::LogOutput::Console { message }
                | bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::StdErr { message },
            ) => Ok(message),
            Ok(bollard::container::LogOutput::StdIn { .. }) => Ok(bytes::Bytes::new()),
            Err(e) => Err(Self::err(&host_for_err, e)),
        });

        Ok(ExecSession {
            id: exec.id,
            kind: ExecKind::Exec,
            stdin: input,
            output: Box::pin(mapped),
        })
    }

    async fn resize_exec(
        &self,
        host: &Host,
        exec_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            docker
                .resize_exec(
                    exec_id,
                    bollard::exec::ResizeExecOptions {
                        height: rows,
                        width: cols,
                    },
                )
                .await
        })
        .await
    }

    async fn resize_container_tty(
        &self,
        host: &Host,
        container: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = bollard::query_parameters::ResizeContainerTTYOptionsBuilder::default()
                .h(i32::from(rows))
                .w(i32::from(cols))
                .build();
            docker.resize_container_tty(container, opts).await
        })
        .await
    }

    async fn start_debug_sidecar(
        &self,
        host: &Host,
        target_container: &str,
        image: &str,
        rows: u16,
        cols: u16,
    ) -> Result<ExecSession, DockerError> {
        let docker = self.client_for(host).await?;

        // Pull the debug image best-effort — if it's already cached
        // this is a fast no-op; if the daemon can't reach the registry
        // and the image isn't cached, create_container will surface a
        // clearer error than pull would.
        let _ = self.pull_image(host, image, "latest", None).await;

        // Generate a unique-ish name so multiple debug sessions can
        // coexist (operator opens two side-by-side, one crashes, etc.).
        let suffix = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_micros())
        );
        let name = format!("yoink-debug-{target_container}-{suffix}");
        let target_ref = format!("container:{target_container}");
        let body = ContainerCreateBody {
            image: Some(format!("{image}:latest")),
            cmd: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                // Print a short banner so the operator immediately
                // knows they're inside the sidecar (not the target).
                format!(
                    "echo '=== yoink debug sidecar — sharing pid+net with {target_container} ==='; \
                     exec /bin/sh"
                ),
            ]),
            tty: Some(true),
            open_stdin: Some(true),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            stdin_once: Some(false),
            labels: Some(
                [
                    ("yoink.managed".to_string(), "true".to_string()),
                    ("yoink.debug-sidecar".to_string(), "true".to_string()),
                    (
                        "yoink.debug-target".to_string(),
                        target_container.to_string(),
                    ),
                ]
                .into(),
            ),
            host_config: Some(HostConfig {
                pid_mode: Some(target_ref.clone()),
                network_mode: Some(target_ref),
                auto_remove: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_opts = CreateContainerOptionsBuilder::default().name(&name).build();
        docker
            .create_container(Some(create_opts), body)
            .await
            .map_err(|s| Self::err(host, s))?;

        // Attach BEFORE start so we don't miss the banner echo.
        let attach_opts = bollard::query_parameters::AttachContainerOptionsBuilder::default()
            .stdin(true)
            .stdout(true)
            .stderr(true)
            .stream(true)
            .build();
        let attached = docker
            .attach_container(&name, Some(attach_opts))
            .await
            .map_err(|s| Self::err(host, s))?;

        docker
            .start_container(
                &name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .map_err(|s| Self::err(host, s))?;

        let resize_opts = bollard::query_parameters::ResizeContainerTTYOptionsBuilder::default()
            .h(i32::from(rows))
            .w(i32::from(cols))
            .build();
        let _ = docker.resize_container_tty(&name, resize_opts).await;

        let host_for_err = host.clone();
        let mapped = attached.output.map(move |item| match item {
            Ok(
                bollard::container::LogOutput::Console { message }
                | bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::StdErr { message },
            ) => Ok(message),
            Ok(bollard::container::LogOutput::StdIn { .. }) => Ok(bytes::Bytes::new()),
            Err(e) => Err(Self::err(&host_for_err, e)),
        });

        Ok(ExecSession {
            id: name,
            kind: ExecKind::Sidecar,
            stdin: attached.input,
            output: Box::pin(mapped),
        })
    }

    async fn healthcheck_tcp(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
    ) -> Result<(), DockerError> {
        // `nc -z` (busybox netcat) does pure TCP connect + close,
        // doesn't read afterward. Critical for endpoints that accept
        // the connection but don't send anything until the client
        // initiates TLS (caddy with strict-SNI / mTLS, redis raw
        // protocol). curl's `--connect-only` is HTTP-scheme-bound
        // and `telnet://` reads after connect — both fall over on
        // those.
        // Without this the first probe on a fresh daemon (a
        // just-provisioned host) 404s on create_container.
        self.ensure_probe_image(host, HEALTHCHECK_TCP_IMAGE).await;
        let probe_name = format!("yoink-tcp-probe-{}-{}", target, rand_hex());
        let body = probe_body_with_image(
            HEALTHCHECK_TCP_IMAGE,
            network,
            vec![
                "nc".into(),
                "-z".into(),
                "-w".into(),
                "5".into(),
                target.to_string(),
                port.to_string(),
            ],
        );
        let exit_status = self.create_start_wait(host, &probe_name, body).await?;
        let _ = self.force_remove_container(host, &probe_name).await;

        if exit_status == 0 {
            Ok(())
        } else {
            Err(DockerError::Invalid(format!(
                "tcp probe to {target}:{port} failed (curl exit {exit_status})"
            )))
        }
    }

    async fn fetch_recent_logs(
        &self,
        host: &Host,
        name: &str,
        lines: u32,
    ) -> Result<Vec<String>, DockerError> {
        self.with_retry(host, |docker| async move {
            let opts = LogsOptionsBuilder::new()
                .stdout(true)
                .stderr(true)
                .follow(false)
                .tail(&lines.to_string())
                .build();
            let mut stream = docker.logs(name, Some(opts));
            let mut out = Vec::new();
            while let Some(item) = stream.next().await {
                let log = item?;
                let bytes = match log {
                    bollard::container::LogOutput::StdOut { message }
                    | bollard::container::LogOutput::StdErr { message }
                    | bollard::container::LogOutput::Console { message }
                    | bollard::container::LogOutput::StdIn { message } => message,
                };
                out.push(String::from_utf8_lossy(&bytes).into_owned());
            }
            Ok(out)
        })
        .await
    }

    async fn run_one_shot(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<OneShotResult, DockerError> {
        let exit_code = self.create_start_wait(host, name, body).await?;
        // Remove on every path, including a failed log collection, as
        // the trait contract promises.
        let collected = async {
            let docker = self.client_for(host).await?;
            collect_stdout_stderr(&docker, name).await
        }
        .await;
        let _ = self.force_remove_container(host, name).await;
        let (stdout, stderr) = collected?;
        Ok(OneShotResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    async fn open_log_stream(
        &self,
        host: &Host,
        name: &str,
        tail_lines: u32,
    ) -> Result<mpsc::UnboundedReceiver<LogLine>, DockerError> {
        let docker = self.client_for(host).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .follow(true)
            .tail(&tail_lines.to_string())
            .build();
        let container = name.to_string();
        let mut stream = docker.logs(&container, Some(opts));
        let host_label = host.address.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(log) => {
                        let (stream_kind, bytes) = match log {
                            bollard::container::LogOutput::StdErr { message } => {
                                (LogStream::Stderr, message)
                            }
                            bollard::container::LogOutput::StdOut { message }
                            | bollard::container::LogOutput::Console { message }
                            | bollard::container::LogOutput::StdIn { message } => {
                                (LogStream::Stdout, message)
                            }
                        };
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        for line in text.split('\n') {
                            if line.is_empty() {
                                continue;
                            }
                            if tx
                                .send(LogLine {
                                    container: container.clone(),
                                    stream: stream_kind,
                                    message: line.to_string(),
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(host = %host_label, container = %container, error = %e, "log stream error");
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn subscribe_events(
        &self,
        host: &Host,
    ) -> Result<mpsc::UnboundedReceiver<DockerEvent>, DockerError> {
        let docker = self.client_for(host).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let opts = EventsOptionsBuilder::new().build();
        let mut stream = docker.events(Some(opts));
        let host_label = host.address.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                match item {
                    Ok(msg) => {
                        let kind = match msg.typ {
                            Some(bollard::models::EventMessageTypeEnum::CONTAINER) => {
                                DockerEventKind::Container
                            }
                            Some(bollard::models::EventMessageTypeEnum::NETWORK) => {
                                DockerEventKind::Network
                            }
                            _ => DockerEventKind::Other,
                        };
                        let action = msg.action.unwrap_or_default();
                        let container = msg.actor.and_then(|a| {
                            a.attributes
                                .and_then(|attrs| attrs.get("name").cloned())
                                .or(a.id)
                        });
                        if tx
                            .send(DockerEvent {
                                kind,
                                action,
                                container,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(host = %host_label, error = %e, "events stream error");
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}

/// Compute CPU% + memory used from one Docker stats snapshot. CPU% is
/// computed from the diff between `cpu_stats` and `precpu_stats` that
/// the daemon ships in every sample.
///
/// CPU/system-usage counters are u64 and memory counters are u64; both
/// fit comfortably in f64 for any realistic container, so the precision
/// loss the cast lint warns about is not a real concern here.
#[allow(clippy::cast_precision_loss)]
fn parse_stats(stats: &bollard::models::ContainerStatsResponse) -> ContainerStats {
    let total_now = stats
        .cpu_stats
        .as_ref()
        .and_then(|c| c.cpu_usage.as_ref())
        .and_then(|u| u.total_usage)
        .unwrap_or(0) as f64;
    let total_prev = stats
        .precpu_stats
        .as_ref()
        .and_then(|c| c.cpu_usage.as_ref())
        .and_then(|u| u.total_usage)
        .unwrap_or(0) as f64;
    let system_now = stats
        .cpu_stats
        .as_ref()
        .and_then(|c| c.system_cpu_usage)
        .unwrap_or(0) as f64;
    let system_prev = stats
        .precpu_stats
        .as_ref()
        .and_then(|c| c.system_cpu_usage)
        .unwrap_or(0) as f64;
    let online_cpus = f64::from(
        stats
            .cpu_stats
            .as_ref()
            .and_then(|c| c.online_cpus)
            .unwrap_or(1)
            .max(1),
    );

    let cpu_delta = total_now - total_prev;
    let system_delta = system_now - system_prev;
    let cpu_pct = if system_delta > 0.0 && cpu_delta >= 0.0 {
        (cpu_delta / system_delta) * online_cpus * 100.0
    } else {
        0.0
    };

    let mem_used: i64 = stats
        .memory_stats
        .as_ref()
        .and_then(|m| m.usage)
        .and_then(|u| i64::try_from(u).ok())
        .unwrap_or(0);
    let mem_limit: Option<i64> = stats
        .memory_stats
        .as_ref()
        .and_then(|m| m.limit)
        .and_then(|l| i64::try_from(l).ok());

    // Sum bytes across every interface — bollard's `networks` is a map
    // keyed by interface name (`eth0`, `eth1`, …). Container-side `lo`
    // doesn't appear, so this is the meaningful "total network usage".
    let (mut net_rx, mut net_tx): (i64, i64) = (0, 0);
    if let Some(nets) = stats.networks.as_ref() {
        for v in nets.values() {
            if let Some(rx) = v.rx_bytes
                && let Ok(rx) = i64::try_from(rx)
            {
                net_rx = net_rx.saturating_add(rx);
            }
            if let Some(tx) = v.tx_bytes
                && let Ok(tx) = i64::try_from(tx)
            {
                net_tx = net_tx.saturating_add(tx);
            }
        }
    }

    let (mut block_read, mut block_write): (i64, i64) = (0, 0);
    if let Some(io) = stats
        .blkio_stats
        .as_ref()
        .and_then(|b| b.io_service_bytes_recursive.as_ref())
    {
        for entry in io {
            let val = entry.value.and_then(|v| i64::try_from(v).ok()).unwrap_or(0);
            match entry.op.as_deref() {
                Some("read" | "Read") => block_read = block_read.saturating_add(val),
                Some("write" | "Write") => block_write = block_write.saturating_add(val),
                _ => {}
            }
        }
    }

    let pids: i64 = stats
        .pids_stats
        .as_ref()
        .and_then(|p| p.current)
        .and_then(|p| i64::try_from(p).ok())
        .unwrap_or(0);

    ContainerStats {
        cpu_pct,
        mem_used,
        mem_limit,
        net_rx_bytes: net_rx,
        net_tx_bytes: net_tx,
        block_read_bytes: block_read,
        block_write_bytes: block_write,
        pids,
    }
}

/// Distill bollard's `ContainerInspectResponse` into the trimmed
/// `ContainerDetail` the TUI renders. Pulls the bits an operator
/// actually reads (image, command, env, ports, mounts, networks)
/// and skips the noise (graph driver state, deeply-nested config
/// fields nobody looks at in a dashboard).
#[allow(clippy::too_many_lines)]
fn parse_inspect(name: &str, resp: &bollard::models::ContainerInspectResponse) -> ContainerDetail {
    let config = resp.config.as_ref();
    let state = resp.state.as_ref();
    let host_config = resp.host_config.as_ref();
    let network_settings = resp.network_settings.as_ref();

    let command = config.and_then(|c| {
        let parts = c.entrypoint.iter().flatten().chain(c.cmd.iter().flatten());
        let joined: Vec<&str> = parts.map(String::as_str).collect();
        if joined.is_empty() {
            None
        } else {
            Some(joined.join(" "))
        }
    });

    let mut env: Vec<String> = config
        .and_then(|c| c.env.clone())
        .unwrap_or_default()
        .into_iter()
        .collect();
    env.sort();

    let ports: Vec<String> = network_settings
        .and_then(|n| n.ports.as_ref())
        .map(|map| {
            let mut out: Vec<String> = map
                .iter()
                .flat_map(|(container_port, bindings)| {
                    let bindings_vec: Vec<String> = bindings
                        .iter()
                        .flatten()
                        .filter_map(|b| {
                            let host_port = b.host_port.as_deref()?;
                            Some(format!("{host_port}:{container_port}"))
                        })
                        .collect();
                    if bindings_vec.is_empty() {
                        vec![format!("(unpublished) {container_port}")]
                    } else {
                        bindings_vec
                    }
                })
                .collect();
            out.sort();
            out
        })
        .unwrap_or_default();

    let mounts: Vec<String> = resp
        .mounts
        .as_ref()
        .map(|ms| {
            let mut out: Vec<String> = ms
                .iter()
                .map(|m| {
                    let src = m.source.as_deref().unwrap_or("?");
                    let dst = m.destination.as_deref().unwrap_or("?");
                    let mode = m.mode.as_deref().unwrap_or("");
                    if mode.is_empty() {
                        format!("{src} → {dst}")
                    } else {
                        format!("{src} → {dst} [{mode}]")
                    }
                })
                .collect();
            out.sort();
            out
        })
        .unwrap_or_default();

    let networks: Vec<String> = network_settings
        .and_then(|n| n.networks.as_ref())
        .map(|m| {
            let mut out: Vec<String> = m.keys().cloned().collect();
            out.sort();
            out
        })
        .unwrap_or_default();

    let network_aliases: BTreeMap<String, Vec<String>> = network_settings
        .and_then(|n| n.networks.as_ref())
        .map(|m| {
            m.iter()
                .map(|(net, endpoint)| {
                    let mut names: Vec<String> = endpoint
                        .aliases
                        .iter()
                        .flatten()
                        .chain(endpoint.dns_names.iter().flatten())
                        .cloned()
                        .collect();
                    names.sort();
                    names.dedup();
                    (net.clone(), names)
                })
                .collect()
        })
        .unwrap_or_default();

    let labels: BTreeMap<String, String> = config
        .and_then(|c| c.labels.clone())
        .unwrap_or_default()
        .into_iter()
        .collect();

    ContainerDetail {
        name: name.into(),
        image: config.and_then(|c| c.image.clone()),
        image_id: resp.image.clone(),
        command,
        working_dir: config.and_then(|c| c.working_dir.clone()),
        state: state.and_then(|s| s.status.map(|st| format!("{st:?}").to_lowercase())),
        status: state.and_then(|s| s.error.clone()),
        started_at: state.and_then(|s| s.started_at.clone()),
        finished_at: state
            .and_then(|s| s.finished_at.clone())
            .filter(|s| s != "0001-01-01T00:00:00Z" && !s.is_empty()),
        exit_code: state.and_then(|s| s.exit_code),
        restart_count: resp.restart_count,
        pid: state.and_then(|s| s.pid),
        restart_policy: host_config.and_then(|h| {
            h.restart_policy
                .as_ref()
                .and_then(|p| p.name.map(|n| format!("{n:?}").to_lowercase()))
        }),
        env,
        ports,
        mounts,
        networks,
        network_aliases,
        user: config
            .and_then(|c| c.user.clone())
            .filter(|u| !u.is_empty()),
        memory_bytes: host_config.and_then(|h| h.memory).filter(|n| *n > 0),
        nano_cpus: host_config.and_then(|h| h.nano_cpus).filter(|n| *n > 0),
        pids_limit: host_config.and_then(|h| h.pids_limit).filter(|n| *n > 0),
        cap_drop: host_config
            .and_then(|h| h.cap_drop.clone())
            .unwrap_or_default(),
        cap_add: host_config
            .and_then(|h| h.cap_add.clone())
            .unwrap_or_default(),
        security_opt: host_config
            .and_then(|h| h.security_opt.clone())
            .unwrap_or_default(),
        read_only: host_config.and_then(|h| h.readonly_rootfs).unwrap_or(false),
        labels,
    }
}

/// Drain a stopped container's logs into separate stdout / stderr
/// strings. Each frame in the docker log stream is tagged by source so
/// we can split cleanly — handy for surfacing migration output.
async fn collect_stdout_stderr(
    docker: &Docker,
    name: &str,
) -> Result<(String, String), DockerError> {
    let opts = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .follow(false)
        .tail("all")
        .build();
    let mut stream = docker.logs(name, Some(opts));
    let mut stdout = String::new();
    let mut stderr = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(
                bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::Console { message },
            ) => {
                stdout.push_str(&String::from_utf8_lossy(&message));
            }
            Ok(bollard::container::LogOutput::StdErr { message }) => {
                stderr.push_str(&String::from_utf8_lossy(&message));
            }
            Ok(bollard::container::LogOutput::StdIn { .. }) => {}
            Err(e) => {
                return Err(DockerError::Invalid(format!(
                    "log stream error for {name}: {e}"
                )));
            }
        }
    }
    Ok((stdout, stderr))
}

/// Build a `ContainerCreateBody` for a one-shot curl probe container:
/// always uses `HEALTHCHECK_CURL_IMAGE`, attaches to the named docker
/// network as a guest, and disables auto-remove so the caller can
/// inspect logs / exit code before reaping.
fn probe_body(network: &str, cmd: Vec<String>) -> ContainerCreateBody {
    probe_body_with_image(HEALTHCHECK_CURL_IMAGE, network, cmd)
}

fn probe_body_with_image(image: &str, network: &str, cmd: Vec<String>) -> ContainerCreateBody {
    let mut endpoints = HashMap::new();
    endpoints.insert(network.to_string(), EndpointSettings::default());
    ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: Some(cmd),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints),
        }),
        host_config: Some(HostConfig {
            auto_remove: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    }
}
