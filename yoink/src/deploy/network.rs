//! Idempotent docker network setup. Wraps `DockerOps::ensure_network`
//! so callers don't need to think about it. The admin network of the
//! proxy gets an ownership check, see [`ensure_admin`].

use std::collections::BTreeMap;

use crate::base::host::Host;
use crate::config::{ADMIN_NETWORK, PROXY_SERVICE_NAME};
use crate::docker_ops::{DockerError, DockerOps, NetworkDetail};

pub async fn ensure(ops: &dyn DockerOps, host: &Host, network: &str) -> Result<bool, DockerError> {
    ops.ensure_network(host, network).await
}

/// Label key and value that yoink puts on the admin network it creates.
pub const ADMIN_NETWORK_LABEL: (&str, &str) = ("yoink.role", "proxy-admin-network");

/// Why yoink does not use the existing admin network.
#[derive(Debug, thiserror::Error)]
pub enum AdminNetworkError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(
        "network {ADMIN_NETWORK:?} is internal. The admin relay cannot publish its port \
         on an internal network. Remove the network with `docker network rm \
         {ADMIN_NETWORK}` and run `yoink up` again."
    )]
    Internal,
    #[error(
        "network {ADMIN_NETWORK:?} was not created by yoink: it has no \
         `yoink.role=proxy-admin-network` label, and containers other than the proxy \
         and the admin relay are on it ({}). yoink does not put the proxy admin API on \
         it. Remove or rename that network, then run `yoink up` again.",
        foreign.join(", ")
    )]
    NotOwned { foreign: Vec<String> },
}

/// Make sure the admin network of the proxy exists and belongs to
/// yoink. Returns `true` when yoink created it. The admin API has no
/// authentication, so yoink does not join a network that another tool
/// made:
///
/// - yoink creates the network with [`ADMIN_NETWORK_LABEL`].
/// - A network with that label is yoink's.
/// - Earlier versions of yoink created the network without labels.
///   Docker cannot add a label to a network, so an unlabeled network is
///   accepted only when each container on it is a yoink proxy or the
///   admin relay. A network without containers is accepted too.
/// - An internal network is refused: the relay publishes a port.
pub async fn ensure_admin(ops: &dyn DockerOps, host: &Host) -> Result<bool, AdminNetworkError> {
    let detail = if let Some(detail) = ops.inspect_network(host, ADMIN_NETWORK).await? {
        detail
    } else {
        let labels: BTreeMap<String, String> = [ADMIN_NETWORK_LABEL]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if ops.create_network(host, ADMIN_NETWORK, &labels).await? {
            return Ok(true);
        }
        // Another run created it first. Check that one.
        ops.inspect_network(host, ADMIN_NETWORK)
            .await?
            .unwrap_or_default()
    };
    check_admin_network(ops, host, &detail).await?;
    Ok(false)
}

