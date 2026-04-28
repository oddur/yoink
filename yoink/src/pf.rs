//! `yoink pf` — laptop-to-container port-forwarding.
//!
//! Two paths, picked automatically:
//!
//! - **Published path.** The fast / zero-extra-state form. When the
//!   service declares a `publish:` entry that matches the requested
//!   container port, `docker-proxy` is already listening on the host
//!   at `host_ip:host_port` and we just bridge the laptop into it
//!   with a single `ssh -L`. Used for services like pgadmin where
//!   exposing a host port is the deliberate setup.
//!
//! - **Sidecar path.** For services that are *not* published —
//!   anything fronted by Caddy, sealed-network internal services,
//!   the secure-by-default api/web shape — we spawn an ephemeral
//!   `alpine/socat` sidecar that joins the same docker network as
//!   the target, listens on its own internal port, and forwards
//!   to `<service-alias>:<container-port>`. Yoink then `ssh -L`s
//!   to the sidecar's published-on-loopback port. The sidecar's
//!   lifetime is tied to the `pf` invocation; Drop force-removes it.
//!
//! Either way the operator-visible UX is identical: an open URL,
//! Ctrl-C / `Shift-F` closes everything cleanly. The mode selection
//! is automatic; `--mode published` / `--mode sidecar` overrides
//! when an operator wants to be explicit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, ServiceConfig};
use crate::docker_ops::{DockerOps, Host};

/// How long we'll wait for the SSH tunnel's local listener to come
/// up before giving up. The `ssh -L` child binds locally before
/// auth completes, so this is mostly waiting on cold-DNS / cold-TCP
/// to the SSH host. Same default as the image-push tunnel.
pub const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Image used for the sidecar in the non-published path. Picked
/// because it's small (~5 MB), maintained by the alpine team, and
/// available on Docker Hub without auth — operators don't need to
/// thread credentials through `pf`. socat is the simplest "tcp
/// listen + forward" we have; busybox `nc` would also work but
/// alpine/socat ships ready to go and is more flexible if we ever
/// need UDP / TLS later.
pub const SIDECAR_IMAGE: &str = "alpine/socat";
pub const SIDECAR_TAG: &str = "latest";

/// Internal port socat listens on inside the sidecar's namespace.
/// Arbitrary; nothing else runs in the sidecar so any unassigned
/// port works. Picked 1080 because it's high (no privilege needed)
/// and recognizable in `docker ps -a` output if the operator ever
/// looks.
const SIDECAR_INTERNAL_PORT: u16 = 1080;

/// How long we wait for the sidecar to start + bind before giving
/// up. socat boots in <100 ms; the budget covers the cold-start
/// image pull on first use.
const SIDECAR_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Host-side IP the sidecar's port-binding lands on. Loopback-only
/// is the whole point of `pf` — operators tunnel to it via SSH;
/// nothing fronts the public internet even briefly.
pub const SIDECAR_DIAL_HOST: &str = "127.0.0.1";

/// Backoff between sidecar / tunnel cleanup retries. Matches the
/// admin-API readiness backoff so cancellation latency is uniform
/// across yoink's network paths.
const CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Operator-visible URL scheme override. `Auto` runs the
/// port-number heuristic (`default_scheme`); `Http`/`Https` force
/// the obvious wrapper; `Tcp`/`None` print bare `localhost:N` and
/// suppress browser-open. Backed by a `clap::ValueEnum` derive at
/// the CLI surface so `--scheme=tcp` validates at parse time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SchemeOverride {
    #[default]
    Auto,
    Http,
    Https,
    Tcp,
    /// Bare `localhost:N` — explicitly says "this isn't HTTP."
    /// Equivalent to `Tcp` for `forward_url` but distinct in CLI
    /// help so operators see the intent option.
    None,
}

impl SchemeOverride {
    fn web_scheme(self, container_port: u16) -> Option<&'static str> {
        match self {
            Self::Auto => default_scheme(container_port),
            Self::Http => Some("http"),
            Self::Https => Some("https"),
            Self::Tcp | Self::None => None,
        }
    }
}

