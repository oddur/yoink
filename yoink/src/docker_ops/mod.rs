//! Docker operations behind a small async trait. `RealDockerOps` connects
//! to each remote daemon via bollard's `ssh://` transport (no remote shell
//! exec; bollard speaks the Docker Engine API over an ssh-tunnelled
//! socket). `FakeDockerOps` returns scripted typed responses so the rest
//! of the crate is testable without spawning real containers.

mod exec_io;
mod fake;
mod real;

pub use fake::{FakeDockerOps, RecordedCall};
pub use real::RealDockerOps;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use bollard::models::{ContainerCreateBody, ContainerSummary};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::base::host::Host;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("docker error on {host}: {source}")]
    Bollard {
        host: String,
        #[source]
        source: bollard::errors::Error,
    },
    #[error("connect failed for {host}: {source}")]
    Connect {
        host: String,
        #[source]
        source: bollard::errors::Error,
    },
    /// Pre-bollard ssh probe failed — surfaces a classified hint
    /// (e.g. Tailscale auth URL, permission denied with ssh-add
    /// pointer) instead of letting bollard hang for 120s waiting on
    /// an interactive prompt the operator can't see.
    #[error("ssh probe failed for {host}: {detail}")]
    SshProbe { host: String, detail: String },
    #[error("scripted fake exhausted: no response left for {0}")]
    FakeExhausted(&'static str),
    #[error("invalid response from docker: {0}")]
    Invalid(String),
}

impl DockerError {
    /// True when the request failed in the transport (a stale pooled
    /// connection, a dropped ssh session, an I/O error), not in the
    /// daemon. The request can have run, so only an idempotent
    /// operation can retry on it.
    #[must_use]
    pub fn is_transport(&self) -> bool {
        match self {
            Self::Bollard { source, .. } => {
                real::is_stale_transport(source)
                    || matches!(source, bollard::errors::Error::IOError { .. })
            }
            _ => false,
        }
    }

    /// True when the daemon answered the request and rejected it
    /// with a 4xx (no such network, no such image, bad request). The
    /// request reached the daemon, so this is not a transport blip.
    /// Transport errors, ssh failures and 5xx are not included.
    #[must_use]
    pub fn is_daemon_rejection(&self) -> bool {
        matches!(
            self,
            Self::Bollard {
                source: bollard::errors::Error::DockerResponseServerError { status_code, .. },
                ..
            } if (400..500).contains(status_code)
        )
    }

    /// True for an HTTP 409 Conflict from the daemon. For example, a
    /// probe cannot join the network namespace of a container that
    /// restarts at that moment.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(
            self,
            Self::Bollard {
                source: bollard::errors::Error::DockerResponseServerError {
                    status_code: 409,
                    ..
                },
                ..
            }
        )
    }
}

/// Outcome of [`DockerOps::remove_stopped_container`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedRemoval {
    /// The container existed, was not running, and is now removed.
    Removed,
    /// No container has that name.
    Absent,
    /// The container is running, restarting or paused. It was not
    /// touched.
    Running,
}

/// Hard cap on a single `build_image` call's wall-clock. Generous: an
/// xcaddy compile against a slow link with cold caches is rarely above
/// 5 min, so 15 covers the worst case while keeping CI runners from
/// sitting on a hung daemon for hours. Configurable later if anyone
/// actually hits it; a `pub const` keeps the value findable.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(900);

/// What we surface for a container after listing/inspect. Independent of
/// Docker container lifecycle state. Parsed from bollard's typed
/// `state` field at the boundary so downstream code matches an enum
/// instead of doing string-equality on `"running"`. `Unknown`
/// captures values bollard adds in future versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    Created,
    Running,
    Restarting,
    Removing,
    Paused,
    Exited,
    Dead,
    /// Anything bollard reports that we don't recognise. Round-trips
    /// through `Unknown` rather than mapping to a concrete variant
    /// because misclassifying a future docker state would be worse
    /// than rendering it as "unknown" for one yoink release.
    Unknown,
}

