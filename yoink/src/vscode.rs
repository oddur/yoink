//! `yoink vscode` — VS Code in your browser, rooted in a running container's
//! filesystem.
//!
//! Pattern: spawn a `codercom/code-server` sidecar on the host, share the
//! target container's PID namespace (`pid_mode: container:<target>`), and
//! point code-server at `/proc/1/root`. Inside the shared PID namespace,
//! `/proc/1` is the target's PID 1, and `/proc/1/root` is a magic symlink
//! to its rootfs — every read/write through that path lands on the target's
//! filesystem (including bind mounts and tmpfs the daemon set up).
//!
//! Why not `nsenter -m`? Switching mount namespaces would also lose the
//! code-server binary itself (it lives in the sidecar's filesystem,
//! invisible after `setns(CLONE_NEWNS)`). `/proc/1/root` keeps the binary
//! resolution in the sidecar's mount ns and only redirects file *content*
//! through the target's view — same end result, no `CAP_SYS_ADMIN`.
//!
//! The sidecar's network namespace is its own (joined to the target's
//! docker network for DNS), so docker can publish `8080` on the host's
//! loopback and yoink can `ssh -L` to it. Same shape as `yoink pf`'s
//! sidecar path.
//!
//! Caveat: the in-browser terminal still runs in the *sidecar's* mount
//! namespace, so `ls /` there shows the code-server image's filesystem,
//! not the target's. For a shell rooted in the target, use `yoink shell`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, ServiceConfig};
use crate::docker_ops::{DockerOps, Host};
use crate::pf::{self, SidecarError, SidecarHandle};

/// Pinned major tag — code-server's `latest` floats and we don't want a
/// surprise UI/API regression mid-deploy. Bump deliberately.
pub const CODE_SERVER_IMAGE: &str = "codercom/code-server";
pub const CODE_SERVER_TAG: &str = "4.96.4";

/// Port code-server binds inside its container. Hard-coded by the image's
/// entrypoint; we publish `127.0.0.1:0:8080` and let docker assign a free
/// host port we tunnel into.
const SIDECAR_INTERNAL_PORT: u16 = 8080;

/// Sidecar startup budget: cold image pull (~30-60s on a slow link) plus
/// code-server's ~2s boot. Generous on first use, instant on warm cache.
const SIDECAR_READY_TIMEOUT: Duration = Duration::from_secs(90);

/// After the host port is published, wait for code-server to actually
/// answer HTTP. The TCP listener comes up before the Express handler is
/// ready, so opening the browser too eagerly shows a "site can't be
/// reached" before code-server's first response.
const HTTP_READY_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_READY_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct VscodeOptions {
    pub host_filter: Option<String>,
    pub replica: usize,
    pub open: bool,
    pub local_port: Option<u16>,
}

#[derive(Debug, Error)]
pub enum VscodeError {
    #[error("no service named {0:?} in this config")]
    ServiceNotFound(String),
    #[error(
        "service {service:?} declares no `networks:` — `yoink vscode` needs a docker network to join \
         (so code-server can resolve service DNS in its terminal). Add a `networks:` entry to the \
         service or `deploy.networks:`."
    )]
    NoNetwork { service: String },
    #[error(
        "service {service:?} runs on `address: local` — `yoink vscode` needs an SSH endpoint to tunnel \
         through. Run against a remote host."
    )]
    LocalHost { service: String },
    #[error("service {service:?} has no applicable host{matched}")]
    NoApplicableHost { service: String, matched: String },
    #[error("service {service:?} runs on {n} hosts; pin one with --host:\n{listing}")]
    AmbiguousHost {
        service: String,
        n: usize,
        listing: String,
    },
    #[error("service {service:?} has {replicas} replica(s); --replica {replica} is out of range")]
    ReplicaOutOfRange {
        service: String,
        replicas: u32,
        replica: usize,
    },
    #[error(
        "no running container for service {service:?} on {host} (looked up by `yoink.service` \
         label). Has `yoink up` finished?"
    )]
    TargetNotRunning { service: String, host: String },
    #[error("list containers by label {label} on {host}: {source}")]
    ListContainers {
        host: String,
        label: String,
        #[source]
        source: crate::docker_ops::DockerError,
    },
    #[error(transparent)]
    Sidecar(#[from] SidecarError),
    #[error(
        "code-server on {host_port} did not answer HTTP within {timeout:?}; the sidecar started \
         but its server never came up — try again, or check `docker logs` for the sidecar"
    )]
    HttpNotReady { host_port: u16, timeout: Duration },
}