/// Default scheme heuristic mapping a container port to a URL
/// scheme. Conservative: only well-known HTTP ports auto-resolve;
/// everything else returns `None` and the operator gets `localhost:N`
/// (still copyable, but no auto-open).
#[must_use]
pub fn default_scheme(container_port: u16) -> Option<&'static str> {
    match container_port {
        80 | 3000 | 5000 | 5050 | 8000 | 8080 | 8081 | 8443 | 9000 => Some("http"),
        443 => Some("https"),
        _ => None,
    }
}

/// Render the URL the operator wants. `scheme` overrides the
/// heuristic; defaults to `Auto` (port-based).
#[must_use]
pub fn forward_url(local_port: u16, container_port: u16, scheme: SchemeOverride) -> String {
    match scheme.web_scheme(container_port) {
        Some(s) => format!("{s}://localhost:{local_port}"),
        None => format!("localhost:{local_port}"),
    }
}

/// Spawn the operator's system browser at `url`. Best-effort: returns
/// `Err` when no opener is available or the platform handler refuses
/// to launch (rare; logs to the caller-facing error so they can paste
/// the URL by hand). Cross-platform: `open` on macOS, `xdg-open` on
/// Linux, `cmd /C start` on Windows.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).status();
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).status();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", "", url]).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no browser-open shim for this platform",
    ));
    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "browser-open exited {status}"
        )))
    }
}

#[derive(Debug, Error)]
pub enum PfError {
    #[error("no service named {0:?} in this config")]
    ServiceNotFound(String),
    #[error(
        "service {service:?} has no `publish:` block and `--mode published` was forced; \
         either drop the flag (default `auto` falls back to a sidecar) or pass `--mode sidecar` \
         explicitly."
    )]
    NoPublishes { service: String },
    #[error(
        "service {service:?} has no published container port {port} (available: {available}). \
         Container ports come from the right-hand side of `publish: \"H:C\"` entries."
    )]
    PortNotPublished {
        service: String,
        port: u16,
        available: String,
    },
    #[error(
        "service {service:?} publishes {port} on more than one host port (got {choices}); \
         pin one with `LOCAL:CONTAINER` syntax (e.g. `yoink pf {service} {first}:{port}`)."
    )]
    AmbiguousHostPort {
        service: String,
        port: u16,
        choices: String,
        first: u16,
    },
    #[error("invalid publish spec {spec:?}: {reason}")]
    InvalidPublishSpec { spec: String, reason: String },
    #[error(
        "service {service:?} declares no `networks:` — sidecar mode needs at least one docker network to join. Add a `networks:` entry to the service or its `deploy.networks:` default, or pin one published container port and use `--mode published`."
    )]
    SidecarNoNetwork { service: String },
    #[error(transparent)]
    Sidecar(#[from] SidecarError),
}

/// Resolved publish entry for a single (service, container_port) pair.
/// `host_ip` defaults to `127.0.0.1` when the publish doesn't specify
/// one — that's the docker-cli default and the address `docker-proxy`
/// binds to under `0.0.0.0` publishes too on most kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEndpoint {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
}

/// Look up the host-side endpoint for `(service, container_port)`.
/// Pure function; the caller wires the SSH connection separately.
pub fn resolve_endpoint(
    service: &ServiceConfig,
    container_port: u16,
) -> Result<PublishedEndpoint, PfError> {
    if service.run.publish.is_empty() {
        return Err(PfError::NoPublishes {
            service: service.name.clone(),
        });
    }
    let mut matches = Vec::new();
    let mut available = Vec::new();
    for spec in &service.run.publish {
        let parsed = parse_publish_spec(spec).map_err(|reason| PfError::InvalidPublishSpec {
            spec: spec.clone(),
            reason,
        })?;
        available.push(parsed.container_port);
        if parsed.container_port == container_port {
            matches.push(parsed);
        }
    }
    if matches.is_empty() {
        let mut sorted = available;
        sorted.sort_unstable();
        sorted.dedup();
        let available = sorted
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PfError::PortNotPublished {
            service: service.name.clone(),
            port: container_port,
            available,
        });
    }
    if matches.len() > 1 {
        let mut hosts: Vec<u16> = matches.iter().map(|p| p.host_port).collect();
        hosts.sort_unstable();
        let first = hosts[0];
        let choices = hosts
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PfError::AmbiguousHostPort {
            service: service.name.clone(),
            port: container_port,
            choices,
            first,
        });
    }
    Ok(matches.into_iter().next().expect("checked length above"))
}

