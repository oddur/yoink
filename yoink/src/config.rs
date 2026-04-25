//! Multi-service yoink config. One TOML file describes the full set of
//! services to run on a fleet of hosts, plus optional pre-deploy hooks.
//! The unified model — every service goes through the same reconcile
//! loop; the differences are config knobs (healthcheck or not, ports
//! published or not, etc.) rather than service kinds.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("read config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config: {0}")]
    Parse(#[from] yaml_serde::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("invalid include glob {pattern:?}: {source}")]
    GlobPattern {
        pattern: String,
        #[source]
        source: glob::PatternError,
    },
    #[error("error walking include glob {pattern:?}: {source}")]
    GlobWalk {
        pattern: String,
        #[source]
        source: glob::GlobError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deploy: DeployDefaults,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub secrets: Option<SecretsConfig>,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    /// Optional registry credentials. When set, yoink resolves the named
    /// secrets from the configured `secrets:` provider and passes them
    /// to docker as `X-Registry-Auth` on every pull. Without this, only
    /// public registries (or hosts with cached `docker login` creds)
    /// will pull successfully.
    #[serde(default)]
    pub registry: Option<RegistryConfig>,
    /// Glob patterns (relative to this file's directory) of additional
    /// config fragments to load. Each fragment may declare `services`
    /// and/or `hooks.pre_deploy`; everything else (`hosts`, `secrets`,
    /// `deploy.network`) stays in the main file. Lets you put one
    /// service per file under `services/*.yaml` while keeping a single
    /// shared network and host fleet.
    #[serde(default)]
    pub include: Vec<String>,
    /// Directory the config was loaded from. Used as the base for
    /// resolving relative paths (e.g. `service.run.files` entries).
    /// Skipped during serde — populated only by `load_from_path`.
    #[serde(default, skip)]
    pub config_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub hooks: HookConfig,
}

/// Defaults that apply across every service. A service can override most
/// of these in its own block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployDefaults {
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub on_failure: OnFailure,
}

