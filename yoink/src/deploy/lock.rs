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
//!   task and force-removes its own sentinel (by id). Next deploy: sentinel
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

use std::sync::Arc;
use std::time::Duration;

use bollard::models::ContainerCreateBody;
use tokio::task::JoinHandle;

use crate::base::host::Host;
use crate::docker_ops::{DockerError, DockerOps, StoppedRemoval};

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
    /// Id of the sentinel *this* holder created. Heartbeat and release
    /// address the sentinel by id, never by name: if our sentinel
    /// exited and another operator reaped it, the name points at
    /// their live lock.
    id: String,
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
        let docker_err = |source| LockError::Docker {
            host: host.address.clone(),
            source,
        };
        // A 409 from the daemon means another operator has the lock.
        let held_or_err = |e: DockerError| {
            if is_conflict(&e) {
                already_held(&host)
            } else {
                docker_err(e)
            }
        };

        // Daemons without alpine pre-pulled (laptops, fresh hosts)
        // would otherwise 404 on create_container below. Pull only
        // when the image is missing: a pull contacts the registry
        // even for a cached tag, and the lock must not depend on
        // Docker Hub being reachable.
        if !ops
            .image_present(&host, LOCK_IMAGE_REPO, LOCK_IMAGE_TAG)
            .await
            .map_err(docker_err)?
        {
            ops.pull_image(&host, LOCK_IMAGE_REPO, LOCK_IMAGE_TAG, None)
                .await
                .map_err(docker_err)?;
        }

        // The daemon's name-uniqueness check on create is the atomic
        // step: of N concurrent operators exactly one create succeeds.
        // Never remove an existing sentinel before trying, and never
        // force-remove one; that would delete a concurrent holder's
        // live lock.
        let id = match ops.create_container(&host, LOCK_NAME, lock_body()).await {
            Ok(id) => id,
            Err(e) if is_conflict(&e) => {
                let running = ops
                    .list_running_containers(&host)
                    .await
                    .map_err(docker_err)?;
                if running.iter().any(|c| c.name == LOCK_NAME) {
                    return Err(already_held(&host));
                }
                // A stopped orphan from a crashed deploy whose
                // heartbeat task died. The non-force remove fails
                // with a 409 if another operator started a sentinel
                // since the list above, so it cannot reap a live lock.
                if ops
                    .remove_stopped_container(&host, LOCK_NAME)
                    .await
                    .map_err(held_or_err)?
                    == StoppedRemoval::Running
                {
                    return Err(already_held(&host));
                }
                // A 409 here: another operator reaped the orphan too
                // and won the create.
                ops.create_container(&host, LOCK_NAME, lock_body())
                    .await
                    .map_err(held_or_err)?
            }
            Err(e) => return Err(docker_err(e)),
        };
        // Start by id, not by name. If a concurrent operator took our
        // created-but-not-started container for an orphan and replaced
        // it, the name now points at *their* sentinel; starting by id
        // fails instead of silently sharing their lock: a 404 once
        // their remove finished, a 409 while it is in progress. Either
        // way they hold the lock now.
        ops.start_container(&host, &id).await.map_err(|e| {
            if is_not_found(&e) {
                already_held(&host)
            } else {
                held_or_err(e)
            }
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
            id,
            heartbeat: None,
            released: false,
        })
    }

    /// Spawn the heartbeat task on the caller's runtime. [`acquire_all`]
    /// calls it as soon as the lock is acquired. Separate method (vs
    /// done in `acquire`) so the trait-object bound on `acquire`
    /// doesn't have to be `Send + Sync + 'static`, which would force
    /// every test to clone-into-Arc.
    fn spawn_heartbeat(&mut self, ops: Arc<dyn DockerOps>) {
        let host = self.host.clone();
        let id = self.id.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            // The first tick fires immediately. The sentinel script
            // touched the file itself on start, so skip that tick.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = ops
                    .exec_oneshot(&host, &id, vec!["touch".into(), HEARTBEAT_FILE.into()])
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
        // By id: after missed heartbeats our sentinel may have been
        // reaped and replaced, and removing by name would delete the
        // new holder's live lock. A missing id is success.
        let _ = ops.force_remove_container(&self.host, &self.id).await;
        self.released = true;
    }
}

