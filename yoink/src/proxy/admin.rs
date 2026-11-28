//! Push rendered Caddy JSON to the proxy's admin API.
//!
//! Caddy's admin API listens on `ADMIN_API_HOST` *inside* the
//! container: the proxy IP on the `yoink-proxy-admin` network only.
//! The proxy does not publish the admin port. The admin relay (see
//! [`super::relay`]) publishes it on the host's loopback at an
//! ephemeral port and forwards each connection to the admin API. The
//! yoink CLI:
//!
//! 1. Looks up the proxy container. If it has the admin API alias, it
//!    reads the host-side port of the relay. A proxy from an earlier
//!    yoink version has no alias and publishes the admin port itself;
//!    yoink then reads that port, see [`AdminListen::Legacy`].
//! 2. SSH-tunnels from `operator-localhost:<rand>` to
//!    `host-localhost:<published>`.
//! 3. `POST`s the JSON to `http://127.0.0.1:<rand>/load`, with the
//!    `Host` header `ADMIN_API_HOST`. Caddy rejects any other `Host`,
//!    because its listen address is not a wildcard.
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

use crate::base::host::Host;
use crate::docker_ops::{ContainerDetail, DockerError, DockerOps, parse_published_port};
use crate::ssh::tunnel::{SshTunnel, TunnelError};

use crate::config::{
    ADMIN_API_ALIAS, ADMIN_API_HOST, ADMIN_NETWORK, ADMIN_PORT, PROXY_SERVICE_NAME,
};

use super::relay::{RELAY_NAME, relay_label_filter};

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
/// Pause before retrying a failed `/load` push. Short enough to be
/// invisible to operators on the happy retry path, long enough that
/// we don't dial a fresh ssh tunnel into the same NAT/sshd state
/// that just dropped the previous one.
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

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
        "admin relay {RELAY_NAME:?} not running on {host}. Each `yoink up` with the \
         proxy on creates it again. If it stops again, run `docker logs {RELAY_NAME}` \
         on the host to see why."
    )]
    RelayNotRunning { host: String },
    #[error(
        "container {container:?} on {host} has no published admin port — yoink expected \
         it on `127.0.0.1:0:{ADMIN_PORT}` but inspect returned: {ports:?}"
    )]
    NoPublishedAdminPort {
        host: String,
        container: String,
        ports: Vec<String>,
    },
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

/// Push `config` to the proxy on `host`. Looks up the host-side admin
/// port (of the relay, or of an old proxy), opens an SSH tunnel to host
/// loopback at that port, POSTs to `/load`, drops the tunnel.
///
/// Retries once on transport-class failures (tunnel didn't come up,
/// readiness probe timed out, or `reqwest` errored mid-request) —
/// each ssh tunnel is short-lived and CI environments occasionally
/// see one drop between the readiness probe and the actual `/load`,
/// surfacing as `error sending request`. Real Caddy errors propagate
/// immediately without a retry.
///
/// The `admin` block of `config` is for a proxy that has the admin API
/// alias. A proxy from an earlier yoink version does not have it, and
/// Caddy cannot bind that address. For such a proxy, yoink sends the
/// earlier block instead, see [`AdminListen::Legacy`]. The return value
/// tells the caller which block it sent.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<AdminListen, AdminError> {
    // The discovery phase (label lookup + inspect + port parse +
    // keyfile) is invariant across retries on transport-class
    // failures, so we run it once and only re-tunnel-and-load on
    // retry. `AdminError::Docker` deliberately stays out of the
    // transient set: bollard's `with_retry` already covers the
    // ssh-pool flakiness for those calls (see `RealDockerOps`),
    // adding another retry layer here would compound the wait on
    // genuinely down hosts.
    let target = discover_admin_target(host, ops).await?;
    let config = &config_for(config, target.listen);
    match tunnel_and_load(host, &target, config).await {
        Ok(()) => Ok(target.listen),
        Err(e) if is_transient(&e) => {
            tracing::debug!(
                host = %host.address,
                error = %e,
                "transient proxy admin push error; reopening tunnel and retrying once",
            );
            tokio::time::sleep(RETRY_BACKOFF).await;
            tunnel_and_load(host, &target, config).await?;
            Ok(target.listen)
        }
        Err(e) => Err(e),
    }
}

