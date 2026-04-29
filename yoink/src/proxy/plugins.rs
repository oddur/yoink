//! Static map of well-known Caddy handler / app names to the Go module
//! path that provides them. Used by `inject_implicit_proxy` to surface
//! a soft warning when a `caddy_extra_json:` / `proxy.global_handlers:`
//! snippet references a plugin handler whose Go module isn't in
//! `proxy.xcaddy.plugins:` — the user would otherwise discover this at
//! `caddy /load` time on the host with a generic
//! `unrecognized handler module: <name>` error.
//!
//! Conservative on purpose: only the handler names mentioned in the
//! `caddy-plugins.md` recipe are listed. Unknown names get no warning
//! (no false positives), so adding a fringe plugin never regresses
//! anyone's deploy.

/// Caddy handler / app name → expected Go-module-path substring. The
/// substring match handles variants like `github.com/foo/bar/v3`
/// (versioned subpaths) without a separate entry per major version.
pub const HANDLER_TO_MODULE: &[(&str, &str)] = &[
    ("crowdsec", "github.com/hslatman/caddy-crowdsec-bouncer"),
    ("appsec", "github.com/hslatman/caddy-crowdsec-bouncer"),
    ("waf", "github.com/corazawaf/coraza-caddy"),
    ("rate_limit", "github.com/mholt/caddy-ratelimit"),
    ("cache", "github.com/caddyserver/cache-handler"),
    ("authenticate", "github.com/greenpau/caddy-security"),
    ("authorize", "github.com/greenpau/caddy-security"),
    ("layer4", "github.com/mholt/caddy-l4"),
];

/// `Some(module-path)` if `name` is a recognised handler/app whose
/// implementing Go module yoink can identify; `None` otherwise. The
/// `None` case is the silent path — yoink never warns about unknown
/// handler names, so adding a new fringe plugin doesn't generate noise.
#[must_use]
pub fn handler_module(name: &str) -> Option<&'static str> {
    HANDLER_TO_MODULE
        .iter()
        .find_map(|(handler, module)| (*handler == name).then_some(*module))
}

/// `true` when `plugins` (the `proxy.xcaddy.plugins` list, in
/// `module[@version]` form) contains an entry whose module path
/// includes `module_path` as a substring. Substring match because
/// `xcaddy build --with` accepts any of `github.com/foo/bar`,
/// `github.com/foo/bar@v1.2.3`, or `github.com/foo/bar/v3` — the user-
/// supplied form varies but the canonical module path is stable.
#[must_use]
pub fn plugins_contain(plugins: &[String], module_path: &str) -> bool {
    plugins.iter().any(|p| p.contains(module_path))
}

/// One warning line per handler reference in `extra_json` whose required
/// Go module isn't declared in `xcaddy_plugins`. `extra_json` is a
/// parsed JSON value (object, array, or scalar) — the function walks
/// it for `handler` keys at any depth. `context` is included verbatim
/// in the warning prefix (e.g. `service "api": caddy_extra_json` or
/// `proxy.global_handlers[2]`). Pure for unit-testability.
#[must_use]
pub fn missing_plugin_warnings(
    extra_json: &serde_json::Value,
    xcaddy_plugins: &[String],
    context: &str,
) -> Vec<String> {
    let mut handlers_seen: Vec<&str> = Vec::new();
    collect_handler_names(extra_json, &mut handlers_seen);
    handlers_seen
        .into_iter()
        .filter_map(|name| {
            handler_module(name).and_then(|module| {
                (!plugins_contain(xcaddy_plugins, module)).then(|| {
                    format!(
                        "{context} references handler {name:?} but \
                         proxy.xcaddy.plugins doesn't contain {module:?} — \
                         caddy /load will reject this with `unrecognized \
                         handler module: {name}` at deploy time"
                    )
                })
            })
        })
        .collect()
}

/// Recursively walk `value` collecting every `"handler"` string-valued
/// field. Both arrays and objects are traversed; non-string handler
/// values are silently skipped (Caddy itself rejects them at /load).
fn collect_handler_names<'v>(value: &'v serde_json::Value, out: &mut Vec<&'v str>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(name)) = map.get("handler") {
                out.push(name.as_str());
            }
            for (_, v) in map {
                collect_handler_names(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_handler_names(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handler_module_known_names() {
        assert_eq!(
            handler_module("crowdsec"),
            Some("github.com/hslatman/caddy-crowdsec-bouncer"),
        );
        assert_eq!(
            handler_module("waf"),
            Some("github.com/corazawaf/coraza-caddy"),
        );
        assert_eq!(handler_module("authenticate"), handler_module("authorize"));
    }

    #[test]
    fn handler_module_unknown_silent() {
        assert!(handler_module("not_a_known_handler").is_none());
        assert!(handler_module("reverse_proxy").is_none()); // built-in, not a plugin
    }

    #[test]
    fn plugins_contain_substring_match() {
        let plugins = vec![
            "github.com/corazawaf/coraza-caddy/v3@v3.1.0".to_string(),
            "github.com/mholt/caddy-ratelimit".to_string(),
        ];
        assert!(plugins_contain(
            &plugins,
            "github.com/corazawaf/coraza-caddy"
        ));
        assert!(plugins_contain(
            &plugins,
            "github.com/mholt/caddy-ratelimit"
        ));
        assert!(!plugins_contain(
            &plugins,
            "github.com/hslatman/caddy-crowdsec-bouncer"
        ));
    }

    #[test]
    fn missing_plugin_warnings_flags_unbacked_handler() {
        let snippet = json!({"handler": "crowdsec", "appsec_url": "http://crowdsec:8080"});
        let plugins: Vec<String> = vec![];
        let warnings = missing_plugin_warnings(&snippet, &plugins, "ctx");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ctx"));
        assert!(warnings[0].contains("crowdsec"));
        assert!(warnings[0].contains("hslatman/caddy-crowdsec-bouncer"));
    }

    #[test]
    fn missing_plugin_warnings_silent_when_plugin_present() {
        let snippet = json!({"handler": "crowdsec"});
        let plugins = vec!["github.com/hslatman/caddy-crowdsec-bouncer@v0.7.0".to_string()];
        assert!(missing_plugin_warnings(&snippet, &plugins, "ctx").is_empty());
    }

    #[test]
    fn missing_plugin_warnings_silent_for_unknown_handler() {
        // `reverse_proxy` is built-in and not in the map. No warning.
        let snippet = json!({"handler": "reverse_proxy"});
        let plugins: Vec<String> = vec![];
        assert!(missing_plugin_warnings(&snippet, &plugins, "ctx").is_empty());
    }

    #[test]
    fn missing_plugin_warnings_walks_array_and_nested_objects() {
        let snippet = json!([
            {"handler": "subroute", "routes": [{"handle": [{"handler": "crowdsec"}]}]},
            {"handler": "waf"},
        ]);
        let plugins = vec!["github.com/hslatman/caddy-crowdsec-bouncer".to_string()];
        let warnings = missing_plugin_warnings(&snippet, &plugins, "ctx");
        // `crowdsec` is backed (no warning); `waf` isn't (one warning).
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("waf"));
    }
}
