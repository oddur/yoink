//! The admin relay: a small socat container that gives yoink a path to
//! the Caddy admin API.
//!
//! Docker forwards every published port of a container to its gateway
//! endpoint. The gateway endpoint of the proxy is `yoink-ingress`, and
//! it must stay there: ports 80 and 443 then go to the proxy IP on
//! `yoink-ingress`, so a service on that network that calls a public
//! domain of its own host reaches the proxy on the same bridge. A
//! published admin port on the proxy would thus also land on
//! `yoink-ingress`.
//!
//! The relay joins only [`ADMIN_NETWORK`], so that network is its
//! gateway. It publishes `127.0.0.1:0:2019` and forwards each
//! connection to [`ADMIN_API_ALIAS`]`:2019`. socat resolves the alias
//! for each connection, so a recreated proxy needs no relay restart.
//! yoink SSH-tunnels to the published port of the relay.
//!
//! The relay belongs to the implicit proxy deploy: every reconcile with
//! the proxy on calls [`ensure`] on each proxy host before the first
//! Caddy push. It has the label `yoink.managed=true` but no
//! `yoink.service`, so the deploy never takes it for a replica.

use crate::base::host::Host;
use crate::config::{ADMIN_API_ALIAS, ADMIN_NETWORK, ADMIN_PORT, RunOptions};
use crate::docker::{self, RunSpec};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps};

/// Container name of the relay. One relay per host.
pub const RELAY_NAME: &str = "yoink-proxy-admin-relay";

/// Label that marks the relay, with the value [`RELAY_ROLE`]. Same key
/// as the unregistry sidecar.
pub const RELAY_ROLE_LABEL: &str = "yoink.role";
/// Value of [`RELAY_ROLE_LABEL`] on the relay.
pub const RELAY_ROLE: &str = "proxy-admin-relay";

/// Image of the relay. Pinned: a new tag changes the spec hash and
/// recreates the relay once.
pub const RELAY_IMAGE: &str = "alpine/socat";
pub const RELAY_TAG: &str = "1.8.1.3";

/// Label filter for `list_containers_by_label` that finds the relay.
#[must_use]
pub fn relay_label_filter() -> String {
    format!("{RELAY_ROLE_LABEL}={RELAY_ROLE}")
}

/// True when `c` (from a `yoink.managed=true` listing) is the relay.
/// The listing drops `yoink.*` labels from `other_labels`, so the
/// check uses the fixed name and the missing `yoink.service`.
#[must_use]
pub fn is_relay(c: &ContainerInfo) -> bool {
    c.name == RELAY_NAME && c.yoink_service.is_none()
}

/// What [`ensure`] did on one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayChange {
    /// The relay ran at the current spec.
    Unchanged,
    /// yoink created the relay, or replaced one with another spec.
    Created,
}

/// The run spec of the relay, without the spec hash label.
fn relay_spec() -> RunSpec {
    let labels = [
        ("yoink.managed".to_string(), "true".to_string()),
        (RELAY_ROLE_LABEL.to_string(), RELAY_ROLE.to_string()),
    ]
    .into_iter()
    .collect();
    RunSpec {
        image: RELAY_IMAGE.to_string(),
        tag: RELAY_TAG.to_string(),
        networks: vec![ADMIN_NETWORK.to_string()],
        container_name: RELAY_NAME.to_string(),
        labels,
        env: std::collections::BTreeMap::new(),
        // The secure default: all capabilities dropped, read-only root
        // file system, user 65534, restart unless-stopped (as the proxy).
        // socat binds port 2019, which needs no capability.
        options: RunOptions::default(),
        entrypoint: None,
        command: vec![
            format!("TCP-LISTEN:{ADMIN_PORT},fork,reuseaddr"),
            format!("TCP:{ADMIN_API_ALIAS}:{ADMIN_PORT}"),
        ],
        publish: vec![format!("127.0.0.1:0:{ADMIN_PORT}")],
        binds: Vec::new(),
        volumes: Vec::new(),
        docker_healthcheck: None,
        proxy: false,
    }
}

/// The spec hash of the relay that this yoink version runs.
#[must_use]
pub fn relay_spec_hash() -> String {
    docker::compute_spec_hash(&relay_spec())
}

