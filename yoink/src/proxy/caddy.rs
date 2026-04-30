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

    // Order services so path-constrained routes come before
    // catch-all routes for the same host. Caddy uses first-match;
    // without ordering, a `domain: example.com` (no path) before a
    // `domain: example.com, path_prefix: /api/*` would always win.
    let mut ordered: Vec<&ServiceConfig> = proxied.clone();
    ordered.sort_by_key(|s| s.path_prefix.is_none()); // false (has prefix) < true (catch-all)

    let mut routes: Vec<Value> = Vec::with_capacity(ordered.len() + 1);
    for svc in &ordered {
        // Canonical-domain redirect: when a service lists multiple
        // hosts and pins one as canonical, render a 308 redirect
        // route from the non-canonical entries before the main route.
        if let Some(redirect) = render_canonical_redirect_route(svc)? {
            routes.push(redirect);
        }
        // A service serves TLS when its own `tls:` is Auto/Cert OR
        // when proxy-level TLS is configured (every routed service
        // inherits). Off explicitly disables. HSTS only emits when
        // the operator's actually serving HTTPS.
        let tls_active = !matches!(svc.tls, TlsMode::Off) || proxy_tls.is_some();
        routes.push(render_route(
            svc,
            &container_names_for(&svc.name),
            tls_active,
        )?);
    }

    // Auto :80 → :443 redirect when proxy-level TLS is in play. Caddy
    // would normally do this via auto_https; we emit it explicitly so
    // the rendered config is self-contained and doesn't rely on
    // Caddy's auto-pilot semantics around inline certs.
    if proxy_tls.is_some() {
        routes.insert(0, redirect_route_http_to_https());
    }

    apply_global_handlers(&mut routes, cfg)?;

    let mut http_server = json!({
        "listen": [":80", ":443"],
        "routes": routes,
    });

    // mTLS lives in connection_policies, applied to every TLS handshake.
    // Caddy 2's `tls.ca_pool.source.inline` provider takes
    // base64-encoded DER certs (the bytes that sit between PEM
    // BEGIN/END markers), one entry per cert in the chain. We split
    // the trust-pool PEM into blocks here so a multi-cert bundle
    // (e.g. Cloudflare's origin-pull CA chain) round-trips correctly.
    if let Some(tls) = proxy_tls
        && let Some(client_auth) = tls.client_auth.as_ref()
    {
        let bundle = bundle.ok_or_else(|| {
            anyhow!(
                "proxy.tls.client_auth requires a [secrets] block to resolve \
                 trust_pool_secret"
            )
        })?;
        let trust_pool = bundle.get(&client_auth.trust_pool_secret).ok_or_else(|| {
            anyhow!(
                "proxy.tls.client_auth.trust_pool_secret={:?} not found in the \
                     secrets bundle",
                client_auth.trust_pool_secret,
            )
        })?;
        let der_b64_certs = pem_to_der_b64_blocks(trust_pool);
        if der_b64_certs.is_empty() {
            return Err(anyhow!(
                "proxy.tls.client_auth.trust_pool_secret={:?} has no CERTIFICATE PEM \
                 blocks",
                client_auth.trust_pool_secret,
            ));
        }
        http_server["tls_connection_policies"] = json!([{
            "client_authentication": {
                "mode": client_auth.mode.as_caddy(),
                "ca": {
                    "provider": "inline",
                    "trusted_ca_certs": der_b64_certs,
                }
            }
        }]);
    }

    let mut config = json!({
        // Same admin block as the synthesized proxy's bootstrap. Caddy
        // /load REPLACES the active config — if we omit the admin
        // block, Caddy reverts admin from `0.0.0.0:2019` to the
        // default `localhost:2019`, which breaks docker's port
        // forwarding (the docker-proxy on the host sends to the
        // container's eth0:2019, not lo). Subsequent /load calls
        // would then time out trying to reach the admin endpoint.
        "admin": {
            "listen": "0.0.0.0:2019",
            "enforce_origin": false,
        },
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
    let any_acme = proxy_tls.is_none() && proxied.iter().any(|s| matches!(s.tls, TlsMode::Auto));
    if any_acme {
        let email = cfg
            .proxy
            .as_ref()
            .and_then(|p| p.email.as_ref())
            .ok_or_else(|| {
                anyhow!(
                    "at least one service uses `tls: auto` (Let's Encrypt) but `proxy.email:` \
                     is unset — Let's Encrypt requires a registration email (or set \
                     `proxy.tls.cert_secret` for an inline cert instead)"
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

    apply_config_extra(&mut config, cfg)?;

    Ok(config)
}

/// Wrap the per-service routes in a single wildcard-match route whose
/// `handle` chain is
/// `proxy.global_handlers...,subroute(routes),proxy.global_handlers_after...`.
/// Pre-handlers run on every request before any service route matches
/// (`CrowdSec`, Coraza, fleet-wide rate-limit); post-handlers run after
/// the matched service route's per-service handlers complete. No-op
/// when both lists are empty.
fn apply_global_handlers(routes: &mut Vec<Value>, cfg: &Config) -> Result<()> {
    let (pre_snips, post_snips) =
        cfg.proxy
            .as_ref()
            .map_or((&[] as &[String], &[] as &[String]), |p| {
                (
                    p.global_handlers.as_slice(),
                    p.global_handlers_after.as_slice(),
                )
            });
    if pre_snips.is_empty() && post_snips.is_empty() {
        return Ok(());
    }
    let mut chain: Vec<Value> = Vec::new();
    for (i, raw) in pre_snips.iter().enumerate() {
        let label = format!("proxy.global_handlers[{i}]");
        chain.extend(parse_handler_snippet(raw, &label)?);
    }
    let inner_routes = std::mem::take(routes);
    chain.push(json!({
        "handler": "subroute",
        "routes": inner_routes,
    }));
    for (i, raw) in post_snips.iter().enumerate() {
        let label = format!("proxy.global_handlers_after[{i}]");
        chain.extend(parse_handler_snippet(raw, &label)?);
    }
    routes.push(json!({ "handle": chain }));
    Ok(())
}

/// Deep-merge `proxy.config_extra:` (a top-level Caddy JSON snippet)
/// over the rendered config. Extras win at leaf conflicts; yoink-managed
/// keys at non-overlapping paths survive. Validated as a JSON object
/// at config-load time. No-op when `config_extra:` is unset.
fn apply_config_extra(config: &mut Value, cfg: &Config) -> Result<()> {
    let Some(extra) = cfg.proxy.as_ref().and_then(|p| p.config_extra.as_deref()) else {
        return Ok(());
    };
    let extra_value: Value = serde_json::from_str(extra)
        .map_err(|e| anyhow!("proxy.config_extra is not valid JSON: {e}"))?;
    deep_merge(config, extra_value);
    Ok(())
}

/// Parse a single raw-JSON snippet (one entry of `caddy_extra_json:` /
/// `proxy.global_handlers:`) into the list of handlers it expands to.
/// Reused by both the per-service splice in `render_route` and the
/// global splice in `apply_global_handlers` to keep error messages
/// consistent and de-duplicate the parse-then-normalize pipeline.
fn parse_handler_snippet(raw: &str, label: &str) -> Result<Vec<Value>> {
    let parsed: Value =
        serde_json::from_str(raw).with_context(|| format!("{label} is not valid JSON"))?;
    normalize_extra_json(parsed, label)
}

/// Deep-merge `src` into `dst`. Both objects → merge keys (recurse
/// where both sides are objects, src wins on scalar leaves and arrays).
/// Non-object src wholesale replaces dst at this position.
fn deep_merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(dst_map), Value::Object(src_map)) => {
            for (k, v) in src_map {
                match dst_map.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        dst_map.insert(k, v);
                    }
                }
            }
        }
        (slot, src) => *slot = src,
    }
}

