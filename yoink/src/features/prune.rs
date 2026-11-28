//! Prune yoink-managed containers that the current config no longer
//! describes — renamed services, removed services, or stale exited
//! containers from a previous deploy. Run as `yoink prune` (or
//! `yoink prune --dry-run` to see what would go).
//!
//! A prune that removes containers holds the deploy lock on every host,
//! as `yoink up` does. A stop-first deploy stops the old container
//! before the new one is healthy, and starts it again when the deploy
//! fails. Without the lock, a prune could remove that stopped container
//! in this window.

use std::collections::BTreeSet;
use std::sync::Arc;

use thiserror::Error;

use crate::config::Config;
use crate::deploy::lock::{self, LockError};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps};

#[derive(Debug, Error)]
pub enum PruneError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error(
        "could not get the deploy lock on {host}: {source}. A `yoink up` can be in \
         progress. Run `yoink prune` again when it is complete."
    )]
    Lock {
        host: String,
        #[source]
        source: LockError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneReason {
    /// `yoink.service` label refers to a service no longer in config
    /// (renamed or deleted).
    ServiceNotInConfig,
    /// Container exited and isn't the current spec for its service.
    /// Old containers from a previous reconcile.
    StaleExited,
}

#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub host: String,
    pub container: String,
    pub service: Option<String>,
    pub reason: PruneReason,
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed: Vec<PruneCandidate>,
    pub planned: Vec<PruneCandidate>,
}

/// Compute candidates and (unless `dry_run`) actually remove them.
/// Always reports what was found so the caller can print it either way.
///
/// Without `dry_run`, the prune first gets the deploy lock on every
/// host of the fleet, and releases the locks when it ends, also on an
/// error. When a lock is held, the prune removes nothing.
pub async fn run(
    ops: &Arc<dyn DockerOps>,
    config: &Config,
    dry_run: bool,
) -> Result<PruneReport, PruneError> {
    if dry_run {
        return scan(&**ops, config, true).await;
    }
    let locks = lock::acquire_all(ops, config.fleet(), |host, e| {
        tracing::warn!(host = %host.address, error = %e, "could not get the deploy lock");
    })
    .await
    .map_err(|(host, source)| PruneError::Lock {
        host: host.address,
        source,
    })?;
    let result = scan(&**ops, config, false).await;
    for lock in locks {
        lock.release(&**ops).await;
    }
    result
}

/// Find the candidates on each host and, unless `dry_run`, remove them.
async fn scan(
    ops: &dyn DockerOps,
    config: &Config,
    dry_run: bool,
) -> Result<PruneReport, PruneError> {
    let known_services: BTreeSet<String> = config.service_names().map(str::to_string).collect();
    let proxy_enabled = config.proxy_enabled();

    let mut report = PruneReport::default();
    for host in config.fleet() {
        let containers = ops
            .list_containers_by_label(&host, "yoink.managed=true")
            .await
            .map_err(|source| PruneError::Docker {
                host: host.address.clone(),
                source,
            })?;
        for c in containers {
            let Some(reason) = classify(&c, &known_services, proxy_enabled) else {
                continue;
            };
            let candidate = PruneCandidate {
                host: host.address.clone(),
                container: c.name.clone(),
                service: c.yoink_service.clone(),
                reason,
            };
            if dry_run {
                report.planned.push(candidate);
            } else {
                if let Err(e) = ops.force_remove_container(&host, &c.name).await {
                    return Err(PruneError::Docker {
                        host: host.address.clone(),
                        source: e,
                    });
                }
                report.removed.push(candidate);
            }
        }
    }
    Ok(report)
}