impl Default for DeployDefaults {
    fn default() -> Self {
        Self {
            network: default_network(),
            strategy: Strategy::default(),
            on_failure: OnFailure::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    #[default]
    Parallel,
    Rolling,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    #[default]
    Halt,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub address: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    /// Hostname of the registry (e.g. `4db05qgnlk.registry.depot.dev`).
    pub server: String,
    /// Secret key that holds the registry username. Resolved against
    /// the configured `secrets:` provider at deploy time.
    pub username_secret: String,
    /// Secret key that holds the registry password / API token.
    pub password_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Currently only "infisical" is supported.
    pub provider: String,
    pub project_id: String,
    pub environment: String,
    /// Optional path within the project (Infisical "folder").
    #[serde(default)]
    pub path: Option<String>,
    /// Self-hosted Infisical instance URL. When unset, the CLI default
    /// (the cloud `SaaS` at `app.infisical.com`) is used.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub image: String,
    /// Image tag. Optional: when omitted from the config the operator
    /// MUST pass `--tag <name>=<value>` (or per-service `--service x
    /// --tag <value>`) at deploy time. Useful for code-versioned
    /// services (api, web) where the right answer is "whatever CI
    /// just built" and a hard-coded tag in the file rots immediately.
    /// Stable infrastructure (caddy, redis, etc.) keeps a literal
    /// tag here so `task yoink:up` works without arguments.
    #[serde(default)]
    pub tag: Option<String>,
    /// Restrict to a subset of `[[hosts]]`. None = every configured host.
    #[serde(default)]
    pub hosts: Option<Vec<String>>,
    /// Literal env vars baked into the container.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of secret keys to fetch from the configured `[secrets]`
    /// provider and inject as env vars at deploy time. Each entry is
    /// the same string for the secret key and the env var name.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Inject a secret under a *different* env var name. Map of
    /// `env_var_name -> secret_key`. Useful when the secret in
    /// Infisical is stored under one name (e.g. `AUTH_DATABASE_MIGRATE_URL`)
    /// but the running container expects another (e.g. `AUTH_DATABASE_URL`).
    #[serde(default)]
    pub env_from_secrets: BTreeMap<String, String>,
    /// Extra labels to apply to the container (in addition to the yoink
    /// management labels).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Service-scoped pre-deploy hooks. Fire just before this service
    /// is reconciled, regardless of how many hosts it runs on.
    /// Filtering by `--service` skips both the service and its hooks
    /// (so `yoink up --service api` does NOT run web's migrations).
    #[serde(default)]
    pub pre_deploy: Vec<HookSpec>,
    pub run: ServiceRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRun {
    /// Application port the container listens on. Required when
    /// `healthcheck_path` is set (yoink uses it to build the probe URL).
    #[serde(default)]
    pub port: Option<u16>,
    /// HTTP path probed to confirm the new container is healthy. None
    /// disables the healthcheck — the swap goes straight from start to
    /// stop-old.
    #[serde(default)]
    pub healthcheck_path: Option<String>,
    #[serde(default = "default_healthcheck_timeout", with = "humantime_serde")]
    pub healthcheck_timeout: Duration,
    /// Number of container replicas to run per host for this service.
    /// Default 1. Replicas share the same network alias so the embedded
    /// docker DNS returns multiple A records — caddy (or any other
    /// in-network client) sees them as a round-robin upstream pool.
    /// Replicas are mutually exclusive with `publish:` because host
    /// port bindings are exclusive at the kernel level.
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default = "default_drain_timeout", with = "humantime_serde")]
    pub drain_timeout: Duration,

    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Vec<String>,

    #[serde(default)]
    pub options: RunOptions,
    /// docker-cli style port publishes: `"443:443"`, `"127.0.0.1:5050:80"`,
    /// `"8080:8080/tcp"`. Parsed at deploy time.
    #[serde(default)]
    pub publish: Vec<String>,
    /// docker-cli style bind mounts: `"/root/certs:/certs:ro"`,
    /// `"./config/Caddyfile:/etc/caddy/Caddyfile:ro"`. Host paths must
    /// already exist on the target host.
    #[serde(default)]
    pub binds: Vec<String>,
    /// docker-cli style named-volume mounts: `"caddy-data:/data"`.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Files to upload from the operator's machine to the target host
    /// before `docker create`, then bind-mount into the container.
    /// Format: `"local_path:container_path[:ro]"`.
    /// Local paths are resolved relative to the config file. The on-host
    /// staging path is content-addressed (a sha256 of the file) so
    /// concurrent deploys never overwrite each other and a config edit
    /// triggers a redeploy via the spec hash.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    #[serde(default)]
    pub network_aliases: Vec<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub tmpfs: BTreeMap<String, String>,
    #[serde(default)]
    pub restart: Option<String>,
    /// Override the container's effective user (e.g. `"0:0"` for otel).
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    #[serde(default)]
    pub pre_deploy: Vec<HookSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    pub name: String,
    pub image: String,
    /// Either a literal tag string or `{ service = "api" }` to mirror
    /// the deploy-time tag of another service in this config.
    pub tag: HookTag,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Same shape as on `ServiceConfig` — inject a secret value under
    /// a renamed env var. Lets a hook reuse a Kamal-era secret (e.g.
    /// `AUTH_DATABASE_MIGRATE_URL`) under the env name the container
    /// actually reads (`AUTH_DATABASE_URL`).
    #[serde(default)]
    pub env_from_secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum HookTag {
    Literal(String),
    Ref { service: String },
}

fn default_healthcheck_timeout() -> Duration {
    Duration::from_secs(60)
}

fn default_drain_timeout() -> Duration {
    Duration::from_secs(15)
}

fn default_network() -> String {
    "yoink".to_string()
}

fn default_replicas() -> u32 {
    1
}

/// Subset of `Config` permitted in fragment files included via the
/// main config's `include:` globs. Fragments may add services and
/// pre-deploy hooks; they cannot redefine `hosts`, `deploy.network`,
/// or `secrets` — those are global and live in the main file.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFragment {
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub hooks: HookConfig,
}

/// True when one of the platform-default docker socket paths exists.
/// Doesn't actually try to connect — that's a config-load fast path
/// and hangs would be much worse than a stale check.
fn local_socket_exists() -> bool {
    // Order matters: rootless / Docker Desktop / OrbStack on macOS
    // expose `~/.docker/run/docker.sock`; the system daemon symlinks
    // the same path at `/var/run/docker.sock`. Either one is good.
    if std::path::Path::new("/var/run/docker.sock").exists() {
        return true;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".docker/run/docker.sock");
        if p.exists() {
            return true;
        }
    }
    false
}

