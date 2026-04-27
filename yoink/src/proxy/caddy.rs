//! Render a [`Config`] into the JSON document Caddy's admin API
//! `/load` endpoint accepts.
//!
//! See <https://caddyserver.com/docs/json/> for the schema. The shape
//! we emit is deliberately the smallest one that covers yoink's
//! routing primitives:
//!
//! - One HTTP server (`apps.http.servers.main`) listening on `:80` +
//!   `:443`.
//! - One route per service-with-`domain:`, matching by host header
//!   and forwarding to the service's container DNS name on the
//!   ingress network.
//! - Optional ACME automation when any service uses `tls: auto`.
//! - Optional inline `tls.certificates.load_pem` entries for
//!   `tls: cert` services (e.g. Cloudflare Origin Certs).
//!
//! Any directives beyond this surface — auth, rate limiting, headers
//! — go through `caddy_extra_json:`, which is merged verbatim into
//! the route's `handle` array immediately before the auto-generated
//! `reverse_proxy` handler. Yoink does not validate the JSON; Caddy
//! does, on `/load`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::config::{Config, ServiceConfig, TlsMode};
use crate::secrets::SecretsBundle;

use super::is_proxied;

/// Render `cfg` into the Caddy admin-API JSON config. Container names
/// in the upstream pool come from `container_names_for(service_name)`
/// — one entry per replica, in deterministic order. Cert/key bytes
/// for `tls: cert` services come from `bundle`; an `Err` is returned
/// if a referenced secret is missing.
#[allow(clippy::too_many_lines)] // mostly straight-line JSON assembly
pub fn render<F>(
    cfg: &Config,
    container_names_for: F,
    bundle: Option<&SecretsBundle>,
) -> Result<Value>
where
    F: Fn(&str) -> Vec<String>,
{
    let proxy_tls = cfg.proxy.as_ref().and_then(|p| p.tls.as_ref());
    let proxied: Vec<&ServiceConfig> = cfg.services.iter().filter(|s| is_proxied(s)).collect();

    let mut routes: Vec<Value> = Vec::with_capacity(proxied.len() + 1);
    for svc in &proxied {
        routes.push(render_route(svc, &container_names_for(&svc.name))?);
    }

    // Auto :80 → :443 redirect when proxy-level TLS is in play. Caddy
    // would normally do this via auto_https; we emit it explicitly so
    // the rendered config is self-contained and doesn't rely on
    // Caddy's auto-pilot semantics around inline certs.
    if proxy_tls.is_some() {
        routes.insert(0, redirect_route_http_to_https());
    }

    let mut http_server = json!({
        "listen": [":80", ":443"],
        "routes": routes,
    });

    // mTLS lives in connection_policies, applied to every TLS handshake.
    if let Some(tls) = proxy_tls
        && let Some(client_auth) = tls.client_auth.as_ref()
    {
        let bundle = bundle.ok_or_else(|| {
            anyhow!(
                "proxy.tls.client_auth requires a [secrets] block to resolve \
                 trust_pool_secret"
            )
        })?;
        let trust_pool = bundle
            .get(&client_auth.trust_pool_secret)
            .ok_or_else(|| {
                anyhow!(
                    "proxy.tls.client_auth.trust_pool_secret={:?} not found in the \
                     secrets bundle",
                    client_auth.trust_pool_secret,
                )
            })?;
        http_server["tls_connection_policies"] = json!([{
            "client_authentication": {
                "mode": client_auth.mode.as_caddy(),
                "trusted_ca_certs_pem": [trust_pool],
            }
        }]);
    }

    let mut config = json!({
        "apps": {
            "http": {
                "servers": {
                    "main": http_server.take(),
                }
            }
        }
    });

    // ACME is only for services that genuinely want it AND aren't
    // covered by a proxy-level inline cert. Proxy-level cert disables
    // ACME entirely (we never want both running on the same domains).
    let any_acme = proxy_tls.is_none()
        && proxied.iter().any(|s| matches!(s.tls, TlsMode::Auto));
    if any_acme {
        let email = cfg
            .proxy
            .as_ref()
            .and_then(|p| p.email.as_ref())
            .ok_or_else(|| {
                anyhow!(
                    "rendering ACME policy requires `proxy.email:` — should have been \
                     caught by inject_implicit_proxy validation"
                )
            })?;
        config["apps"]["tls"] = json!({
            "automation": {
                "policies": [{
                    "issuers": [{
                        "module": "acme",
                        "email": email,
                    }]
                }]
            }
        });
    }

    // Resolve the inline cert(s). Two sources merge into one
    // `load_pem` list:
    //   - proxy.tls.cert_secret/key_secret (covers every routed
    //     service that doesn't override).
    //   - per-service tls_cert_secret/tls_key_secret (the override).
    let mut pem_entries: Vec<Value> = Vec::new();

    if let Some(tls) = proxy_tls {
        let bundle = bundle.ok_or_else(|| {
            anyhow!("proxy.tls.cert_secret requires a [secrets] block to resolve")
        })?;
        let cert = bundle.get(&tls.cert_secret).ok_or_else(|| {
            anyhow!(
                "proxy.tls.cert_secret={:?} not found in the secrets bundle",
                tls.cert_secret,
            )
        })?;
        let key = bundle.get(&tls.key_secret).ok_or_else(|| {
            anyhow!(
                "proxy.tls.key_secret={:?} not found in the secrets bundle",
                tls.key_secret,
            )
        })?;
        pem_entries.push(json!({
            "certificate": cert,
            "key": key,
            "tags": ["yoink:proxy-default"],
        }));
    }

    let cert_services: Vec<&&ServiceConfig> = proxied
        .iter()
        .filter(|s| matches!(s.tls, TlsMode::Cert))
        .collect();
    if !cert_services.is_empty() {
        let bundle = bundle.ok_or_else(|| {
            anyhow!(
                "{} service(s) use `tls: cert` but no [secrets] block is configured",
                cert_services.len(),
            )
        })?;
        for svc in &cert_services {
            let cert_name = svc.tls_cert_secret.as_deref().expect("validated upstream");
            let key_name = svc.tls_key_secret.as_deref().expect("validated upstream");
            let cert = bundle.get(cert_name).ok_or_else(|| {
                anyhow!(
                    "service {:?} references tls_cert_secret={cert_name:?} but the \
                     secrets bundle has no such key",
                    svc.name,
                )
            })?;
            let key = bundle.get(key_name).ok_or_else(|| {
                anyhow!(
                    "service {:?} references tls_key_secret={key_name:?} but the \
                     secrets bundle has no such key",
                    svc.name,
                )
            })?;
            pem_entries.push(json!({
                "certificate": cert,
                "key": key,
                "tags": [format!("yoink:{}", svc.name)],
            }));
        }
    }

    if !pem_entries.is_empty() {
        let tls = config["apps"].as_object_mut().unwrap();
        let entry = tls.entry("tls").or_insert_with(|| json!({}));
        entry["certificates"] = json!({ "load_pem": pem_entries });
    }

    Ok(config)
}

