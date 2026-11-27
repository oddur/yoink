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

/// Docker network the Caddy admin API listens on. Only the proxy
/// joins it. The name sorts before [`INGRESS_NETWORK`], and the proxy
/// endpoint on it has gateway priority [`ADMIN_GW_PRIORITY`], so
/// Docker makes it the gateway endpoint of the proxy. Docker programs
/// published ports only for the gateway endpoint, so the loopback
/// publish of the admin port lands on this network and not on
/// `yoink-ingress`, where every routed service is.
pub const ADMIN_NETWORK: &str = "yoink-admin";

/// Gateway priority of the proxy endpoint on [`ADMIN_NETWORK`]. The
/// other endpoints keep the default 0, and the highest priority wins.
/// Docker Engine 28 added the field (API 1.48).
pub const ADMIN_GW_PRIORITY: i64 = 1;

/// Network alias that the proxy has only on [`ADMIN_NETWORK`]. Caddy
/// binds the admin API to the one address this name resolves to, the
/// proxy IP on that network. A service on `yoink-ingress` cannot
/// connect to that address.
pub const ADMIN_API_ALIAS: &str = "yoink-proxy-admin-api";

/// Container port the Caddy admin API listens on (Caddy default).
pub const ADMIN_PORT: u16 = 2019;

/// The `admin.listen` address of Caddy, and the only `Host` header
/// value that the admin API accepts. With a listen address that is not
/// a wildcard, Caddy rejects a request whose `Host` is not in
/// `admin.origins`. yoink sends this value on every admin request, and
/// the deploy probe and the Docker HEALTHCHECK send it because they
/// use this name in the URL.
pub const ADMIN_API_HOST: &str = "yoink-proxy-admin-api:2019";

