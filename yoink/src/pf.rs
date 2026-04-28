//! `yoink pf` — laptop-to-container port-forwarding via the same
//! `ssh -L` mechanism the image-push tunnel uses.
//!
//! Scope is deliberately narrow: yoink only forwards services that
//! already declare a `publish:` block in yoink.yaml. The host's
//! `docker-proxy` is already listening on the published `host_ip:
//! host_port` (typically `127.0.0.1:N`); we just bridge the laptop
//! to that listener with a single `ssh -L` channel. No probe
//! container, no socat sidecar, no extra plumbing for caller code.
//!
//! Services without `publish:` (the api/web pattern — only reachable
//! via the bundled Caddy proxy on :443) error out with a message
//! pointing at `yoink shell` for the docker-exec workflow that
//! actually fits that case.
//!
//! Two surfaces consume this module:
//!   - the CLI `yoink pf <service> [LOCAL:]CONTAINER_PORT` command
//!     in `main.rs` (foreground, holds until SIGINT)
//!   - the TUI `f` key affordance in `tui::app` (binds in the
//!     background, footer band shows the active forwards, `o`
//!     opens the URL in the system browser, `c` copies it)

use std::time::Duration;

use thiserror::Error;

use crate::config::{Config, ServiceConfig};

/// How long we'll wait for the SSH tunnel's local listener to come
/// up before giving up. The `ssh -L` child binds locally before
/// auth completes, so this is mostly waiting on cold-DNS / cold-TCP
/// to the SSH host. Same default as the image-push tunnel.
pub const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Operator-visible URL scheme override. `Auto` runs the
/// port-number heuristic (`default_scheme`); `Http`/`Https` force
/// the obvious wrapper; `Tcp`/`None` print bare `localhost:N` and
/// suppress browser-open. Backed by a `clap::ValueEnum` derive at
/// the CLI surface so `--scheme=tcp` validates at parse time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SchemeOverride {
    #[default]
    Auto,
    Http,
    Https,
    Tcp,
    /// Bare `localhost:N` — explicitly says "this isn't HTTP."
    /// Equivalent to `Tcp` for `forward_url` but distinct in CLI
    /// help so operators see the intent option.
    None,
}

impl SchemeOverride {
    fn web_scheme(self, container_port: u16) -> Option<&'static str> {
        match self {
            Self::Auto => default_scheme(container_port),
            Self::Http => Some("http"),
            Self::Https => Some("https"),
            Self::Tcp | Self::None => None,
        }
    }
}

/// Default scheme heuristic mapping a container port to a URL
/// scheme. Conservative: only well-known HTTP ports auto-resolve;
/// everything else returns `None` and the operator gets `localhost:N`
/// (still copyable, but no auto-open).
#[must_use]
pub fn default_scheme(container_port: u16) -> Option<&'static str> {
    match container_port {
        80 | 3000 | 5000 | 5050 | 8000 | 8080 | 8081 | 8443 | 9000 => Some("http"),
        443 => Some("https"),
        _ => None,
    }
}

/// Render the URL the operator wants. `scheme` overrides the
/// heuristic; defaults to `Auto` (port-based).
#[must_use]
pub fn forward_url(local_port: u16, container_port: u16, scheme: SchemeOverride) -> String {
    match scheme.web_scheme(container_port) {
        Some(s) => format!("{s}://localhost:{local_port}"),
        None => format!("localhost:{local_port}"),
    }
}

/// Spawn the operator's system browser at `url`. Best-effort: returns
/// `Err` when no opener is available or the platform handler refuses
/// to launch (rare; logs to the caller-facing error so they can paste
/// the URL by hand). Cross-platform: `open` on macOS, `xdg-open` on
/// Linux, `cmd /C start` on Windows.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).status();
    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).status();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", "", url]).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let result: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no browser-open shim for this platform",
    ));
    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "browser-open exited {status}"
        )))
    }
}

#[derive(Debug, Error)]
pub enum PfError {
    #[error("no service named {0:?} in this config")]
    ServiceNotFound(String),
    #[error(
        "service {service:?} has no `publish:` block — port-forward only works for services that publish a host port. Use `yoink shell {service}` to drop into a docker-exec session inside the running container instead."
    )]
    NoPublishes { service: String },
    #[error(
        "service {service:?} has no published container port {port} (available: {available}). \
         Container ports come from the right-hand side of `publish: \"H:C\"` entries."
    )]
    PortNotPublished {
        service: String,
        port: u16,
        available: String,
    },
    #[error(
        "service {service:?} publishes {port} on more than one host port (got {choices}); \
         pin one with `LOCAL:CONTAINER` syntax (e.g. `yoink pf {service} {first}:{port}`)."
    )]
    AmbiguousHostPort {
        service: String,
        port: u16,
        choices: String,
        first: u16,
    },
    #[error("invalid publish spec {spec:?}: {reason}")]
    InvalidPublishSpec { spec: String, reason: String },
}