fn redirect_route_http_to_https() -> Value {
    // Match :80 only; rewrite scheme + 308 redirect. Caddy's `redir`
    // handler is the JSON shape below.
    json!({
        "match": [{"protocol": "http"}],
        "handle": [{
            "handler": "static_response",
            "status_code": 308,
            "headers": {
                "Location": ["https://{http.request.host}{http.request.uri}"],
            }
        }],
        "terminal": true,
    })
}

fn render_route(svc: &ServiceConfig, containers: &[String]) -> Result<Value> {
    let port = svc.run.port.ok_or_else(|| {
        anyhow!(
            "service {:?} has `domain:` but no `run.port` (should have been caught \
             by inject_implicit_proxy)",
            svc.name,
        )
    })?;

    let upstreams: Vec<Value> = if containers.is_empty() {
        // No live container yet (e.g. first deploy, /load before
        // create). Use the service name as a single-host fallback so
        // Caddy's active health check still sees the upstream when
        // it appears.
        vec![json!({"dial": format!("{}:{port}", svc.name)})]
    } else {
        containers
            .iter()
            .map(|c| json!({"dial": format!("{c}:{port}")}))
            .collect()
    };

    let health_path = svc
        .run
        .healthcheck_path
        .clone()
        .unwrap_or_else(|| "/".to_string());

    let mut handle: Vec<Value> = Vec::new();

    // Operator-supplied snippet. Append before the reverse_proxy so
    // gating directives (forward_auth) run first.
    if let Some(extra) = svc.caddy_extra_json.as_deref() {
        let parsed: Value = serde_json::from_str(extra).with_context(|| {
            format!(
                "service {:?}: caddy_extra_json is not valid JSON",
                svc.name,
            )
        })?;
        // Accept either a single object (one handler) or a list.
        match parsed {
            Value::Array(xs) => handle.extend(xs),
            Value::Object(_) => handle.push(parsed),
            other => {
                return Err(anyhow!(
                    "service {:?}: caddy_extra_json must be a JSON object or array of \
                     objects, got {}",
                    svc.name,
                    discriminant_str(&other),
                ));
            }
        }
    }

    handle.push(json!({
        "handler": "reverse_proxy",
        "upstreams": upstreams,
        "health_checks": {
            "active": {
                "uri": health_path,
                "interval": "10s",
                "timeout": "2s",
            }
        }
    }));

    let host_match = json!({"host": svc.domain.as_ref().unwrap().as_list()});

    let mut route = json!({
        "match": [host_match],
        "handle": handle,
        "terminal": true,
    });

    if matches!(svc.tls, TlsMode::Off) {
        route["@yoink_tls_off"] = json!(true);
    }

    Ok(route)
}

