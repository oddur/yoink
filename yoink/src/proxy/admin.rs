//! Push rendered Caddy JSON to the proxy's admin API.
//!
//! Caddy's admin API listens on `127.0.0.1:2019` *inside* the proxy
//! container ([`ADMIN_LISTEN`]). It is not published, and it does not
//! listen on any docker network, so no other container can reach it.
//! The admin API has no authentication, so this matters: a container
//! that reaches it can rewrite every route of the proxy. The yoink CLI:
//!
//! 1. Looks up the running proxy container.
//! 2. Runs `caddy reload --config /dev/stdin` in it with `docker exec`
//!    and sends the JSON on stdin. The config carries certificate
//!    keys, so it never goes on a command line.
//! 3. Reads the route count back with a curl probe container that
//!    shares the network namespace of the proxy. The probe brings its
//!    own curl, so the proxy image needs only `caddy` and `sh`.
//!
//! `docker exec` goes through the docker API connection that yoink
//! already has, so this needs no SSH tunnel and no published port.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

use crate::base::host::Host;
use crate::docker_ops::{DockerError, DockerOps};

use crate::config::{ADMIN_LISTEN, PROXY_SERVICE_NAME};

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
    /// `caddy reload` exited non-zero. `detail` is its output, which
    /// carries the admin API's answer (e.g. a Caddy parser error).
    #[error("proxy admin rejected /load: {detail}")]
    LoadRejected { detail: String },
    /// `caddy reload` could not connect to the admin API (for example,
    /// Caddy is still starting). Caddy did not see the config, so this
    /// is not a config error.
    #[error("proxy admin API not reachable: {detail}")]
    AdminUnreachable { detail: String },
    /// The route-count read-back after a successful `/load` failed.
    #[error("read back proxy routes: {detail}")]
    ReadBackFailed { detail: String },
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

impl AdminError {
    /// True for a failure that a second push can fix: the admin API
    /// did not accept connections, or the docker transport failed.
    fn is_transient(&self) -> bool {
        match self {
            Self::AdminUnreachable { .. } => true,
            Self::Docker { source, .. } => source.is_transport(),
            _ => false,
        }
    }
}

/// Pause before the second push. Long enough for a Caddy that is
/// still starting to bind the admin port.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Push `config` to the proxy on `host` with `docker exec` into the
/// running proxy container, then verify the route count.
///
/// The admin address is always [`ADMIN_LISTEN`]: config load rejects
/// an `admin` key in `proxy.config_extra`. A proxy that an
/// older yoink started listens on `0.0.0.0:2019`, which includes the
/// container loopback, so the first push to it also works. That push
/// carries the loopback-only admin block and moves the proxy onto it.
///
/// A transient failure ([`AdminError::is_transient`]) gets one more
/// push. That is safe even when the first reload ran: `caddy reload`
/// with the same config is idempotent. A Caddy error does not retry.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<(), AdminError> {
    let container = running_proxy(host, ops).await?;
    match push_once(host, ops, &container, config).await {
        Err(e) if e.is_transient() => {
            tracing::debug!(
                host = %host.address,
                error = %e,
                "transient proxy admin push error; retrying once",
            );
            tokio::time::sleep(RETRY_BACKOFF).await;
            push_once(host, ops, &container, config).await
        }
        other => other,
    }
}

async fn push_once(
    host: &Host,
    ops: &dyn DockerOps,
    container: &str,
    config: &Value,
) -> Result<(), AdminError> {
    let docker_err = |source| AdminError::Docker {
        host: host.address.clone(),
        source,
    };

    let body = serde_json::to_vec(config).expect("serde_json::Value always serializes");
    let load = ops
        .exec_with_stdin(
            host,
            container,
            vec![
                "caddy".into(),
                "reload".into(),
                "--config".into(),
                "/dev/stdin".into(),
                "--address".into(),
                ADMIN_LISTEN.into(),
            ],
            body,
        )
        .await
        .map_err(docker_err)?;
    if load.exit_code != 0 {
        // Caddy parser errors can echo a slice of the submitted JSON,
        // which carries inline cert+key PEMs from
        // `tls.certificates.load_pem`. Strip them before the error
        // lands in stderr / tracing / TUI toasts.
        let detail = redact_pem_blocks(exec_output(&load).trim());
        if detail.to_ascii_lowercase().contains("connection refused") {
            return Err(AdminError::AdminUnreachable { detail });
        }
        return Err(AdminError::LoadRejected { detail });
    }

    // Belt-and-braces: read the route count back from the daemon and
    // compare. `/load` is synchronous, so a success already implies the
    // config is live — but a future Caddy version that silently
    // drops routes (or any third-party admin-API shim) would otherwise
    // leave us claiming success while the proxy serves 5xx for those
    // domains. Verifying upfront beats discovering it from a Cloudflare
    // 521 alert.
    let expected = route_count(config);
    let actual = read_route_count(host, ops, container).await?;
    if expected != actual {
        return Err(AdminError::LoadVerifyMismatch { expected, actual });
    }
    Ok(())
}