/// Make sure the relay runs on `host` at the current spec. A relay with
/// another spec hash, or one that does not run, is removed and created
/// again. The admin network must exist.
pub async fn ensure(ops: &dyn DockerOps, host: &Host) -> Result<RelayChange, DockerError> {
    let hash = relay_spec_hash();
    let existing = ops
        .list_containers_by_label(host, &relay_label_filter())
        .await?;
    let current = existing.iter().any(|c| {
        c.name == RELAY_NAME && c.is_running() && c.yoink_spec_hash.as_deref() == Some(&hash)
    });
    if current {
        return Ok(RelayChange::Unchanged);
    }
    for c in &existing {
        ops.force_remove_container(host, &c.name).await?;
    }
    if !ops.image_present(host, RELAY_IMAGE, RELAY_TAG).await? {
        ops.pull_image(host, RELAY_IMAGE, RELAY_TAG, None).await?;
    }
    let mut spec = relay_spec();
    spec.labels.insert("yoink.spec_hash".into(), hash);
    let body = docker::build_container(&spec).map_err(|e| DockerError::Invalid(e.to_string()))?;
    ops.create_container(host, RELAY_NAME, body).await?;
    ops.start_container(host, RELAY_NAME).await?;
    Ok(RelayChange::Created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{FakeDockerOps, RecordedCall};

    fn host() -> Host {
        Host {
            user: "u".into(),
            address: "h".into(),
            port: None,
        }
    }

    fn relay(state: &str, hash: &str) -> ContainerInfo {
        ContainerInfo {
            host: "h".into(),
            name: RELAY_NAME.into(),
            image: format!("{RELAY_IMAGE}:{RELAY_TAG}"),
            state: state.into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: None,
            yoink_version: None,
            yoink_spec_hash: Some(hash.into()),
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: vec![ADMIN_NETWORK.into()],
            other_labels: std::collections::BTreeMap::new(),
        }
    }

    /// The relay is on the admin network only, so that network is its
    /// gateway and gets the loopback publish. It forwards to the alias
    /// that exists only on the admin endpoint of the proxy.
    #[test]
    fn relay_body_publishes_loopback_on_the_admin_network() {
        let body = docker::build_container(&relay_spec()).expect("body");
        assert_eq!(body.image.as_deref(), Some("alpine/socat:1.8.1.3"));
        assert_eq!(
            body.cmd.as_deref(),
            Some(
                &[
                    "TCP-LISTEN:2019,fork,reuseaddr".to_string(),
                    "TCP:yoink-proxy-admin-api:2019".to_string(),
                ][..]
            )
        );
        let host_config = body.host_config.expect("host config");
        assert_eq!(host_config.network_mode.as_deref(), Some(ADMIN_NETWORK));
        let bindings = host_config.port_bindings.expect("port bindings");
        let admin = bindings["2019/tcp"].as_ref().expect("2019 binding");
        assert_eq!(admin[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(admin[0].host_port.as_deref(), Some("0"));
        assert_eq!(host_config.cap_drop, Some(vec!["ALL".to_string()]));
        let endpoints = body.networking_config.unwrap().endpoints_config.unwrap();
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), [ADMIN_NETWORK]);
        let labels = body.labels.expect("labels");
        assert_eq!(
            labels.get(RELAY_ROLE_LABEL).map(String::as_str),
            Some(RELAY_ROLE)
        );
        assert!(!labels.contains_key("yoink.service"));
    }

    #[tokio::test]
    async fn running_relay_at_spec_is_kept() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![relay("running", &relay_spec_hash())]));
        assert_eq!(ensure(&ops, &host()).await.unwrap(), RelayChange::Unchanged);
        assert_eq!(
            ops.calls(),
            [RecordedCall::ListContainersByLabel(
                host(),
                "yoink.role=proxy-admin-relay".into()
            )]
        );
    }

    /// A relay with an old spec or a stopped relay is created again.
    #[tokio::test]
    async fn relay_with_another_spec_or_stopped_is_recreated() {
        for existing in [
            relay("running", "0000"),
            relay("exited", &relay_spec_hash()),
        ] {
            let ops = FakeDockerOps::new();
            ops.push_list_containers(Ok(vec![existing]));
            ops.push_force_remove(Ok(()));
            ops.push_pull_image(Ok(()));
            ops.push_create_container(Ok("id".into()));
            ops.push_start_container(Ok(()));
            assert_eq!(ensure(&ops, &host()).await.unwrap(), RelayChange::Created);
            let calls = ops.calls();
            assert!(calls.contains(&RecordedCall::ForceRemoveContainer(
                host(),
                RELAY_NAME.into()
            )));
            assert!(calls.contains(&RecordedCall::PullImage(
                host(),
                RELAY_IMAGE.into(),
                RELAY_TAG.into()
            )));
            assert_eq!(
                calls.last(),
                Some(&RecordedCall::StartContainer(host(), RELAY_NAME.into()))
            );
        }
    }

    #[tokio::test]
    async fn missing_relay_is_created_without_a_pull_when_the_image_is_present() {
        let ops = FakeDockerOps::new();
        ops.mark_image_present(RELAY_IMAGE, RELAY_TAG);
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        assert_eq!(ensure(&ops, &host()).await.unwrap(), RelayChange::Created);
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(..)))
        );
    }
}