async fn check_admin_network(
    ops: &dyn DockerOps,
    host: &Host,
    detail: &NetworkDetail,
) -> Result<(), AdminNetworkError> {
    if detail.internal {
        return Err(AdminNetworkError::Internal);
    }
    let (key, value) = ADMIN_NETWORK_LABEL;
    if detail.labels.get(key).map(String::as_str) == Some(value) || detail.containers.is_empty() {
        return Ok(());
    }
    let ours: Vec<String> = ops
        .list_containers_by_label(host, "yoink.managed=true")
        .await?
        .into_iter()
        .filter(|c| {
            c.yoink_service.as_deref() == Some(PROXY_SERVICE_NAME)
                || crate::proxy::relay::is_relay(c)
        })
        .map(|c| c.name)
        .collect();
    let foreign: Vec<String> = detail
        .containers
        .iter()
        .filter(|name| !ours.contains(name))
        .cloned()
        .collect();
    if foreign.is_empty() {
        Ok(())
    } else {
        Err(AdminNetworkError::NotOwned { foreign })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{FakeDockerOps, RecordedCall};

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        }
    }

    #[tokio::test]
    async fn ensure_propagates_already_exists() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        let created = ensure(&ops, &host(), "yoink").await.unwrap();
        assert!(!created);
        assert_eq!(
            ops.calls(),
            vec![RecordedCall::EnsureNetwork(host(), "yoink".into())]
        );
    }

    #[tokio::test]
    async fn ensure_propagates_created() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(true));
        let created = ensure(&ops, &host(), "yoink").await.unwrap();
        assert!(created);
    }

    fn container(name: &str, service: Option<&str>) -> crate::docker_ops::ContainerInfo {
        crate::docker_ops::ContainerInfo {
            host: "host-a".into(),
            name: name.into(),
            image: String::new(),
            state: "running".into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: service.map(Into::into),
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: vec![ADMIN_NETWORK.into()],
            other_labels: BTreeMap::new(),
        }
    }

    fn unlabeled(containers: &[&str]) -> NetworkDetail {
        NetworkDetail {
            containers: containers.iter().map(|c| (*c).to_string()).collect(),
            ..NetworkDetail::default()
        }
    }

    #[tokio::test]
    async fn admin_network_is_created_with_the_yoink_label() {
        let ops = FakeDockerOps::new();
        assert!(ensure_admin(&ops, &host()).await.unwrap());
        let labels: BTreeMap<String, String> =
            [("yoink.role".to_string(), "proxy-admin-network".to_string())].into();
        assert_eq!(
            ops.calls(),
            vec![RecordedCall::CreateNetwork(
                host(),
                ADMIN_NETWORK.into(),
                labels
            )]
        );
    }

    #[tokio::test]
    async fn labeled_admin_network_is_used_as_is() {
        let ops = FakeDockerOps::new();
        let (k, v) = ADMIN_NETWORK_LABEL;
        ops.set_network(
            ADMIN_NETWORK,
            NetworkDetail {
                labels: [(k.to_string(), v.to_string())].into(),
                containers: vec!["anything".into()],
                internal: false,
            },
        );
        assert!(!ensure_admin(&ops, &host()).await.unwrap());
        assert!(ops.calls().is_empty(), "got: {:?}", ops.calls());
    }

    /// An existing host has the unlabeled network that an earlier yoink
    /// created, with the old proxy on it. yoink keeps using it.
    #[tokio::test]
    async fn unlabeled_admin_network_with_only_yoink_containers_is_accepted() {
        use crate::proxy::relay::RELAY_NAME;
        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&["yoink-proxy-abc", RELAY_NAME]));
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container(RELAY_NAME, None),
            container("api-1", Some("api")),
        ]));
        assert!(!ensure_admin(&ops, &host()).await.unwrap());

        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&[]));
        assert!(!ensure_admin(&ops, &host()).await.unwrap());
    }

    /// A network of that name from another tool: the proxy and the
    /// relay would share it with containers that can call the admin
    /// API, which has no authentication.
    #[tokio::test]
    async fn unlabeled_admin_network_with_other_containers_is_refused() {
        let ops = FakeDockerOps::new();
        ops.set_network(
            ADMIN_NETWORK,
            unlabeled(&["yoink-proxy-abc", "api-1", "other"]),
        );
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container("api-1", Some("api")),
        ]));
        let err = ensure_admin(&ops, &host()).await.unwrap_err();
        let AdminNetworkError::NotOwned { foreign } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(foreign, &["api-1", "other"]);
        let msg = err.to_string();
        assert!(msg.contains("not created by yoink"), "got: {msg}");
        assert!(msg.contains("(api-1, other)"), "got: {msg}");
    }

    #[tokio::test]
    async fn internal_admin_network_is_refused() {
        let (k, v) = ADMIN_NETWORK_LABEL;
        let ops = FakeDockerOps::new();
        ops.set_network(
            ADMIN_NETWORK,
            NetworkDetail {
                labels: [(k.to_string(), v.to_string())].into(),
                internal: true,
                containers: Vec::new(),
            },
        );
        let err = ensure_admin(&ops, &host()).await.unwrap_err();
        assert!(matches!(err, AdminNetworkError::Internal), "got {err:?}");
    }

    #[tokio::test]
    async fn ensure_propagates_error() {
        let ops = FakeDockerOps::new();
        let err = ensure(&ops, &host(), "yoink").await.unwrap_err();
        assert!(matches!(err, DockerError::FakeExhausted(_)));
    }
}
