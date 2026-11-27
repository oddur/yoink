//! Status reporting. Queries each host through `DockerOps` and aggregates
//! the typed responses into a `StatusReport`. Multi-service-aware: the
//! report is grouped by host with all yoink-managed containers visible.

use futures_util::future::join_all;
use thiserror::Error;

use crate::config::Config;
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps};

#[derive(Debug, Error)]
pub enum StatusError {
    #[error("docker error: {0}")]
    Docker(#[from] DockerError),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostStatus {
    pub host: String,
    pub containers: Vec<ContainerInfo>,
}

/// A host that did not answer the status query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFailure {
    pub host: String,
    pub error: String,
}

impl std::fmt::Display for HostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.host, self.error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct StatusReport {
    pub hosts: Vec<HostStatus>,
    /// Hosts that did not answer. Only [`StatusReport::collect`] fills
    /// this. Kept out of the JSON output so its shape stays the same.
    #[serde(skip)]
    pub failed: Vec<HostFailure>,
}

impl StatusReport {
    /// All containers managed by yoink (any service) across every host
    /// in the config. Filtered server-side via the `yoink.managed=true`
    /// label. A host that does not answer goes into `failed` and the
    /// other hosts are still reported. Returns an error only when no
    /// host answered.
    pub async fn collect(ops: &dyn DockerOps, config: &Config) -> Result<Self, StatusError> {
        let mut report = Self::default();
        let mut first_error = None;
        for (host, result) in Self::query(ops, config, "yoink.managed=true").await {
            match result {
                Ok(containers) => report.hosts.push(HostStatus { host, containers }),
                Err(e) => {
                    report.failed.push(HostFailure {
                        host,
                        error: e.to_string(),
                    });
                    first_error.get_or_insert(e);
                }
            }
        }
        if report.hosts.is_empty()
            && let Some(e) = first_error
        {
            return Err(e.into());
        }
        Ok(report)
    }

    /// Keep the rows of a host that did not answer this time: each
    /// host in `failed` gets its `HostStatus` from `previous`, the last
    /// report that had it. A one-tick ssh blip then does not hide the
    /// host's containers. `failed` stays as is, so the error still
    /// shows. Hosts stay in the order of `config.fleet()`.
    pub fn keep_failed_hosts_from(&mut self, previous: &Self, config: &Config) {
        let carried: Vec<HostStatus> = previous
            .hosts
            .iter()
            .filter(|prev| self.failed.iter().any(|f| f.host == prev.host))
            .cloned()
            .collect();
        if carried.is_empty() {
            return;
        }
        self.hosts.extend(carried);
        let fleet = config.fleet();
        self.hosts
            .sort_by_key(|h| fleet.iter().position(|f| f.address == h.host));
    }

    /// All containers for one named service across every host.
    pub async fn collect_for_service(
        ops: &dyn DockerOps,
        config: &Config,
        service_name: &str,
    ) -> Result<Self, StatusError> {
        let mut report = Self::default();
        let label = format!("yoink.service={service_name}");
        for (host, result) in Self::query(ops, config, &label).await {
            report.hosts.push(HostStatus {
                host,
                containers: result?,
            });
        }
        Ok(report)
    }

    /// Per-host listing fan-out — runs the docker API calls concurrently
    /// across hosts. Called every `FAST_TICK` from the dashboard, so
    /// concurrency matters once we have multiple hosts.
    async fn query(
        ops: &dyn DockerOps,
        config: &Config,
        label: &str,
    ) -> Vec<(String, Result<Vec<ContainerInfo>, DockerError>)> {
        let per_host = config.fleet().into_iter().map(|host| async move {
            let containers = ops.list_containers_by_label(&host, label).await;
            (host.address, containers)
        });
        join_all(per_host).await
    }

    /// Highest-version running container for `service_name` across all
    /// hosts, excluding `current_version`. Used by rollback.
    #[must_use]
    pub fn previous_version(&self, current_version: &str) -> Option<String> {
        let mut versions: Vec<String> = self
            .hosts
            .iter()
            .flat_map(|h| h.containers.iter())
            .filter_map(|c| c.yoink_version.clone())
            .filter(|v| v != current_version)
            .collect();
        versions.sort();
        versions.dedup();
        versions.into_iter().next_back()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;
    use std::collections::BTreeMap;

    fn config_with_two_hosts() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
  - { address: host-b, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap()
    }

    fn container(host: &str, name: &str, version: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            host: host.into(),
            name: name.into(),
            image: String::new(),
            state: state.into(),
            status_text: "Up 1h (healthy)".into(),
            created_unix: Some(1_735_128_000),
            yoink_service: Some("app-a".into()),
            yoink_version: Some(version.into()),
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn collect_aggregates_each_host() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container(
            "host-a",
            "app-a-a1b2c3d",
            "a1b2c3d",
            "running",
        )]));
        ops.push_list_containers(Ok(vec![container(
            "host-b",
            "app-a-9f8e7d6",
            "9f8e7d6",
            "running",
        )]));
        let cfg = config_with_two_hosts();
        let report = StatusReport::collect(&ops, &cfg).await.unwrap();
        assert_eq!(report.hosts.len(), 2);
        assert_eq!(report.hosts[0].host, "host-a");
        assert_eq!(report.hosts[1].host, "host-b");
    }

    #[tokio::test]
    async fn collect_reports_reachable_hosts_when_one_fails() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Err(DockerError::SshProbe {
            host: "host-a".into(),
            detail: "connection refused".into(),
        }));
        ops.push_list_containers(Ok(vec![container(
            "host-b",
            "app-a-9f8e7d6",
            "9f8e7d6",
            "running",
        )]));
        let cfg = config_with_two_hosts();
        let report = StatusReport::collect(&ops, &cfg).await.unwrap();
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].host, "host-b");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].host, "host-a");
        assert!(report.failed[0].error.contains("connection refused"));
    }

    #[tokio::test]
    async fn collect_errors_when_every_host_fails() {
        let ops = FakeDockerOps::new();
        for host in ["host-a", "host-b"] {
            ops.push_list_containers(Err(DockerError::SshProbe {
                host: host.into(),
                detail: "connection refused".into(),
            }));
        }
        let cfg = config_with_two_hosts();
        assert!(StatusReport::collect(&ops, &cfg).await.is_err());
    }

    #[tokio::test]
    async fn previous_version_returns_highest_other() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![
            container("host-a", "app-a-a1b2c3d", "a1b2c3d", "running"),
            container("host-a", "app-a-9f8e7d6", "9f8e7d6", "exited"),
        ]));
        ops.push_list_containers(Ok(vec![]));
        let cfg = config_with_two_hosts();
        let report = StatusReport::collect(&ops, &cfg).await.unwrap();
        assert_eq!(report.previous_version("a1b2c3d"), Some("9f8e7d6".into()));
    }
}