fn discriminant_str(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Compute a stable hash of the rendered Caddy JSON for use in the
/// `_proxy` service's `yoink.spec_hash` label. Drift detection re-uses
/// the same `compute_spec_hash` plumbing as every other service.
///
/// `serde_json::to_string` of a `serde_json::Value` is deterministic
/// for the JSON shapes we render (BTreeMap-backed objects), so this
/// hash is stable across runs given the same input.
#[must_use]
pub fn config_fingerprint(json: &Value) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let canonical = serde_json::to_string(json).expect("rendered JSON always serializes");
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Per-host map of (service name → [container name, …]). Used as the
/// `container_names_for` input to [`render`]. Empty for first-deploy
/// when the route is being pushed before any container exists.
pub type UpstreamMap = BTreeMap<String, Vec<String>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    fn parse(yaml: &str) -> Config {
        Config::parse_str(yaml).expect("parse")
    }

    #[test]
    fn vanilla_acme_service_renders() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy: { email: ops@example.com }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let upstreams = |s: &str| {
            if s == "api" {
                vec!["api-1".to_string()]
            } else {
                vec![]
            }
        };
        let json = render(&cfg, upstreams, None).expect("render");
        // ACME issuer present
        let issuer = &json["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["module"];
        assert_eq!(issuer.as_str(), Some("acme"));
        // Route has the right host + upstream
        let route = &json["apps"]["http"]["servers"]["main"]["routes"][0];
        assert_eq!(
            route["match"][0]["host"][0].as_str(),
            Some("api.example.com")
        );
        let dial = &route["handle"].as_array().unwrap().last().unwrap()["upstreams"][0]["dial"];
        assert_eq!(dial.as_str(), Some("api-1:8080"));
    }

    #[test]
    fn caddy_extra_json_array_renders_before_reverse_proxy() {
        let mut cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy: { email: ops@example.com }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    caddy_extra_json: '[{"handler":"forward_auth","upstreams":[{"dial":"auth:9091"}]}]'
    run: { port: 8080 }
"#,
        );
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            ..Default::default()
        });
        let json = render(&cfg, |_| vec![], None).expect("render");
        let handlers = json["apps"]["http"]["servers"]["main"]["routes"][0]["handle"]
            .as_array()
            .unwrap();
        assert_eq!(handlers.len(), 2);
        assert_eq!(handlers[0]["handler"], "forward_auth");
        assert_eq!(handlers[1]["handler"], "reverse_proxy");
    }

    #[test]
    fn cert_path_inlines_secrets() {
        let yaml = r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
secrets: { provider: age, recipients: [age1xxxxx] }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    tls: cert
    tls_cert_secret: CF_CERT
    tls_key_secret: CF_KEY
    run: { port: 8080 }
"#;
        let cfg = parse(yaml);
        let mut values = std::collections::BTreeMap::new();
        values.insert("CF_CERT".to_string(), "-----BEGIN CERT-----\n".to_string());
        values.insert("CF_KEY".to_string(), "-----BEGIN KEY-----\n".to_string());
        let bundle = SecretsBundle::new(values);
        let json = render(&cfg, |_| vec!["api-1".into()], Some(&bundle)).expect("render");
        let entries = &json["apps"]["tls"]["certificates"]["load_pem"];
        assert_eq!(entries.as_array().unwrap().len(), 1);
        assert!(entries[0]["certificate"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN CERT"));
    }

    #[test]
    fn proxy_tls_with_mtls_renders_connection_policies_and_redirect() {
        let yaml = r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
secrets: { provider: age, recipients: [age1xxxxx] }
proxy:
  tls:
    cert_secret: CF_CERT
    key_secret: CF_KEY
    client_auth:
      mode: require_and_verify
      trust_pool_secret: CF_CA
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
  - name: web
    image: img
    tag: t
    domain: [example.com, www.example.com]
    run: { port: 3000 }
"#;
        let cfg = parse(yaml);
        let mut values = std::collections::BTreeMap::new();
        values.insert("CF_CERT".to_string(), "-----BEGIN CERTIFICATE-----\n".into());
        values.insert("CF_KEY".to_string(), "-----BEGIN PRIVATE KEY-----\n".into());
        values.insert("CF_CA".to_string(), "-----BEGIN CERTIFICATE-----CA\n".into());
        let bundle = SecretsBundle::new(values);
        let json = render(&cfg, |_| vec!["api-1".into()], Some(&bundle)).expect("render");

        // No ACME — proxy-level cert overrides it.
        assert!(json["apps"]["tls"]["automation"].is_null());

        // mTLS connection_policies present.
        let policies = &json["apps"]["http"]["servers"]["main"]["tls_connection_policies"];
        assert_eq!(
            policies[0]["client_authentication"]["mode"].as_str(),
            Some("require_and_verify")
        );
        assert!(policies[0]["client_authentication"]["trusted_ca_certs_pem"][0]
            .as_str()
            .unwrap()
            .contains("CA"));

        // First route is the :80 → :443 redirect.
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .unwrap();
        assert_eq!(
            routes[0]["match"][0]["protocol"].as_str(),
            Some("http")
        );
        assert_eq!(routes[0]["handle"][0]["status_code"].as_i64(), Some(308));

        // Inline cert is present (one entry covers everything).
        let pem = &json["apps"]["tls"]["certificates"]["load_pem"];
        assert_eq!(pem.as_array().unwrap().len(), 1);
        assert_eq!(
            pem[0]["tags"][0].as_str(),
            Some("yoink:proxy-default")
        );

        // Both services routed (api + web).
        let host_routes: Vec<&Value> = routes.iter().skip(1).collect();
        assert_eq!(host_routes.len(), 2);
    }

    #[test]
    fn proxy_tls_without_acme_skips_email_requirement() {
        let yaml = r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
secrets: { provider: age, recipients: [age1xxxxx] }
proxy:
  tls:
    cert_secret: CF_CERT
    key_secret: CF_KEY
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#;
        // No proxy.email: but no ACME because proxy.tls is set.
        let cfg = parse(yaml);
        let mut values = std::collections::BTreeMap::new();
        values.insert("CF_CERT".to_string(), "cert".into());
        values.insert("CF_KEY".to_string(), "key".into());
        let json = render(&cfg, |_| vec![], Some(&SecretsBundle::new(values))).expect("render");
        assert!(json["apps"]["tls"]["automation"].is_null());
        assert!(json["apps"]["tls"]["certificates"]["load_pem"][0].is_object());
    }

    #[test]
    fn fingerprint_stable_across_runs() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy: { email: ops@example.com }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json1 = render(&cfg, |_| vec!["api-1".into()], None).unwrap();
        let json2 = render(&cfg, |_| vec!["api-1".into()], None).unwrap();
        assert_eq!(config_fingerprint(&json1), config_fingerprint(&json2));
    }
}
