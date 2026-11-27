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

use crate::base::host::Host;
use crate::config::{Config, SecretsConfig, ServiceConfig, TlsMode};
use crate::docker_ops::DockerOps;
use crate::sealed;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
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

/// Pass / Warn / Error counts in a single pass.
#[must_use]
pub fn tally(findings: &[Finding]) -> (usize, usize, usize) {
    let mut t = (0, 0, 0);
    for f in findings {
        match f.severity {
            Severity::Pass => t.0 += 1,
            Severity::Warn => t.1 += 1,
            Severity::Error => t.2 += 1,
        }
    }
    t
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// Short categorical label — `host`, `config`, `secrets`, `dns`,
    /// `build`. Used by the TUI to colour-code groups.
    pub category: &'static str,
    /// One-line summary suitable for a list view.
    pub title: String,
    /// Optional follow-up — error text, observed values, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Optional remediation hint — the actionable bit.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    // Fan out the slow / shared I/O up front in one round-trip so
    // every downstream check reads from cached data:
    //   - `ops.version` per host (was: called twice per host, once
    //     in check_hosts + once in check_arch_alignment).
    //   - bundle load (was: called twice, in check_tls_cert_secrets
    //     and check_secret_references — two age decrypts / two
    //     `provider: command` spawns).
    //   - DNS lookups for `domain:` services (independent of the
    //     above; concurrent with the version probes).
    let local_host = Host::local();
    let host_version_futs = config.fleet().into_iter().map(|host| {
        let ops = ops.clone();
        async move { (host.clone(), ops.version(&host).await) }
    });
    let local_version_fut = ops.version(&local_host);
    let bundle_fut = crate::secrets::load_bundle(config);
    let dns_fut = check_dns_for_domains(config);

    let (host_versions, local_version, bundle_result, dns_findings) = tokio::join!(
        futures_util::future::join_all(host_version_futs),
        local_version_fut,
        bundle_fut,
        dns_fut,
    );
    let (bundle, bundle_error) = match bundle_result {
        Ok(bundle) => (bundle, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let bundle_ref = bundle.as_ref();

    // Every other check is sync (config walk + cached lookups).
    let mut findings: Vec<Finding> = Vec::new();
    findings.extend(check_secrets(config));
    findings.extend(check_sealed_file_exists(config));
    findings.extend(check_provider_command(config));
    findings.extend(check_keys_dir_perms());
    findings.extend(check_build_blocks(config));
    findings.extend(check_registry_consistency(config));
    findings.extend(check_dockerfile_paths(config));
    findings.extend(check_proxied_services(config));
    findings.extend(check_bundle_loaded(
        config,
        bundle.is_some(),
        bundle_error.as_deref(),
    ));
    findings.extend(check_tls_cert_secrets(config, bundle_ref));
    findings.extend(check_secret_references(config, bundle_ref));
    findings.extend(check_hosts_from_versions(&host_versions));
    findings.extend(check_arch_alignment_from_versions(
        config,
        local_version.as_ref().ok(),
        &host_versions,
    ));
    findings.extend(dns_findings);

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

    match sealed::resolve_identity(recipients) {
        Ok(resolved) => {
            let public = resolved.identity.to_public().to_string();
            let store = crate::base::keychain::system_store();
            // Search for this identity only. A search over every recipient
            // stops at the first key file it finds, which can be the file
            // of another recipient.
            let file_copy = sealed::file_identity_for(std::slice::from_ref(&public))
                .ok()
                .flatten()
                .map(|(_, path)| path);
            identity_findings(
                &public,
                &resolved.source,
                recipients.len(),
                recipients.contains(&public),
                file_copy.as_deref(),
                // Over SSH the import hint would point at a command that
                // cannot ask for the permission it needs.
                store.supported() && !crate::base::keychain::prompts_off(),
            )
        }
        // A refusal on the keychain prompt stops before the key files
        // are read, so "place a key file" would be the wrong fix.
        Err(e @ sealed::SealedError::Keychain(_)) => vec![
            Finding::error("secrets", "the keychain refused access to the age identity")
                .with_detail(e.to_string())
                .with_fix(
                    "run the command again and allow yoink in the keychain prompt, \
                     or set YOINK_AGE_KEY",
                ),
        ],
        Err(sealed::SealedError::KeychainUnavailable { reason }) => vec![
            Finding::error(
                "secrets",
                "the keychain could not be read, and no key file holds the identity",
            )
            // The reason only: the advice is the fix line below.
            .with_detail(reason)
            .with_fix(crate::base::keychain::advice()),
        ],
        Err(e) => vec![
            Finding::error(
                "secrets",
                "no age identity found for this config's recipients",
            )
            .with_detail(e.to_string())
            .with_fix(
                "set YOINK_AGE_KEY (raw) or YOINK_AGE_KEY_FILE, \
                     or place a key at ~/.config/yoink/keys/<recipient>.key \
                     (which `yoink secrets key generate` does by default)",
            ),
        ],
    }
}

/// Findings about where the identity lives. Pure, so tests cover each
/// case without a keychain.
///
/// `file_copy` is a key file that holds the same identity.
/// `keychain_supported` is true on macOS.
fn identity_findings(
    public: &str,
    source: &sealed::IdentitySource,
    recipient_count: usize,
    in_recipients: bool,
    file_copy: Option<&std::path::Path>,
    keychain_supported: bool,
) -> Vec<Finding> {
    use sealed::IdentitySource;
    let mut out = if in_recipients {
        vec![
            Finding::pass("secrets", "age identity available").with_detail(format!(
                "loaded identity matches recipient {public} (source: {source})"
            )),
        ]
    } else {
        // The legacy file loads without a recipient match. A key that
        // is not a recipient cannot decrypt secrets.age.
        vec![
            Finding::warn(
                "secrets",
                "the age identity is not a recipient of this config",
            )
            .with_detail(format!(
                "{public} (source: {source}) is not in `secrets.recipients:`, \
                     so it cannot decrypt the sealed file"
            ))
            .with_fix(
                "use the key of one of the recipients, or add this recipient and \
                     seal the file again",
            ),
        ]
    };
    if *source == IdentitySource::Keychain {
        if let Some(path) = file_copy {
            out.push(
                Finding::warn(
                    "secrets",
                    "the age identity is in the keychain and in a key file",
                )
                .with_detail(format!(
                    "{} holds the same identity in plaintext",
                    path.display()
                ))
                .with_fix(format!(
                    "delete {} — yoink reads the keychain copy",
                    path.display()
                )),
            );
        } else if recipient_count == 1 {
            out.push(
                Finding::warn("secrets", "the only recipient is a keychain-only identity")
                    .with_detail(
                        "a reset keychain or a new Mac without a migration loses the key, \
                     and then secrets.age cannot be decrypted",
                    )
                    .with_fix(
                        "add a backup recipient to `secrets.recipients:`. A copy made with \
                         `yoink secrets key export --keychain` is also a backup, but yoink \
                         cannot see it, so this warning stays",
                    ),
            );
        }
    } else if keychain_supported
        // The legacy file loads without a recipient match. A key that is
        // not in `secrets.recipients:` would be stored under a name
        // yoink never asks for, so the import is not offered.
        && in_recipients
        && matches!(
            source,
            IdentitySource::KeysDir(_) | IdentitySource::Legacy(_)
        )
    {
        // Not for `YOINK_AGE_KEY_FILE`: the env var outranks the
        // keychain, so an import would change nothing.
        // An info line, not a warning: the exit code does not change.
        out.push(
            Finding::pass("secrets", "the age identity is a plaintext key file").with_fix(
                "optional: `yoink secrets key import --keychain` moves it into the macOS keychain",
            ),
        );
    }
    out
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

// ---------- config: registry block consistency ----------

/// Flag each orphan registry entry (`registry:` or `registries:`) — set
/// in config but no service actually references it. `yoink up` never
/// sends that entry's credentials for a service image, so it is dead
/// weight; doctor surfaces it so the operator can either remove it or
/// wire a service to it. One finding per configured registry.
fn check_registry_consistency(config: &Config) -> Vec<Finding> {
    config
        .configured_registries()
        .map(|reg| {
            let n = config.services_using_registry(reg).count();
            if n > 0 {
                return Finding::pass(
                    "config",
                    format!("registry `{}` is referenced by {n} service(s)", reg.server),
                );
            }
            Finding::warn(
                "config",
                format!(
                    "`registry: {}` is configured but no service references it",
                    reg.server
                ),
            )
            .with_detail(
                "`yoink up` won't use this registry — services without a `build:` \
                 block pull from whatever host is embedded in their `image:` ref, \
                 services with `build:` are shipped from the operator's local \
                 docker daemon. The configured creds are dead weight.",
            )
            .with_fix(format!(
                "drop this registry from the config, or update a service's `image:` to \
                 use it (e.g. `image: {}/your-app`)",
                reg.server
            ))
        })
        .collect()
}

// ---------- config: proxy + domain shape ----------

fn check_proxied_services(config: &Config) -> Vec<Finding> {
    let mut out = Vec::new();
    let proxied: Vec<&ServiceConfig> = config.proxied_services();
    if proxied.is_empty() {
        return vec![Finding::pass(
            "config",
            "proxy/domain checks skipped (no services have `domain:` set)",
        )];
    }

    let email = config.proxy_email();
    if email.is_none() && config.proxy_uses_acme() {
        // `yoink up` fails at Caddy render in this case, so this is
        // an error, not a warning.
        out.push(
            Finding::error(
                "config",
                "a service uses `tls: auto` (Let's Encrypt) but `proxy.email:` is unset; \
                 `yoink up` fails",
            )
            .with_fix(
                "set `proxy: { email: <your real email> }` (or set `proxy.tls` for an inline cert)",
            ),
        );
    } else if config.proxy_uses_acme() && email.is_some_and(is_placeholder_email) {
        out.push(
            Finding::warn(
                "config",
                "`proxy.email:` is still the placeholder; ACME expiry warnings won't reach a real address",
            )
            .with_fix("replace `proxy.email:` with an address you actually read"),
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
                    format!("service `{}` has `domain:` but no `run.port`", svc.name),
                )
                .with_fix("set `run.port: <container-port>` so the proxy knows where to forward"),
            );
        }
    }
    out
}

/// Catch the placeholders the docs and recipe templates ship with —
/// users who copy-paste forget to swap them for a real address, which
/// silently breaks ACME expiry warnings.
fn is_placeholder_email(email: &str) -> bool {
    let lower = email.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "you@example.com"
            | "user@example.com"
            | "admin@example.com"
            | "me@example.com"
            | "your-email@example.com"
    ) || lower.ends_with("@example.com")
        || lower.ends_with("@example.org")
}

