//! First-class reverse-proxy integration.
//!
//! Yoink bundles Caddy as a managed service when any service in the
//! config declares `domain:`. The proxy is just a regular yoink
//! service (`name = "yoink-proxy"`, `kind = ServiceKind::Proxy`) — it
//! reconciles through the same code path as everything else, drift-
//! detects via the same `spec_hash` label, and shows up in the same
//! TUI. The only special handling is:
//!
//! 1. Auto-injection at config-load time so users never write the
//!    `_proxy` block themselves (this module).
//! 2. Caddy JSON config rendered from the user-facing schema and
//!    pushed via Caddy's admin API at deterministic points in the
//!    rolling deploy ([`caddy`] + [`admin`]).
//!
//! Application-layer concerns (auth, rate limits, headers, CORS) are
//! deliberately not modeled. Use [`ServiceConfig::caddy_extra_json`]
//! to drop in a JSON snippet of additional Caddy handlers.

pub mod admin;
pub mod caddy;
pub mod xcaddy;

use crate::config::{
    Config, ConfigError, ProxyConfig, ServiceConfig, ServiceKind, ServiceRun, TlsMode,
};

/// The internal service name yoink uses for the implicit reverse
/// proxy. Users may not declare a service with this name when they
/// also use `domain:` — that's a collision error.
pub const PROXY_SERVICE_NAME: &str = "yoink-proxy";

/// Docker network the proxy uses to reach upstream containers by
/// name. Every service with `domain:` joins it automatically.
pub const INGRESS_NETWORK: &str = "yoink-ingress";

/// Docker network the proxy publishes its admin API on. Only the
/// proxy itself joins this network — the yoink CLI reaches the
/// admin API by inspecting the proxy container's IP on this network
/// and opening an SSH-tunnelled forward to it.
pub const ADMIN_NETWORK: &str = "yoink-proxy-admin";

/// Container port the Caddy admin API listens on (Caddy default).
pub const ADMIN_PORT: u16 = 2019;

/// True when the proxy should be enabled for `cfg` — either explicitly
/// (`proxy.enabled: true`) or implicitly (any service has `domain:`).
#[must_use]
pub fn proxy_enabled(cfg: &Config) -> bool {
    if let Some(p) = &cfg.proxy
        && let Some(explicit) = p.enabled
    {
        return explicit;
    }
    cfg.services.iter().any(|s| s.domain.is_some())
}

/// True when this service should be routed by the proxy.
#[must_use]
pub fn is_proxied(svc: &ServiceConfig) -> bool {
    svc.domain.is_some()
}

