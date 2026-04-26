//! Deploy orchestration. The reconciliation loop is the same for every
//! service in the config — versioned app code (api, web) and stable
//! infrastructure (caddy, redis, otel, pgadmin) go through identical
//! steps. Per host, per service:
//!
//!  1. Ensure the shared docker network exists.
//!  2. Pull the new image:tag.
//!  3. Discover existing containers labeled with this service.
//!  4. Force-remove any container squatting on the new container's name.
//!  5. Create + start the new container with the service's labels, env,
//!     and run options.
//!  6. Poll the healthcheck (if configured) until 200, otherwise skip.
//!  7. Stop every previously-running container for this service.
//!
//! Hosts are iterated sequentially. A failure on any host short-circuits
//! that service; already-deployed hosts keep the new version.

use std::collections::BTreeMap;
use std::time::Duration;

use thiserror::Error;
use tracing::warn;

use crate::config::{Config, HookSpec, HookTag, ServiceConfig};
use crate::docker::{self, BuildError, RunSpec, SHORT_HASH_LEN};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, Host};
use crate::files::{self, FileError, FileMount};
use crate::healthcheck::{self, HealthcheckError};
use crate::network;
use crate::secrets::SecretsBundle;

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("docker error on {host}: {source}")]
    Docker {
        host: String,
        #[source]
        source: DockerError,
    },
    #[error("config build error: {0}")]
    Build(#[from] BuildError),
    #[error("healthcheck failed on {host} for {container}: {source}")]
    Healthcheck {
        host: String,
        container: String,
        #[source]
        source: HealthcheckError,
    },
    #[error("hook {name:?} failed: {message}")]
    Hook { name: String, message: String },
    #[error("file mount error for service {service:?}: {source}")]
    Files {
        service: String,
        #[source]
        source: FileError,
    },
    #[error(
        "service {service:?} has no `tag:` in config and was not given one via `--tag`; pass `--tag {service}=<value>` or add `tag:` to the service fragment"
    )]
    TagMissing { service: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployEvent {
    HookStarted {
        name: String,
    },
    HookFinished {
        name: String,
    },
    Started {
        service: String,
        tag: String,
        host: String,
    },
    NetworkReady {
        host: String,
        network: String,
        created: bool,
    },
    PullStarted {
        host: String,
        image: String,
        tag: String,
    },
    PullFinished {
        host: String,
    },
    ContainerStarted {
        host: String,
        container: String,
    },
    HealthcheckHealthy {
        host: String,
        container: String,
        attempts: u32,
    },
    HealthcheckSkipped {
        host: String,
        container: String,
    },
    OldContainerStopped {
        host: String,
        container: String,
    },
    AlreadyAtSpec {
        host: String,
        container: String,
    },
    /// Last N log lines from a failed container — fired right
    /// before a healthcheck-failed deploy errors out so the caller
    /// can surface "why did it die?" without separate fetch.
    ContainerLogTail {
        host: String,
        container: String,
        lines: Vec<String>,
    },
    Done {
        host: String,
        container: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDeployResult {
    pub host: String,
    pub container: String,
    pub healthcheck_attempts: u32,
    pub stopped_old: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDeployReport {
    pub service: String,
    pub tag: String,
    pub hosts: Vec<HostDeployResult>,
}

/// `<service>-<short_hash>` for the single-replica case (which is
/// most services), or `<service>-<short_hash>-<index>` when scaling
/// out. Keeping the un-suffixed form for `replicas == 1` preserves
/// backward compatibility with containers already running on hosts
/// that were deployed before yoink supported replicas.
#[must_use]
pub fn container_name(service: &str, spec_hash: &str, replica_index: u32, replicas: u32) -> String {
    let short = &spec_hash[..SHORT_HASH_LEN];
    if replicas == 1 {
        format!("{service}-{short}")
    } else {
        format!("{service}-{short}-{replica_index}")
    }
}

/// Yoink-managed labels written to every container we start. The
/// `spec_hash` (full hex digest) is the source of truth for drift
/// detection on the next reconcile.
#[must_use]
pub fn build_labels(
    service: &ServiceConfig,
    tag: &str,
    spec_hash: &str,
) -> BTreeMap<String, String> {
    let mut labels: BTreeMap<String, String> = service.labels.clone();
    labels.insert("yoink.managed".into(), "true".into());
    labels.insert("yoink.service".into(), service.name.clone());
    labels.insert("yoink.version".into(), tag.into());
    labels.insert("yoink.spec_hash".into(), spec_hash.into());
    // Audit trail: who ran the deploy + when. Read by `yoink history`
    // and the TUI history pane to answer "what changed and why?".
    // Best-effort: $USER falls back to "?" inside CI runners that
    // don't set it; the timestamp always works.
    labels.insert(
        "yoink.deployed-by".into(),
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "?".into()),
    );
    labels.insert(
        "yoink.deployed-at".into(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or_else(|_| "0".into(), |d| d.as_secs().to_string()),
    );
    labels
}

/// Env that goes onto the container: literal `service.env` plus any
/// secret keys the service requested from the operator's pre-fetched
/// `SecretsBundle`. Missing keys are silently skipped — the bundle is
/// the source of truth, and deploys against a partial bundle should
/// fail visibly when the container starts.
#[must_use]
pub fn build_env(
    service: &ServiceConfig,
    secrets: Option<&SecretsBundle>,
) -> BTreeMap<String, String> {
    let mut env = service.env.clone();
    if let Some(bundle) = secrets {
        for key in &service.secrets {
            if let Some(value) = bundle.get(key) {
                env.insert(key.clone(), value.to_string());
            }
        }
        for (env_name, secret_key) in &service.env_from_secrets {
            if let Some(value) = bundle.get(secret_key) {
                env.insert(env_name.clone(), value.to_string());
            }
        }
    }
    env
}

/// Reconcile every service in the config to the spec. Top-level entry
/// point for `yoink up`. `tag_overrides` lets the CLI override the tag
/// for specific services (`yoink up --service api --tag a1b2c3d`).
pub async fn reconcile(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<Vec<ServiceDeployReport>, DeployError> {
    // Run pre-deploy hooks first; one failure halts the whole reconcile.
    for hook in &config.hooks.pre_deploy {
        let tag = resolve_hook_tag(hook, tag_overrides, &config.services);
        on_event(DeployEvent::HookStarted {
            name: hook.name.clone(),
        });
        run_hook(ops, config, hook, &tag, secrets).await?;
        on_event(DeployEvent::HookFinished {
            name: hook.name.clone(),
        });
    }

    let mut reports = Vec::with_capacity(config.services.len());
    for service in &config.services {
        if let Some(filter) = services_filter
            && !filter.iter().any(|n| n == &service.name)
        {
            continue;
        }
        let tag = match tag_overrides.get(&service.name).cloned() {
            Some(t) => t,
            None => service.tag.clone().ok_or_else(|| DeployError::TagMissing {
                service: service.name.clone(),
            })?,
        };
        let report = deploy_service(ops, config, service, &tag, secrets, on_event).await?;
        reports.push(report);
    }
    Ok(reports)
}

/// Reconcile a single service across its applicable hosts.
pub async fn deploy_service(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<ServiceDeployReport, DeployError> {
    let hosts = service.applicable_hosts(&config.hosts);

    // ── Phase 1: PREPARE ──────────────────────────────────────────
    // For every host: ensure network, pull image, upload files,
    // create (but do not start) the new container. After this phase
    // every host has a fully-configured container that's just one
    // `docker start` away from running.
    let mut prepared: Vec<HostPrep> = Vec::with_capacity(hosts.len());
    for host_cfg in &hosts {
        let prep = prepare_one_host(ops, config, service, tag, secrets, host_cfg, on_event).await?;
        prepared.push(prep);
    }

    // ── Phase 2: HOOKS ────────────────────────────────────────────
    // Service-scoped pre-deploy hooks fire here. Image is already
    // pulled and the runtime container is already created — the only
    // step left after hooks return is `docker start`, which keeps the
    // gap between schema migration and runtime container start as
    // close to zero as possible. `--service` filtering skips the
    // service and its hooks together (`yoink up --service api` will
    // not run web's migrations).
    for hook in &service.pre_deploy {
        let hook_tag = resolve_hook_tag(hook, &BTreeMap::new(), &config.services);
        let resolved_tag = if matches!(hook.tag, HookTag::Ref { .. }) {
            // For `tag: { service: <self> }`, use the deploy-time tag
            // we were called with rather than the static fallback.
            tag.to_string()
        } else {
            hook_tag
        };
        on_event(DeployEvent::HookStarted {
            name: hook.name.clone(),
        });
        run_hook(ops, config, hook, &resolved_tag, secrets).await?;
        on_event(DeployEvent::HookFinished {
            name: hook.name.clone(),
        });
    }

    // ── Phase 3: FINALIZE ─────────────────────────────────────────
    // Start each pre-created container, healthcheck, swap out old.
    let mut host_results = Vec::with_capacity(prepared.len());
    for prep in prepared {
        let result = finalize_one_host(ops, config, service, prep, on_event).await?;
        host_results.push(result);
    }
    Ok(ServiceDeployReport {
        service: service.name.clone(),
        tag: tag.to_string(),
        hosts: host_results,
    })
}

/// One container replica's prep state. `already_running` short-circuits
/// the start step in finalize when the spec already matches what's on
/// the host.
struct ReplicaPrep {
    name: String,
    already_running: bool,
}

/// Output of [`prepare_one_host`]. Holds N replicas worth of prep state
/// plus shared per-host context (existing containers, swap mode).
struct HostPrep {
    host: Host,
    replicas: Vec<ReplicaPrep>,
    /// Existing containers labeled with this service. Used to drive
    /// stop-old after the new replicas are healthy.
    existing: Vec<ContainerInfo>,
    /// `service.run.publish` non-empty — port bindings are exclusive
    /// so old must go before new can `start`. Always false when
    /// `replicas > 1` (validation forbids combining the two).
    stop_first: bool,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_one_host(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    host_cfg: &crate::config::HostConfig,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<HostPrep, DeployError> {
    let host = Host::from(host_cfg);
    on_event(DeployEvent::Started {
        service: service.name.clone(),
        tag: tag.to_string(),
        host: host.address.clone(),
    });

    // Ensure every declared network exists on this host. Each
    // ensure_network call is idempotent (returns whether it had to
    // create) — emit one NetworkReady event per network.
    for net in &config.deploy.networks {
        let created = network::ensure(ops, &host, net)
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
        on_event(DeployEvent::NetworkReady {
            host: host.address.clone(),
            network: net.clone(),
            created,
        });
    }

    let credentials = registry_credentials(config, secrets);
    pull_image(ops, &host, &service.image, tag, credentials, on_event).await?;
    let existing = list_existing_containers(ops, &host, &service.name).await?;

    // Per-host (not per-replica) prep: file upload, spec hash. The
    // file binds + env + options are identical across replicas, so the
    // spec hash is identical too — only the container name differs.
    let resolved_files = resolve_files(config, service)?;
    let file_binds: Vec<String> = resolved_files
        .iter()
        .map(|(m, hash)| m.as_bind_string(hash))
        .collect();
    let probe_spec = build_run_spec(
        config,
        service,
        tag,
        secrets,
        String::new(),
        "",
        &file_binds,
    );
    let spec_hash = docker::compute_spec_hash(&probe_spec);

    // Files only need to land on the host once per service, regardless
    // of replica count. Idempotent (content-addressed) so re-uploads
    // are no-ops anyway.
    upload_files(&host, &resolved_files, &service.name).await?;

    let replicas_count = service.run.replicas;
    let mut replicas: Vec<ReplicaPrep> = Vec::with_capacity(replicas_count as usize);
    for index in 0..replicas_count {
        let name = container_name(&service.name, &spec_hash, index, replicas_count);
        let already = existing.iter().any(|c| {
            c.name == name
                && c.is_running()
                && c.yoink_spec_hash.as_deref() == Some(spec_hash.as_str())
        });
        if already {
            on_event(DeployEvent::AlreadyAtSpec {
                host: host.address.clone(),
                container: name.clone(),
            });
            replicas.push(ReplicaPrep {
                name,
                already_running: true,
            });
            continue;
        }
        force_remove_name(ops, &host, &name).await;
        create_pending_container(
            ops,
            &host,
            config,
            service,
            tag,
            secrets,
            &name,
            &spec_hash,
            &file_binds,
        )
        .await?;
        replicas.push(ReplicaPrep {
            name,
            already_running: false,
        });
    }

    Ok(HostPrep {
        host,
        replicas,
        existing,
        stop_first: !service.run.publish.is_empty(),
    })
}

async fn finalize_one_host(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    prep: HostPrep,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<HostDeployResult, DeployError> {
    let HostPrep {
        host,
        replicas,
        existing,
        stop_first,
    } = prep;

    let expected_names: std::collections::BTreeSet<String> =
        replicas.iter().map(|r| r.name.clone()).collect();

    // If every replica is already at spec, nothing to do.
    if replicas.iter().all(|r| r.already_running) {
        let primary = replicas.first().map(|r| r.name.clone()).unwrap_or_default();
        return Ok(HostDeployResult {
            host: host.address,
            container: primary,
            healthcheck_attempts: 0,
            stopped_old: vec![],
        });
    }

    // For port-publishing services, host port bindings are exclusive:
    // the old container has to stop before `docker start` on the new
    // one can bind. Replica count is forced to 1 in this branch (config
    // validation rejects publish + replicas > 1), so the swap is
    // straightforward.
    let mut stopped_old: Vec<String> = Vec::new();
    if stop_first {
        stopped_old = swap_out_old_containers_by_set(
            ops,
            &host,
            service.run.drain_timeout,
            &existing,
            &expected_names,
            on_event,
        )
        .await?;
    }

    let mut total_attempts: u32 = 0;
    for replica in &replicas {
        if replica.already_running {
            continue;
        }
        ops.start_container(&host, &replica.name)
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
        // Multi-network attach: docker only honors the first
        // endpoint at create time. Connect each additional network
        // explicitly post-start, with the same alias set so the
        // container resolves by name on every network.
        let effective = service.effective_networks(&config.deploy);
        let mut aliases = vec![replica.name.clone()];
        aliases.extend(service.run.options.network_aliases.iter().cloned());
        for net in effective.iter().skip(1) {
            ops.connect_container_network(&host, &replica.name, net, &aliases)
                .await
                .map_err(|source| DeployError::Docker {
                    host: host.address.clone(),
                    source,
                })?;
        }
        on_event(DeployEvent::ContainerStarted {
            host: host.address.clone(),
            container: replica.name.clone(),
        });
        let attempts =
            wait_until_healthy(ops, &host, config, service, &replica.name, on_event).await?;
        total_attempts = total_attempts.max(attempts);
    }

    if !stop_first {
        stopped_old = swap_out_old_containers_by_set(
            ops,
            &host,
            service.run.drain_timeout,
            &existing,
            &expected_names,
            on_event,
        )
        .await?;
    }

    let primary = replicas.first().map(|r| r.name.clone()).unwrap_or_default();
    on_event(DeployEvent::Done {
        host: host.address.clone(),
        container: primary.clone(),
    });

    Ok(HostDeployResult {
        host: host.address,
        container: primary,
        healthcheck_attempts: total_attempts,
        stopped_old,
    })
}

/// Build a `RunSpec` for the given service+tag. Used both to compute the
/// spec hash (with a placeholder name + empty hash) and to actually
/// create the container (with the derived name + hash baked into the
/// labels). `extra_binds` is appended to `service.run.binds` — used to
/// inject the host-side paths of resolved `files:` entries.
/// Build the `RunSpec` that `yoink up` *would* produce for this
/// service+tag, suitable for hashing via `docker::compute_spec_hash`
/// and comparing against a running container's `yoink.spec_hash`.
/// Resolves `service.run.files` so the spec includes the same
/// content-hashed bind strings deploy-time uses. The TUI's
/// dashboard drift column calls this.
#[allow(clippy::result_large_err)]
pub fn build_desired_spec(
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<RunSpec, DeployError> {
    let resolved_files = resolve_files(config, service)?;
    let file_binds: Vec<String> = resolved_files
        .iter()
        .map(|(m, hash)| m.as_bind_string(hash))
        .collect();
    // container_name and spec_hash don't affect compute_spec_hash —
    // see docker.rs::compute_spec_hash_ignores_container_name.
    Ok(build_run_spec(
        config,
        service,
        tag,
        secrets,
        String::new(),
        "",
        &file_binds,
    ))
}

fn build_run_spec(
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    container_name: String,
    spec_hash: &str,
    extra_binds: &[String],
) -> RunSpec {
    let mut binds = service.run.binds.clone();
    binds.extend(extra_binds.iter().cloned());
    RunSpec {
        image: service.image.clone(),
        tag: tag.to_string(),
        networks: service.effective_networks(&config.deploy),
        container_name,
        labels: build_labels(service, tag, spec_hash),
        env: build_env(service, secrets),
        options: service.run.options.clone(),
        entrypoint: service.run.entrypoint.clone(),
        command: service.run.cmd.clone(),
        publish: service.run.publish.clone(),
        binds,
        volumes: service.run.volumes.clone(),
    }
}

/// Parse + content-hash every entry in `service.run.files`. Failure to
/// read a local file aborts the whole deploy — there's no point
/// uploading a partial set of mounts.
#[allow(clippy::result_large_err)]
pub fn resolve_files(
    config: &Config,
    service: &ServiceConfig,
) -> Result<Vec<(FileMount, String)>, DeployError> {
    let base = config.config_dir.as_deref();
    let mut out = Vec::with_capacity(service.run.files.len());
    for spec in &service.run.files {
        let mount = FileMount::parse(spec, base).map_err(|source| DeployError::Files {
            service: service.name.clone(),
            source,
        })?;
        let hash = mount.content_hash().map_err(|source| DeployError::Files {
            service: service.name.clone(),
            source,
        })?;
        out.push((mount, hash));
    }
    Ok(out)
}

async fn upload_files(
    host: &Host,
    files: &[(FileMount, String)],
    service_name: &str,
) -> Result<(), DeployError> {
    for (mount, hash) in files {
        files::upload(host, mount, hash)
            .await
            .map_err(|source| DeployError::Files {
                service: service_name.to_string(),
                source,
            })?;
    }
    Ok(())
}

/// Resolve `[registry]`-configured username/password secret keys
/// against the operator's `SecretsBundle` and return them as bollard
/// `DockerCredentials`. Returns `None` when no registry block is set
/// (typical for public-image-only deploys).
#[must_use]
pub fn registry_credentials(
    config: &Config,
    secrets: Option<&SecretsBundle>,
) -> Option<bollard::auth::DockerCredentials> {
    let reg = config.registry.as_ref()?;
    let bundle = secrets?;
    let username = bundle.get(&reg.username_secret)?.to_string();
    let password = bundle.get(&reg.password_secret)?.to_string();
    Some(bollard::auth::DockerCredentials {
        username: Some(username),
        password: Some(password),
        serveraddress: Some(reg.server.clone()),
        ..Default::default()
    })
}

async fn pull_image(
    ops: &dyn DockerOps,
    host: &Host,
    image: &str,
    tag: &str,
    credentials: Option<bollard::auth::DockerCredentials>,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<(), DeployError> {
    on_event(DeployEvent::PullStarted {
        host: host.address.clone(),
        image: image.to_string(),
        tag: tag.to_string(),
    });
    ops.pull_image(host, image, tag, credentials)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    on_event(DeployEvent::PullFinished {
        host: host.address.clone(),
    });
    Ok(())
}

async fn list_existing_containers(
    ops: &dyn DockerOps,
    host: &Host,
    service: &str,
) -> Result<Vec<ContainerInfo>, DeployError> {
    let label = format!("yoink.service={service}");
    ops.list_containers_by_label(host, &label)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })
}

async fn force_remove_name(ops: &dyn DockerOps, host: &Host, name: &str) {
    if let Err(e) = ops.force_remove_container(host, name).await {
        warn!(host = %host.address, container = %name, error = %e, "force_remove_container failed; continuing");
    }
}

/// Create the new container in `created` state but do not start it.
/// `finalize_one_host` later issues the start, after pre-deploy hooks
/// (e.g. schema migrations) have run — minimizing the window between
/// migration completion and the runtime container actually serving.
#[allow(clippy::too_many_arguments)]
async fn create_pending_container(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    new_name: &str,
    spec_hash: &str,
    extra_binds: &[String],
) -> Result<(), DeployError> {
    let spec = build_run_spec(
        config,
        service,
        tag,
        secrets,
        new_name.to_string(),
        spec_hash,
        extra_binds,
    );
    let body = docker::build_container(&spec)?;
    ops.create_container(host, new_name, body)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    Ok(())
}

async fn wait_until_healthy(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    service: &ServiceConfig,
    new_name: &str,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<u32, DeployError> {
    let Some(port) = service.run.port else {
        // No port at all → no probe possible.
        on_event(DeployEvent::HealthcheckSkipped {
            host: host.address.clone(),
            container: new_name.to_string(),
        });
        return Ok(0);
    };
    // Path present → HTTP probe. Path absent → TCP-connect probe (for
    // non-HTTP services like redis or TLS-only edges like caddy).
    // The probe must share at least one network with the target;
    // pick the service's first effective network.
    let effective = service.effective_networks(&config.deploy);
    let probe_network = effective.first().map_or("bridge", String::as_str);
    let result = match service.run.healthcheck_path.as_deref() {
        Some(path) => {
            healthcheck::poll(
                ops,
                host,
                probe_network,
                new_name,
                port,
                path,
                service.run.healthcheck_timeout,
            )
            .await
        }
        None => {
            healthcheck::poll_tcp(
                ops,
                host,
                probe_network,
                new_name,
                port,
                service.run.healthcheck_timeout,
            )
            .await
        }
    };
    match result {
        Ok(attempts) => {
            on_event(DeployEvent::HealthcheckHealthy {
                host: host.address.clone(),
                container: new_name.to_string(),
                attempts,
            });
            Ok(attempts)
        }
        Err(source) => {
            // Stop the broken container so it can't compete for caddy routing.
            let _ = ops
                .stop_container(host, new_name, Duration::from_secs(5))
                .await;
            // Pull the tail of its logs so the operator gets the
            // "why did it die?" answer in the same pane the
            // reconcile is running in. Best-effort: a fetch error
            // here just means no log preview, not deploy retry.
            if let Ok(lines) = ops.fetch_recent_logs(host, new_name, 30).await {
                on_event(DeployEvent::ContainerLogTail {
                    host: host.address.clone(),
                    container: new_name.to_string(),
                    lines,
                });
            }
            Err(DeployError::Healthcheck {
                host: host.address.clone(),
                container: new_name.to_string(),
                source,
            })
        }
    }
}

/// Stop every running container labeled with this service whose name
/// is *not* in `keep`. Used after the new replicas are healthy to
/// reap whatever's left from a previous `spec_hash`. Set-shape (rather
/// than single-name) so it handles multi-replica services correctly.
async fn swap_out_old_containers_by_set(
    ops: &dyn DockerOps,
    host: &Host,
    drain: Duration,
    existing: &[ContainerInfo],
    keep: &std::collections::BTreeSet<String>,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<Vec<String>, DeployError> {
    let mut stopped = Vec::new();
    for old in existing
        .iter()
        .filter(|c| !keep.contains(&c.name) && c.is_running())
    {
        if let Err(e) = ops.stop_container(host, &old.name, drain).await {
            warn!(host = %host.address, old = %old.name, error = %e, "failed to stop old container");
            continue;
        }
        on_event(DeployEvent::OldContainerStopped {
            host: host.address.clone(),
            container: old.name.clone(),
        });
        stopped.push(old.name.clone());
    }
    Ok(stopped)
}

fn resolve_hook_tag(
    hook: &HookSpec,
    overrides: &BTreeMap<String, String>,
    services: &[ServiceConfig],
) -> String {
    match &hook.tag {
        HookTag::Literal(t) => t.clone(),
        HookTag::Ref { service: name } => overrides.get(name).cloned().unwrap_or_else(|| {
            services
                .iter()
                .find(|s| &s.name == name)
                .and_then(|s| s.tag.clone())
                .unwrap_or_else(|| "latest".to_string())
        }),
    }
}

/// Run a one-shot hook container on the first applicable host. Pulls
/// the image, runs to completion, surfaces stdout+stderr on failure.
async fn run_hook(
    ops: &dyn DockerOps,
    config: &Config,
    hook: &HookSpec,
    tag: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<(), DeployError> {
    let host_cfg = config.hosts.first().ok_or_else(|| DeployError::Hook {
        name: hook.name.clone(),
        message: "no hosts configured".into(),
    })?;
    let host = Host::from(host_cfg);

    let credentials = registry_credentials(config, secrets);
    ops.pull_image(&host, &hook.image, tag, credentials)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;

    // Hooks share the deploy's first network so they can reach
    // services on it (e.g. a migration hook hitting the database).
    // Operators with stricter isolation needs can put their hooks
    // on a dedicated network and put it first in deploy.networks.
    let hook_network = config
        .deploy
        .networks
        .first()
        .map_or("bridge", String::as_str);
    let body = docker::build_hook_container(
        &hook.image,
        tag,
        hook_network,
        hook.entrypoint.as_deref(),
        &hook.cmd,
        &hook_env(hook, secrets),
    );
    let container_name = format!("yoink-hook-{}-{tag}", hook.name);
    let result = ops
        .run_one_shot(&host, &container_name, body)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    if result.exit_code != 0 {
        return Err(DeployError::Hook {
            name: hook.name.clone(),
            message: format!(
                "exit {} — stdout:\n{}\nstderr:\n{}",
                result.exit_code,
                result.stdout.trim_end(),
                result.stderr.trim_end()
            ),
        });
    }
    Ok(())
}

fn hook_env(hook: &HookSpec, secrets: Option<&SecretsBundle>) -> BTreeMap<String, String> {
    let mut env = hook.env.clone();
    if let Some(bundle) = secrets {
        for key in &hook.secrets {
            if let Some(value) = bundle.get(key) {
                env.insert(key.clone(), value.to_string());
            }
        }
        for (env_name, secret_key) in &hook.env_from_secrets {
            if let Some(value) = bundle.get(secret_key) {
                env.insert(env_name.clone(), value.to_string());
            }
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;

    fn config_one_service() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    env: { LOG_LEVEL: info }
    run:
      port: 3000
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
      options: { memory: 512m }
"#,
        )
        .unwrap()
    }

    #[test]
    fn container_name_uses_short_hash_suffix_when_single_replica() {
        let name = container_name("app-a", "0123456789abcdef", 0, 1);
        assert_eq!(name, "app-a-01234567");
    }

    #[test]
    fn container_name_appends_replica_index_when_scaled_out() {
        let r0 = container_name("api", "0123456789abcdef", 0, 3);
        let r1 = container_name("api", "0123456789abcdef", 1, 3);
        let r2 = container_name("api", "0123456789abcdef", 2, 3);
        assert_eq!(r0, "api-01234567-0");
        assert_eq!(r1, "api-01234567-1");
        assert_eq!(r2, "api-01234567-2");
    }

    #[test]
    fn build_labels_includes_yoink_management_and_spec_hash() {
        let cfg = config_one_service();
        let labels = build_labels(&cfg.services[0], "abc1234", "0123456789abcdef");
        assert_eq!(labels.get("yoink.managed"), Some(&"true".into()));
        assert_eq!(labels.get("yoink.service"), Some(&"app-a".into()));
        assert_eq!(labels.get("yoink.version"), Some(&"abc1234".into()));
        assert_eq!(
            labels.get("yoink.spec_hash"),
            Some(&"0123456789abcdef".into())
        );
    }

    #[test]
    fn build_env_includes_run_env_only_when_no_bundle() {
        let cfg = config_one_service();
        let env = build_env(&cfg.services[0], None);
        assert_eq!(env.get("LOG_LEVEL"), Some(&"info".into()));
        assert!(!env.contains_key("INFISICAL_TOKEN"));
    }

    #[test]
    fn build_env_injects_only_requested_secret_keys() {
        let mut cfg = config_one_service();
        cfg.services[0].secrets = vec!["DATABASE_URL".into(), "MISSING".into()];
        let mut values = BTreeMap::new();
        values.insert("DATABASE_URL".into(), "fake-test-fixture".into());
        values.insert("UNRELATED".into(), "ignored".into());
        let bundle = SecretsBundle::new(values);
        let env = build_env(&cfg.services[0], Some(&bundle));
        assert_eq!(
            env.get("DATABASE_URL").map(String::as_str),
            Some("fake-test-fixture")
        );
        assert!(!env.contains_key("UNRELATED"));
        assert!(!env.contains_key("MISSING"));
    }

    fn happy_ops(existing_running: &[(&str, &str)]) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(existing_running
            .iter()
            .map(|(name, version)| ContainerInfo {
                host: "host-a".into(),
                name: (*name).into(),
                image: String::new(),
                state: "running".into(),
                status_text: "Up".into(),
                created_unix: None,
                yoink_service: Some("app-a".into()),
                yoink_version: Some((*version).into()),
                yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
                other_labels: std::collections::BTreeMap::new(),
            })
            .collect()));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        for _ in existing_running {
            ops.push_stop_container(Ok(()));
        }
        ops
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_service_happy_path() {
        let ops = happy_ops(&[("app-a-9f8e7d6", "9f8e7d6")]);
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let report = deploy_service(&ops, &cfg, &cfg.services[0], "a1b2c3d", None, &mut sink)
            .await
            .unwrap();
        assert_eq!(report.service, "app-a");
        assert_eq!(report.tag, "a1b2c3d");
        assert!(
            report.hosts[0].container.starts_with("app-a-"),
            "got {:?}",
            report.hosts[0].container
        );
        assert_eq!(
            report.hosts[0].stopped_old,
            vec!["app-a-9f8e7d6".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_loops_over_services() {
        // Single service still works through the reconcile entry point.
        let ops = happy_ops(&[]);
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let reports = reconcile(&ops, &cfg, &BTreeMap::new(), None, None, &mut sink)
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].tag, "latest");
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_applies_tag_override() {
        let ops = happy_ops(&[]);
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let mut overrides = BTreeMap::new();
        overrides.insert("app-a".into(), "a1b2c3d".into());
        let reports = reconcile(&ops, &cfg, &overrides, None, None, &mut sink)
            .await
            .unwrap();
        assert_eq!(reports[0].tag, "a1b2c3d");
        assert!(reports[0].hosts[0].container.starts_with("app-a-"));
    }

    #[tokio::test(start_paused = true)]
    async fn deploy_is_noop_when_existing_container_matches_spec_hash() {
        let cfg = config_one_service();
        let probe = build_run_spec(
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            None,
            String::new(),
            "",
            &[],
        );
        let hash = docker::compute_spec_hash(&probe);
        let expected_name = container_name("app-a", &hash, 0, 1);

        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![ContainerInfo {
            host: "host-a".into(),
            name: expected_name.clone(),
            image: String::new(),
            state: "running".into(),
            status_text: "Up 5 minutes (healthy)".into(),
            created_unix: None,
            yoink_service: Some("app-a".into()),
            yoink_version: Some("a1b2c3d".into()),
            yoink_spec_hash: Some(hash.clone()),
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: std::collections::BTreeMap::new(),
        }]));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let report = deploy_service(&ops, &cfg, &cfg.services[0], "a1b2c3d", None, &mut sink)
            .await
            .unwrap();
        assert_eq!(report.hosts[0].container, expected_name);
        assert!(report.hosts[0].stopped_old.is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            DeployEvent::AlreadyAtSpec { container, .. } if container == &expected_name
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn pre_deploy_hook_runs_and_emits_events_on_success() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
hooks:
  pre_deploy:
    - name: migrate
      image: registry.example.com/app-a
      tag: { service: app-a }
      cmd: ["bin/migrate"]
"#,
        )
        .unwrap();

        let ops = happy_ops(&[]);
        ops.push_pull_image(Ok(()));
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        }));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let _ = reconcile(&ops, &cfg, &BTreeMap::new(), None, None, &mut sink)
            .await
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            DeployEvent::HookStarted { name } if name == "migrate"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            DeployEvent::HookFinished { name } if name == "migrate"
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn pre_deploy_hook_failure_aborts_reconcile_with_logs() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
hooks:
  pre_deploy:
    - name: migrate
      image: registry.example.com/app-a
      tag: { service: app-a }
      cmd: ["bin/migrate"]
"#,
        )
        .unwrap();

        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: "applying 0001_init...".into(),
            stderr: "ERROR: connection refused".into(),
        }));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let err = reconcile(&ops, &cfg, &BTreeMap::new(), None, None, &mut sink)
            .await
            .unwrap_err();
        let DeployError::Hook { name, message } = err else {
            panic!("expected Hook error, got {err:?}");
        };
        assert_eq!(name, "migrate");
        assert!(message.contains("ERROR: connection refused"));
        assert!(message.contains("applying 0001_init"));
    }

    #[tokio::test(start_paused = true)]
    async fn services_filter_skips_unselected() {
        let ops = FakeDockerOps::new(); // no responses needed; should be no-op
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let reports = reconcile(
            &ops,
            &cfg,
            &BTreeMap::new(),
            Some(&["nonexistent".into()]),
            None,
            &mut sink,
        )
        .await
        .unwrap();
        assert!(reports.is_empty());
    }
}