/// Expand every `caddy_extra_caddyfile:` snippet in `cfg` into its
/// JSON form (mutating the service's `caddy_extra_json` slot in
/// place). Shells out to `docker run --rm -i caddy:2 caddy adapt`
/// for each snippet — requires docker on the operator's machine.
///
/// Caller pattern: clone the config, expand, then render. Render
/// stays sync because all docker spawning happens here.
pub async fn expand_caddyfile_snippets(cfg: &mut crate::config::Config) -> anyhow::Result<()> {
    for svc in &mut cfg.services {
        let Some(caddyfile) = svc.caddy_extra_caddyfile.take() else {
            continue;
        };
        if svc.caddy_extra_json.is_some() {
            return Err(anyhow!(
                "service {:?}: caddy_extra_caddyfile and caddy_extra_json are \
                 mutually exclusive — pick one",
                svc.name,
            ));
        }
        let handlers = adapt_snippet_to_handlers(&caddyfile, &svc.name).await?;
        svc.caddy_extra_json = Some(handlers);
    }
    Ok(())
}

async fn adapt_snippet_to_handlers(snippet: &str, svc_name: &str) -> anyhow::Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    // Caddyfile requires a top-level site block; wrap the user's
    // bare directives so `caddy adapt` accepts the snippet.
    let wrapped = format!(":80 {{\n{snippet}\n}}\n");

    // `caddy adapt` doesn't support stdin via `--config -`; it
    // requires a real file. Write the wrapped snippet to a tempfile
    // inside the container, then adapt against it. Container writes
    // to /tmp (writable in the default caddy:2 image).
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "sh",
            crate::config::CADDY_DEFAULT_IMAGE,
            "-c",
            "cat > /tmp/snippet.caddyfile && \
             caddy adapt --config /tmp/snippet.caddyfile --adapter caddyfile",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!("service {svc_name:?}: spawn `docker run caddy adapt` (is docker installed?)")
        })?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(wrapped.as_bytes())
        .await
        .with_context(|| format!("service {svc_name:?}: write Caddyfile to adapt stdin"))?;
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("service {svc_name:?}: wait on caddy adapt"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "service {svc_name:?}: caddy adapt rejected the Caddyfile snippet:\n{}",
            format_adapt_failure_with_hint(&stderr),
        ));
    }

    // Adapter output is a full Caddy JSON config. Caddy may emit one
    // route per directive (e.g. `respond /robots.txt ...` becomes a
    // route with a path matcher) OR pile multiple directives into
    // one route's handle list — depends on directive ordering rules.
    // To preserve everything, take the full routes list and wrap it
    // in a single `subroute` handler, which `normalize_extra_json`
    // will then splice into the service's site-block.
    let parsed: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("service {svc_name:?}: parse caddy adapt output"))?;
    let routes = parsed
        .pointer("/apps/http/servers/srv0/routes")
        .ok_or_else(|| {
            anyhow!(
                "service {svc_name:?}: caddy adapt output didn't contain a routes list \
                 at apps/http/servers/srv0/routes. Snippet may be empty."
            )
        })?
        .clone();
    // Single subroute wrapping all of Caddy's generated routes.
    // normalize_extra_json's "handler form" branch passes this
    // through verbatim into the site-block handle array.
    let wrapped = json!([{
        "handler": "subroute",
        "routes": routes,
    }]);
    serde_json::to_string(&wrapped)
        .with_context(|| format!("service {svc_name:?}: serialize adapted handlers"))
}

