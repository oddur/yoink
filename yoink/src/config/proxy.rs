//! The implicit reverse proxy, as a config normalisation step. When
//! any service declares `domain:`, loading the config synthesises the
//! `yoink-proxy` service, declares the ingress and admin networks, and
//! wires every routed service to them. The Docker-side work (Caddy
//! JSON, admin API pushes, xcaddy builds) is in `crate::proxy`.

mod plugins;

use super::{
    Config, ConfigError, ProxyConfig, ProxyTls, ServiceConfig, ServiceKind, ServiceRun, TlsMode,
    XcaddyConfig,
};

/// The internal service name yoink uses for the implicit reverse
/// proxy. Users may not declare a service with this name when they
/// also use `domain:` — that's a collision error.
pub const PROXY_SERVICE_NAME: &str = "yoink-proxy";

/// Docker network the proxy uses to reach upstream containers by
/// name. Every service with `domain:` joins it automatically.
pub const INGRESS_NETWORK: &str = "yoink-ingress";

/// Docker network of the admin relay (see `crate::proxy::relay`). The
/// relay needs a bridge network to publish its port on the host
/// loopback. Only the relay joins it. The proxy does not: the Caddy
/// admin API listens on [`ADMIN_SOCKET`], not on a network address.
/// yoink v0.22 and earlier put the proxy (and before v0.21, every
/// service) on this network too.
pub const ADMIN_NETWORK: &str = "yoink-proxy-admin";

/// Container name of the admin relay (see `crate::proxy::relay`). One
/// relay per host.
pub const ADMIN_RELAY_NAME: &str = "yoink-proxy-admin-relay";

/// The port in the relay that its Caddy listens on. The relay publishes
/// it on `127.0.0.1` of the host at an ephemeral port. It is not 2019,
/// so that a `ssh -L` to the relay and a `ssh -L` to an old proxy
/// cannot be mixed up.
pub const ADMIN_RELAY_PORT: u16 = 2020;

/// The admin port that the proxy of yoink v0.22 and earlier publishes
/// on `127.0.0.1:0`. Caddy in such a proxy listens on `0.0.0.0:2019`.
/// yoink finds an old proxy by this published port and moves its
/// admin API to [`ADMIN_SOCKET`] with the first push, see
/// `crate::proxy::admin`.
pub const LEGACY_ADMIN_PORT: u16 = 2019;

/// The named volume that holds the Caddy config directory of the
/// proxy: `autosave.json` and the admin socket. The proxy mounts it at
/// `/config`, the admin relay mounts it read-only. No other container
/// mounts it.
pub const CADDY_CONFIG_VOLUME: &str = "yoink_caddy_config";

/// The unix socket of the Caddy admin API, in the proxy container. The
/// path is on [`CADDY_CONFIG_VOLUME`]. Caddy creates the socket with
/// mode 0200 and the owner of the Caddy process (root in the proxy).
/// No container has a network path to the admin API: only a process
/// that sees this file and runs as root can connect to it.
pub const ADMIN_SOCKET: &str = "/config/caddy/admin.sock";

/// The `Host` header of each request to the admin API: the admin relay
/// sets it, and [`admin_block`] allows it.
pub const ADMIN_HOST: &str = "127.0.0.1";

/// The `admin` block of every Caddy config that yoink writes: the
/// bootstrap of the proxy container and each `/load`. A `/load` that
/// omits the block sets the admin API back to the Caddy default
/// `localhost:2019`, which the admin relay cannot reach.
///
/// `origins` is [`ADMIN_HOST`], the `Host` that the relay sends. Caddy
/// checks `Host` on a unix socket up to v2.9 (`enforceHost` is
/// `!addr.isWildcardInterface()`, true for a socket path). Without
/// `origins`, v2.6 allows only an empty `Host` there, and v2.7 to v2.9
/// allow `""`, `127.0.0.1` and `::1`. v2.10 and later do not check
/// `Host` on a unix socket. Thus `127.0.0.1` without `origins` fails on
/// v2.6 with a 403. With `origins` set, every version from v2.6 allows
/// exactly that `Host`. On v2.11, `origins` also sets the allowed
/// `Origin` of a request that has one. The block has no
/// `enforce_origin`: yoink sends no `Origin` header.
#[must_use]
pub fn admin_block() -> serde_json::Value {
    serde_json::json!({
        "listen": format!("unix/{ADMIN_SOCKET}"),
        "origins": [ADMIN_HOST],
    })
}

impl Config {
    /// True when the proxy should be enabled for this config — either
    /// explicitly (`proxy.enabled: true`) or implicitly (any service
    /// has `domain:`).
    #[must_use]
    pub fn proxy_enabled(&self) -> bool {
        if let Some(p) = &self.proxy
            && let Some(explicit) = p.enabled
        {
            return explicit;
        }
        self.services.iter().any(ServiceConfig::is_proxied)
    }

    /// The managed Caddy build (`proxy.xcaddy:`), when configured.
    #[must_use]
    pub fn xcaddy(&self) -> Option<&XcaddyConfig> {
        self.proxy.as_ref().and_then(|p| p.xcaddy.as_ref())
    }

    /// The ACME registration email (`proxy.email:`), when set.
    #[must_use]
    pub fn proxy_email(&self) -> Option<&str> {
        self.proxy.as_ref().and_then(|p| p.email.as_deref())
    }

    /// The proxy-level TLS block (`proxy.tls:`), when set.
    #[must_use]
    pub fn proxy_tls(&self) -> Option<&ProxyTls> {
        self.proxy.as_ref().and_then(|p| p.tls.as_ref())
    }

    /// True when the proxy asks Let's Encrypt for certs: the proxy is
    /// enabled, a routed service uses `tls: auto`, and no proxy-level
    /// `proxy.tls` cert covers it. ACME then needs `proxy.email:`.
    /// With the proxy off, yoink renders no Caddy config at all.
    #[must_use]
    pub fn proxy_uses_acme(&self) -> bool {
        self.proxy_enabled()
            && self.proxy_tls().is_none()
            && self
                .proxied_services()
                .iter()
                .any(|s| matches!(s.tls, TlsMode::Auto))
    }

    /// The Caddy image the proxy runs: `proxy.image:`, the xcaddy
    /// tag, or the default `caddy:2`.
    #[must_use]
    pub fn proxy_image(&self) -> String {
        self.proxy.as_ref().map_or_else(
            || super::CADDY_DEFAULT_IMAGE.to_string(),
            ProxyConfig::resolved_image,
        )
    }

    /// The raw `proxy.global_handlers:` and `proxy.global_handlers_after:`
    /// snippets. Both empty without a `proxy:` block.
    #[must_use]
    pub fn proxy_global_handlers(&self) -> (&[String], &[String]) {
        self.proxy
            .as_ref()
            .map_or((&[] as &[String], &[] as &[String]), |p| {
                (
                    p.global_handlers.as_slice(),
                    p.global_handlers_after.as_slice(),
                )
            })
    }

    /// The raw `proxy.config_extra:` snippet, when set.
    #[must_use]
    pub fn proxy_config_extra(&self) -> Option<&str> {
        self.proxy.as_ref().and_then(|p| p.config_extra.as_deref())
    }

