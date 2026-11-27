//! Typed-config builders for bollard. Pure functions: take a `RunSpec`
//! plus the operator's [`crate::config::RunOptions`] and produce the
//! `ContainerCreateBody` we hand to bollard. Snapshot tests catch
//! regressions in how each option maps to a bollard field.

use std::collections::{BTreeMap, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointSettings, HealthConfig, HostConfig, NetworkingConfig, PortBinding,
    RestartPolicy, RestartPolicyNameEnum,
};
use sha2::{Digest, Sha256};

use crate::config::RunOptions;
use crate::image::image_reference;
use crate::run_args::{
    BuildError, default_bind_to_ro, harden_tmpfs_options, parse_cpus, parse_device, parse_memory,
    parse_port_bindings,
};

/// Number of hex characters from the spec hash that go into the
/// container name suffix. 8 chars = 32 bits = ~65k services before 50%
/// chance of collision (we'll never get close).
pub const SHORT_HASH_LEN: usize = 8;

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
    /// Docker-native HEALTHCHECK directive (`Config.Healthcheck`).
    /// Distinct from `ServiceRun.healthcheck_path`, which yoink uses
    /// as its deploy-time HTTP probe gate. When set, this becomes
    /// the container-image-level healthcheck that `docker ps` /
    /// `docker inspect` / lazydocker / cAdvisor read — i.e. the
    /// "is this container running cleanly RIGHT NOW" signal that
    /// outlives the deploy. Currently populated by yoink only for
    /// the synthesized `yoink-proxy`; user services inherit whatever
    /// HEALTHCHECK their image's Dockerfile declares.
    pub docker_healthcheck: Option<HealthConfig>,
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
        image: Some(image_reference(&spec.image, &spec.tag)),
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
        healthcheck: spec.docker_healthcheck.clone(),
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
        image: Some(image_reference(image, tag)),
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
    let network = spec.networks.first().map_or("bridge", String::as_str);
    let memory = options.memory.as_deref().map(parse_memory).transpose()?;

    let restart_policy = match options.restart.unwrap_or_default() {
        crate::config::RestartPolicy::No => RestartPolicyNameEnum::NO,
        crate::config::RestartPolicy::Always => RestartPolicyNameEnum::ALWAYS,
        crate::config::RestartPolicy::UnlessStopped => RestartPolicyNameEnum::UNLESS_STOPPED,
        crate::config::RestartPolicy::OnFailure => RestartPolicyNameEnum::ON_FAILURE,
    };

    let tmpfs: Option<HashMap<String, String>> = if options.tmpfs.is_empty() {
        None
    } else {
        Some(
            options
                .tmpfs
                .iter()
                .map(|(k, v)| (k.clone(), harden_tmpfs_options(v)))
                .collect(),
        )
    };

    let cap_drop = vec_opt(&options.cap_drop);
    let cap_add = vec_opt(&options.cap_add);
    let security_opt = vec_opt(&options.security_opt);

    let devices = if options.devices.is_empty() {
        None
    } else {
        Some(
            options
                .devices
                .iter()
                .map(|s| parse_device(s))
                .collect::<Result<Vec<_>, _>>()?,
        )
    };

    let mut binds: Vec<String> = Vec::new();
    binds.extend(spec.binds.iter().map(|b| default_bind_to_ro(b)));
    binds.extend(spec.volumes.iter().cloned());
    let binds = vec_opt(&binds);
    let port_bindings = if port_bindings.is_empty() {
        None
    } else {
        Some(port_bindings)
    };

    let nano_cpus = options.cpus.as_deref().map(parse_cpus).transpose()?;

    Ok(HostConfig {
        memory,
        nano_cpus,
        pids_limit: options.pids_limit,
        cap_drop,
        cap_add,
        security_opt,
        readonly_rootfs: Some(options.read_only),
        tmpfs,
        binds,
        port_bindings,
        devices,
        restart_policy: Some(RestartPolicy {
            name: Some(restart_policy),
            maximum_retry_count: None,
        }),
        network_mode: Some(network.to_string()),
        auto_remove: Some(false),
        init: Some(options.init),
        ..Default::default()
    })
}