/// Acquire the lock on every host in `hosts`, concurrently. Each lock
/// starts its heartbeat as soon as it is acquired, so it cannot expire
/// while a slow host still pulls the sentinel image.
///
/// All attempts run to completion. At the first failure, the locks
/// already acquired start their release (heartbeat stopped, sentinel
/// removed) at once, and each lock acquired after it is released as
/// it arrives. Every release ends before the error returns, so an
/// immediate retry does not find a lock still held. The error carries
/// the host that failed first. `on_failure` gets that first failure at
/// once when other attempts are still running, so the caller can show
/// it while it waits.
pub async fn acquire_all(
    ops: &Arc<dyn DockerOps>,
    hosts: Vec<Host>,
    mut on_failure: impl FnMut(&Host, &LockError),
) -> Result<Vec<HostLock>, (Host, LockError)> {
    use futures_util::StreamExt as _;
    let mut pending: futures_util::stream::FuturesUnordered<_> = hosts
        .into_iter()
        .map(|host| async move {
            let result = HostLock::acquire(&**ops, host.clone()).await;
            (host, result)
        })
        .collect();
    // A spawned release runs while the loop waits on the slow hosts.
    let release = |lock: HostLock| {
        let ops = ops.clone();
        tokio::spawn(async move { lock.release(&*ops).await })
    };
    let mut locks = Vec::new();
    let mut releases = Vec::new();
    let mut first_error = None;
    while let Some((host, result)) = pending.next().await {
        match result {
            Ok(lock) if first_error.is_some() => releases.push(release(lock)),
            Ok(mut lock) => {
                lock.spawn_heartbeat(ops.clone());
                locks.push(lock);
            }
            Err(e) if first_error.is_none() => {
                if !pending.is_empty() {
                    on_failure(&host, &e);
                }
                releases.extend(locks.drain(..).map(release));
                first_error = Some((host, e));
            }
            Err(_) => {}
        }
    }
    match first_error {
        None => Ok(locks),
        Some(err) => {
            futures_util::future::join_all(releases).await;
            Err(err)
        }
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

fn lock_body() -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(LOCK_IMAGE.to_string()),
        cmd: Some(vec!["sh".into(), "-c".into(), heartbeat_watcher_script()]),
        ..Default::default()
    }
}

fn already_held(host: &Host) -> LockError {
    LockError::Docker {
        host: host.address.clone(),
        source: DockerError::Invalid(format!(
            "lock container {LOCK_NAME} is already running on {} \
             (another deploy in progress)",
            host.address
        )),
    }
}

/// `true` for the daemon's 409: a name conflict on create, or a
/// non-force remove of a running container.
fn is_conflict(e: &DockerError) -> bool {
    daemon_status(e) == Some(409)
}

/// `true` for the daemon's 404: the container id no longer exists.
fn is_not_found(e: &DockerError) -> bool {
    daemon_status(e) == Some(404)
}