    /// Synthesize the `_proxy` service, ensure ingress/admin networks
    /// exist in `deploy.networks`, and add `_proxy` to every routed
    /// service's `depends_on:` list. Idempotent — safe to call after
    /// validation has already added the proxy.
    #[allow(clippy::too_many_lines)] // mostly straight-line validation
    pub(crate) fn inject_implicit_proxy(&mut self) -> Result<(), ConfigError> {
        self.reject_admin_network()?;
        self.reject_caddy_config_volume()?;
        if !self.proxy_enabled() {
            return Ok(());
        }

        if let Some(p) = self.proxy.as_ref() {
            p.validate(&self.services)?;
        }

        // Validation: any service using `tls: cert` must name both
        // `tls_cert_secret` and `tls_key_secret`, or name neither and
        // inherit the global `proxy.tls` cert; reject early so the
        // operator gets a config-time error rather than a deploy-time one.
        let has_global_cert = self.proxy_tls().is_some();
        for svc in &self.services {
            if svc.domain.is_some() && svc.run.port.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?} has `domain:` but no `run.port` — the proxy needs to know \
                     which container port to forward to",
                    svc.name,
                )));
            }
            let own_cert = (svc.tls_cert_secret.is_some(), svc.tls_key_secret.is_some());
            let cert_ok = match own_cert {
                (true, true) => true,
                (false, false) => has_global_cert,
                _ => false,
            };
            if matches!(svc.tls, TlsMode::Cert) && !cert_ok {
                return Err(ConfigError::Invalid(format!(
                    "service {:?} sets `tls: cert` but is missing `tls_cert_secret` and/or \
                     `tls_key_secret` — both are required for the inline-cert path \
                     (or leave both unset and set `proxy.tls` to use the global cert)",
                    svc.name,
                )));
            }
        }

        // path_prefix disambiguation: when N services share a hostname,
        // at most one can be the catch-all (no path_prefix); the rest must
        // declare a path_prefix. Otherwise route order is operator-
        // dependent and bugs are subtle.
        let mut by_host: std::collections::BTreeMap<String, Vec<&ServiceConfig>> =
            std::collections::BTreeMap::new();
        for svc in &self.services {
            let Some(domain) = &svc.domain else { continue };
            for host in domain.as_list() {
                by_host.entry(host).or_default().push(svc);
            }
        }
        for (host, services) in &by_host {
            if services.len() < 2 {
                continue;
            }
            let catch_alls: Vec<&str> = services
                .iter()
                .filter(|s| s.path_prefix.is_none())
                .map(|s| s.name.as_str())
                .collect();
            if catch_alls.len() > 1 {
                return Err(ConfigError::Invalid(format!(
                    "{} services route {host:?} without a path_prefix \
                     (services: {catch_alls:?}) — at most one catch-all per host; \
                     add `path_prefix:` to the others",
                    catch_alls.len(),
                )));
            }
        }

        // ACME-email validation moved out of load-time. The proxy renderer
        // (`proxy::caddy::render`) checks the same condition when it
        // actually emits the ACME policy and bails with the same operator-
        // facing message. Keeping the check here too forced load failures
        // on commands that don't render Caddy (`secrets edit`, `hosts add`,
        // …), which broke the bootstrap-then-add-email flow.

        // Refuse the user-defined `_proxy` collision case. Safer than
        // silently overriding because the user's definition probably
        // wouldn't have the right networks/admin-port wiring anyway.
        if self
            .services
            .iter()
            .any(|s| s.name == PROXY_SERVICE_NAME && !matches!(s.kind, Some(ServiceKind::Proxy)))
        {
            return Err(ConfigError::Invalid(format!(
                "service name {PROXY_SERVICE_NAME:?} is reserved for the implicit reverse \
                 proxy — rename your service or remove `domain:` from the routed services",
            )));
        }

        // Pin every service that has no `networks:` to the networks the
        // operator declared, before the implicit ones are added below. A
        // service without `networks:` defaults to all of `deploy.networks`,
        // so without this pin it would also join `yoink-proxy-admin` and
        // could reach the admin relay, which forwards each request to the
        // Caddy admin API. The admin API has no authentication. Only the
        // relay belongs on that network. An operator who lists
        // `yoink-ingress` in `deploy.networks` still gets it here.
        // `reject_admin_network` refuses `yoink-proxy-admin` there.
        let operator_networks = self.deploy.networks.clone();
        for svc in &mut self.services {
            if svc.networks.is_none() && !matches!(svc.kind, Some(ServiceKind::Proxy)) {
                svc.networks = Some(operator_networks.clone());
            }
        }

        // Ensure both implicit networks are declared, so they are created
        // on each host. A user can also declare `yoink-ingress`
        // explicitly (e.g. to attach extra services to it); idempotent.
        for net in [INGRESS_NETWORK, ADMIN_NETWORK] {
            if !self.deploy.networks.iter().any(|n| n == net) {
                self.deploy.networks.push(net.to_string());
            }
        }

        // Every routed service joins the ingress network. Preserve any
        // tier networks the operator already declared.
        for svc in &mut self.services {
            if !svc.is_proxied() {
                continue;
            }
            let mut nets = svc
                .networks
                .clone()
                .unwrap_or_else(|| operator_networks.clone());
            if !nets.iter().any(|n| n == INGRESS_NETWORK) {
                nets.push(INGRESS_NETWORK.to_string());
            }
            svc.networks = Some(nets);
            if !svc.depends_on.iter().any(|d| d == PROXY_SERVICE_NAME) {
                svc.depends_on.push(PROXY_SERVICE_NAME.to_string());
            }
        }

        // Add the synthesized proxy service if not already present
        // (idempotent for tests that round-trip Config → render → re-parse).
        if self.service(PROXY_SERVICE_NAME).is_none() {
            let proxy_cfg = self.proxy.clone().unwrap_or_default();
            self.services.push(synthesized_proxy_service(&proxy_cfg));
        }
        Ok(())
    }
}

impl Config {
    /// Only the admin relay may join [`ADMIN_NETWORK`]. The relay
    /// forwards each request to the Caddy admin API, which has no
    /// authentication, so any other container on that network can
    /// change the proxy config. Rejects the name in `deploy.networks`
    /// and in the `networks:` of every service that is not the proxy.
    /// A second call after [`Self::inject_implicit_proxy`] skips
    /// `deploy.networks`, where the injection put the name.
    fn reject_admin_network(&self) -> Result<(), ConfigError> {
        let reserved = |field: &str| {
            ConfigError::Invalid(format!(
                "{field} lists {ADMIN_NETWORK:?}. This network is reserved for the \
                 admin relay of the proxy. Use a different network name."
            ))
        };
        let injected = self
            .services
            .iter()
            .any(|s| s.kind == Some(ServiceKind::Proxy));
        if !injected && self.deploy.networks.iter().any(|n| n == ADMIN_NETWORK) {
            return Err(reserved("deploy.networks"));
        }
        for svc in &self.services {
            if svc.kind != Some(ServiceKind::Proxy)
                && svc.networks.iter().flatten().any(|n| n == ADMIN_NETWORK)
            {
                return Err(reserved(&format!("service {:?}: networks", svc.name)));
            }
            // The deploy probe of the proxy calls the relay by its
            // name. No other container answers to that name.
            if svc
                .run
                .options
                .network_aliases
                .iter()
                .any(|a| a == ADMIN_RELAY_NAME)
            {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}: run.options.network_aliases lists {ADMIN_RELAY_NAME:?}. \
                     This name is reserved for the admin relay of the proxy. Use a \
                     different alias.",
                    svc.name
                )));
            }
        }
        Ok(())
    }
}