/// Synthesize the `_proxy` service, ensure ingress/admin networks
/// exist in `deploy.networks`, and add `_proxy` to every routed
/// service's `depends_on:` list. Idempotent — safe to call after
/// validation has already added the proxy.
#[allow(clippy::too_many_lines)] // mostly straight-line validation
pub fn inject_implicit_proxy(cfg: &mut Config) -> Result<(), ConfigError> {
    if !proxy_enabled(cfg) {
        return Ok(());
    }

    if let Some(p) = cfg.proxy.as_ref()
        && let Some(extra) = p.config_extra.as_deref()
    {
        match serde_json::from_str::<serde_json::Value>(extra) {
            Ok(serde_json::Value::Object(_)) => {}
            Ok(_) => {
                return Err(ConfigError::Invalid(
                    "proxy.config_extra must be a JSON object (top-level Caddy \
                     config keys like `apps`, `storage`, `admin`)"
                        .to_string(),
                ));
            }
            Err(e) => {
                return Err(ConfigError::Invalid(format!(
                    "proxy.config_extra is not valid JSON: {e}"
                )));
            }
        }
    }

    if let Some(p) = cfg.proxy.as_ref()
        && let Some(x) = &p.xcaddy
    {
        if p.image.is_some() {
            return Err(ConfigError::Invalid(
                "proxy.image and proxy.xcaddy are mutually exclusive — `image:` is the \
                 bring-your-own-image escape hatch; `xcaddy:` is the managed-build path. \
                 Pick one."
                    .to_string(),
            ));
        }
        if x.plugins.is_empty() {
            return Err(ConfigError::Invalid(
                "proxy.xcaddy.plugins is empty — set at least one plugin or remove the \
                 `xcaddy:` block to use vanilla `caddy:2`"
                    .to_string(),
            ));
        }
        for plugin in &x.plugins {
            if !is_plausible_go_module(plugin) {
                return Err(ConfigError::Invalid(format!(
                    "proxy.xcaddy.plugins entry {plugin:?} doesn't look like a Go module \
                     path (expected `<host>/<owner>/<repo>` or \
                     `<host>/<owner>/<repo>@<version>`)"
                )));
            }
        }
    }

    let proxy_has_inline_cert = cfg.proxy.as_ref().and_then(|p| p.tls.as_ref()).is_some();

    // Validation: any service using `tls: cert` must name both
    // `tls_cert_secret` and `tls_key_secret` (or inherit from
    // `proxy.tls`); reject early so the operator gets a config-time
    // error rather than a deploy-time one.
    for svc in &cfg.services {
        if svc.domain.is_some() && svc.run.port.is_none() {
            return Err(ConfigError::Invalid(format!(
                "service {:?} has `domain:` but no `run.port` — the proxy needs to know \
                 which container port to forward to",
                svc.name,
            )));
        }
        if matches!(svc.tls, TlsMode::Cert)
            && (svc.tls_cert_secret.is_none() || svc.tls_key_secret.is_none())
        {
            return Err(ConfigError::Invalid(format!(
                "service {:?} sets `tls: cert` but is missing `tls_cert_secret` and/or \
                 `tls_key_secret` — both are required for the inline-cert path \
                 (or set `proxy.tls.cert_secret` to inherit)",
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
    for svc in &cfg.services {
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

    // ACME requires an email address — fail loudly if any service uses
    // `tls: auto` (the default) and we'd actually run ACME (no
    // proxy-level inline cert overrides it) and `proxy.email` isn't
    // set.
    let any_acme = !proxy_has_inline_cert
        && cfg
            .services
            .iter()
            .any(|s| s.domain.is_some() && matches!(s.tls, TlsMode::Auto));
    if any_acme && cfg.proxy.as_ref().and_then(|p| p.email.as_ref()).is_none() {
        return Err(ConfigError::Invalid(
            "at least one service uses `tls: auto` (Let's Encrypt) but `proxy.email:` \
             is unset — Let's Encrypt requires a registration email (or set \
             `proxy.tls.cert_secret` for an inline cert instead)"
                .to_string(),
        ));
    }

    // Refuse the user-defined `_proxy` collision case. Safer than
    // silently overriding because the user's definition probably
    // wouldn't have the right networks/admin-port wiring anyway.
    if cfg
        .services
        .iter()
        .any(|s| s.name == PROXY_SERVICE_NAME && !matches!(s.kind, Some(ServiceKind::Proxy)))
    {
        return Err(ConfigError::Invalid(format!(
            "service name {PROXY_SERVICE_NAME:?} is reserved for the implicit reverse \
             proxy — rename your service or remove `domain:` from the routed services",
        )));
    }

    // Ensure both implicit networks are declared. A user can also
    // declare them explicitly (e.g. to attach extra services to
    // `yoink-ingress`); idempotent.
    for net in [INGRESS_NETWORK, ADMIN_NETWORK] {
        if !cfg.deploy.networks.iter().any(|n| n == net) {
            cfg.deploy.networks.push(net.to_string());
        }
    }

    // Every routed service joins the ingress network. Preserve any
    // tier networks the operator already declared.
    for svc in &mut cfg.services {
        if !is_proxied(svc) {
            continue;
        }
        let mut nets = svc
            .networks
            .clone()
            .unwrap_or_else(|| cfg.deploy.networks.clone());
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
    if !cfg.services.iter().any(|s| s.name == PROXY_SERVICE_NAME) {
        let proxy_cfg = cfg.proxy.clone().unwrap_or_default();
        cfg.services.push(synthesized_proxy_service(&proxy_cfg));
    }
    Ok(())
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
        networks: Some(vec![INGRESS_NETWORK.to_string(), ADMIN_NETWORK.to_string()]),
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
            // Healthcheck probes Caddy's admin endpoint (`/config/`)
            // on the container's :2019 from inside the proxy's first
            // network. Eliminates the race where yoink-proxy is
            // "started" per docker but Caddy's HTTP server hasn't
            // bound yet — without this gate, the first /load can
            // arrive before admin is up.
            port: Some(ADMIN_PORT),
            healthcheck_path: Some("/config/".to_string()),
            // Caddy serves :80 + :443 on the host. Admin API on :2019
            // is published on 127.0.0.1 with an ephemeral host port
            // (`:0:2019`) so it's reachable from the host (and only
            // from the host) — docker doesn't route user-defined-
            // network container IPs from the host. yoink looks up
            // the actual host port via `inspect_container` and
            // SSH-tunnels to it for one `/load` call.
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
                    "127.0.0.1:0:2019".to_string(),
                ]
            },
            // Override the official caddy:2 image's entrypoint to
            // start with a minimal bootstrap config: admin bound to
            // 0.0.0.0:2019 (so the admin endpoint is reachable from
            // the docker bridge — the default `localhost:2019` would
            // refuse our SSH-tunneled `/load`), `enforce_origin: false`
            // + a known origin (`yoink-admin`) so we can route past
            // Caddy's host-header check by setting Host: yoink-admin
            // on every admin request. The actual routes are pushed
            // via `/load` immediately after the container is healthy.
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            cmd: vec![
                concat!(
                    "echo '{\"admin\":{\"listen\":\"0.0.0.0:2019\",",
                    "\"enforce_origin\":false,",
                    "\"origins\":[\"yoink-admin\"]}}'",
                    " | exec caddy run --config /dev/stdin",
                )
                .to_string(),
            ],
            // Yoink's `cap_drop: [ALL]` default would block Caddy
            // from binding :80/:443 (privileged ports). Re-add the
            // one cap that lets it; everything else stays dropped.
            options: crate::config::RunOptions {
                cap_drop: vec!["ALL".to_string()],
                cap_add: vec!["NET_BIND_SERVICE".to_string()],
                // Caddy's official image runs as root and writes ACME
                // state to /data as root — overriding to a non-root
                // user would lose write access to the cert volume.
                user: Some("0:0".to_string()),
                ..crate::config::RunOptions::default()
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
        assert!(!proxy_enabled(&cfg));
        assert!(!cfg.services.iter().any(|s| s.name == PROXY_SERVICE_NAME));
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
        inject_implicit_proxy(&mut cfg).expect("inject ok");
        assert!(cfg.deploy.networks.iter().any(|n| n == INGRESS_NETWORK));
        assert!(cfg.deploy.networks.iter().any(|n| n == ADMIN_NETWORK));
        let proxy = cfg
            .services
            .iter()
            .find(|s| s.name == PROXY_SERVICE_NAME)
            .expect("proxy synthesized");
        assert!(matches!(proxy.kind, Some(ServiceKind::Proxy)));
        let api = cfg.services.iter().find(|s| s.name == "api").unwrap();
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
    fn auto_tls_without_email_errors() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        assert!(format!("{err}").contains("proxy.email"), "got: {err}");
    }

    #[test]
    fn tls_cert_without_secrets_errors() {
        let mut cfg = config_with_one_service(
            Some(DomainSpec::Single("api.example.com".into())),
            Some(8080),
        );
        cfg.services[0].tls = TlsMode::Cert;
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tls_cert_secret"), "got: {msg}");
    }

    #[test]
    fn domain_without_port_errors() {
        let mut cfg =
            config_with_one_service(Some(DomainSpec::Single("api.example.com".into())), None);
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
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
            }),
            ..ProxyConfig::default()
        });
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
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
            }),
            ..ProxyConfig::default()
        });
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        assert!(format!("{err}").contains("empty"), "got: {err}");
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
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        assert!(
            format!("{err}").contains("must be a JSON object"),
            "got: {err}"
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
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"), "got: {err}");
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
        let err = inject_implicit_proxy(&mut cfg).unwrap_err();
        assert!(format!("{err}").contains("reserved"), "got: {err}");
    }
}
