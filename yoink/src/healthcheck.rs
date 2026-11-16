//! HTTP healthcheck polling. Issues `DockerOps::healthcheck` (a one-shot
//! curl container against the container's name on the shared network)
//! until it returns 200 or the budget runs out. Backoff matches Kamal's
//! poller: `sleep min(attempt, time_left)`.

use std::time::Duration;

use thiserror::Error;
use tokio::time::{Instant, sleep};
use tracing::debug;

use crate::docker_ops::{DockerError, DockerOps, Host};

#[derive(Debug, Error)]
pub enum HealthcheckError {
    #[error("docker error: {0}")]
    Docker(#[from] DockerError),
    #[error(
        "healthcheck timed out after {budget:?} ({attempts} attempts; last status: {last_http_status:?}{})",
        fmt_last_error(.last_error.as_deref())
    )]
    TimedOut {
        budget: Duration,
        attempts: u32,
        last_http_status: Option<u16>,
        /// Most recent probe error seen during the budget, if any.
        /// A probe error is retried like a non-200, so this is the
        /// only place it surfaces.
        last_error: Option<String>,
    },
}

fn fmt_last_error(last_error: Option<&str>) -> String {
    last_error.map_or_else(String::new, |e| format!("; last probe error: {e}"))
}

/// Outcome of one probe attempt: either healthy (caller stops polling)
/// or not-yet, with an optional HTTP status to surface in the timeout
/// error message. The TCP probe always reports `None` since there's no
/// HTTP code to report. `error` carries a failed probe's error (ssh
/// blip, docker API error) so the timeout message can name it.
enum AttemptOutcome {
    Healthy,
    NotYet {
        last_http_status: Option<u16>,
        error: Option<String>,
    },
}

/// Generic backoff loop shared by `poll` (HTTP) and `poll_tcp` (TCP).
/// Calls `attempt_fn` repeatedly until it returns `Healthy` or `budget`
/// elapses. Backoff is `min(attempt_secs, time_left)` — same shape
/// Kamal's poller uses.
async fn poll_until<F, Fut>(budget: Duration, mut attempt_fn: F) -> Result<u32, HealthcheckError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<AttemptOutcome, DockerError>>,
{
    let deadline = Instant::now() + budget;
    let mut attempt: u32 = 0;
    // `last_http_status` is overwritten by the first `NotYet` arm
    // before any read; the initial `None` is just to satisfy the
    // borrow checker on the timeout branch.
    #[allow(unused_assignments)]
    let mut last_http_status: Option<u16> = None;
    let mut last_error: Option<String> = None;
    loop {
        attempt += 1;
        match attempt_fn(attempt).await? {
            AttemptOutcome::Healthy => return Ok(attempt),
            AttemptOutcome::NotYet {
                last_http_status: s,
                error,
            } => {
                last_http_status = s;
                if error.is_some() {
                    last_error = error;
                }
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HealthcheckError::TimedOut {
                budget,
                attempts: attempt,
                last_http_status,
                last_error,
            });
        }
        let remaining = deadline - now;
        let backoff = Duration::from_secs(u64::from(attempt)).min(remaining);
        sleep(backoff).await;
    }
}

/// Poll a TCP-connect probe until it succeeds or `budget` elapses.
/// Used for services that don't expose a useful HTTP endpoint —
/// redis (line-protocol on :6379), caddy (TLS on :443), and so on.
/// "Healthy" here means "the new container is accepting TCP
/// connections" — same proxy for "the process is up + listening".
pub async fn poll_tcp(
    ops: &dyn DockerOps,
    host: &Host,
    network: &str,
    container: &str,
    port: u16,
    budget: Duration,
) -> Result<u32, HealthcheckError> {
    poll_until(budget, |attempt| async move {
        match ops.healthcheck_tcp(host, network, container, port).await {
            Ok(()) => {
                debug!(host = %host.address, container, attempt, "tcp probe ok");
                Ok(AttemptOutcome::Healthy)
            }
            Err(e) => {
                debug!(host = %host.address, container, attempt, error = %e, "tcp probe failed");
                Ok(AttemptOutcome::NotYet {
                    last_http_status: None,
                    error: Some(e.to_string()),
                })
            }
        }
    })
    .await
}

