//! Push rendered Caddy JSON to the proxy's admin API.
//!
//! Caddy's admin API listens on the unix socket
//! [`crate::config::ADMIN_SOCKET`] in the proxy container. It has no
//! network address. The admin relay (see [`super::relay`]) mounts the
//! socket and publishes its port [`RELAY_PORT`] on the host's loopback
//! at an ephemeral port. The yoink CLI:
//!
//! 1. Looks up the running proxy and the host-side port of the relay.
//! 2. SSH-tunnels from `operator-localhost:<rand>` to
//!    `host-localhost:<published>`, see [`DockerOps::forward_loopback`].
//! 3. Waits until the admin API answers through the relay, `POST`s the
//!    JSON to `/load`, and reads the route count back.
//! 4. Drops the tunnel.
//!
//! A proxy of yoink v0.22 or earlier listens on `0.0.0.0:2019` and
//! publishes that port itself. The first push to such a proxy goes to
//! that port and moves the admin API to the socket, see
//! [`move_admin_to_socket`]. Every later push goes through the relay.
//!
//! Why loopback-published instead of using the container's docker IP
//! directly: docker doesn't route user-defined-network container IPs
//! from the host, so an SSH-tunnel to `<container_ip>:2020` gets
//! "connection refused". Loopback publishing is the established
//! pattern (same shape as the unregistry transport).

use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;

use crate::base::host::Host;
use crate::config::{LEGACY_ADMIN_PORT, PROXY_SERVICE_NAME};
use crate::docker_ops::{ContainerDetail, DockerError, DockerOps, parse_published_port};
use crate::ssh::tunnel::TunnelError;

use super::relay::{RELAY_NAME, RELAY_PORT, relay_label_filter};

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
         it on `127.0.0.1:0:{port}` but inspect returned: {ports:?}"
    )]
    NoPublishedAdminPort {
        host: String,
        container: String,
        port: u16,
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
    /// The proxy is from yoink v0.22 or earlier. The push to its own
    /// admin port failed, and then the push through the relay failed.
    #[error(
        "the proxy publishes the admin port {LEGACY_ADMIN_PORT} of yoink v0.22 or earlier. \
         The push to that port failed ({legacy}), and the push through the admin relay \
         failed too: {relay}"
    )]
    UpgradeFailed {
        legacy: Box<AdminError>,
        relay: Box<AdminError>,
    },
}

/// Push `config` to the proxy on `host`, through the admin relay.
///
/// The `admin` block of `config` puts the admin API on the socket. A
/// proxy of yoink v0.22 or earlier gets the push on its own published
/// admin port first, see [`move_admin_to_socket`].
///
/// Retries once on transport-class failures (tunnel didn't come up,
/// readiness probe timed out, or `reqwest` errored mid-request) —
/// each ssh tunnel is short-lived and CI environments occasionally
/// see one drop between the readiness probe and the actual `/load`,
/// surfacing as `error sending request`. Real Caddy errors propagate
/// immediately without a retry.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<(), AdminError> {
    let proxy = running_proxy(host, ops).await?;
    // The relay must run before yoink moves the admin API of an old
    // proxy to the socket: else nothing reaches the admin API after.
    let relay_port = relay_port(host, ops).await?;
    let legacy_error = match legacy_admin_port(&proxy) {
        Some(port) => move_admin_to_socket(host, ops, port, config).await?,
        None => None,
    };
    push_through_relay(host, ops, relay_port, config)
        .await
        .map_err(|relay| match legacy_error {
            Some(legacy) => AdminError::UpgradeFailed {
                legacy: Box::new(legacy),
                relay: Box::new(relay),
            },
            None => relay,
        })
}

/// True when `proxy`, the inspect of a proxy container, is from yoink
/// v0.22 or earlier: it publishes the admin port itself.
#[must_use]
pub fn is_legacy_proxy(proxy: &ContainerDetail) -> bool {
    legacy_admin_port(proxy).is_some()
}

/// The host port of the admin port that a proxy of yoink v0.22 or
/// earlier publishes. `None` for a proxy of this version.
fn legacy_admin_port(proxy: &ContainerDetail) -> Option<u16> {
    parse_published_port(&proxy.ports, LEGACY_ADMIN_PORT)
}

