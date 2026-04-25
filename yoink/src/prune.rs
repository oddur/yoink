//! Prune yoink-managed containers that the current config no longer
//! describes — renamed services, removed services, or stale exited
//! containers from a previous deploy. Run as `yoink prune` (or
//! `yoink prune --dry-run` to see what would go).

use std::collections::BTreeSet;

use thiserror::Error;

use crate::config::Config;
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, Host};

#[derive(Debug, Error)]
pub enum PruneError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
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
pub async fn run(
    ops: &dyn DockerOps,
    config: &Config,
    dry_run: bool,
) -> Result<PruneReport, PruneError> {
    let known_services: BTreeSet<String> = config.services.iter().map(|s| s.name.clone()).collect();

    let mut report = PruneReport::default();
    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        let containers = ops
            .list_containers_by_label(&host, "yoink.managed=true")
            .await
            .map_err(|source| PruneError::Docker {
                host: host.address.clone(),
                source,
            })?;
        for c in containers {
            let Some(reason) = classify(&c, &known_services) else {
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

fn classify(c: &ContainerInfo, known: &BTreeSet<String>) -> Option<PruneReason> {
    let svc = c.yoink_service.as_deref();
    match svc {
        Some(name) if !known.contains(name) => Some(PruneReason::ServiceNotInConfig),
        Some(_) if !c.is_running() => Some(PruneReason::StaleExited),
        // Running container of a known service: keep.
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
        let report = run(&ops, &cfg(&["api", "web"]), true).await.unwrap();
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
    async fn prune_actually_removes_when_not_dry_run() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![
            container("api-aaa", "api", true),
            container("yoink-old-ccc", "yoink-old", true),
        ]));
        ops.push_force_remove(Ok(()));
        let report = run(&ops, &cfg(&["api"]), false).await.unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].container, "yoink-old-ccc");
        assert!(report.planned.is_empty());
    }
}
