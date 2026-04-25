//! Typed-config builders for bollard. Pure functions: take a `RunSpec`
//! plus the operator's [`crate::config::RunOptions`] and produce the
//! `ContainerCreateBody` we hand to bollard. Snapshot tests catch
//! regressions in how each option maps to a bollard field.

use std::collections::{BTreeMap, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, NetworkingConfig, PortBinding,
    RestartPolicy, RestartPolicyNameEnum,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::RunOptions;

/// Number of hex characters from the spec hash that go into the
/// container name suffix. 8 chars = 32 bits = ~65k services before 50%
/// chance of collision (we'll never get close).
pub const SHORT_HASH_LEN: usize = 8;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid memory value {0:?}: expected like \"512m\" or \"1g\"")]
    Memory(String),
    #[error("unknown restart policy {0:?}: expected one of no|always|unless-stopped|on-failure")]
    RestartPolicy(String),
    #[error(
        "invalid publish spec {0:?}: expected \"host_port:container_port[/proto]\" or \"ip:host_port:container_port[/proto]\""
    )]
    Publish(String),
}

/// Inputs for `bollard.create_container`. Owned so call sites don't
/// fight lifetimes.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub image: String,
    pub tag: String,
    /// Docker networks the container attaches to. Always non-empty
    /// (validated upstream in `Config::validate`). The first entry
    /// is also used as `host_config.network_mode` (docker requires
    /// a primary), and as the network the healthcheck probe is
    /// attached to.
    pub networks: Vec<String>,
    pub container_name: String,
    pub labels: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub options: RunOptions,
    pub entrypoint: Option<Vec<String>>,
    pub command: Vec<String>,
    /// docker-cli publish strings, e.g. `"443:443"`, `"127.0.0.1:5050:80"`,
    /// `"8080:80/udp"`. Parsed into `host_config.port_bindings` plus the
    /// container's `exposed_ports`.
    pub publish: Vec<String>,
    /// docker-cli bind strings, e.g. `"/host/path:/container/path[:ro]"`.
    /// Passed through verbatim to `host_config.binds`.
    pub binds: Vec<String>,
    /// docker-cli volume strings, e.g. `"named-volume:/data"`. Concatenated
    /// onto `host_config.binds` (Docker accepts named volumes there too).
    pub volumes: Vec<String>,
}

/// Build the typed body for `bollard.create_container`.
pub fn build_container(spec: &RunSpec) -> Result<ContainerCreateBody, BuildError> {
    let env: Vec<String> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

    let labels: HashMap<String, String> = spec
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let port_bindings = parse_port_bindings(&spec.publish)?;
    let exposed_ports: Vec<String> = port_bindings.keys().cloned().collect();

    let host_config = build_host_config(spec, port_bindings)?;
    let networking_config =
        build_networking_config(&spec.networks, &spec.container_name, &spec.options);

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
        user: spec.options.user.clone(),
        exposed_ports: vec_opt(&exposed_ports),
        host_config: Some(host_config),
        networking_config: Some(networking_config),
        ..Default::default()
    })
}

/// Build a minimal `ContainerCreateBody` for a one-shot hook container.
/// Hooks share the deploy network so they can reach in-network services
/// (e.g. a migration hook hitting the database). They do not get the
/// full `RunSpec` treatment — no port publishing, mounts, or restart
/// policy.
#[must_use]
pub fn build_hook_container(
    image: &str,
    tag: &str,
    network: &str,
    entrypoint: Option<&[String]>,
    cmd: &[String],
    env: &BTreeMap<String, String>,
) -> ContainerCreateBody {
    let env_pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut endpoints = HashMap::new();
    endpoints.insert(network.to_string(), EndpointSettings::default());
    ContainerCreateBody {
        image: Some(format!("{image}:{tag}")),
        env: if env_pairs.is_empty() {
            None
        } else {
            Some(env_pairs)
        },
        entrypoint: entrypoint.map(<[String]>::to_vec),
        cmd: if cmd.is_empty() {
            None
        } else {
            Some(cmd.to_vec())
        },
        host_config: Some(HostConfig {
            network_mode: Some(network.to_string()),
            auto_remove: Some(false),
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(endpoints),
        }),
        ..Default::default()
    }
}

