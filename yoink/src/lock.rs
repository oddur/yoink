//! Host-side advisory locking for `yoink up`. Two operators (or two CI
//! jobs) running `yoink up` against the same host at the same time
//! would race on the `spec_hash` → swap dance. This module wraps the
//! docker daemon as a serialization primitive: a sentinel container
//! per host that lives only as long as the operator is alive.
//!
//! ## How "alive" is detected
//!
//! The sentinel runs a tiny shell loop that checks `/tmp/heartbeat`
//! every few seconds and exits when the file is older than
//! `HEARTBEAT_STALE` (or doesn't exist). The yoink process spawns a
//! tokio task that touches `/tmp/heartbeat` via `docker exec` every
//! `HEARTBEAT_INTERVAL`. Three failure modes:
//!
//! - **Yoink finishes cleanly** → `release()` aborts the heartbeat
//!   task and force-removes the sentinel. Next deploy: sentinel
//!   doesn't exist → instant acquire.
//! - **Yoink crashes / SIGKILL / network drop** → the heartbeat task
//!   dies with the process. The sentinel notices the heartbeat is
//!   stale within `HEARTBEAT_STALE` seconds and exits. Next deploy:
//!   sentinel is *stopped*, treated as orphan, reaped + acquired.
//! - **Concurrent operator tries to acquire while we hold** → they
//!   see a *running* sentinel and bail with a clear message.
//!
//! Worst-case false-blocked window after a crash is `HEARTBEAT_STALE`
//! seconds (currently 30s). No infinite-deadlock failure mode.

use std::time::Duration;

use bollard::models::ContainerCreateBody;
use tokio::task::JoinHandle;

use crate::docker_ops::{DockerError, DockerOps, Host};

const LOCK_IMAGE_REPO: &str = "alpine";
const LOCK_IMAGE_TAG: &str = "3.20";
const LOCK_IMAGE: &str = "alpine:3.20";
pub const LOCK_NAME: &str = "yoink-deploy-lock";
const HEARTBEAT_FILE: &str = "/tmp/heartbeat";
/// Operator pings every 5s. Three pings before the sentinel decides
/// we're dead, which gives plenty of slack for a slow ssh roundtrip.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// After 30s without a fresh ping, the sentinel exits. Next deploy
/// reaps the stopped container as an orphan and acquires.
const HEARTBEAT_STALE_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
}

/// RAII-ish guard. Call [`HostLock::release`] explicitly when the deploy
/// finishes; the [`Drop`] impl aborts the heartbeat task on its way out
/// so that — even if `release()` is skipped — the sentinel exits within
/// `HEARTBEAT_STALE_SECS` and the next deploy can reap it.
pub struct HostLock {
    host: Host,
    heartbeat: Option<JoinHandle<()>>,
    released: bool,
}

impl HostLock {
    /// Acquire the lock for `host`. If a *running* sentinel already
    /// exists, returns an error so the caller can present a clear
    /// concurrent-deploy message. A stopped sentinel from a crashed
    /// previous holder is reaped and the lock is claimed.
    pub async fn acquire<O>(ops: &O, host: Host) -> Result<Self, LockError>
    where
        O: DockerOps + ?Sized,
    {
        let running =
            ops.list_running_containers(&host)
                .await
                .map_err(|source| LockError::Docker {
                    host: host.address.clone(),
                    source,
                })?;
        if running.iter().any(|c| c.name == LOCK_NAME) {
            return Err(LockError::Docker {
                host: host.address.clone(),
                source: DockerError::Invalid(format!(
                    "lock container {LOCK_NAME} is already running on {} \
                     (another deploy in progress)",
                    host.address
                )),
            });
        }
        // Either nothing exists or a stopped orphan from a crashed
        // deploy whose heartbeat task died. Sweep + start fresh.
        let _ = ops.force_remove_container(&host, LOCK_NAME).await;

        // Pull is idempotent — no-op when the image is already
        // cached. Daemons without alpine pre-pulled (laptops, fresh
        // hosts) would otherwise 404 on create_container below.
        ops.pull_image(&host, LOCK_IMAGE_REPO, LOCK_IMAGE_TAG, None)
            .await
            .map_err(|source| LockError::Docker {
                host: host.address.clone(),
                source,
            })?;

        let body = ContainerCreateBody {
            image: Some(LOCK_IMAGE.to_string()),
            cmd: Some(vec!["sh".into(), "-c".into(), heartbeat_watcher_script()]),
            ..Default::default()
        };
        ops.create_container(&host, LOCK_NAME, body)
            .await
            .map_err(|source| LockError::Docker {
                host: host.address.clone(),
                source,
            })?;
        ops.start_container(&host, LOCK_NAME)
            .await
            .map_err(|source| LockError::Docker {
                host: host.address.clone(),
                source,
            })?;

        // No external initial-touch — the script itself touches
        // `HEARTBEAT_FILE` as its first action (see
        // `heartbeat_watcher_script`). The previous design ran a
        // separate `exec_oneshot ["touch", …]` *after* `start_container`
        // and raced against the script's first `[ -f $FILE ]` check;
        // if the check lost, the loop exited immediately and every
        // subsequent yoink heartbeat hit 409 "container not running".

        Ok(Self {
            host,
            heartbeat: None,
            released: false,
        })
    }