async fn running_proxy(host: &Host, ops: &dyn DockerOps) -> Result<String, AdminError> {
    let label = format!("yoink.service={PROXY_SERVICE_NAME}");
    let containers = ops
        .list_containers_by_label(host, &label)
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    containers
        .into_iter()
        .find(crate::docker_ops::ContainerInfo::is_running)
        .map(|c| c.name)
        .ok_or_else(|| AdminError::ProxyNotRunning {
            host: host.address.clone(),
        })
}

/// stderr of an exec, or stdout when stderr is empty.
fn exec_output(r: &crate::docker_ops::OneShotResult) -> &str {
    if r.stderr.trim().is_empty() {
        &r.stdout
    } else {
        &r.stderr
    }
}

fn route_count(config: &Value) -> usize {
    config
        .pointer("/apps/http/servers/main/routes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

async fn read_route_count(
    host: &Host,
    ops: &dyn DockerOps,
    container: &str,
) -> Result<usize, AdminError> {
    // A curl probe in the network namespace of the proxy, not an exec
    // in the proxy: a custom proxy image does not always have an HTTP
    // client. `-w` appends the status code on its own line.
    let url = format!("http://{ADMIN_LISTEN}/config/apps/http/servers/main/routes/");
    let out = ops
        .curl_in_netns(
            host,
            container,
            vec![
                "-sS".into(),
                "--max-time".into(),
                "5".into(),
                "-w".into(),
                "\n%{http_code}".into(),
                url,
            ],
        )
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    let (body, status) = out.stdout.rsplit_once('\n').unwrap_or(("", &out.stdout));
    match status.trim() {
        // 404 = no routes object at that pointer (e.g. a config with no
        // routed services). Treat as zero rather than an error so the
        // verify step doesn't trip on legitimate empty configs.
        "404" => Ok(0),
        "200" => {
            let routes: Value =
                serde_json::from_str(body.trim()).map_err(|e| AdminError::ReadBackFailed {
                    detail: format!("parse routes JSON: {e}"),
                })?;
            Ok(routes.as_array().map_or(0, Vec::len))
        }
        status => Err(AdminError::ReadBackFailed {
            detail: redact_pem_blocks(&format!(
                "HTTP status {status:?}, curl exit {}: {}",
                out.exit_code,
                exec_output(&out).trim()
            )),
        }),
    }
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

    fn proxy_container() -> crate::docker_ops::ContainerInfo {
        crate::docker_ops::ContainerInfo {
            host: "h1".into(),
            name: "yoink-proxy-abc".into(),
            image: String::new(),
            state: "running".into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: Some(PROXY_SERVICE_NAME.into()),
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: std::collections::BTreeMap::new(),
        }
    }

    fn host() -> Host {
        Host {
            user: "root".into(),
            address: "h1".into(),
            port: None,
        }
    }

    fn exit(code: i64, stdout: &str, stderr: &str) -> crate::docker_ops::OneShotResult {
        crate::docker_ops::OneShotResult {
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// The push used to go through a published host port and an SSH
    /// tunnel, which needed the admin API on `0.0.0.0:2019`. It now runs
    /// `caddy reload` in the proxy container and sends the config on
    /// stdin, so the admin API can stay on the container loopback.
    #[tokio::test]
    async fn push_runs_caddy_reload_in_the_proxy_container() {
        use crate::docker_ops::{FakeDockerOps, RecordedCall};
        let config = serde_json::json!({
            "admin": { "listen": ADMIN_LISTEN },
            "apps": { "http": { "servers": { "main": { "routes": [{}, {}] } } } },
        });
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(0, "", "")));
        ops.push_curl_in_netns(Ok(exit(0, "[{},{}]\n\n200", "")));
        push_config(&host(), &ops, &config).await.unwrap();

        let calls = ops.calls();
        let (container, cmd, stdin) = calls
            .iter()
            .find_map(|c| match c {
                RecordedCall::ExecWithStdin(_, container, cmd, stdin) => {
                    Some((container, cmd, stdin))
                }
                _ => None,
            })
            .expect("caddy reload exec");
        assert_eq!(container, "yoink-proxy-abc");
        assert_eq!(
            cmd,
            &[
                "caddy",
                "reload",
                "--config",
                "/dev/stdin",
                "--address",
                "127.0.0.1:2019"
            ]
        );
        assert_eq!(serde_json::from_slice::<Value>(stdin).unwrap(), config);
        // The read-back runs in a curl probe in the network namespace
        // of the proxy, not with `wget` in the proxy image.
        assert!(calls.iter().any(|c| matches!(
            c,
            RecordedCall::CurlInNetns(_, container, args)
                if container == "yoink-proxy-abc"
                    && args.last().map(String::as_str)
                        == Some("http://127.0.0.1:2019/config/apps/http/servers/main/routes/")
        )));
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, RecordedCall::ExecOneshot(..))),
            "got {calls:?}"
        );
    }

    #[tokio::test]
    async fn push_reports_a_rejected_load_without_pem() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(
            1,
            "",
            "Error: caddy responded with error: HTTP 400: -----BEGIN PRIVATE KEY-----\nMIIabc\n-----END PRIVATE KEY----- bad",
        )));
        let err = push_config(&host(), &ops, &config).await.unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, AdminError::LoadRejected { .. }), "got {msg}");
        assert!(msg.contains("HTTP 400"), "got {msg}");
        assert!(!msg.contains("MIIabc"), "got {msg}");
    }

    #[tokio::test]
    async fn push_treats_a_404_route_read_back_as_zero_routes() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(0, "", "")));
        ops.push_curl_in_netns(Ok(exit(0, "{\"error\":\"unknown path\"}\n404", "")));
        push_config(&host(), &ops, &config).await.unwrap();
    }

    const REFUSED: &str = "Error: sending configuration to instance: performing request: \
        Post \"http://127.0.0.1:2019/load\": dial tcp 127.0.0.1:2019: connect: connection refused";

    /// The old HTTP push retried once on a transport error. The exec
    /// push did not, and it reported a Caddy that was not listening yet
    /// as a rejected config.
    #[tokio::test]
    async fn push_retries_once_when_the_admin_api_refuses_the_connection() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(1, "", REFUSED)));
        ops.push_exec_with_stdin(Ok(exit(0, "", "")));
        ops.push_curl_in_netns(Ok(exit(0, "{}\n404", "")));
        push_config(&host(), &ops, &config).await.unwrap();
        let reloads = ops
            .calls()
            .iter()
            .filter(|c| matches!(c, crate::docker_ops::RecordedCall::ExecWithStdin(..)))
            .count();
        assert_eq!(reloads, 2);
    }

    #[tokio::test]
    async fn push_reports_a_refused_connection_as_unreachable_not_rejected() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(1, "", REFUSED)));
        ops.push_exec_with_stdin(Ok(exit(1, "", REFUSED)));
        let err = push_config(&host(), &ops, &config).await.unwrap_err();
        assert!(
            matches!(err, AdminError::AdminUnreachable { .. }),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn push_retries_once_on_a_docker_transport_error() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Err(DockerError::Bollard {
            host: "h1".into(),
            source: bollard::errors::Error::IOError {
                err: std::io::Error::from(std::io::ErrorKind::ConnectionReset),
            },
        }));
        ops.push_exec_with_stdin(Ok(exit(0, "", "")));
        ops.push_curl_in_netns(Ok(exit(0, "{}\n404", "")));
        push_config(&host(), &ops, &config).await.unwrap();
    }

    #[tokio::test]
    async fn push_does_not_retry_a_rejected_config() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(1, "", "Error: HTTP 400: unknown module")));
        let err = push_config(&host(), &ops, &config).await.unwrap_err();
        assert!(matches!(err, AdminError::LoadRejected { .. }), "got {err}");
    }

    #[tokio::test]
    async fn push_reports_a_failed_route_read_back() {
        let config = serde_json::json!({ "admin": { "listen": ADMIN_LISTEN } });
        let ops = crate::docker_ops::FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container()]));
        ops.push_exec_with_stdin(Ok(exit(0, "", "")));
        ops.push_curl_in_netns(Ok(exit(
            7,
            "\n000",
            "curl: (7) Failed to connect to 127.0.0.1 port 2019",
        )));
        let err = push_config(&host(), &ops, &config).await.unwrap_err();
        assert!(
            matches!(err, AdminError::ReadBackFailed { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("Failed to connect"), "got {err}");
    }
}
