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

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Config, SecretsConfig, ServiceConfig, TlsMode};
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
    findings.extend(check_sealed_file_exists(config));
    findings.extend(check_provider_command(config));
    findings.extend(check_keys_dir_perms());
    findings.extend(check_build_blocks(config));
    findings.extend(check_dockerfile_paths(config));
    findings.extend(check_proxied_services(config));
    findings.extend(check_tls_cert_secrets(config).await);
    findings.extend(check_secret_references(config).await);
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
        return vec![Finding::pass(
            "config",
            "proxy/domain checks skipped (no services have `domain:` set)",
        )];
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
        return vec![Finding::pass(
            "build",
            "arch-alignment check skipped (no services have `build:` blocks)",
        )];
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
    let any_domain = config.services.iter().any(|s| s.domain.is_some());
    if !any_domain {
        return vec![Finding::pass(
            "dns",
            "DNS check skipped (no services have `domain:` set)",
        )];
    }
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

// ---------- sealed.age existence ----------

fn check_sealed_file_exists(config: &Config) -> Vec<Finding> {
    let Some(SecretsConfig::Age { file, recipients }) = &config.secrets else {
        return vec![Finding::pass(
            "secrets",
            "sealed file check skipped (not using `provider: age`)",
        )];
    };
    if recipients.is_empty() {
        return vec![Finding::pass(
            "secrets",
            "sealed file check skipped (no `secrets.recipients:`)",
        )];
    }
    let any_refs = config
        .services
        .iter()
        .any(|s| !s.secrets.is_empty() || !s.env_from_secrets.is_empty());
    if !any_refs {
        return vec![Finding::pass(
            "secrets",
            "sealed file check skipped (no services reference secrets)",
        )];
    }
    let path = match sealed::resolve_sealed_path(config, file.as_deref()) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    if path.exists() {
        return vec![Finding::pass(
            "secrets",
            format!("sealed file present at {}", path.display()),
        )];
    }
    vec![
        Finding::error(
            "secrets",
            format!("{} doesn't exist but services reference secrets", path.display()),
        )
        .with_fix("run `yoink secrets edit` to create the bundle, then commit the file"),
    ]
}

// ---------- provider: command binary on PATH ----------

fn check_provider_command(config: &Config) -> Vec<Finding> {
    let Some(SecretsConfig::Command { command, .. }) = &config.secrets else {
        return vec![Finding::pass(
            "secrets",
            "provider command check skipped (not using `provider: command`)",
        )];
    };
    let Some(prog) = command.first() else {
        return vec![
            Finding::error("secrets", "`secrets.command:` is empty")
                .with_fix("set the binary as the first list element"),
        ];
    };
    if which_on_path(prog).is_some() {
        vec![Finding::pass(
            "secrets",
            format!("provider command `{prog}` is on PATH"),
        )]
    } else {
        vec![
            Finding::error(
                "secrets",
                format!("provider command `{prog}` not found on PATH"),
            )
            .with_fix(format!(
                "install `{prog}` (or fix PATH so yoink can spawn it at deploy time)"
            )),
        ]
    }
}

/// Lightweight `which` — walk `PATH` and return the first match. We
/// don't pull in the `which` crate for one call site.
fn which_on_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---------- keys-dir file modes ----------

#[cfg(unix)]
fn check_keys_dir_perms() -> Vec<Finding> {
    use std::os::unix::fs::MetadataExt;
    let Ok(dir) = sealed::keys_dir() else {
        return Vec::new();
    };
    if !dir.exists() {
        return vec![Finding::pass(
            "secrets",
            format!(
                "keys-dir perms check skipped ({} doesn't exist)",
                dir.display()
            ),
        )];
    }
    let mut bad = Vec::new();
    let mut scanned = 0_usize;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("key") {
            continue;
        }
        scanned += 1;
        let Ok(meta) = entry.metadata() else { continue };
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            bad.push(
                Finding::warn(
                    "secrets",
                    format!(
                        "{} has permissive mode {:o} (group/world readable)",
                        path.display(),
                        mode
                    ),
                )
                .with_fix(format!("chmod 600 {}", path.display())),
            );
        }
    }
    if scanned == 0 {
        return vec![Finding::pass(
            "secrets",
            format!("keys-dir perms check skipped ({} is empty)", dir.display()),
        )];
    }
    if bad.is_empty() {
        return vec![Finding::pass(
            "secrets",
            format!("all {scanned} key file(s) in keys-dir are mode 0600"),
        )];
    }
    bad
}

#[cfg(not(unix))]
fn check_keys_dir_perms() -> Vec<Finding> {
    vec![Finding::pass(
        "secrets",
        "keys-dir perms check skipped (non-unix)",
    )]
}

// ---------- Dockerfile path resolves ----------

fn check_dockerfile_paths(config: &Config) -> Vec<Finding> {
    let base = config
        .config_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let mut out = Vec::new();
    let mut scanned = 0_usize;
    for svc in &config.services {
        let Some(build) = &svc.build else { continue };
        scanned += 1;
        let context = base.join(&build.context);
        let dockerfile = context.join(build.dockerfile.as_deref().unwrap_or("Dockerfile"));
        if !dockerfile.exists() {
            out.push(
                Finding::error(
                    "build",
                    format!(
                        "service `{}`: Dockerfile not found at {}",
                        svc.name,
                        dockerfile.display()
                    ),
                )
                .with_fix(format!(
                    "create {}, or set `build.dockerfile:` to an existing path \
                     relative to the build context (`{}`)",
                    dockerfile.display(),
                    context.display()
                )),
            );
        }
    }
    if scanned == 0 {
        return vec![Finding::pass(
            "build",
            "Dockerfile-path check skipped (no services have a `build:` block)",
        )];
    }
    if out.is_empty() {
        return vec![Finding::pass(
            "build",
            format!("all {scanned} service Dockerfile path(s) resolve"),
        )];
    }
    out
}

