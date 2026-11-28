//! Idempotent docker network setup. Wraps `DockerOps::ensure_network`
//! so callers don't need to think about it. The admin network of the
//! proxy gets an ownership check, see [`ensure_admin`].

use std::collections::BTreeMap;

use crate::base::host::Host;
use crate::config::{ADMIN_NETWORK, PROXY_SERVICE_NAME};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, NetworkDetail, is_probe};
use crate::proxy::relay::is_relay;

pub async fn ensure(ops: &dyn DockerOps, host: &Host, network: &str) -> Result<bool, DockerError> {
    ops.ensure_network(host, network).await
}

/// Label key and value that yoink puts on the admin network it creates.
pub const ADMIN_NETWORK_LABEL: (&str, &str) = ("yoink.role", "proxy-admin-network");

/// Why yoink does not use the existing admin network. Each message
/// ends with the commands that clear the network: Docker cannot rename
/// a network, and `docker network rm` refuses a network that a
/// container is on.
#[derive(Debug, thiserror::Error)]
pub enum AdminNetworkError {
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error(
        "network {ADMIN_NETWORK:?} is internal. The admin relay cannot publish its port \
         on an internal network. {remedy}"
    )]
    Internal { remedy: String },
    #[error(
        "network {ADMIN_NETWORK:?} was not created by yoink: it has no \
         `yoink.role=proxy-admin-network` label, and containers that yoink does not \
         manage are on it ({}). yoink does not put the proxy admin API on it. {remedy}",
        foreign.join(", ")
    )]
    NotOwned {
        foreign: Vec<String>,
        remedy: String,
    },
}

/// The commands that clear the admin network so that `yoink up` can
/// create it again. `disconnect` are the containers to take off the
/// network. `remove` are the proxy and the relay: a proxy that stays
/// at spec is not connected again, so yoink must create it again.
fn remedy(disconnect: &[String], remove: &[String]) -> String {
    let mut steps = Vec::new();
    for name in disconnect {
        steps.push(format!(
            "`docker network disconnect {ADMIN_NETWORK} {name}`"
        ));
    }
    if !remove.is_empty() {
        steps.push(format!(
            "`docker rm -f {}` (the routes are down until `yoink up` creates the proxy again)",
            remove.join(" ")
        ));
    }
    steps.push(format!("`docker network rm {ADMIN_NETWORK}`"));
    steps.push("`yoink up`".to_string());
    format!("To fix, run on the host: {}.", steps.join(", then "))
}

/// What [`ensure_admin`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdminNetworkReady {
    /// yoink created the network in this call.
    pub created: bool,
    /// yoink app containers from an earlier version that yoink removed
    /// from the network.
    pub disconnected: Vec<String>,
}

/// Make sure the admin network of the proxy exists and belongs to
/// yoink. The admin API has no authentication, so yoink does not join
/// a network that another tool made:
///
/// - yoink creates the network with [`ADMIN_NETWORK_LABEL`].
/// - A network with that label is yoink's.
/// - Earlier versions of yoink created the network without labels.
///   Docker cannot add a label to a network, so an unlabeled network is
///   accepted only when each container on it is a yoink container. A
///   network without containers is accepted too.
/// - Before v0.21, services joined the network by default. yoink
///   disconnects those app containers from an unlabeled network and
///   reports them in [`AdminNetworkReady::disconnected`]. Only the proxy
///   and the admin relay stay on it.
/// - The deploy probe of the proxy joins the network. yoink removes a
///   stopped probe from an earlier run and ignores a running probe.
/// - An internal network is refused: the relay publishes a port.
pub async fn ensure_admin(
    ops: &dyn DockerOps,
    host: &Host,
) -> Result<AdminNetworkReady, AdminNetworkError> {
    let detail = if let Some(detail) = ops.inspect_network(host, ADMIN_NETWORK).await? {
        detail
    } else {
        let labels: BTreeMap<String, String> = [ADMIN_NETWORK_LABEL]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if ops.create_network(host, ADMIN_NETWORK, &labels).await? {
            return Ok(AdminNetworkReady {
                created: true,
                disconnected: Vec::new(),
            });
        }
        // Another run created it first. Check that one.
        ops.inspect_network(host, ADMIN_NETWORK)
            .await?
            .unwrap_or_default()
    };
    let disconnected = check_admin_network(ops, host, &detail).await?;
    Ok(AdminNetworkReady {
        created: false,
        disconnected,
    })
}

