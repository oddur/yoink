//! Warn about services on `yoink-ingress` that can send crafted
//! packets. Caddy binds its admin API to the proxy IP on the admin
//! network. That bind is not access control on Linux: the kernel
//! accepts a packet for any local address on any interface. A
//! container on `yoink-ingress` with `NET_ADMIN` can add a route to
//! the admin network through the proxy, and one with `NET_RAW` can
//! send such frames itself. yoink's default `cap_drop: [ALL]` removes
//! both capabilities.

use super::{Config, INGRESS_NETWORK, ServiceKind};
use crate::config::RunOptions;

/// The capabilities that let a container reach the admin API through
/// its `yoink-ingress` interface.
const RISKY_CAPS: [&str; 2] = ["NET_ADMIN", "NET_RAW"];

/// `NET_RAW` is in Docker's default capability set. `NET_ADMIN` is not.
const DOCKER_DEFAULT_RISKY: [&str; 1] = ["NET_RAW"];

impl Config {
    /// One warning line per service on `yoink-ingress` that keeps
    /// `NET_ADMIN` or `NET_RAW`. Empty when the proxy is off. Pure: the
    /// config load wraps each line in `tracing::warn!`, and `yoink
    /// doctor` reports each line as a finding.
    #[must_use]
    pub fn ingress_capability_warnings(&self) -> Vec<String> {
        if !self.proxy_enabled() {
            return Vec::new();
        }
        self.services
            .iter()
            .filter(|s| s.kind != Some(ServiceKind::Proxy))
            .filter(|s| self.networks_for(s).iter().any(|n| n == INGRESS_NETWORK))
            .filter_map(|s| {
                let caps = risky_caps(&s.run.options);
                (!caps.is_empty()).then(|| {
                    format!(
                        "service {:?} is on {INGRESS_NETWORK} and has {}. With these \
                         capabilities, a process in the container can reach the Caddy \
                         admin API of the proxy, which has no authentication. Remove \
                         them from `run.options.cap_add`, or keep \
                         `run.options.cap_drop: [ALL]`.",
                        s.name,
                        caps.join(" and "),
                    )
                })
            })
            .collect()
    }
}

/// The capabilities of [`RISKY_CAPS`] that a container with `options`
/// has. Follows Docker: `cap_drop: [ALL]` starts from no capability,
/// else from the Docker default set minus `cap_drop`. Then `cap_add`
/// adds to it. `ALL` in `cap_add` adds every capability.
fn risky_caps(options: &RunOptions) -> Vec<&'static str> {
    let norm = |c: &String| {
        let upper = c.trim().to_ascii_uppercase();
        upper
            .strip_prefix("CAP_")
            .map_or(upper.clone(), str::to_string)
    };
    let dropped: Vec<String> = options.cap_drop.iter().map(norm).collect();
    let added: Vec<String> = options.cap_add.iter().map(norm).collect();
    let drop_all = dropped.iter().any(|c| c == "ALL");
    RISKY_CAPS
        .into_iter()
        .filter(|cap| {
            let in_base = !drop_all
                && DOCKER_DEFAULT_RISKY.contains(cap)
                && !dropped.iter().any(|c| c == cap);
            in_base || added.iter().any(|c| c == cap || c == "ALL")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn config(run: &str, extra_service: &str) -> Config {
        Config::parse_str(&format!(
            r"
hosts:
  - {{ address: host-a, user: deploy }}
deploy:
  networks: [app]
services:
  - name: api
    image: registry.example.com/api
    domain: api.example.test
    tls: off
    run:
      port: 8080
{run}
{extra_service}
"
        ))
        .unwrap()
    }

    #[test]
    fn default_profile_has_no_warning() {
        assert!(config("", "").ingress_capability_warnings().is_empty());
        let cfg = config("      options: { cap_add: [NET_BIND_SERVICE, CHOWN] }", "");
        assert!(cfg.ingress_capability_warnings().is_empty());
    }

    #[test]
    fn added_capabilities_are_named() {
        let cfg = config("      options: { cap_add: [net_admin, CAP_NET_RAW] }", "");
        let warnings = cfg.ingress_capability_warnings();
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0]
                .starts_with("service \"api\" is on yoink-ingress and has NET_ADMIN and NET_RAW."),
            "got: {warnings:?}"
        );
        let cfg = config("      options: { cap_add: [ALL] }", "");
        assert!(cfg.ingress_capability_warnings()[0].contains("NET_ADMIN and NET_RAW"));
    }

    /// Docker's default set has `NET_RAW` but not `NET_ADMIN`.
    #[test]
    fn opting_out_of_the_drop_keeps_net_raw() {
        let cfg = config("      options: { cap_drop: [] }", "");
        let warnings = cfg.ingress_capability_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("has NET_RAW."), "got: {warnings:?}");

        let cfg = config("      options: { cap_drop: [NET_RAW] }", "");
        assert!(cfg.ingress_capability_warnings().is_empty());
    }

    /// A service that is not routed and does not list `yoink-ingress`
    /// cannot reach the proxy. One that lists the network can.
    #[test]
    fn only_services_on_the_ingress_network_are_checked() {
        let internal = r"  - name: worker
    image: registry.example.com/worker
    networks: [app]
    run:
      options: { cap_add: [NET_ADMIN] }
  - name: sidecar
    image: registry.example.com/sidecar
    networks: [app, yoink-ingress]
    run:
      options: { cap_add: [NET_RAW] }";
        let cfg = config("    networks: [app]", internal);
        let warnings = cfg.ingress_capability_warnings();
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(warnings[0].contains("\"sidecar\""), "got: {warnings:?}");
    }
}