/// The `admin` block of every Caddy config that yoink writes: the
/// bootstrap of the proxy container and each `/load`. A `/load` that
/// omits the block sets the admin API back to the Caddy default
/// `localhost:2019`, which the published port cannot reach.
///
/// `enforce_origin` stays false: then Caddy checks `Origin` only when
/// a request carries `Origin` or `Sec-Fetch-Mode`, which yoink's
/// clients do not send. The `Host` check is on because the listen
/// address is not a wildcard.
#[must_use]
pub fn admin_block() -> serde_json::Value {
    serde_json::json!({
        "listen": ADMIN_API_HOST,
        "enforce_origin": false,
        "origins": [ADMIN_API_HOST],
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
        // so without this pin it would also join `yoink-admin` and
        // could reach the Caddy admin API, which has no authentication.
        // Only the proxy belongs on that network. An operator who lists an
        // implicit network in `deploy.networks` still gets it here.
        let operator_networks = self.deploy.networks.clone();
        for svc in &mut self.services {
            if svc.networks.is_none() && !matches!(svc.kind, Some(ServiceKind::Proxy)) {
                svc.networks = Some(operator_networks.clone());
            }
        }

        // Ensure both implicit networks are declared, so they are created
        // on each host. A user can also declare them explicitly (e.g. to
        // attach extra services to `yoink-ingress`); idempotent.
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
        // The admin network first: the create-time endpoint, and the
        // gateway endpoint (see `ADMIN_NETWORK`).
        networks: Some(vec![ADMIN_NETWORK.to_string(), INGRESS_NETWORK.to_string()]),
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
            // (`/config/`) at `ADMIN_API_HOST` from the admin network.
            // Eliminates the race where yoink-proxy is "started" per
            // docker but Caddy's HTTP server hasn't bound yet —
            // without this gate, the first /load can arrive before
            // admin is up.
            port: Some(ADMIN_PORT),
            healthcheck_path: Some("/config/".to_string()),
            // Caddy serves :80 + :443 on the host. The admin port is
            // published on 127.0.0.1 with an ephemeral host port
            // (`:0:2019`) so it's reachable from the host (and only
            // from the host) — docker doesn't route user-defined-
            // network container IPs from the host. yoink looks up
            // the actual host port via `inspect_container` and
            // SSH-tunnels to it for one `/load` call. Docker forwards
            // the publish to the proxy IP on `ADMIN_NETWORK`, the
            // gateway endpoint.
            //
            // `proxy.bind:` (when set) prepends the bind address to
            // :80/:443 so the proxy is reachable only on that
            // interface (e.g. tailnet IP). Admin port stays on
            // 127.0.0.1 regardless.
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
                vec![
                    format!("{prefix}80:80"),
                    format!("{prefix}443:443"),
                    format!("127.0.0.1:0:{ADMIN_PORT}"),
                ]
            },
            // Override the official caddy:2 image's entrypoint to
            // start with a minimal bootstrap config: the `admin_block`
            // that every `/load` also sends. The admin API listens on
            // `ADMIN_API_ALIAS`, the proxy IP on the admin network
            // only. The actual routes are pushed via `/load`
            // immediately after the container is healthy.
            //
            // `--resume` makes Caddy fall back to its autosaved
            // runtime config (`/config/caddy/autosave.json` on the
            // mounted `yoink_caddy_config` volume) when one exists,
            // ignoring the stdin bootstrap. That's how the site
            // survives a host reboot: yoink-proxy comes back with
            // the last-pushed routing table instead of the empty
            // admin-only bootstrap, even before any operator runs
            // `yoink up`. On a fresh proxy (no autosave yet) the
            // bootstrap takes effect as before.
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: vec![format!(
                "echo '{}' | exec caddy run --config /dev/stdin --resume",
                serde_json::json!({ "admin": admin_block() }),
            )],
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
                "yoink_caddy_config:/config".to_string(),
            ],
            ..ServiceRun::default()
        },
    }
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
    // yoink binds the admin API to the admin network only, and sends
    // a fixed `Host`. Another `listen` can open the admin API to every
    // routed service again, and other `origins` break the next push.
    (
        &["admin"],
        "the Caddy admin API listens only on the `yoink-admin` network",
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
    /// `deploy.networks`, which includes `yoink-admin` once the
    /// proxy is on. It could then reach the unauthenticated Caddy
    /// admin API. Only the proxy may be on that network.
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
        assert_eq!(nets(PROXY_SERVICE_NAME), [ADMIN_NETWORK, INGRESS_NETWORK]);
        // Still declared, so each host creates it for the proxy.
        assert!(cfg.deploy.networks.iter().any(|n| n == ADMIN_NETWORK));
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
            assert!(msg.contains("`admin`"), "got: {msg}");
            assert!(msg.contains(ADMIN_NETWORK), "got: {msg}");
        }
    }

    /// The bootstrap config of the proxy used `0.0.0.0:2019`. With
    /// the publish on the gateway endpoint, which was `yoink-ingress`,
    /// every routed service could POST `/load`.
    #[test]
    fn bootstrap_admin_listens_on_the_admin_alias() {
        let proxy = synthesized_proxy_service(&ProxyConfig::default());
        let [cmd] = proxy.run.cmd.as_slice() else {
            panic!("one shell command, got {:?}", proxy.run.cmd);
        };
        let json = cmd
            .strip_prefix("echo '")
            .and_then(|rest| rest.split_once("' | exec caddy run --config /dev/stdin --resume"))
            .map_or_else(|| panic!("unexpected bootstrap: {cmd}"), |(json, _)| json);
        let bootstrap: serde_json::Value = serde_json::from_str(json).expect("bootstrap is JSON");
        assert_eq!(bootstrap["admin"], admin_block());
        assert_eq!(bootstrap["admin"]["listen"], ADMIN_API_HOST);
        assert_eq!(
            bootstrap["admin"]["origins"],
            serde_json::json!([ADMIN_API_HOST])
        );
        assert!(!cmd.contains("0.0.0.0"), "got: {cmd}");
        assert_eq!(ADMIN_API_HOST, format!("{ADMIN_API_ALIAS}:{ADMIN_PORT}"));
        // When gateway priorities tie, Docker picks the network name
        // that sorts first.
        assert!(ADMIN_NETWORK < INGRESS_NETWORK);
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