impl ContainerState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Removing => "removing",
            Self::Paused => "paused",
            Self::Exited => "exited",
            Self::Dead => "dead",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "created" => Self::Created,
            "running" => Self::Running,
            "restarting" => Self::Restarting,
            "removing" => Self::Removing,
            "paused" => Self::Paused,
            "exited" => Self::Exited,
            "dead" => Self::Dead,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for ContainerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for ContainerState {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Container healthcheck status. Bollard reports a parallel `health`
/// field on inspect responses and embeds the status in the
/// `status_text` line of `list_containers` (e.g. `Up 5 minutes
/// (healthy)`). yoink reads the latter today; this enum gives that
/// parse a typed home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Starting,
    Healthy,
    Unhealthy,
}

impl HealthStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// bollard's `ContainerSummary` so callers don't depend on bollard types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContainerInfo {
    pub host: String,
    pub name: String,
    /// Image reference as docker reports it — usually `repo:tag` for
    /// pulled images, sometimes `sha256:…` for content-addressed
    /// runs, or empty when docker hasn't yet resolved the image name.
    pub image: String,
    pub state: ContainerState,
    pub status_text: String,
    /// Container creation time as Unix epoch seconds. `None` when the
    /// daemon didn't return one (e.g. a freshly-created container the
    /// daemon hasn't fully indexed yet). Render-time formatting lives
    /// in `output::format_relative_time` so the dashboard can show
    /// "5m" / "2h" / "3d" rather than a raw epoch.
    pub created_unix: Option<i64>,
    pub yoink_service: Option<String>,
    pub yoink_version: Option<String>,
    pub yoink_spec_hash: Option<String>,
    /// Operator who ran `yoink up` for this container — value of
    /// `yoink.deployed-by`. `None` for containers from before the
    /// label was introduced.
    pub yoink_deployed_by: Option<String>,
    /// Unix-seconds timestamp the container was deployed, parsed
    /// from `yoink.deployed-at`. Used by `yoink history` to sort.
    pub yoink_deployed_at: Option<i64>,
    /// Docker networks this container is attached to (sorted, so
    /// the rendered display is stable).
    pub networks: Vec<String>,
    pub other_labels: BTreeMap<String, String>,
}

impl ContainerInfo {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state == ContainerState::Running
    }

    #[must_use]
    pub fn health_hint(&self) -> Option<HealthStatus> {
        let s = self.status_text.to_lowercase();
        if s.contains("(healthy)") {
            Some(HealthStatus::Healthy)
        } else if s.contains("(unhealthy)") {
            Some(HealthStatus::Unhealthy)
        } else if s.contains("(starting)") {
            Some(HealthStatus::Starting)
        } else {
            None
        }
    }

    #[must_use]
    pub fn from_summary(host: &str, summary: ContainerSummary) -> Self {
        let labels = summary.labels.unwrap_or_default();
        let yoink_service = labels.get("yoink.service").cloned();
        let yoink_version = labels.get("yoink.version").cloned();
        let yoink_spec_hash = labels.get("yoink.spec_hash").cloned();
        let yoink_deployed_by = labels.get("yoink.deployed-by").cloned();
        let yoink_deployed_at = labels
            .get("yoink.deployed-at")
            .and_then(|s| s.parse::<i64>().ok());
        let other_labels: BTreeMap<String, String> = labels
            .into_iter()
            .filter(|(k, _)| !k.starts_with("yoink."))
            .collect();
        let names = summary.names.unwrap_or_default();
        let name = names
            .first()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();
        let networks = summary
            .network_settings
            .and_then(|n| n.networks)
            .map(|m| {
                let mut keys: Vec<String> = m.into_keys().collect();
                keys.sort();
                keys
            })
            .unwrap_or_default();
        Self {
            host: host.to_string(),
            name,
            image: summary.image.unwrap_or_default(),
            state: summary.state.map_or(ContainerState::Unknown, |s| {
                ContainerState::parse(&format!("{s:?}"))
            }),
            status_text: summary.status.unwrap_or_default(),
            created_unix: summary.created,
            yoink_service,
            yoink_version,
            yoink_spec_hash,
            yoink_deployed_by,
            yoink_deployed_at,
            networks,
            other_labels,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerVersion {
    pub server_version: Option<String>,
    pub api_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
}

/// Host-level capacity + container counts (one-shot from `docker info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub n_cpu: Option<i64>,
    pub mem_total: Option<i64>,
    pub containers: Option<i64>,
    pub containers_running: Option<i64>,
    pub images: Option<i64>,
    pub kernel: Option<String>,
    pub operating_system: Option<String>,
}

/// One-shot snapshot of a container's CPU + memory utilization.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerStats {
    /// CPU% across all online CPUs — max ≈ `n_cpu * 100`.
    pub cpu_pct: f64,
    /// Resident memory in bytes.
    pub mem_used: i64,
    /// Limit in bytes if a memory cap is set, else None.
    pub mem_limit: Option<i64>,
    /// Cumulative bytes received across every interface, since the
    /// container started. Time-series consumers diff successive samples
    /// to get rates.
    pub net_rx_bytes: i64,
    /// Cumulative bytes transmitted, see `net_rx_bytes`.
    pub net_tx_bytes: i64,
    /// Cumulative bytes read from block devices.
    pub block_read_bytes: i64,
    /// Cumulative bytes written to block devices.
    pub block_write_bytes: i64,
    /// Number of pids running inside the container at sample time.
    pub pids: i64,
}

