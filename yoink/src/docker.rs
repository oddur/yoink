//! Typed-config builders for bollard. Pure functions: take a `RunSpec`
//! plus the operator's [`crate::config::RunOptions`] and produce the
//! `ContainerCreateBody` we hand to bollard. Snapshot tests catch
//! regressions in how each option maps to a bollard field.

use std::collections::{BTreeMap, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, NetworkingConfig, RestartPolicy,
    RestartPolicyNameEnum,
};
use thiserror::Error;

use crate::config::RunOptions;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid memory value {0:?}: expected like \"512m\" or \"1g\"")]
    Memory(String),
    #[error("unknown restart policy {0:?}: expected one of no|always|unless-stopped|on-failure")]
    RestartPolicy(String),
}

/// Inputs for `bollard.create_container`. Owned so call sites don't
/// fight lifetimes.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub image: String,
    pub tag: String,
    pub network: String,
    pub container_name: String,
    pub labels: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub options: RunOptions,
    pub entrypoint: Option<Vec<String>>,
    pub command: Vec<String>,
}

/// Build the typed body for `bollard.create_container`.
pub fn build_container(spec: &RunSpec) -> Result<ContainerCreateBody, BuildError> {
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let labels: HashMap<String, String> = spec
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let host_config = build_host_config(&spec.options, &spec.network)?;
    let networking_config =
        build_networking_config(&spec.network, &spec.container_name, &spec.options);

    Ok(ContainerCreateBody {
        image: Some(format!("{}:{}", spec.image, spec.tag)),
        env: Some(env),
        labels: Some(labels),
        entrypoint: spec.entrypoint.clone(),
        cmd: if spec.command.is_empty() {
            None
        } else {
            Some(spec.command.clone())
        },
        host_config: Some(host_config),
        networking_config: Some(networking_config),
        ..Default::default()
    })
}