/// Resolve `service` to a single running target container on `host`.
///
/// Returns the container name (yoink's per-replica naming, e.g.
/// `service-r0-vN`) plus the docker network the sidecar should join so
/// code-server's terminal can resolve sister-service DNS.
pub async fn resolve_target(
    ops: &dyn DockerOps,
    host: &Host,
    service: &ServiceConfig,
) -> std::result::Result<(String, String), VscodeError> {
    let label = format!("yoink.service={}", service.name);
    let containers = ops
        .list_containers_by_label(host, &label)
        .await
        .map_err(|source| VscodeError::ListContainers {
            host: host.address.clone(),
            label: label.clone(),
            source,
        })?;

    let target = containers
        .into_iter()
        .find(super::docker_ops::ContainerInfo::is_running)
        .ok_or_else(|| VscodeError::TargetNotRunning {
            service: service.name.clone(),
            host: host.address.clone(),
        })?;

    // Pick a docker network: prefer the service's declared `networks:`
    // (joined deterministically) over scraping the live container, so a
    // service that's been re-attached to extra networks behind yoink's
    // back doesn't surprise us.
    let network = service
        .networks
        .as_ref()
        .and_then(|v| v.first().cloned())
        .or_else(|| target.networks.first().cloned())
        .ok_or_else(|| VscodeError::NoNetwork {
            service: service.name.clone(),
        })?;

    Ok((target.name, network))
}