impl Config {
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        // Parse the main file *without* validation — fragments contribute
        // services + hooks, so duplicate / dangling-reference checks only
        // make sense after the merge.
        let mut cfg: Self = yaml_serde::from_str(&text)?;
        cfg.config_dir = path.parent().map(std::path::Path::to_path_buf);
        cfg.merge_includes()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = yaml_serde::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Add a synthetic `local` host pointing at the local docker
    /// socket — for the operator's laptop daemon, k3d/colima/orbstack
    /// dev environments, or anywhere yoink is run without a real
    /// fleet. Only fires when (a) the config doesn't already declare
    /// a `local` host and (b) one of the platform-default socket
    /// paths exists. No-op on Windows for now (npipe detection
    /// requires a connect attempt rather than a path check).
    ///
    /// Read-only commands (`tui`, `status`, `diff`, `version`) call
    /// this so the dashboard "just works" pointing at your laptop;
    /// destructive commands (`up`, `restart`, `prune`, `rollback`)
    /// don't, since silently deploying to localhost would surprise.
    pub fn push_local_host_if_socket(&mut self) {
        if self.hosts.iter().any(|h| h.address == "local") {
            return;
        }
        if !local_socket_exists() {
            return;
        }
        self.hosts.push(HostConfig {
            address: "local".to_string(),
            user: "local".to_string(),
        });
    }

    /// Expand `self.include` globs against `self.config_dir`, parse each
    /// match as a [`ConfigFragment`], and append its contributions onto
    /// `self`. Globs that match zero files are allowed (so commenting
    /// out a fragment doesn't break the main config); a glob with an
    /// invalid pattern is a hard error.
    fn merge_includes(&mut self) -> Result<(), ConfigError> {
        if self.include.is_empty() {
            return Ok(());
        }
        let base = self.config_dir.clone().unwrap_or_else(|| ".".into());
        // Collect matched paths first, sort for deterministic order
        // (filesystems return entries in arbitrary order; keeping the
        // service list stable means spec hashes don't churn between
        // identical configs on different boxes).
        let mut matched: Vec<std::path::PathBuf> = Vec::new();
        for pattern in &self.include {
            let abs_pat = if Path::new(pattern).is_absolute() {
                pattern.clone()
            } else {
                base.join(pattern).display().to_string()
            };
            let entries = glob::glob(&abs_pat).map_err(|source| ConfigError::GlobPattern {
                pattern: pattern.clone(),
                source,
            })?;
            for entry in entries {
                let path = entry.map_err(|source| ConfigError::GlobWalk {
                    pattern: pattern.clone(),
                    source,
                })?;
                matched.push(path);
            }
        }
        matched.sort();
        for path in matched {
            let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })?;
            let fragment: ConfigFragment = yaml_serde::from_str(&text)?;
            self.services.extend(fragment.services);
            self.hooks.pre_deploy.extend(fragment.hooks.pre_deploy);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), ConfigError> {
        if self.deploy.network.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "deploy.network must not be empty".into(),
            ));
        }
        if self.hosts.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one entry under `hosts:` required".into(),
            ));
        }
        for (i, host) in self.hosts.iter().enumerate() {
            if host.address.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hosts[{i}].address must not be empty"
                )));
            }
            if host.user.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hosts[{i}].user must not be empty"
                )));
            }
        }
        let host_addresses: std::collections::HashSet<&str> =
            self.hosts.iter().map(|h| h.address.as_str()).collect();

        if self.services.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one entry under `services:` required".into(),
            ));
        }
        let mut seen_names = std::collections::HashSet::new();
        for service in &self.services {
            validate_service_name(&service.name)?;
            if !seen_names.insert(service.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate [[service]] name {:?}",
                    service.name
                )));
            }
            if service.image.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.image must not be empty",
                    service.name
                )));
            }
            // `tag` is optional in the schema; when absent, the
            // operator must pass it via `--tag` at deploy time. We only
            // reject the explicitly-empty case here (`tag: ""`), which
            // is almost always a typo.
            if service.tag.as_deref().is_some_and(|t| t.trim().is_empty()) {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.tag is set but empty; remove the field or supply a value",
                    service.name
                )));
            }
            if let Some(subset) = &service.hosts {
                for h in subset {
                    if !host_addresses.contains(h.as_str()) {
                        return Err(ConfigError::Invalid(format!(
                            "service {:?}.hosts references unknown host {h:?}",
                            service.name
                        )));
                    }
                }
            }
            if service.run.replicas == 0 {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.replicas must be >= 1",
                    service.name
                )));
            }
            if service.run.replicas > 1 && !service.run.publish.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.replicas > 1 is incompatible with publish: \
                     host port bindings are exclusive — only one replica can bind",
                    service.name
                )));
            }
            if let Some(p) = &service.run.healthcheck_path
                && !p.starts_with('/')
            {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.healthcheck_path must start with '/'",
                    service.name
                )));
            }
            if service.run.healthcheck_path.is_some() && service.run.port.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "service {:?}.run.port required when healthcheck_path is set",
                    service.name
                )));
            }
        }

        if let Some(secrets) = &self.secrets
            && secrets.provider != "infisical"
        {
            return Err(ConfigError::Invalid(format!(
                "secrets.provider {:?} not supported (only \"infisical\")",
                secrets.provider
            )));
        }

        // Hook tag refs must point at a real service.
        for hook in &self.hooks.pre_deploy {
            if hook.image.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "hook {:?}.image must not be empty",
                    hook.name
                )));
            }
            if let HookTag::Ref { service } = &hook.tag
                && !seen_names.contains(service.as_str())
            {
                return Err(ConfigError::Invalid(format!(
                    "hook {:?}.tag references unknown service {service:?}",
                    hook.name
                )));
            }
        }

        Ok(())
    }
}

