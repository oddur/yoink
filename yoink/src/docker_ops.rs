//! Docker operations behind a small async trait. `RealDockerOps` connects
//! to each remote daemon via bollard's `ssh://` transport (no remote shell
//! exec; bollard speaks the Docker Engine API over an ssh-tunnelled
//! socket). `FakeDockerOps` returns scripted typed responses so the rest
//! of the crate is testable without spawning real containers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, ContainerSummary, EndpointSettings, HostConfig, NetworkCreateRequest,
    NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, EventsOptionsBuilder,
    ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures_util::StreamExt;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::warn;

use crate::config::HostConfig as YoinkHost;

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

/// Minimal host identity used by the trait; converts cheaply from
/// [`crate::config::HostConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub user: String,
    pub address: String,
}

impl Host {
    /// Sentinel address marking the synthetic host that talks to the
    /// local docker socket instead of going over ssh. One constant so
    /// the magic isn't a stringly-typed sprinkle across the codebase.
    pub const LOCAL_ADDRESS: &'static str = "local";

    #[must_use]
    pub fn ssh_url(&self) -> String {
        format!("ssh://{}@{}", self.user, self.address)
    }

    /// True for the magical synthetic local host — bypasses ssh and
    /// uses bollard's platform-default unix socket / npipe transport.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.address == Self::LOCAL_ADDRESS
    }

    /// Construct the synthetic local host. The empty `user` is fine —
    /// the local-socket transport ignores it. Symmetric with
    /// [`Self::is_local`].
    #[must_use]
    pub fn local() -> Self {
        Self {
            user: String::new(),
            address: Self::LOCAL_ADDRESS.to_string(),
        }
    }
}

/// Hard cap on a single `build_image` call's wall-clock. Generous: an
/// xcaddy compile against a slow link with cold caches is rarely above
/// 5 min, so 15 covers the worst case while keeping CI runners from
/// sitting on a hung daemon for hours. Configurable later if anyone
/// actually hits it; a `pub const` keeps the value findable.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(900);

impl From<&YoinkHost> for Host {
    fn from(h: &YoinkHost) -> Self {
        Self {
            user: h.user.clone(),
            address: h.address.clone(),
        }
    }
}

/// What we surface for a container after listing/inspect. Independent of
/// bollard's `ContainerSummary` so callers don't depend on bollard types.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContainerInfo {
    pub host: String,
    pub name: String,
    /// Image reference as docker reports it — usually `repo:tag` for
    /// pulled images, sometimes `sha256:…` for content-addressed
    /// runs, or empty when docker hasn't yet resolved the image name.
    pub image: String,
    pub state: String,
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
        self.state.eq_ignore_ascii_case("running")
    }

    #[must_use]
    pub fn health_hint(&self) -> Option<&'static str> {
        let s = self.status_text.to_lowercase();
        if s.contains("(healthy)") {
            Some("healthy")
        } else if s.contains("(unhealthy)") {
            Some("unhealthy")
        } else if s.contains("(starting)") {
            Some("starting")
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
            state: summary
                .state
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_default(),
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
    /// key was decrypted into a tempfile by `ssh_keys::prepare`.
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
/// hyper pool entry; over a long TUI session the pool grows and so
/// does the openssh process / fd footprint. Tailscale SSH server
/// behavior also changed in recent releases (see 1.94 → 1.96), so
/// idle ssh-tunnelled sockets go stale faster than the pool reaper
/// notices, surfacing as `client error (Connect)` on the first call
/// after a brief idle. Periodic eviction tears down the old hyper
/// Client (and therefore every openssh master it pooled), bounding
/// fd usage and the staleness window.
///
/// Tuned to be much longer than typical bursty call gaps so an active
/// `yoink up` reuses the same handle, but short enough that an idle
/// TUI cycles state in single-digit minutes.
const CLIENT_MAX_AGE: Duration = Duration::from_secs(120);

/// One bollard `Docker` per host with the time it was created.
/// Drives `CLIENT_MAX_AGE` eviction in `client_for`.
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
    /// `ssh_key_secret:`. Built by `crate::ssh_keys::prepare` from the
    /// secrets bundle; passed into bollard `connect_with_ssh` as the
    /// `keypair_path` arg, and added to `ssh_probe`'s `ssh -i`.
    key_manager: Option<std::sync::Arc<crate::ssh_keys::KeyManager>>,
}

