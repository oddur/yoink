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

/// How long the SSH tunnel + Caddy admin readiness probe each get
/// before giving up. 20s comfortably covers a cold proxy boot on a
/// laggy host without making transient ssh blips look like real outages.
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(20);
/// HTTP timeout for the actual admin-API push. The push uploads the
/// rendered Caddy JSON; 60s is plenty for any realistic config.
const ADMIN_API_TIMEOUT: Duration = Duration::from_secs(60);
/// HTTP timeout for the readiness probe loop. Short — we expect a
/// fast no-content response or fast failure.
const ADMIN_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Backoff between readiness probes while the proxy is coming up.
const ADMIN_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
    /// `/load` returned 200 but reading `/config/...routes/` back yielded
    /// a different route count. Caught early so a silent under-load
    /// doesn't end up serving 521 for routes the operator believes are
    /// up. Triggered today (#88) only by Caddy parser quirks; cheap
    /// insurance against future shapes of the same bug.
    #[error(
        "proxy /load reported success but route count mismatch: \
         pushed {expected}, daemon reports {actual}"
    )]
    LoadVerifyMismatch { expected: usize, actual: usize },
}

/// Push `config` to the proxy on `host`. Looks up the proxy
/// container's host-side admin port, opens an SSH tunnel to host
/// loopback at that port, POSTs to `/load`, drops the tunnel.
///
/// Retries once on transport-class failures (tunnel didn't come up,
/// readiness probe timed out, or `reqwest` errored mid-request) —
/// each ssh tunnel is short-lived and CI environments occasionally
/// see one drop between the readiness probe and the actual `/load`,
/// surfacing as `error sending request`. Real Caddy errors
/// (`LoadRejected`, `LoadVerifyMismatch`) propagate immediately
/// without a retry.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<(), AdminError> {
    match push_config_once(host, ops, config).await {
        Ok(()) => Ok(()),
        Err(e) if is_transient(&e) => {
            tracing::debug!(
                host = %host.address,
                error = %e,
                "transient proxy admin push error; reopening tunnel and retrying once",
            );
            push_config_once(host, ops, config).await
        }
        Err(e) => Err(e),
    }
}

fn is_transient(e: &AdminError) -> bool {
    matches!(
        e,
        AdminError::Tunnel(_) | AdminError::NotReady { .. } | AdminError::Http { .. }
    )
}

async fn push_config_once(
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
        TUNNEL_READY_TIMEOUT,
        keyfile.as_deref(),
    )
    .await?;

    wait_until_ready(tunnel.local_port(), TUNNEL_READY_TIMEOUT).await?;

    let url = format!("http://127.0.0.1:{}/load", tunnel.local_port());
    let client = reqwest::Client::builder()
        .timeout(ADMIN_API_TIMEOUT)
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
        // Caddy parser errors can echo a slice of the submitted JSON,
        // which carries inline cert+key PEMs from
        // `tls.certificates.load_pem`. Strip them before the error
        // lands in stderr / tracing / TUI toasts.
        let body = redact_pem_blocks(&resp.text().await.unwrap_or_default());
        return Err(AdminError::LoadRejected { status, body });
    }

    // Belt-and-braces: read the route count back from the daemon and
    // compare. `/load` is synchronous, so a 200 already implies the
    // config is live — but a future Caddy version that silently
    // drops routes (or any third-party admin-API shim) would otherwise
    // leave us claiming success while the proxy serves 5xx for those
    // domains. Verifying upfront beats discovering it from a Cloudflare
    // 521 alert.
    let expected = route_count(config);
    let actual = read_route_count(&client, tunnel.local_port()).await?;
    drop(tunnel);
    if expected != actual {
        return Err(AdminError::LoadVerifyMismatch { expected, actual });
    }
    Ok(())
}

fn route_count(config: &Value) -> usize {
    config
        .pointer("/apps/http/servers/main/routes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

async fn read_route_count(client: &reqwest::Client, local_port: u16) -> Result<usize, AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/config/apps/http/servers/main/routes/");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|source| AdminError::Http { source })?;
    // 404 = no routes object at that pointer (e.g. a config with no
    // routed services). Treat as zero rather than an error so the
    // verify step doesn't trip on legitimate empty configs.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(0);
    }
    let routes: Value = resp
        .json()
        .await
        .map_err(|source| AdminError::Http { source })?;
    Ok(routes.as_array().map_or(0, Vec::len))
}

const REDACTED_PEM: &str = "<redacted PEM>";

/// Replace `-----BEGIN <kind>----- … -----END <kind>-----` regions
/// with a placeholder; an unterminated BEGIN is over-redacted to the
/// end of the body (better than leaking).
fn redact_pem_blocks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    loop {
        let Some(begin_pos) = rest.find("-----BEGIN ") else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..begin_pos]);
        let after_begin = &rest[begin_pos..];
        let Some(end_marker_pos) = after_begin.find("-----END ") else {
            out.push_str(REDACTED_PEM);
            return out;
        };
        let tail = &after_begin[end_marker_pos..];
        let after_end_label = tail
            .find("-----")
            .map_or(after_begin.len(), |idx| idx + "-----".len());
        out.push_str(REDACTED_PEM);
        rest = &after_begin[end_marker_pos + after_end_label..];
    }
}

async fn wait_until_ready(local_port: u16, timeout: Duration) -> Result<(), AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/config/");
    let client = reqwest::Client::builder()
        .timeout(ADMIN_PROBE_TIMEOUT)
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
        tokio::time::sleep(ADMIN_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_full_pem_blocks() {
        let body = "json error at offset 42: -----BEGIN CERTIFICATE-----\nABCDEF\n-----END CERTIFICATE-----\n more context";
        let out = redact_pem_blocks(body);
        assert!(out.contains("<redacted PEM>"));
        assert!(!out.contains("ABCDEF"));
        assert!(out.contains("more context"));
    }

    #[test]
    fn redact_handles_unterminated_pem() {
        let body = "load failed: -----BEGIN PRIVATE KEY-----\nMIIabc... (truncated)";
        let out = redact_pem_blocks(body);
        assert!(out.contains("<redacted PEM>"));
        assert!(!out.contains("MIIabc"));
    }

    #[test]
    fn redact_passthrough_when_no_pem() {
        let body = r#"{"error":"unknown directive 'foo'"}"#;
        assert_eq!(redact_pem_blocks(body), body);
    }
}
