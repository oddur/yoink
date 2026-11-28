//! The admin relay: a small Caddy container that gives yoink a path to
//! the Caddy admin API.
//!
//! The admin API of the proxy listens on the unix socket
//! [`ADMIN_SOCKET`], on the volume [`CADDY_CONFIG_VOLUME`]. It has no
//! network address, so no container can reach it over a network, with
//! or without `NET_ADMIN` or `NET_RAW`. The relay mounts the same volume
//! read-only at [`RELAY_CONFIG_MOUNT`] and forwards HTTP from its port
//! [`RELAY_PORT`] to the socket. It joins only [`ADMIN_NETWORK`], a
//! bridge network, because Docker publishes ports only from a network.
//! It publishes `127.0.0.1:0:2020`, and yoink SSH-tunnels to that port.
//!
//! The relay runs the image of the proxy, with the config of
//! [`relay_config`]: its own admin API is off, and it opens a new
//! connection to the socket for each request. A recreated proxy thus
//! needs no relay restart. The proxy image is on each proxy host
//! already, so the relay needs no other image and no other registry.
//!
//! The relay belongs to the implicit proxy deploy: every reconcile with
//! the proxy on makes sure that it runs on each proxy host before the
//! first Caddy push, see `crate::deploy`. It has the label
//! `yoink.managed=true` but no `yoink.service`, so the deploy never
//! takes it for a replica.

use crate::base::host::Host;
use crate::config::{
    ADMIN_NETWORK, ADMIN_SOCKET, CADDY_CONFIG_VOLUME, Config, PROXY_SERVICE_NAME, RunOptions,
};
use crate::docker::{self, RunSpec};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps};

/// Container name of the relay. One relay per host.
pub const RELAY_NAME: &str = crate::config::ADMIN_RELAY_NAME;

/// Label that marks the relay, with the value [`RELAY_ROLE`]. Same key
/// as the unregistry sidecar.
pub const RELAY_ROLE_LABEL: &str = "yoink.role";
/// Value of [`RELAY_ROLE_LABEL`] on the relay.
pub const RELAY_ROLE: &str = "proxy-admin-relay";

/// The port in the relay that Caddy listens on, see
/// [`crate::config::ADMIN_RELAY_PORT`].
pub const RELAY_PORT: u16 = crate::config::ADMIN_RELAY_PORT;

/// Where the relay mounts [`CADDY_CONFIG_VOLUME`]. Not `/config`: the
/// relay keeps its own Caddy config directory on a tmpfs there, and a
/// path that the image does not have makes Docker copy no image files
/// into the volume.
pub const RELAY_CONFIG_MOUNT: &str = "/proxy-config";

/// The admin socket of the proxy, as the relay sees it.
#[must_use]
pub fn relay_socket_path() -> String {
    let in_volume = ADMIN_SOCKET.strip_prefix("/config").unwrap_or(ADMIN_SOCKET);
    format!("{RELAY_CONFIG_MOUNT}{in_volume}")
}

/// The `Host` header that the relay sends to the admin API. Caddy 2.10
/// and later do not check `Host` on a unix socket. Earlier versions
/// accept `127.0.0.1` there, see [`crate::config::admin_block`].
pub const RELAY_UPSTREAM_HOST: &str = "127.0.0.1";

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

/// The image of the relay: the image that the proxy runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayImage {
    pub image: String,
    pub tag: String,
}

impl RelayImage {
    /// The proxy image of `config`, with a `--tag yoink-proxy=…`
    /// override from `tag_overrides`. `None` when the proxy is off.
    #[must_use]
    pub fn of(
        config: &Config,
        tag_overrides: &std::collections::BTreeMap<String, String>,
    ) -> Option<Self> {
        if !config.proxy_enabled() {
            return None;
        }
        let proxy = config.service(PROXY_SERVICE_NAME)?;
        let tag = tag_overrides
            .get(PROXY_SERVICE_NAME)
            .cloned()
            .or_else(|| proxy.tag.clone())
            .unwrap_or_else(|| "latest".to_string());
        Some(Self {
            image: proxy.image.clone(),
            tag,
        })
    }

