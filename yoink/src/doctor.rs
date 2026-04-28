//! `yoink doctor` — diagnose common deploy-blockers before they fire
//! at deploy time.
//!
//! The deploy path's own errors are usually accurate but late: the
//! operator hits "image platform mismatch" / "no age identity found"
//! / "EAI\_AGAIN redis" / "Let's Encrypt validation failed" partway
//! through `yoink up` and has to unwind. Doctor runs the same checks
//! up front, classifies them by severity, and prints actionable fix
//! hints — the difference between "deploy ran for 90 seconds, then
//! failed" and "doctor ran for 5 seconds, told me to fix DNS first."
//!
//! Designed as a pure data-returning function so the TUI and CLI
//! share the engine. CLI formats `Vec<Finding>` as a list; the TUI
//! pane renders the same vector in a scrolling table.

use std::sync::Arc;

use crate::config::{Config, SecretsConfig, ServiceConfig};
use crate::docker_ops::{DockerOps, Host};
use crate::sealed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Check ran and the answer is good. Pass-level findings are
    /// surfaced because absence of evidence is not evidence — the
    /// operator should be able to see what was checked.
    Pass,
    /// Something's off but the deploy might still work. Examples:
    /// "your local docker is amd64, host is amd64, but you have no
    /// `build:` block — fine if you'll push from a registry."
    Warn,
    /// Will block a deploy as configured. Examples: "no age identity
    /// found for the configured recipients," "host unreachable,"
    /// "domain doesn't resolve."
    Error,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// Short categorical label — `host`, `config`, `secrets`, `dns`,
    /// `build`. Used by the TUI to colour-code groups.
    pub category: &'static str,
    /// One-line summary suitable for a list view.
    pub title: String,
    /// Optional follow-up — error text, observed values, etc.
    pub detail: Option<String>,
    /// Optional remediation hint — the actionable bit.
    pub fix: Option<String>,
}

impl Finding {
    fn pass(category: &'static str, title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Pass,
            category,
            title: title.into(),
            detail: None,
            fix: None,
        }
    }

    fn warn(category: &'static str, title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            category,
            title: title.into(),
            detail: None,
            fix: None,
        }
    }

    fn error(category: &'static str, title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            category,
            title: title.into(),
            detail: None,
            fix: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }

    #[must_use]
    pub fn with_fix(mut self, f: impl Into<String>) -> Self {
        self.fix = Some(f.into());
        self
    }
}

/// Run every check in parallel where possible and return findings in
/// the order they completed. Caller decides how to render. Always
/// completes — no individual check failure aborts the whole run; each
/// surfaces as its own `Error`-severity Finding.
pub async fn run_doctor(config: &Config, ops: Arc<dyn DockerOps>) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();

    findings.extend(check_secrets(config));
    findings.extend(check_build_blocks(config));
    findings.extend(check_proxied_services(config));
    findings.extend(check_hosts(config, ops.clone()).await);
    findings.extend(check_arch_alignment(config, ops.clone()).await);
    findings.extend(check_dns_for_domains(config).await);

    findings
}

// ---------- secrets ----------

fn check_secrets(config: &Config) -> Vec<Finding> {
    let Some(SecretsConfig::Age { recipients, .. }) = &config.secrets else {
        // No age block (or `provider: command`) — there's nothing
        // for doctor to verify; the `provider: command` path is
        // exercised at deploy time and not pre-flightable here.
        return vec![Finding::pass("secrets", "no age block to validate")];
    };

    if recipients.is_empty() {
        return vec![
            Finding::error("secrets", "`secrets.recipients:` is empty")
                .with_fix("add at least one age public key (run `yoink secrets key generate`)"),
        ];
    }

    match sealed::load_identity(recipients) {
        Ok(identity) => {
            let public = identity.to_public().to_string();
            vec![
                Finding::pass("secrets", "age identity available")
                    .with_detail(format!("loaded identity matches recipient {public}")),
            ]
        }
        Err(e) => vec![
            Finding::error("secrets", "no age identity found for this config's recipients")
                .with_detail(e.to_string())
                .with_fix(
                    "set YOINK_AGE_KEY (raw) or YOINK_AGE_KEY_FILE, \
                     or place a key at ~/.config/yoink/keys/<recipient>.key \
                     (which `yoink secrets key generate` does by default)",
                ),
        ],
    }
}

// ---------- config: build blocks vs image refs ----------