/// One image as the dashboard / `yoink images` show it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImageInfo {
    pub host: String,
    /// Short SHA-256 (first 12 hex chars).
    pub id: String,
    /// All `repo:tag` references this image is known by.
    pub repo_tags: Vec<String>,
    pub size_bytes: i64,
    /// Unix epoch seconds, if reported.
    pub created_unix: Option<i64>,
    /// `true` when no container references this image (eligible for prune).
    pub dangling: bool,
}

/// Single process row from `docker top` (output of `ps -ef` inside the
/// container). Column meanings vary by container-side ps; we surface
/// the daemon's reported titles verbatim alongside the values so the
/// operator can interpret them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProcessTable {
    pub titles: Vec<String>,
    pub processes: Vec<Vec<String>>,
}

/// Result of a `docker {images,volumes,networks} prune` call. Object
/// is uniform across all three so the TUI's prune-confirmation flow
/// can render the same toast format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Names / IDs reclaimed.
    pub reclaimed: Vec<String>,
    /// Bytes reclaimed (0 for prunes that don't free disk — networks).
    pub space_reclaimed_bytes: i64,
}

/// Realtime change notification from a host's docker daemon. Currently
/// we only surface container-scoped events; the `action` field is the
/// raw `docker events` action string (`start`, `die`, `health_status`,
/// `destroy`, …) so callers can decide which ones warrant a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerEvent {
    pub kind: DockerEventKind,
    pub action: String,
    pub container: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerEventKind {
    Container,
    Network,
    Other,
}

/// Handle to an interactive (TTY) docker exec or attached sidecar
/// container. Output is the merged PTY byte stream (stdout+stderr
/// combined when `tty: true`); writing to `stdin` sends keystrokes.
/// `kind` tells the caller which resize / cleanup endpoint to use.
pub struct ExecSession {
    /// Exec ID for `Exec` kind, or container name for `Sidecar` kind.
    pub id: String,
    pub kind: ExecKind,
    pub stdin: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    pub output: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, DockerError>> + Send>,
    >,
}

/// Whether an `ExecSession` is backed by a docker exec (resize via
/// `resize_exec`) or by an attached sidecar container (resize via
/// `resize_container_tty`, cleanup via `force_remove_container`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Exec,
    Sidecar,
}

impl std::fmt::Debug for ExecSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecSession")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Rich inspect view of one container. The fields here are what the
/// TUI's container-detail pane renders — image + command + the
/// runtime knobs (ports, mounts, env, networks). Mirrors what
/// `docker inspect` would return but trimmed to the bits an operator
/// reads at a glance.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ContainerDetail {
    pub name: String,
    pub image: Option<String>,
    pub image_id: Option<String>,
    pub command: Option<String>,
    pub working_dir: Option<String>,
    pub state: Option<String>,
    pub status: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub restart_count: Option<i64>,
    pub pid: Option<i64>,
    pub restart_policy: Option<String>,
    /// Sorted KEY=VALUE pairs. The TUI redacts values whose key
    /// contains common secret-ish substrings (TOKEN/SECRET/…).
    pub env: Vec<String>,
    /// `"host_port:container_port/proto"` entries.
    pub ports: Vec<String>,
    /// "source → target [mode]" entries.
    pub mounts: Vec<String>,
    /// Network names this container is attached to.
    pub networks: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Effective user (`container.config.user`). Empty string when
    /// the image's USER is in effect.
    pub user: Option<String>,
    /// Memory limit in bytes. `None` → uncapped.
    pub memory_bytes: Option<i64>,
    /// CPU limit in nano-CPUs (1 core = 1e9). `None` → uncapped.
    pub nano_cpus: Option<i64>,
    /// pids cgroup limit. `None` → unlimited.
    pub pids_limit: Option<i64>,
    /// Linux capabilities dropped (`["ALL"]` is yoink's secure default).
    pub cap_drop: Vec<String>,
    /// Linux capabilities re-added on top of `cap_drop`.
    pub cap_add: Vec<String>,
    /// `--security-opt` entries (e.g. `no-new-privileges:true`).
    pub security_opt: Vec<String>,
    /// `--read-only` (immutable rootfs).
    pub read_only: bool,
}