/// Extract each `-----BEGIN CERTIFICATE-----` block from a PEM bundle
/// and return its base64-encoded DER body (the inner base64 content
/// without headers / whitespace). Caddy 2's `tls.ca_pool.source.inline`
/// `trusted_ca_certs` is a list of base64-DER strings — one per cert
/// in the chain — not full PEM. Other PEM types (PRIVATE KEY,
/// EC PRIVATE KEY, …) are ignored: trust pools are CERTIFICATE only.
fn pem_to_der_b64_blocks(pem: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_cert = false;
    let mut acc = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line == "-----BEGIN CERTIFICATE-----" {
            in_cert = true;
            acc.clear();
        } else if line == "-----END CERTIFICATE-----" {
            if in_cert && !acc.is_empty() {
                out.push(std::mem::take(&mut acc));
            }
            in_cert = false;
        } else if in_cert {
            acc.push_str(line);
        }
    }
    out
}

/// Build a `:443`-side route that 308-redirects every non-canonical
/// hostname listed in `domain:` to `canonical_domain:`. Returns
/// `Ok(None)` for services that don't set `canonical_domain` or that
/// only declare one hostname (nothing to redirect).
fn render_canonical_redirect_route(svc: &ServiceConfig) -> Result<Option<Value>> {
    let Some(canonical) = svc.canonical_domain.as_deref() else {
        return Ok(None);
    };
    let domains = svc
        .domain
        .as_ref()
        .ok_or_else(|| {
            anyhow!(
                "service {:?}: canonical_domain set without domain (caught by \
                 inject_implicit_proxy validation upstream)",
                svc.name,
            )
        })?
        .as_list();
    if !domains.iter().any(|d| d == canonical) {
        return Err(anyhow!(
            "service {:?}: canonical_domain={canonical:?} is not one of the entries \
             in domain: {domains:?}",
            svc.name,
        ));
    }
    let non_canonical: Vec<String> = domains.into_iter().filter(|d| d != canonical).collect();
    if non_canonical.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "match": [{"host": non_canonical}],
        "handle": [{
            "handler": "static_response",
            "status_code": 308,
            "headers": {
                "Location": [format!("https://{canonical}{{http.request.uri}}")],
            }
        }],
        "terminal": true,
    })))
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