/// The first push to a proxy of yoink v0.22 or earlier. Caddy there
/// listens on `0.0.0.0:2019`, published on the host loopback at
/// `port`. yoink sends `config` to that port. Its `admin` block moves
/// the admin API to the socket, and Caddy saves it in `autosave.json`.
/// Caddy answers the `/load` and then closes the TCP listener. A
/// restart of this proxy, and a new proxy that resumes the saved
/// config, listen on the socket.
///
/// The read-back of the routes happens through the relay, after this.
///
/// - A rejection by Caddy is the result: Caddy keeps the old config and
///   the TCP listener.
/// - A transport error does not stop the push. An earlier push may have
///   moved the admin API to the socket already (its answer was lost),
///   and then nothing listens on the port. The push through the relay
///   decides. The error comes back for the message if that push fails
///   too.
async fn move_admin_to_socket(
    host: &Host,
    ops: &dyn DockerOps,
    port: u16,
    config: &Value,
) -> Result<Option<AdminError>, AdminError> {
    let attempt = || async {
        let tunnel = ops
            .forward_loopback(host, port, TUNNEL_READY_TIMEOUT)
            .await?;
        post_load(&http_client(ADMIN_API_TIMEOUT), tunnel.local_port(), config).await
    };
    let result = match attempt().await {
        Err(e) if !matches!(e, AdminError::LoadRejected { .. }) => {
            tokio::time::sleep(RETRY_BACKOFF).await;
            attempt().await
        }
        other => other,
    };
    match result {
        Ok(()) => Ok(None),
        Err(e @ AdminError::LoadRejected { .. }) => Err(e),
        Err(e) => {
            tracing::debug!(
                host = %host.address,
                error = %e,
                "push to the admin port of an old proxy failed; trying the relay",
            );
            Ok(Some(e))
        }
    }
}

/// The push through the relay at host port `relay_port`, with one retry
/// on a transport-class failure.
async fn push_through_relay(
    host: &Host,
    ops: &dyn DockerOps,
    relay_port: u16,
    config: &Value,
) -> Result<(), AdminError> {
    // The discovery phase (label lookup + inspect + port parse) is
    // invariant across retries on transport-class failures, so we run
    // it once and only re-tunnel-and-load on retry.
    // `AdminError::Docker` deliberately stays out of the transient set:
    // bollard's `with_retry` already covers the ssh-pool flakiness for
    // those calls (see `RealDockerOps`), adding another retry layer here
    // would compound the wait on genuinely down hosts.
    match tunnel_and_load(host, ops, relay_port, config).await {
        Err(e) if is_transient(&e) => {
            tracing::debug!(
                host = %host.address,
                error = %e,
                "transient proxy admin push error; reopening tunnel and retrying once",
            );
            tokio::time::sleep(RETRY_BACKOFF).await;
            tunnel_and_load(host, ops, relay_port, config).await
        }
        other => other,
    }
}

/// A 502 comes from the relay: it could not reach the admin socket, or
/// the connection broke. `/load` is idempotent, so one retry is safe.
fn is_transient(e: &AdminError) -> bool {
    matches!(
        e,
        AdminError::Tunnel(_) | AdminError::NotReady { .. } | AdminError::Http { .. }
    ) || matches!(e, AdminError::LoadRejected { status, .. } if is_gateway_error(*status))
}

/// The relay answers 502, 503 or 504 when the admin API behind it does
/// not answer.
fn is_gateway_error(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 502..=504)
}

/// The inspect of the running proxy container on `host`.
async fn running_proxy(host: &Host, ops: &dyn DockerOps) -> Result<ContainerDetail, AdminError> {
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
    inspect(ops, host, &running.name).await
}

/// The host-side port of the running relay on `host`.
async fn relay_port(host: &Host, ops: &dyn DockerOps) -> Result<u16, AdminError> {
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
    let detail = inspect(ops, host, &relay.name).await?;
    parse_published_port(&detail.ports, RELAY_PORT).ok_or_else(|| {
        AdminError::NoPublishedAdminPort {
            host: host.address.clone(),
            container: detail.name.clone(),
            port: RELAY_PORT,
            ports: detail.ports.clone(),
        }
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
    ops: &dyn DockerOps,
    relay_port: u16,
    config: &Value,
) -> Result<(), AdminError> {
    let tunnel = ops
        .forward_loopback(host, relay_port, TUNNEL_READY_TIMEOUT)
        .await?;
    wait_until_ready(tunnel.local_port(), TUNNEL_READY_TIMEOUT).await?;
    let result = load_and_verify(tunnel.local_port(), config).await;
    drop(tunnel);
    result
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client builder")
}

/// `POST /load` through the tunnel at `local_port`. The relay sets the
/// `Host` header that Caddy accepts on the socket.
async fn post_load(
    client: &reqwest::Client,
    local_port: u16,
    config: &Value,
) -> Result<(), AdminError> {
    let url = format!("http://127.0.0.1:{local_port}/load");
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
    Ok(())
}

/// `POST /load` through the tunnel at `local_port`, then read the
/// route count back.
async fn load_and_verify(local_port: u16, config: &Value) -> Result<(), AdminError> {
    let client = http_client(ADMIN_API_TIMEOUT);
    post_load(&client, local_port, config).await?;

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
    let client = http_client(ADMIN_PROBE_TIMEOUT);
    let deadline = Instant::now() + timeout;
    loop {
        // Through the relay, a socket that does not answer gives a 502.
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|resp| !is_gateway_error(resp.status()))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AdminError::NotReady { timeout });
        }
        tokio::time::sleep(ADMIN_POLL_INTERVAL).await;
    }
}