impl ContainerDetail {
    /// Parse `env: Vec<"KEY=VALUE">` into a key/value map. Lines
    /// without `=` (rare but legal in docker) are dropped — the
    /// callers (TUI redaction, dry-run diff) only care about keyed
    /// values. Cheap; recompute at call sites rather than caching.
    #[must_use]
    pub fn env_map(&self) -> BTreeMap<String, String> {
        self.env
            .iter()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

/// One docker network as the dashboard / `yoink networks` show it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NetworkInfo {
    pub host: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub internal: bool,
    /// Number of containers attached (parsed from inspect; some
    /// drivers don't report this so it can be 0 even when in use).
    pub container_count: usize,
}

/// One docker volume.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeInfo {
    pub host: String,
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    /// Created-at timestamp from docker (RFC-3339 string), if known.
    pub created: Option<String>,
}

/// Result of a one-shot container run (e.g. a pre-deploy hook).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub container: String,
    pub stream: LogStream,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// Body shape for `DockerOps::load_image` — a chunked stream of
/// `Bytes` (typically wrapping `docker save`'s stdout via
/// `tokio_util::io::ReaderStream`). Pinned + boxed so the trait
/// stays object-safe.
pub type ImageTarStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>,
>;

/// The async surface every consumer of Docker uses. Methods take a `&Host`
/// so the same trait object can drive multiple remote daemons.
#[async_trait]
pub trait DockerOps: Send + Sync {
    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError>;

    /// Path to the SSH private key to use when subprocess-spawning
    /// `ssh` for this host (e.g. unregistry's tunnel + probe). Returns
    /// `Some` only when the host declared `ssh_key_secret:` and the
    /// key was decrypted into a tempfile by `ssh::keys::prepare`.
    /// Default `None` for `FakeDockerOps` and the no-key case.
    fn ssh_keyfile(&self, _host: &Host) -> Option<std::path::PathBuf> {
        None
    }

    /// Host-level info from `docker info` — capacity, container counts.
    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError>;

    /// One-shot snapshot of CPU% + memory used for a single container.
    async fn container_stats(&self, host: &Host, name: &str)
    -> Result<ContainerStats, DockerError>;

