//! Deploy orchestration. Per host:
//!  1. Ensure the shared docker network exists.
//!  2. Pull the new image:tag.
//!  3. Discover existing containers labeled with this service.
//!  4. Force-remove any container squatting on the new container's name.
//!  5. Create + start the new container (with yoink + caddy labels, env).
//!  6. Poll the healthcheck until 200 (or fail).
//!  7. Stop every previously-running container for this service.
//!
//! Hosts are iterated sequentially. Failure on any host short-circuits —
//! already-deployed hosts keep the new version.

use std::collections::BTreeMap;

use thiserror::Error;
use tracing::warn;

use crate::config::{Config, HostConfig};
use crate::docker::{self, BuildError, RunSpec};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, Host};
use crate::healthcheck::{self, HealthcheckError};
use crate::network;
use crate::secrets::InfisicalToken;

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error("config build error: {0}")]
    Build(#[from] BuildError),
    #[error("healthcheck failed on {host} for {container}: {source}")]
    Healthcheck {
        host: String,
        container: String,
        #[source]
        source: HealthcheckError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployEvent {
    Started {
        service: String,
        version: String,
        host: String,
    },
    NetworkReady {
        host: String,
        network: String,
        created: bool,
    },
    PullStarted {
        host: String,
        image: String,
        tag: String,
    },
    PullFinished {
        host: String,
    },
    ContainerStarted {
        host: String,
        container: String,
    },
    HealthcheckHealthy {
        host: String,
        container: String,
        attempts: u32,
    },
    OldContainerStopped {
        host: String,
        container: String,
    },
    Done {
        host: String,
        container: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDeployResult {
    pub host: String,
    pub container: String,
    pub healthcheck_attempts: u32,
    pub stopped_old: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployReport {
    pub service: String,
    pub version: String,
    pub hosts: Vec<HostDeployResult>,
}

#[must_use]
pub fn container_name(service: &str, version: &str) -> String {
    format!("{service}-{version}")
}

#[must_use]
pub fn build_labels(config: &Config, version: &str) -> BTreeMap<String, String> {
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    if let Some(c) = &config.caddy {
        for entry in &c.labels {
            if let Some((k, v)) = entry.split_once('=') {
                labels.insert(k.trim().to_string(), v.to_string());
            } else {
                labels.insert(entry.trim().to_string(), String::new());
            }
        }
    }
    // yoink labels always win on collision so the source-of-truth labels
    // can't be hijacked by a typo in [caddy].labels.
    labels.insert("yoink.service".into(), config.service.name.clone());
    labels.insert("yoink.version".into(), version.to_string());
    labels
}

#[must_use]
pub fn build_env(config: &Config, token: Option<&InfisicalToken>) -> BTreeMap<String, String> {
    let mut env = config.run.env.clone();
    if let Some(token) = token {
        env.insert("INFISICAL_TOKEN".into(), token.expose().to_string());
    }
    env
}

pub async fn deploy(
    ops: &dyn DockerOps,
    config: &Config,
    version: &str,
    secrets_token: Option<&InfisicalToken>,
    on_event: &mut dyn FnMut(DeployEvent),
) -> Result<DeployReport, DeployError> {
    let mut host_results = Vec::with_capacity(config.hosts.len());
    for host in &config.hosts {
        let result = deploy_one_host(ops, config, version, secrets_token, host, on_event).await?;
        host_results.push(result);
    }
    Ok(DeployReport {
        service: config.service.name.clone(),
        version: version.to_string(),
        hosts: host_results,
    })
}

async fn deploy_one_host(
    ops: &dyn DockerOps,
    config: &Config,
    version: &str,
    secrets_token: Option<&InfisicalToken>,
    host_cfg: &HostConfig,
    on_event: &mut dyn FnMut(DeployEvent),
) -> Result<HostDeployResult, DeployError> {
    let host = Host::from(host_cfg);
    on_event(DeployEvent::Started {
        service: config.service.name.clone(),
        version: version.to_string(),
        host: host.address.clone(),
    });

    let created = network::ensure(ops, &host, &config.deploy.network)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    on_event(DeployEvent::NetworkReady {
        host: host.address.clone(),
        network: config.deploy.network.clone(),
        created,
    });

    pull_image(ops, &host, config, version, on_event).await?;
    let existing = list_existing_containers(ops, &host, &config.service.name).await?;

    let new_name = container_name(&config.service.name, version);
    force_remove_name(ops, &host, &new_name).await;
    start_new_container(ops, &host, config, version, secrets_token, &new_name).await?;
    on_event(DeployEvent::ContainerStarted {
        host: host.address.clone(),
        container: new_name.clone(),
    });

    let attempts = wait_until_healthy(ops, &host, config, &new_name).await?;
    on_event(DeployEvent::HealthcheckHealthy {
        host: host.address.clone(),
        container: new_name.clone(),
        attempts,
    });

    let stopped_old = swap_out_old_containers(
        ops,
        &host,
        config.deploy.drain_timeout,
        &existing,
        &new_name,
        on_event,
    )
    .await?;

    on_event(DeployEvent::Done {
        host: host.address.clone(),
        container: new_name.clone(),
    });

    Ok(HostDeployResult {
        host: host.address,
        container: new_name,
        healthcheck_attempts: attempts,
        stopped_old,
    })
}

async fn pull_image(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    version: &str,
    on_event: &mut dyn FnMut(DeployEvent),
) -> Result<(), DeployError> {
    on_event(DeployEvent::PullStarted {
        host: host.address.clone(),
        image: config.service.image.clone(),
        tag: version.to_string(),
    });
    ops.pull_image(host, &config.service.image, version)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    on_event(DeployEvent::PullFinished {
        host: host.address.clone(),
    });
    Ok(())
}

async fn list_existing_containers(
    ops: &dyn DockerOps,
    host: &Host,
    service: &str,
) -> Result<Vec<ContainerInfo>, DeployError> {
    let label = format!("yoink.service={service}");
    ops.list_containers_by_label(host, &label)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })
}

async fn force_remove_name(ops: &dyn DockerOps, host: &Host, name: &str) {
    if let Err(e) = ops.force_remove_container(host, name).await {
        warn!(host = %host.address, container = %name, error = %e, "force_remove_container failed; continuing");
    }
}

async fn start_new_container(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    version: &str,
    secrets_token: Option<&InfisicalToken>,
    new_name: &str,
) -> Result<(), DeployError> {
    let spec = RunSpec {
        image: config.service.image.clone(),
        tag: version.to_string(),
        network: config.deploy.network.clone(),
        container_name: new_name.to_string(),
        labels: build_labels(config, version),
        env: build_env(config, secrets_token),
        options: config.run.options.clone(),
        entrypoint: None,
        command: Vec::new(),
    };
    let body = docker::build_container(&spec)?;
    ops.create_container(host, new_name, body)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    ops.start_container(host, new_name)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    Ok(())
}

async fn wait_until_healthy(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    new_name: &str,
) -> Result<u32, DeployError> {
    match healthcheck::poll(
        ops,
        host,
        &config.deploy.network,
        new_name,
        config.run.port,
        &config.deploy.healthcheck_path,
        config.deploy.healthcheck_timeout,
    )
    .await
    {
        Ok(a) => Ok(a),
        Err(source) => {
            // Stop the broken container so it can't compete for caddy routing.
            let _ = ops
                .stop_container(host, new_name, std::time::Duration::from_secs(5))
                .await;
            Err(DeployError::Healthcheck {
                host: host.address.clone(),
                container: new_name.to_string(),
                source,
            })
        }
    }
}

async fn swap_out_old_containers(
    ops: &dyn DockerOps,
    host: &Host,
    drain: std::time::Duration,
    existing: &[ContainerInfo],
    new_name: &str,
    on_event: &mut dyn FnMut(DeployEvent),
) -> Result<Vec<String>, DeployError> {
    let mut stopped = Vec::new();
    for old in existing
        .iter()
        .filter(|c| c.name != new_name && c.is_running())
    {
        if let Err(e) = ops.stop_container(host, &old.name, drain).await {
            warn!(host = %host.address, old = %old.name, error = %e, "failed to stop old container");
            continue;
        }
        on_event(DeployEvent::OldContainerStopped {
            host: host.address.clone(),
            container: old.name.clone(),
        });
        stopped.push(old.name.clone());
    }
    Ok(stopped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;
    use std::collections::BTreeMap;

    fn one_host_config_with_caddy() -> Config {
        Config::parse_str(
            r#"
[service]
name = "app-a"
image = "registry.example.com/app-a"

[deploy]
healthcheck_timeout = "60s"
drain_timeout = "10s"

[[hosts]]
address = "host-a"
user = "deploy"

[caddy]
labels = ["caddy=app-a.example.com", "caddy.reverse_proxy={{upstreams 3000}}"]

[run]
port = 3000

[run.env]
LOG_LEVEL = "info"

[run.options]
memory = "512m"
"#,
        )
        .unwrap()
    }

    fn container_info(name: &str, version: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: name.into(),
            state: state.into(),
            status_text: "Up 1h (healthy)".into(),
            created_at: "2026-04-25 09:00".into(),
            yoink_service: Some("app-a".into()),
            yoink_version: Some(version.into()),
            other_labels: BTreeMap::new(),
        }
    }

    fn happy_path_ops(existing_running: &[(&str, &str)]) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(existing_running
            .iter()
            .map(|(n, v)| container_info(n, v, "running"))
            .collect()));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        for _ in existing_running {
            ops.push_stop_container(Ok(()));
        }
        ops
    }

    #[test]
    fn container_name_format() {
        assert_eq!(container_name("app-a", "a1b2c3d"), "app-a-a1b2c3d");
    }

    #[test]
    fn build_labels_merges_yoink_and_caddy() {
        let cfg = one_host_config_with_caddy();
        let labels = build_labels(&cfg, "abc1234");
        assert_eq!(labels.get("yoink.service"), Some(&"app-a".into()));
        assert_eq!(labels.get("yoink.version"), Some(&"abc1234".into()));
        assert_eq!(labels.get("caddy"), Some(&"app-a.example.com".into()));
    }

    #[test]
    fn build_labels_yoink_overrides_user_collision() {
        let mut cfg = one_host_config_with_caddy();
        cfg.caddy
            .as_mut()
            .unwrap()
            .labels
            .push("yoink.service=hijack".into());
        let labels = build_labels(&cfg, "v");
        assert_eq!(labels.get("yoink.service"), Some(&"app-a".into()));
    }

    #[test]
    fn build_env_includes_run_env() {
        let cfg = one_host_config_with_caddy();
        let env = build_env(&cfg, None);
        assert_eq!(env.get("LOG_LEVEL"), Some(&"info".into()));
        assert!(!env.contains_key("INFISICAL_TOKEN"));
    }

    #[test]
    fn build_env_injects_infisical_token() {
        let cfg = one_host_config_with_caddy();
        let token = InfisicalToken::new("st_abcdefghijklmnop").unwrap();
        let env = build_env(&cfg, Some(&token));
        assert_eq!(
            env.get("INFISICAL_TOKEN"),
            Some(&"st_abcdefghijklmnop".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_happy_path_emits_expected_events_and_swaps_old() {
        let ops = happy_path_ops(&[("app-a-9f8e7d6", "9f8e7d6")]);
        let cfg = one_host_config_with_caddy();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let report = deploy(&ops, &cfg, "a1b2c3d", None, &mut sink)
            .await
            .unwrap();
        assert_eq!(report.service, "app-a");
        assert_eq!(report.version, "a1b2c3d");
        assert_eq!(report.hosts[0].container, "app-a-a1b2c3d");
        assert_eq!(
            report.hosts[0].stopped_old,
            vec!["app-a-9f8e7d6".to_string()]
        );

        let kinds: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                DeployEvent::Started { .. } => "Started",
                DeployEvent::NetworkReady { .. } => "NetworkReady",
                DeployEvent::PullStarted { .. } => "PullStarted",
                DeployEvent::PullFinished { .. } => "PullFinished",
                DeployEvent::ContainerStarted { .. } => "ContainerStarted",
                DeployEvent::HealthcheckHealthy { .. } => "HealthcheckHealthy",
                DeployEvent::OldContainerStopped { .. } => "OldContainerStopped",
                DeployEvent::Done { .. } => "Done",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "Started",
                "NetworkReady",
                "PullStarted",
                "PullFinished",
                "ContainerStarted",
                "HealthcheckHealthy",
                "OldContainerStopped",
                "Done",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_happy_path_with_no_existing_containers() {
        let ops = happy_path_ops(&[]);
        let cfg = one_host_config_with_caddy();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let report = deploy(&ops, &cfg, "abcdef0", None, &mut sink)
            .await
            .unwrap();
        assert_eq!(report.hosts[0].stopped_old.len(), 0);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DeployEvent::OldContainerStopped { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_propagates_pull_failure() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        // pull fails (no scripted response → FakeExhausted)
        let cfg = one_host_config_with_caddy();
        let mut sink = |_: DeployEvent| {};
        let err = deploy(&ops, &cfg, "ffffff0", None, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Docker { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_propagates_create_container_failure() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![]));
        ops.push_force_remove(Ok(()));
        // create_container has no scripted response
        let cfg = one_host_config_with_caddy();
        let mut sink = |_: DeployEvent| {};
        let err = deploy(&ops, &cfg, "abcdef0", None, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Docker { .. }));
    }
}