impl Config {
    /// Only the proxy and the admin relay may mount
    /// [`CADDY_CONFIG_VOLUME`]. It holds the admin socket: a container
    /// that mounts it and runs as root can change the proxy config.
    /// Rejects the volume as the source of a `run.volumes` or
    /// `run.binds` entry of every service that is not the proxy, and as
    /// `proxy.cert_volume`. Hooks have no mounts.
    fn reject_caddy_config_volume(&self) -> Result<(), ConfigError> {
        let reserved = |field: &str| {
            ConfigError::Invalid(format!(
                "{field} uses the volume {CADDY_CONFIG_VOLUME:?}. This volume is reserved \
                 for the admin socket of the proxy. Use a different volume name."
            ))
        };
        if self
            .proxy
            .as_ref()
            .and_then(|p| p.cert_volume.as_deref())
            .is_some_and(|v| v == CADDY_CONFIG_VOLUME)
        {
            return Err(reserved("proxy.cert_volume"));
        }
        for svc in &self.services {
            if svc.kind == Some(ServiceKind::Proxy) {
                continue;
            }
            for (field, mounts) in [
                ("run.volumes", &svc.run.volumes),
                ("run.binds", &svc.run.binds),
            ] {
                if mounts
                    .iter()
                    .any(|m| mount_source(m) == CADDY_CONFIG_VOLUME)
                {
                    return Err(reserved(&format!("service {:?}: {field}", svc.name)));
                }
            }
        }
        Ok(())
    }
}

/// The source of a docker-cli style mount `source:target[:options]`.
fn mount_source(mount: &str) -> &str {
    mount
        .split_once(':')
        .map_or(mount, |(source, _)| source)
        .trim()
}

impl ProxyConfig {
    /// Check the `proxy:` block on its own: JSON shapes of the raw
    /// Caddy snippets, reserved `config_extra` paths, and the
    /// `xcaddy:` build. `services` supplies the per-service
    /// `caddy_extra_json:` snippets for the missing-plugin warnings.
    fn validate(&self, services: &[ServiceConfig]) -> Result<(), ConfigError> {
        if let Some(extra) = self.config_extra.as_deref() {
            let parsed = validate_json_shape(extra, "proxy.config_extra", JsonShape::ObjectOnly)?;
            reject_reserved_config_extra_paths(&parsed)?;
        }
        // Parsed JSON for each handler-shaped field, kept so the
        // missing-plugin warning pass below can walk them without a
        // second parse round-trip.
        let mut parsed_handler_snippets: Vec<(String, serde_json::Value)> = Vec::new();
        for (i, raw) in self.global_handlers.iter().enumerate() {
            let label = format!("proxy.global_handlers[{i}]");
            let parsed = validate_json_shape(raw, &label, JsonShape::ObjectOrArray)?;
            parsed_handler_snippets.push((label, parsed));
        }
        for (i, raw) in self.global_handlers_after.iter().enumerate() {
            let label = format!("proxy.global_handlers_after[{i}]");
            let parsed = validate_json_shape(raw, &label, JsonShape::ObjectOrArray)?;
            parsed_handler_snippets.push((label, parsed));
        }
        // Per-service `caddy_extra_json:` is parsed lazily by the
        // renderer; for the missing-plugin check we parse here too.
        // Errors at this layer are best-effort (the renderer surfaces
        // shape errors with full context); skip silently on parse fail.
        for svc in services {
            if let Some(extra) = svc.caddy_extra_json.as_deref()
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(extra)
            {
                let label = format!("service {:?}: caddy_extra_json", svc.name);
                parsed_handler_snippets.push((label, parsed));
            }
        }
        if let Some(x) = &self.xcaddy {
            x.validate(self.image.is_some(), &parsed_handler_snippets)?;
        }
        Ok(())
    }
}

impl XcaddyConfig {
    /// Check the `proxy.xcaddy:` block. `image_is_set` is whether
    /// `proxy.image:` is also set, which is an error. Warns about
    /// unpinned plugins and about handlers in `snippets` whose plugin
    /// is missing.
    fn validate(
        &self,
        image_is_set: bool,
        snippets: &[(String, serde_json::Value)],
    ) -> Result<(), ConfigError> {
        for warning in unpinned_plugin_warnings(&self.plugins) {
            tracing::warn!(target: "yoink::proxy::xcaddy", "{warning}");
        }
        if image_is_set {
            return Err(ConfigError::Invalid(
                "proxy.image and proxy.xcaddy are mutually exclusive — `image:` is the \
                 bring-your-own-image escape hatch; `xcaddy:` is the managed-build path. \
                 Pick one."
                    .to_string(),
            ));
        }
        if self.plugins.is_empty() {
            return Err(ConfigError::Invalid(
                "proxy.xcaddy.plugins is empty — set at least one plugin or remove the \
                 `xcaddy:` block to use vanilla `caddy:2`"
                    .to_string(),
            ));
        }
        for plugin in &self.plugins {
            if !is_plausible_go_module(plugin) {
                return Err(ConfigError::Invalid(format!(
                    "proxy.xcaddy.plugins entry {plugin:?} doesn't look like a Go module \
                     path (expected `<host>/<owner>/<repo>` or \
                     `<host>/<owner>/<repo>@<version>`)"
                )));
            }
        }
        for (label, snippet) in snippets {
            for warning in plugins::missing_plugin_warnings(snippet, &self.plugins, label) {
                tracing::warn!(target: "yoink::proxy::xcaddy", "{warning}");
            }
        }
        Ok(())
    }
}

impl ServiceConfig {
    /// True when this service should be routed by the proxy.
    #[must_use]
    pub fn is_proxied(&self) -> bool {
        self.domain.is_some()
    }
}