// ---------- TLS cert mode requires bundle entries ----------

async fn check_tls_cert_secrets(config: &Config) -> Vec<Finding> {
    let proxy_default_cert = config
        .proxy
        .as_ref()
        .and_then(|p| p.tls.as_ref())
        .map(|t| t.cert_secret.clone());
    let proxy_default_key = config
        .proxy
        .as_ref()
        .and_then(|p| p.tls.as_ref())
        .map(|t| t.key_secret.clone());

    let needs_bundle = config
        .services
        .iter()
        .any(|s| matches!(s.tls, TlsMode::Cert));
    if !needs_bundle {
        return vec![Finding::pass(
            "config",
            "TLS cert-mode check skipped (no services use `tls: cert`)",
        )];
    }
    let bundle = match crate::secrets::load_bundle(config).await {
        Ok(Some(b)) => b,
        // If we can't load the bundle, `check_secret_references` will
        // already have surfaced that — don't double-report.
        _ => return Vec::new(),
    };

    let mut out = Vec::new();
    for svc in &config.services {
        if !matches!(svc.tls, TlsMode::Cert) {
            continue;
        }
        let cert_key = svc
            .tls_cert_secret
            .clone()
            .or_else(|| proxy_default_cert.clone());
        let key_key = svc
            .tls_key_secret
            .clone()
            .or_else(|| proxy_default_key.clone());
        for (label, key) in [("tls_cert_secret", cert_key), ("tls_key_secret", key_key)] {
            match key {
                None => {
                    out.push(
                        Finding::error(
                            "config",
                            format!(
                                "service `{}` uses `tls: cert` but {label} is unset",
                                svc.name
                            ),
                        )
                        .with_fix(format!(
                            "set `{label}: <SECRET_NAME>` on the service \
                             (or `proxy.tls.{}` to inherit)",
                            label.trim_start_matches("tls_").trim_end_matches("_secret")
                        )),
                    );
                }
                Some(k) if bundle.get(&k).is_none() => {
                    out.push(
                        Finding::error(
                            "secrets",
                            format!(
                                "service `{}` references {label} `{k}` not in the bundle",
                                svc.name
                            ),
                        )
                        .with_fix(format!(
                            "add `{k}=<pem-bytes>` via `yoink secrets edit`"
                        )),
                    );
                }
                _ => {}
            }
        }
    }
    out
}

// ---------- env_from_secrets / secrets references match the bundle ----------

async fn check_secret_references(config: &Config) -> Vec<Finding> {
    let any_refs = config
        .services
        .iter()
        .any(|s| !s.secrets.is_empty() || !s.env_from_secrets.is_empty());
    if !any_refs {
        return Vec::new();
    }
    let bundle = match crate::secrets::load_bundle(config).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return vec![
                Finding::error(
                    "secrets",
                    "services reference secrets but no bundle is configured",
                )
                .with_fix("set `secrets:` in yoink.yaml (`yoink secrets key generate` to start)"),
            ];
        }
        Err(e) => {
            return vec![
                Finding::error("secrets", "couldn't load secrets bundle for cross-check")
                    .with_detail(e.to_string()),
            ];
        }
    };

    let mut out = Vec::new();
    for svc in &config.services {
        for key in &svc.secrets {
            if bundle.get(key).is_none() {
                out.push(
                    Finding::error(
                        "secrets",
                        format!(
                            "service `{}` references missing secret `{key}`",
                            svc.name
                        ),
                    )
                    .with_fix(format!(
                        "seal `{key}=<value>` (`yoink secrets edit`) or remove the reference"
                    )),
                );
            }
        }
        for (env_name, secret_key) in &svc.env_from_secrets {
            if bundle.get(secret_key).is_none() {
                out.push(
                    Finding::error(
                        "secrets",
                        format!(
                            "service `{}`: env_from_secrets `{env_name}: {secret_key}` — `{secret_key}` not in bundle",
                            svc.name
                        ),
                    )
                    .with_fix(format!(
                        "seal `{secret_key}=<value>` (typo? — bundle has {} keys)",
                        bundle.len()
                    )),
                );
            }
        }
    }
    if out.is_empty() {
        out.push(Finding::pass(
            "secrets",
            "every service's secret references resolve in the bundle",
        ));
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
    fn dockerfile_path_check_errors_when_missing() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             services:\n  - name: app\n    image: app\n    build: { context: nonexistent-dir }\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_dockerfile_paths(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f[0].title.contains("Dockerfile not found"));
    }

    #[test]
    fn provider_command_missing_binary_errors() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             secrets: { provider: command, command: [\"definitely-not-a-real-binary-xyzzy\"] }\n\
             services:\n  - name: a\n    image: a\n    run: {}\n",
        )
        .unwrap();
        let f = check_provider_command(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f[0].title.contains("not found on PATH"));
    }

    #[test]
    fn provider_command_present_passes() {
        // `sh` is on every reasonable PATH.
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             secrets: { provider: command, command: [sh, -c, \"echo X=1\"] }\n\
             services:\n  - name: a\n    image: a\n    run: {}\n",
        )
        .unwrap();
        let f = check_provider_command(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Pass);
    }

    #[test]
    fn which_on_path_finds_sh() {
        assert!(which_on_path("sh").is_some());
        assert!(which_on_path("definitely-not-on-path-xyzzy").is_none());
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
