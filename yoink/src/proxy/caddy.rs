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
pub fn render<F>(
    cfg: &Config,
    container_names_for: F,
    bundle: Option<&SecretsBundle>,
) -> Result<Value>
where
    F: Fn(&str) -> Vec<String>,
{
    let proxied: Vec<&ServiceConfig> = cfg.services.iter().filter(|s| is_proxied(s)).collect();

    let mut routes: Vec<Value> = Vec::with_capacity(proxied.len());
    for svc in &proxied {
        routes.push(render_route(svc, &container_names_for(&svc.name))?);
    }

    let mut http_server = json!({
        "listen": [":80", ":443"],
        "routes": routes,
    });

    let mut config = json!({
        "apps": {
            "http": {
                "servers": {
                    "main": http_server.take(),
                }
            }
        }
    });

    // ACME automation, only when at least one service uses `tls: auto`.
    let any_acme = proxied.iter().any(|s| matches!(s.tls, TlsMode::Auto));
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

    // Inline certificates for `tls: cert` services. We collect them
    // into a single `load_pem` list rather than one per service so
    // Caddy's matcher dedup is straightforward.
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
        let mut pem_entries: Vec<Value> = Vec::new();
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

        // Merge with any existing `apps.tls` (from ACME above).
        let tls = config["apps"].as_object_mut().unwrap();
        let entry = tls.entry("tls").or_insert_with(|| json!({}));
        entry["certificates"] = json!({ "load_pem": pem_entries });
    }

    Ok(config)
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
