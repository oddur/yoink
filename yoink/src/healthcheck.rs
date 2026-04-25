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

/// Poll the new container's healthcheck endpoint until 200 or `budget`
/// elapses. Returns the number of attempts on success.
#[allow(clippy::too_many_arguments)]
pub async fn poll(
    ops: &dyn DockerOps,
    host: &Host,
    network: &str,
    container: &str,
    port: u16,
    path: &str,
    budget: Duration,
) -> Result<u32, HealthcheckError> {
    let deadline = Instant::now() + budget;
    let mut attempt: u32 = 0;
    let mut last_http_status: Option<u16>;
    loop {
        attempt += 1;
        let status = ops
            .healthcheck(host, network, container, port, path)
            .await?;
        if status == 200 {
            debug!(host = %host.address, container, attempt, "healthcheck 200");
            return Ok(attempt);
        }
        last_http_status = Some(status);
        debug!(
            host = %host.address,
            container,
            attempt,
            status,
            "healthcheck non-200"
        );
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