fn daemon_status(e: &DockerError) -> Option<u16> {
    match e {
        DockerError::Bollard {
            source: bollard::errors::Error::DockerResponseServerError { status_code, .. },
            ..
        } => Some(*status_code),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{ContainerInfo, ContainerState, FakeDockerOps, RecordedCall};

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "10.0.0.1".into(),
            port: None,
        }
    }

    fn conflict() -> DockerError {
        daemon_error(409)
    }

    fn daemon_error(status_code: u16) -> DockerError {
        DockerError::Bollard {
            host: "10.0.0.1".into(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code,
                message: "scripted".into(),
            },
        }
    }

    fn sentinel() -> ContainerInfo {
        ContainerInfo {
            host: "10.0.0.1".into(),
            name: LOCK_NAME.into(),
            image: LOCK_IMAGE.into(),
            state: ContainerState::Running,
            status_text: "Up 2 seconds".into(),
            created_unix: None,
            yoink_service: None,
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: std::collections::BTreeMap::new(),
        }
    }

    fn removals(ops: &FakeDockerOps) -> Vec<RecordedCall> {
        ops.calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    RecordedCall::ForceRemoveContainer(..)
                        | RecordedCall::RemoveStoppedContainer(..)
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn acquire_on_clean_host_creates_and_starts_by_id() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));

        let lock = HostLock::acquire(&ops, host()).await.expect("acquire");

        assert_eq!(
            ops.calls(),
            vec![
                RecordedCall::CreateContainer(host(), LOCK_NAME.into()),
                RecordedCall::StartContainer(host(), "abc123".into()),
            ]
        );
        lock.release(&ops).await;
    }

    /// Regression: when the acquire failed on one host, the locks
    /// already acquired on the other hosts were dropped without
    /// `release()`. They stayed held for ~30s and a retry failed.
    #[tokio::test]
    async fn acquire_all_releases_acquired_locks_when_one_host_fails() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));
        ops.push_create_container(Err(daemon_error(500)));
        ops.push_force_remove(Ok(()));
        let other = Host {
            user: "deploy".into(),
            address: "10.0.0.2".into(),
            port: None,
        };

        let dyn_ops: Arc<dyn DockerOps> = ops.clone();

        let (failed, _) = acquire_all(&dyn_ops, vec![host(), other.clone()], |_, _| {})
            .await
            .err()
            .expect("second host fails");

        assert_eq!(failed, other);
        assert_eq!(
            removals(&ops),
            vec![RecordedCall::ForceRemoveContainer(host(), "abc123".into())]
        );
    }

    fn slow_host() -> Host {
        Host {
            user: "deploy".into(),
            address: "10.0.0.9".into(),
            port: None,
        }
    }

    fn creates_on(ops: &FakeDockerOps, host: &Host) -> usize {
        ops.calls()
            .iter()
            .filter(|c| matches!(c, RecordedCall::CreateContainer(h, _) if h == host))
            .count()
    }

    /// Regression: `acquire_all` waited for every host before it
    /// reported a failure, so `yoink up` sat silent on an error it
    /// already had. The first failure now reaches `on_failure` while
    /// the slow host still pulls, and the slow host's lock is still
    /// released before the error returns.
    #[tokio::test(start_paused = true)]
    async fn acquire_all_reports_the_first_failure_while_others_still_run() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.delay_pulls_on(&slow_host().address, Duration::from_secs(60));
        ops.push_pull_image(Err(daemon_error(500)));
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("slow-id".into()));
        ops.push_start_container(Ok(()));
        ops.push_force_remove(Ok(()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();

        let mut reported = Vec::new();
        let (failed, _) = acquire_all(&dyn_ops, vec![host(), slow_host()], |h, _| {
            reported.push((h.clone(), creates_on(&ops, &slow_host())));
        })
        .await
        .err()
        .expect("first host fails");

        assert_eq!(failed, host());
        assert_eq!(
            reported,
            vec![(host(), 0)],
            "reported before the slow host got its lock"
        );
        assert_eq!(
            removals(&ops),
            vec![RecordedCall::ForceRemoveContainer(
                slow_host(),
                "slow-id".into()
            )]
        );
    }

    /// Regression: heartbeats started only after `acquire_all`
    /// returned. While a slow host pulled the sentinel image for more
    /// than 30s, the lock on a fast host expired, and the deploy ran
    /// there unlocked. Each lock now beats from the moment it is
    /// acquired.
    #[tokio::test(start_paused = true)]
    async fn acquire_all_starts_each_heartbeat_as_soon_as_its_lock_is_held() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.delay_pulls_on(&slow_host().address, Duration::from_secs(60));
        ops.push_pull_image(Ok(()));
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("fast-id".into()));
        ops.push_start_container(Ok(()));
        ops.push_create_container(Ok("slow-id".into()));
        ops.push_start_container(Ok(()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();

        let locks = acquire_all(&dyn_ops, vec![host(), slow_host()], |_, _| {})
            .await
            .expect("both acquire");

        let calls = ops.calls();
        let slow_create = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::CreateContainer(h, _) if *h == slow_host()))
            .expect("slow host creates its sentinel");
        let touch = RecordedCall::ExecOneshot(
            host(),
            "fast-id".into(),
            vec!["touch".into(), HEARTBEAT_FILE.into()],
        );
        let beats_before = calls[..slow_create].iter().filter(|c| **c == touch).count();
        // 60s of pull at one beat per 5s, minus the skipped first tick.
        assert!(
            beats_before >= 10,
            "{beats_before} beats while the slow host pulled"
        );
        for lock in locks {
            lock.release(&*ops).await;
        }
    }

    /// Regression: after the first failure, the locks already held
    /// stayed held until the slowest attempt ended, although the
    /// callers say they release them at once. The fast lock is now
    /// removed while the slow host still pulls, and the slow lock is
    /// released when it arrives.
    #[tokio::test(start_paused = true)]
    async fn acquire_all_releases_held_locks_at_the_first_failure() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.delay_pulls_on(&slow_host().address, Duration::from_secs(60));
        let failing = Host {
            user: "deploy".into(),
            address: "10.0.0.2".into(),
            port: None,
        };
        ops.push_pull_image(Ok(()));
        ops.push_pull_image(Err(daemon_error(500)));
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("fast-id".into()));
        ops.push_start_container(Ok(()));
        ops.push_create_container(Ok("slow-id".into()));
        ops.push_start_container(Ok(()));
        ops.push_force_remove(Ok(()));
        ops.push_force_remove(Ok(()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();

        let (failed, _) = acquire_all(
            &dyn_ops,
            vec![host(), failing.clone(), slow_host()],
            |_, _| {},
        )
        .await
        .err()
        .expect("second host fails");
        assert_eq!(failed, failing);

        let calls = ops.calls();
        let fast_removed = calls
            .iter()
            .position(|c| *c == RecordedCall::ForceRemoveContainer(host(), "fast-id".into()))
            .expect("fast lock released");
        let slow_create = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::CreateContainer(h, _) if *h == slow_host()))
            .expect("slow host creates its sentinel");
        assert!(
            fast_removed < slow_create,
            "the fast lock was released before the slow host got its lock"
        );
        assert_eq!(
            removals(&ops),
            vec![
                RecordedCall::ForceRemoveContainer(host(), "fast-id".into()),
                RecordedCall::ForceRemoveContainer(slow_host(), "slow-id".into()),
            ]
        );
    }

    /// The release-on-failure path stops the heartbeat of each lock it
    /// releases: no touch reaches the sentinel after its removal.
    #[tokio::test(start_paused = true)]
    async fn acquire_all_stops_the_heartbeats_of_the_locks_it_releases() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.delay_pulls_on(&slow_host().address, Duration::from_secs(60));
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("fast-id".into()));
        ops.push_start_container(Ok(()));
        ops.push_pull_image(Err(daemon_error(500)));
        ops.push_force_remove(Ok(()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();

        let (failed, _) = acquire_all(&dyn_ops, vec![host(), slow_host()], |_, _| {})
            .await
            .err()
            .expect("slow host fails");
        assert_eq!(failed, slow_host());
        tokio::time::sleep(Duration::from_secs(60)).await;

        let calls = ops.calls();
        let removed = calls
            .iter()
            .position(|c| *c == RecordedCall::ForceRemoveContainer(host(), "fast-id".into()))
            .expect("fast lock released");
        assert!(
            calls[..removed]
                .iter()
                .any(|c| matches!(c, RecordedCall::ExecOneshot(..))),
            "the fast lock beat while the slow host pulled"
        );
        assert!(
            !calls[removed..]
                .iter()
                .any(|c| matches!(c, RecordedCall::ExecOneshot(..))),
            "no beat after the release"
        );
    }

    /// Operator B loses the create to operator A. B must bail and must
    /// not remove A's sentinel.
    #[tokio::test]
    async fn acquire_never_removes_a_running_sentinel() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Err(conflict()));
        ops.push_list_containers(Ok(vec![sentinel()]));

        let err = HostLock::acquire(&ops, host()).await.err().expect("held");

        assert!(err.to_string().contains("another deploy in progress"));
        assert_eq!(removals(&ops), vec![]);
    }

    /// The race from the bug report: B's "is it running" list comes
    /// back empty because A has not started its sentinel yet. B must
    /// only attempt the non-force remove, and bail when the daemon
    /// refuses because A's sentinel is running by then.
    #[tokio::test]
    async fn acquire_does_not_force_remove_sentinel_started_after_the_check() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_list_containers(Ok(vec![]));
        ops.push_create_container(Err(conflict()));
        // The daemon refuses the non-force remove of a running
        // container; `remove_stopped_container` reports that as
        // `Running` and leaves the container alone.
        ops.push_remove_stopped(Ok(StoppedRemoval::Running));

        let res = HostLock::acquire(&ops, host()).await;

        assert_eq!(
            removals(&ops),
            vec![RecordedCall::RemoveStoppedContainer(
                host(),
                LOCK_NAME.into()
            )]
        );
        let err = res.err().expect("held");
        assert!(err.to_string().contains("another deploy in progress"));
    }

    #[tokio::test]
    async fn acquire_reaps_stopped_orphan() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Err(conflict()));
        ops.push_list_containers(Ok(vec![]));
        ops.push_remove_stopped(Ok(StoppedRemoval::Removed));
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));

        let lock = HostLock::acquire(&ops, host()).await.expect("acquire");

        assert_eq!(
            ops.calls(),
            vec![
                RecordedCall::CreateContainer(host(), LOCK_NAME.into()),
                RecordedCall::ListRunningContainers(host()),
                RecordedCall::RemoveStoppedContainer(host(), LOCK_NAME.into()),
                RecordedCall::CreateContainer(host(), LOCK_NAME.into()),
                RecordedCall::StartContainer(host(), "abc123".into()),
            ]
        );
        lock.release(&ops).await;
    }

    /// Two operators reap the same orphan; the other one wins the
    /// second create.
    #[tokio::test]
    async fn acquire_bails_when_create_after_reap_conflicts() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Err(conflict()));
        ops.push_list_containers(Ok(vec![]));
        ops.push_remove_stopped(Ok(StoppedRemoval::Removed));
        ops.push_create_container(Err(conflict()));

        let err = HostLock::acquire(&ops, host()).await.err().expect("held");

        assert!(err.to_string().contains("another deploy in progress"));
    }

    /// Registry unreachable (outage, rate limit, air gap) must not
    /// block a deploy when the sentinel image is already cached.
    #[tokio::test]
    async fn acquire_skips_pull_when_image_cached() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_pull_image(Err(DockerError::Invalid("registry unreachable".into())));
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));

        let lock = HostLock::acquire(&ops, host()).await.expect("acquire");

        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(..)))
        );
        lock.release(&ops).await;
    }

    /// A created its sentinel, B took the not-yet-started container
    /// for an orphan and replaced it. A's start must go to A's own id
    /// and A must report the lock as held: 404 when B's remove has
    /// finished, 409 while the daemon still has it marked for removal.
    #[tokio::test]
    async fn acquire_reports_held_when_own_sentinel_was_reaped_before_start() {
        for status in [404, 409] {
            let ops = FakeDockerOps::new();
            ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
            ops.push_create_container(Ok("abc123".into()));
            ops.push_start_container(Err(daemon_error(status)));

            let err = HostLock::acquire(&ops, host()).await.err().expect("held");

            assert!(err.to_string().contains("another deploy in progress"));
            assert_eq!(
                ops.calls().last(),
                Some(&RecordedCall::StartContainer(host(), "abc123".into()))
            );
            assert_eq!(removals(&ops), vec![]);
        }
    }

    /// Any other start failure is not "held" and keeps its cause.
    #[tokio::test]
    async fn acquire_surfaces_other_start_errors() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Err(daemon_error(500)));

        let err = HostLock::acquire(&ops, host()).await.err().expect("error");

        assert!(!err.to_string().contains("another deploy in progress"));
    }

    /// A holder whose sentinel exited (missed heartbeats) and was
    /// reaped by a later holder must not remove the later holder's
    /// sentinel on release: release removes the id it created, never
    /// the shared name.
    #[tokio::test]
    async fn release_removes_own_sentinel_by_id_not_by_name() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));
        ops.push_force_remove(Ok(()));

        let lock = HostLock::acquire(&ops, host()).await.expect("acquire");
        lock.release(&ops).await;

        assert_eq!(
            removals(&ops),
            vec![RecordedCall::ForceRemoveContainer(host(), "abc123".into())]
        );
    }

    /// Same invariant for the heartbeat: it must touch our own
    /// sentinel, not keep a later holder's alive by name.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_execs_into_own_sentinel_by_id() {
        let ops = std::sync::Arc::new(FakeDockerOps::new());
        ops.mark_image_present(LOCK_IMAGE_REPO, LOCK_IMAGE_TAG);
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));

        let mut lock = HostLock::acquire(&*ops, host()).await.expect("acquire");
        lock.spawn_heartbeat(ops.clone());
        tokio::time::sleep(HEARTBEAT_INTERVAL + Duration::from_secs(1)).await;
        lock.release(&*ops).await;

        assert!(ops.calls().contains(&RecordedCall::ExecOneshot(
            host(),
            "abc123".into(),
            vec!["touch".into(), HEARTBEAT_FILE.into()],
        )));
    }

    #[tokio::test]
    async fn acquire_pulls_when_image_missing() {
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("abc123".into()));
        ops.push_start_container(Ok(()));

        let lock = HostLock::acquire(&ops, host()).await.expect("acquire");

        assert_eq!(
            ops.calls()[0],
            RecordedCall::PullImage(host(), LOCK_IMAGE_REPO.into(), LOCK_IMAGE_TAG.into())
        );
        lock.release(&ops).await;
    }
}