fn render_route(svc: &ServiceConfig, containers: &[String], tls_active: bool) -> Result<Value> {
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
    // gating directives (forward_auth) run first. Routes are
    // auto-wrapped in a `subroute` handler so the operator never has
    // to know about `subroute` to inline a (match → handle).
    if let Some(extra) = svc.caddy_extra_json.as_deref() {
        let label = format!("service {:?}: caddy_extra_json", svc.name);
        for item in parse_handler_snippet(extra, &label)? {
            handle.push(item);
        }
    }

    // Optional gzip + zstd compression handler. Vanilla Caddy's
    // `encode` module — no plugin needed.
    if svc.compression {
        handle.push(json!({
            "handler": "encode",
            "encodings": {"gzip": {}, "zstd": {}},
            "prefer": ["zstd", "gzip"],
        }));
    }

    // HSTS for TLS sites. Default-on: every browser pins HTTPS-only
    // after first visit. One year + includeSubDomains is the
    // standard prod setting (preload-eligible). `hsts: false` opts
    // out for the rare mixed-protocol case.
    if svc.hsts && tls_active {
        handle.push(json!({
            "handler": "headers",
            "response": {
                "set": {
                    "Strict-Transport-Security": [
                        "max-age=31536000; includeSubDomains"
                    ]
                }
            }
        }));
    }

    let mut reverse_proxy = json!({
        "handler": "reverse_proxy",
        "upstreams": upstreams,
        "health_checks": {
            "active": {
                "uri": health_path,
                "interval": "10s",
                "timeout": "2s",
            }
        }
    });
    if svc.upstream_h2c {
        // h2c = HTTP/2 cleartext to the upstream. Required for native
        // gRPC backends (Tonic, grpc-go, grpc-java) and for
        // HTTP/2-only upstream apps. Doesn't affect what Caddy serves
        // to clients.
        reverse_proxy["transport"] = json!({
            "protocol": "http",
            "versions": ["h2c"],
        });
    }
    handle.push(reverse_proxy);

    // Match on host always; on path too when path_prefix is set.
    // Caddy's `path` matcher accepts globs (`/api/*`); we pass the
    // operator's value verbatim, so `/api/*` and `/api*` and
    // `/api/foo` all do what Caddy does with them.
    let mut matchers = serde_json::Map::new();
    matchers.insert("host".into(), json!(svc.domain.as_ref().unwrap().as_list()));
    if let Some(prefix) = &svc.path_prefix {
        matchers.insert("path".into(), json!([prefix]));
    }

    let route = json!({
        "match": [Value::Object(matchers)],
        "handle": handle,
        "terminal": true,
    });

    Ok(route)
}