// ---------- hosts ----------

fn check_hosts_from_versions(
    versions: &[(
        Host,
        Result<crate::docker_ops::DockerVersion, crate::docker_ops::DockerError>,
    )],
) -> Vec<Finding> {
    let mut out = Vec::new();
    for (host, result) in versions {
        match result {
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
                    Finding::error("host", format!("{}: cannot reach docker", host.address))
                        .with_detail(e.to_string())
                        .with_fix(format!(
                            "verify `ssh {}` works and `docker info` runs on the host",
                            host.ssh_args().join(" ")
                        )),
                );
            }
        }
    }
    out
}

// ---------- arch alignment (local vs hosts) ----------

fn check_arch_alignment_from_versions(
    config: &Config,
    local_version: Option<&crate::docker_ops::DockerVersion>,
    versions: &[(
        Host,
        Result<crate::docker_ops::DockerVersion, crate::docker_ops::DockerError>,
    )],
) -> Vec<Finding> {
    let any_local_build = config.any_service_builds_locally();
    if !any_local_build {
        return vec![Finding::pass(
            "build",
            "arch-alignment check skipped (no services have `build:` blocks)",
        )];
    }
    let Some(local) = local_version else {
        return vec![Finding::warn(
            "build",
            "couldn't query local docker arch — skipping cross-build check",
        )];
    };
    let local_arch = local.arch.as_deref().unwrap_or("");

    let mut out = Vec::new();
    for (host, result) in versions {
        if host.is_local() {
            continue;
        }
        // Skip silently — check_hosts already surfaced the failure.
        let Ok(remote) = result else { continue };
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

    let any_domain = config.services.iter().any(|s| s.domain.is_some());
    if !any_domain {
        return vec![Finding::pass(
            "dns",
            "DNS check skipped (no services have `domain:` set)",
        )];
    }
    // Flatten every (service, host) into one list so they all
    // resolve concurrently via join_all instead of awaiting each
    // lookup serially.
    let domains: Vec<String> = config
        .services
        .iter()
        .filter_map(|s| s.domain.as_ref())
        .flat_map(crate::config::DomainSpec::as_list)
        .collect();
    let lookups = domains.into_iter().map(|host| async move {
        // `lookup_host` wants `host:port`; we don't care about the
        // port, just whether the name resolves to A/AAAA records.
        let probe = format!("{host}:443");
        let result = lookup_host(probe).await;
        (host, result)
    });
    futures_util::future::join_all(lookups)
        .await
        .into_iter()
        .map(|(host, result)| match result {
            Ok(addrs) => {
                let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
                if ips.is_empty() {
                    Finding::error("dns", format!("`{host}` doesn't resolve")).with_fix(format!(
                        "add an A/AAAA record for `{host}` \
                                 pointing at the host's public IP — \
                                 Let's Encrypt validates by HTTP-01"
                    ))
                } else {
                    Finding::pass("dns", format!("`{host}` resolves"))
                        .with_detail(format!("→ {}", ips.join(", ")))
                }
            }
            Err(e) => Finding::error("dns", format!("`{host}` lookup failed"))
                .with_detail(e.to_string())
                .with_fix(format!(
                    "add an A/AAAA record for `{host}` and wait for propagation"
                )),
        })
        .collect()
}

// ---------- sealed.age existence ----------

fn check_sealed_file_exists(config: &Config) -> Vec<Finding> {
    if !matches!(config.secrets, Some(SecretsConfig::Age { .. })) {
        return vec![Finding::pass(
            "secrets",
            "sealed file check skipped (not using `provider: age`)",
        )];
    }
    // Every secrets load resolves this path first, so an escaping
    // `secrets.file:` breaks `yoink up` whatever the services reference.
    let path = match config.sealed_path() {
        Ok(path) => path,
        Err(e) => {
            return vec![Finding::error("secrets", e.to_string()).with_fix(
                "set `secrets.file:` to a relative path inside the directory of yoink.yaml",
            )];
        }
    };
    // `load_bundle` reads the file for every `provider: age` config,
    // before it looks at the recipients or at what the services
    // reference. A missing file fails `yoink up`, also in a new
    // project that has no secrets yet.
    if path.exists() {
        return vec![Finding::pass(
            "secrets",
            format!("sealed file present at {}", path.display()),
        )];
    }
    vec![
        Finding::error(
            "secrets",
            format!(
                "{} does not exist, and `yoink up` reads it for every `provider: age` config",
                path.display()
            ),
        )
        .with_fix(
            "run `yoink secrets edit` to create the bundle, then commit the file \
             (or remove the `secrets:` block if the project has no secrets)",
        ),
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

fn check_tls_cert_secrets(
    config: &Config,
    bundle: Option<&crate::base::bundle::SecretsBundle>,
) -> Vec<Finding> {
    let proxy_default_cert = config.proxy_tls().map(|t| t.cert_secret.clone());
    let proxy_default_key = config.proxy_tls().map(|t| t.key_secret.clone());
    let global_tls = global_proxy_tls(config);

    if !tls_needs_bundle(config) {
        return vec![Finding::pass(
            "config",
            "TLS cert-mode check skipped (no services use `tls: cert`)",
        )];
    }
    // `check_bundle_loaded` reports a missing bundle.
    let Some(bundle) = bundle else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if let Some(tls) = global_tls {
        let mut refs = vec![
            ("cert_secret", tls.cert_secret.as_str()),
            ("key_secret", tls.key_secret.as_str()),
        ];
        if let Some(ca) = &tls.client_auth {
            refs.push((
                "client_auth.trust_pool_secret",
                ca.trust_pool_secret.as_str(),
            ));
        }
        for (field, key) in refs {
            if bundle.get(key).is_none() {
                out.push(
                    Finding::error(
                        "secrets",
                        format!(
                            "`proxy.tls.{field}` references `{key}`, which is not in the bundle"
                        ),
                    )
                    .with_fix(format!("add `{key}=<pem-bytes>` via `yoink secrets edit`")),
                );
            }
        }
    }
    for svc in &config.services {
        // The Caddy config reads certs only for routed services.
        if !svc.is_proxied() || !matches!(svc.tls, TlsMode::Cert) {
            continue;
        }
        // A service that inherits the `proxy.tls` cert is covered by
        // the check above.
        if global_tls.is_some() && svc.own_tls_cert().is_none() {
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
                            label.trim_start_matches("tls_")
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
                        .with_fix(format!("add `{k}=<pem-bytes>` via `yoink secrets edit`")),
                    );
                }
                _ => {}
            }
        }
    }
    out
}