/// Poll the new container's HTTP healthcheck endpoint until 200 or
/// `budget` elapses. Returns the number of attempts on success.
///
/// A probe that errors (ssh transport blip, daemon 5xx on the one-shot
/// curl container) counts as not-yet, same as `poll_tcp`: one bad
/// probe must not fail a deploy whose app would answer 200 on the
/// next attempt. The last error is kept for the timeout message.
///
/// The exception is a request the daemon rejected with a 4xx (the
/// probe network or the probe image does not exist). The next attempt
/// gets the same answer, so that error ends the poll at once instead
/// of after the whole budget.
pub async fn poll(
    ops: &dyn DockerOps,
    host: &Host,
    network: &str,
    container: &str,
    port: u16,
    path: &str,
    budget: Duration,
) -> Result<u32, HealthcheckError> {
    poll_until(budget, |attempt| async move {
        match ops.healthcheck(host, network, container, port, path).await {
            Ok(200) => {
                debug!(host = %host.address, container, attempt, "healthcheck 200");
                Ok(AttemptOutcome::Healthy)
            }
            Ok(status) => {
                debug!(host = %host.address, container, attempt, status, "healthcheck non-200");
                Ok(AttemptOutcome::NotYet {
                    last_http_status: Some(status),
                    error: None,
                })
            }
            Err(e) if e.is_daemon_rejection() => Err(e),
            Err(e) => {
                debug!(host = %host.address, container, attempt, error = %e, "healthcheck probe failed");
                Ok(AttemptOutcome::NotYet {
                    last_http_status: None,
                    error: Some(e.to_string()),
                })
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn returns_ok_on_immediate_200() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Ok(200));
        let attempts = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn polls_again_then_succeeds() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Ok(503));
        ops.push_healthcheck(Ok(503));
        ops.push_healthcheck(Ok(200));
        let attempts = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(attempts, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_when_never_healthy() {
        let ops = FakeDockerOps::new();
        for _ in 0..50 {
            ops.push_healthcheck(Ok(503));
        }
        let err = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(3),
        )
        .await
        .unwrap_err();
        match err {
            HealthcheckError::TimedOut {
                budget,
                attempts,
                last_http_status,
                last_error,
            } => {
                assert_eq!(budget, Duration::from_secs(3));
                assert!(attempts >= 2, "got {attempts}");
                assert_eq!(last_http_status, Some(503));
                assert_eq!(last_error, None);
            }
            HealthcheckError::Docker(e) => panic!("expected timeout, got {e:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn tcp_returns_ok_on_immediate_connect() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck_tcp(Ok(()));
        let attempts = poll_tcp(&ops, &host(), "yoink", "c", 6379, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn tcp_retries_then_succeeds() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck_tcp(Err(crate::docker_ops::DockerError::Invalid("nope".into())));
        ops.push_healthcheck_tcp(Err(crate::docker_ops::DockerError::Invalid("nope".into())));
        ops.push_healthcheck_tcp(Ok(()));
        let attempts = poll_tcp(&ops, &host(), "yoink", "c", 6379, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(attempts, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn tcp_times_out_when_never_reachable() {
        let ops = FakeDockerOps::new();
        for _ in 0..50 {
            ops.push_healthcheck_tcp(Err(crate::docker_ops::DockerError::Invalid(
                "refused".into(),
            )));
        }
        let err = poll_tcp(&ops, &host(), "yoink", "c", 6379, Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(matches!(err, HealthcheckError::TimedOut { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_after_probe_error_then_succeeds() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Err(DockerError::Invalid("ssh blip".into())));
        ops.push_healthcheck(Ok(200));
        let attempts = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(attempts, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_rejection_fails_at_once() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Err(DockerError::Bollard {
            host: "host-a".into(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message: "network yoink not found".into(),
            },
        }));
        // A second attempt would get this 200. The poll must not make it.
        ops.push_healthcheck(Ok(200));
        let err = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(60),
        )
        .await
        .unwrap_err();
        match err {
            HealthcheckError::Docker(e) => {
                assert!(e.to_string().contains("network yoink not found"), "got {e}");
            }
            HealthcheckError::TimedOut { .. } => panic!("expected docker error, got {err:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_server_error_is_retried() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Err(DockerError::Bollard {
            host: "host-a".into(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code: 500,
                message: "daemon busy".into(),
            },
        }));
        ops.push_healthcheck(Ok(200));
        let attempts = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(attempts, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_with_last_probe_error_in_message() {
        let ops = FakeDockerOps::new();
        ops.push_healthcheck(Ok(503));
        for _ in 0..50 {
            ops.push_healthcheck(Err(DockerError::Invalid("ssh blip".into())));
        }
        let err = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(3),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        match err {
            HealthcheckError::TimedOut {
                attempts,
                last_http_status,
                last_error,
                ..
            } => {
                assert!(attempts >= 2, "got {attempts}");
                assert_eq!(last_http_status, None);
                let last_error = last_error.expect("probe error kept");
                assert!(last_error.contains("ssh blip"), "got {last_error}");
                assert!(message.contains("last probe error"), "got {message}");
                assert!(message.contains("ssh blip"), "got {message}");
            }
            HealthcheckError::Docker(e) => panic!("expected timeout, got {e:?}"),
        }
    }
}
