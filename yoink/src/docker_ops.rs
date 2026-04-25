//! Docker operations behind a small async trait. `RealDockerOps` connects
//! to each remote daemon via bollard's `ssh://` transport (no remote shell
//! exec; bollard speaks the Docker Engine API over an ssh-tunnelled
//! socket). `FakeDockerOps` returns scripted typed responses so the rest
//! of the crate is testable without spawning real containers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

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
    #[must_use]
    pub fn ssh_url(&self) -> String {
        format!("ssh://{}@{}", self.user, self.address)
    }
}

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
        let other_labels: BTreeMap<String, String> = labels
            .into_iter()
            .filter(|(k, _)| !k.starts_with("yoink."))
            .collect();
        let names = summary.names.unwrap_or_default();
        let name = names
            .first()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();
        Self {
            host: host.to_string(),
            name,
            state: summary
                .state
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_default(),
            status_text: summary.status.unwrap_or_default(),
            created_unix: summary.created,
            yoink_service,
            yoink_version,
            yoink_spec_hash,
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
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerStats {
    /// CPU% across all online CPUs — max ≈ `n_cpu * 100`.
    pub cpu_pct: f64,
    /// Resident memory in bytes.
    pub mem_used: i64,
    /// Limit in bytes if a memory cap is set, else None.
    pub mem_limit: Option<i64>,
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

/// The async surface every consumer of Docker uses. Methods take a `&Host`
/// so the same trait object can drive multiple remote daemons.
#[async_trait]
pub trait DockerOps: Send + Sync {
    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError>;

    /// Host-level info from `docker info` — capacity, container counts.
    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError>;

    /// One-shot snapshot of CPU% + memory used for a single container.
    async fn container_stats(&self, host: &Host, name: &str)
    -> Result<ContainerStats, DockerError>;

    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError>;

    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError>;

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
    /// Used by the host advisory lock to heartbeat the sentinel
    /// without recreating it. Returns Ok on exit code 0; surfaces
    /// non-zero exits and bollard errors.
    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<(), DockerError>;

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

/// Real implementation. One `bollard::Docker` per host, cached for the
/// lifetime of the process.
#[derive(Default)]
pub struct RealDockerOps {
    clients: tokio::sync::Mutex<HashMap<String, Docker>>,
    timeout_secs: u64,
}

impl RealDockerOps {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clients: tokio::sync::Mutex::new(HashMap::new()),
            timeout_secs: 120,
        }
    }

    async fn client_for(&self, host: &Host) -> Result<Docker, DockerError> {
        let key = host.ssh_url();
        let mut clients = self.clients.lock().await;
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let docker =
            Docker::connect_with_ssh(&key, self.timeout_secs, bollard::API_DEFAULT_VERSION, None)
                .map_err(|source| DockerError::Connect {
                host: host.address.clone(),
                source,
            })?;
        clients.insert(key.clone(), docker.clone());
        Ok(docker)
    }

    fn err(host: &Host, source: bollard::errors::Error) -> DockerError {
        DockerError::Bollard {
            host: host.address.clone(),
            source,
        }
    }
}

#[async_trait]
impl DockerOps for RealDockerOps {
    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError> {
        let docker = self.client_for(host).await?;
        let v = docker.version().await.map_err(|s| Self::err(host, s))?;
        Ok(DockerVersion {
            server_version: v.version,
            api_version: v.api_version,
            os: v.os,
            arch: v.arch,
        })
    }

    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError> {
        let docker = self.client_for(host).await?;
        let i = docker.info().await.map_err(|s| Self::err(host, s))?;
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
        let docker = self.client_for(host).await?;
        let opts = bollard::query_parameters::StatsOptionsBuilder::new()
            .stream(false)
            .one_shot(false)
            .build();
        let mut stream = docker.stats(name, Some(opts));
        let stats = match stream.next().await {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(Self::err(host, e)),
            None => return Err(DockerError::Invalid("stats stream yielded no item".into())),
        };
        Ok(parse_stats(&stats))
    }

    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError> {
        let docker = self.client_for(host).await?;
        match docker.inspect_network(network, None).await {
            Ok(_) => Ok(false),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                let req = NetworkCreateRequest {
                    name: network.to_string(),
                    ..Default::default()
                };
                docker
                    .create_network(req)
                    .await
                    .map_err(|s| Self::err(host, s))?;
                Ok(true)
            }
            Err(other) => Err(Self::err(host, other)),
        }
    }

    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        let opts = CreateImageOptionsBuilder::new()
            .from_image(image)
            .tag(tag)
            .build();
        let mut stream = docker.create_image(Some(opts), None, credentials);
        while let Some(item) = stream.next().await {
            item.map_err(|s| Self::err(host, s))?;
        }
        Ok(())
    }

    async fn list_containers_by_label(
        &self,
        host: &Host,
        label: &str,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let docker = self.client_for(host).await?;
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![label.to_string()]);
        let opts = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let summaries = docker
            .list_containers(Some(opts))
            .await
            .map_err(|s| Self::err(host, s))?;
        Ok(summaries
            .into_iter()
            .map(|s| ContainerInfo::from_summary(&host.address, s))
            .collect())
    }

    async fn list_running_containers(
        &self,
        host: &Host,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let docker = self.client_for(host).await?;
        let mut filters = HashMap::new();
        filters.insert("status".to_string(), vec!["running".to_string()]);
        let opts = ListContainersOptionsBuilder::new()
            .all(false)
            .filters(&filters)
            .build();
        let summaries = docker
            .list_containers(Some(opts))
            .await
            .map_err(|s| Self::err(host, s))?;
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
        let docker = self.client_for(host).await?;
        let opts = CreateContainerOptionsBuilder::new().name(name).build();
        let resp = docker
            .create_container(Some(opts), body)
            .await
            .map_err(|s| Self::err(host, s))?;
        Ok(resp.id)
    }

    async fn start_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        docker
            .start_container(name, None)
            .await
            .map_err(|s| Self::err(host, s))
    }

    async fn stop_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        let opts = StopContainerOptionsBuilder::new()
            .t(i32::try_from(drain.as_secs()).unwrap_or(i32::MAX))
            .build();
        docker
            .stop_container(name, Some(opts))
            .await
            .map_err(|s| Self::err(host, s))
    }

    async fn force_remove_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        let opts = RemoveContainerOptionsBuilder::new().force(true).build();
        // 404 = container didn't exist, treat as idempotent success.
        match docker.remove_container(name, Some(opts)).await {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(other) => Err(Self::err(host, other)),
        }
    }

    async fn healthcheck(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
        path: &str,
    ) -> Result<u16, DockerError> {
        let docker = self.client_for(host).await?;
        let url = format!("http://{target}:{port}{path}");
        let probe_name = format!("yoink-probe-{}-{}", target, rand_hex());

        let mut endpoints = HashMap::new();
        endpoints.insert(network.to_string(), EndpointSettings::default());
        let body = ContainerCreateBody {
            image: Some(HEALTHCHECK_CURL_IMAGE.to_string()),
            cmd: Some(vec![
                "-fsS".into(),
                "-o".into(),
                "/dev/null".into(),
                "-w".into(),
                "%{http_code}".into(),
                "--max-time".into(),
                "5".into(),
                url,
            ]),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(endpoints),
            }),
            host_config: Some(HostConfig {
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_opts = CreateContainerOptionsBuilder::new()
            .name(&probe_name)
            .build();
        docker
            .create_container(Some(create_opts), body)
            .await
            .map_err(|s| Self::err(host, s))?;
        docker
            .start_container(&probe_name, None)
            .await
            .map_err(|s| Self::err(host, s))?;

        // Wait for the probe to exit.
        let wait_opts = WaitContainerOptionsBuilder::new()
            .condition("not-running")
            .build();
        let mut wait_stream = docker.wait_container(&probe_name, Some(wait_opts));
        let exit_status_code = match wait_stream.next().await {
            Some(Ok(resp)) => i32::try_from(resp.status_code).unwrap_or(i32::MAX),
            Some(Err(bollard::errors::Error::DockerContainerWaitError { error: _, code })) => {
                i32::try_from(code).unwrap_or(i32::MAX)
            }
            Some(Err(other)) => {
                let _ = self.force_remove_container(host, &probe_name).await;
                return Err(Self::err(host, other));
            }
            None => {
                let _ = self.force_remove_container(host, &probe_name).await;
                return Err(DockerError::Invalid(
                    "wait_container yielded no event".into(),
                ));
            }
        };

        // Drain stdout to recover the HTTP status code curl printed.
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
        if exit_status_code == 0 {
            Ok(200)
        } else {
            Ok(0)
        }
    }

    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        let exec = docker
            .create_exec(
                container,
                bollard::exec::CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(false),
                    attach_stderr: Some(false),
                    ..Default::default()
                },
            )
            .await
            .map_err(|s| Self::err(host, s))?;
        // Detached start — returns immediately; we then inspect for the
        // exit code. Heartbeats need to be cheap, so don't stream.
        docker
            .start_exec(
                &exec.id,
                Some(bollard::exec::StartExecOptions {
                    detach: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|s| Self::err(host, s))?;
        let inspect = docker
            .inspect_exec(&exec.id)
            .await
            .map_err(|s| Self::err(host, s))?;
        if inspect.exit_code.unwrap_or(0) != 0 {
            return Err(DockerError::Invalid(format!(
                "exec in {container} exited with code {:?}",
                inspect.exit_code
            )));
        }
        Ok(())
    }

    async fn healthcheck_tcp(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
    ) -> Result<(), DockerError> {
        let docker = self.client_for(host).await?;
        // curl supports the telnet:// scheme with `--connect-timeout`
        // for raw TCP probes — connect succeeds → exit 0, connect
        // fails (refused/timeout/host unreachable) → non-zero exit.
        // Reuses the same probe image as the HTTP healthcheck so we
        // don't pull a second tools image just for `nc`.
        let url = format!("telnet://{target}:{port}");
        let probe_name = format!("yoink-tcp-probe-{}-{}", target, rand_hex());

        let mut endpoints = HashMap::new();
        endpoints.insert(network.to_string(), EndpointSettings::default());
        let body = ContainerCreateBody {
            image: Some(HEALTHCHECK_CURL_IMAGE.to_string()),
            cmd: Some(vec![
                "--connect-timeout".into(),
                "5".into(),
                "--max-time".into(),
                "5".into(),
                url,
            ]),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(endpoints),
            }),
            host_config: Some(HostConfig {
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let create_opts = CreateContainerOptionsBuilder::new()
            .name(&probe_name)
            .build();
        docker
            .create_container(Some(create_opts), body)
            .await
            .map_err(|s| Self::err(host, s))?;
        docker
            .start_container(&probe_name, None)
            .await
            .map_err(|s| Self::err(host, s))?;

        let wait_opts = WaitContainerOptionsBuilder::new()
            .condition("not-running")
            .build();
        let mut wait_stream = docker.wait_container(&probe_name, Some(wait_opts));
        let exit_status_code = match wait_stream.next().await {
            Some(Ok(resp)) => i32::try_from(resp.status_code).unwrap_or(i32::MAX),
            Some(Err(bollard::errors::Error::DockerContainerWaitError { error: _, code })) => {
                i32::try_from(code).unwrap_or(i32::MAX)
            }
            Some(Err(other)) => {
                let _ = self.force_remove_container(host, &probe_name).await;
                return Err(Self::err(host, other));
            }
            None => {
                let _ = self.force_remove_container(host, &probe_name).await;
                return Err(DockerError::Invalid(
                    "wait_container yielded no event".into(),
                ));
            }
        };
        let _ = self.force_remove_container(host, &probe_name).await;

        if exit_status_code == 0 {
            Ok(())
        } else {
            Err(DockerError::Invalid(format!(
                "tcp probe to {target}:{port} failed (curl exit {exit_status_code})"
            )))
        }
    }

    async fn fetch_recent_logs(
        &self,
        host: &Host,
        name: &str,
        lines: u32,
    ) -> Result<Vec<String>, DockerError> {
        let docker = self.client_for(host).await?;
        let opts = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .follow(false)
            .tail(&lines.to_string())
            .build();
        let mut stream = docker.logs(name, Some(opts));
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(log) => {
                    let bytes = match log {
                        bollard::container::LogOutput::StdOut { message }
                        | bollard::container::LogOutput::StdErr { message }
                        | bollard::container::LogOutput::Console { message }
                        | bollard::container::LogOutput::StdIn { message } => message,
                    };
                    out.push(String::from_utf8_lossy(&bytes).into_owned());
                }
                Err(e) => return Err(Self::err(host, e)),
            }
        }
        Ok(out)
    }

    async fn run_one_shot(
        &self,
        host: &Host,
        name: &str,
        body: ContainerCreateBody,
    ) -> Result<OneShotResult, DockerError> {
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
        let exit_code: i64 = match wait_stream.next().await {
            Some(Ok(resp)) => resp.status_code,
            Some(Err(bollard::errors::Error::DockerContainerWaitError { error: _, code })) => code,
            Some(Err(other)) => {
                let _ = self.force_remove_container(host, name).await;
                return Err(Self::err(host, other));
            }
            None => {
                let _ = self.force_remove_container(host, name).await;
                return Err(DockerError::Invalid(
                    "wait_container yielded no event".into(),
                ));
            }
        };

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

    ContainerStats {
        cpu_pct,
        mem_used,
        mem_limit,
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

/// Cheap random hex suffix for one-shot probe container names so two
/// concurrent deploys can't collide.
fn rand_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{nanos:08x}")
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
    list_containers: VecDeque<Result<Vec<ContainerInfo>, DockerError>>,
    create_container: VecDeque<Result<String, DockerError>>,
    start_container: VecDeque<Result<(), DockerError>>,
    stop_container: VecDeque<Result<(), DockerError>>,
    force_remove_container: VecDeque<Result<(), DockerError>>,
    healthcheck: VecDeque<Result<u16, DockerError>>,
    healthcheck_tcp: VecDeque<Result<(), DockerError>>,
    exec_oneshot: VecDeque<Result<(), DockerError>>,
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
    pub fn push_exec_oneshot(&self, v: Result<(), DockerError>) {
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
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ExecOneshot(
            host.clone(),
            container.into(),
            cmd,
        ));
        pop(&mut s.exec_oneshot, "exec_oneshot")
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