fn build_networking_config(
    networks: &[String],
    container_name: &str,
    options: &RunOptions,
) -> NetworkingConfig {
    // Only the first (primary) network goes here — docker silently
    // drops endpoints_config entries that don't match
    // host_config.network_mode at create time. Additional networks
    // get attached post-start via `connect_container_network` in
    // the deploy code, with the same aliases. The container is
    // reachable as `container_name` on every attached network by
    // virtue of docker's auto-alias plus our explicit one.
    let mut aliases = vec![container_name.to_string()];
    aliases.extend(options.network_aliases.iter().cloned());
    let mut endpoints = HashMap::new();
    if let Some(primary) = networks.first() {
        endpoints.insert(
            primary.clone(),
            EndpointSettings {
                aliases: Some(aliases),
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
    if let Some(hc) = &spec.docker_healthcheck {
        // Stringify each field independently. test is the load-bearing
        // bit; interval/timeout/retries/start_period are tuning knobs
        // that should still trigger a re-roll when changed (e.g.
        // bumping retries from 3 → 5).
        feed_opt_list(&mut h, "healthcheck.test", hc.test.as_deref());
        feed(
            &mut h,
            "healthcheck.interval_ns",
            hc.interval.unwrap_or(0).to_le_bytes().as_slice(),
        );
        feed(
            &mut h,
            "healthcheck.timeout_ns",
            hc.timeout.unwrap_or(0).to_le_bytes().as_slice(),
        );
        feed(
            &mut h,
            "healthcheck.retries",
            hc.retries.unwrap_or(0).to_le_bytes().as_slice(),
        );
        feed(
            &mut h,
            "healthcheck.start_period_ns",
            hc.start_period.unwrap_or(0).to_le_bytes().as_slice(),
        );
    }

    let opts = &spec.options;
    feed_list(&mut h, "network_aliases", &opts.network_aliases);
    feed(
        &mut h,
        "memory",
        opts.memory.as_deref().unwrap_or("").as_bytes(),
    );
    feed(
        &mut h,
        "cpus",
        opts.cpus.as_deref().unwrap_or("").as_bytes(),
    );
    feed(
        &mut h,
        "pids_limit",
        opts.pids_limit
            .map(|n| n.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    feed_list(&mut h, "cap_drop", &opts.cap_drop);
    feed_list(&mut h, "cap_add", &opts.cap_add);
    feed_list(&mut h, "security_opt", &opts.security_opt);
    feed_list(&mut h, "devices", &opts.devices);
    feed(&mut h, "read_only", &[u8::from(opts.read_only)]);
    feed(&mut h, "init", &[u8::from(opts.init)]);
    feed_map(&mut h, "tmpfs", &opts.tmpfs);
    feed(
        &mut h,
        "restart",
        opts.restart
            .map_or("", crate::config::RestartPolicy::as_str)
            .as_bytes(),
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

    /// Regression: hook containers formatted `{image}:{tag}` by hand,
    /// so a digest tag produced the invalid `repo:sha256:<hex>`.
    #[test]
    fn hook_container_uses_at_separator_for_digest_tags() {
        let build = |tag: &str| {
            build_hook_container(
                "registry.example.com/migrations",
                tag,
                "yoink",
                None,
                &[],
                &BTreeMap::new(),
            )
            .image
        };
        assert_eq!(
            build("sha256:abc123").as_deref(),
            Some("registry.example.com/migrations@sha256:abc123"),
        );
        assert_eq!(
            build("v1.2.3").as_deref(),
            Some("registry.example.com/migrations:v1.2.3"),
        );
    }

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
                cpus: Some("1.5".into()),
                pids_limit: Some(512),
                cap_drop: vec!["ALL".into()],
                cap_add: vec![],
                security_opt: vec!["no-new-privileges".into()],
                read_only: true,
                tmpfs,
                network_aliases: vec!["api".into()],
                restart: None,
                user: None,
                init: true,
                devices: Vec::new(),
            },
            entrypoint: None,
            command: vec![],
            publish: vec![],
            binds: vec![],
            volumes: vec![],
            docker_healthcheck: None,
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
        // tmpfs options get auto-hardened with noexec/nosuid/nodev.
        let opts = tmpfs.get("/tmp").unwrap();
        assert!(opts.contains("size=64m"));
        assert!(opts.contains("mode=1777"));
        assert!(opts.contains("noexec"));
        assert!(opts.contains("nosuid"));
        assert!(opts.contains("nodev"));
        assert_eq!(host.network_mode.as_deref(), Some("yoink"));
        assert_eq!(host.auto_remove, Some(false));
        let rp = host.restart_policy.unwrap();
        assert_eq!(rp.name, Some(RestartPolicyNameEnum::UNLESS_STOPPED));
    }

    #[test]
    fn build_container_emits_docker_healthcheck_when_set() {
        let mut spec = sample_spec();
        spec.docker_healthcheck = Some(HealthConfig {
            test: Some(vec![
                "CMD-SHELL".into(),
                "wget --spider --quiet http://127.0.0.1:2019/config/ || exit 1".into(),
            ]),
            interval: Some(30 * 1_000_000_000),
            timeout: Some(5 * 1_000_000_000),
            retries: Some(3),
            start_period: Some(10 * 1_000_000_000),
            ..Default::default()
        });
        let body = build_container(&spec).unwrap();
        let hc = body.healthcheck.expect("healthcheck on body");
        let test = hc.test.unwrap();
        assert_eq!(test[0], "CMD-SHELL");
        assert!(test[1].contains("127.0.0.1:2019/config/"));
        assert_eq!(hc.interval, Some(30_000_000_000));
        assert_eq!(hc.retries, Some(3));
    }

    #[test]
    fn spec_hash_differs_when_healthcheck_changes() {
        let base = sample_spec();
        let baseline = compute_spec_hash(&base);
        let mut with_hc = base.clone();
        with_hc.docker_healthcheck = Some(HealthConfig {
            test: Some(vec!["CMD".into(), "true".into()]),
            ..Default::default()
        });
        let with_hc_hash = compute_spec_hash(&with_hc);
        assert_ne!(baseline, with_hc_hash, "adding a healthcheck must reroll");
        let mut tighter = with_hc.clone();
        tighter.docker_healthcheck.as_mut().unwrap().retries = Some(5);
        assert_ne!(
            with_hc_hash,
            compute_spec_hash(&tighter),
            "tightening retries must reroll"
        );
    }

    #[test]
    fn build_container_default_options_yields_secure_host_config() {
        // RunOptions::default() encodes the secure-by-default profile:
        // every cap dropped, no-new-privileges set, immutable rootfs,
        // pids_limit bounded; memory + cpus stay uncapped.
        let mut spec = sample_spec();
        spec.options = RunOptions::default();
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.memory, None);
        assert_eq!(host.nano_cpus, None);
        assert_eq!(host.pids_limit, Some(1024));
        assert_eq!(host.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(host.cap_add, None);
        assert_eq!(
            host.security_opt,
            Some(vec!["no-new-privileges:true".to_string()])
        );
        assert_eq!(host.readonly_rootfs, Some(true));
        assert_eq!(host.tmpfs, None);
    }

    #[test]
    fn build_container_explicit_opt_outs_yield_lax_host_config() {
        // Operators with a stubborn legacy image opt out per knob.
        let mut spec = sample_spec();
        spec.options = RunOptions {
            cap_drop: vec![],
            security_opt: vec![],
            read_only: false,
            pids_limit: None,
            ..RunOptions::default()
        };
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.cap_drop, None);
        assert_eq!(host.security_opt, None);
        assert_eq!(host.readonly_rootfs, Some(false));
        assert_eq!(host.pids_limit, None);
    }

    #[test]
    fn build_container_default_options_set_init_true() {
        let mut spec = sample_spec();
        spec.options = RunOptions::default();
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.init, Some(true));
    }

    #[test]
    fn build_container_init_false_opt_out() {
        let mut spec = sample_spec();
        spec.options = RunOptions {
            init: false,
            ..RunOptions::default()
        };
        let body = build_container(&spec).unwrap();
        let host = body.host_config.unwrap();
        assert_eq!(host.init, Some(false));
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

    #[test]
    fn build_host_config_emits_devices_when_set() {
        let mut spec = sample_spec();
        spec.options.devices = vec!["/dev/fuse".into(), "/dev/dri:/dev/dri:rw".into()];
        let host = build_host_config(&spec, HashMap::new()).unwrap();
        let devices = host.devices.expect("devices should be set");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].path_on_host.as_deref(), Some("/dev/fuse"));
        assert_eq!(devices[0].cgroup_permissions.as_deref(), Some("rwm"));
        assert_eq!(devices[1].path_in_container.as_deref(), Some("/dev/dri"));
        assert_eq!(devices[1].cgroup_permissions.as_deref(), Some("rw"));
    }

    #[test]
    fn build_host_config_omits_devices_when_empty() {
        let host = build_host_config(&sample_spec(), HashMap::new()).unwrap();
        assert!(host.devices.is_none());
    }

    #[test]
    fn spec_hash_differs_when_devices_change() {
        let base = compute_spec_hash(&sample_spec());

        let mut with_dev = sample_spec();
        with_dev.options.devices = vec!["/dev/fuse".into()];
        assert_ne!(base, compute_spec_hash(&with_dev));

        let mut other_dev = sample_spec();
        other_dev.options.devices = vec!["/dev/dri".into()];
        assert_ne!(compute_spec_hash(&with_dev), compute_spec_hash(&other_dev));
    }
}