    /// Spawn the heartbeat task on the caller's runtime. Separate
    /// method (vs done in `acquire`) so the trait-object bound on
    /// `acquire` doesn't have to be `Send + Sync + 'static` — the
    /// `Arc<dyn DockerOps>` we have in `cmd_up` *is* both, but
    /// requiring it on `acquire` forces every test to clone-into-Arc.
    pub fn spawn_heartbeat(&mut self, ops: std::sync::Arc<dyn DockerOps>) {
        let host = self.host.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            tick.tick().await; // first tick fires immediately; we did
            // an initial touch in acquire(), so
            // skip and wait for the next one.
            loop {
                tick.tick().await;
                if let Err(e) = ops
                    .exec_oneshot(
                        &host,
                        LOCK_NAME,
                        vec!["touch".into(), HEARTBEAT_FILE.into()],
                    )
                    .await
                {
                    // A single failure (transient ssh blip) is fine —
                    // the watcher tolerates HEARTBEAT_STALE seconds of
                    // missed pings. Log + keep trying.
                    tracing::warn!(host = %host.address, error = %e, "lock heartbeat exec failed");
                }
            }
        });
        self.heartbeat = Some(task);
    }

    /// Release the lock. Idempotent. Aborts the heartbeat task and
    /// **awaits its completion** before removing the sentinel —
    /// otherwise an in-flight `exec_oneshot` can race against the
    /// `force_remove_container` and surface as a 409 "container not
    /// running" WARN on the very last heartbeat tick.
    pub async fn release<O>(mut self, ops: &O)
    where
        O: DockerOps + ?Sized,
    {
        if self.released {
            return;
        }
        if let Some(task) = self.heartbeat.take() {
            task.abort();
            // Drain the cancellation so the task is guaranteed gone
            // before we touch the container. `JoinError` on a cancelled
            // task is the expected outcome — discard it.
            let _ = task.await;
        }
        let _ = ops.force_remove_container(&self.host, LOCK_NAME).await;
        self.released = true;
    }
}

impl Drop for HostLock {
    fn drop(&mut self) {
        if let Some(task) = self.heartbeat.take() {
            task.abort();
        }
        if !self.released {
            tracing::warn!(
                host = %self.host.address,
                name = LOCK_NAME,
                "host lock dropped without explicit release; sentinel will \
                 self-exit within {HEARTBEAT_STALE_SECS}s and the next deploy \
                 will reap it"
            );
        }
    }
}

/// Tiny `sh` script that runs as PID 1 in the sentinel container.
/// First action: `touch` the heartbeat file so the loop's first
/// `[ -f … ]` check is unambiguous (the previous design did the
/// initial touch from the operator side and raced against PID 1's
/// startup, leaving the container in `exited` state by the time
/// yoink's heartbeat task fired). Then polls every 5s; exits when
/// the file is missing or older than `HEARTBEAT_STALE_SECS`.
/// busybox `stat -c %Y` returns mtime as Unix epoch so the math is
/// portable across distros.
fn heartbeat_watcher_script() -> String {
    format!(
        "touch {HEARTBEAT_FILE}; \
         while [ -f {HEARTBEAT_FILE} ] && \
         [ $(($(date +%s) - $(stat -c %Y {HEARTBEAT_FILE}))) -lt {HEARTBEAT_STALE_SECS} ]; do \
           sleep 5; \
         done"
    )
}