/// Resolved publish entry for a single (service, container_port) pair.
/// `host_ip` defaults to `127.0.0.1` when the publish doesn't specify
/// one — that's the docker-cli default and the address `docker-proxy`
/// binds to under `0.0.0.0` publishes too on most kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEndpoint {
    pub host_ip: String,
    pub host_port: u16,
    pub container_port: u16,
}

/// Look up the host-side endpoint for `(service, container_port)`.
/// Pure function; the caller wires the SSH connection separately.
pub fn resolve_endpoint(
    service: &ServiceConfig,
    container_port: u16,
) -> Result<PublishedEndpoint, PfError> {
    if service.run.publish.is_empty() {
        return Err(PfError::NoPublishes {
            service: service.name.clone(),
        });
    }
    let mut matches = Vec::new();
    let mut available = Vec::new();
    for spec in &service.run.publish {
        let parsed = parse_publish_spec(spec).map_err(|reason| PfError::InvalidPublishSpec {
            spec: spec.clone(),
            reason,
        })?;
        available.push(parsed.container_port);
        if parsed.container_port == container_port {
            matches.push(parsed);
        }
    }
    if matches.is_empty() {
        let mut sorted = available;
        sorted.sort_unstable();
        sorted.dedup();
        let available = sorted
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PfError::PortNotPublished {
            service: service.name.clone(),
            port: container_port,
            available,
        });
    }
    if matches.len() > 1 {
        let mut hosts: Vec<u16> = matches.iter().map(|p| p.host_port).collect();
        hosts.sort_unstable();
        let first = hosts[0];
        let choices = hosts
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PfError::AmbiguousHostPort {
            service: service.name.clone(),
            port: container_port,
            choices,
            first,
        });
    }
    Ok(matches.into_iter().next().expect("checked length above"))
}

/// If a service has exactly one publish entry, surface it for the
/// "no port arg given" CLI shortcut. Returns `None` for zero or
/// multiple publishes — caller surfaces `NoPublishes` or asks the
/// operator which port they meant.
#[must_use]
pub fn sole_publish(service: &ServiceConfig) -> Option<PublishedEndpoint> {
    if service.run.publish.len() != 1 {
        return None;
    }
    parse_publish_spec(&service.run.publish[0]).ok()
}

fn parse_publish_spec(spec: &str) -> Result<PublishedEndpoint, String> {
    // `[ip:]hostport:containerport[/proto]` — same shape `parse_port_bindings`
    // accepts in docker.rs. Different output (we want a typed
    // PublishedEndpoint, the deploy-time parser wants bollard's
    // PortBinding map), so duplicating the small bit of parsing is
    // cheaper than threading the bollard type into a public helper.
    let mapping = spec.split_once('/').map_or(spec, |(m, _proto)| m);
    let parts: Vec<&str> = mapping.split(':').collect();
    let (host_ip, host_port, container_port) = match parts.as_slice() {
        [hp, cp] => ("127.0.0.1", *hp, *cp),
        [ip, hp, cp] => (*ip, *hp, *cp),
        _ => return Err("expected `[ip:]host_port:container_port[/proto]`".into()),
    };
    let host_port: u16 = host_port
        .parse()
        .map_err(|_| format!("host port {host_port:?} not in 0..65535"))?;
    let container_port: u16 = container_port
        .parse()
        .map_err(|_| format!("container port {container_port:?} not in 0..65535"))?;
    Ok(PublishedEndpoint {
        host_ip: host_ip.to_string(),
        host_port,
        container_port,
    })
}