fn check_build_blocks(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    for svc in &config.services {
        // Bare image (no `/`, no registry prefix) without a build:
        // block means yoink can't pull it AND can't build it. The
        // service deploys only if the operator pre-loads the image
        // on the host themselves, which is unusual.
        if svc.image.contains('/') || svc.image.starts_with("docker.io/") {
            continue; // registry-prefixed; pull works
        }
        if svc.build.is_some() {
            continue; // local build covers it
        }
        // Some bare names are real Docker Hub library images
        // (`postgres`, `redis`, `caddy`, `node`). Only flag when we
        // can't prove that.
        if is_likely_hub_library(&svc.image) {
            continue;
        }
        out.push(
            Finding::warn(
                "build",
                format!(
                    "service `{}` has bare image `{}` and no `build:` block",
                    svc.name, svc.image
                ),
            )
            .with_fix(format!(
                "add a `build: {{ context: . }}` block, or change `image:` to a \
                 registry-prefixed reference (e.g. `ghcr.io/you/{0}:tag`)",
                svc.image
            )),
        );
    }
    if out.is_empty() {
        vec![Finding::pass(
            "build",
            "every service is either registry-pullable or has a build: block",
        )]
    } else {
        out
    }
}

fn is_likely_hub_library(image: &str) -> bool {
    matches!(
        image,
        "postgres"
            | "redis"
            | "caddy"
            | "node"
            | "alpine"
            | "ubuntu"
            | "debian"
            | "nginx"
            | "mariadb"
            | "mysql"
            | "mongo"
            | "memcached"
            | "rabbitmq"
            | "elasticsearch"
            | "minio"
            | "traefik"
            | "busybox"
    )
}

// ---------- config: proxy + domain shape ----------

fn check_proxied_services(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    let proxied: Vec<&ServiceConfig> = config
        .services
        .iter()
        .filter(|s| s.domain.is_some())
        .collect();
    if proxied.is_empty() {
        return out;
    }

    if config.proxy.as_ref().and_then(|p| p.email.as_deref()).is_none() {
        out.push(
            Finding::warn(
                "config",
                "`proxy.email:` is unset; Let's Encrypt registration uses a placeholder",
            )
            .with_fix(
                "set `proxy: { email: you@example.com }` so ACME expiry warnings reach you",
            ),
        );
    }

    for svc in &proxied {
        if svc.run.port.is_none() {
            // The proxy needs a port to forward to — yoink rejects
            // this at validate, so reaching doctor with this state
            // would be unusual. Surface it anyway.
            out.push(
                Finding::error(
                    "config",
                    format!(
                        "service `{}` has `domain:` but no `run.port`",
                        svc.name
                    ),
                )
                .with_fix("set `run.port: <container-port>` so the proxy knows where to forward"),
            );
        }
    }
    out
}

// ---------- hosts ----------

async fn check_hosts(config: &Config, ops: Arc<dyn DockerOps>) -> Vec<Finding> {
    let mut out = Vec::new();
    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        match ops.version(&host).await {
            Ok(v) => {
                out.push(
                    Finding::pass("host", format!("{}: docker reachable", host.address))
                        .with_detail(format!(
                            "docker {} (api {}) on {}/{}",
                            v.server_version.as_deref().unwrap_or("?"),
                            v.api_version.as_deref().unwrap_or("?"),
                            v.os.as_deref().unwrap_or("?"),
                            v.arch.as_deref().unwrap_or("?"),
                        )),
                );
            }
            Err(e) => {
                out.push(
                    Finding::error(
                        "host",
                        format!("{}: cannot reach docker", host.address),
                    )
                    .with_detail(e.to_string())
                    .with_fix(format!(
                        "verify `ssh {}@{}` works and `docker info` runs on the host",
                        host.user, host.address
                    )),
                );
            }
        }
    }
    out
}

// ---------- arch alignment (local vs hosts) ----------

