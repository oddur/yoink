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
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub service: ServiceConfig,
    #[serde(default)]
    pub deploy: DeployConfig,
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub secrets: Option<SecretsConfig>,
    #[serde(default)]
    pub caddy: Option<CaddyConfig>,
    pub run: RunConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub image: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default = "default_healthcheck_path")]
    pub healthcheck_path: String,
    #[serde(default = "default_healthcheck_timeout", with = "humantime_serde")]
    pub healthcheck_timeout: Duration,
    #[serde(default = "default_drain_timeout", with = "humantime_serde")]
    pub drain_timeout: Duration,
    #[serde(default)]
    pub on_failure: OnFailure,
    #[serde(default = "default_network")]
    pub network: String,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            healthcheck_path: default_healthcheck_path(),
            healthcheck_timeout: default_healthcheck_timeout(),
            drain_timeout: default_drain_timeout(),
            on_failure: OnFailure::default(),
            network: default_network(),
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
pub struct SecretsConfig {
    pub provider: String,
    pub project_id: String,
    pub environment: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaddyConfig {
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub port: u16,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub options: RunOptions,
}

/// Typed mirror of the docker-run flags yoink supports. Maps directly to
/// bollard's `HostConfig` fields. Any flag not listed here is unsupported
/// — set it on the image's Dockerfile or move the workload to a docker
/// compose accessory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    /// `--memory` in human bytes (e.g. `"512m"`, `"1g"`). Internally stored
    /// raw and parsed at deploy time so config errors surface there.
    #[serde(default)]
    pub memory: Option<String>,
    /// `--cap-drop`. Defaults to empty (no caps dropped).
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// `--cap-add`. Use sparingly; favor dropping all and adding back.
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// `--security-opt`. Repeated flag: `["no-new-privileges"]`.
    #[serde(default)]
    pub security_opt: Vec<String>,
    /// `--read-only` rootfs.
    #[serde(default)]
    pub read_only: bool,
    /// `--tmpfs` mounts: `{ "/tmp" = "size=64m,mode=1777" }`.
    #[serde(default)]
    pub tmpfs: BTreeMap<String, String>,
    /// `--network-alias` entries (in addition to the container name itself).
    #[serde(default)]
    pub network_aliases: Vec<String>,
    /// `--restart` policy. Defaults to `unless-stopped` at deploy time.
    #[serde(default)]
    pub restart: Option<String>,
}

fn default_healthcheck_path() -> String {
    "/health".to_string()
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

impl Config {
    /// Read and parse a config file from disk.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse_str(&text)
    }

    /// Parse and validate a config from a TOML string.
    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        validate_service_name(&self.service.name)?;
        if self.service.image.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "service.image must not be empty".into(),
            ));
        }
        if !self.deploy.healthcheck_path.starts_with('/') {
            return Err(ConfigError::Invalid(format!(
                "deploy.healthcheck_path must start with '/' (got {:?})",
                self.deploy.healthcheck_path
            )));
        }
        if self.deploy.network.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "deploy.network must not be empty".into(),
            ));
        }
        if self.hosts.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[hosts]] required".into(),
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
        if self.run.port == 0 {
            return Err(ConfigError::Invalid("run.port must not be 0".into()));
        }
        if let Some(secrets) = &self.secrets
            && secrets.provider != "infisical"
        {
            return Err(ConfigError::Invalid(format!(
                "secrets.provider {:?} not supported (only \"infisical\")",
                secrets.provider
            )));
        }
        Ok(())
    }
}

/// Service name must be Docker-name-safe: leading alnum, then [a-zA-Z0-9._-].
/// We also reject empty/whitespace-only names.
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
[service]
name = "app-a"
image = "registry.example.com/app-a"

[[hosts]]
address = "host-a"
user = "deploy"