fn build_host_config(options: &RunOptions, network: &str) -> Result<HostConfig, BuildError> {
    let memory = options.memory.as_deref().map(parse_memory).transpose()?;

    let restart_policy = match options.restart.as_deref().unwrap_or("unless-stopped") {
        "no" => RestartPolicyNameEnum::NO,
        "always" => RestartPolicyNameEnum::ALWAYS,
        "unless-stopped" => RestartPolicyNameEnum::UNLESS_STOPPED,
        "on-failure" => RestartPolicyNameEnum::ON_FAILURE,
        other => return Err(BuildError::RestartPolicy(other.to_string())),
    };

    let tmpfs: Option<HashMap<String, String>> = if options.tmpfs.is_empty() {
        None
    } else {
        Some(
            options
                .tmpfs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    };

    let cap_drop = vec_opt(&options.cap_drop);
    let cap_add = vec_opt(&options.cap_add);
    let security_opt = vec_opt(&options.security_opt);

    Ok(HostConfig {
        memory,
        cap_drop,
        cap_add,
        security_opt,
        readonly_rootfs: Some(options.read_only),
        tmpfs,
        restart_policy: Some(RestartPolicy {
            name: Some(restart_policy),
            maximum_retry_count: None,
        }),
        network_mode: Some(network.to_string()),
        auto_remove: Some(false),
        ..Default::default()
    })
}

fn build_networking_config(
    network: &str,
    container_name: &str,
    options: &RunOptions,
) -> NetworkingConfig {
    // The container is always reachable on the shared docker network by
    // its container name (Docker's built-in alias). We add additional
    // user-supplied aliases on top, which is how Caddy / other apps
    // discover this service when its name isn't a stable URL.
    let mut aliases = vec![container_name.to_string()];
    aliases.extend(options.network_aliases.iter().cloned());
    let mut endpoints = HashMap::new();
    endpoints.insert(
        network.to_string(),
        EndpointSettings {
            aliases: Some(aliases),
            ..Default::default()
        },
    );
    NetworkingConfig {
        endpoints_config: Some(endpoints),
    }
}

fn vec_opt(v: &[String]) -> Option<Vec<String>> {
    if v.is_empty() { None } else { Some(v.to_vec()) }
}

/// Parse a docker-style memory value: bare bytes (`536870912`), or with
/// suffix `b`, `k`, `m`, `g` (case-insensitive). Returns the value in
/// bytes as `i64` (bollard's type).
pub fn parse_memory(s: &str) -> Result<i64, BuildError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(BuildError::Memory(s.to_string()));
    }
    let (num, mult) = match s.as_bytes().last() {
        Some(b'b' | b'B') => (&s[..s.len() - 1], 1_i64),
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1_024_i64),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1_024 * 1_024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1_024 * 1_024 * 1_024),
        Some(c) if c.is_ascii_digit() => (s, 1_i64),
        _ => return Err(BuildError::Memory(s.to_string())),
    };
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| BuildError::Memory(s.to_string()))?;
    Ok(n.saturating_mul(mult))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RunOptions;
    use pretty_assertions::assert_eq;

    fn sample_spec() -> RunSpec {
        let mut labels = BTreeMap::new();
        labels.insert("yoink.service".into(), "app-a".into());
        labels.insert("yoink.version".into(), "a1b2c3d".into());
        labels.insert("caddy".into(), "app-a.example.com".into());
        let mut env = BTreeMap::new();
        env.insert("LOG_LEVEL".into(), "info".into());
        let mut tmpfs = BTreeMap::new();
        tmpfs.insert("/tmp".into(), "size=64m,mode=1777".into());
        RunSpec {
            image: "registry.example.com/app-a".into(),
            tag: "a1b2c3d".into(),
            network: "yoink".into(),
            container_name: "app-a-a1b2c3d".into(),
            labels,
            env,
            options: RunOptions {
                memory: Some("512m".into()),
                cap_drop: vec!["ALL".into()],
                cap_add: vec![],
                security_opt: vec!["no-new-privileges".into()],
                read_only: true,
                tmpfs,
                network_aliases: vec!["api".into()],
                restart: None,
            },
            entrypoint: None,
            command: vec![],
        }
    }

    #[test]
    fn build_container_maps_options_to_host_config() {
        let body = build_container(&sample_spec()).unwrap();
        assert_eq!(
            body.image.as_deref(),
            Some("registry.example.com/app-a:a1b2c3d")
        );
        let labels = body.labels.unwrap();
        assert_eq!(labels.get("yoink.service"), Some(&"app-a".to_string()));
        assert_eq!(labels.get("yoink.version"), Some(&"a1b2c3d".to_string()));
        assert_eq!(labels.get("caddy"), Some(&"app-a.example.com".to_string()));
        assert_eq!(body.env.unwrap(), vec!["LOG_LEVEL=info".to_string()]);

        let host = body.host_config.unwrap();
        assert_eq!(host.memory, Some(512 * 1024 * 1024));
        assert_eq!(host.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(
            host.security_opt,
            Some(vec!["no-new-privileges".to_string()])
        );
        assert_eq!(host.readonly_rootfs, Some(true));
        let tmpfs = host.tmpfs.unwrap();
        assert_eq!(
            tmpfs.get("/tmp").map(String::as_str),
            Some("size=64m,mode=1777")
        );
        assert_eq!(host.network_mode.as_deref(), Some("yoink"));
        assert_eq!(host.auto_remove, Some(false));
        let rp = host.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::UNLESS_STOPPED));
    }

    #[test]
    fn build_container_minimal_spec() {
        let mut spec = sample_spec();
        spec.options = RunOptions::default();
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.memory, None);
        assert_eq!(host.cap_drop, None);
        assert_eq!(host.cap_add, None);
        assert_eq!(host.security_opt, None);
        assert_eq!(host.readonly_rootfs, Some(false));
        assert_eq!(host.tmpfs, None);
    }

    #[test]
    fn parse_memory_accepts_common_suffixes() {
        assert_eq!(parse_memory("1024").unwrap(), 1024);
        assert_eq!(parse_memory("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_memory("64k").unwrap(), 64 * 1024);
    }

    #[test]
    fn parse_memory_rejects_garbage() {
        assert!(parse_memory("").is_err());
        assert!(parse_memory("abc").is_err());
        assert!(parse_memory("12x").is_err());
    }

    #[test]
    fn build_container_propagates_unknown_restart_policy() {
        let mut spec = sample_spec();
        spec.options.restart = Some("explode".into());
        let err = build_container(&spec).unwrap_err();
        assert!(matches!(err, BuildError::RestartPolicy(_)));
    }

    #[test]
    fn network_aliases_include_container_name() {
        let body = build_container(&sample_spec()).unwrap();
        let net = body.networking_config.unwrap();
        let endpoints = net.endpoints_config.unwrap();
        let yoink_ep = endpoints.get("yoink").unwrap();
        let aliases = yoink_ep.aliases.as_ref().unwrap();
        assert!(aliases.contains(&"app-a-a1b2c3d".to_string()));
        assert!(aliases.contains(&"api".to_string()));
    }
}