impl ServiceConfig {
    /// The set of hosts this service should run on, intersected against
    /// the global [[hosts]] list.
    #[must_use]
    pub fn applicable_hosts<'a>(&'a self, all: &'a [HostConfig]) -> Vec<&'a HostConfig> {
        match &self.hosts {
            Some(subset) => all
                .iter()
                .filter(|h| subset.iter().any(|addr| addr == &h.address))
                .collect(),
            None => all.iter().collect(),
        }
    }
}

fn validate_service_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::Invalid(
            "service.name must not be empty".into(),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty above");
    if !first.is_ascii_alphanumeric() {
        return Err(ConfigError::Invalid(format!(
            "service.name {name:?} must start with an alphanumeric character"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(ConfigError::Invalid(format!(
                "service.name {name:?} contains invalid character {c:?} (allowed: A-Z a-z 0-9 . _ -)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn minimal() -> &'static str {
        r#"
hosts:
  - address: host-a
    user: deploy

services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run:
      port: 3000
      healthcheck_path: /health
"#
    }

    /// Round-trip a Config + `ConfigFragment` via `load_from_path`, using a
    /// temp dir so glob expansion against `config_dir` works the same
    /// way it does in production.
    fn write_temp_tree(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("yoink-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, body) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, body).unwrap();
        }
        dir
    }

    #[test]
    fn include_glob_appends_services_from_fragments() {
        let dir = write_temp_tree(&[
            (
                "yoink.yaml",
                r#"
deploy:
  network: kamal
hosts:
  - { address: h1, user: root }
include:
  - services/*.yaml
"#,
            ),
            (
                "services/api.yaml",
                r#"
services:
  - name: api
    image: img/api
    tag: v1
    run: { port: 3000, healthcheck_path: /health }
"#,
            ),
            (
                "services/web.yaml",
                r#"
services:
  - name: web
    image: img/web
    tag: v1
    run: { port: 8080, healthcheck_path: /healthz }
hooks:
  pre_deploy:
    - name: migrate
      image: img/api
      tag: { service: api }
      cmd: ["bin/migrate"]
"#,
            ),
        ]);
        let cfg = Config::load_from_path(&dir.join("yoink.yaml")).unwrap();
        let names: Vec<&str> = cfg.services.iter().map(|s| s.name.as_str()).collect();
        // sorted by file name ⇒ api before web
        assert_eq!(names, vec!["api", "web"]);
        assert_eq!(cfg.deploy.network, "kamal");
        assert_eq!(cfg.hooks.pre_deploy.len(), 1);
        assert_eq!(cfg.hooks.pre_deploy[0].name, "migrate");
    }

    #[test]
    fn include_glob_with_zero_matches_is_ok() {
        let dir = write_temp_tree(&[(
            "yoink.yaml",
            r#"
hosts:
  - { address: h1, user: root }
services:
  - name: a
    image: img
    tag: v1
    run: {}
include:
  - "missing/*.yaml"
"#,
        )]);
        let cfg = Config::load_from_path(&dir.join("yoink.yaml")).unwrap();
        assert_eq!(cfg.services.len(), 1);
    }

    #[test]
    fn duplicate_service_across_files_is_rejected() {
        let dir = write_temp_tree(&[
            (
                "yoink.yaml",
                r#"
hosts:
  - { address: h1, user: root }
services:
  - name: dupe
    image: img
    tag: v1
    run: {}
include:
  - "fragments/*.yaml"
"#,
            ),
            (
                "fragments/dupe.yaml",
                r#"
services:
  - name: dupe
    image: img
    tag: v2
    run: {}
"#,
            ),
        ]);
        let err = Config::load_from_path(&dir.join("yoink.yaml")).unwrap_err();
        assert!(format!("{err}").contains("dupe"), "got: {err}");
    }

    #[test]
    fn parses_minimal() {
        let c = Config::parse_str(minimal()).unwrap();
        assert_eq!(c.deploy.network, "yoink");
        assert_eq!(c.hosts.len(), 1);
        assert_eq!(c.services.len(), 1);
        assert_eq!(c.services[0].name, "app-a");
        assert_eq!(c.services[0].tag.as_deref(), Some("latest"));
        assert_eq!(c.services[0].run.port, Some(3000));
        assert_eq!(
            c.services[0].run.healthcheck_path.as_deref(),
            Some("/health")
        );
        assert!(c.hooks.pre_deploy.is_empty());
    }

    #[test]
    fn parses_multiple_services() {
        let s = r#"
hosts:
  - { address: host-a, user: root }

services:
  - name: api
    image: registry/bt-api
    tag: a1b2c3d
    run:
      port: 8080
      healthcheck_path: /health
  - name: caddy
    image: caddy
    tag: 2-alpine
    run:
      publish: ["80:80", "443:443"]
      binds: ["/root/certs:/certs:ro"]
      volumes: ["caddy-data:/data"]
      options:
        cap_drop: [ALL]
        cap_add: [NET_BIND_SERVICE]
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(c.services.len(), 2);
        assert_eq!(c.services[0].name, "api");
        assert_eq!(c.services[1].name, "caddy");
        assert_eq!(c.services[1].run.publish, vec!["80:80", "443:443"]);
        assert_eq!(c.services[1].run.binds, vec!["/root/certs:/certs:ro"]);
        assert_eq!(c.services[1].run.volumes, vec!["caddy-data:/data"]);
        assert_eq!(c.services[1].run.options.cap_drop, vec!["ALL"]);
        assert!(c.services[1].run.healthcheck_path.is_none());
    }

    #[test]
    fn parses_pre_deploy_hooks() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 8080, healthcheck_path: /h }

hooks:
  pre_deploy:
    - name: bt-migrate
      image: registry/bt-api
      tag: { service: api }
      entrypoint: [/usr/local/bin/bt-migrate]
      secrets: [DATABASE_MIGRATE_URL]
    - name: auth-migrate
      image: registry/bt-web
      tag: latest
      entrypoint: [node]
      cmd: [/app/dist/server/migrate.js]
"#;
        let c = Config::parse_str(s).unwrap();
        assert_eq!(c.hooks.pre_deploy.len(), 2);
        assert_eq!(c.hooks.pre_deploy[0].name, "bt-migrate");
        assert!(matches!(
            c.hooks.pre_deploy[0].tag,
            HookTag::Ref { ref service } if service == "api"
        ));
        assert!(matches!(
            c.hooks.pre_deploy[1].tag,
            HookTag::Literal(ref t) if t == "latest"
        ));
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let bad = format!("{}\nzomg: true\n", minimal());
        let err = Config::parse_str(&bad).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn rejects_duplicate_service_names() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 8080, healthcheck_path: /h }
  - name: api
    image: i
    tag: v2
    run: { port: 8081, healthcheck_path: /h }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("duplicate")));
    }

    #[test]
    fn rejects_zero_services() {
        let s = "hosts:\n  - { address: h, user: u }\n";
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("services")));
    }

    #[test]
    fn rejects_zero_hosts() {
        let s = r#"
services:
  - { name: x, image: i, tag: v1, run: { port: 1 } }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("hosts")));
    }

    #[test]
    fn rejects_healthcheck_path_without_port() {
        let s = r#"
hosts:
  - { address: h, user: u }
services:
  - name: x
    image: i
    tag: v1
    run: { healthcheck_path: /h }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(m) if m.contains("port required")));
    }

    #[test]
    fn rejects_unknown_secret_provider() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: x
    image: i
    tag: v1
    run: { port: 1 }

secrets:
  provider: vault
  project_id: p
  environment: prod
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(m) if m.contains("provider")));
    }

    #[test]
    fn rejects_hook_tag_ref_to_unknown_service() {
        let s = r#"
hosts:
  - { address: h, user: u }

services:
  - name: api
    image: i
    tag: v1
    run: { port: 1 }

hooks:
  pre_deploy:
    - name: x
      image: i
      tag: { service: nope }
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(m) if m.contains("references unknown service")));
    }

    #[test]
    fn applicable_hosts_filters_subset() {
        let s = r#"
hosts:
  - { address: host-a, user: u }
  - { address: host-b, user: u }

services:
  - name: x
    image: i
    tag: v1
    hosts: [host-a]
    run: { port: 1 }
"#;
        let c = Config::parse_str(s).unwrap();
        let applicable = c.services[0].applicable_hosts(&c.hosts);
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].address, "host-a");
    }

    #[test]
    fn parses_examples_yoink_yaml() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("examples/yoink.yaml");
        let _ = Config::load_from_path(&path).expect("examples/yoink.yaml must parse");
    }
}