/// Append a workaround hint when the adapter's stderr suggests the
/// snippet hit a plugin directive that the bundled `caddy:2` adapter
/// doesn't know. Yoink shells out to the vanilla `caddy:2` image for
/// `caddy adapt` regardless of what `proxy.xcaddy:` compiles into the
/// runtime — the adapter's directive registry is fixed, the runtime's
/// is not. Operators hit this exactly when they try to write a
/// plugin-aware `caddy_extra_caddyfile:` snippet.
fn format_adapt_failure_with_hint(stderr: &str) -> String {
    if stderr.contains("unknown directive") || stderr.contains("unrecognized directive") {
        format!(
            "{stderr}\n\nhint: `caddy_extra_caddyfile:` adapts via the \
             vanilla caddy:2 adapter, which doesn't know plugin directives. \
             Either write the same logic as `caddy_extra_json:` (skips the \
             adapter entirely), or run a custom adapter image with the plugin \
             compiled in."
        )
    } else {
        stderr.to_string()
    }
}

/// Coerce a `caddy_extra_json` / `proxy.global_handlers` parsed value
/// into a list of handler objects ready to splice into a `handle` array.
///
/// - Handler shape (`{"handler": "x", ...}`) → pass-through.
/// - Route shape (`{"match": ..., "handle": ...}`) → wrapped in a
///   single `subroute` handler. Operators get to write the natural
///   shape ("when X, do Y") without knowing `subroute` exists.
/// - Mixed lists → each entry classified independently; route-shaped
///   entries are wrapped together in one `subroute`.
///
/// `context` is a human-readable prefix included verbatim in error
/// messages — e.g. `service "api": caddy_extra_json` or
/// `proxy.global_handlers[0]`.
fn normalize_extra_json(parsed: Value, context: &str) -> Result<Vec<Value>> {
    let items: Vec<Value> = match parsed {
        Value::Array(xs) => xs,
        Value::Object(_) => vec![parsed],
        other => {
            return Err(anyhow!(
                "{context}: must be a JSON object or array of objects, got {}",
                discriminant_str(&other),
            ));
        }
    };

    let mut handlers: Vec<Value> = Vec::with_capacity(items.len());
    let mut routes_buf: Vec<Value> = Vec::new();
    for item in items {
        let obj = match item.as_object() {
            Some(_) => item,
            None => {
                return Err(anyhow!(
                    "{context}: entries must be JSON objects (a handler or a route), got {}",
                    discriminant_str(&item),
                ));
            }
        };
        if obj.get("handler").is_some() {
            // Flush any pending routes into a subroute, then append
            // this handler — preserves user-written ordering.
            if !routes_buf.is_empty() {
                handlers.push(json!({
                    "handler": "subroute",
                    "routes": std::mem::take(&mut routes_buf),
                }));
            }
            handlers.push(obj);
        } else if obj.get("match").is_some() || obj.get("handle").is_some() {
            routes_buf.push(obj);
        } else {
            return Err(anyhow!(
                "{context}: entry has neither `handler` (handler form) nor \
                 `match`/`handle` (route form): {obj}",
            ));
        }
    }
    if !routes_buf.is_empty() {
        handlers.push(json!({
            "handler": "subroute",
            "routes": routes_buf,
        }));
    }
    Ok(handlers)
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
        // forward_auth (operator), headers (HSTS, default-on), reverse_proxy.
        assert_eq!(handlers.len(), 3);
        assert_eq!(handlers[0]["handler"], "forward_auth");
        assert_eq!(handlers[1]["handler"], "headers");
        assert_eq!(handlers[2]["handler"], "reverse_proxy");
    }

    #[test]
    fn hsts_default_on_for_tls_auto() {
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
        let json = render(&cfg, |_| vec![], None).expect("render");
        let handlers = json["apps"]["http"]["servers"]["main"]["routes"][0]["handle"]
            .as_array()
            .unwrap();
        let hsts = handlers
            .iter()
            .find(|h| h["handler"] == "headers")
            .expect("HSTS handler should be present");
        let value = &hsts["response"]["set"]["Strict-Transport-Security"][0];
        assert!(
            value.as_str().unwrap().contains("max-age=31536000"),
            "got: {value}"
        );
    }

    #[test]
    fn hsts_skipped_for_tls_off() {
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
    tls: off
    run: { port: 8080 }
"#,
        );
        // Force the proxy block in (no TLS sites + no email might
        // otherwise trip a different validation path).
        cfg.proxy = Some(ProxyConfig {
            email: Some("ops@example.com".into()),
            ..Default::default()
        });
        let json = render(&cfg, |_| vec![], None).expect("render");
        let handlers = json["apps"]["http"]["servers"]["main"]["routes"][0]["handle"]
            .as_array()
            .unwrap();
        assert!(
            handlers.iter().all(|h| h["handler"] != "headers"),
            "no HSTS expected when tls: off"
        );
    }

    #[test]
    fn hsts_opt_out_via_field() {
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
    hsts: false
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec![], None).expect("render");
        let handlers = json["apps"]["http"]["servers"]["main"]["routes"][0]["handle"]
            .as_array()
            .unwrap();
        assert!(handlers.iter().all(|h| h["handler"] != "headers"));
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
        assert!(
            entries[0]["certificate"]
                .as_str()
                .unwrap()
                .starts_with("-----BEGIN CERT")
        );
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
        values.insert(
            "CF_CERT".to_string(),
            "-----BEGIN CERTIFICATE-----\n".into(),
        );
        values.insert("CF_KEY".to_string(), "-----BEGIN PRIVATE KEY-----\n".into());
        values.insert(
            "CF_CA".to_string(),
            "-----BEGIN CERTIFICATE-----\nMIIBfakeCAder\n-----END CERTIFICATE-----\n".into(),
        );
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
        assert_eq!(
            policies[0]["client_authentication"]["ca"]["provider"].as_str(),
            Some("inline")
        );
        assert_eq!(
            policies[0]["client_authentication"]["ca"]["trusted_ca_certs"][0].as_str(),
            Some("MIIBfakeCAder")
        );

        // First route is the :80 → :443 redirect.
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .unwrap();
        assert_eq!(routes[0]["match"][0]["protocol"].as_str(), Some("http"));
        assert_eq!(routes[0]["handle"][0]["status_code"].as_i64(), Some(308));

        // Inline cert is present (one entry covers everything).
        let pem = &json["apps"]["tls"]["certificates"]["load_pem"];
        assert_eq!(pem.as_array().unwrap().len(), 1);
        assert_eq!(pem[0]["tags"][0].as_str(), Some("yoink:proxy-default"));

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

    #[test]
    fn config_extra_merges_into_rendered_config() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy:
  email: ops@example.com
  config_extra: |
    {
      "storage": {"module": "redis", "address": "redis:6379"},
      "apps": {
        "http": {
          "servers": {
            "main": {
              "trusted_proxies": {"source": "cloudflare"},
              "client_ip_headers": ["CF-Connecting-IP"]
            }
          }
        }
      }
    }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        // Top-level escape hatch keys land where the user put them.
        assert_eq!(json["storage"]["module"].as_str(), Some("redis"));
        assert_eq!(
            json["apps"]["http"]["servers"]["main"]["trusted_proxies"]["source"].as_str(),
            Some("cloudflare"),
        );
        assert_eq!(
            json["apps"]["http"]["servers"]["main"]["client_ip_headers"][0].as_str(),
            Some("CF-Connecting-IP"),
        );
        // Yoink-managed siblings under the same `main` server survive
        // the merge — routes were not clobbered.
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .expect("routes array");
        assert!(!routes.is_empty(), "yoink routes preserved across merge");
    }

    #[test]
    fn global_handlers_wrap_service_routes_in_subroute() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy:
  email: ops@example.com
  global_handlers:
    - '{"handler": "crowdsec", "appsec_url": "http://crowdsec:8080"}'
    - '{"handler": "waf", "directives": ["SecRuleEngine On"]}'
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .expect("routes array");
        // With global_handlers set, every yoink-managed route lives
        // inside a single wildcard wrapper.
        assert_eq!(routes.len(), 1, "wrapped into one route");
        let wrapper_handle = routes[0]["handle"].as_array().expect("wrapper handle");
        // Order: crowdsec, waf, then a subroute carrying the original
        // service routes.
        assert_eq!(wrapper_handle[0]["handler"].as_str(), Some("crowdsec"));
        assert_eq!(wrapper_handle[1]["handler"].as_str(), Some("waf"));
        assert_eq!(wrapper_handle[2]["handler"].as_str(), Some("subroute"));
        let inner_routes = wrapper_handle[2]["routes"]
            .as_array()
            .expect("inner subroute carries the service routes");
        assert!(
            inner_routes
                .iter()
                .any(|r| r["match"][0]["host"][0].as_str() == Some("api.example.com")),
            "service route preserved inside the subroute",
        );
    }

    #[test]
    fn global_handlers_after_runs_after_subroute() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy:
  email: ops@example.com
  global_handlers:
    - '{"handler": "crowdsec"}'
  global_handlers_after:
    - '{"handler": "log_append"}'
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .expect("routes array");
        assert_eq!(routes.len(), 1, "wrapped into one route");
        let chain = routes[0]["handle"].as_array().expect("wrapper handle");
        // Expected order: pre handler → subroute → post handler.
        assert_eq!(chain[0]["handler"].as_str(), Some("crowdsec"));
        assert_eq!(chain[1]["handler"].as_str(), Some("subroute"));
        assert_eq!(chain[2]["handler"].as_str(), Some("log_append"));
    }

    #[test]
    fn global_handlers_after_alone_still_wraps() {
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy:
  email: ops@example.com
  global_handlers_after:
    - '{"handler": "log_append"}'
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        let routes = json["apps"]["http"]["servers"]["main"]["routes"]
            .as_array()
            .expect("routes array");
        assert_eq!(routes.len(), 1, "wrapped into one route");
        let chain = routes[0]["handle"].as_array().expect("wrapper handle");
        assert_eq!(chain[0]["handler"].as_str(), Some("subroute"));
        assert_eq!(chain[1]["handler"].as_str(), Some("log_append"));
    }

    #[test]
    fn per_route_handler_order_is_extra_json_then_compression_then_hsts_then_proxy() {
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
    compression: true
    hsts: true
    caddy_extra_json: '{"handler": "forward_auth"}'
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        let route = &json["apps"]["http"]["servers"]["main"]["routes"][0];
        let handlers = route["handle"]
            .as_array()
            .expect("route handle is array")
            .iter()
            .map(|h| h["handler"].as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        // Hand-written snippet first, then yoink's auto-added
        // compression + HSTS, then the reverse_proxy at the end.
        let auth_idx = handlers
            .iter()
            .position(|h| *h == "forward_auth")
            .expect("forward_auth in chain");
        let encode_idx = handlers
            .iter()
            .position(|h| *h == "encode")
            .expect("encode in chain");
        let headers_idx = handlers
            .iter()
            .position(|h| *h == "headers")
            .expect("headers (HSTS) in chain");
        let proxy_idx = handlers
            .iter()
            .position(|h| *h == "reverse_proxy")
            .expect("reverse_proxy in chain");
        assert!(
            auth_idx < encode_idx,
            "snippet runs before compression: {handlers:?}",
        );
        assert!(
            encode_idx < headers_idx,
            "compression runs before HSTS: {handlers:?}",
        );
        assert!(
            headers_idx < proxy_idx,
            "HSTS runs before reverse_proxy: {handlers:?}",
        );
    }

    #[test]
    fn adapt_failure_hint_steers_at_caddy_extra_json_for_plugin_directive() {
        let stderr = "Caddyfile:1: unknown directive: rate_limit";
        let formatted = format_adapt_failure_with_hint(stderr);
        assert!(formatted.contains(stderr));
        assert!(formatted.contains("hint:"));
        assert!(formatted.contains("caddy_extra_json"));
    }

    #[test]
    fn adapt_failure_hint_silent_for_unrelated_errors() {
        let stderr = "Caddyfile:1: syntax error: unexpected `{`";
        let formatted = format_adapt_failure_with_hint(stderr);
        assert!(formatted.contains(stderr));
        assert!(!formatted.contains("hint:"));
    }

    #[test]
    fn config_extra_user_keys_win_on_conflict() {
        // User explicitly overrides yoink's default admin block.
        let cfg = parse(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy:
  email: ops@example.com
  config_extra: |
    {"admin": {"listen": "127.0.0.1:9999"}}
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        );
        let json = render(&cfg, |_| vec!["api-1".into()], None).expect("render");
        assert_eq!(json["admin"]["listen"].as_str(), Some("127.0.0.1:9999"));
    }
}