impl RealDockerOps {
    #[must_use]
    pub fn new() -> Self {
        Self::with_key_manager(None)
    }

    #[must_use]
    pub fn with_key_manager(km: Option<std::sync::Arc<crate::ssh_keys::KeyManager>>) -> Self {
        Self {
            clients: tokio::sync::Mutex::new(HashMap::new()),
            timeout_secs: 120,
            key_manager: km,
        }
    }

    async fn client_for(&self, host: &Host) -> Result<Docker, DockerError> {
        let key = host.ssh_url();
        let mut clients = self.clients.lock().await;
        if let Some(entry) = clients.get(&key) {
            // Local-socket transport doesn't share the ssh-pool
            // staleness / fd-leak failure modes — keep its handle for
            // the whole process lifetime.
            if host.is_local() || entry.created.elapsed() < CLIENT_MAX_AGE {
                return Ok(entry.docker.clone());
            }
            clients.remove(&key);
        }
        // The magical "local" host (address == "local", no user)
        // routes to the local docker socket via bollard's
        // platform-default unix socket / npipe transport. Lets the
        // operator run yoink against their laptop's docker without
        // editing yoink.yaml.
        let docker = if host.is_local() {
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
            crate::ssh_probe::probe(host, keyfile.as_deref())
                .await
                .map_err(|detail| DockerError::SshProbe {
                    host: host.address.clone(),
                    detail,
                })?;
            Docker::connect_with_ssh(
                &key,
                self.timeout_secs,
                bollard::API_DEFAULT_VERSION,
                keyfile,
            )
            .map_err(|source| DockerError::Connect {
                host: host.address.clone(),
                source,
            })?
        };
        clients.insert(
            key.clone(),
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
    /// Why retry: bollard's ssh transport keeps an idle hyper pool
    /// of openssh-mux'd `docker system dial-stdio` sessions. Tailscale
    /// SSH on the host (`tailscaled`'s built-in SSH server, not
    /// OpenSSH) closes idle dial-stdio sessions much sooner than its
    /// keepalive hints suggest, so any pooled connection that's been
    /// idle for more than a couple of seconds can produce
    /// `client error (Connect)` or `connection closed before message
    /// completed` on first reuse — surfacing as transient red blips
    /// in the TUI even though the next call would succeed.
    /// Evicting the cached `Docker` (and therefore its hyper pool +
    /// underlying ssh master) and dialing fresh papers over the
    /// stale-pool race transparently. We retry once and only on
    /// transport errors; real docker API errors propagate as before.
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
                match stream.next().await {
                    Some(Ok(s)) => Ok(s),
                    Some(Err(e)) => Err(e),
                    None => Err(bollard::errors::Error::IOError {
                        err: std::io::Error::other("stats stream yielded no item"),
                    }),
                }
            })
            .await?;
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
                    docker.create_network(req).await?;
                    Ok(true)
                }
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
        aliases: &[String],
    ) -> Result<(), DockerError> {
        self.with_retry(host, |docker| async move {
            let req = bollard::models::NetworkConnectRequest {
                container: container.to_string(),
                endpoint_config: Some(bollard::models::EndpointSettings {
                    aliases: if aliases.is_empty() {
                        None
                    } else {
                        Some(aliases.to_vec())
                    },
                    ..Default::default()
                }),
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
        let reference = crate::docker::image_reference(image, tag);
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
        self.with_retry(host, |docker| async move {
            let opts = RemoveContainerOptionsBuilder::new().force(true).build();
            // 404 = container didn't exist, treat as idempotent success.
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
        let logs = self.fetch_recent_logs(host, &probe_name, 32).await?;
        let _ = self.force_remove_container(host, &probe_name).await;

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
        // Pull the busybox image best-effort. Idempotent — no-op
        // when already cached. Without this the first probe on a
        // fresh daemon 404s on create_container.
        let _ = self.pull_image(host, "busybox", "1.37", None).await;
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
        let docker = self.client_for(host).await?;
        let (stdout, stderr) = collect_stdout_stderr(&docker, name).await?;
        let _ = self.force_remove_container(host, name).await;
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

/// `parse_inspect` (and `ContainerInfo`'s port rendering) format each
/// port-bindings entry as `"<host_port>:<container_port>/<proto>"`.
/// Find the entry whose container port matches `wanted` and return
/// the host port. Used by both the unregistry transport and the
/// proxy admin client to look up an ephemeral published port.
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

// ───── FakeDockerOps ────────────────────────────────────────────────────

/// Test double. Push scripted typed responses keyed by op kind, then assert
/// against `calls()` afterwards.
#[derive(Default)]
pub struct FakeDockerOps {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    version: VecDeque<Result<DockerVersion, DockerError>>,
    host_info: VecDeque<Result<HostInfo, DockerError>>,
    container_stats: VecDeque<Result<ContainerStats, DockerError>>,
    ensure_network: VecDeque<Result<bool, DockerError>>,
    pull_image: VecDeque<Result<(), DockerError>>,
    load_image: VecDeque<Result<(), DockerError>>,
    build_image: VecDeque<Result<(), DockerError>>,
    /// `image:tag` → presence flag. Defaults `false` so existing tests
    /// (which expect pulls to actually fire) keep working. After a
    /// successful `build_image` the entry is upserted to `true`.
    images_present: HashMap<String, bool>,
    list_containers: VecDeque<Result<Vec<ContainerInfo>, DockerError>>,
    create_container: VecDeque<Result<String, DockerError>>,
    start_container: VecDeque<Result<(), DockerError>>,
    stop_container: VecDeque<Result<(), DockerError>>,
    force_remove_container: VecDeque<Result<(), DockerError>>,
    healthcheck: VecDeque<Result<u16, DockerError>>,
    healthcheck_tcp: VecDeque<Result<(), DockerError>>,
    exec_oneshot: VecDeque<Result<OneShotResult, DockerError>>,
    fetch_logs: VecDeque<Result<Vec<String>, DockerError>>,
    one_shot: VecDeque<Result<OneShotResult, DockerError>>,
    event_streams: VecDeque<Vec<DockerEvent>>,
    log_streams: VecDeque<Vec<LogLine>>,
    calls: Vec<RecordedCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    Version(Host),
    HostInfo(Host),
    ContainerStats(Host, String),
    EnsureNetwork(Host, String),
    PullImage(Host, String, String),
    LoadImage(Host),
    BuildImage(Host, String, usize),
    ListContainersByLabel(Host, String),
    ListRunningContainers(Host),
    CreateContainer(Host, String),
    StartContainer(Host, String),
    StopContainer(Host, String, Duration),
    ForceRemoveContainer(Host, String),
    Healthcheck(Host, String, String, u16, String),
    HealthcheckTcp(Host, String, String, u16),
    ExecOneshot(Host, String, Vec<String>),
    FetchRecentLogs(Host, String, u32),
    OpenLogStream(Host, String),
    RunOneShot(Host, String),
    SubscribeEvents(Host),
}

impl FakeDockerOps {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("FakeDockerOps mutex poisoned")
    }

    pub fn push_version(&self, v: Result<DockerVersion, DockerError>) {
        self.lock().version.push_back(v);
    }
    pub fn push_host_info(&self, v: Result<HostInfo, DockerError>) {
        self.lock().host_info.push_back(v);
    }
    pub fn push_container_stats(&self, v: Result<ContainerStats, DockerError>) {
        self.lock().container_stats.push_back(v);
    }
    pub fn push_ensure_network(&self, v: Result<bool, DockerError>) {
        self.lock().ensure_network.push_back(v);
    }
    pub fn push_pull_image(&self, v: Result<(), DockerError>) {
        self.lock().pull_image.push_back(v);
    }
    pub fn push_build_image(&self, v: Result<(), DockerError>) {
        self.lock().build_image.push_back(v);
    }
    /// Mark `image:tag` as already present so `image_present` returns
    /// `true` and the caller skips its pull/build.
    pub fn mark_image_present(&self, image: &str, tag: &str) {
        let key = crate::docker::image_reference(image, tag);
        self.lock().images_present.insert(key, true);
    }
    pub fn push_list_containers(&self, v: Result<Vec<ContainerInfo>, DockerError>) {
        self.lock().list_containers.push_back(v);
    }
    pub fn push_create_container(&self, v: Result<String, DockerError>) {
        self.lock().create_container.push_back(v);
    }
    pub fn push_start_container(&self, v: Result<(), DockerError>) {
        self.lock().start_container.push_back(v);
    }
    pub fn push_stop_container(&self, v: Result<(), DockerError>) {
        self.lock().stop_container.push_back(v);
    }
    pub fn push_force_remove(&self, v: Result<(), DockerError>) {
        self.lock().force_remove_container.push_back(v);
    }
    pub fn push_healthcheck(&self, v: Result<u16, DockerError>) {
        self.lock().healthcheck.push_back(v);
    }
    pub fn push_healthcheck_tcp(&self, v: Result<(), DockerError>) {
        self.lock().healthcheck_tcp.push_back(v);
    }
    pub fn push_exec_oneshot(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().exec_oneshot.push_back(v);
    }
    pub fn push_fetch_logs(&self, v: Result<Vec<String>, DockerError>) {
        self.lock().fetch_logs.push_back(v);
    }
    pub fn push_log_stream(&self, lines: Vec<LogLine>) {
        self.lock().log_streams.push_back(lines);
    }
    pub fn push_one_shot(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().one_shot.push_back(v);
    }
    pub fn push_event_stream(&self, events: Vec<DockerEvent>) {
        self.lock().event_streams.push_back(events);
    }

    #[must_use]
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock().calls.clone()
    }
}

fn pop<T>(q: &mut VecDeque<Result<T, DockerError>>, name: &'static str) -> Result<T, DockerError> {
    q.pop_front()
        .unwrap_or(Err(DockerError::FakeExhausted(name)))
}

#[async_trait]
impl DockerOps for FakeDockerOps {
    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::Version(host.clone()));
        pop(&mut s.version, "version")
    }
    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::HostInfo(host.clone()));
        pop(&mut s.host_info, "host_info")
    }
    async fn container_stats(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<ContainerStats, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::ContainerStats(host.clone(), name.into()));
        pop(&mut s.container_stats, "container_stats")
    }
    async fn inspect_container(
        &self,
        _host: &Host,
        name: &str,
    ) -> Result<ContainerDetail, DockerError> {
        Ok(ContainerDetail {
            name: name.into(),
            ..Default::default()
        })
    }
    async fn list_networks(&self, _host: &Host) -> Result<Vec<NetworkInfo>, DockerError> {
        Ok(Vec::new())
    }
    async fn connect_container_network(
        &self,
        _host: &Host,
        _container: &str,
        _network: &str,
        _aliases: &[String],
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn list_volumes(&self, _host: &Host) -> Result<Vec<VolumeInfo>, DockerError> {
        Ok(Vec::new())
    }
    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::EnsureNetwork(host.clone(), network.into()));
        pop(&mut s.ensure_network, "ensure_network")
    }
    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        _credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::PullImage(
            host.clone(),
            image.into(),
            tag.into(),
        ));
        pop(&mut s.pull_image, "pull_image")
    }
    async fn image_present(
        &self,
        _host: &Host,
        image: &str,
        tag: &str,
    ) -> Result<bool, DockerError> {
        let key = crate::docker::image_reference(image, tag);
        Ok(*self.lock().images_present.get(&key).unwrap_or(&false))
    }
    async fn load_image(&self, host: &Host, _body: ImageTarStream) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::LoadImage(host.clone()));
        pop(&mut s.load_image, "load_image")
    }
    async fn build_image(
        &self,
        host: &Host,
        tag: &str,
        context_tar: bytes::Bytes,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::BuildImage(
            host.clone(),
            tag.into(),
            context_tar.len(),
        ));
        let result = pop(&mut s.build_image, "build_image");
        if result.is_ok() {
            // Successful build leaves the image present for subsequent
            // `image_present` checks — mirrors real docker behavior.
            s.images_present.insert(tag.into(), true);
        }
        result
    }
    async fn list_containers_by_label(
        &self,
        host: &Host,
        label: &str,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ListContainersByLabel(
            host.clone(),
            label.into(),
        ));
        pop(&mut s.list_containers, "list_containers_by_label")
    }
    async fn list_running_containers(
        &self,
        host: &Host,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::ListRunningContainers(host.clone()));
        pop(&mut s.list_containers, "list_running_containers")
    }
    async fn create_container(
        &self,
        host: &Host,
        name: &str,
        _body: ContainerCreateBody,
    ) -> Result<String, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::CreateContainer(host.clone(), name.into()));
        pop(&mut s.create_container, "create_container")
    }
    async fn start_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::StartContainer(host.clone(), name.into()));
        pop(&mut s.start_container, "start_container")
    }
    async fn stop_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::StopContainer(
            host.clone(),
            name.into(),
            drain,
        ));
        pop(&mut s.stop_container, "stop_container")
    }
    async fn force_remove_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ForceRemoveContainer(
            host.clone(),
            name.into(),
        ));
        pop(&mut s.force_remove_container, "force_remove_container")
    }
    async fn kill_container(&self, _host: &Host, _name: &str) -> Result<(), DockerError> {
        Ok(())
    }
    async fn healthcheck(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
        path: &str,
    ) -> Result<u16, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::Healthcheck(
            host.clone(),
            network.into(),
            target.into(),
            port,
            path.into(),
        ));
        pop(&mut s.healthcheck, "healthcheck")
    }
    async fn healthcheck_tcp(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::HealthcheckTcp(
            host.clone(),
            network.into(),
            target.into(),
            port,
        ));
        pop(&mut s.healthcheck_tcp, "healthcheck_tcp")
    }
    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<OneShotResult, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ExecOneshot(
            host.clone(),
            container.into(),
            cmd,
        ));
        pop(&mut s.exec_oneshot, "exec_oneshot")
    }
    async fn exec_interactive(
        &self,
        _host: &Host,
        _container: &str,
        _cmd: Vec<String>,
        _rows: u16,
        _cols: u16,
    ) -> Result<ExecSession, DockerError> {
        Err(DockerError::Invalid(
            "exec_interactive not supported by FakeDockerOps".into(),
        ))
    }
    async fn resize_exec(
        &self,
        _host: &Host,
        _exec_id: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn resize_container_tty(
        &self,
        _host: &Host,
        _container: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn start_debug_sidecar(
        &self,
        _host: &Host,
        _target_container: &str,
        _image: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<ExecSession, DockerError> {
        Err(DockerError::Invalid(
            "start_debug_sidecar not supported by FakeDockerOps".into(),
        ))
    }
    async fn fetch_recent_logs(
        &self,
        host: &Host,
        name: &str,
        lines: u32,
    ) -> Result<Vec<String>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::FetchRecentLogs(
            host.clone(),
            name.into(),
            lines,
        ));
        pop(&mut s.fetch_logs, "fetch_recent_logs")
    }
    async fn open_log_stream(
        &self,
        host: &Host,
        name: &str,
        _tail_lines: u32,
    ) -> Result<mpsc::UnboundedReceiver<LogLine>, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::OpenLogStream(host.clone(), name.into()));
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(lines) = s.log_streams.pop_front() {
            for line in lines {
                let _ = tx.send(line);
            }
        }
        Ok(rx)
    }
    async fn run_one_shot(
        &self,
        host: &Host,
        name: &str,
        _body: ContainerCreateBody,
    ) -> Result<OneShotResult, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::RunOneShot(host.clone(), name.into()));
        pop(&mut s.one_shot, "run_one_shot")
    }
    async fn subscribe_events(
        &self,
        host: &Host,
    ) -> Result<mpsc::UnboundedReceiver<DockerEvent>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::SubscribeEvents(host.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(events) = s.event_streams.pop_front() {
            for e in events {
                let _ = tx.send(e);
            }
        }
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
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
    fn host_ssh_url_format() {
        let h = host();
        assert_eq!(h.ssh_url(), "ssh://deploy@host-a");
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
        assert_eq!(info.health_hint(), Some("healthy"));
        assert_eq!(info.yoink_service.as_deref(), Some("app-a"));
        assert_eq!(info.yoink_version.as_deref(), Some("a1b2c3d"));
        assert_eq!(
            info.other_labels.get("caddy"),
            Some(&"app-a.example.com".to_string())
        );
    }
}