fn build_host_config(
    spec: &RunSpec,
    port_bindings: HashMap<String, Option<Vec<PortBinding>>>,
) -> Result<HostConfig, BuildError> {
    let options = &spec.options;
    // Docker requires a single primary `network_mode`; we use the
    // first declared network and attach the rest via the
    // networking_config endpoints map.
    let network = spec
        .networks
        .first()
        .map_or("bridge", String::as_str);
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

    let mut binds: Vec<String> = Vec::new();
    binds.extend(spec.binds.iter().cloned());
    binds.extend(spec.volumes.iter().cloned());
    let binds = vec_opt(&binds);
    let port_bindings = if port_bindings.is_empty() {
        None
    } else {
        Some(port_bindings)
    };

    Ok(HostConfig {
        memory,
        cap_drop,
        cap_add,
        security_opt,
        readonly_rootfs: Some(options.read_only),
        tmpfs,
        binds,
        port_bindings,
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
    networks: &[String],
    container_name: &str,
    options: &RunOptions,
) -> NetworkingConfig {
    // The container is reachable on each attached network by its
    // container name (docker's built-in alias). User-supplied
    // aliases are added to every network — same alias works
    // regardless of which network the caller dialed in on.
    let mut aliases = vec![container_name.to_string()];
    aliases.extend(options.network_aliases.iter().cloned());
    let mut endpoints = HashMap::new();
    for network in networks {
        endpoints.insert(
            network.clone(),
            EndpointSettings {
                aliases: Some(aliases.clone()),
                ..Default::default()
            },
        );
    }
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

/// Parse docker-cli `--publish` strings into a port-bindings map keyed
/// by `"<container_port>/<proto>"` (the format bollard expects). Accepts:
///   - `"8080:80"`
///   - `"8080:80/udp"`
///   - `"127.0.0.1:8080:80"` (host-ip pin)
///   - `"127.0.0.1:8080:80/tcp"`
///
/// Multiple specs targeting the same container port stack into the
/// returned `Vec<PortBinding>` (Docker supports binding the same
/// container port to multiple host ips).
fn parse_port_bindings(
    publish: &[String],
) -> Result<HashMap<String, Option<Vec<PortBinding>>>, BuildError> {
    let mut out: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for spec in publish {
        let (mapping, proto) = match spec.split_once('/') {
            Some((m, p)) if !p.is_empty() => (m, p),
            Some(_) => return Err(BuildError::Publish(spec.clone())),
            None => (spec.as_str(), "tcp"),
        };
        let parts: Vec<&str> = mapping.split(':').collect();
        let (host_ip, host_port, container_port) = match parts.as_slice() {
            [hp, cp] => (None, *hp, *cp),
            [ip, hp, cp] => (Some((*ip).to_string()), *hp, *cp),
            _ => return Err(BuildError::Publish(spec.clone())),
        };
        if host_port.is_empty()
            || container_port.is_empty()
            || host_port.parse::<u16>().is_err()
            || container_port.parse::<u16>().is_err()
        {
            return Err(BuildError::Publish(spec.clone()));
        }
        let key = format!("{container_port}/{proto}");
        let binding = PortBinding {
            host_ip,
            host_port: Some(host_port.to_string()),
        };
        out.entry(key)
            .or_insert_with(|| Some(Vec::new()))
            .as_mut()
            .expect("just inserted Some")
            .push(binding);
    }
    Ok(out)
}

/// Stable hex digest over every input that affects the running
/// container: image, tag, network, env, user labels, options, mounts,
/// entrypoint, command. Returned as 16 hex chars (the first 64 bits of
/// SHA-256). The first `SHORT_HASH_LEN` chars go into the container
/// name; the full string is persisted as the `yoink.spec_hash` label.
///
/// Inputs are concatenated with `\0` separators and field tags so the
/// hash is robust against re-ordering or empty fields hashing the
/// same as a missing one.
#[must_use]
pub fn compute_spec_hash(spec: &RunSpec) -> String {
    let mut h = Sha256::new();
    feed(&mut h, "image", spec.image.as_bytes());
    feed(&mut h, "tag", spec.tag.as_bytes());
    // Sort networks so the hash is stable across yaml reorderings.
    let mut networks_sorted = spec.networks.clone();
    networks_sorted.sort();
    feed_list(&mut h, "networks", &networks_sorted);
    feed_map(&mut h, "env", &spec.env);
    feed_map(&mut h, "labels", &spec.labels);
    feed_opt_list(&mut h, "entrypoint", spec.entrypoint.as_deref());
    feed_list(&mut h, "cmd", &spec.command);
    feed_list(&mut h, "publish", &spec.publish);
    feed_list(&mut h, "binds", &spec.binds);
    feed_list(&mut h, "volumes", &spec.volumes);

    let opts = &spec.options;
    feed_list(&mut h, "network_aliases", &opts.network_aliases);
    feed(
        &mut h,
        "memory",
        opts.memory.as_deref().unwrap_or("").as_bytes(),
    );
    feed_list(&mut h, "cap_drop", &opts.cap_drop);
    feed_list(&mut h, "cap_add", &opts.cap_add);
    feed_list(&mut h, "security_opt", &opts.security_opt);
    feed(&mut h, "read_only", &[u8::from(opts.read_only)]);
    feed_map(&mut h, "tmpfs", &opts.tmpfs);
    feed(
        &mut h,
        "restart",
        opts.restart.as_deref().unwrap_or("").as_bytes(),
    );
    feed(
        &mut h,
        "user",
        opts.user.as_deref().unwrap_or("").as_bytes(),
    );

    let digest = h.finalize();
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn feed(h: &mut Sha256, tag: &str, value: &[u8]) {
    h.update(tag.as_bytes());
    h.update(b"\0");
    h.update(value);
    h.update(b"\x1e"); // ASCII record separator
}

fn feed_list(h: &mut Sha256, tag: &str, values: &[String]) {
    h.update(tag.as_bytes());
    h.update(b"\0");
    for v in values {
        h.update(v.as_bytes());
        h.update(b"\x1f"); // ASCII unit separator
    }
    h.update(b"\x1e");
}

fn feed_opt_list(h: &mut Sha256, tag: &str, values: Option<&[String]>) {
    feed_list(h, tag, values.unwrap_or(&[]));
}

fn feed_map(h: &mut Sha256, tag: &str, map: &BTreeMap<String, String>) {
    h.update(tag.as_bytes());
    h.update(b"\0");
    for (k, v) in map {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(v.as_bytes());
        h.update(b"\x1f");
    }
    h.update(b"\x1e");
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
            networks: vec!["yoink".into()],
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
                user: None,
            },
            entrypoint: None,
            command: vec![],
            publish: vec![],
            binds: vec![],
            volumes: vec![],
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
    fn publish_binds_volumes_map_to_host_config() {
        let mut spec = sample_spec();
        spec.publish = vec!["443:443".into(), "127.0.0.1:8080:80/tcp".into()];
        spec.binds = vec!["/host/cfg:/etc/cfg:ro".into()];
        spec.volumes = vec!["mydata:/data".into()];
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();

        let binds = host.binds.unwrap();
        assert!(binds.contains(&"/host/cfg:/etc/cfg:ro".to_string()));
        assert!(binds.contains(&"mydata:/data".to_string()));

        let pb = host.port_bindings.unwrap();
        let p443 = pb.get("443/tcp").unwrap().as_ref().unwrap();
        assert_eq!(p443[0].host_port.as_deref(), Some("443"));
        assert_eq!(p443[0].host_ip, None);
        let p80 = pb.get("80/tcp").unwrap().as_ref().unwrap();
        assert_eq!(p80[0].host_port.as_deref(), Some("8080"));
        assert_eq!(p80[0].host_ip.as_deref(), Some("127.0.0.1"));

        let exposed = body.exposed_ports.unwrap();
        assert!(exposed.contains(&"443/tcp".to_string()));
        assert!(exposed.contains(&"80/tcp".to_string()));
    }

    #[test]
    fn parse_port_bindings_accepts_udp_and_rejects_garbage() {
        let mut spec = sample_spec();
        spec.publish = vec!["53:53/udp".into()];
        let body = build_container(&spec).unwrap();
        let pb = body.host_config.unwrap().port_bindings.unwrap();
        assert!(pb.contains_key("53/udp"));

        let mut bad = sample_spec();
        bad.publish = vec!["nope".into()];
        assert!(matches!(build_container(&bad), Err(BuildError::Publish(_))));

        let mut bad2 = sample_spec();
        bad2.publish = vec!["80:notaport".into()];
        assert!(matches!(
            build_container(&bad2),
            Err(BuildError::Publish(_))
        ));
    }

    #[test]
    fn compute_spec_hash_includes_publish_binds_volumes() {
        let base = compute_spec_hash(&sample_spec());

        let mut p = sample_spec();
        p.publish = vec!["443:443".into()];
        assert_ne!(base, compute_spec_hash(&p));

        let mut b = sample_spec();
        b.binds = vec!["/h:/c".into()];
        assert_ne!(base, compute_spec_hash(&b));

        let mut v = sample_spec();
        v.volumes = vec!["data:/d".into()];
        assert_ne!(base, compute_spec_hash(&v));
    }

    #[test]
    fn compute_spec_hash_is_stable_and_short() {
        let h1 = compute_spec_hash(&sample_spec());
        let h2 = compute_spec_hash(&sample_spec());
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_spec_hash_ignores_container_name() {
        let mut a = sample_spec();
        let mut b = sample_spec();
        a.container_name = "anything".into();
        b.container_name = "different".into();
        assert_eq!(compute_spec_hash(&a), compute_spec_hash(&b));
    }

    #[test]
    fn compute_spec_hash_changes_when_inputs_change() {
        let base = compute_spec_hash(&sample_spec());

        let mut tag = sample_spec();
        tag.tag = "ffffff0".into();
        assert_ne!(base, compute_spec_hash(&tag));

        let mut env = sample_spec();
        env.env.insert("NEW".into(), "v".into());
        assert_ne!(base, compute_spec_hash(&env));

        let mut mem = sample_spec();
        mem.options.memory = Some("1g".into());
        assert_ne!(base, compute_spec_hash(&mem));

        let mut ro = sample_spec();
        ro.options.read_only = !ro.options.read_only;
        assert_ne!(base, compute_spec_hash(&ro));
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