/// Resolve the right service + host pair for a `yoink pf <service>`
/// invocation. Returns the host the service actually runs on (single-
/// host configs are a no-op; multi-host needs the operator to disambiguate
/// via `--host` later if/when that flag lands).
pub fn resolve_service<'a>(
    config: &'a Config,
    name: &str,
) -> Result<&'a ServiceConfig, PfError> {
    config
        .services
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| PfError::ServiceNotFound(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Config {
        crate::config::Config::parse_str(yaml).unwrap()
    }

    fn cfg_with(svc_yaml: &str) -> Config {
        let yaml = format!(
            r#"
hosts:
  - {{ address: host-a, user: deploy }}
services:
{svc_yaml}
"#
        );
        parse(&yaml)
    }

    #[test]
    fn resolve_endpoint_picks_matching_publish() {
        let cfg = cfg_with(
            r#"  - name: pgadmin
    image: dpage/pgadmin4
    tag: "9"
    run:
      port: 80
      healthcheck_path: /login
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "127.0.0.1:5050:80"
"#,
        );
        let ep = resolve_endpoint(&cfg.services[0], 80).unwrap();
        assert_eq!(ep.host_ip, "127.0.0.1");
        assert_eq!(ep.host_port, 5050);
        assert_eq!(ep.container_port, 80);
    }

    #[test]
    fn resolve_endpoint_defaults_host_ip_to_loopback() {
        // Two-token form `H:C` has no IP — docker-proxy actually binds
        // to 0.0.0.0 in this case, but on every kernel we test against
        // 127.0.0.1 also reaches it, so we present the loopback view.
        let cfg = cfg_with(
            r#"  - name: api
    image: bt-api
    tag: latest
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "8080:8080"
"#,
        );
        let ep = resolve_endpoint(&cfg.services[0], 8080).unwrap();
        assert_eq!(ep.host_ip, "127.0.0.1");
        assert_eq!(ep.host_port, 8080);
    }

    #[test]
    fn resolve_endpoint_no_publishes_errors_with_shell_hint() {
        let cfg = cfg_with(
            r#"  - name: api
    image: bt-api
    tag: latest
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
"#,
        );
        let err = resolve_endpoint(&cfg.services[0], 8080).unwrap_err();
        assert!(matches!(err, PfError::NoPublishes { .. }));
        let msg = err.to_string();
        assert!(msg.contains("yoink shell"), "got: {msg}");
    }

    #[test]
    fn resolve_endpoint_unknown_port_lists_alternatives() {
        let cfg = cfg_with(
            r#"  - name: web
    image: bt-web
    tag: latest
    run:
      port: 3000
      healthcheck_path: /healthz
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "3000:3000"
        - "8080:8080"
"#,
        );
        let err = resolve_endpoint(&cfg.services[0], 9999).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3000"), "missing 3000 in: {msg}");
        assert!(msg.contains("8080"), "missing 8080 in: {msg}");
    }

    #[test]
    fn sole_publish_one_entry() {
        let cfg = cfg_with(
            r#"  - name: pgadmin
    image: dpage/pgadmin4
    tag: "9"
    run:
      port: 80
      healthcheck_path: /login
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "127.0.0.1:5050:80"
"#,
        );
        let ep = sole_publish(&cfg.services[0]).unwrap();
        assert_eq!(ep.container_port, 80);
        assert_eq!(ep.host_port, 5050);
    }

    #[test]
    fn sole_publish_returns_none_for_multi() {
        let cfg = cfg_with(
            r#"  - name: web
    image: bt-web
    tag: latest
    run:
      port: 3000
      healthcheck_path: /healthz
      healthcheck_timeout: 60s
      drain_timeout: 10s
      publish:
        - "3000:3000"
        - "8080:8080"
"#,
        );
        assert!(sole_publish(&cfg.services[0]).is_none());
    }

    #[test]
    fn forward_url_uses_heuristic_when_unforced() {
        assert_eq!(
            forward_url(54321, 80, SchemeOverride::Auto),
            "http://localhost:54321"
        );
        assert_eq!(
            forward_url(54322, 443, SchemeOverride::Auto),
            "https://localhost:54322"
        );
        assert_eq!(
            forward_url(54323, 5432, SchemeOverride::Auto),
            "localhost:54323"
        );
    }

    #[test]
    fn forward_url_explicit_scheme_overrides() {
        // Force https on a port the heuristic doesn't recognize.
        assert_eq!(
            forward_url(54324, 8443, SchemeOverride::Https),
            "https://localhost:54324"
        );
        // Tcp / None suppress the http:// wrapper even on port 80.
        assert_eq!(
            forward_url(54325, 80, SchemeOverride::None),
            "localhost:54325"
        );
        assert_eq!(
            forward_url(54326, 80, SchemeOverride::Tcp),
            "localhost:54326"
        );
    }
}