/// If a service has exactly one publish entry, surface it for the
/// "no port arg given" CLI shortcut. Returns `None` for zero or
/// multiple publishes — caller surfaces `NoPublishes` or asks the
/// operator which port they meant.
#[must_use]
pub fn sole_publish(service: &ServiceConfig) -> Option<PublishedEndpoint> {
    if service.run.publish.len() != 1 {
        return None;
    }
    parse_publish_spec(&service.run.publish[0]).ok()
}

fn parse_publish_spec(spec: &str) -> Result<PublishedEndpoint, String> {
    // `[ip:]hostport:containerport[/proto]` — same shape `parse_port_bindings`
    // accepts in docker.rs. Different output (we want a typed
    // PublishedEndpoint, the deploy-time parser wants bollard's
    // PortBinding map), so duplicating the small bit of parsing is
    // cheaper than threading the bollard type into a public helper.
    let mapping = spec.split_once('/').map_or(spec, |(m, _proto)| m);
    let parts: Vec<&str> = mapping.split(':').collect();
    let (host_ip, host_port, container_port) = match parts.as_slice() {
        [hp, cp] => ("127.0.0.1", *hp, *cp),
        [ip, hp, cp] => (*ip, *hp, *cp),
        _ => return Err("expected `[ip:]host_port:container_port[/proto]`".into()),
    };
    let host_port: u16 = host_port
        .parse()
        .map_err(|_| format!("host port {host_port:?} not in 0..65535"))?;
    let container_port: u16 = container_port
        .parse()
        .map_err(|_| format!("container port {container_port:?} not in 0..65535"))?;
    Ok(PublishedEndpoint {
        host_ip: host_ip.to_string(),
        host_port,
        container_port,
    })
}

/// Resolve the right service + host pair for a `yoink pf <service>`
/// invocation. Returns the host the service actually runs on (single-
/// host configs are a no-op; multi-host needs the operator to disambiguate
/// via `--host` later if/when that flag lands).
pub fn resolve_service<'a>(config: &'a Config, name: &str) -> Result<&'a ServiceConfig, PfError> {
    config
        .services
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| PfError::ServiceNotFound(name.to_string()))
}

/// Forward path the operator asked for. `Auto` falls back to a
/// sidecar when the service doesn't publish the requested port;
/// `Published` errors instead; `Sidecar` always uses a sidecar
/// (useful when the operator wants to bypass `docker-proxy` for a
/// reason — e.g. checking the container's `:8080` directly even
/// though it's also published as `127.0.0.1:5050:8080`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Auto,
    Published,
    Sidecar,
}

/// What the auto-resolution decided and produced for a single
/// `pf` invocation. `Published` carries the resolved host endpoint
/// to SSH-forward to. `Sidecar` carries an opaque handle that owns
/// the spun-up container; the host-side port is a method on the
/// handle so the two can't drift.
pub enum ResolvedTarget {
    Published(PublishedEndpoint),
    Sidecar(SidecarHandle),
}