fn synthesized_proxy_service(p: &ProxyConfig) -> ServiceConfig {
    // Split `image[:tag]` into the two fields yoink expects. Yoink's
    // normalize_image_references only splits on `@` for digest pinning,
    // not on `:` for tag — that pass runs before we synthesize, so we
    // do the split inline here. Defaults to `caddy:2` when unset.
    let raw = p.resolved_image();
    let (image, tag) = match raw.rsplit_once(':') {
        // Reject `host:port/repo` (port-bearing registries) by checking
        // the suffix doesn't contain `/`. For the registry case the
        // operator passes the full ref including tag, e.g.
        // `ghcr.io/me/caddy-redis:2.7`.
        Some((repo, t)) if !t.contains('/') => (repo.to_string(), Some(t.to_string())),
        _ => (raw, None),
    };
    ServiceConfig {
        name: PROXY_SERVICE_NAME.to_string(),
        image,
        description: None,
        kind: Some(ServiceKind::Proxy),
        build: None,
        tag,
        hosts: None,
        env: std::collections::BTreeMap::default(),
        secrets: Vec::new(),
        env_from_secrets: std::collections::BTreeMap::default(),
        labels: std::collections::BTreeMap::default(),
        pre_deploy: Vec::new(),
        depends_on: Vec::new(),
        // Only `yoink-ingress`: the admin API is on a unix socket, so
        // the proxy needs no admin network. Docker forwards the
        // published ports 80 and 443 to this network.
        networks: Some(vec![INGRESS_NETWORK.to_string()]),
        domain: None,
        tls: TlsMode::Auto,
        tls_cert_secret: None,
        tls_key_secret: None,
        caddy_extra_json: None,
        caddy_extra_caddyfile: None,
        upstream_h2c: false,
        canonical_domain: None,
        compression: false,
        path_prefix: None,
        hsts: false, // proxy itself doesn't serve TLS; this field is irrelevant
        run: ServiceRun {
            // The deploy healthcheck probes Caddy's admin endpoint
            // (`/config/`) through the admin relay, on the admin
            // network: the relay port, not a port of the proxy. That is
            // the path of every later `/load`. Eliminates the race where
            // yoink-proxy is "started" per docker but Caddy's admin API
            // hasn't bound yet — without this gate, the first /load can
            // arrive before admin is up. See `deploy::wait_until_healthy`.
            port: Some(ADMIN_RELAY_PORT),
            healthcheck_path: Some("/config/".to_string()),
            // Caddy serves :80 + :443 on the host. The admin API has no
            // port: it listens on `ADMIN_SOCKET`. The admin relay
            // publishes a port on 127.0.0.1 only, see
            // `crate::proxy::relay`.
            //
            // `proxy.bind:` (when set) prepends the bind address to
            // :80/:443 so the proxy is reachable only on that
            // interface (e.g. tailnet IP).
            //
            // The :80/:443 publishes also implicitly force stop-first
            // deploy (port bindings are exclusive at the kernel
            // level), so a proxy redeploy briefly drops :80/:443
            // until the new container binds.
            publish: {
                let prefix = p
                    .bind
                    .as_deref()
                    .map(|ip| format!("{ip}:"))
                    .unwrap_or_default();
                vec![format!("{prefix}80:80"), format!("{prefix}443:443")]
            },
            // Override the official caddy:2 image's entrypoint to
            // start with a minimal bootstrap config (written to
            // `/config/caddy/bootstrap.json`): the `admin_block`
            // that every `/load` also sends. The admin API listens on
            // the unix socket `ADMIN_SOCKET`. The actual routes are
            // pushed via `/load` immediately after the container is
            // healthy.
            //
            // `--resume` makes Caddy use its autosaved config
            // (`autosave.json` on the `yoink_caddy_config` volume)
            // instead of the bootstrap. Thus the proxy serves the
            // last-pushed routes after a host reboot, before the next
            // `yoink up`. See `start_script` for when the script adds
            // `--resume`.
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: vec![start_script()],
            // Yoink's `cap_drop: [ALL]` default would block Caddy
            // from binding :80/:443 (privileged ports). Re-add the
            // one cap that lets it; everything else stays dropped.
            options: super::RunOptions {
                cap_drop: vec!["ALL".to_string()],
                cap_add: vec!["NET_BIND_SERVICE".to_string()],
                // Caddy's official image runs as root and writes ACME
                // state to /data as root — overriding to a non-root
                // user would lose write access to the cert volume.
                user: Some("0:0".to_string()),
                ..super::RunOptions::default()
            },
            // ACME state, certs, OCSP staples — survives container
            // recreation. Volume name comes from `proxy.cert_volume`
            // (default `yoink_caddy_data`). A second volume holds
            // Caddy's autosave config so it can resume on restart.
            volumes: vec![
                format!("{}:/data", p.resolved_cert_volume()),
                format!("{CADDY_CONFIG_VOLUME}:/config"),
            ],
            ..ServiceRun::default()
        },
    }
}

/// [`admin_block`] as compact JSON, with the keys in sorted order: the
/// form that Go's `json.Marshal` gives, and thus the form in Caddy's
/// `autosave.json`. The string is part of the proxy `cmd`, and thus of
/// the spec hash. It is written out with a fixed key order, because the
/// key order of a `serde_json::Value` depends on the `preserve_order`
/// feature. A dependency that turns that feature on would otherwise
/// change the hash and recreate the proxy on every host.
fn admin_block_json() -> String {
    format!(r#"{{"listen":"unix/{ADMIN_SOCKET}","origins":["{ADMIN_HOST}"]}}"#)
}

/// The bootstrap Caddy config of the proxy: only [`admin_block`].
fn bootstrap_config() -> String {
    format!(r#"{{"admin":{}}}"#, admin_block_json())
}

/// The start of an `autosave.json` whose admin API is on the socket.
/// Caddy writes the file as compact JSON with sorted keys, so `admin`
/// is the first key. The `,` after the block makes sure that the block
/// is complete.
fn socket_autosave_prefix() -> String {
    format!(r#"{{"admin":{},"#, admin_block_json())
}

/// The `sh` script that starts Caddy in the proxy container. It uses
/// only `sh` built-ins besides `mkdir` and `rm`.
///
/// - `mkdir -p`: Caddy creates `/config/caddy` only when it writes the
///   first autosave, after the admin API is up. The official image
///   ships the directory, and Docker copies it into a new volume. A
///   custom image can lack it, and then the admin socket cannot bind.
/// - `rm -f` of the admin socket: Caddy 2.6.3 and earlier do not remove
///   an old socket file before they bind. After an unclean stop (a
///   `SIGKILL`, a host crash), the file stays on the volume, and the
///   bind fails with "address already in use". The removal is safe: a
///   proxy deploy stops the old proxy first, so no other Caddy uses
///   the socket.
/// - `--resume` only when the admin block in `autosave.json` is the
///   socket block. The autosave of a yoink v0.22 proxy has the admin
///   API on `0.0.0.0:2019`. A proxy that resumes it has no admin socket,
///   fails its healthcheck and is never ready. Thus the script starts
///   from the bootstrap: the admin API is on the socket, and the push
///   after the healthcheck adds the routes. Until that push, the proxy
///   has no routes.
/// - The bootstrap goes to `bootstrap.json` in the config directory, on
///   the volume. The root file system is read-only and has no `/tmp`,
///   and a heredoc needs a temporary file in some shells (bash before
///   5.1, zsh, mksh). `printf` is a built-in. With `--resume`, Caddy
///   uses `autosave.json` when it exists, and the file only when not.
/// - `exec`: Caddy replaces the shell, so it gets the stop signal. In
///   `echo … | exec caddy`, Caddy runs in a subshell of the pipeline,
///   and the shell gets the signal. The bootstrap and the prefix are in
///   single quotes and have no single quote.
fn start_script() -> String {
    start_script_in(admin_socket_dir())
}

/// [`start_script`] with the Caddy config directory `dir`. The tests
/// run it in a temporary directory.
fn start_script_in(dir: &str) -> String {
    format!(
        concat!(
            "mkdir -p {dir} || exit 1\n",
            "head=\n",
            "{{ IFS= read -r head < {dir}/autosave.json || :; }} 2>/dev/null\n",
            "case $head in\n",
            "'{prefix}'*) set -- --resume ;;\n",
            "*) set -- ;;\n",
            "esac\n",
            "rm -f {dir}/{socket}\n",
            "printf '%s' '{bootstrap}' > {dir}/bootstrap.json || exit 1\n",
            "exec caddy run --config {dir}/bootstrap.json \"$@\"",
        ),
        dir = dir,
        socket = admin_socket_name(),
        prefix = socket_autosave_prefix(),
        bootstrap = bootstrap_config(),
    )
}

/// The directory of [`ADMIN_SOCKET`]: `$XDG_CONFIG_HOME/caddy` in the
/// proxy container, the directory of Caddy's `autosave.json` too.
fn admin_socket_dir() -> &'static str {
    ADMIN_SOCKET
        .rsplit_once('/')
        .map_or(ADMIN_SOCKET, |(dir, _)| dir)
}

/// The file name of [`ADMIN_SOCKET`].
fn admin_socket_name() -> &'static str {
    ADMIN_SOCKET
        .rsplit_once('/')
        .map_or(ADMIN_SOCKET, |(_, name)| name)
}

/// JSON shapes accepted by `validate_json_shape`.
#[derive(Clone, Copy)]
enum JsonShape {
    /// Top-level Caddy config block (`apps`, `storage`, ...).
    ObjectOnly,
    /// A single handler / route, OR a list of either.
    ObjectOrArray,
}

/// Verify a raw-JSON config value parses and matches the expected
/// top-level shape. Used by both `proxy.config_extra` and
/// `proxy.global_handlers[i]` validation; `label` becomes the
/// human-readable prefix in error messages.
fn validate_json_shape(
    raw: &str,
    label: &str,
    shape: JsonShape,
) -> Result<serde_json::Value, ConfigError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| ConfigError::Invalid(format!("{label} is not valid JSON: {e}")))?;
    let ok = match shape {
        JsonShape::ObjectOnly => matches!(value, serde_json::Value::Object(_)),
        JsonShape::ObjectOrArray => matches!(
            value,
            serde_json::Value::Object(_) | serde_json::Value::Array(_)
        ),
    };
    if ok {
        return Ok(value);
    }
    Err(ConfigError::Invalid(match shape {
        JsonShape::ObjectOnly => format!(
            "{label} must be a JSON object (top-level Caddy config keys like \
             `apps`, `storage`)"
        ),
        JsonShape::ObjectOrArray => {
            format!("{label} must be a JSON object (handler) or array of handlers/routes")
        }
    }))
}