/// A small stand-in for the Caddy admin API, for tests: an HTTP server
/// on a loopback port that keeps the last `/load` and answers the
/// route read-back from it.
#[cfg(test)]
pub(crate) mod fake_caddy {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    /// One request that the fake answered.
    #[derive(Debug, Clone)]
    pub(crate) struct Request {
        pub method: String,
        pub path: String,
        pub body: String,
    }

    #[derive(Default)]
    struct State {
        requests: Vec<Request>,
        config: Option<Value>,
        /// Status for each `/load`. 200 when empty.
        load_status: Vec<u16>,
        /// Route count of the read-back, instead of the loaded one.
        routes: Option<usize>,
    }

    pub(crate) struct FakeCaddy {
        pub port: u16,
        state: Arc<Mutex<State>>,
    }

    impl FakeCaddy {
        /// Start the server on a free loopback port. The thread ends
        /// with the test process.
        pub(crate) fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let state = Arc::new(Mutex::new(State::default()));
            let shared = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    serve_one(stream, &shared);
                }
            });
            Self { port, state }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().expect("fake caddy state")
        }

        /// Answer the next `/load` requests with these statuses.
        pub(crate) fn load_statuses(&self, statuses: &[u16]) {
            self.lock().load_status = statuses.to_vec();
        }

        pub(crate) fn requests(&self) -> Vec<Request> {
            self.lock().requests.clone()
        }

        /// The bodies of the `/load` requests, as JSON.
        pub(crate) fn loads(&self) -> Vec<Value> {
            self.requests()
                .iter()
                .filter(|r| r.method == "POST" && r.path == "/load")
                .map(|r| serde_json::from_str(&r.body).expect("load body is JSON"))
                .collect()
        }
    }

    /// A loopback port on which nothing listens.
    pub(crate) fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }

    fn serve_one(stream: std::net::TcpStream, state: &Mutex<State>) {
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let mut parts = head.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        let body = String::from_utf8_lossy(&body).to_string();
        let (status, reply) = {
            let mut s = state.lock().expect("fake caddy state");
            s.requests.push(Request {
                method: method.clone(),
                path: path.clone(),
                body: body.clone(),
            });
            answer(&mut s, &method, &path, &body)
        };
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = reader.get_mut().write_all(response.as_bytes());
    }

    fn answer(s: &mut State, method: &str, path: &str, body: &str) -> (u16, String) {
        match (method, path) {
            ("POST", "/load") => {
                let status = if s.load_status.is_empty() {
                    200
                } else {
                    s.load_status.remove(0)
                };
                if status == 200 {
                    s.config = serde_json::from_str(body).ok();
                    (200, String::new())
                } else {
                    (status, "{\"error\":\"rejected\"}".to_string())
                }
            }
            ("GET", "/config/") => (200, s.config.clone().unwrap_or(Value::Null).to_string()),
            ("GET", "/config/apps/http/servers/main/routes/") => {
                let loaded = s
                    .config
                    .as_ref()
                    .and_then(|c| c.pointer("/apps/http/servers/main/routes"))
                    .and_then(Value::as_array)
                    .map(Vec::len);
                match s.routes.or(loaded) {
                    Some(n) => (200, Value::Array(vec![Value::Null; n]).to_string()),
                    None => (404, String::new()),
                }
            }
            _ => (404, String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake_caddy::{FakeCaddy, closed_port};
    use super::*;
    use crate::docker_ops::{FakeDockerOps, RecordedCall};

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

    const PROXY: &str = "yoink-proxy-abc1234";
    /// The host ports that the fake host publishes.
    const RELAY_HOST_PORT: u16 = 40000;
    const LEGACY_HOST_PORT: u16 = 32768;

    /// A fake host with one running proxy that publishes `ports`, and a
    /// running relay when `relay` is true.
    fn fake_host(ports: &[&str], relay: bool) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running(PROXY, Some(PROXY_SERVICE_NAME))]));
        ops.set_inspect(ContainerDetail {
            name: PROXY.into(),
            ports: ports.iter().map(|p| (*p).to_string()).collect(),
            ..ContainerDetail::default()
        });
        if relay {
            ops.push_list_containers(Ok(vec![running(RELAY_NAME, None)]));
            ops.set_inspect(ContainerDetail {
                name: RELAY_NAME.into(),
                ports: vec![format!("{RELAY_HOST_PORT}:2020/tcp")],
                ..ContainerDetail::default()
            });
        } else {
            ops.push_list_containers(Ok(Vec::new()));
        }
        ops
    }

    /// The proxy of v0.22 and earlier: it publishes the admin port.
    fn old_proxy_ports() -> [&'static str; 3] {
        ["80:80/tcp", "443:443/tcp", "32768:2019/tcp"]
    }

    fn host() -> Host {
        Host {
            user: "u".into(),
            address: "h".into(),
            port: None,
        }
    }

    /// The config that a deploy renders for one routed service.
    fn rendered() -> Value {
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
        crate::proxy::caddy::render(&cfg, |_| vec!["api-1".into()], None).expect("render")
    }

    fn forwards(ops: &FakeDockerOps) -> Vec<u16> {
        ops.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::ForwardLoopback(_, port) => Some(port),
                _ => None,
            })
            .collect()
    }

    fn paths(caddy: &FakeCaddy) -> Vec<String> {
        caddy
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }

    /// A proxy of this version publishes no admin port. yoink pushes
    /// through the relay only.
    #[tokio::test]
    async fn new_proxy_is_reached_through_the_relay() {
        let relay = FakeCaddy::start();
        let ops = fake_host(&["80:80/tcp", "443:443/tcp"], true);
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        push_config(&host(), &ops, &rendered()).await.expect("push");
        assert_eq!(forwards(&ops), [RELAY_HOST_PORT]);
        assert_eq!(
            paths(&relay),
            [
                "GET /config/",
                "POST /load",
                "GET /config/apps/http/servers/main/routes/"
            ]
        );
        assert_eq!(
            relay.loads()[0]["admin"],
            serde_json::json!({ "listen": "unix//config/caddy/admin.sock" })
        );
    }

    /// v0.22 and earlier: Caddy listens on `0.0.0.0:2019`, and every
    /// container on `yoink-ingress` can reach it. The first push goes
    /// to the published admin port and moves the admin API to the
    /// socket. The read-back goes through the relay.
    #[tokio::test]
    async fn first_push_to_an_old_proxy_moves_the_admin_api_to_the_socket() {
        let legacy = FakeCaddy::start();
        let relay = FakeCaddy::start();
        let ops = fake_host(&old_proxy_ports(), true);
        ops.forward_loopback_to(LEGACY_HOST_PORT, legacy.port);
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        push_config(&host(), &ops, &rendered()).await.expect("push");
        assert_eq!(forwards(&ops), [LEGACY_HOST_PORT, RELAY_HOST_PORT]);
        // One `/load` on the old port, no read-back there: Caddy
        // closes the TCP listener after it answers.
        assert_eq!(paths(&legacy), ["POST /load"]);
        assert_eq!(
            legacy.loads()[0]["admin"],
            serde_json::json!({ "listen": "unix//config/caddy/admin.sock" })
        );
        assert_eq!(legacy.loads()[0], rendered());
        assert_eq!(
            paths(&relay),
            [
                "GET /config/",
                "POST /load",
                "GET /config/apps/http/servers/main/routes/"
            ]
        );
    }

    /// An earlier push moved the admin API of the old proxy to the
    /// socket, but its answer was lost. Nothing listens on the old port
    /// now. The relay decides.
    #[tokio::test]
    async fn old_proxy_with_the_admin_api_on_the_socket_is_reached_through_the_relay() {
        let relay = FakeCaddy::start();
        let ops = fake_host(&old_proxy_ports(), true);
        ops.forward_loopback_to(LEGACY_HOST_PORT, closed_port());
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        push_config(&host(), &ops, &rendered()).await.expect("push");
        assert_eq!(relay.loads().len(), 1);
    }

    /// Caddy rejects the config: it keeps the old one and the TCP
    /// listener. yoink does not try the relay.
    #[tokio::test]
    async fn rejected_push_to_an_old_proxy_stops_the_push() {
        let legacy = FakeCaddy::start();
        legacy.load_statuses(&[400]);
        let relay = FakeCaddy::start();
        let ops = fake_host(&old_proxy_ports(), true);
        ops.forward_loopback_to(LEGACY_HOST_PORT, legacy.port);
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        let err = push_config(&host(), &ops, &rendered())
            .await
            .expect_err("rejected");
        assert!(
            matches!(err, AdminError::LoadRejected { status, .. } if status.as_u16() == 400),
            "got: {err}"
        );
        assert!(relay.requests().is_empty());
    }

    /// Without a relay, yoink does not move the admin API of an old
    /// proxy: nothing could reach the socket after.
    #[tokio::test]
    async fn old_proxy_without_a_relay_keeps_its_admin_port() {
        let legacy = FakeCaddy::start();
        let ops = fake_host(&old_proxy_ports(), false);
        ops.forward_loopback_to(LEGACY_HOST_PORT, legacy.port);
        let err = push_config(&host(), &ops, &rendered())
            .await
            .expect_err("no relay");
        assert!(
            matches!(err, AdminError::RelayNotRunning { .. }),
            "got: {err}"
        );
        assert!(legacy.requests().is_empty());
        assert!(forwards(&ops).is_empty());
    }

    /// Both paths fail: the error names both.
    #[tokio::test]
    async fn failed_upgrade_names_both_errors() {
        let ops = fake_host(&old_proxy_ports(), true);
        ops.forward_loopback_to(LEGACY_HOST_PORT, closed_port());
        // No forward to the relay port: the tunnel fails.
        let err = push_config(&host(), &ops, &rendered())
            .await
            .expect_err("both fail");
        let AdminError::UpgradeFailed { legacy, relay } = &err else {
            panic!("got: {err}");
        };
        assert!(matches!(**legacy, AdminError::Http { .. }), "got: {legacy}");
        assert!(matches!(**relay, AdminError::Tunnel(_)), "got: {relay}");
        assert!(
            err.to_string()
                .contains("publishes the admin port 2019 of yoink v0.22 or earlier"),
            "got: {err}"
        );
    }

    /// yoink reads only the relay port 2020.
    #[tokio::test]
    async fn relay_port_is_2020() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running(RELAY_NAME, None)]));
        ops.set_inspect(ContainerDetail {
            name: RELAY_NAME.into(),
            ports: vec!["40000:2019/tcp".into()],
            ..ContainerDetail::default()
        });
        let err = relay_port(&host(), &ops).await.expect_err("no relay port");
        assert!(err.to_string().contains("`127.0.0.1:0:2020`"), "got: {err}");
    }

    /// A 502 from the relay means that the admin API did not answer.
    /// The readiness probe waits, and a `/load` gets one retry.
    #[test]
    fn gateway_errors_from_the_relay_are_transient() {
        for code in [502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert!(is_gateway_error(status));
            assert!(is_transient(&AdminError::LoadRejected {
                status,
                body: String::new(),
            }));
        }
        assert!(!is_transient(&AdminError::LoadRejected {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: String::new(),
        }));
    }

    #[tokio::test]
    async fn push_without_a_proxy_is_an_error() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new()));
        let err = push_config(&host(), &ops, &serde_json::json!({}))
            .await
            .expect_err("no proxy");
        assert!(
            matches!(err, AdminError::ProxyNotRunning { .. }),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn new_proxy_without_a_relay_is_an_error() {
        let ops = fake_host(&[], false);
        let err = push_config(&host(), &ops, &serde_json::json!({}))
            .await
            .expect_err("no relay");
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
    }

    #[test]
    fn legacy_proxy_is_detected_by_its_published_admin_port() {
        let detail = |ports: &[&str]| ContainerDetail {
            ports: ports.iter().map(|p| (*p).to_string()).collect(),
            ..ContainerDetail::default()
        };
        assert!(is_legacy_proxy(&detail(&old_proxy_ports())));
        assert!(!is_legacy_proxy(&detail(&["80:80/tcp", "443:443/tcp"])));
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

    #[test]
    fn redact_passthrough_when_no_pem() {
        let body = r#"{"error":"unknown directive 'foo'"}"#;
        assert_eq!(redact_pem_blocks(body), body);
    }
}