/// The `proxy.tls` block when the proxy runs. The Caddy renderer
/// resolves its secrets then, whatever `tls:` mode the services use.
fn global_proxy_tls(config: &Config) -> Option<&crate::config::ProxyTls> {
    config.proxy_tls().filter(|_| config.proxy_enabled())
}

/// True when the TLS certs need the secrets bundle: the proxy runs
/// with `proxy.tls`, or with a routed service that uses `tls: cert`.
/// With the proxy off, or for a service without `domain:`, yoink
/// renders no Caddy config and reads no cert.
fn tls_needs_bundle(config: &Config) -> bool {
    config.proxy_enabled()
        && (config.proxy_tls().is_some()
            || config
                .services
                .iter()
                .any(|s| s.is_proxied() && matches!(s.tls, TlsMode::Cert)))
}

fn services_reference_secrets(config: &Config) -> bool {
    config
        .services
        .iter()
        .any(|s| !s.secrets.is_empty() || !s.env_from_secrets.is_empty())
}

// ---------- the bundle loads when a check needs it ----------

/// One error when the bundle is needed and there is none. `loaded` is
/// true when the bundle loaded. `load_error` is the error of the load.
/// With a `secrets:` block, `yoink up` loads the bundle first, so a
/// failed load is an error whatever references the secrets. Without
/// the block the load gives no bundle and no error, so it is an error
/// only when the services or the TLS certs need the bundle.
fn check_bundle_loaded(config: &Config, loaded: bool, load_error: Option<&str>) -> Vec<Finding> {
    if loaded {
        return Vec::new();
    }
    let mut needs = Vec::new();
    if services_reference_secrets(config) {
        needs.push("services that reference secrets");
    }
    if tls_needs_bundle(config) {
        needs.push("the TLS certs (`proxy.tls` or a `tls: cert` service)");
    }
    let what = needs.join(" and ");
    let finding = if config.secrets.is_none() {
        if needs.is_empty() {
            return Vec::new();
        }
        Finding::error(
            "secrets",
            format!("{what} need the secrets bundle, but yoink.yaml has no `secrets:` block"),
        )
        .with_fix("set `secrets:` in yoink.yaml (`yoink secrets key generate` to start)")
    } else {
        let title = if needs.is_empty() {
            "`yoink up` loads the secrets bundle first, but the bundle did not load".to_string()
        } else {
            format!("{what} need the secrets bundle, but the bundle did not load")
        };
        Finding::error("secrets", title)
            .with_detail(load_error.unwrap_or("unknown error").to_string())
            .with_fix("fix the load error in the detail, then run `yoink doctor` again")
    };
    vec![finding]
}

