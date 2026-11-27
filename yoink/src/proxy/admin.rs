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
//! 3. Reads the route count back with `wget` in the same container.
//!
//! `docker exec` goes through the docker API connection that yoink
//! already has, so this needs no SSH tunnel and no published port.

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

/// Push `config` to the proxy on `host` with `docker exec` into the
/// running proxy container, then verify the route count.
///
/// The admin address is always [`ADMIN_LISTEN`]: config load rejects
/// an `admin` key in `proxy.config_extra`. A proxy that an
/// older yoink started listens on `0.0.0.0:2019`, which includes the
/// container loopback, so the first push to it also works. That push
/// carries the loopback-only admin block and moves the proxy onto it.
pub async fn push_config(
    host: &Host,
    ops: &dyn DockerOps,
    config: &Value,
) -> Result<(), AdminError> {
    let container = running_proxy(host, ops).await?;
    let docker_err = |source| AdminError::Docker {
        host: host.address.clone(),
        source,
    };

    let body = serde_json::to_vec(config).expect("serde_json::Value always serializes");
    let load = ops
        .exec_with_stdin(
            host,
            &container,
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
        return Err(AdminError::LoadRejected {
            detail: redact_pem_blocks(exec_output(&load).trim()),
        });
    }

    // Belt-and-braces: read the route count back from the daemon and
    // compare. `/load` is synchronous, so a success already implies the
    // config is live — but a future Caddy version that silently
    // drops routes (or any third-party admin-API shim) would otherwise
    // leave us claiming success while the proxy serves 5xx for those
    // domains. Verifying upfront beats discovering it from a Cloudflare
    // 521 alert.
    let expected = route_count(config);
    let actual = read_route_count(host, ops, &container).await?;
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
    // BusyBox `wget` ships in the `caddy:2` image (the docker
    // HEALTHCHECK uses it too).
    let url = format!("http://{ADMIN_LISTEN}/config/apps/http/servers/main/routes/");
    let out = ops
        .exec_oneshot(
            host,
            container,
            vec!["wget".into(), "-q".into(), "-O".into(), "-".into(), url],
        )
        .await
        .map_err(|source| AdminError::Docker {
            host: host.address.clone(),
            source,
        })?;
    if out.exit_code != 0 {
        // 404 = no routes object at that pointer (e.g. a config with no
        // routed services). Treat as zero rather than an error so the
        // verify step doesn't trip on legitimate empty configs.
        let detail = exec_output(&out).trim();
        if detail.contains("404") {
            return Ok(0);
        }
        return Err(AdminError::ReadBackFailed {
            detail: redact_pem_blocks(detail),
        });
    }
    let routes: Value =
        serde_json::from_str(out.stdout.trim()).map_err(|e| AdminError::ReadBackFailed {
            detail: format!("parse routes JSON: {e}"),
        })?;
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
        ops.push_exec_oneshot(Ok(exit(0, "[{},{}]\n", "")));
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
        assert!(calls.iter().any(|c| matches!(
            c,
            RecordedCall::ExecOneshot(_, _, cmd)
                if cmd.last().map(String::as_str)
                    == Some("http://127.0.0.1:2019/config/apps/http/servers/main/routes/")
        )));
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
        ops.push_exec_oneshot(Ok(exit(
            1,
            "",
            "wget: server returned error: HTTP/1.1 404 Not Found",
        )));
        push_config(&host(), &ops, &config).await.unwrap();
    }
}