    /// `image:tag`.
    #[must_use]
    pub fn reference(&self) -> String {
        crate::base::image::image_reference(&self.image, &self.tag)
    }

    /// Split an image reference, as the inspect of a container shows
    /// it: `repo:tag` or `repo@sha256:…`. A reference without a tag
    /// gets `latest`. The inverse of [`Self::reference`].
    #[must_use]
    pub fn parse(reference: &str) -> Self {
        if let Some((image, digest)) = reference.split_once('@') {
            return Self {
                image: image.to_string(),
                tag: digest.to_string(),
            };
        }
        // A `:` before the last `/` is the port of a registry host.
        let name_start = reference.rfind('/').map_or(0, |i| i + 1);
        match reference[name_start..].rfind(':') {
            Some(i) => Self {
                image: reference[..name_start + i].to_string(),
                tag: reference[name_start + i + 1..].to_string(),
            },
            None => Self {
                image: reference.to_string(),
                tag: "latest".to_string(),
            },
        }
    }
}

/// True when a run with `services_filter` deploys the proxy: an
/// unscoped run, or a run with `--service yoink-proxy`. Only such a run
/// replaces a relay at another spec or gets the proxy image.
#[must_use]
pub fn run_selects_proxy(services_filter: Option<&[String]>) -> bool {
    services_filter.is_none_or(|f| f.iter().any(|s| s == PROXY_SERVICE_NAME))
}

/// The image of the running proxy container on `host`, from its
/// inspect. `None` when no proxy container runs there.
pub async fn running_proxy_image(
    ops: &dyn DockerOps,
    host: &Host,
) -> Result<Option<RelayImage>, DockerError> {
    let proxies = ops
        .list_containers_by_label(host, &format!("yoink.service={PROXY_SERVICE_NAME}"))
        .await?;
    let Some(running) = proxies.iter().find(|c| c.is_running()) else {
        return Ok(None);
    };
    let detail = ops.inspect_container(host, &running.name).await?;
    Ok(detail
        .image
        .filter(|i| !i.is_empty())
        .map(|i| RelayImage::parse(&i)))
}

/// The Caddy config of the relay.
///
/// - `admin.disabled`: Caddy opens no admin listener in the relay. The
///   config cannot change at run time; yoink recreates the relay.
/// - `admin.config.persist: false`: no autosave file.
/// - One HTTP server on [`RELAY_PORT`], automatic HTTPS off.
/// - `reverse_proxy` to the admin socket at [`relay_socket_path`], with
///   `Host` [`RELAY_UPSTREAM_HOST`]. Keep-alive is off: each request
///   dials the socket again, so a recreated proxy gets the next request.
///   The relay sets no timeouts besides the default dial timeout, so a
///   long `/load` is not cut. A socket that does not answer gives a 502.
///
/// The string is part of the relay command, and thus of the spec hash.
/// It is written out with a fixed key order, as `bootstrap_config` of
/// the proxy: the key order of a `serde_json::Value` depends on the
/// `preserve_order` feature. A dependency that turns that feature on
/// would otherwise change the hash and recreate the relay on every
/// host.
#[must_use]
pub fn relay_config() -> String {
    format!(
        concat!(
            r#"{{"admin":{{"config":{{"persist":false}},"disabled":true}},"#,
            r#""apps":{{"http":{{"servers":{{"relay":{{"#,
            r#""automatic_https":{{"disable":true}},"#,
            r#""listen":[":{port}"],"#,
            r#""routes":[{{"handle":[{{"handler":"reverse_proxy","#,
            r#""headers":{{"request":{{"set":{{"Host":["{host}"]}}}}}},"#,
            r#""transport":{{"keep_alive":{{"enabled":false}},"protocol":"http"}},"#,
            r#""upstreams":[{{"dial":"unix/{socket}"}}]}}]}}]}}}}}}}}}}"#,
        ),
        port = RELAY_PORT,
        host = RELAY_UPSTREAM_HOST,
        socket = relay_socket_path(),
    )
}