/// Decide which path to take for `(service, container_port)` and
/// drive whichever side spawns/inspects need running. Returns the
/// resolved endpoint (published) or sidecar handle (when we had to
/// spin one up).
pub async fn resolve_target(
    ops: Arc<dyn DockerOps>,
    host: &Host,
    service: &ServiceConfig,
    container_port: u16,
    mode: Mode,
) -> Result<ResolvedTarget, PfError> {
    // 1. Try published unless the operator forced sidecar.
    let published = match mode {
        Mode::Published | Mode::Auto => match resolve_endpoint(service, container_port) {
            Ok(ep) => Some(ep),
            Err(e) => match e {
                // Auto mode: a missing publish is not an error,
                // it's a signal to fall back to the sidecar path.
                PfError::NoPublishes { .. } | PfError::PortNotPublished { .. }
                    if matches!(mode, Mode::Auto) =>
                {
                    None
                }
                other => return Err(other),
            },
        },
        Mode::Sidecar => None,
    };
    if let Some(ep) = published {
        return Ok(ResolvedTarget::Published(ep));
    }

    // 2. Sidecar path. Pick a network the target shares; bind a
    //    socat container into it and let docker-proxy publish a
    //    random host port back at us.
    let network = service
        .networks
        .as_ref()
        .and_then(|v| v.first().cloned())
        .ok_or_else(|| PfError::SidecarNoNetwork {
            service: service.name.clone(),
        })?;
    let handle = spawn_sidecar(ops, host.clone(), &service.name, container_port, &network).await?;
    Ok(ResolvedTarget::Sidecar(handle))
}

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("pull alpine/socat on {host}: {source}")]
    Pull {
        host: String,
        #[source]
        source: crate::docker_ops::DockerError,
    },
    #[error("create sidecar container {name} on {host}: {source}")]
    Create {
        host: String,
        name: String,
        #[source]
        source: crate::docker_ops::DockerError,
    },
    #[error("start sidecar container {name} on {host}: {source}")]
    Start {
        host: String,
        name: String,
        #[source]
        source: crate::docker_ops::DockerError,
    },
    #[error("inspect sidecar container {name} on {host}: {source}")]
    Inspect {
        host: String,
        name: String,
        #[source]
        source: crate::docker_ops::DockerError,
    },
    #[error(
        "sidecar {name} on {host} did not publish a host port for :{internal} within {timeout:?}"
    )]
    NotReady {
        host: String,
        name: String,
        internal: u16,
        timeout: Duration,
    },
}

/// Owns a spawned sidecar container. Cleanup is explicit: callers
/// must `await` `close()` from their async context before dropping
/// the handle, otherwise the container is left running. Drop emits
/// a tracing warning + best-effort `tokio::spawn`'d remove, but
/// runtime-shutdown races mean it can't be relied on.
///
/// The `cleanup_done` flag is the discriminator — `close()` flips
/// it after the docker call returns, and Drop only fires the
/// best-effort spawn when it's still false. Tests can construct a
/// closed-state handle directly without docker calls.
pub struct SidecarHandle {
    pub container_name: String,
    pub host: Host,
    /// Docker-assigned host port that maps to the sidecar's
    /// internal listener. The CLI / TUI tunnel into this port via
    /// `ssh -L`. Authoritative; no caller-side duplication.
    host_port: u16,
    ops: Arc<dyn DockerOps>,
    cleanup_done: bool,
}

impl SidecarHandle {
    /// Host-side port the sidecar listens on. Pass to `ssh -L
    /// laptop:CONTAINER_HOST_PORT` to reach the target.
    #[must_use]
    pub fn host_port(&self) -> u16 {
        self.host_port
    }

    /// Force-remove the sidecar container. Call this in your async
    /// context after the operator's done with the tunnel; Drop is
    /// only a best-effort fallback and may not complete if the
    /// tokio runtime is being torn down.
    ///
    /// Bollard's pooled SSH connection idles out after ~10 seconds —
    /// when the operator holds a tunnel open longer than that the
    /// first remove attempt returns a `SendRequest` error from the
    /// dead connection. We retry once with a tiny backoff so the
    /// pool reopens transparently. Two failures in a row usually
    /// mean the host is genuinely unreachable; we log and move on
    /// so a flaky cleanup doesn't strand the operator's terminal.
    pub async fn close(mut self) {
        if self.cleanup_done {
            return;
        }
        let mut last_err = None;
        for attempt in 0..3 {
            match self
                .ops
                .force_remove_container(&self.host, &self.container_name)
                .await
            {
                Ok(()) => {
                    self.cleanup_done = true;
                    return;
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(CLEANUP_POLL_INTERVAL).await;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            tracing::warn!(
                container = %self.container_name,
                host = %self.host.address,
                error = %e,
                "yoink-pf sidecar close: force-remove failed after 3 attempts; auto_remove on container exit will catch up if the host comes back"
            );
        }
        self.cleanup_done = true;
    }
}

impl Drop for SidecarHandle {
    fn drop(&mut self) {
        if self.cleanup_done {
            return;
        }
        // No explicit close() — best-effort fallback. We'd rather
        // log loudly than silently leak; the spawn-from-Drop path
        // races with runtime shutdown and may be a no-op, but on
        // panic / unwind it's the only path we have.
        tracing::warn!(
            container = %self.container_name,
            host = %self.host.address,
            "yoink-pf sidecar dropped without close(); cleanup is best-effort"
        );
        let ops = self.ops.clone();
        let host = self.host.clone();
        let name = self.container_name.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = ops.force_remove_container(&host, &name).await;
            });
        }
    }
}

