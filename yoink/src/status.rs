//! Status reporting. Queries each host through `DockerOps` and aggregates
//! the typed responses into a `StatusReport`. No more JSON parsing —
//! bollard's `ContainerSummary` already gives us labels as a `HashMap`.

use thiserror::Error;

use crate::config::Config;
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, Host};

#[derive(Debug, Error)]
pub enum StatusError {
    #[error("docker error: {0}")]
    Docker(#[from] DockerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatus {
    pub host: String,
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub service: String,
    pub hosts: Vec<HostStatus>,
}

impl StatusReport {
    pub async fn collect(ops: &dyn DockerOps, config: &Config) -> Result<Self, StatusError> {
        let label = format!("yoink.service={}", config.service.name);
        let mut hosts = Vec::with_capacity(config.hosts.len());
        for host_cfg in &config.hosts {
            let host = Host::from(host_cfg);
            let containers = ops.list_containers_by_label(&host, &label).await?;
            hosts.push(HostStatus {
                host: host.address,
                containers,
            });
        }
        Ok(Self {
            service: config.service.name.clone(),
            hosts,
        })
    }

    /// Highest-version running container for the service across all hosts,
    /// excluding `current_version`. Used by rollback.
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
[service]
name = "app-a"
image = "registry.example.com/app-a"

[[hosts]]
address = "host-a"
user = "deploy"

[[hosts]]
address = "host-b"
user = "deploy"

[run]
port = 3000
"#,
        )
        .unwrap()
    }

    fn container(host: &str, name: &str, version: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            host: host.into(),
            name: name.into(),
            state: state.into(),
            status_text: "Up 1h (healthy)".into(),
            created_at: "2026-04-25 09:00".into(),
            yoink_service: Some("app-a".into()),
            yoink_version: Some(version.into()),
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
        assert_eq!(report.service, "app-a");
        assert_eq!(report.hosts.len(), 2);
        assert_eq!(report.hosts[0].host, "host-a");
        assert_eq!(report.hosts[1].host, "host-b");
    }

    #[tokio::test]
    async fn collect_propagates_docker_error() {
        let ops = FakeDockerOps::new();
        let cfg = config_with_two_hosts();
        let err = StatusReport::collect(&ops, &cfg).await.unwrap_err();
        assert!(matches!(err, StatusError::Docker(_)));
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

    #[tokio::test]
    async fn previous_version_none_when_only_current_exists() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container(
            "host-a",
            "app-a-a1b2c3d",
            "a1b2c3d",
            "running",
        )]));
        ops.push_list_containers(Ok(vec![]));
        let cfg = config_with_two_hosts();
        let report = StatusReport::collect(&ops, &cfg).await.unwrap();
        assert_eq!(report.previous_version("a1b2c3d"), None);
    }
}