/// The run spec of the relay, without the spec hash label.
fn relay_spec(image: &RelayImage) -> RunSpec {
    let labels = [
        ("yoink.managed".to_string(), "true".to_string()),
        (RELAY_ROLE_LABEL.to_string(), RELAY_ROLE.to_string()),
    ]
    .into_iter()
    .collect();
    // Caddy keeps its state in `XDG_DATA_HOME` and `XDG_CONFIG_HOME`.
    // The official image sets them to `/data` and `/config`. The relay
    // sets them too, for a custom `proxy.image` that does not. A tmpfs
    // on each path gives Caddy writable space on the read-only root
    // file system.
    let env = [
        ("XDG_CONFIG_HOME".to_string(), "/config".to_string()),
        ("XDG_DATA_HOME".to_string(), "/data".to_string()),
    ]
    .into_iter()
    .collect();
    let tmpfs = ["/config", "/data"]
        .into_iter()
        .map(|p| (p.to_string(), "size=16m,mode=1777".to_string()))
        .collect();
    RunSpec {
        image: image.image.clone(),
        tag: image.tag.clone(),
        networks: vec![ADMIN_NETWORK.to_string()],
        container_name: RELAY_NAME.to_string(),
        labels,
        env,
        // The secure default: all capabilities dropped, read-only root
        // file system, no-new-privileges, restart unless-stopped (as
        // the proxy). Port 2020 needs no capability.
        //
        // User 0: Caddy in the proxy runs as root and creates the admin
        // socket with mode 0200, owned by root. A connect to a unix
        // socket needs write permission on the socket file. Without
        // `CAP_DAC_OVERRIDE`, only the owner gets that permission, so
        // the relay must be root too. Root without capabilities cannot
        // override file permissions, change owners, or change its own
        // user. It gets access only where it owns a file: its tmpfs,
        // and the files on the read-only volume. The admin API that it
        // forwards to exposes the same data anyway.
        //
        // The relay adds `NET_BIND_SERVICE` back all the same. The
        // official image sets `setcap cap_net_bind_service=+p` on the
        // caddy binary (caddy-docker `2.11/alpine/Dockerfile`, line
        // 39). A custom `proxy.image` can use `+ep`. Then the kernel
        // refuses the exec when the capability is not in the bounding
        // set. The capability only lets the relay bind a port below
        // 1024 in its own network namespace.
        options: RunOptions {
            tmpfs,
            cap_add: vec!["NET_BIND_SERVICE".to_string()],
            user: Some("0:0".to_string()),
            ..RunOptions::default()
        },
        // The same shape as the proxy bootstrap: the config comes on
        // stdin. `exec` makes Caddy the child of the init process, so
        // it gets the stop signal. The JSON has no single quote.
        entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
        command: vec![format!(
            "exec caddy run --config /dev/stdin <<'EOF'\n{}\nEOF",
            relay_config()
        )],
        publish: vec![format!("127.0.0.1:0:{RELAY_PORT}")],
        binds: Vec::new(),
        // Read-only: a connect to a unix socket needs no write access to
        // the mount. Linux checks only the permission bits of the socket
        // file for a connect (`unix_find_bsd` calls `path_permission`
        // with `MAY_WRITE`), not the read-only flag of the mount. The
        // same is true for `/var/run/docker.sock:ro`.
        volumes: vec![format!("{CADDY_CONFIG_VOLUME}:{RELAY_CONFIG_MOUNT}:ro")],
        docker_healthcheck: None,
    }
}

/// The spec hash of the relay that runs `image`. A new proxy image
/// gives a new hash and recreates the relay once.
#[must_use]
pub fn relay_spec_hash(image: &RelayImage) -> String {
    docker::compute_spec_hash(&relay_spec(image))
}

/// What [`plan`] found on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayPlan {
    /// The relay runs at the current spec.
    Current,
    /// yoink must create the relay. `stale` are the relay containers to
    /// remove first: another spec, or not running.
    Create { stale: Vec<String> },
}