/// Spawn a `<image>:<tag>` socat sidecar that joins `network`,
/// listens on `SIDECAR_INTERNAL_PORT`, forwards to `target_alias:
/// target_port`. Polls `inspect_container` until docker reports a
/// host-side port mapping, then returns the assigned `host_port`
/// alongside the handle.
async fn spawn_sidecar(
    ops: Arc<dyn DockerOps>,
    host: Host,
    target_alias: &str,
    target_port: u16,
    network: &str,
) -> Result<SidecarHandle, SidecarError> {
    use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};

    let container_name = unique_sidecar_name(target_alias, target_port);

    // Skip the pull when the image is already cached. Bollard
    // tolerates a redundant pull but the registry round-trip is
    // visible (~hundreds of ms on a slow link) and pf is the kind
    // of debug primitive operators run mid-incident; the warm-start
    // path should be sub-second.
    let cached = ops
        .image_present(&host, SIDECAR_IMAGE, SIDECAR_TAG)
        .await
        .map_err(|source| SidecarError::Pull {
            host: host.address.clone(),
            source,
        })?;
    if !cached {
        ops.pull_image(&host, SIDECAR_IMAGE, SIDECAR_TAG, None)
            .await
            .map_err(|source| SidecarError::Pull {
                host: host.address.clone(),
                source,
            })?;
    }

    let internal_key = format!("{SIDECAR_INTERNAL_PORT}/tcp");
    let mut port_bindings = HashMap::new();
    port_bindings.insert(
        internal_key.clone(),
        Some(vec![PortBinding {
            // Loopback-only — pf's whole story is "tunnel to laptop";
            // we don't want this port fronting the public internet
            // even briefly. Docker assigns a free host port via :0.
            host_ip: Some("127.0.0.1".into()),
            host_port: Some("0".into()),
        }]),
    );
    let mut labels = HashMap::new();
    labels.insert("yoink.managed".into(), "true".into());
    labels.insert("yoink.kind".into(), "pf-sidecar".into());
    labels.insert("yoink.pf.target_service".into(), target_alias.into());
    labels.insert("yoink.pf.target_port".into(), target_port.to_string());

    let body = ContainerCreateBody {
        image: Some(format!("{SIDECAR_IMAGE}:{SIDECAR_TAG}")),
        cmd: Some(vec![
            // socat invocation: tcp-listen forks per connection so
            // the operator can hold multiple parallel laptop-side
            // connections through one tunnel; `reuseaddr` makes
            // restarts immediate after a previous sidecar exits.
            format!("tcp-listen:{SIDECAR_INTERNAL_PORT},fork,reuseaddr"),
            format!("tcp:{target_alias}:{target_port}"),
        ]),
        exposed_ports: Some(vec![internal_key.clone()]),
        labels: Some(labels),
        host_config: Some(HostConfig {
            // network_mode pins the container to the target's
            // docker network so socat resolves `target_alias` via
            // docker DNS to the running replica's IP.
            network_mode: Some(network.into()),
            port_bindings: Some(port_bindings),
            // Auto-remove on stop is belt-and-braces alongside the
            // explicit force-remove the cleanup task runs.
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    ops.create_container(&host, &container_name, body)
        .await
        .map_err(|source| SidecarError::Create {
            host: host.address.clone(),
            name: container_name.clone(),
            source,
        })?;
    ops.start_container(&host, &container_name)
        .await
        .map_err(|source| SidecarError::Start {
            host: host.address.clone(),
            name: container_name.clone(),
            source,
        })?;

    // Poll inspect for the docker-assigned host port. socat boots
    // fast but `inspect` may briefly return ports without the
    // host-side mapping populated if we ask too eagerly.
    let host_port = wait_for_host_port(
        ops.as_ref(),
        &host,
        &container_name,
        SIDECAR_INTERNAL_PORT,
        SIDECAR_READY_TIMEOUT,
    )
    .await?;

    Ok(SidecarHandle {
        container_name,
        host,
        host_port,
        ops,
        cleanup_done: false,
    })
}

async fn wait_for_host_port(
    ops: &dyn DockerOps,
    host: &Host,
    container_name: &str,
    internal_port: u16,
    timeout: Duration,
) -> Result<u16, SidecarError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let detail = ops
            .inspect_container(host, container_name)
            .await
            .map_err(|source| SidecarError::Inspect {
                host: host.address.clone(),
                name: container_name.to_string(),
                source,
            })?;
        if let Some(host_port) = host_port_from_detail(&detail.ports, internal_port) {
            return Ok(host_port);
        }
        if std::time::Instant::now() >= deadline {
            return Err(SidecarError::NotReady {
                host: host.address.clone(),
                name: container_name.to_string(),
                internal: internal_port,
                timeout,
            });
        }
        tokio::time::sleep(CLEANUP_POLL_INTERVAL).await;
    }
}

