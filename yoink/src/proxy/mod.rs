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
pub fn inject_implicit_proxy(cfg: &mut Config) -> Result<(), ConfigError> {
    if !proxy_enabled(cfg) {
        return Ok(());
    }

    let proxy_has_inline_cert = cfg
        .proxy
        .as_ref()
        .and_then(|p| p.tls.as_ref())
        .is_some();

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
    if cfg.services.iter().any(|s| {
        s.name == PROXY_SERVICE_NAME && !matches!(s.kind, Some(ServiceKind::Proxy))
    }) {
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
        let mut nets = svc.networks.clone().unwrap_or_else(|| cfg.deploy.networks.clone());
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
    if !cfg
        .services
        .iter()
        .any(|s| s.name == PROXY_SERVICE_NAME)
    {
        let proxy_cfg = cfg.proxy.clone().unwrap_or_default();
        cfg.services.push(synthesized_proxy_service(&proxy_cfg));
    }
    Ok(())
}

fn synthesized_proxy_service(p: &ProxyConfig) -> ServiceConfig {
    ServiceConfig {
        name: PROXY_SERVICE_NAME.to_string(),
        image: p.resolved_image(),
        kind: Some(ServiceKind::Proxy),
        build: None,
        // Tag baked into the image ref — Caddy's official tag is the
        // version (`caddy:2`); for xcaddy-built images the operator
        // pins the tag in `proxy.image:` directly.
        tag: None,
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
        run: ServiceRun {
            // Caddy serves :80 + :443 on the host. We publish those
            // because that's the whole point of the proxy. The admin
            // API on :2019 is NOT published — it's reached via
            // SSH-tunnelled access to the container's IP on the
            // `_proxy-admin` network.
            port: None,
            healthcheck_path: None,
            ..ServiceRun::default()
        },
    }
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
        assert!(api.networks.as_ref().unwrap().iter().any(|n| n == INGRESS_NETWORK));
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