// ---------- env_from_secrets / secrets references match the bundle ----------

fn check_secret_references(
    config: &Config,
    bundle: Option<&crate::base::bundle::SecretsBundle>,
) -> Vec<Finding> {
    if !services_reference_secrets(config) {
        return vec![Finding::pass(
            "secrets",
            "secret-reference check skipped (no services reference secrets)",
        )];
    }
    // `check_bundle_loaded` reports a missing bundle.
    let Some(bundle) = bundle else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for svc in &config.services {
        for key in &svc.secrets {
            if bundle.get(key).is_none() {
                out.push(
                    Finding::error(
                        "secrets",
                        format!("service `{}` references missing secret `{key}`", svc.name),
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

    /// Regression: doctor said nothing when `secrets.file:` points
    /// outside the config directory. Load accepts it, and every
    /// secrets command fails later with `PathEscape`.
    #[test]
    fn secrets_file_outside_config_dir_is_an_error() {
        let config = Config::parse_str(
            r#"
hosts: [{ address: h1, user: deploy }]
secrets:
  provider: age
  file: ../elsewhere/sealed.age
  recipients: [age1lvc8nwnhjruuaagj99nj3zp0djh7m7tvqs055yfl8mjcahxuvu4sn954gy]
services:
  - name: api
    image: img
    tag: t
    run: { port: 8080 }
"#,
        )
        .expect("load accepts the path today");
        let findings = check_sealed_file_exists(&config);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].title.contains("escapes"),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: with `provider: age`, a missing sealed file and no
    /// service that references secrets, doctor said "skipped".
    /// `load_bundle` reads the file for every age config, so `yoink up`
    /// failed. Doctor now reports the missing file as an error.
    #[tokio::test]
    async fn missing_sealed_file_is_an_error_for_every_age_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yoink.yaml");
        std::fs::write(
            &path,
            r#"
hosts: [{ address: h1, user: deploy }]
secrets:
  provider: age
  recipients: [age1lvc8nwnhjruuaagj99nj3zp0djh7m7tvqs055yfl8mjcahxuvu4sn954gy]
services:
  - name: api
    image: img
    tag: t
    run: { port: 8080 }
"#,
        )
        .unwrap();
        let config = Config::load_from_path(&path).unwrap();
        let findings = check_sealed_file_exists(&config);
        assert_eq!(findings.len(), 1, "{:?}", titles(&findings));
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(
            findings[0].title.contains("does not exist"),
            "{:?}",
            titles(&findings)
        );
        // The same config fails the load that `yoink up` runs first.
        // The read fails before any identity is loaded.
        assert!(matches!(
            crate::secrets::load_bundle(&config).await,
            Err(crate::secrets::SecretsError::SealedRead { .. })
        ));

        std::fs::write(config.sealed_path().unwrap(), b"").unwrap();
        let findings = check_sealed_file_exists(&config);
        assert_eq!(findings[0].severity, Severity::Pass);
    }

    /// Regression: the hint named `proxy.tls.cert` / `proxy.tls.key`.
    /// The fields are `cert_secret` / `key_secret`, and the wrong names
    /// break config parsing (`deny_unknown_fields`).
    #[test]
    fn tls_cert_hint_names_the_real_proxy_tls_fields() {
        // Load rejects a routed `tls: cert` service with no cert, so
        // the test drops the cert names after the parse.
        let mut config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    tls: cert
    tls_cert_secret: API_CERT
    tls_key_secret: API_KEY
    run: { port: 8080 }
"#,
        )
        .unwrap();
        let api = config
            .services
            .iter_mut()
            .find(|s| s.name == "api")
            .unwrap();
        api.tls_cert_secret = None;
        api.tls_key_secret = None;
        let bundle = crate::base::bundle::SecretsBundle::new(std::collections::BTreeMap::new());
        let fixes: Vec<String> = check_tls_cert_secrets(&config, Some(&bundle))
            .into_iter()
            .filter_map(|f| f.fix)
            .collect();
        assert_eq!(fixes.len(), 2, "{fixes:?}");
        assert!(fixes[0].contains("`proxy.tls.cert_secret`"), "{fixes:?}");
        assert!(fixes[1].contains("`proxy.tls.key_secret`"), "{fixes:?}");
    }

    /// Regression: with only `tls: auto` services, doctor skipped the
    /// `proxy.tls` secrets. The Caddy renderer still needs them, so
    /// `yoink up` failed on a secret that doctor never checked.
    #[test]
    fn missing_proxy_tls_secret_is_an_error_with_tls_auto_services() {
        let config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
secrets: { provider: age, recipients: [age1lvc8nwnhjruuaagj99nj3zp0djh7m7tvqs055yfl8mjcahxuvu4sn954gy] }
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
"#,
        )
        .unwrap();
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "CF_CERT".to_string(),
            zeroize::Zeroizing::new("cert".to_string()),
        );
        let bundle = crate::base::bundle::SecretsBundle::new(values);
        let findings = check_tls_cert_secrets(&config, Some(&bundle));
        let errors: Vec<&str> = findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .map(|f| f.title.as_str())
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("proxy.tls.key_secret"), "{errors:?}");
        assert!(errors[0].contains("CF_KEY"), "{errors:?}");
    }

    /// Regression: doctor only warned about a missing `proxy.email:`
    /// while `yoink up` fails at Caddy render for a `tls: auto`
    /// service. Doctor must report an error.
    #[test]
    fn missing_proxy_email_with_tls_auto_is_an_error() {
        let config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        )
        .unwrap();
        let findings = check_proxied_services(&config);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.title.contains("proxy.email")),
            "{:?}",
            titles(&findings)
        );
        assert!(
            crate::proxy::caddy::render(&config, |_| vec![], None).is_err(),
            "up fails at render for the same config"
        );
    }

    /// Regression: with `proxy.enabled: false`, doctor reported the
    /// missing `proxy.email:` as an error. yoink renders no Caddy
    /// config then, and `yoink up` succeeds.
    #[test]
    fn missing_proxy_email_is_no_error_when_the_proxy_is_off() {
        let config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
proxy: { enabled: false }
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: { port: 8080 }
"#,
        )
        .unwrap();
        assert!(!config.proxy_uses_acme());
        let findings = check_proxied_services(&config);
        assert!(
            !findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.title.contains("proxy.email")),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: doctor warned about a placeholder `proxy.email:`
    /// without ACME (here an inline `proxy.tls` cert). The address
    /// then gets no Let's Encrypt mail, so the warning was noise.
    #[test]
    fn placeholder_proxy_email_is_not_reported_without_acme() {
        let yaml = |tls: &str| {
            format!(
                r#"
deploy: {{ networks: [n] }}
hosts: [{{ address: h1, user: deploy }}]
proxy:
  email: you@example.com
{tls}
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    run: {{ port: 8080 }}
"#
            )
        };
        let inline = Config::parse_str(&yaml(
            "  tls:\n    cert_secret: CF_CERT\n    key_secret: CF_KEY",
        ))
        .unwrap();
        assert!(!inline.proxy_uses_acme());
        let findings = check_proxied_services(&inline);
        assert!(
            !findings.iter().any(|f| f.title.contains("placeholder")),
            "{:?}",
            titles(&findings)
        );
        let acme = Config::parse_str(&yaml("")).unwrap();
        assert!(acme.proxy_uses_acme());
        let findings = check_proxied_services(&acme);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Warn && f.title.contains("placeholder")),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: without ACME (here an inline `proxy.tls` cert),
    /// doctor warned that "Let's Encrypt registration uses a
    /// placeholder". No Let's Encrypt registration happens then.
    #[test]
    fn missing_proxy_email_is_not_reported_without_acme() {
        let config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
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
"#,
        )
        .unwrap();
        let findings = check_proxied_services(&config);
        assert!(
            !findings.iter().any(|f| f.title.contains("proxy.email")),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: a `tls: cert` service without `domain:`, or with the
    /// proxy off, made doctor ask for the bundle. No Caddy config is
    /// rendered for it, so the config deploys without a bundle.
    #[test]
    fn unrouted_tls_cert_services_do_not_need_the_bundle() {
        let config = |proxy: &str, domain: &str| {
            Config::parse_str(&format!(
                r#"
deploy: {{ networks: [n] }}
hosts: [{{ address: h1, user: deploy }}]
{proxy}
services:
  - name: api
    image: img
    tag: t
    {domain}
    tls: cert
    tls_cert_secret: API_CERT
    tls_key_secret: API_KEY
    run: {{ port: 8080 }}
"#
            ))
            .unwrap()
        };
        let no_domain = config("", "");
        assert!(!tls_needs_bundle(&no_domain));
        assert!(check_bundle_loaded(&no_domain, false, None).is_empty());

        let proxy_off = config("proxy: { enabled: false }", "domain: api.example.com");
        assert!(!tls_needs_bundle(&proxy_off));
        assert!(check_bundle_loaded(&proxy_off, false, None).is_empty());

        let routed = config("", "domain: api.example.com");
        assert!(tls_needs_bundle(&routed));
        assert_eq!(check_bundle_loaded(&routed, false, None).len(), 1);

        // The proxy runs for `proxy.tls`, but the unrouted service
        // needs no cert of its own in the bundle.
        let global = config(
            "proxy: { enabled: true, tls: { cert_secret: CF_CERT, key_secret: CF_KEY } }",
            "",
        );
        let bundle = crate::base::bundle::SecretsBundle::new(
            [("CF_CERT", "c"), ("CF_KEY", "k")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), zeroize::Zeroizing::new(v.to_string())))
                .collect(),
        );
        let findings = check_tls_cert_secrets(&global, Some(&bundle));
        assert!(
            findings.iter().all(|f| f.severity != Severity::Error),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: with `proxy.tls` set and no bundle, doctor reported
    /// nothing, and no other check did when no service lists
    /// `secrets:`. `yoink up` then fails at Caddy render.
    #[test]
    fn tls_certs_without_a_bundle_are_an_error() {
        let config = Config::parse_str(
            r#"
deploy: { networks: [n] }
hosts: [{ address: h1, user: deploy }]
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
"#,
        )
        .unwrap();
        let findings = check_bundle_loaded(&config, false, None);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == Severity::Error && f.title.contains("bundle")),
            "{:?}",
            titles(&findings)
        );
    }

    /// Regression: with services that reference secrets and TLS certs
    /// that need the bundle, a missing bundle was reported twice. The
    /// fix said to set `secrets:` even when the block was set and only
    /// the load failed.
    #[test]
    fn a_missing_bundle_is_reported_once_with_a_fix_for_its_cause() {
        let yaml = |secrets: &str| {
            format!(
                r#"
deploy: {{ networks: [n] }}
hosts: [{{ address: h1, user: deploy }}]
{secrets}
proxy:
  tls:
    cert_secret: CF_CERT
    key_secret: CF_KEY
services:
  - name: api
    image: img
    tag: t
    domain: api.example.com
    secrets: [DB_URL]
    run: {{ port: 8080 }}
"#
            )
        };
        let missing = |config: &Config, error: Option<&str>| {
            let mut findings = check_bundle_loaded(config, false, error);
            findings.extend(check_tls_cert_secrets(config, None));
            findings.extend(check_secret_references(config, None));
            findings
                .into_iter()
                .filter(|f| f.severity == Severity::Error)
                .collect::<Vec<_>>()
        };

        let no_block = Config::parse_str(&yaml("")).unwrap();
        let findings = missing(&no_block, None);
        assert_eq!(findings.len(), 1, "{:?}", titles(&findings));
        assert!(
            findings[0]
                .title
                .contains("services that reference secrets and the TLS certs")
        );
        assert!(findings[0].title.contains("no `secrets:` block"));
        assert!(
            findings[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("set `secrets:`")
        );

        let block = Config::parse_str(&yaml(
            "secrets:\n  provider: age\n  recipients: [age1lvc8nwnhjruuaagj99nj3zp0djh7m7tvqs055yfl8mjcahxuvu4sn954gy]",
        ))
        .unwrap();
        let findings = missing(&block, Some("read sealed file: not found"));
        assert_eq!(findings.len(), 1, "{:?}", titles(&findings));
        assert!(findings[0].title.contains("did not load"));
        assert_eq!(
            findings[0].detail.as_deref(),
            Some("read sealed file: not found")
        );
        assert!(
            !findings[0]
                .fix
                .as_deref()
                .unwrap()
                .contains("set `secrets:`")
        );

        assert!(check_bundle_loaded(&block, true, None).is_empty());
    }

    /// Regression: with a `secrets:` block and secrets referenced only
    /// by a pre-deploy hook, doctor dropped the load error. `yoink up`
    /// loads the bundle first and failed where doctor passed.
    #[test]
    fn a_failed_load_is_an_error_when_no_service_needs_the_bundle() {
        let config = Config::parse_str(
            r#"
hosts: [{ address: h1, user: deploy }]
secrets:
  provider: age
  recipients: [age1lvc8nwnhjruuaagj99nj3zp0djh7m7tvqs055yfl8mjcahxuvu4sn954gy]
hooks:
  pre_deploy:
    - name: migrate
      cmd: [./migrate]
      secrets: [DB_URL]
services:
  - name: api
    image: img
    tag: t
    run: { port: 8080 }
"#,
        )
        .unwrap();
        assert!(!services_reference_secrets(&config));
        assert!(!tls_needs_bundle(&config));
        let findings = check_bundle_loaded(&config, false, Some("decrypt failed"));
        assert_eq!(findings.len(), 1, "{:?}", titles(&findings));
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].title.contains("did not load"));
        assert_eq!(findings[0].detail.as_deref(), Some("decrypt failed"));

        // Without a `secrets:` block the load gives no bundle and no
        // error, and `yoink up` does not fail on it.
        let no_block = Config::parse_str(
            r#"
hosts: [{ address: h1, user: deploy }]
services:
  - name: api
    image: img
    tag: t
    run: { port: 8080 }
"#,
        )
        .unwrap();
        assert!(check_bundle_loaded(&no_block, false, None).is_empty());
    }

    fn titles(findings: &[Finding]) -> Vec<(Severity, &str)> {
        findings
            .iter()
            .map(|f| (f.severity, f.title.as_str()))
            .collect()
    }

    #[test]
    fn identity_findings_report_the_source() {
        let f = identity_findings(
            "age1x",
            &sealed::IdentitySource::EnvKey,
            2,
            true,
            None,
            true,
        );
        assert_eq!(f.len(), 1);
        assert!(
            f[0].detail
                .as_deref()
                .unwrap()
                .contains("YOINK_AGE_KEY env var")
        );
    }

    #[test]
    fn identity_findings_warn_when_keychain_and_file_hold_the_same_key() {
        let path = std::path::Path::new("/tmp/k.key");
        let f = identity_findings(
            "age1x",
            &sealed::IdentitySource::Keychain,
            2,
            true,
            Some(path),
            true,
        );
        assert_eq!(
            titles(&f)[1],
            (
                Severity::Warn,
                "the age identity is in the keychain and in a key file"
            )
        );
    }

    #[test]
    fn identity_findings_warn_on_a_single_keychain_only_recipient() {
        let f = identity_findings(
            "age1x",
            &sealed::IdentitySource::Keychain,
            1,
            true,
            None,
            true,
        );
        assert_eq!(
            titles(&f)[1],
            (
                Severity::Warn,
                "the only recipient is a keychain-only identity"
            )
        );
        // A second recipient is the backup: no warning.
        let f = identity_findings(
            "age1x",
            &sealed::IdentitySource::Keychain,
            2,
            true,
            None,
            true,
        );
        assert_eq!(f.len(), 1);
    }

    /// The hint to move a key file into the keychain is an info line on
    /// macOS only, and never a warning.
    #[test]
    fn identity_findings_hint_at_the_keychain_for_a_key_file_on_macos() {
        let source = sealed::IdentitySource::KeysDir("/tmp/k.key".into());
        let f = identity_findings("age1x", &source, 1, true, None, true);
        assert_eq!(
            titles(&f)[1],
            (Severity::Pass, "the age identity is a plaintext key file")
        );
        assert!(f[1].fix.as_deref().unwrap().contains("import --keychain"));
        assert_eq!(
            identity_findings("age1x", &source, 1, true, None, false).len(),
            1
        );
        // It is a warning, not a pass: that key cannot decrypt the file.
        let f = identity_findings("age1x", &source, 1, false, None, true);
        assert_eq!(
            titles(&f),
            [(
                Severity::Warn,
                "the age identity is not a recipient of this config"
            )]
        );
        // A legacy key that is not a recipient of the project: no hint.
        assert_eq!(
            identity_findings("age1x", &source, 1, false, None, true).len(),
            1
        );
        // `YOINK_AGE_KEY_FILE` outranks the keychain: no hint.
        let env_file = sealed::IdentitySource::EnvFile("/tmp/k.key".into());
        assert_eq!(
            identity_findings("age1x", &env_file, 1, true, None, true).len(),
            1
        );
    }

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
             services:\n  - name: db\n    image: postgres\n    tag: \"16-alpine\"\n    run: {}\n",
        )
        .unwrap();
        let f = check_build_blocks(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Pass);
    }

    #[test]
    fn check_registry_consistency_passes_when_image_uses_configured_registry() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             registry:\n  server: ghcr.io\n  username_secret: U\n  password_secret: P\n\
             secrets: { provider: command, command: [sh, -c, \"echo X=1\"] }\n\
             services:\n  - name: api\n    image: ghcr.io/me/api\n    tag: v1\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_registry_consistency(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Pass);
        assert!(f[0].title.contains("ghcr.io"));
    }

    #[test]
    fn check_registry_consistency_warns_on_orphan_registry_block() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             registry:\n  server: 4db05qgnlk.registry.depot.dev\n  username_secret: U\n  password_secret: P\n\
             secrets: { provider: command, command: [sh, -c, \"echo X=1\"] }\n\
             services:\n  - name: db\n    image: postgres\n    tag: \"16\"\n    run: { port: 5432, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_registry_consistency(&cfg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Warn);
        assert!(f[0].title.contains("no service references it"));
    }

    #[test]
    fn check_registry_consistency_silent_without_registry_block() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             services:\n  - name: api\n    image: ghcr.io/me/api\n    tag: v1\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_registry_consistency(&cfg);
        assert!(f.is_empty(), "no registry block → no finding (good or bad)");
    }

    #[test]
    fn check_registry_consistency_reports_each_registries_entry() {
        let cfg = Config::parse_str(
            "deploy:\n  networks: [yoink]\nhosts:\n  - { address: h, user: u }\n\
             registries:\n\
             \x20 - { server: ghcr.io, username_secret: U, password_secret: P }\n\
             \x20 - { server: quay.io, username_secret: U2, password_secret: P2 }\n\
             secrets: { provider: command, command: [sh, -c, \"echo X=1\"] }\n\
             services:\n  - name: api\n    image: ghcr.io/me/api\n    tag: v1\n    run: { port: 8080, healthcheck_path: / }\n"
        )
        .unwrap();
        let f = check_registry_consistency(&cfg);
        assert_eq!(f.len(), 2, "one finding per registry");
        assert_eq!(f[0].severity, Severity::Pass);
        assert!(f[0].title.contains("ghcr.io"));
        assert!(f[0].title.contains("1 service(s)"));
        assert_eq!(f[1].severity, Severity::Warn);
        assert!(f[1].title.contains("quay.io"));
        assert!(f[1].title.contains("no service references it"));
    }
}