/// Parse `ContainerDetail.ports` for the host-side port mapped to
/// `internal_port`. Entries are `[ip:]host:container/proto` strings
/// (yoink's `parse_inspect` flattens bollard's port map). Pure fn,
/// unit-tested.
fn host_port_from_detail(ports: &[String], internal_port: u16) -> Option<u16> {
    for entry in ports {
        let mapping = entry.split('/').next().unwrap_or(entry);
        let parts: Vec<&str> = mapping.split(':').collect();
        let (host_str, container_str) = match parts.as_slice() {
            [h, c] => (*h, *c),
            [_ip, h, c] => (*h, *c),
            _ => continue,
        };
        if container_str.parse::<u16>().ok() != Some(internal_port) {
            continue;
        }
        if let Ok(host_port) = host_str.parse::<u16>() {
            return Some(host_port);
        }
    }
    None
}

fn unique_sidecar_name(target_service: &str, target_port: u16) -> String {
    // Compact pid + nanos suffix is enough to avoid collisions across
    // concurrent `pf` invocations from the same operator. We never
    // reuse a sidecar across `pf` runs.
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let suffix = format!("{pid:x}{:x}", nanos & 0xff_ffff);
    format!("yoink-pf-{target_service}-{target_port}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Config {
        crate::config::Config::parse_str(yaml).unwrap()
    }

    fn cfg_with(svc_yaml: &str) -> Config {
        let yaml = format!(
            r#"
hosts:
  - {{ address: host-a, user: deploy }}
services:
{svc_yaml}
"#
        );
        parse(&yaml)
    }

    #[test]
    fn resolve_endpoint_picks_matching_publish() {
        let cfg = cfg_with(
            r#"  - name: pgadmin
    image: dpage/pgadmin4
    tag: "9"
    run:
      port: 80
      healthcheck_path: /login
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "127.0.0.1:5050:80"
"#,
        );
        let ep = resolve_endpoint(&cfg.services[0], 80).unwrap();
        assert_eq!(ep.host_ip, "127.0.0.1");
        assert_eq!(ep.host_port, 5050);
        assert_eq!(ep.container_port, 80);
    }

    #[test]
    fn resolve_endpoint_defaults_host_ip_to_loopback() {
        // Two-token form `H:C` has no IP — docker-proxy actually binds
        // to 0.0.0.0 in this case, but on every kernel we test against
        // 127.0.0.1 also reaches it, so we present the loopback view.
        let cfg = cfg_with(
            r#"  - name: api
    image: bt-api
    tag: latest
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "8080:8080"
"#,
        );
        let ep = resolve_endpoint(&cfg.services[0], 8080).unwrap();
        assert_eq!(ep.host_ip, "127.0.0.1");
        assert_eq!(ep.host_port, 8080);
    }

    #[test]
    fn resolve_endpoint_no_publishes_errors_with_shell_hint() {
        let cfg = cfg_with(
            r#"  - name: api
    image: bt-api
    tag: latest
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
"#,
        );
        let err = resolve_endpoint(&cfg.services[0], 8080).unwrap_err();
        assert!(matches!(err, PfError::NoPublishes { .. }));
        let msg = err.to_string();
        // Error fires only when `--mode published` is forced — auto
        // mode silently falls back to a sidecar. Verify the message
        // points the operator at that escape hatch.
        assert!(msg.contains("sidecar"), "got: {msg}");
    }

    #[test]
    fn resolve_endpoint_unknown_port_lists_alternatives() {
        let cfg = cfg_with(
            r#"  - name: web
    image: bt-web
    tag: latest
    run:
      port: 3000
      healthcheck_path: /healthz
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "3000:3000"
        - "8080:8080"
"#,
        );
        let err = resolve_endpoint(&cfg.services[0], 9999).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3000"), "missing 3000 in: {msg}");
        assert!(msg.contains("8080"), "missing 8080 in: {msg}");
    }

    #[test]
    fn sole_publish_one_entry() {
        let cfg = cfg_with(
            r#"  - name: pgadmin
    image: dpage/pgadmin4
    tag: "9"
    run:
      port: 80
      healthcheck_path: /login
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "127.0.0.1:5050:80"
"#,
        );
        let ep = sole_publish(&cfg.services[0]).unwrap();
        assert_eq!(ep.container_port, 80);
        assert_eq!(ep.host_port, 5050);
    }

    #[test]
    fn sole_publish_returns_none_for_multi() {
        let cfg = cfg_with(
            r#"  - name: web
    image: bt-web
    tag: latest
    run:
      port: 3000
      healthcheck_path: /healthz
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "3000:3000"
        - "8080:8080"
"#,
        );
        assert!(sole_publish(&cfg.services[0]).is_none());
    }

    #[test]
    fn forward_url_uses_heuristic_when_unforced() {
        assert_eq!(
            forward_url(54321, 80, SchemeOverride::Auto),
            "http://localhost:54321"
        );
        assert_eq!(
            forward_url(54322, 443, SchemeOverride::Auto),
            "https://localhost:54322"
        );
        assert_eq!(
            forward_url(54323, 5432, SchemeOverride::Auto),
            "localhost:54323"
        );
    }

    #[test]
    fn host_port_from_detail_picks_two_token_form() {
        // bollard's port format: `host_port:container_port[/proto]`.
        let ports = vec!["54321:1080/tcp".into()];
        assert_eq!(host_port_from_detail(&ports, 1080), Some(54321));
    }

    #[test]
    fn host_port_from_detail_picks_three_token_form() {
        // With explicit host IP: `ip:host:container[/proto]`.
        let ports = vec!["127.0.0.1:54322:1080/tcp".into()];
        assert_eq!(host_port_from_detail(&ports, 1080), Some(54322));
    }

    #[test]
    fn host_port_from_detail_skips_unrelated_mappings() {
        let ports = vec!["54321:9090/tcp".into(), "127.0.0.1:54322:1080".into()];
        assert_eq!(host_port_from_detail(&ports, 1080), Some(54322));
        assert_eq!(host_port_from_detail(&ports, 9090), Some(54321));
        assert_eq!(host_port_from_detail(&ports, 7777), None);
    }

    #[test]
    fn forward_url_explicit_scheme_overrides() {
        // Force https on a port the heuristic doesn't recognize.
        assert_eq!(
            forward_url(54324, 8443, SchemeOverride::Https),
            "https://localhost:54324"
        );
        // Tcp / None suppress the http:// wrapper even on port 80.
        assert_eq!(
            forward_url(54325, 80, SchemeOverride::None),
            "localhost:54325"
        );
        assert_eq!(
            forward_url(54326, 80, SchemeOverride::Tcp),
            "localhost:54326"
        );
    }
}