[run]
port = 3000
"#
    }

    #[test]
    fn parses_minimal_config_with_defaults() {
        let config = Config::parse_str(minimal()).unwrap();
        assert_eq!(config.service.name, "app-a");
        assert_eq!(config.service.image, "registry.example.com/app-a");
        assert_eq!(config.deploy.strategy, Strategy::Parallel);
        assert_eq!(config.deploy.healthcheck_path, "/health");
        assert_eq!(config.deploy.healthcheck_timeout, Duration::from_secs(60));
        assert_eq!(config.deploy.drain_timeout, Duration::from_secs(15));
        assert_eq!(config.deploy.on_failure, OnFailure::Halt);
        assert_eq!(config.deploy.network, "yoink");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].address, "host-a");
        assert_eq!(config.hosts[0].user, "deploy");
        assert!(config.secrets.is_none());
        assert!(config.caddy.is_none());
        assert_eq!(config.run.port, 3000);
        assert_eq!(config.run.options, RunOptions::default());
        assert!(config.run.env.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let err = Config::parse_str(&format!("{}\nzomg = true\n", minimal())).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn rejects_unknown_subtable_field() {
        let bad = r#"
[service]
name = "app-a"
image = "x"
nope = 1

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn parses_durations_humantime() {
        let bad = r#"
[service]
name = "x"
image = "y"

[deploy]
healthcheck_timeout = "2m"
drain_timeout = "500ms"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let config = Config::parse_str(bad).unwrap();
        assert_eq!(config.deploy.healthcheck_timeout, Duration::from_secs(120));
        assert_eq!(config.deploy.drain_timeout, Duration::from_millis(500));
    }

    #[test]
    fn rejects_empty_service_name() {
        let bad = r#"
[service]
name = ""
image = "x"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("service.name")));
    }

    #[test]
    fn rejects_invalid_service_name_char() {
        let bad = r#"
[service]
name = "app/a"
image = "x"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(
            matches!(&err, ConfigError::Invalid(s) if s.contains("invalid character")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_service_name_starting_with_punctuation() {
        let bad = r#"
[service]
name = "-foo"
image = "x"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("alphanumeric")));
    }

    #[test]
    fn rejects_empty_image() {
        let bad = r#"
[service]
name = "ok"
image = ""

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("service.image")));
    }

    #[test]
    fn rejects_healthcheck_path_without_leading_slash() {
        let bad = r#"
[service]
name = "ok"
image = "x"

[deploy]
healthcheck_path = "health"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("healthcheck_path")));
    }

    #[test]
    fn rejects_empty_network() {
        let bad = r#"
[service]
name = "ok"
image = "x"

[deploy]
network = ""

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("network")));
    }

    #[test]
    fn rejects_zero_port() {
        let bad = r#"
[service]
name = "ok"
image = "x"

[[hosts]]
address = "h"
user = "u"

[run]
port = 0
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("run.port")));
    }

    #[test]
    fn rejects_zero_hosts() {
        // `hosts = []` must appear before any table headers so it is parsed
        // as a top-level array, not as a key inside the previous table.
        let bad = r#"
hosts = []

[service]
name = "ok"
image = "x"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("hosts")));
    }

    #[test]
    fn rejects_empty_host_address() {
        let bad = r#"
[service]
name = "ok"
image = "x"

[[hosts]]
address = ""
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("address")));
    }

    #[test]
    fn rejects_unknown_secret_provider() {
        let bad = r#"
[service]
name = "ok"
image = "x"

[[hosts]]
address = "h"
user = "u"

[secrets]
provider = "vault"
project_id = "p"
environment = "prod"
path = "/x"

[run]
port = 1
"#;
        let err = Config::parse_str(bad).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(s) if s.contains("provider")));
    }

    #[test]
    fn accepts_strategy_rolling() {
        let s = r#"
[service]
name = "ok"
image = "x"

[deploy]
strategy = "rolling"
on_failure = "continue"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let config = Config::parse_str(s).unwrap();
        assert_eq!(config.deploy.strategy, Strategy::Rolling);
        assert_eq!(config.deploy.on_failure, OnFailure::Continue);
    }

    #[test]
    fn rejects_unknown_strategy() {
        let s = r#"
[service]
name = "ok"
image = "x"

[deploy]
strategy = "blue-green"

[[hosts]]
address = "h"
user = "u"

[run]
port = 1
"#;
        let err = Config::parse_str(s).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn parses_full_config_from_spec() {
        let s = r#"
[service]
name = "app-a"
image = "registry.example.com/app-a"

[deploy]
strategy = "rolling"
healthcheck_path = "/health"
healthcheck_timeout = "60s"
drain_timeout = "15s"
on_failure = "halt"
network = "yoink"

[[hosts]]
address = "host-a"
user = "deploy"

[[hosts]]
address = "host-b"
user = "deploy"

[secrets]
provider = "infisical"
project_id = "p"
environment = "prod"
path = "/app-a"

[caddy]
labels = ["caddy=app-a.example.com"]

[run]
port = 3000

[run.env]
LOG_LEVEL = "info"

[run.options]
memory = "512m"
cap_drop = ["ALL"]
security_opt = ["no-new-privileges"]
read_only = true
network_aliases = ["api"]

[run.options.tmpfs]
"/tmp" = "size=64m,mode=1777"
"#;
        let config = Config::parse_str(s).unwrap();
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.deploy.strategy, Strategy::Rolling);
        assert_eq!(
            config.caddy.as_ref().unwrap().labels,
            vec!["caddy=app-a.example.com".to_string()]
        );
        assert_eq!(config.run.options.memory.as_deref(), Some("512m"));
        assert_eq!(config.run.options.cap_drop, vec!["ALL".to_string()]);
        assert!(config.run.options.read_only);
        assert_eq!(config.run.options.network_aliases, vec!["api".to_string()]);
        assert_eq!(
            config.run.options.tmpfs.get("/tmp").map(String::as_str),
            Some("size=64m,mode=1777")
        );
        assert_eq!(config.run.env.get("LOG_LEVEL"), Some(&"info".to_string()));
    }

    #[test]
    fn parses_examples_yoink_toml() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("examples/yoink.toml");
        let config = Config::load_from_path(&path).expect("examples/yoink.toml must parse");
        assert_eq!(config.service.name, "app-a");
        assert_eq!(config.run.port, 3000);
    }
}