/// Build + spawn the code-server sidecar; return a `SidecarHandle` whose
/// Drop / `close()` force-removes the container.
pub async fn spawn_codeserver_sidecar(
    ops: Arc<dyn DockerOps>,
    host: Host,
    target_service: &str,
    target_container: &str,
    network: &str,
) -> std::result::Result<SidecarHandle, VscodeError> {
    use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};

    let container_name = pf::unique_sidecar_name_with_prefix("yoink-vscode", target_service);

    let cached = ops
        .image_present(&host, CODE_SERVER_IMAGE, CODE_SERVER_TAG)
        .await
        .map_err(|source| SidecarError::Pull {
            host: host.address.clone(),
            source,
        })?;
    if !cached {
        ops.pull_image(&host, CODE_SERVER_IMAGE, CODE_SERVER_TAG, None)
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
            host_ip: Some("127.0.0.1".into()),
            host_port: Some("0".into()),
        }]),
    );

    let mut labels = HashMap::new();
    labels.insert("yoink.managed".into(), "true".into());
    labels.insert("yoink.kind".into(), "vscode-sidecar".into());
    labels.insert("yoink.vscode.target_service".into(), target_service.into());

    // /proc/1 inside the sidecar is the *target's* PID 1 because we share
    // the target's PID namespace; /proc/1/root is the magic symlink to
    // that PID's rootfs. Pointing code-server at it serves the target's
    // filesystem without a mount-namespace switch.
    let entrypoint = vec![
        "/usr/bin/entrypoint.sh".to_string(),
        "--auth".into(),
        "none".into(),
        "--bind-addr".into(),
        format!("0.0.0.0:{SIDECAR_INTERNAL_PORT}"),
        "/proc/1/root".into(),
    ];

    let body = ContainerCreateBody {
        image: Some(format!("{CODE_SERVER_IMAGE}:{CODE_SERVER_TAG}")),
        entrypoint: Some(entrypoint),
        exposed_ports: Some(vec![internal_key.clone()]),
        labels: Some(labels),
        // Run as root so `/proc/1/root` (owned by the target's PID 1,
        // typically root) is readable. The image's default `coder` user
        // would get EACCES on the magic symlink.
        user: Some("0:0".into()),
        host_config: Some(HostConfig {
            // Share the target's PID namespace — the *whole point* of
            // this design. Makes /proc/1 = target's PID 1.
            pid_mode: Some(format!("container:{target_container}")),
            // /proc/1/root resolves through the kernel's
            // `ptrace_may_access` check; without CAP_SYS_PTRACE the
            // sidecar gets EACCES even when both run as root, because
            // Docker's default cap set drops PTRACE. We don't need
            // ACTUAL ptrace — only the access check that gates the
            // /proc/PID/root magic symlink. SYS_ADMIN would also work
            // and is broader; PTRACE is the minimum.
            cap_add: Some(vec!["SYS_PTRACE".into()]),
            // Network is independent of the target's so docker can
            // publish 8080 on the host. Joining the target's docker
            // network gives code-server's terminal sister-service DNS.
            network_mode: Some(network.into()),
            port_bindings: Some(port_bindings),
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

    let host_port = pf::wait_for_host_port(
        ops.as_ref(),
        &host,
        &container_name,
        SIDECAR_INTERNAL_PORT,
        SIDECAR_READY_TIMEOUT,
    )
    .await?;

    Ok(SidecarHandle::from_running(
        container_name,
        host,
        host_port,
        ops,
    ))
}

/// Poll loopback `local_port` until a TCP connect succeeds — the
/// signal that the SSH tunnel is up AND code-server's listener has
/// bound. Code-server's Express handler answers immediately after
/// the listener accepts, so a successful TCP connect is enough; no
/// HTTP round-trip needed.
pub async fn wait_for_local_tcp(local_port: u16) -> std::result::Result<(), VscodeError> {
    let deadline = std::time::Instant::now() + HTTP_READY_TIMEOUT;
    loop {
        if let Ok(Ok(_)) = tokio::time::timeout(
            HTTP_READY_POLL,
            tokio::net::TcpStream::connect(("127.0.0.1", local_port)),
        )
        .await
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(VscodeError::HttpNotReady {
                host_port: local_port,
                timeout: HTTP_READY_TIMEOUT,
            });
        }
        tokio::time::sleep(HTTP_READY_POLL).await;
    }
}

/// Resolve service → host → SSH probe → spawn sidecar → SSH-tunnel →
/// open browser. Holds open until SIGINT.
#[allow(clippy::too_many_lines)]
pub async fn cmd_vscode(
    config: &Config,
    service_name: &str,
    options: VscodeOptions,
    ops: Arc<dyn DockerOps>,
) -> anyhow::Result<()> {
    use crate::transport::tunnel::SshTunnel;

    let service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| VscodeError::ServiceNotFound(service_name.to_string()))?;

    let applicable: Vec<_> = service
        .applicable_hosts(&config.hosts)
        .into_iter()
        .filter(|h| {
            options
                .host_filter
                .as_deref()
                .is_none_or(|f| f == h.address)
        })
        .collect();
    let host_cfg = match applicable.as_slice() {
        [] => {
            return Err(VscodeError::NoApplicableHost {
                service: service_name.to_string(),
                matched: options
                    .host_filter
                    .as_deref()
                    .map_or(String::new(), |f| format!(" matching --host={f}")),
            }
            .into());
        }
        [single] => *single,
        many if options.host_filter.is_some() => many[0],
        many => {
            let listing = many
                .iter()
                .map(|h| format!("  {}", h.address))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(VscodeError::AmbiguousHost {
                service: service_name.to_string(),
                n: many.len(),
                listing,
            }
            .into());
        }
    };

    if host_cfg.address == "local" {
        return Err(VscodeError::LocalHost {
            service: service_name.to_string(),
        }
        .into());
    }

    if u32::try_from(options.replica).map_or(true, |r| r >= service.run.replicas) {
        return Err(VscodeError::ReplicaOutOfRange {
            service: service_name.to_string(),
            replicas: service.run.replicas,
            replica: options.replica,
        }
        .into());
    }

    let host = Host::from(host_cfg);

    let keyfile = ops.ssh_keyfile(&host);
    crate::ssh_probe::probe(&host, keyfile.as_deref().and_then(|p| p.to_str()))
        .await
        .map_err(|e| anyhow::anyhow!("ssh probe to {}: {e}", host.address))?;

    let (target_container, network) = resolve_target(ops.as_ref(), &host, service).await?;

    let handle = spawn_codeserver_sidecar(
        ops.clone(),
        host.clone(),
        service_name,
        &target_container,
        &network,
    )
    .await?;
    let host_port = handle.host_port();

    let tunnel = SshTunnel::open_with_local_port(
        &host.user,
        &host.address,
        pf::SIDECAR_DIAL_HOST,
        host_port,
        options.local_port,
        pf::TUNNEL_READY_TIMEOUT,
        keyfile.as_deref(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("open ssh tunnel to {}: {e}", host.address))?;

    let local_port = tunnel.local_port();
    let url = format!("http://127.0.0.1:{local_port}/?folder=/proc/1/root");

    eprintln!(
        "→ {} ({}) ⇆ {url}\n  Ctrl-C to close",
        service_name, host.address
    );
    eprintln!("  waiting for code-server …");

    if let Err(e) = wait_for_local_tcp(local_port).await {
        // Tear down before propagating so we don't leave the sidecar
        // running after a readiness failure.
        handle.close().await;
        drop(tunnel);
        return Err(e.into());
    }

    if options.open
        && let Err(e) = pf::open_in_browser(&url)
    {
        eprintln!("✗ failed to open browser: {e}\n  paste into one yourself: {url}");
    }

    tokio::signal::ctrl_c()
        .await
        .map_err(|e| anyhow::anyhow!("install SIGINT handler: {e}"))?;
    handle.close().await;
    drop(tunnel);
    eprintln!("\n✓ tunnel closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vscode_error_messages_actionable() {
        let e = VscodeError::NoNetwork {
            service: "api".into(),
        };
        let s = e.to_string();
        assert!(s.contains("networks:"), "got: {s}");

        let e = VscodeError::LocalHost {
            service: "api".into(),
        };
        let s = e.to_string();
        assert!(s.contains("SSH"), "got: {s}");

        let e = VscodeError::TargetNotRunning {
            service: "api".into(),
            host: "h1".into(),
        };
        let s = e.to_string();
        assert!(s.contains("yoink up"), "got: {s}");
    }
}
