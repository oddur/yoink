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
        "healthcheck timed out after {budget:?} ({attempts} attempts; last status: {last_http_status:?})"
    )]
    TimedOut {
        budget: Duration,
        attempts: u32,
        last_http_status: Option<u16>,
    },
}

/// Outcome of one probe attempt: either healthy (caller stops polling)
/// or not-yet, with an optional HTTP status to surface in the timeout
/// error message. The TCP probe always reports `None` since there's no
/// HTTP code to report.
enum AttemptOutcome {
    Healthy,
    NotYet { last_http_status: Option<u16> },
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
    loop {
        attempt += 1;
        match attempt_fn(attempt).await? {
            AttemptOutcome::Healthy => return Ok(attempt),
            AttemptOutcome::NotYet {
                last_http_status: s,
            } => last_http_status = s,
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(HealthcheckError::TimedOut {
                budget,
                attempts: attempt,
                last_http_status,
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
                })
            }
        }
    })
    .await
}

/// Poll the new container's HTTP healthcheck endpoint until 200 or
/// `budget` elapses. Returns the number of attempts on success.
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
        let status = ops
            .healthcheck(host, network, container, port, path)
            .await?;
        if status == 200 {
            debug!(host = %host.address, container, attempt, "healthcheck 200");
            Ok(AttemptOutcome::Healthy)
        } else {
            debug!(host = %host.address, container, attempt, status, "healthcheck non-200");
            Ok(AttemptOutcome::NotYet {
                last_http_status: Some(status),
            })
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
            } => {
                assert_eq!(budget, Duration::from_secs(3));
                assert!(attempts >= 2, "got {attempts}");
                assert_eq!(last_http_status, Some(503));
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
    async fn propagates_docker_error() {
        let ops = FakeDockerOps::new();
        let err = poll(
            &ops,
            &host(),
            "yoink",
            "c",
            3000,
            "/h",
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HealthcheckError::Docker(_)));
    }
}