/// Top-level Caddy config keys that yoink renders itself and that
/// would silently disappear if a user wrote them into `proxy.config_extra:`.
/// `deep_merge` replaces arrays wholesale (Caddy config doesn't have a
/// concept of array merging), so a top-level write to either of these
/// paths would clobber yoink's output. The two routes-shaped slots are
/// the dangerous ones — `routes` would wipe every service yoink rendered,
/// and `tls_connection_policies` would silently disable mTLS configured
/// via `proxy.tls.client_auth:` (a security regression). Other routes-
/// shaped paths inside the rendered config (e.g. `apps.tls.automation.policies`)
/// are also yoink-managed but the realistic operator footguns are these two.
/// `admin` is reserved whole.
const RESERVED_CONFIG_EXTRA_PATHS: &[(&[&str], &str)] = &[
    // yoink puts the admin API on a unix socket that only the admin
    // relay shares. Another `listen` can open the admin API to every
    // routed service again.
    (
        &["admin"],
        "remove the `admin` key from `proxy.config_extra`; yoink manages the admin block",
    ),
    (
        &["apps", "http", "servers", "main", "routes"],
        "set this via `proxy.global_handlers:` / `caddy_extra_json:` instead",
    ),
    (
        &["apps", "http", "servers", "main", "tls_connection_policies"],
        "set this via `proxy.tls.client_auth:` instead",
    ),
];

/// Walk `parsed` for any of the reserved nested-object paths and reject
/// the config if the user wrote there. The paths describe descents
/// through nested objects, so `apps.http.servers.main.routes` matches a
/// `parsed` that has an `apps` object containing `http` containing
/// `servers` containing `main` with a key `routes` (any value type).
fn reject_reserved_config_extra_paths(parsed: &serde_json::Value) -> Result<(), ConfigError> {
    for (path, hint) in RESERVED_CONFIG_EXTRA_PATHS {
        if walks_to(parsed, path) {
            return Err(ConfigError::Invalid(format!(
                "proxy.config_extra: writes to `{}` are reserved for yoink — {hint}",
                path.join(".")
            )));
        }
    }
    Ok(())
}

/// `true` when `value` has an object descent matching every segment of
/// `path` — i.e. the user supplied a value at that path. Stops as soon
/// as a non-object is found; so a partial overlap (e.g. extras only
/// reach `apps.http.servers.main` without setting `routes` underneath)
/// is fine.
fn walks_to(value: &serde_json::Value, path: &[&str]) -> bool {
    let mut cursor = value;
    for segment in path {
        match cursor.get(*segment) {
            Some(next) => cursor = next,
            None => return false,
        }
    }
    true
}

/// One warning line per `xcaddy.plugins` entry that lacks an `@version`
/// pin. Without the pin, `xcaddy build` resolves whatever Go's module
/// proxy decides on the day of the build, so two hosts that compile at
/// different times can produce semantically different binaries with
/// the same `yoink-caddy:<hash>` tag (the hash captures the *string*,
/// not the resolved module version). Pure for unit-testability; the
/// caller wraps each line in `tracing::warn!`.
fn unpinned_plugin_warnings(plugins: &[String]) -> Vec<String> {
    plugins
        .iter()
        .filter(|p| !p.contains('@'))
        .map(|p| {
            format!(
                "proxy.xcaddy.plugins entry {p:?} has no `@<version>` pin — \
                 the resolved module version will drift across builds. Pin \
                 e.g. {p}@v1.2.3 for reproducible images."
            )
        })
        .collect()
}