fn classify(c: &ContainerInfo, known: &BTreeSet<String>, proxy: bool) -> Option<PruneReason> {
    // The admin relay belongs to the proxy: it goes when the proxy is
    // off. `yoink up` recreates a relay that does not run.
    if crate::proxy::relay::is_relay(c) {
        return (!proxy).then_some(PruneReason::ServiceNotInConfig);
    }
    // A probe that leaked (Ctrl-C, a failed remove). A running probe
    // belongs to a deploy that removes it when the probe ends.
    if crate::docker_ops::is_probe(c) {
        return matches!(c.state.as_str(), "exited" | "dead").then_some(PruneReason::StaleExited);
    }
    let svc = c.yoink_service.as_deref();
    match svc {
        Some(name) if !known.contains(name) => Some(PruneReason::ServiceNotInConfig),
        // Only a container that has finished is stale. A paused,
        // restarting or just-created container of a declared service
        // is still in use.
        Some(_) if matches!(c.state.as_str(), "exited" | "dead") => Some(PruneReason::StaleExited),
        // Any other container of a known service: keep.
        // Container with no `yoink.service` label but managed=true:
        // shouldn't happen (every yoink-created container gets the
        // label) — leave it alone rather than blow it away on a
        // partial-state machine.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;
    use std::collections::BTreeMap;

    fn cfg(services: &[&str]) -> Config {
        use std::fmt::Write as _;
        let mut services_yaml = String::new();
        for n in services {
            writeln!(
                services_yaml,
                "  - name: {n}\n    image: img\n    tag: v1\n    run: {{}}"
            )
            .unwrap();
        }
        Config::parse_str(&format!(
            "hosts:\n  - {{ address: h1, user: root }}\nservices:\n{services_yaml}"
        ))
        .unwrap()
    }

    fn container(name: &str, service: &str, running: bool) -> ContainerInfo {
        ContainerInfo {
            host: "h1".into(),
            name: name.into(),
            image: String::new(),
            state: if running { "running" } else { "exited" }.into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: Some(service.into()),
            yoink_version: Some("v1".into()),
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn prune_lists_renamed_services_and_stale_exited() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![
            container("api-aaa", "api", true),
            container("api-bbb", "api", false), // stale exited
            container("yoink-old-ccc", "yoink-old", true), // renamed away
            container("web-ddd", "web", true),
        ]));
        let report = scan(&ops, &cfg(&["api", "web"]), true).await.unwrap();
        assert_eq!(report.removed.len(), 0);
        let names: Vec<&str> = report
            .planned
            .iter()
            .map(|p| p.container.as_str())
            .collect();
        assert!(names.contains(&"api-bbb"));
        assert!(names.contains(&"yoink-old-ccc"));
        assert!(!names.contains(&"api-aaa"));
        assert!(!names.contains(&"web-ddd"));
    }

    #[tokio::test]
    async fn prune_keeps_paused_restarting_and_created_containers() {
        let ops = FakeDockerOps::new();
        let with_state = |name: &str, state: &str| ContainerInfo {
            state: state.into(),
            ..container(name, "api", false)
        };
        ops.push_list_containers(Ok(vec![
            with_state("api-paused", "paused"),
            with_state("api-restarting", "restarting"),
            with_state("api-created", "created"),
            with_state("api-dead", "dead"),
            with_state("api-exited", "exited"),
        ]));
        let report = scan(&ops, &cfg(&["api"]), true).await.unwrap();
        let names: Vec<&str> = report
            .planned
            .iter()
            .map(|p| p.container.as_str())
            .collect();
        assert_eq!(names, ["api-dead", "api-exited"]);
    }

    /// The admin relay has no `yoink.service`. It stays while the proxy
    /// is on, and goes with the proxy.
    #[tokio::test]
    async fn prune_keeps_the_admin_relay_only_while_the_proxy_is_on() {
        use crate::proxy::relay::RELAY_NAME;
        let relay = ContainerInfo {
            yoink_service: None,
            ..container(RELAY_NAME, "", true)
        };
        let routed = Config::parse_str(
            "hosts:\n  - { address: h1, user: root }\nservices:\n  - name: api\n    \
             image: img\n    tag: v1\n    domain: api.example.com\n    run: { port: 8080 }\n",
        )
        .unwrap();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![relay.clone()]));
        assert!(scan(&ops, &routed, true).await.unwrap().planned.is_empty());

        ops.push_list_containers(Ok(vec![relay]));
        let report = scan(&ops, &cfg(&["api"]), true).await.unwrap();
        assert_eq!(report.planned.len(), 1);
        assert_eq!(report.planned[0].container, RELAY_NAME);
        assert_eq!(report.planned[0].reason, PruneReason::ServiceNotInConfig);
    }

    /// A leaked probe has no `yoink.service`. A stopped one goes, a
    /// running one stays.
    #[tokio::test]
    async fn prune_removes_only_a_stopped_probe() {
        let probe = |name: &str, running: bool| ContainerInfo {
            yoink_service: None,
            ..container(name, "", running)
        };
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![
            probe("yoink-probe-api-1", false),
            probe("yoink-tcp-probe-api-2", true),
        ]));
        let report = scan(&ops, &cfg(&["api"]), true).await.unwrap();
        let names: Vec<&str> = report
            .planned
            .iter()
            .map(|p| p.container.as_str())
            .collect();
        assert_eq!(names, ["yoink-probe-api-1"]);
        assert_eq!(report.planned[0].reason, PruneReason::StaleExited);
    }

    fn lock_free(ops: &FakeDockerOps) {
        ops.mark_image_present("alpine", "3.20");
        ops.push_create_container(Ok("lock-id".into()));
        ops.push_start_container(Ok(()));
    }

    #[tokio::test]
    async fn prune_actually_removes_when_not_dry_run() {
        use crate::docker_ops::RecordedCall;
        let ops = Arc::new(FakeDockerOps::new());
        lock_free(&ops);
        ops.push_list_containers(Ok(vec![
            container("api-aaa", "api", true),
            container("yoink-old-ccc", "yoink-old", true),
        ]));
        ops.push_force_remove(Ok(()));
        ops.push_force_remove(Ok(()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();
        let report = run(&dyn_ops, &cfg(&["api"]), false).await.unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].container, "yoink-old-ccc");
        assert!(report.planned.is_empty());
        // Lock, list, remove, release the lock by id.
        let calls = ops.calls();
        let h1 = cfg(&["api"]).fleet().remove(0);
        assert_eq!(
            calls.first(),
            Some(&RecordedCall::CreateContainer(
                h1.clone(),
                lock::LOCK_NAME.into()
            ))
        );
        assert_eq!(
            calls.last(),
            Some(&RecordedCall::ForceRemoveContainer(h1, "lock-id".into()))
        );
    }

    /// Regression: prune took no lock. During the healthcheck of a
    /// stop-first deploy, it removed the stopped old container, so the
    /// deploy could not start it again. With a deploy in progress, the
    /// prune now removes nothing.
    #[tokio::test]
    async fn prune_removes_nothing_while_a_deploy_holds_the_lock() {
        use crate::docker_ops::RecordedCall;
        let ops = Arc::new(FakeDockerOps::new());
        ops.mark_image_present("alpine", "3.20");
        ops.push_create_container(Err(DockerError::Bollard {
            host: "h1".into(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code: 409,
                message: "name in use".into(),
            },
        }));
        let sentinel = ContainerInfo {
            yoink_service: None,
            ..container(lock::LOCK_NAME, "", true)
        };
        ops.push_list_containers(Ok(vec![sentinel]));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();
        let err = run(&dyn_ops, &cfg(&["api"]), false).await.unwrap_err();
        assert!(
            matches!(err, PruneError::Lock { ref host, .. } if host == "h1"),
            "got: {err}"
        );
        assert!(err.to_string().contains("`yoink up`"), "got: {err}");
        assert!(
            !ops.calls().iter().any(|c| matches!(
                c,
                RecordedCall::ListContainersByLabel(..) | RecordedCall::ForceRemoveContainer(..)
            )),
            "got: {:?}",
            ops.calls()
        );
    }

    /// A dry run takes no lock: it removes nothing.
    #[tokio::test]
    async fn prune_dry_run_takes_no_lock() {
        let ops = Arc::new(FakeDockerOps::new());
        ops.push_list_containers(Ok(Vec::new()));
        let dyn_ops: Arc<dyn DockerOps> = ops.clone();
        run(&dyn_ops, &cfg(&["api"]), true).await.unwrap();
        assert_eq!(
            ops.calls(),
            [crate::docker_ops::RecordedCall::ListContainersByLabel(
                cfg(&["api"]).fleet().remove(0),
                "yoink.managed=true".into()
            )]
        );
    }
}
