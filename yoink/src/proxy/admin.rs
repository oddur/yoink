//! Push rendered Caddy JSON to the proxy's admin API.
//!
//! Caddy's admin API listens on `:2019` *inside* the container and is
//! published on the host's loopback at an ephemeral port (chosen by
//! docker via `127.0.0.1:0:2019`). The yoink CLI:
//!
//! 1. Looks up the proxy container, reads the host-side admin port
//!    from its inspect output.
//! 2. SSH-tunnels from `operator-localhost:<rand>` to
//!    `host-localhost:<published>`.
//! 3. `POST`s the JSON to `http://127.0.0.1:<rand>/load`.
//! 4. Drops the tunnel.
//!
//! Why loopback-published instead of using the container's docker IP
//! directly: docker doesn't route user-defined-network container IPs
//! from the host, so an SSH-tunnel `-L … :<container_ip>:2019` gets
//! "connection refused". Loopback publishing is the established
//! pattern (same shape as the unregistry transport).

use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use crate::docker_ops::{DockerError, DockerOps, Host, parse_published_port};
use crate::transport::tunnel::{SshTunnel, TunnelError};

use super::{ADMIN_PORT, PROXY_SERVICE_NAME};

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
        "proxy container on {host} has no published admin port — yoink expected \
         it on `127.0.0.1:0:{ADMIN_PORT}` but inspect returned: {ports:?}"
    )]
    NoPublishedAdminPort { host: String, ports: Vec<String> },
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
/// container's host-side admin port, opens an SSH tunnel to host
/// loopback at that port, POSTs to `/load`, drops the tunnel.
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

    let detail = ops
        .inspect_container(host, &running.name)
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    let host_port = parse_published_port(&detail.ports, ADMIN_PORT).ok_or_else(|| {
        AdminError::NoPublishedAdminPort {
            host: host.address.clone(),
            ports: detail.ports.clone(),
        }
    })?;

    let keyfile = ops.ssh_keyfile(host);
    let tunnel = SshTunnel::open(
        &host.user,
        &host.address,
        host_port,
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

async fn wait_until_ready(local_port: u16, timeout: Duration) -> Result<(), AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/config/");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client builder");
    let deadline = Instant::now() + timeout;
    loop {
        if client.get(&url).send().await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AdminError::NotReady { timeout });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