async fn check_arch_alignment(config: &Config, ops: Arc<dyn DockerOps>) -> Vec<Finding> {
    let any_local_build = config.services.iter().any(|s| s.build.is_some());
    if !any_local_build {
        return Vec::new();
    }

    // We need both local docker (for the build) and each remote
    // host's arch (for the run target). If either fails, bail with
    // a single Finding rather than spamming.
    let local_host = Host {
        user: String::new(),
        address: Host::LOCAL_ADDRESS.to_string(),
    };
    let local = match ops.version(&local_host).await {
        Ok(v) => v,
        Err(e) => {
            return vec![
                Finding::warn(
                    "build",
                    "couldn't query local docker arch — skipping cross-build check",
                )
                .with_detail(e.to_string()),
            ];
        }
    };
    let local_arch = local.arch.as_deref().unwrap_or("");

    let mut out = Vec::new();
    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        if host.is_local() {
            continue;
        }
        // Skip silently — `check_hosts` already surfaced the failure.
        let Ok(remote) = ops.version(&host).await else {
            continue;
        };
        let remote_arch = remote.arch.as_deref().unwrap_or("");
        if local_arch.is_empty() || remote_arch.is_empty() {
            continue;
        }
        if arches_compatible(local_arch, remote_arch) {
            out.push(Finding::pass(
                "build",
                format!(
                    "arch alignment: laptop {local_arch} <-> {} {remote_arch} (compatible)",
                    host.address
                ),
            ));
        } else {
            out.push(
                Finding::warn(
                    "build",
                    format!(
                        "arch mismatch: laptop docker is {local_arch}, {} is {remote_arch}",
                        host.address
                    ),
                )
                .with_detail(
                    "`docker build` defaults to the laptop arch; \
                     pushing the resulting image to a different-arch host fails with \
                     a confusing 'network sandbox not found' error at start time.",
                )
                .with_fix(format!(
                    "add `extra_args: [\"--platform\", \"linux/{}\"]` \
                     to every service's `build:` block",
                    canonical_platform(remote_arch)
                )),
            );
        }
    }
    out
}

fn arches_compatible(a: &str, b: &str) -> bool {
    canonical_platform(a) == canonical_platform(b)
}

fn canonical_platform(arch: &str) -> &'static str {
    match arch {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        "armv7l" | "arm" => "arm/v7",
        "riscv64" => "riscv64",
        _ => "unknown",
    }
}

// ---------- DNS for `domain:` services ----------

async fn check_dns_for_domains(config: &Config) -> Vec<Finding> {
    use tokio::net::lookup_host;

    let mut out = Vec::new();
    for svc in &config.services {
        let Some(domain) = &svc.domain else { continue };
        for host in domain.as_list() {
            // `lookup_host` wants `host:port`; we don't care about the
            // port, just whether the name resolves to A/AAAA records.
            let probe = format!("{host}:443");
            match lookup_host(&probe).await {
                Ok(addrs) => {
                    let ips: Vec<String> =
                        addrs.map(|a| a.ip().to_string()).collect();
                    if ips.is_empty() {
                        out.push(
                            Finding::error(
                                "dns",
                                format!("`{host}` doesn't resolve"),
                            )
                            .with_fix(format!(
                                "add an A/AAAA record for `{host}` \
                                 pointing at the host's public IP — \
                                 Let's Encrypt validates by HTTP-01"
                            )),
                        );
                    } else {
                        out.push(
                            Finding::pass(
                                "dns",
                                format!("`{host}` resolves"),
                            )
                            .with_detail(format!("→ {}", ips.join(", "))),
                        );
                    }
                }
                Err(e) => {
                    out.push(
                        Finding::error(
                            "dns",
                            format!("`{host}` lookup failed"),
                        )
                        .with_detail(e.to_string())
                        .with_fix(format!(
                            "add an A/AAAA record for `{host}` and wait for propagation"
                        )),
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arches_compatible_pairs() {
        assert!(arches_compatible("x86_64", "amd64"));
        assert!(arches_compatible("aarch64", "arm64"));
        assert!(!arches_compatible("aarch64", "amd64"));
    }

    #[test]
    fn canonical_platform_normalises() {
        assert_eq!(canonical_platform("x86_64"), "amd64");
        assert_eq!(canonical_platform("aarch64"), "arm64");
        assert_eq!(canonical_platform("nonsense"), "unknown");
    }

    #[test]
    fn is_likely_hub_library_recognises_postgres() {
        assert!(is_likely_hub_library("postgres"));
        assert!(is_likely_hub_library("redis"));
        assert!(!is_likely_hub_library("ghcr.io/you/app"));
        assert!(!is_likely_hub_library("my-internal-app"));
    }

    #[test]
    fn check_build_blocks_passes_clean_config() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             services:\n  - name: api\n    image: ghcr.io/me/api\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_build_blocks(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Pass);
    }

    #[test]
    fn check_build_blocks_warns_bare_image_without_build() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             services:\n  - name: my-tool\n    image: my-tool\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_build_blocks(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Warn);
        assert!(f[0].title.contains("my-tool"));
    }

    #[test]
    fn check_build_blocks_skips_hub_library_bare_image() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             services:\n  - name: db\n    image: postgres\n    tag: \"16-alpine\"\n    run: {}\n"
        )
        .unwrap();
        let f = check_build_blocks(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Pass);
    }
}