/// The admin address that the running proxy container can bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminListen {
    /// The proxy is on `ADMIN_NETWORK` and has `ADMIN_API_ALIAS`
    /// there. yoink tunnels to the admin relay and sends
    /// [`crate::config::admin_block`].
    Alias,
    /// The proxy is from an earlier yoink version: it does not have the
    /// alias on `ADMIN_NETWORK`. Caddy cannot bind the alias address,
    /// rejects the `/load`, and keeps the old config. yoink thus sends
    /// the admin block of the earlier version, see
    /// [`legacy_admin_block`], to the admin port that the proxy
    /// publishes itself, until a deploy recreates the proxy.
    Legacy,
}

impl AdminListen {
    /// Read the admin address from the inspect output of the running
    /// proxy container.
    #[must_use]
    pub fn of(detail: &ContainerDetail) -> Self {
        let has_alias = detail
            .network_aliases
            .get(ADMIN_NETWORK)
            .is_some_and(|aliases| aliases.iter().any(|a| a == ADMIN_API_ALIAS));
        if has_alias { Self::Alias } else { Self::Legacy }
    }
}

/// The `admin` block that yoink sent on every `/load` before the admin
/// API moved to `ADMIN_API_ALIAS`. The wildcard address binds in a proxy
/// of any version. With a wildcard address, Caddy does not check the
/// `Host` header, so the `ADMIN_API_HOST` header that yoink sends is
/// accepted.
fn legacy_admin_block() -> Value {
    serde_json::json!({
        "listen": format!("0.0.0.0:{ADMIN_PORT}"),
        "enforce_origin": false,
    })
}

/// `config` with the `admin` block that `listen` can bind.
fn config_for(config: &Value, listen: AdminListen) -> Value {
    let mut config = config.clone();
    if listen == AdminListen::Legacy {
        config["admin"] = legacy_admin_block();
    }
    config
}

fn is_transient(e: &AdminError) -> bool {
    matches!(
        e,
        AdminError::Tunnel(_) | AdminError::NotReady { .. } | AdminError::Http { .. }
    )
}

/// Resolved admin endpoint for one push attempt: the host-side TCP
/// port docker published `:2019` to (of the relay or of an old proxy),
/// plus the ssh keyfile the tunnel should authenticate with.
struct AdminTarget {
    host_port: u16,
    keyfile: Option<std::path::PathBuf>,
    listen: AdminListen,
}

async fn discover_admin_target(
    host: &Host,
    ops: &dyn DockerOps,
) -> Result<AdminTarget, AdminError> {
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

    let detail = inspect(ops, host, &running.name).await?;
    let listen = AdminListen::of(&detail);
    let published = match listen {
        AdminListen::Legacy => detail,
        AdminListen::Alias => {
            let relays = ops
                .list_containers_by_label(host, &relay_label_filter())
                .await
                .map_err(|source| AdminError::Docker {
                    host: host.address.clone(),
                    source,
                })?;
            let relay = relays
                .into_iter()
                .find(|c| c.name == RELAY_NAME && c.is_running())
                .ok_or_else(|| AdminError::RelayNotRunning {
                    host: host.address.clone(),
                })?;
            inspect(ops, host, &relay.name).await?
        }
    };
    let host_port = parse_published_port(&published.ports, ADMIN_PORT).ok_or_else(|| {
        AdminError::NoPublishedAdminPort {
            host: host.address.clone(),
            container: published.name.clone(),
            ports: published.ports.clone(),
        }
    })?;

    Ok(AdminTarget {
        host_port,
        keyfile: ops.ssh_keyfile(host),
        listen,
    })
}

async fn inspect(
    ops: &dyn DockerOps,
    host: &Host,
    name: &str,
) -> Result<ContainerDetail, AdminError> {
    ops.inspect_container(host, name)
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })
}