/// Check an existing admin network. Returns the yoink app containers
/// that this call disconnected from an unlabeled network.
async fn check_admin_network(
    ops: &dyn DockerOps,
    host: &Host,
    detail: &NetworkDetail,
) -> Result<Vec<String>, AdminNetworkError> {
    let (key, value) = ADMIN_NETWORK_LABEL;
    let labeled = detail.labels.get(key).map(String::as_str) == Some(value);
    if labeled && !detail.internal {
        return Ok(Vec::new());
    }
    if detail.internal && detail.containers.is_empty() {
        return Err(AdminNetworkError::Internal {
            remedy: remedy(&[], &[]),
        });
    }
    // `detail.containers` comes from `docker network inspect` and has
    // only running containers. Docker connects a stopped container to
    // the network again when it starts, so yoink also lists the
    // containers in each state that are attached to the network.
    let attached = ops.list_containers_on_network(host, ADMIN_NETWORK).await?;
    let mut on_network: Vec<String> = detail.containers.clone();
    for c in &attached {
        if !on_network.contains(&c.name) {
            on_network.push(c.name.clone());
        }
    }
    // The label filter returns stopped containers too.
    let managed = ops
        .list_containers_by_label(host, "yoink.managed=true")
        .await?;
    // The deploy probe of the proxy joins this network. A stopped probe
    // leaked (Ctrl-C, a failed remove): yoink removes it. A running probe
    // belongs to a deploy that removes it when the probe ends.
    for c in managed.iter().filter(|c| is_probe(c)) {
        if !c.is_running() && on_network.contains(&c.name) {
            ops.force_remove_container(host, &c.name).await?;
        }
    }
    on_network.retain(|name| !managed.iter().any(|c| &c.name == name && is_probe(c)));
    let is_admin_peer =
        |c: &ContainerInfo| c.yoink_service.as_deref() == Some(PROXY_SERVICE_NAME) || is_relay(c);
    let (peers, others): (Vec<String>, Vec<String>) = on_network
        .iter()
        .cloned()
        .partition(|name| managed.iter().any(|c| &c.name == name && is_admin_peer(c)));
    if detail.internal {
        return Err(AdminNetworkError::Internal {
            remedy: remedy(&others, &peers),
        });
    }
    let foreign: Vec<String> = others
        .iter()
        .filter(|name| !managed.iter().any(|c| &c.name == *name))
        .cloned()
        .collect();
    if !foreign.is_empty() {
        return Err(AdminNetworkError::NotOwned {
            foreign,
            remedy: remedy(&others, &peers),
        });
    }
    let mut disconnected = Vec::new();
    for c in managed.iter().filter(|c| !is_admin_peer(c) && !is_probe(c)) {
        let attached_here =
            on_network.contains(&c.name) || c.networks.iter().any(|n| n == ADMIN_NETWORK);
        if attached_here {
            ops.disconnect_container_network(host, &c.name, ADMIN_NETWORK)
                .await?;
            disconnected.push(c.name.clone());
        }
    }
    Ok(disconnected)
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
        assert!(ensure_admin(&ops, &host()).await.unwrap().created);
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
        assert_eq!(
            ensure_admin(&ops, &host()).await.unwrap(),
            AdminNetworkReady::default()
        );
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
        ]));
        assert_eq!(
            ensure_admin(&ops, &host()).await.unwrap(),
            AdminNetworkReady::default()
        );

        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&[]));
        ops.push_list_containers(Ok(Vec::new()));
        assert_eq!(
            ensure_admin(&ops, &host()).await.unwrap(),
            AdminNetworkReady::default()
        );
    }

    /// Before v0.21, services joined the admin network by default. A
    /// host from that time has yoink app containers on the unlabeled
    /// network. yoink disconnects them, reports them and keeps the
    /// network. A stopped app container is disconnected too, because
    /// Docker connects it again when it starts.
    #[tokio::test]
    async fn unlabeled_admin_network_drops_old_yoink_app_containers() {
        use crate::proxy::relay::RELAY_NAME;
        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&["yoink-proxy-abc", "api-1"]));
        let mut stopped = container("worker-1", Some("worker"));
        stopped.state = "exited".into();
        let mut elsewhere = container("web-1", Some("web"));
        elsewhere.networks = vec!["yoink-ingress".into()];
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container(RELAY_NAME, None),
            container("api-1", Some("api")),
            stopped,
            elsewhere,
        ]));
        let ready = ensure_admin(&ops, &host()).await.unwrap();
        assert_eq!(
            ready,
            AdminNetworkReady {
                created: false,
                disconnected: vec!["api-1".into(), "worker-1".into()],
            }
        );
        let disconnects: Vec<RecordedCall> = ops
            .calls()
            .into_iter()
            .filter(|c| matches!(c, RecordedCall::DisconnectContainerNetwork(..)))
            .collect();
        assert_eq!(
            disconnects,
            vec![
                RecordedCall::DisconnectContainerNetwork(
                    host(),
                    "api-1".into(),
                    ADMIN_NETWORK.into()
                ),
                RecordedCall::DisconnectContainerNetwork(
                    host(),
                    "worker-1".into(),
                    ADMIN_NETWORK.into()
                ),
            ]
        );
    }

    /// The deploy turns the disconnects into one warning event.
    #[tokio::test]
    async fn host_networks_warn_about_disconnected_app_containers() {
        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&["yoink-proxy-abc", "api-1"]));
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container("api-1", Some("api")),
        ]));
        let mut events = Vec::new();
        crate::deploy::ensure_host_networks(
            &ops,
            &host(),
            &[ADMIN_NETWORK.to_string()],
            &mut |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(
            events,
            vec![
                crate::deploy::DeployEvent::AdminNetworkContainersDisconnected {
                    host: "host-a".into(),
                    containers: vec!["api-1".into()],
                },
                crate::deploy::DeployEvent::NetworkReady {
                    host: "host-a".into(),
                    network: ADMIN_NETWORK.into(),
                    created: false,
                },
            ]
        );
    }

    /// A network of that name from another tool: the proxy and the
    /// relay would share it with containers that can call the admin
    /// API, which has no authentication.
    #[tokio::test]
    async fn unlabeled_admin_network_with_other_containers_is_refused() {
        let ops = FakeDockerOps::new();
        ops.set_network(
            ADMIN_NETWORK,
            unlabeled(&["yoink-proxy-abc", "api-1", "other", "db"]),
        );
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container("api-1", Some("api")),
        ]));
        let err = ensure_admin(&ops, &host()).await.unwrap_err();
        let AdminNetworkError::NotOwned { foreign, .. } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(foreign, &["other", "db"]);
        let msg = err.to_string();
        assert!(msg.contains("not created by yoink"), "got: {msg}");
        assert!(msg.contains("(other, db)"), "got: {msg}");
        // Docker cannot rename a network or remove one in use, so the
        // fix lists commands that Docker accepts, in order.
        assert!(
            msg.ends_with(
                "To fix, run on the host: \
                 `docker network disconnect yoink-proxy-admin api-1`, then \
                 `docker network disconnect yoink-proxy-admin other`, then \
                 `docker network disconnect yoink-proxy-admin db`, then \
                 `docker rm -f yoink-proxy-abc` (the routes are down until `yoink up` \
                 creates the proxy again), then \
                 `docker network rm yoink-proxy-admin`, then `yoink up`."
            ),
            "got: {msg}"
        );
        assert!(!msg.contains("rename"), "got: {msg}");
        // A refused network stays as it is: no yoink container leaves it.
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::DisconnectContainerNetwork(..))),
            "got: {:?}",
            ops.calls()
        );
    }

    /// `docker network inspect` lists only running containers. A
    /// stopped container that yoink does not manage is still attached:
    /// it joins the network again when it starts, so yoink refuses the
    /// network.
    #[tokio::test]
    async fn unlabeled_admin_network_with_a_stopped_foreign_container_is_refused() {
        let ops = FakeDockerOps::new();
        ops.set_network(ADMIN_NETWORK, unlabeled(&["yoink-proxy-abc"]));
        let mut stopped = container("other", None);
        stopped.state = "exited".into();
        ops.set_network_containers(
            ADMIN_NETWORK,
            vec![
                container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
                stopped,
            ],
        );
        ops.push_list_containers(Ok(vec![container(
            "yoink-proxy-abc",
            Some(PROXY_SERVICE_NAME),
        )]));
        let err = ensure_admin(&ops, &host()).await.unwrap_err();
        let AdminNetworkError::NotOwned { foreign, .. } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(foreign, &["other"]);
        assert!(
            ops.calls().contains(&RecordedCall::ListContainersOnNetwork(
                host(),
                ADMIN_NETWORK.into()
            )),
            "got: {:?}",
            ops.calls()
        );
    }

    /// A probe that leaked (Ctrl-C, a failed remove) stays on the
    /// network. yoink removes a stopped one, ignores a running one, and
    /// accepts the network.
    #[tokio::test]
    async fn unlabeled_admin_network_removes_a_leaked_stopped_probe() {
        let ops = FakeDockerOps::new();
        ops.set_network(
            ADMIN_NETWORK,
            unlabeled(&["yoink-proxy-abc", "yoink-probe-yoink-proxy-admin-api-2"]),
        );
        let mut leaked = container("yoink-probe-yoink-proxy-admin-api-1", None);
        leaked.state = "exited".into();
        let running = container("yoink-probe-yoink-proxy-admin-api-2", None);
        ops.set_network_containers(
            ADMIN_NETWORK,
            vec![
                container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
                leaked.clone(),
                running.clone(),
            ],
        );
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            leaked,
            running,
        ]));
        ops.push_force_remove(Ok(()));
        assert_eq!(
            ensure_admin(&ops, &host()).await.unwrap(),
            AdminNetworkReady::default()
        );
        let removes: Vec<RecordedCall> = ops
            .calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    RecordedCall::ForceRemoveContainer(..)
                        | RecordedCall::DisconnectContainerNetwork(..)
                )
            })
            .collect();
        assert_eq!(
            removes,
            vec![RecordedCall::ForceRemoveContainer(
                host(),
                "yoink-probe-yoink-proxy-admin-api-1".into()
            )]
        );
    }

    /// With a foreign container on the network, the error lists only the
    /// foreign container. The remedy does not name a probe: yoink
    /// removed the stopped one, and a running one ends by itself.
    #[tokio::test]
    async fn refused_admin_network_does_not_list_probes() {
        let ops = FakeDockerOps::new();
        let mut leaked = container("yoink-tcp-probe-x-1", None);
        leaked.state = "exited".into();
        ops.set_network(ADMIN_NETWORK, unlabeled(&["yoink-proxy-abc", "other"]));
        ops.set_network_containers(ADMIN_NETWORK, vec![leaked.clone()]);
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            leaked,
        ]));
        ops.push_force_remove(Ok(()));
        let err = ensure_admin(&ops, &host()).await.unwrap_err();
        let AdminNetworkError::NotOwned { foreign, .. } = &err else {
            panic!("got {err:?}");
        };
        assert_eq!(foreign, &["other"]);
        assert!(!err.to_string().contains("probe"), "got: {err}");
    }

    #[tokio::test]
    async fn internal_admin_network_is_refused() {
        use crate::proxy::relay::RELAY_NAME;
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
        assert!(
            matches!(err, AdminNetworkError::Internal { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string()
                .ends_with("`docker network rm yoink-proxy-admin`, then `yoink up`."),
            "got: {err}"
        );

        // With the proxy and the relay on it, both must go first.
        let ops = FakeDockerOps::new();
        ops.set_network(
            ADMIN_NETWORK,
            NetworkDetail {
                internal: true,
                ..unlabeled(&["yoink-proxy-abc", RELAY_NAME])
            },
        );
        ops.push_list_containers(Ok(vec![
            container("yoink-proxy-abc", Some(PROXY_SERVICE_NAME)),
            container(RELAY_NAME, None),
        ]));
        let msg = ensure_admin(&ops, &host()).await.unwrap_err().to_string();
        assert!(
            msg.contains("run on the host: `docker rm -f yoink-proxy-abc yoink-proxy-admin-relay`"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn ensure_propagates_error() {
        let ops = FakeDockerOps::new();
        let err = ensure(&ops, &host(), "yoink").await.unwrap_err();
        assert!(matches!(err, DockerError::FakeExhausted(_)));
    }
}