/// Cheap shape-check for an xcaddy plugin entry. Accepts
/// `<host>/<owner>/<repo>(/<sub>)*` optionally followed by `@<version>`.
/// We don't try to parse Go module semantics here — xcaddy itself will
/// reject anything truly malformed at build time. The goal is to catch
/// obvious typos at config-load.
fn is_plausible_go_module(s: &str) -> bool {
    let module_part = s.split_once('@').map_or(s, |(m, _)| m);
    if module_part.is_empty() || module_part.starts_with('/') || module_part.ends_with('/') {
        return false;
    }
    let segments: Vec<&str> = module_part.split('/').collect();
    if segments.len() < 3 {
        return false;
    }
    if !segments[0].contains('.') {
        return false;
    }
    segments
        .iter()
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_graphic() && c != '@'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DomainSpec;

    fn config_with_one_service(domain: Option<DomainSpec>, port: Option<u16>) -> Config {
        let mut cfg = Config::parse_str(
            r#"
deploy:
  networks: [n]
hosts:
  - { address: h1, user: deploy }
services:
  - name: api
    image: img
    tag: t
    networks: [n]
    run:
      port: 8080
"#,
        )
        .expect("base config parses");
        cfg.services[0].domain = domain;
        cfg.services[0].run.port = port;
        cfg
    }

    #[test]
    fn no_domain_means_no_proxy() {
        let cfg = config_with_one_service(None, Some(8080));
        assert!(!cfg.proxy_enabled());
        assert!(!cfg.services.iter().any(|s| s.name == PROXY_SERVICE_NAME));
    }

    /// A service without `networks:` used to default to every entry of
    /// `deploy.networks`, which includes `yoink-proxy-admin` once the
    /// proxy is on. It could then reach the admin relay and through it
    /// the unauthenticated Caddy admin API. Only the relay may be on
    /// that network.
    #[test]
    fn services_without_networks_stay_off_the_admin_network() {
        let mut cfg = Config::parse_str(
            r#"
hosts:
  - { address: h, user: u }
deploy:
  networks: [app]
proxy:
  email: ops@example.com
services:
  - name: api
    image: example/api
    tag: v1
    domain: api.example.com
    run: { port: 8080 }
  - name: worker
    image: example/worker
    tag: v1
    run: {}
"#,
        )
        .expect("config parses");
        // `parse_str` may already inject; a second call must not change it.
        cfg.inject_implicit_proxy().expect("inject ok");

        let nets = |name: &str| cfg.networks_for(cfg.service(name).unwrap());
        assert_eq!(nets("api"), ["app", INGRESS_NETWORK]);
        assert_eq!(nets("worker"), ["app"]);
        // The admin API is on a unix socket: the proxy is not on the
        // admin network.
        assert_eq!(nets(PROXY_SERVICE_NAME), [INGRESS_NETWORK]);
        // Still declared, so each host creates it for the relay.
        assert!(cfg.deploy.networks.iter().any(|n| n == ADMIN_NETWORK));
    }

    /// Nothing stopped a service from joining `yoink-proxy-admin` through
    /// `deploy.networks` or its own `networks:`. It could then change
    /// the proxy config through the admin API.
    #[test]
    fn admin_network_is_reserved_for_the_proxy() {
        let cases = [
            (
                "deploy: { networks: [app, yoink-proxy-admin] }\nservices: []",
                "deploy.networks",
            ),
            (
                "services:\n  - { name: worker, image: w, tag: v1, networks: [yoink-proxy-admin], run: {} }",
                "service \"worker\": networks",
            ),
            (
                "services:\n  - name: api\n    image: a\n    tag: v1\n    domain: api.example.com\n    networks: [yoink-proxy-admin]\n    run: { port: 8080 }",
                "service \"api\": networks",
            ),
        ];
        for (body, field) in cases {
            let yaml = format!(
                "hosts: [{{ address: h, user: u }}]\nproxy: {{ email: o@example.com }}\n{body}\n"
            );
            let err = Config::parse_str(&yaml).expect_err(field);
            let msg = err.to_string();
            assert!(msg.contains(field), "got: {msg}");
            assert!(msg.contains("\"yoink-proxy-admin\""), "got: {msg}");
            assert!(
                msg.contains("reserved for the admin relay of the proxy"),
                "got: {msg}"
            );
            assert!(msg.contains("Use a different network name"), "got: {msg}");
        }
    }

    /// The config volume holds the admin socket. A service that mounts
    /// it as root can change the proxy config, and as the cert volume
    /// the proxy would mount it twice.
    #[test]
    fn caddy_config_volume_is_reserved_for_the_proxy() {
        let cases = [
            (
                "proxy: { email: o@example.com, cert_volume: yoink_caddy_config }\n\
                 services:\n  - { name: api, image: a, tag: v1, domain: api.example.com, run: { port: 8080 } }",
                "proxy.cert_volume",
            ),
            (
                "services:\n  - { name: worker, image: w, tag: v1, \
                 run: { volumes: [\"yoink_caddy_config:/c:ro\"] } }",
                "service \"worker\": run.volumes",
            ),
            (
                "proxy: { email: o@example.com }\nservices:\n  - { name: api, image: a, tag: v1, \
                 domain: api.example.com, run: { port: 8080, volumes: [\"yoink_caddy_config:/c\"] } }",
                "service \"api\": run.volumes",
            ),
            (
                "services:\n  - { name: worker, image: w, tag: v1, \
                 run: { binds: [\"yoink_caddy_config:/c\"] } }",
                "service \"worker\": run.binds",
            ),
        ];
        for (body, field) in cases {
            let yaml = format!("hosts: [{{ address: h, user: u }}]\n{body}\n");
            let err = Config::parse_str(&yaml).expect_err(field);
            let msg = err.to_string();
            assert!(msg.contains(field), "got: {msg}");
            assert!(msg.contains("\"yoink_caddy_config\""), "got: {msg}");
            assert!(msg.contains("reserved for the admin socket"), "got: {msg}");
        }
        // The synthesized proxy mounts it, and other volumes stay free.
        let ok = "hosts: [{ address: h, user: u }]\nproxy: { email: o@example.com }\n\
                  services:\n  - { name: api, image: a, tag: v1, domain: api.example.com, \
                  run: { port: 8080, volumes: [\"yoink_caddy_config_x:/c\", \"data:/d\"] } }\n";
        let cfg = Config::parse_str(ok).expect("parses");
        let proxy = cfg.service(PROXY_SERVICE_NAME).expect("proxy");
        assert!(
            proxy
                .run
                .volumes
                .contains(&format!("{CADDY_CONFIG_VOLUME}:/config"))
        );
    }

    /// The deploy probe of the proxy calls the relay by its name. The
    /// former admin API alias is free again: no address of the proxy
    /// serves the admin API.
    #[test]
    fn relay_name_is_reserved_as_a_network_alias() {
        let alias = ADMIN_RELAY_NAME;
        let yaml = format!(
            "hosts: [{{ address: h, user: u }}]\nproxy: {{ email: o@example.com }}\n\
             services:\n  - name: api\n    image: a\n    tag: v1\n    \
             domain: api.example.com\n    \
             run: {{ port: 8080, options: {{ network_aliases: [api, {alias}] }} }}\n"
        );
        let msg = Config::parse_str(&yaml).expect_err(alias).to_string();
        assert!(
            msg.contains(&format!(
                "service \"api\": run.options.network_aliases lists \"{alias}\""
            )),
            "got: {msg}"
        );
    }

    #[test]
    fn former_admin_api_alias_is_a_normal_alias() {
        let yaml = "hosts: [{ address: h, user: u }]\nproxy: { email: o@example.com }\n\
                    services:\n  - name: api\n    image: a\n    tag: v1\n    \
                    domain: api.example.com\n    \
                    run: { port: 8080, options: { network_aliases: [yoink-proxy-admin-api] } }\n";
        Config::parse_str(yaml).expect("the alias is free");
    }

    /// `kind: proxy` in the YAML made a user service look like the
    /// proxy. It then skipped `reject_admin_network` and could join the
    /// admin network. Only yoink sets `kind`, so the YAML key is refused.
    #[test]
    fn kind_is_not_a_yaml_key() {
        let yaml = "hosts: [{ address: h, user: u }]\nproxy: { email: o@example.com }\n\
                    services:\n  - name: api\n    image: a\n    tag: v1\n    \
                    domain: api.example.com\n    run: { port: 8080 }\n  - name: worker\n    \
                    image: w\n    tag: v1\n    kind: proxy\n    networks: [yoink-proxy-admin]\n    \
                    run: {}\n";
        let msg = Config::parse_str(yaml)
            .expect_err("kind: proxy")
            .to_string();
        assert!(msg.contains("unknown field `kind`"), "got: {msg}");
    }

    #[test]
    fn domain_synthesizes_proxy_and_networks() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        // Need ACME email since default tls is Auto.
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            ..ProxyConfig::default()
        });
        cfg.inject_implicit_proxy().expect("inject ok");
        assert!(cfg.deploy.networks.iter().any(|n| n == INGRESS_NETWORK));
        assert!(cfg.deploy.networks.iter().any(|n| n == ADMIN_NETWORK));
        let proxy = cfg
            .services
            .iter()
            .find(|s| s.name == PROXY_SERVICE_NAME)
            .expect("proxy synthesized");
        assert!(matches!(proxy.kind, Some(ServiceKind::Proxy)));
        let api = cfg.service("api").unwrap();
        assert!(
            api.networks
                .as_ref()
                .unwrap()
                .iter()
                .any(|n| n == INGRESS_NETWORK)
        );
        assert!(api.depends_on.iter().any(|d| d == PROXY_SERVICE_NAME));
    }

    #[test]
    fn auto_tls_without_email_errors_at_render_time() {
        // Load-time validation accepts the missing-email config so
        // operators can bootstrap a project before setting it. The
        // strict check fires at render time inside `proxy::caddy::render`,
        // which is exercised by `yoink up`.
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        // Load-time succeeds.
        cfg.inject_implicit_proxy()
            .expect("load-time accepts missing email");
        // Render-time bails with the same operator-facing message.
        let err = crate::proxy::caddy::render(&cfg, |_| vec!["c1".into()], None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("proxy.email") && msg.contains("Let's Encrypt"),
            "got: {msg}"
        );
    }

    #[test]
    fn tls_cert_without_secrets_errors() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.services[0].tls = TlsMode::Cert;
        let err = cfg.inject_implicit_proxy().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tls_cert_secret"), "got: {msg}");
    }

    #[test]
    fn domain_without_port_errors() {
        let mut cfg =
            config_with_one_service(Some(DomainSpec::Single("api.example.com".into())), None);
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(format!("{err}").contains("run.port"), "got: {err}");
    }

    #[test]
    fn xcaddy_and_image_mutually_exclusive() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            image: Some("ghcr.io/me/caddy:custom".into()),
            xcaddy: Some(crate::config::XcaddyConfig {
                plugins: vec!["github.com/caddy-dns/cloudflare".into()],
                caddy_version: None,
                base_image: None,
                builder_image: None,
                env: std::collections::BTreeMap::new(),
                replace: Vec::new(),
            }),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(
            format!("{err}").contains("mutually exclusive"),
            "got: {err}"
        );
    }

    #[test]
    fn xcaddy_empty_plugins_rejected() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            xcaddy: Some(crate::config::XcaddyConfig {
                plugins: vec![],
                caddy_version: None,
                base_image: None,
                builder_image: None,
                env: std::collections::BTreeMap::new(),
                replace: Vec::new(),
            }),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(format!("{err}").contains("empty"), "got: {err}");
    }

    #[test]
    fn config_extra_rejects_routes_clobber() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            config_extra: Some(r#"{"apps":{"http":{"servers":{"main":{"routes":[]}}}}}"#.into()),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("apps.http.servers.main.routes"), "got: {msg}");
        assert!(msg.contains("proxy.global_handlers"), "got: {msg}");
    }

    #[test]
    fn config_extra_rejects_tls_connection_policies_clobber() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            config_extra: Some(
                r#"{"apps":{"http":{"servers":{"main":{"tls_connection_policies":[]}}}}}"#.into(),
            ),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tls_connection_policies"), "got: {msg}");
        assert!(msg.contains("proxy.tls.client_auth"), "got: {msg}");
    }

    /// An `admin` override in `config_extra` could set a wildcard
    /// `listen` again and open the admin API to every routed service.
    #[test]
    fn config_extra_rejects_admin_overrides() {
        for extra in [
            r#"{"admin":{"listen":"0.0.0.0:2019"}}"#,
            r#"{"admin":{"origins":["example.com"]}}"#,
            r#"{"admin":{}}"#,
        ] {
            let mut cfg = config_with_one_service(
                Some(DomainSpec::Single("api.example.com".into())),
                Some(8080),
            );
            cfg.proxy = Some(ProxyConfig {
                email: Some("ops@example.com".into()),
                config_extra: Some(extra.into()),
                ..ProxyConfig::default()
            });
            let msg = format!("{}", cfg.inject_implicit_proxy().unwrap_err());
            assert!(msg.contains("proxy.config_extra"), "got: {msg}");
            assert!(
                msg.ends_with(
                    "remove the `admin` key from `proxy.config_extra`; \
                     yoink manages the admin block"
                ),
                "got: {msg}"
            );
        }
    }

    /// The bootstrap config of the proxy used `0.0.0.0:2019`, so every
    /// routed service on `yoink-ingress` could POST `/load`. Then it
    /// used an address on the admin network, which a container on
    /// `yoink-ingress` with `NET_ADMIN` or `NET_RAW` could still reach
    /// through the proxy. Now the admin API is on a unix socket.
    #[test]
    fn bootstrap_admin_listens_on_the_unix_socket() {
        let proxy = synthesized_proxy_service(&ProxyConfig::default());
        let [cmd] = proxy.run.cmd.as_slice() else {
            panic!("one shell command, got {:?}", proxy.run.cmd);
        };
        let json = cmd
            .split_once("printf '%s' '")
            .and_then(|(_, rest)| rest.split_once("' > "))
            .map_or_else(|| panic!("unexpected bootstrap: {cmd}"), |(json, _)| json);
        let bootstrap: serde_json::Value = serde_json::from_str(json).expect("bootstrap is JSON");
        assert_eq!(bootstrap["admin"], admin_block());
        assert_eq!(
            bootstrap["admin"],
            serde_json::json!({
                "listen": "unix//config/caddy/admin.sock",
                "origins": ["127.0.0.1"],
            })
        );
        assert!(!cmd.contains("0.0.0.0"), "got: {cmd}");
        assert!(!cmd.contains("2019"), "got: {cmd}");
        // The proxy is on `yoink-ingress` only and publishes no admin
        // port.
        assert_eq!(proxy.networks, Some(vec![INGRESS_NETWORK.to_string()]));
        assert_eq!(proxy.run.publish, ["80:80", "443:443"]);
        // The socket is on the config volume, which the relay mounts.
        assert!(
            proxy
                .run
                .volumes
                .contains(&format!("{CADDY_CONFIG_VOLUME}:/config")),
            "got: {:?}",
            proxy.run.volumes
        );
        assert!(ADMIN_SOCKET.starts_with("/config/"));
        // The deploy probe goes through the relay.
        assert_eq!(proxy.run.port, Some(ADMIN_RELAY_PORT));
    }

    /// The start script is part of the spec hash of the proxy. The
    /// bootstrap came from `serde_json::Value` Display, so its key order
    /// depended on the `preserve_order` feature of `serde_json`.
    #[test]
    fn start_script_is_pinned() {
        let proxy = synthesized_proxy_service(&ProxyConfig::default());
        assert_eq!(
            proxy.run.cmd,
            [concat!(
                "mkdir -p /config/caddy || exit 1\n",
                "head=\n",
                "{ IFS= read -r head < /config/caddy/autosave.json || :; } 2>/dev/null\n",
                "case $head in\n",
                r#"'{"admin":{"listen":"unix//config/caddy/admin.sock","origins":["127.0.0.1"]},'*) set -- --resume ;;"#,
                "\n*) set -- ;;\n",
                "esac\n",
                "rm -f /config/caddy/admin.sock\n",
                "printf '%s' '",
                r#"{"admin":{"listen":"unix//config/caddy/admin.sock","origins":["127.0.0.1"]}}"#,
                "' > /config/caddy/bootstrap.json || exit 1\n",
                "exec caddy run --config /config/caddy/bootstrap.json \"$@\"",
            )]
        );
    }

    /// The script resumes an autosave only when it starts with this
    /// prefix. Caddy writes the file with Go's `json.Marshal`: compact,
    /// with sorted map keys. `serde_json` without `preserve_order`
    /// writes the same form. A change of `admin_block` that the prefix
    /// does not follow breaks this test.
    #[test]
    fn socket_autosave_prefix_is_the_serialized_admin_block() {
        let autosave = serde_json::json!({
            "storage": { "module": "file_system" },
            "apps": { "http": { "servers": {} } },
            "admin": admin_block(),
        });
        let written = serde_json::to_string(&autosave).expect("JSON");
        assert!(
            written.starts_with(&socket_autosave_prefix()),
            "{written} does not start with {}",
            socket_autosave_prefix()
        );
        assert_eq!(
            socket_autosave_prefix(),
            r#"{"admin":{"listen":"unix//config/caddy/admin.sock","origins":["127.0.0.1"]},"#
        );
        // The prefix is in single quotes in the script.
        assert!(!socket_autosave_prefix().contains('\''));
        assert!(!bootstrap_config().contains('\''));
    }

    /// What the stub `caddy` saw when the start script ran it.
    struct StubRun {
        args: String,
        /// The content of the file after `--config`.
        config: String,
        /// The config directory of the run.
        dir: String,
        /// The stub ran in the process of the shell: `exec`, no fork.
        same_pid: bool,
        /// The admin socket file existed when the stub ran.
        socket_left: bool,
    }

    /// The shells to test the start script with: `/bin/sh`, and `dash`
    /// when the host has it (a strict POSIX shell, close to the busybox
    /// `ash` of the Caddy image).
    fn shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/dash", "/usr/bin/dash"]
            .into_iter()
            .filter(|s| std::path::Path::new(s).exists())
            .collect()
    }

    /// Run the start script under `shell`, with a stub `caddy` first on
    /// `PATH` and the config directory in a temporary directory.
    /// `autosave` is the content of `autosave.json`, `None` for no file.
    /// Each run starts with a stale admin socket file, as an unclean
    /// stop leaves it.
    fn run_start_script(shell: &str, autosave: Option<&str>) -> StubRun {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        let config_dir = dir.join("caddy");
        std::fs::create_dir_all(&config_dir).expect("mkdir");
        let socket = config_dir.join(admin_socket_name());
        std::fs::write(&socket, "").expect("stale socket");
        if let Some(content) = autosave {
            std::fs::write(config_dir.join("autosave.json"), content).expect("autosave");
        }
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir bin");
        let stub = bin.join("caddy");
        std::fs::write(
            &stub,
            format!(
                concat!(
                    "#!/bin/sh\n",
                    "echo $$ > {out}/pid\n",
                    "echo \"$*\" > {out}/args\n",
                    "[ \"$2\" = --config ] && cat \"$3\" > {out}/config\n",
                    "if [ -e {socket} ]; then echo yes; else echo no; fi > {out}/socket\n",
                ),
                out = dir.display(),
                socket = socket.display(),
            ),
        )
        .expect("stub");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let script = start_script_in(&config_dir.to_string_lossy());
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let child = std::process::Command::new(shell)
            .arg("-c")
            .arg(&script)
            .env("PATH", path)
            .spawn()
            .expect("spawn sh");
        let shell_pid = child.id();
        let status = child.wait_with_output().expect("wait").status;
        assert!(status.success(), "{shell}: script failed: {status}");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect(name);
        StubRun {
            args: read("args").trim_end().to_string(),
            config: read("config"),
            dir: config_dir.to_string_lossy().into_owned(),
            same_pid: read("pid").trim() == shell_pid.to_string(),
            socket_left: read("socket").trim() == "yes",
        }
    }

    /// An autosave with the socket block: Caddy resumes it.
    #[test]
    fn start_script_resumes_an_autosave_on_the_socket() {
        let autosave = format!(r#"{}"apps":{{}}}}"#, socket_autosave_prefix());
        for shell in shells() {
            let run = run_start_script(shell, Some(&autosave));
            assert_eq!(
                run.args,
                format!("run --config {}/bootstrap.json --resume", run.dir),
                "{shell}"
            );
            assert_eq!(run.config, bootstrap_config(), "{shell}");
            assert!(run.same_pid, "{shell}: caddy must replace the shell");
            assert!(!run.socket_left, "{shell}: the stale socket must go");
        }
    }

    /// The autosave of a v0.22 proxy (admin on `0.0.0.0:2019`), an
    /// admin block with one more key, an empty file and no file: Caddy
    /// starts from the bootstrap, so the admin API is on the socket.
    #[test]
    fn start_script_starts_from_the_bootstrap_otherwise() {
        let other_key = socket_autosave_prefix().replace("},", r#","x":1},"#);
        let autosaves = [
            Some(r#"{"admin":{"listen":"0.0.0.0:2019"},"apps":{}}"#),
            Some(other_key.as_str()),
            Some(""),
            None,
        ];
        for shell in shells() {
            for autosave in autosaves {
                let run = run_start_script(shell, autosave);
                assert_eq!(
                    run.args,
                    format!("run --config {}/bootstrap.json", run.dir),
                    "{shell}: {autosave:?}"
                );
                assert_eq!(run.config, bootstrap_config(), "{shell}");
                assert!(run.same_pid, "{shell}: caddy must replace the shell");
                assert!(!run.socket_left, "{shell}: the stale socket must go");
            }
        }
    }

    #[test]
    fn config_extra_allows_legitimate_paths_under_main() {
        // Setting trusted_proxies on `main` is exactly the supported path.
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            config_extra: Some(
                r#"{"apps":{"http":{"servers":{"main":{"trusted_proxies":{"source":"cloudflare"}}}}}}"#
                    .into(),
            ),
            ..ProxyConfig::default()
        });
        cfg.inject_implicit_proxy()
            .expect("legitimate config_extra should pass");
    }

    #[test]
    fn config_extra_must_be_a_json_object() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            config_extra: Some("[1, 2, 3]".into()),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(
            format!("{err}").contains("must be a JSON object"),
            "got: {err}"
        );
    }

    #[test]
    fn global_handlers_must_be_json() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            global_handlers: vec!["{not json".into()],
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(
            format!("{err}").contains("proxy.global_handlers[0] is not valid JSON"),
            "got: {err}",
        );
    }

    #[test]
    fn config_extra_must_be_valid_json() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            config_extra: Some("{not json".into()),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"), "got: {err}");
    }

    #[test]
    fn unpinned_plugin_warnings_flags_unpinned_only() {
        let plugins = vec![
            "github.com/foo/bar".to_string(),
            "github.com/baz/quux@v0.1.0".to_string(),
            "github.com/zip/zap".to_string(),
        ];
        let warnings = unpinned_plugin_warnings(&plugins);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("github.com/foo/bar"));
        assert!(warnings[1].contains("github.com/zip/zap"));
        assert!(!warnings.iter().any(|w| w.contains("github.com/baz/quux")));
    }

    #[test]
    fn unpinned_plugin_warnings_empty_when_all_pinned() {
        let plugins = vec!["github.com/foo/bar@v1.0.0".to_string()];
        assert!(unpinned_plugin_warnings(&plugins).is_empty());
    }

    #[test]
    fn xcaddy_plugin_shape_validation() {
        assert!(is_plausible_go_module("github.com/caddy-dns/cloudflare"));
        assert!(is_plausible_go_module("github.com/foo/bar@v1.2.3"));
        assert!(is_plausible_go_module("git.example.org/owner/repo/sub"));
        assert!(!is_plausible_go_module("not-a-module"));
        assert!(!is_plausible_go_module("github.com/foo"));
        assert!(!is_plausible_go_module("/leading/slash/repo"));
        assert!(!is_plausible_go_module("nopath/in/firstseg"));
    }

    #[test]
    fn user_defined_proxy_service_collides() {
        let mut cfg = Config::parse_str(
            r#"
deploy:
  networks: [n]
hosts:
  - { address: h1, user: deploy }
services:
  - name: yoink-proxy
    image: caddy
    tag: latest
    networks: [n]
    run: { port: 80 }
  - name: api
    image: img
    tag: t
    networks: [n]
    run: { port: 8080 }
"#,
        )
        .expect("base config parses");
        cfg.services[1].domain = Some(DomainSpec::Single("api.example.com".into()));
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            ..ProxyConfig::default()
        });
        let err = cfg.inject_implicit_proxy().unwrap_err();
        assert!(format!("{err}").contains("reserved"), "got: {err}");
    }
}
