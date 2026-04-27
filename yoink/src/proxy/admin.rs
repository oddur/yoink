//! Push rendered Caddy JSON to the proxy's admin API.
//!
//! Caddy's admin API listens on `:2019` *inside* the container, on
//! the `yoink-proxy-admin` Docker network only (never published to
//! the host). The yoink CLI reaches it for the duration of one
//! `/load` call by:
//!
//! 1. Looking up the proxy container's IP on the admin network.
//! 2. Opening an SSH-tunnelled forward from `localhost:<rand>` →
//!    `<container_ip>:2019` on the host.
//! 3. `POST`ing the JSON to `http://localhost:<rand>/load` with
//!    `Content-Type: application/json`.
//! 4. Dropping the tunnel.
//!
//! The state machine integration in `deploy.rs` calls
//! [`push_config`] at the routing-flip moment of the rolling swap.

use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use crate::docker_ops::{DockerError, DockerOps, Host};
use crate::transport::tunnel::{SshTunnel, TunnelError};

use super::{ADMIN_NETWORK, ADMIN_PORT, PROXY_SERVICE_NAME};

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("docker op against {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error("proxy container {PROXY_SERVICE_NAME:?} not running on {host}")]
    ProxyNotRunning { host: String },
    #[error(
        "proxy container on {host} is not attached to {ADMIN_NETWORK:?} \
         (yoink expected the implicit injection to put it there)"
    )]
    NoAdminNetwork { host: String },
    #[error("ssh tunnel to proxy admin: {0}")]
    Tunnel(#[from] TunnelError),
    #[error("proxy admin endpoint did not respond within {timeout:?}")]
    NotReady { timeout: Duration },
    #[error("HTTP error talking to proxy admin: {source}")]
    Http {
        #[source]
        source: reqwest::Error,
    },
    #[error("proxy admin rejected /load with status {status}: {body}")]
    LoadRejected {
        status: reqwest::StatusCode,
        body: String,
    },
}

/// Push `config` to the proxy on `host`. Looks up the proxy
/// container's IP on the admin network, opens an SSH tunnel, POSTs
/// to `/load`, drops the tunnel.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<(), AdminError> {
    let label = format!("yoink.service={PROXY_SERVICE_NAME}");
    let containers = ops
        .list_containers_by_label(host, &label)
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    let running = containers
        .into_iter()
        .find(crate::docker_ops::ContainerInfo::is_running)
        .ok_or_else(|| AdminError::ProxyNotRunning {
            host: host.address.clone(),
        })?;

    // The admin-network IP isn't carried in `ContainerDetail.networks`
    // (which is just network names). Use a one-shot exec against the
    // proxy container itself to read its own IP — see `pick_admin_ip`.
    let ip = pick_admin_ip(host, ops, &running.name).await?;

    let keyfile = ops.ssh_keyfile(host);
    let tunnel = SshTunnel::open_to(
        &host.user,
        &host.address,
        &ip,
        ADMIN_PORT,
        Duration::from_secs(20),
        keyfile.as_deref(),
    )
    .await?;

    wait_until_ready(tunnel.local_port(), Duration::from_secs(20)).await?;

    let url = format!("http://127.0.0.1:{}/load", tunnel.local_port());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client builder");
    let resp = client
        .post(&url)
        .json(config)
        .send()
        .await
        .map_err(|source| AdminError::Http { source })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AdminError::LoadRejected { status, body });
    }
    drop(tunnel);
    Ok(())
}

async fn pick_admin_ip(
    host: &Host,
    ops: &dyn DockerOps,
    container: &str,
) -> Result<String, AdminError> {
    let detail = ops
        .inspect_container(host, container)
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    if !detail.networks.iter().any(|n| n == ADMIN_NETWORK) {
        return Err(AdminError::NoAdminNetwork {
            host: host.address.clone(),
        });
    }
    // ContainerDetail surfaces network names but not IPs. Use the
    // bollard inspect raw response indirectly via a one-shot exec
    // probe: ask the container to print its address on the admin
    // network. Cheap, no schema changes to ContainerDetail.
    //
    // Caddy doesn't ship with `ip` or `hostname -I`. Use `getent
    // hosts <self-name>` which works on Alpine + Debian + Caddy's
    // distroless variants because nsswitch falls back to /etc/hosts
    // where Docker writes the container's own IPs.
    let result = ops
        .exec_oneshot(
            host,
            container,
            vec!["sh".into(), "-c".into(), format!("getent hosts {container} | awk '{{print $1}}' | head -1")],
        )
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    let ip = result.stdout.trim().to_string();
    if ip.is_empty() {
        return Err(AdminError::NoAdminNetwork {
            host: host.address.clone(),
        });
    }
    Ok(ip)
}

async fn wait_until_ready(local_port: u16, timeout: Duration) -> Result<(), AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/config/");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client builder");
    let deadline = Instant::now() + timeout;
    loop {
        match client.get(&url).send().await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            Err(_) => return Err(AdminError::NotReady { timeout }),
        }
    }
}