    /// Rich inspect for a single container — image, command, env,
    /// ports, mounts, networks, restart info. Used by the TUI's
    /// container-detail pane.
    async fn inspect_container(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<ContainerDetail, DockerError>;

    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError>;

    /// List every docker network on `host`. Used by `yoink networks`.
    async fn list_networks(&self, host: &Host) -> Result<Vec<NetworkInfo>, DockerError>;

    /// Connect an already-running container to an additional docker
    /// network. Used post-start for multi-network attachment because
    /// docker's create-time `endpoints_config` can silently drop
    /// entries that don't match `host_config.network_mode`.
    /// Aliases ensure the container resolves by its name on the new
    /// network too. Idempotent: already-connected returns Ok.
    async fn connect_container_network(
        &self,
        host: &Host,
        container: &str,
        network: &str,
        aliases: &[String],
    ) -> Result<(), DockerError>;

    /// List every docker volume on `host`. Used by `yoink volumes`.
    async fn list_volumes(&self, host: &Host) -> Result<Vec<VolumeInfo>, DockerError>;

    /// List every image cached on `host`. The TUI's Resources pane
    /// renders this as a sortable table with size + age columns.
    /// Default impl returns an empty list so callers (Fake / future
    /// alt backends) keep working without an explicit override.
    async fn list_images(&self, _host: &Host) -> Result<Vec<ImageInfo>, DockerError> {
        Ok(Vec::new())
    }

    /// Remove a single image by id or `repo:tag`. `force=true` removes
    /// even when there are tag references; `false` errors out when
    /// other tags point at the same image id.
    async fn remove_image(
        &self,
        _host: &Host,
        _name: &str,
        _force: bool,
    ) -> Result<(), DockerError> {
        Err(DockerError::Invalid("remove_image not supported".into()))
    }

    /// Prune unused images. `dangling_only=true` matches `docker image
    /// prune` (default — only `<none>:<none>` layers); `false` matches
    /// `docker image prune -a` (every image with no live container
    /// reference).
    async fn prune_images(
        &self,
        _host: &Host,
        _dangling_only: bool,
    ) -> Result<PruneReport, DockerError> {
        Ok(PruneReport::default())
    }

    /// Remove one volume. `force` corresponds to `docker volume rm -f`.
    async fn remove_volume(
        &self,
        _host: &Host,
        _name: &str,
        _force: bool,
    ) -> Result<(), DockerError> {
        Err(DockerError::Invalid("remove_volume not supported".into()))
    }

    /// `docker volume prune` — drop volumes that no container references.
    async fn prune_volumes(&self, _host: &Host) -> Result<PruneReport, DockerError> {
        Ok(PruneReport::default())
    }

    /// Remove one user-defined network. Default networks (`bridge`,
    /// `host`, `none`) reject removal.
    async fn remove_network(&self, _host: &Host, _name: &str) -> Result<(), DockerError> {
        Err(DockerError::Invalid("remove_network not supported".into()))
    }

    /// `docker network prune` — drop user-defined networks with no
    /// attached containers.
    async fn prune_networks(&self, _host: &Host) -> Result<PruneReport, DockerError> {
        Ok(PruneReport::default())
    }

    /// `docker restart` with the standard 10s SIGTERM grace period.
    /// Implemented separately from stop+start so the daemon does the
    /// state transition atomically (no observer-visible "stopped" gap).
    async fn restart_container(
        &self,
        _host: &Host,
        _name: &str,
        _drain: Duration,
    ) -> Result<(), DockerError> {
        Err(DockerError::Invalid(
            "restart_container not supported".into(),
        ))
    }

    /// `docker top` — list processes running inside the container.
    /// Used by the container detail pane's `p` gesture.
    async fn top_container(&self, _host: &Host, _name: &str) -> Result<ProcessTable, DockerError> {
        Ok(ProcessTable {
            titles: Vec::new(),
            processes: Vec::new(),
        })
    }

    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError>;

    /// Stream a `docker save`-style tarball into the host's docker
    /// daemon (`POST /images/load`). Body is a `Stream<Bytes>` so
    /// memory stays bounded by the chunk size, not the image size.
    async fn load_image(&self, host: &Host, body: ImageTarStream) -> Result<(), DockerError>;

    /// `true` if `image:tag` is already present in the host's local
    /// image cache (no pull needed). Used to skip redundant pulls
    /// after a prefetch pass. Errors degrade to `false` so a flaky
    /// docker daemon doesn't silently elide pulls.
    async fn image_present(&self, host: &Host, image: &str, tag: &str)
    -> Result<bool, DockerError>;

    /// Build an image on `host` from an in-memory tar context (a
    /// Dockerfile + any files it `COPY`s). Tag is what the resulting
    /// image is reachable as locally — e.g. `yoink-caddy:abc123`. The
    /// underlying call is bollard `POST /build` over the same
    /// SSH-tunnelled connection used by `pull_image`. Default impl
    /// errors so test fakes that don't need it stay terse.
    async fn build_image(
        &self,
        _host: &Host,
        _tag: &str,
        _context_tar: bytes::Bytes,
    ) -> Result<(), DockerError> {
        Err(DockerError::Invalid("build_image not supported".into()))
    }

    async fn list_containers_by_label(
        &self,
        host: &Host,
        label: &str,
    ) -> Result<Vec<ContainerInfo>, DockerError>;

    /// Every currently-running container on the host, regardless of label.
    /// Used by the Hosts pane to sum CPU% + memory across the whole daemon.
    async fn list_running_containers(&self, host: &Host)
    -> Result<Vec<ContainerInfo>, DockerError>;

    async fn create_container(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<String, DockerError>;

    async fn start_container(&self, host: &Host, name: &str) -> Result<(), DockerError>;

    async fn stop_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError>;

    async fn force_remove_container(&self, host: &Host, name: &str) -> Result<(), DockerError>;

    /// Remove a container only if it is not running. The daemon makes
    /// the decision in one step (a remove without `force` is refused
    /// for a running, restarting or paused container), so there is no
    /// window between a state check and the remove in which the
    /// container could be killed.
    async fn remove_stopped_container(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<StoppedRemoval, DockerError>;

    /// Send SIGKILL to a container without removing it. The exited
    /// container stays around for `docker logs` / `inspect` and can
    /// be reaped later. Used by `yoink kill` for the "stop responding
    /// NOW" case where the configured drain isn't fast enough.
    async fn kill_container(&self, host: &Host, name: &str) -> Result<(), DockerError>;

    /// Run a one-shot curl container against `target:port/path` on
    /// `network`. Returns the HTTP status code curl observed (parsed from
    /// stdout) or 0 if curl couldn't reach the service.
    async fn healthcheck(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
        path: &str,
    ) -> Result<u16, DockerError>;

    /// Run `cmd` inside `container` to completion (one-shot exec).
    /// Captures stdout + stderr and the exit code. Used both by the
    /// host advisory lock heartbeat (ignores the output, just needs
    /// success/fail) and by `yoink exec` (prints output to the
    /// operator's terminal, mirrors the exit code).
    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<OneShotResult, DockerError>;

    /// Run a one-shot curl container that shares the network namespace
    /// of `container` (`network_mode: container:<container>`), so
    /// `127.0.0.1` in the URL is the loopback of `container`. `args`
    /// go to curl and must start with an option (the curl image
    /// entrypoint adds `curl` then). Collects stdout, stderr and the
    /// exit code, and removes the probe on every path. The probe image
    /// is the one [`Self::healthcheck`] uses, so the target image needs
    /// no HTTP client of its own.
    async fn curl_in_netns(
        &self,
        host: &Host,
        container: &str,
        args: Vec<String>,
    ) -> Result<OneShotResult, DockerError>;

    /// [`Self::exec_oneshot`] that also writes `stdin` to the process
    /// and then closes its stdin, so the process reads to EOF. The
    /// proxy admin client sends the Caddy config this way: the config
    /// carries certificate keys, so it must not go on the command line.
    async fn exec_with_stdin(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
        stdin: Vec<u8>,
    ) -> Result<OneShotResult, DockerError>;

    /// Start an interactive (PTY-mode) docker exec. The returned
    /// `ExecSession` lets the caller stream raw terminal bytes both ways:
    /// the embedded TUI shell drives `vt100::Parser` from `output` and
    /// forwards key events into `stdin`. Initial size is set in the same
    /// call so the in-container shell sees the right dimensions on the
    /// first prompt.
    async fn exec_interactive(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
        rows: u16,
        cols: u16,
    ) -> Result<ExecSession, DockerError>;

    /// Tell the daemon the PTY size has changed. Called whenever the
    /// embedded shell panel is resized so applications like `top` or
    /// `vim` re-flow.
    async fn resize_exec(
        &self,
        host: &Host,
        exec_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), DockerError>;

    /// Same idea as `resize_exec` but for an attached sidecar
    /// container (different docker endpoint, same payload).
    async fn resize_container_tty(
        &self,
        host: &Host,
        container: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), DockerError>;

    /// Start a "debug sidecar" container that shares the target's PID
    /// and network namespaces, attach to it interactively, and return
    /// the byte streams. Used as a fallback when the target is
    /// distroless or otherwise lacks a usable shell — the sidecar can
    /// `ps`, `ss`, peek at `/proc/<pid>/root/...`, etc., without
    /// modifying the production image. The caller is responsible for
    /// `force_remove_container` on shutdown; `auto_remove` handles the
    /// happy-path cleanup when the user runs `exit` / Ctrl-D inside.
    async fn start_debug_sidecar(
        &self,
        host: &Host,
        target_container: &str,
        image: &str,
        rows: u16,
        cols: u16,
    ) -> Result<ExecSession, DockerError>;

    /// TCP-only liveness probe — used for services that don't speak
    /// HTTP (redis on :6379) or where the HTTP path is impractical to
    /// configure (caddy with TLS-only :443). Succeeds when a TCP
    /// connect to `target:port` on `network` completes; surfaces an
    /// error for any other outcome.
    async fn healthcheck_tcp(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
    ) -> Result<(), DockerError>;

    /// Subscribe to the host's `docker events` stream. The returned
    /// receiver yields `DockerEvent`s as they happen on the daemon —
    /// container start/stop/die, network create/remove, etc. The TUI
    /// uses this to refresh panes the instant something changes
    /// (instead of waiting for the next polling tick). The spawned
    /// background task ends when the receiver is dropped.
    async fn subscribe_events(
        &self,
        host: &Host,
    ) -> Result<mpsc::UnboundedReceiver<DockerEvent>, DockerError>;

    /// Run a container to completion. Creates, starts, waits for the
    /// container to exit, then captures its stdout + stderr and removes
    /// it. Used for pre-deploy hooks (e.g. database migrations).
    async fn run_one_shot(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<OneShotResult, DockerError>;

    /// Last `lines` log lines (stdout+stderr) from a stopped or running
    /// container, useful for "why did this fail" diagnostics.
    async fn fetch_recent_logs(
        &self,
        host: &Host,
        name: &str,
        lines: u32,
    ) -> Result<Vec<String>, DockerError>;

    /// Open a follow stream of log lines. `tail_lines` is the number of
    /// historical lines to backfill before streaming new ones (`0` for
    /// "only new"). Returns an unbounded receiver; the stream's spawned
    /// task ends when the container stops or the receiver is dropped.
    async fn open_log_stream(
        &self,
        host: &Host,
        name: &str,
        tail_lines: u32,
    ) -> Result<mpsc::UnboundedReceiver<LogLine>, DockerError>;
}

/// `parse_inspect` (and `ContainerInfo`'s port rendering) format each
/// port-bindings entry as `"<host_port>:<container_port>/<proto>"`.
/// Find the entry whose container port matches `wanted` and return
/// the host port. The unregistry transport uses it to look up the
/// ephemeral published port of its sidecar.
#[must_use]
pub fn parse_published_port(entries: &[String], wanted: u16) -> Option<u16> {
    let suffix = format!(":{wanted}/");
    for entry in entries {
        if let Some(idx) = entry.find(&suffix) {
            let host_port = &entry[..idx];
            if let Ok(p) = host_port.parse::<u16>() {
                return Some(p);
            }
        }
    }
    None
}

/// Cheap random hex suffix for one-shot probe container names so two
/// concurrent deploys can't collide.
pub(crate) fn rand_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{nanos:08x}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        }
    }

    #[tokio::test]
    async fn fake_records_calls_and_returns_scripted_responses() {
        let ops = FakeDockerOps::new();
        ops.push_version(Ok(DockerVersion {
            server_version: Some("28.0.1".into()),
            api_version: Some("1.49".into()),
            os: Some("linux".into()),
            arch: Some("amd64".into()),
        }));

        let v = ops.version(&host()).await.unwrap();
        assert_eq!(v.server_version.as_deref(), Some("28.0.1"));
        let calls = ops.calls();
        assert_eq!(calls, vec![RecordedCall::Version(host())]);
    }

    #[tokio::test]
    async fn fake_returns_exhausted_error_when_out_of_responses() {
        let ops = FakeDockerOps::new();
        let err = ops.version(&host()).await.unwrap_err();
        assert!(matches!(err, DockerError::FakeExhausted("version")));
    }

    #[test]
    fn container_info_parses_yoink_labels() {
        let mut labels = HashMap::new();
        labels.insert("yoink.service".into(), "app-a".into());
        labels.insert("yoink.version".into(), "a1b2c3d".into());
        labels.insert("caddy".into(), "app-a.example.com".into());
        let summary = ContainerSummary {
            names: Some(vec!["/app-a-a1b2c3d".into()]),
            state: Some(bollard::models::ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 1h (healthy)".into()),
            labels: Some(labels),
            ..Default::default()
        };
        let info = ContainerInfo::from_summary("host-a", summary);
        assert_eq!(info.name, "app-a-a1b2c3d");
        assert!(info.is_running());
        assert_eq!(info.health_hint(), Some(HealthStatus::Healthy));
        assert_eq!(info.yoink_service.as_deref(), Some("app-a"));
        assert_eq!(info.yoink_version.as_deref(), Some("a1b2c3d"));
        assert_eq!(
            info.other_labels.get("caddy"),
            Some(&"app-a.example.com".to_string())
        );
    }
}