/// Find out whether the relay on `host` runs at the spec for `image`.
/// With `image` `None`, a running relay is current at any spec: a run
/// that does not deploy the proxy keeps it, see [`run_selects_proxy`].
pub async fn plan(
    ops: &dyn DockerOps,
    host: &Host,
    image: Option<&RelayImage>,
) -> Result<RelayPlan, DockerError> {
    let hash = image.map(relay_spec_hash);
    let existing = ops
        .list_containers_by_label(host, &relay_label_filter())
        .await?;
    let current = existing.iter().any(|c| {
        c.name == RELAY_NAME
            && c.is_running()
            && hash
                .as_deref()
                .is_none_or(|h| c.yoink_spec_hash.as_deref() == Some(h))
    });
    if current {
        return Ok(RelayPlan::Current);
    }
    Ok(RelayPlan::Create {
        stale: existing.into_iter().map(|c| c.name).collect(),
    })
}

/// Remove the `stale` relay containers and create the relay for
/// `image`. The caller makes sure that the admin network exists and
/// that `image` is on `host`: the relay never pulls.
pub async fn create(
    ops: &dyn DockerOps,
    host: &Host,
    image: &RelayImage,
    stale: &[String],
) -> Result<(), DockerError> {
    for name in stale {
        ops.force_remove_container(host, name).await?;
    }
    let mut spec = relay_spec(image);
    spec.labels
        .insert("yoink.spec_hash".into(), relay_spec_hash(image));
    let body = docker::build_container(&spec).map_err(|e| DockerError::Invalid(e.to_string()))?;
    ops.create_container(host, RELAY_NAME, body).await?;
    ops.start_container(host, RELAY_NAME).await?;
    Ok(())
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

    fn caddy() -> RelayImage {
        RelayImage {
            image: "caddy".into(),
            tag: "2".into(),
        }
    }

    fn relay(state: &str, hash: &str) -> ContainerInfo {
        ContainerInfo {
            host: "h".into(),
            name: RELAY_NAME.into(),
            image: "caddy:2".into(),
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
    /// gateway and gets the loopback publish. It runs the proxy image
    /// with the hardened defaults, as root so that it can connect to the
    /// admin socket, with the config volume read-only.
    #[test]
    fn relay_body_runs_the_proxy_image_on_the_admin_network() {
        let image = RelayImage {
            image: "ghcr.io/me/caddy-redis".into(),
            tag: "2.8".into(),
        };
        let body = docker::build_container(&relay_spec(&image)).expect("body");
        assert_eq!(body.image.as_deref(), Some("ghcr.io/me/caddy-redis:2.8"));
        assert_eq!(
            body.entrypoint.as_deref(),
            Some(&["sh".to_string(), "-c".to_string()][..])
        );
        let cmd = body.cmd.expect("cmd");
        assert_eq!(cmd.len(), 1);
        assert_eq!(
            cmd[0],
            format!(
                "exec caddy run --config /dev/stdin <<'EOF'\n{}\nEOF",
                relay_config()
            )
        );
        assert!(!cmd[0].contains("socat"));
        let host_config = body.host_config.expect("host config");
        assert_eq!(host_config.network_mode.as_deref(), Some(ADMIN_NETWORK));
        let bindings = host_config.port_bindings.expect("port bindings");
        assert!(!bindings.contains_key("2019/tcp"));
        let relay_port = bindings["2020/tcp"].as_ref().expect("2020 binding");
        assert_eq!(relay_port[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(relay_port[0].host_port.as_deref(), Some("0"));
        assert_eq!(host_config.cap_drop, Some(vec!["ALL".to_string()]));
        // In the bounding set for a custom image with `+ep` file caps.
        assert_eq!(
            host_config.cap_add,
            Some(vec!["NET_BIND_SERVICE".to_string()])
        );
        let security = host_config.security_opt.expect("security opt");
        assert!(security.iter().any(|o| o.starts_with("no-new-privileges")));
        assert_eq!(host_config.readonly_rootfs, Some(true));
        let tmpfs = host_config.tmpfs.expect("tmpfs");
        let mut paths: Vec<&String> = tmpfs.keys().collect();
        paths.sort();
        assert_eq!(paths, ["/config", "/data"]);
        assert_eq!(relay_socket_path(), "/proxy-config/caddy/admin.sock");
        // Root, for the owner match on the root-owned 0200 socket, and
        // no capability besides `NET_BIND_SERVICE`.
        assert_eq!(body.user.as_deref(), Some("0:0"));
        assert_eq!(
            host_config.binds,
            Some(vec!["yoink_caddy_config:/proxy-config:ro".to_string()])
        );
        let env = body.env.expect("env");
        assert!(env.contains(&"XDG_CONFIG_HOME=/config".to_string()));
        assert!(env.contains(&"XDG_DATA_HOME=/data".to_string()));
        let endpoints = body.networking_config.unwrap().endpoints_config.unwrap();
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), [ADMIN_NETWORK]);
        let labels = body.labels.expect("labels");
        assert_eq!(
            labels.get(RELAY_ROLE_LABEL).map(String::as_str),
            Some(RELAY_ROLE)
        );
        assert!(!labels.contains_key("yoink.service"));
    }

    /// The relay config, pinned: no admin listener of its own, one
    /// plain HTTP server on 2020, the admin socket on the read-only
    /// volume as the upstream, a fixed `Host`, no keep-alive and no
    /// timeouts. The string itself is pinned too: it
    /// is part of the spec hash.
    #[test]
    fn relay_config_is_pinned() {
        assert_eq!(
            relay_config(),
            "{\"admin\":{\"config\":{\"persist\":false},\"disabled\":true},\
             \"apps\":{\"http\":{\"servers\":{\"relay\":{\
             \"automatic_https\":{\"disable\":true},\"listen\":[\":2020\"],\
             \"routes\":[{\"handle\":[{\"handler\":\"reverse_proxy\",\
             \"headers\":{\"request\":{\"set\":{\"Host\":[\"127.0.0.1\"]}}},\
             \"transport\":{\"keep_alive\":{\"enabled\":false},\"protocol\":\"http\"},\
             \"upstreams\":[{\"dial\":\"unix//proxy-config/caddy/admin.sock\"}]}]}]}}}}}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&relay_config()).expect("JSON");
        assert_eq!(
            parsed,
            serde_json::json!({
                "admin": { "disabled": true, "config": { "persist": false } },
                "apps": { "http": { "servers": { "relay": {
                    "listen": [":2020"],
                    "automatic_https": { "disable": true },
                    "routes": [{ "handle": [{
                        "handler": "reverse_proxy",
                        "upstreams": [{ "dial": "unix//proxy-config/caddy/admin.sock" }],
                        "headers": { "request": { "set": { "Host": ["127.0.0.1"] } } },
                        "transport": { "protocol": "http", "keep_alive": { "enabled": false } },
                    }] }],
                } } } },
            })
        );
        // The shell command quotes the JSON in a quoted heredoc.
        assert!(!relay_config().contains('\''));
    }

    /// A new proxy image (a new tag, `proxy.image`, an xcaddy build)
    /// recreates the relay.
    #[test]
    fn relay_spec_hash_follows_the_proxy_image() {
        let other_tag = RelayImage {
            tag: "2.9".into(),
            ..caddy()
        };
        let other_image = RelayImage {
            image: "yoink-caddy".into(),
            ..caddy()
        };
        assert_ne!(relay_spec_hash(&caddy()), relay_spec_hash(&other_tag));
        assert_ne!(relay_spec_hash(&caddy()), relay_spec_hash(&other_image));
    }

    #[test]
    fn relay_image_is_the_resolved_proxy_image() {
        let cfg = Config::parse_str(
            "hosts:\n  - { address: h, user: u }\nproxy:\n  image: ghcr.io/me/caddy-redis:2.8\n\
             services:\n  - name: api\n    image: api\n    tag: v1\n    domain: api.example.com\n    \
             run: { port: 8080 }\n",
        )
        .unwrap();
        let none = std::collections::BTreeMap::new();
        assert_eq!(
            RelayImage::of(&cfg, &none),
            Some(RelayImage {
                image: "ghcr.io/me/caddy-redis".into(),
                tag: "2.8".into(),
            })
        );
        let overrides = [(PROXY_SERVICE_NAME.to_string(), "2.9".to_string())].into();
        assert_eq!(RelayImage::of(&cfg, &overrides).unwrap().tag, "2.9");

        let off = Config::parse_str(
            "hosts:\n  - { address: h, user: u }\nservices:\n  - name: api\n    image: api\n    \
             tag: v1\n    run: {}\n",
        )
        .unwrap();
        assert_eq!(RelayImage::of(&off, &none), None);
    }

    #[tokio::test]
    async fn running_relay_at_spec_is_current() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![relay("running", &relay_spec_hash(&caddy()))]));
        assert_eq!(
            plan(&ops, &host(), Some(&caddy())).await.unwrap(),
            RelayPlan::Current
        );
        assert_eq!(
            ops.calls(),
            [RecordedCall::ListContainersByLabel(
                host(),
                "yoink.role=proxy-admin-relay".into()
            )]
        );
    }

    /// A relay with an old spec (such as the socat relay) or a stopped
    /// relay is created again.
    #[tokio::test]
    async fn relay_with_another_spec_or_stopped_is_recreated() {
        for existing in [
            relay("running", "0000"),
            relay("exited", &relay_spec_hash(&caddy())),
        ] {
            let ops = FakeDockerOps::new();
            ops.push_list_containers(Ok(vec![existing]));
            let RelayPlan::Create { stale } = plan(&ops, &host(), Some(&caddy())).await.unwrap()
            else {
                panic!("expected Create");
            };
            assert_eq!(stale, [RELAY_NAME]);
            ops.push_force_remove(Ok(()));
            ops.push_create_container(Ok("id".into()));
            ops.push_start_container(Ok(()));
            create(&ops, &host(), &caddy(), &stale).await.unwrap();
            let calls = ops.calls();
            assert_eq!(
                calls[1..],
                [
                    RecordedCall::ForceRemoveContainer(host(), RELAY_NAME.into()),
                    RecordedCall::CreateContainer(host(), RELAY_NAME.into()),
                    RecordedCall::StartContainer(host(), RELAY_NAME.into()),
                ]
            );
        }
    }

    /// Without an image, a running relay is current at any spec. A
    /// stopped or missing relay is still created.
    #[tokio::test]
    async fn plan_without_image_keeps_a_running_relay() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![relay("running", "0000")]));
        assert_eq!(plan(&ops, &host(), None).await.unwrap(), RelayPlan::Current);
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![relay("exited", "0000")]));
        assert_eq!(
            plan(&ops, &host(), None).await.unwrap(),
            RelayPlan::Create {
                stale: vec![RELAY_NAME.into()]
            }
        );
    }

    #[test]
    fn relay_image_parse_is_the_inverse_of_reference() {
        for (reference, image, tag) in [
            ("caddy:2", "caddy", "2"),
            ("caddy", "caddy", "latest"),
            ("localhost:5000/caddy", "localhost:5000/caddy", "latest"),
            (
                "localhost:5000/me/caddy:2.8",
                "localhost:5000/me/caddy",
                "2.8",
            ),
            ("caddy@sha256:abc", "caddy", "sha256:abc"),
        ] {
            let parsed = RelayImage::parse(reference);
            assert_eq!((parsed.image.as_str(), parsed.tag.as_str()), (image, tag));
        }
        assert_eq!(
            RelayImage::parse("ghcr.io/a/b:1").reference(),
            "ghcr.io/a/b:1"
        );
    }

    #[test]
    fn only_an_unscoped_or_proxy_run_selects_the_proxy() {
        assert!(run_selects_proxy(None));
        assert!(run_selects_proxy(Some(&[
            "api".to_string(),
            PROXY_SERVICE_NAME.to_string()
        ])));
        assert!(!run_selects_proxy(Some(&["api".to_string()])));
    }

    /// `create` never pulls: the deploy makes the proxy image present
    /// first, by the same path as for the proxy.
    #[tokio::test]
    async fn create_does_not_pull() {
        let ops = FakeDockerOps::new();
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        create(&ops, &host(), &caddy(), &[]).await.unwrap();
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(..))),
            "got: {:?}",
            ops.calls()
        );
    }
}