async fn tunnel_and_load(
    host: &Host,
    target: &AdminTarget,
    config: &Value,
) -> Result<(), AdminError> {
    let tunnel = SshTunnel::open(
        host,
        target.host_port,
        TUNNEL_READY_TIMEOUT,
        target.keyfile.as_deref(),
    )
    .await?;

    wait_until_ready(tunnel.local_port(), TUNNEL_READY_TIMEOUT).await?;
    let result = load_and_verify(tunnel.local_port(), config).await;
    drop(tunnel);
    result
}

/// `POST /load` through the tunnel at `local_port`, then read the
/// route count back.
async fn load_and_verify(local_port: u16, config: &Value) -> Result<(), AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/load");
    let client = reqwest::Client::builder()
        .timeout(ADMIN_API_TIMEOUT)
        .build()
        .expect("reqwest client builder");
    let resp = client
        .post(&url)
        .header(reqwest::header::HOST, ADMIN_API_HOST)
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
    let actual = read_route_count(&client, local_port).await?;
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
        .header(reqwest::header::HOST, ADMIN_API_HOST)
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
/// end of the body (better than leaking). An END line without its
/// closing dashes is redacted to the end of that line, or to the next
/// `-----BEGIN ` if that comes first.
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
        // Search for the closing `-----` after the `-----END ` prefix:
        // the prefix itself starts with `-----`. Do not search past the
        // end of the END line or into the next block: the dashes of a
        // later `-----BEGIN ` are not the closing dashes.
        let label_start = end_marker_pos + "-----END ".len();
        let label = &after_begin[label_start..];
        let line_end = [label.find('\n'), label.find("-----BEGIN ")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(label.len());
        let block_end = label[..line_end]
            .find("-----")
            .map_or(label_start + line_end, |idx| {
                label_start + idx + "-----".len()
            });
        out.push_str(REDACTED_PEM);
        rest = &after_begin[block_end..];
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
        let probe = client
            .get(&url)
            .header(reqwest::header::HOST, ADMIN_API_HOST);
        if probe.send().await.is_ok() {
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

    fn running(name: &str, service: Option<&str>) -> crate::docker_ops::ContainerInfo {
        crate::docker_ops::ContainerInfo {
            host: "h".into(),
            name: name.into(),
            image: String::new(),
            state: "running".into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: service.map(Into::into),
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: std::collections::BTreeMap::new(),
        }
    }

    /// A fake host with one running proxy container. `network_aliases`
    /// is its inspect output: network → aliases. `ports` is what the
    /// proxy publishes.
    fn fake_proxy(
        network_aliases: &[(&str, &[&str])],
        ports: &[&str],
    ) -> crate::docker_ops::FakeDockerOps {
        let ops = crate::docker_ops::FakeDockerOps::new();
        let name = "yoink-proxy-abc1234";
        ops.push_list_containers(Ok(vec![running(name, Some(PROXY_SERVICE_NAME))]));
        ops.set_inspect(ContainerDetail {
            name: name.into(),
            ports: ports.iter().map(|p| (*p).to_string()).collect(),
            network_aliases: network_aliases
                .iter()
                .map(|(n, a)| ((*n).into(), a.iter().map(|s| (*s).into()).collect()))
                .collect(),
            ..ContainerDetail::default()
        });
        ops
    }

    /// Add a running admin relay that publishes `2019` on `host_port`.
    fn add_relay(ops: &crate::docker_ops::FakeDockerOps, host_port: u16) {
        ops.push_list_containers(Ok(vec![running(RELAY_NAME, None)]));
        ops.set_inspect(ContainerDetail {
            name: RELAY_NAME.into(),
            ports: vec![format!("{host_port}:{ADMIN_PORT}/tcp")],
            ..ContainerDetail::default()
        });
    }

    fn host() -> Host {
        Host {
            user: "u".into(),
            address: "h".into(),
            port: None,
        }
    }

    /// The tunnel port and the `admin` block that `push_config` sends
    /// to the proxy of `ops`, for the config that a deploy renders.
    async fn pushed_admin(ops: &crate::docker_ops::FakeDockerOps) -> (u16, AdminListen, Value) {
        let cfg = crate::config::Config::parse_str(
            r"
hosts: [{ address: h, user: u }]
proxy: { email: ops@example.com }
services:
  - name: api
    image: example/api
    tag: v1
    domain: api.example.com
    run: { port: 8080 }
",
        )
        .expect("config parses");
        let rendered =
            crate::proxy::caddy::render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        let target = discover_admin_target(&host(), ops).await.expect("target");
        let pushed = config_for(&rendered, target.listen);
        (target.host_port, target.listen, pushed["admin"].clone())
    }

    /// A scoped `yoink up --service api` after an upgrade keeps the
    /// proxy of the earlier version: on `yoink-proxy-admin`, without
    /// the alias, with its own published admin port. Caddy there cannot
    /// bind the alias address, so a `/load` with the new admin block
    /// was rejected, and yoink then removed the new replicas of `api`.
    #[tokio::test]
    async fn old_proxy_gets_the_earlier_admin_block_on_its_own_port() {
        let ops = fake_proxy(
            &[
                (crate::config::INGRESS_NETWORK, &["yoink-proxy-abc1234"]),
                (ADMIN_NETWORK, &["yoink-proxy-abc1234"]),
            ],
            &["80:80/tcp", "443:443/tcp", "32768:2019/tcp"],
        );
        let (port, listen, admin) = pushed_admin(&ops).await;
        assert_eq!(port, 32768);
        assert_eq!(listen, AdminListen::Legacy);
        assert_eq!(
            admin,
            serde_json::json!({"listen": "0.0.0.0:2019", "enforce_origin": false})
        );
        // No relay lookup for an old proxy.
        assert_eq!(
            ops.calls()
                .iter()
                .filter(|c| matches!(
                    c,
                    crate::docker_ops::RecordedCall::ListContainersByLabel(..)
                ))
                .count(),
            1
        );
    }

    /// A proxy with the alias publishes no admin port: Docker would put
    /// it on `yoink-ingress`, the gateway endpoint. yoink tunnels to the
    /// port of the relay.
    #[tokio::test]
    async fn new_proxy_is_reached_through_the_relay() {
        let ops = fake_proxy(
            &[
                (crate::config::INGRESS_NETWORK, &["yoink-proxy-abc1234"]),
                (ADMIN_NETWORK, &["yoink-proxy-abc1234", ADMIN_API_ALIAS]),
            ],
            &["80:80/tcp", "443:443/tcp"],
        );
        add_relay(&ops, 40000);
        let (port, listen, admin) = pushed_admin(&ops).await;
        assert_eq!(port, 40000);
        assert_eq!(listen, AdminListen::Alias);
        assert_eq!(admin, crate::config::admin_block());
    }

    #[tokio::test]
    async fn new_proxy_without_a_relay_is_an_error() {
        let ops = fake_proxy(
            &[(ADMIN_NETWORK, &["yoink-proxy-abc1234", ADMIN_API_ALIAS])],
            &[],
        );
        ops.push_list_containers(Ok(Vec::new()));
        let err = discover_admin_target(&host(), &ops)
            .await
            .err()
            .expect("no relay");
        assert!(
            matches!(err, AdminError::RelayNotRunning { .. }),
            "got: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Each `yoink up` with the proxy on"),
            "got: {msg}"
        );
        assert!(
            msg.contains("`docker logs yoink-proxy-admin-relay`"),
            "got: {msg}"
        );
        assert!(!msg.contains("unscoped"), "got: {msg}");
    }

    #[test]
    fn redact_strips_full_pem_blocks() {
        let body = "json error at offset 42: -----BEGIN CERTIFICATE-----\nABCDEF\n-----END CERTIFICATE-----\n more context";
        let out = redact_pem_blocks(body);
        assert!(out.contains("<redacted PEM>"));
        assert!(!out.contains("ABCDEF"));
        assert!(out.contains("more context"));
    }

    /// The search for the closing `-----` matched the `-----` of the
    /// `-----END ` prefix itself, so `END CERTIFICATE-----` stayed in
    /// the output.
    #[test]
    fn redact_removes_the_end_line() {
        let body = "a -----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE----- b \
                    -----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY----- c";
        assert_eq!(
            redact_pem_blocks(body),
            "a <redacted PEM> b <redacted PEM> c"
        );
    }

    /// An END line without its closing dashes made the search find the
    /// dashes of the next `-----BEGIN `, so the next block leaked.
    #[test]
    fn redact_end_line_without_dashes_keeps_next_block_redacted() {
        let body = "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE\n\
                    -----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----";
        let out = redact_pem_blocks(body);
        assert!(!out.contains("SECRET"), "got: {out}");
        assert!(!out.contains("AAA"), "got: {out}");
        assert_eq!(out, "<redacted PEM>\n<redacted PEM>");
    }

    /// On one line (a JSON-escaped body), an END without dashes stops
    /// at the next `-----BEGIN `.
    #[test]
    fn redact_end_without_dashes_stops_at_next_begin() {
        let body = "-----BEGIN CERTIFICATE-----AAA-----END CERTIFICATE \
                    -----BEGIN PRIVATE KEY-----SECRET-----END PRIVATE KEY----- tail";
        assert_eq!(redact_pem_blocks(body), "<redacted PEM><redacted PEM> tail");
    }

    #[test]
    fn redact_several_blocks_keeps_text_between_them() {
        let body = "x -----BEGIN A-----\n1\n-----END A-----\ny\n\
                    -----BEGIN B-----\n2\n-----END B-----\nz\n\
                    -----BEGIN C-----\n3\n-----END C----- w";
        assert_eq!(
            redact_pem_blocks(body),
            "x <redacted PEM>\ny\n<redacted PEM>\nz\n<redacted PEM> w"
        );
    }

    #[test]
    fn redact_handles_unterminated_pem() {
        let body = "load failed: -----BEGIN PRIVATE KEY-----\nMIIabc... (truncated)";
        let out = redact_pem_blocks(body);
        assert!(out.contains("<redacted PEM>"));
        assert!(!out.contains("MIIabc"));
    }

    /// Serve `responses.len()` HTTP requests on a loopback port, one
    /// per connection, and return the request heads in order.
    fn serve(responses: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for body in responses {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    head.push_str(&line);
                }
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .map_or(0, |v| v.parse::<usize>().expect("length"));
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).expect("body");
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                reader.get_mut().write_all(reply.as_bytes()).expect("write");
                heads.push(head);
            }
            heads
        });
        (port, handle)
    }

    fn host_header(head: &str) -> Option<&str> {
        head.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("host").then(|| value.trim())
        })
    }

    /// Caddy checks `Host` against `admin.origins` because the admin
    /// listen address is not a wildcard. Without the fixed header,
    /// reqwest sends `127.0.0.1:<tunnel port>` and Caddy answers 403.
    #[tokio::test]
    async fn admin_requests_send_the_admin_host_header() {
        let (port, server) = serve(vec!["{}", "", "[]"]);
        wait_until_ready(port, Duration::from_secs(5))
            .await
            .expect("ready");
        load_and_verify(port, &serde_json::json!({"apps": {}}))
            .await
            .expect("load");
        let heads = server.join().expect("server thread");
        assert_eq!(heads.len(), 3);
        assert!(heads[0].starts_with("GET /config/ "), "got: {}", heads[0]);
        assert!(heads[1].starts_with("POST /load "), "got: {}", heads[1]);
        assert!(
            heads[2].starts_with("GET /config/apps/http/servers/main/routes/ "),
            "got: {}",
            heads[2]
        );
        for head in &heads {
            assert_eq!(host_header(head), Some(ADMIN_API_HOST), "got: {head}");
        }
    }

    #[test]
    fn redact_passthrough_when_no_pem() {
        let body = r#"{"error":"unknown directive 'foo'"}"#;
        assert_eq!(redact_pem_blocks(body), body);
    }
}
