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

pub mod files;
pub mod healthcheck;
pub mod lock;
pub mod network;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::warn;
use zeroize::Zeroizing;

use crate::base::bundle::SecretsBundle;
use crate::base::host::Host;
use crate::base::run_args::BuildError;
use crate::config::{Config, HookSpec, HookTag, ServiceConfig};
use crate::docker::{self, RunSpec, SHORT_HASH_LEN};
use crate::docker_ops::{ContainerInfo, DockerError, DockerOps, StoppedRemoval};
use files::{FileError, FileMount};
use healthcheck::HealthcheckError;

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
    #[error("{detail} (host: {host})")]
    Custom { host: String, detail: String },
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
    /// A stop-first deploy failed, and yoink started the old container
    /// that it had stopped for the deploy.
    OldContainerRestarted {
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
    /// The log tail of a failed container could not be fetched. Fired
    /// in place of `ContainerLogTail` so the failure is still recorded
    /// (the audit log builds `DeployFailed` from either event) even
    /// though the container is removed right after.
    ContainerLogTailUnavailable {
        host: String,
        container: String,
        error: String,
    },
    Done {
        host: String,
        container: String,
    },
    /// Before v0.21, services joined `yoink-proxy-admin` by default.
    /// yoink disconnected these yoink app containers from that network
    /// on `host`, because the proxy admin API has no authentication.
    AdminNetworkContainersDisconnected {
        host: String,
        containers: Vec<String>,
    },
    /// yoink created the admin relay on `host`, or replaced a relay
    /// that did not run or had another spec.
    AdminRelayCreated {
        host: String,
    },
    /// The admin relay does not run on `host`, and the run does not
    /// deploy the proxy. Such a run does not pull, and the proxy image
    /// `image` is not on the host, so yoink did not create the relay.
    AdminRelaySkipped {
        host: String,
        image: String,
    },
    /// At the end of the run, the proxy on `hosts` is still from yoink
    /// v0.22 or earlier: it publishes the admin port 2019. Its Docker
    /// HEALTHCHECK asks that port, so Docker shows the proxy as
    /// unhealthy after the move of the admin API to the socket. A
    /// deploy of the proxy recreates it. Sent once per run, also when
    /// the run fails.
    ProxyAdminUpgradePending {
        hosts: Vec<String>,
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
    // Use the OCI standard key so the TUI's single lookup works for
    // both yoink-deployed and 3rd-party OCI containers.
    if let Some(desc) = service
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        labels
            .entry("org.opencontainers.image.description".into())
            .or_insert_with(|| desc.to_string());
    }
    labels.insert("yoink.managed".into(), "true".into());
    labels.insert("yoink.service".into(), service.name.clone());
    labels.insert("yoink.version".into(), tag.into());
    labels.insert("yoink.spec_hash".into(), spec_hash.into());
    // Routing-relevant fields land in labels so changes flow through
    // the existing spec_hash → drift → redeploy → /load pipeline.
    // Without this, editing `caddy_extra_json:` on a stable service
    // wouldn't trigger a redeploy and the new snippet wouldn't reach
    // Caddy until something else changed.
    if let Some(domain) = &service.domain {
        labels.insert("yoink.caddy.domain".into(), domain.as_list().join(","));
    }
    if matches!(service.tls, crate::config::TlsMode::Off) {
        labels.insert("yoink.caddy.tls".into(), "off".into());
    } else if matches!(service.tls, crate::config::TlsMode::Cert) {
        labels.insert("yoink.caddy.tls".into(), "cert".into());
    }
    if let Some(s) = &service.tls_cert_secret {
        labels.insert("yoink.caddy.cert_secret".into(), s.clone());
    }
    if let Some(s) = &service.tls_key_secret {
        labels.insert("yoink.caddy.key_secret".into(), s.clone());
    }
    if let Some(extra) = &service.caddy_extra_json {
        labels.insert(
            "yoink.caddy.extra_hash".into(),
            crate::base::hash::short_sha256(extra),
        );
    }
    labels
}

/// Audit-trail labels added at create-container time. Deliberately
/// kept OUT of `build_labels` (and therefore out of `compute_spec_hash`)
/// so the deploy timestamp doesn't poison drift detection on every
/// reconcile. Read by `yoink history` and the TUI history pane.
/// Best-effort: $USER falls back to "?" inside CI runners that don't
/// set it; the timestamp always works.
#[must_use]
pub fn audit_labels() -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
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
#[allow(clippy::too_many_lines)] // single linear reconcile flow; splitting fragments the wave-by-wave data flow
/// Knobs the operator can flip on a single reconcile invocation. New
/// fields here should default to "preserve existing behaviour" so that
/// `reconcile()` (the no-options shim) stays a drop-in.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileOptions {
    /// Bypass every "already at spec, skip" short-circuit, so each
    /// selected service runs the full prepare → finalize loop and
    /// (for routed services) the Caddy admin-API push. Recovery
    /// gesture for cases where proxy-side state has drifted from
    /// container reality (e.g. a stale upstream pool that never got
    /// cleaned up). Surfaces as `--force` on `yoink up`.
    pub force: bool,
    /// `yoink up --no-registry`: the run ships images and pulls none.
    /// The admin relay then fails when the proxy image is missing on a
    /// host, instead of a pull.
    pub no_registry: bool,
}

/// Backwards-compatible shim — call sites that don't need the new
/// options stay unchanged.
pub async fn reconcile(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    on_event: &mut (dyn FnMut(Option<&str>, DeployEvent) + Send),
) -> Result<Vec<ServiceDeployReport>, DeployError> {
    reconcile_with_options(
        ops,
        config,
        tag_overrides,
        services_filter,
        secrets,
        ReconcileOptions::default(),
        on_event,
    )
    .await
}

/// Reconcile `config` on its fleet, with the knobs of `options`. When a
/// proxy of yoink v0.22 or earlier still runs at the end, the run sends
/// one [`DeployEvent::ProxyAdminUpgradePending`] before it returns, also
/// when the run fails.
pub async fn reconcile_with_options(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    options: ReconcileOptions,
    on_event: &mut (dyn FnMut(Option<&str>, DeployEvent) + Send),
) -> Result<Vec<ServiceDeployReport>, DeployError> {
    let result = reconcile_run(
        ops,
        config,
        tag_overrides,
        services_filter,
        secrets,
        options,
        &mut *on_event,
    )
    .await;
    let hosts = legacy_proxy_hosts(ops, config).await;
    if !hosts.is_empty() {
        on_event(None, DeployEvent::ProxyAdminUpgradePending { hosts });
    }
    result
}

/// The proxy hosts of `config` on which a proxy of yoink v0.22 or
/// earlier runs. Best effort: a host that yoink cannot inspect is left
/// out, so that this check does not hide the error of the run.
async fn legacy_proxy_hosts(ops: &dyn DockerOps, config: &Config) -> Vec<String> {
    if !config.proxy_enabled() {
        return Vec::new();
    }
    let Some(proxy) = config.service(crate::config::PROXY_SERVICE_NAME) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for host in config.fleet_for(proxy) {
        if matches!(
            crate::proxy::admin::runs_legacy_proxy(&host, ops).await,
            Ok(true)
        ) {
            hosts.push(host.address);
        }
    }
    hosts
}

#[allow(clippy::too_many_lines)] // single linear reconcile flow; same rationale as `reconcile`
async fn reconcile_run(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    options: ReconcileOptions,
    on_event: &mut (dyn FnMut(Option<&str>, DeployEvent) + Send),
) -> Result<Vec<ServiceDeployReport>, DeployError> {
    // Ensure every declared network exists on every host, once. Top-
    // level event: no service context.
    {
        let mut none_sink = |e: DeployEvent| on_event(None, e);
        for host in config.fleet() {
            ensure_host_networks(ops, &host, config.networks(), &mut none_sink).await?;
        }
    }
    {
        let mut none_sink = |e: DeployEvent| on_event(None, e);
        ensure_admin_relays(
            ops,
            config,
            tag_overrides,
            services_filter,
            secrets,
            options.no_registry,
            &mut none_sink,
        )
        .await?;
    }

    // Run pre-deploy hooks first; one failure halts the whole reconcile.
    for hook in &config.hooks.pre_deploy {
        let tag = resolve_hook_tag(hook, tag_overrides, &config.services);
        on_event(
            None,
            DeployEvent::HookStarted {
                name: hook.name.clone(),
            },
        );
        run_hook(ops, config, None, hook, &tag, secrets).await?;
        on_event(
            None,
            DeployEvent::HookFinished {
                name: hook.name.clone(),
            },
        );
    }

    // Pre-flight snapshot: read every host's yoink-managed containers
    // once. Lets the per-service path short-circuit fully-converged
    // services without doing the file resolve / upload / create-and-
    // discover-it's-a-noop dance.
    let mut snapshot: std::collections::HashMap<String, Vec<ContainerInfo>> =
        std::collections::HashMap::new();
    for host in config.fleet() {
        let containers = ops
            .list_containers_by_label(&host, "yoink.managed=true")
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
        snapshot.insert(host.address.clone(), containers);
    }

    // Build the wave plan. config.services is already in topo order
    // from Config::topo_sort_services; within a wave we run services
    // concurrently. depends_on edges within `services_filter` define
    // the constraint; deps that fall outside the filter are treated
    // as "already satisfied" (operator opted to skip them).
    let dep_set: BTreeMap<String, std::collections::HashSet<String>> = config
        .services
        .iter()
        .map(|s| (s.name.clone(), s.depends_on.iter().cloned().collect()))
        .collect();
    let selected_names: std::collections::HashSet<String> = match services_filter {
        Some(filter) => filter.iter().cloned().collect(),
        None => config.service_names().map(str::to_string).collect(),
    };
    let mut remaining: Vec<&ServiceConfig> = config
        .services
        .iter()
        .filter(|s| selected_names.contains(&s.name))
        .collect();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut reports: Vec<ServiceDeployReport> = Vec::with_capacity(remaining.len());

    // Wrap the user-supplied sink so wave-parallel futures can each
    // emit through it. Lock duration is one event push (microseconds);
    // contention is irrelevant at our scale (≤ tens of services).
    let sink = std::sync::Mutex::new(on_event);

    let snapshot_ref = &snapshot;
    let sink_ref = &sink;
    let ops_ref = ops;
    let config_ref = config;
    let secrets_ref = secrets;
    let overrides_ref = tag_overrides;

    while !remaining.is_empty() {
        // A service is wave-ready when every depends_on entry that's
        // also in the selected set is already in `done`.
        let (wave, rest): (Vec<&ServiceConfig>, Vec<&ServiceConfig>) =
            remaining.into_iter().partition(|s| {
                dep_set.get(&s.name).is_none_or(|deps| {
                    deps.iter()
                        .all(|d| !selected_names.contains(d) || done.contains(d))
                })
            });
        if wave.is_empty() {
            // Should be impossible — Config::topo_sort_services rejected
            // cycles at load time. Belt-and-suspenders.
            return Err(DeployError::Hook {
                name: "internal".into(),
                message: "no service in remaining wave is ready; dependency graph is stuck".into(),
            });
        }
        remaining = rest;

        let wave_results: Vec<Result<ServiceDeployReport, DeployError>> =
            futures_util::future::join_all(wave.iter().map(|service| {
                let svc_name = service.name.clone();
                async move {
                    let tag = match overrides_ref.get(&service.name).cloned() {
                        Some(t) => t,
                        None => service.tag.clone().ok_or_else(|| DeployError::TagMissing {
                            service: service.name.clone(),
                        })?,
                    };

                    // Pre-flight skip: every applicable host has the
                    // expected replicas at desired_hash → emit
                    // AlreadyAtSpec events + synthesize a report.
                    // Skipped under `--force` so the operator can
                    // explicitly re-run the deploy to reset proxy /
                    // admin state even when nothing has drifted.
                    let desired = build_desired_spec(config_ref, service, &tag, secrets_ref)?;
                    let desired_hash = docker::compute_spec_hash(&desired);
                    let status = if options.force {
                        SpecStatus::NotAtSpec
                    } else {
                        service_already_at_spec(snapshot_ref, config_ref, service, &desired_hash)
                    };
                    drop(desired);

                    if let SpecStatus::AtSpec(hosts) = &status {
                        let mut g = sink_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let mut host_results = Vec::with_capacity(hosts.len());
                        for h in hosts {
                            for name in &h.expected {
                                g(
                                    Some(&svc_name),
                                    DeployEvent::AlreadyAtSpec {
                                        host: h.host.address.clone(),
                                        container: name.clone(),
                                    },
                                );
                            }
                            host_results.push(HostDeployResult {
                                host: h.host.address.clone(),
                                container: h.expected.first().cloned().unwrap_or_default(),
                                healthcheck_attempts: 0,
                                stopped_old: vec![],
                            });
                        }
                        return Ok(ServiceDeployReport {
                            service: service.name.clone(),
                            tag,
                            hosts: host_results,
                        });
                    }

                    // Lock-per-event sink that injects the service tag.
                    let mut local_sink = |e: DeployEvent| {
                        let mut g = sink_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        g(Some(&svc_name), e);
                    };

                    // The expected replicas run at spec, but an old
                    // generation or a surplus replica is still up.
                    // Stop it without a deploy: no image pull and no
                    // `pre_deploy` hooks.
                    if let SpecStatus::AtSpecWithExtras(hosts) = status {
                        return cleanup_extra_containers(
                            ops_ref,
                            config_ref,
                            service,
                            &tag,
                            secrets_ref,
                            hosts,
                            &mut local_sink,
                        )
                        .await;
                    }

                    deploy_service(
                        ops_ref,
                        config_ref,
                        service,
                        &tag,
                        overrides_ref,
                        secrets_ref,
                        options.force,
                        &mut local_sink,
                    )
                    .await
                }
            }))
            .await;

        for (svc, result) in wave.iter().zip(wave_results) {
            let report = result?;
            done.insert(svc.name.clone());
            reports.push(report);
        }
    }

    // Final unconditional Caddy push: recovers from a prior run that
    // replaced containers and then died before the in-flight push
    // (#88), since a follow-up reconcile finds every service already
    // at spec and otherwise never calls `push_caddy_config`.
    // `services_filter` is ignored on purpose — a scoped retry like
    // `yoink up --service web` should still resync Caddy.
    if config.proxy_enabled()
        && let Some(proxy_svc) = config
            .services
            .iter()
            .find(|s| matches!(s.kind, Some(crate::config::ServiceKind::Proxy)))
    {
        for host in config.fleet_for(proxy_svc) {
            push_caddy_config(ops, &host, config, secrets, None).await?;
        }
    }

    Ok(reports)
}

/// Make sure the admin relay runs on every host of the proxy, before
/// any Caddy push of the run. See [`crate::proxy::relay`].
///
/// The relay runs the image of the running proxy. This function runs
/// before the proxy deploy, and a new configured image can fail its
/// healthcheck there. A run that deploys the proxy (see
/// [`relay::run_selects_proxy`]) replaces a relay at another spec than
/// the running proxy image. When no proxy runs, it uses the configured
/// image and makes it present first, see [`ensure_proxy_image`].
///
/// Any other run keeps a running relay, at any spec, and does not pull.
/// It creates a missing or stopped relay with the image of the running
/// proxy, or with the configured image when no proxy runs. When that
/// image is not on the host, it sends [`DeployEvent::AdminRelaySkipped`]
/// and continues. A new proxy image thus does not slow down or stop the
/// deploy of a different service.
async fn ensure_admin_relays(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    no_registry: bool,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<(), DeployError> {
    use crate::proxy::relay::{self, RelayImage, RelayPlan};
    let Some(configured) = RelayImage::of(config, tag_overrides) else {
        return Ok(());
    };
    let Some(proxy) = config.service(crate::config::PROXY_SERVICE_NAME) else {
        return Ok(());
    };
    let selects_proxy = relay::run_selects_proxy(services_filter);
    for host in config.fleet_for(proxy) {
        let docker_err = |source| DeployError::Docker {
            host: host.address.clone(),
            source,
        };
        // The relay follows the image of the proxy that runs now, not the
        // configured one. This function runs before the proxy deploy, so a
        // configured image has not passed a healthcheck yet. If it cannot
        // run Caddy, the failed proxy deploy starts the old proxy again, and
        // a relay on that image would make every later push fail. A new
        // image reaches the relay on the first run after the proxy runs it.
        //
        // A run that does not deploy the proxy keeps a running relay at
        // any spec, so it looks up the running proxy only to create one.
        let running = if selects_proxy {
            relay::running_proxy_image(ops, &host)
                .await
                .map_err(docker_err)?
        } else {
            None
        };
        let desired = running.clone().unwrap_or_else(|| configured.clone());
        let RelayPlan::Create { stale } =
            relay::plan(ops, &host, selects_proxy.then_some(&desired))
                .await
                .map_err(docker_err)?
        else {
            continue;
        };
        let image = if selects_proxy {
            let present = match &running {
                Some(r) => ops
                    .image_present(&host, &r.image, &r.tag)
                    .await
                    .map_err(docker_err)?,
                None => false,
            };
            if present {
                desired
            } else {
                // No proxy runs, or its image has no tag on the host
                // any more: use the configured image.
                ensure_proxy_image(
                    ops,
                    config,
                    secrets,
                    &host,
                    &configured,
                    no_registry,
                    on_event,
                )
                .await?;
                configured.clone()
            }
        } else {
            let image = relay::running_proxy_image(ops, &host)
                .await
                .map_err(docker_err)?
                .unwrap_or_else(|| configured.clone());
            if !ops
                .image_present(&host, &image.image, &image.tag)
                .await
                .map_err(docker_err)?
            {
                on_event(DeployEvent::AdminRelaySkipped {
                    host: host.address.clone(),
                    image: image.reference(),
                });
                continue;
            }
            image
        };
        relay::create(ops, &host, &image, &stale)
            .await
            .map_err(docker_err)?;
        on_event(DeployEvent::AdminRelayCreated {
            host: host.address.clone(),
        });
    }
    Ok(())
}

/// Make the proxy image present on `host`, by the path that the proxy
/// deploy uses: an image that is present stays as it is, an xcaddy
/// image is built on the host, any other image is pulled. In an
/// unscoped run the prefetch (or, with `--no-registry`, the image
/// transfer) did this already. With `--no-registry`, yoink does not
/// pull: a missing image is an error.
async fn ensure_proxy_image(
    ops: &dyn DockerOps,
    config: &Config,
    secrets: Option<&SecretsBundle>,
    host: &Host,
    image: &crate::proxy::relay::RelayImage,
    no_registry: bool,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<(), DeployError> {
    let docker_err = |source| DeployError::Docker {
        host: host.address.clone(),
        source,
    };
    if ops
        .image_present(host, &image.image, &image.tag)
        .await
        .map_err(docker_err)?
    {
        return Ok(());
    }
    if no_registry {
        return Err(DeployError::Custom {
            host: host.address.clone(),
            detail: format!(
                "the admin relay runs the proxy image {}, and the image is not on the host. \
                 With --no-registry, yoink does not pull it. It sends the proxy image only \
                 when the run deploys the proxy: run `yoink up --no-registry` without \
                 --service, or with --service {}.",
                image.reference(),
                crate::config::PROXY_SERVICE_NAME,
            ),
        });
    }
    if let Some(xcaddy) = config.xcaddy() {
        crate::proxy::xcaddy::ensure_xcaddy_image(ops, host, xcaddy, false)
            .await
            .map_err(docker_err)?;
        return Ok(());
    }
    let credentials = registry_credentials(config, secrets, &image.image);
    pull_image(ops, host, &image.image, &image.tag, credentials, on_event).await
}

/// What one host runs for a service, measured against the desired
/// spec. Shared by the reconcile skip decision, the cleanup-only
/// route and the dry-run diff, so the three cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostSpecState {
    /// Replica names the desired spec calls for, in index order.
    pub expected: Vec<String>,
    /// The subset of `expected` that is running with the desired
    /// `yoink.spec_hash`.
    pub present: Vec<String>,
    /// Running containers labelled with the service that are not in
    /// `expected`: an old generation left by a deploy that died
    /// before the swap, or a surplus index after scaling `replicas`
    /// down. Stopped containers do not count.
    pub extras: Vec<String>,
}

impl HostSpecState {
    /// Every expected replica runs at the desired spec.
    pub(crate) fn all_present(&self) -> bool {
        self.present.len() == self.expected.len()
    }
}

/// Classify `containers` (one host's yoink-managed containers) for
/// `service` at `desired_hash`.
pub(crate) fn host_spec_state(
    containers: &[ContainerInfo],
    service: &ServiceConfig,
    desired_hash: &str,
) -> HostSpecState {
    let replicas = service.run.replicas;
    let expected: Vec<String> = (0..replicas)
        .map(|index| container_name(&service.name, desired_hash, index, replicas))
        .collect();
    let present = expected
        .iter()
        .filter(|name| {
            containers.iter().any(|c| {
                c.name == **name
                    && c.is_running()
                    && c.yoink_spec_hash.as_deref() == Some(desired_hash)
            })
        })
        .cloned()
        .collect();
    let extras = containers
        .iter()
        .filter(|c| {
            c.yoink_service.as_deref() == Some(service.name.as_str())
                && c.is_running()
                && !expected.contains(&c.name)
        })
        .map(|c| c.name.clone())
        .collect();
    HostSpecState {
        expected,
        present,
        extras,
    }
}

/// One host of a service whose expected replicas all run at spec.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostAtSpec {
    host: Host,
    /// Expected replica names, in index order.
    expected: Vec<String>,
    /// Running containers of the service to stop. See
    /// [`HostSpecState::extras`].
    extras: Vec<String>,
}

/// Result of the pre-flight check in [`service_already_at_spec`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpecStatus {
    /// At least one host has a missing or mismatched replica (or was
    /// not in the snapshot): the service needs a deploy.
    NotAtSpec,
    /// Every applicable host runs exactly the expected replicas.
    /// Nothing to do.
    AtSpec(Vec<HostAtSpec>),
    /// Every applicable host runs the expected replicas, and at least
    /// one host also runs extras. The service needs a cleanup, not a
    /// deploy.
    AtSpecWithExtras(Vec<HostAtSpec>),
}

/// Pre-flight equality check against the reconcile snapshot.
fn service_already_at_spec(
    snapshot: &std::collections::HashMap<String, Vec<ContainerInfo>>,
    config: &Config,
    service: &ServiceConfig,
    desired_hash: &str,
) -> SpecStatus {
    let mut hosts = Vec::new();
    for host in config.fleet_for(service) {
        let Some(containers) = snapshot.get(&host.address) else {
            return SpecStatus::NotAtSpec;
        };
        let state = host_spec_state(containers, service, desired_hash);
        if !state.all_present() {
            return SpecStatus::NotAtSpec;
        }
        hosts.push(HostAtSpec {
            host,
            expected: state.expected,
            extras: state.extras,
        });
    }
    if hosts.iter().any(|h| !h.extras.is_empty()) {
        SpecStatus::AtSpecWithExtras(hosts)
    } else {
        SpecStatus::AtSpec(hosts)
    }
}

/// Cleanup-only route for a service that is at spec but has extra
/// containers running. Per host: route Caddy to the expected replicas,
/// then stop the extras. No image pull, no `pre_deploy` hooks, no
/// container is created or started.
///
/// The Caddy push comes first for the same reason as in
/// `finalize_one_host`: the extras may be what Caddy routes to today
/// (the earlier deploy may have died at its own Caddy push). If the
/// push fails, nothing is stopped and the error is returned.
///
/// A stop failure is an error here, unlike in the deploy path. The
/// stop is the whole job of this route, and an extra that stays up
/// would be picked up again by the final Caddy push in `reconcile`.
#[allow(clippy::too_many_arguments)] // the per-service context of `deploy_service`, plus the hosts at spec
async fn cleanup_extra_containers(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    hosts: Vec<HostAtSpec>,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<ServiceDeployReport, DeployError> {
    let push_caddy = triggers_caddy_load(config, service);
    let mut host_results = Vec::with_capacity(hosts.len());
    for HostAtSpec {
        host,
        expected,
        extras,
    } in hosts
    {
        let primary = expected.first().cloned().unwrap_or_default();
        // Same event sequence a deploy emits for this host, minus the
        // pull: `Started` ... `Done`. Consumers that track per-service
        // progress (the TUI reconcile table) need the closing `Done`.
        // A host with no extras ends on `AlreadyAtSpec`, as in the
        // plain skip.
        if !extras.is_empty() {
            on_event(DeployEvent::Started {
                service: service.name.clone(),
                tag: tag.to_string(),
                host: host.address.clone(),
            });
        }
        for name in &expected {
            on_event(DeployEvent::AlreadyAtSpec {
                host: host.address.clone(),
                container: name.clone(),
            });
        }
        let mut stopped_old = Vec::with_capacity(extras.len());
        if !extras.is_empty() {
            if push_caddy {
                let keep: std::collections::BTreeSet<String> = expected.iter().cloned().collect();
                push_caddy_config(
                    ops,
                    &host,
                    config,
                    secrets,
                    Some((service.name.as_str(), &keep)),
                )
                .await?;
            }
            for name in extras {
                stop_old_container(ops, &host, service.run.drain_timeout, &name, on_event)
                    .await
                    .map_err(|source| DeployError::Custom {
                        host: host.address.clone(),
                        detail: format!(
                            "service {:?}: failed to stop extra container {name}: {source}",
                            service.name
                        ),
                    })?;
                stopped_old.push(name);
            }
            on_event(DeployEvent::Done {
                host: host.address.clone(),
                container: primary.clone(),
            });
        }
        host_results.push(HostDeployResult {
            host: host.address,
            container: primary,
            healthcheck_attempts: 0,
            stopped_old,
        });
    }
    Ok(ServiceDeployReport {
        service: service.name.clone(),
        tag: tag.to_string(),
        hosts: host_results,
    })
}

/// True when a change to `service`'s containers must be followed by a
/// Caddy config push: the proxy is on, and the service is the proxy
/// itself or is routed by it.
fn triggers_caddy_load(config: &Config, service: &ServiceConfig) -> bool {
    config.proxy_enabled()
        && (matches!(service.kind, Some(crate::config::ServiceKind::Proxy)) || service.is_proxied())
}

/// Reconcile a single service across its applicable hosts.
/// `tag_overrides` is the CLI `--tag` map; service-scoped hooks with
/// `tag: { service: X }` resolve X's tag through it.
#[allow(clippy::too_many_arguments)] // every arg is genuine context for the per-service deploy flow
pub async fn deploy_service(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    tag_overrides: &BTreeMap<String, String>,
    secrets: Option<&SecretsBundle>,
    force: bool,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<ServiceDeployReport, DeployError> {
    let hosts = config.fleet_for(service);
    // A service with `hosts: []` deploys nothing, so it runs no hooks.
    if hosts.is_empty() {
        return Ok(ServiceDeployReport {
            service: service.name.clone(),
            tag: tag.to_string(),
            hosts: Vec::new(),
        });
    }

    // ── Phase 1: PREPARE ──────────────────────────────────────────
    // For every host: ensure network, pull image, upload files,
    // create (but do not start) the new container. After this phase
    // every host has a fully-configured container that's just one
    // `docker start` away from running.
    let mut prepared: Vec<HostPrep> = Vec::with_capacity(hosts.len());
    for host in hosts {
        match prepare_one_host(ops, config, service, tag, secrets, host, force, on_event).await {
            Ok(prep) => prepared.push(prep),
            Err(e) => {
                // Do not leave the replicas created on earlier hosts
                // behind in `created` state.
                cleanup_prepared_replicas(ops, &prepared).await;
                return Err(e);
            }
        }
    }

    // ── Phase 2: HOOKS ────────────────────────────────────────────
    // Service-scoped pre-deploy hooks fire here. Image is already
    // pulled and the runtime container is already created — the only
    // step left after hooks return is `docker start`, which keeps the
    // gap between schema migration and runtime container start as
    // close to zero as possible. `--service` filtering skips the
    // service and its hooks together (`yoink up --service api` will
    // not run web's migrations). On hook failure we clean up the
    // pre-created replicas so a failed deploy doesn't leave
    // `created`-state containers behind. A run that dies here (Ctrl-C,
    // a lost SSH link) cannot clean up: `yoink prune` removes them.
    // `tag: { service: X }` mirrors X's deploy-time tag: the `--tag`
    // override for X, else X's configured tag. This service's own
    // deploy tag is authoritative for a self-reference.
    let mut hook_overrides = tag_overrides.clone();
    hook_overrides.insert(service.name.clone(), tag.to_string());
    for hook in &service.pre_deploy {
        let resolved_tag = resolve_hook_tag(hook, &hook_overrides, &config.services);
        on_event(DeployEvent::HookStarted {
            name: hook.name.clone(),
        });
        if let Err(e) = run_hook(ops, config, Some(service), hook, &resolved_tag, secrets).await {
            cleanup_prepared_replicas(ops, &prepared).await;
            return Err(e);
        }
        on_event(DeployEvent::HookFinished {
            name: hook.name.clone(),
        });
    }

    // ── Phase 3: FINALIZE ─────────────────────────────────────────
    // Start each pre-created container, healthcheck, swap out old.
    let mut host_results = Vec::with_capacity(prepared.len());
    let mut pending = prepared.into_iter();
    while let Some(prep) = pending.next() {
        match finalize_one_host(ops, config, service, prep, secrets, on_event).await {
            Ok(result) => host_results.push(result),
            Err(e) => {
                // `finalize_one_host` unwinds its own host. The hosts
                // not finalized yet still hold this attempt's replicas
                // in `created` state; remove them too.
                cleanup_prepared_replicas(ops, pending.as_slice()).await;
                return Err(e);
            }
        }
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

#[allow(clippy::too_many_arguments)] // every arg is genuine context for the per-host prepare flow
async fn prepare_one_host(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
    host: Host,
    force: bool,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<HostPrep, DeployError> {
    on_event(DeployEvent::Started {
        service: service.name.clone(),
        tag: tag.to_string(),
        host: host.address.clone(),
    });

    // Networks are ensured once per host at the start of reconcile
    // (`ensure_host_networks`), not per-service — every service on
    // the same host needs the same set, so 6 services × 6 networks
    // would emit 36 redundant "ready" lines otherwise.

    let credentials = registry_credentials(config, secrets, &service.image);
    pull_image(ops, &host, &service.image, tag, credentials, on_event).await?;
    let existing = list_existing_containers(ops, &host, &service.name).await?;

    // Per-host (not per-replica) prep: file upload, spec hash. The
    // file binds + env + options are identical across replicas, so the
    // spec hash is identical too — only the container name differs.
    let resolved_files = resolve_files(config, service)?;
    let hash_binds: Vec<String> = resolved_files
        .iter()
        .map(|(m, _bytes, hash)| m.as_bind_string(hash))
        .collect();
    let probe_spec = build_run_spec(
        config,
        service,
        tag,
        secrets,
        String::new(),
        "",
        &hash_binds,
    );
    let spec_hash = docker::compute_spec_hash(&probe_spec);

    // Files only need to land on the host once per service, regardless
    // of replica count. Idempotent (content-addressed) so re-uploads
    // are no-ops anyway.
    // The container binds the files where they were staged. That
    // directory is under the home of the SSH user and is known only
    // after the upload, so it is not part of the spec hash above.
    let file_binds = upload_files(ops, &host, &resolved_files, &service.name).await?;

    let replicas_count = service.run.replicas;
    let mut replicas: Vec<ReplicaPrep> = Vec::with_capacity(replicas_count as usize);
    for index in 0..replicas_count {
        let name = container_name(&service.name, &spec_hash, index, replicas_count);
        // `--force` makes every replica look not-already-running, so
        // the prep path below force-removes the existing container
        // and re-creates it. Same name, same spec_hash — the operator
        // is asking for a forced redeploy, typically to push a fresh
        // Caddy admin config or to recover from drifted proxy state.
        let already = !force
            && existing.iter().any(|c| {
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
        if let Err(e) = create_pending_container(
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
        .await
        {
            // Do not leave the replicas created so far behind in
            // `created` state.
            remove_attempt_replicas(ops, &host, &replicas).await;
            return Err(e);
        }
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

/// Connect `name` to every network in `effective` past the first
/// (the first is set at container create time). Same alias set on
/// every network so callers always reach the container by name.
async fn attach_extra_networks(
    ops: &dyn DockerOps,
    host: &Host,
    name: &str,
    effective: &[String],
    aliases: &[String],
) -> Result<(), DeployError> {
    for net in effective.iter().skip(1) {
        let endpoint = docker::endpoint_settings(aliases);
        ops.connect_container_network(host, name, net, endpoint)
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
    }
    Ok(())
}

/// Attach networks to, start and healthcheck every replica this attempt
/// created on `host`, in replica order. Stops at the first failure and
/// cleans nothing up: [`finalize_one_host`] removes the attempt's
/// replicas on `Err`. Returns the highest healthcheck attempt count.
async fn start_new_replicas(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    service: &ServiceConfig,
    replicas: &[ReplicaPrep],
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<u32, DeployError> {
    let mut total_attempts: u32 = 0;
    for replica in replicas {
        if replica.already_running {
            continue;
        }
        // Multi-network attach BEFORE start: docker only honors the first
        // endpoint at create time, so connect the rest while the container
        // is still created-but-not-running. Starting with every network
        // already attached lets a container that eagerly dials its
        // dependencies (DB, redis) on boot actually reach them. Attaching
        // *after* start meant such a container exited before the networks
        // landed, and the post-start connect then 404'd with "network
        // sandbox not found" on the now-dead container — only services that
        // connect lazily survived. The alias set is identical on every
        // network so the container resolves by name everywhere.
        //
        // If any attach fails partway, the caller removes the attempt's
        // replicas so the next reconcile re-creates from scratch —
        // service_already_at_spec only checks name/hash/running, so a
        // half-networked container would otherwise short-circuit and
        // persist forever.
        let effective = config.networks_for(service);
        let mut aliases = vec![replica.name.clone()];
        aliases.extend(service.run.options.network_aliases.iter().cloned());
        attach_extra_networks(ops, host, &replica.name, &effective, &aliases).await?;
        ops.start_container(host, &replica.name)
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
        on_event(DeployEvent::ContainerStarted {
            host: host.address.clone(),
            container: replica.name.clone(),
        });
        let attempts =
            wait_until_healthy(ops, host, config, service, &replica.name, on_event).await?;
        total_attempts = total_attempts.max(attempts);
    }
    Ok(total_attempts)
}

#[allow(clippy::too_many_lines)] // single linear finalize flow; same rationale as `reconcile`
async fn finalize_one_host(
    ops: &dyn DockerOps,
    config: &Config,
    service: &ServiceConfig,
    prep: HostPrep,
    secrets: Option<&SecretsBundle>,
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

    let triggers_caddy_load = triggers_caddy_load(config, service);

    // No fresh container to start, but a prior partial failure may
    // have left an old generation running alongside the new — run
    // orphan cleanup unconditionally so the second reconcile self-heals.
    if replicas.iter().all(|r| r.already_running) {
        // That prior failure may have been the Caddy push itself, in
        // which case Caddy still routes only to the old generation.
        // Route to the expected replicas before stopping anything, as
        // the normal path below does; otherwise the service is down
        // until the final push at the end of `reconcile`, which never
        // runs if a later service fails.
        let has_old = existing
            .iter()
            .any(|c| c.is_running() && !expected_names.contains(&c.name));
        if has_old && triggers_caddy_load {
            push_caddy_config(
                ops,
                &host,
                config,
                secrets,
                Some((service.name.as_str(), &expected_names)),
            )
            .await?;
        }
        let stopped_old = swap_out_old_containers_by_set(
            ops,
            &host,
            service.run.drain_timeout,
            &existing,
            &expected_names,
            on_event,
        )
        .await?;
        let primary = replicas.first().map(|r| r.name.clone()).unwrap_or_default();
        // `prepare_one_host` emitted `Started` for this host. When the
        // cleanup did work, close the sequence so consumers that track
        // per-service progress do not stay on the swap step.
        if !stopped_old.is_empty() {
            on_event(DeployEvent::Done {
                host: host.address.clone(),
                container: primary.clone(),
            });
        }
        return Ok(HostDeployResult {
            host: host.address,
            container: primary,
            healthcheck_attempts: 0,
            stopped_old,
        });
    }

    // For port-publishing services, host port bindings are exclusive:
    // the old container has to stop before `docker start` on the new
    // one can bind. Replica count is forced to 1 in this branch (config
    // validation rejects publish + replicas > 1), so the swap is
    // straightforward.
    let is_proxy = matches!(service.kind, Some(crate::config::ServiceKind::Proxy));
    if is_proxy
        && let Err(e) = move_old_proxy_admin_to_socket(ops, &host, config, secrets, &existing).await
    {
        // Nothing is stopped yet: the old proxy still serves.
        remove_attempt_replicas(ops, &host, &replicas).await;
        return Err(e);
    }

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

    // From here until the Caddy push below, a failure unwinds the whole
    // attempt on this host: every replica this attempt created is
    // removed, the one that failed and the siblings that already
    // passed. A sibling left running is unrouted (the Caddy push only
    // happens once all replicas pass), and after a later successful
    // deploy stops it, it is indistinguishable from a generation that
    // was once live, so `yoink rollback` could pick the failed version.
    // The previous generation in `existing` is never touched here.
    //
    // A stop-first deploy already stopped the previous generation to
    // free its host ports. After the failed replicas are gone, start it
    // again: else the host has no container of the service at all. For
    // the proxy, that is an outage of every route.
    let total_attempts =
        match start_new_replicas(ops, &host, config, service, &replicas, on_event).await {
            Ok(attempts) => attempts,
            Err(e) => {
                remove_attempt_replicas(ops, &host, &replicas).await;
                restart_old_containers(ops, &host, &stopped_old, on_event).await;
                return Err(e);
            }
        };

    // Caddy admin-API push: route the new replicas in (and the
    // proxy itself, after first deploy, into a serving state).
    // Runs between healthcheck-pass and the old-replica stop. We
    // override the in-flight service's upstream names with the
    // expected (post-deploy) replica names — see `push_caddy_config`
    // for why we deliberately do NOT include the about-to-be-stopped
    // old containers in the rendered upstreams.
    if triggers_caddy_load
        && let Err(failure) = try_push_caddy_config(
            ops,
            &host,
            config,
            secrets,
            Some((service.name.as_str(), &expected_names)),
        )
        .await
    {
        // The new replicas never got routed, so they go too. The
        // proxy container itself is the exception: its predecessor
        // is already stopped (it publishes host ports) and Caddy
        // usually runs with `--resume`, so the new container is
        // serving the autosaved config. Removing it would turn a
        // rejected config push into a full outage.
        //
        // A stop-first service already stopped its old containers.
        // Start them again after the removal frees the host ports, as
        // the healthcheck failure path above does. A rolling deploy
        // has an empty `stopped_old` here.
        //
        // The push may have reached Caddy before it failed: a route
        // count mismatch after a 2xx, a failed read-back, a timeout.
        // Then Caddy routes to the replicas that yoink just removed, and
        // answers 502 until the next successful run. Push again, with
        // the upstreams of the containers that run now: the old ones.
        if !is_proxy {
            remove_attempt_replicas(ops, &host, &replicas).await;
            restart_old_containers(ops, &host, &stopped_old, on_event).await;
            if failure.may_have_applied {
                return Err(route_back(ops, &host, config, secrets, failure.error).await);
            }
        }
        return Err(failure.error);
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

/// Before a deploy of the proxy stops a proxy of yoink v0.22 or earlier,
/// push the Caddy config to it. The push moves its admin API to the
/// unix socket, and Caddy saves that in `autosave.json` on the config
/// volume (see `crate::proxy::admin`). The new proxy resumes that file,
/// so its admin API is on the socket from the start, where the deploy
/// probe and the relay reach it. A failed deploy starts the old proxy
/// again, and it resumes the same file.
///
/// When no old proxy runs, yoink cannot push. Then `autosave.json` can
/// still have the old admin address `0.0.0.0:2019`. The start script
/// of the new proxy does not resume such a file: the proxy starts from
/// the bootstrap, and the push after its healthcheck adds the routes.
///
/// The push renders the routes of the containers that run now, as the
/// final push of a run does. A proxy of this version gets no push here.
async fn move_old_proxy_admin_to_socket(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    secrets: Option<&SecretsBundle>,
    existing: &[ContainerInfo],
) -> Result<(), DeployError> {
    for old in existing.iter().filter(|c| c.is_running()) {
        let detail = ops
            .inspect_container(host, &old.name)
            .await
            .map_err(|source| DeployError::Docker {
                host: host.address.clone(),
                source,
            })?;
        if crate::proxy::admin::is_legacy_proxy(&detail) {
            return push_caddy_config(ops, host, config, secrets, None).await;
        }
    }
    Ok(())
}

/// Build the per-service upstream-name map fed into the Caddy config
/// renderer. Pure-ish helper extracted for testability — the live
/// daemon query is the only side effect.
///
/// For the in-flight service (currently being reconciled), the names
/// come straight from `expected_names`. For every other routed
/// service, query `list_containers_by_label` and filter to running.
async fn build_upstream_names(
    ops: &dyn DockerOps,
    host: &Host,
    routed: &[&ServiceConfig],
    in_flight: Option<(&str, &std::collections::BTreeSet<String>)>,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, DeployError> {
    let mut upstream_names: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for svc in routed {
        let names: Vec<String> = if let Some((sname, expected)) = in_flight
            && sname == svc.name.as_str()
        {
            expected.iter().cloned().collect()
        } else {
            let label = format!("yoink.service={}", svc.name);
            let containers =
                ops.list_containers_by_label(host, &label)
                    .await
                    .map_err(|source| DeployError::Docker {
                        host: host.address.clone(),
                        source,
                    })?;
            containers
                .into_iter()
                .filter(crate::docker_ops::ContainerInfo::is_running)
                .map(|c| c.name)
                .collect()
        };
        upstream_names.insert(svc.name.clone(), names);
    }
    Ok(upstream_names)
}

/// Render the Caddy config for `host`'s view of `config` and POST it
/// to the proxy's admin API. Errors abort the deploy — old config
/// stays active because Caddy's `/load` is atomic. A proxy of yoink
/// v0.22 or earlier gets the push on its own admin port first, which
/// moves its admin API to the unix socket, see `crate::proxy::admin`.
///
/// `in_flight_upstreams` carries `(service_name, expected_names)` for
/// the service the caller is currently reconciling. For that service,
/// the rendered upstream pool is forced to those exact container
/// names — the old (about-to-be-stopped) replicas are NOT included.
///
/// **Why we deliberately exclude the old replicas:** the older shape
/// queried `list_containers_by_label + is_running` for every service,
/// which during the brief window between "new replicas healthy" and
/// "old replicas stopped" returned BOTH sets. Caddy then carried the
/// old container names in its upstream pool. After `swap_out_old`
/// removed them, those names became unresolvable in Docker DNS and
/// Caddy's active health checker logged "server misbehaving" forever
/// (because no subsequent `/load` ever cleaned the pool).
///
/// Caddy's `/load` is graceful regardless of pool membership: existing
/// TCP connections to old containers continue serving in-flight work
/// until they close, and new requests route to the new containers.
/// Including the old upstreams in the pool achieved nothing except
/// creating the stale-DNS hole.
async fn push_caddy_config(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    secrets: Option<&SecretsBundle>,
    in_flight_upstreams: Option<(&str, &std::collections::BTreeSet<String>)>,
) -> Result<(), DeployError> {
    try_push_caddy_config(ops, host, config, secrets, in_flight_upstreams)
        .await
        .map_err(|failure| failure.error)
}

/// A failed [`try_push_caddy_config`]. `may_have_applied` is false only
/// when Caddy certainly kept its old config, see
/// [`crate::proxy::admin::AdminError::may_have_applied`]. A failure
/// before the push (expand, upstream lookup, render) sent nothing.
struct CaddyPushFailure {
    error: DeployError,
    may_have_applied: bool,
}

impl From<DeployError> for CaddyPushFailure {
    fn from(error: DeployError) -> Self {
        Self {
            error,
            may_have_applied: false,
        }
    }
}

/// After a push that may have applied, route Caddy to the containers
/// that run now. Returns the error of the failed push `first`, with the
/// error of this push when it fails too.
async fn route_back(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    secrets: Option<&SecretsBundle>,
    first: DeployError,
) -> DeployError {
    match push_caddy_config(ops, host, config, secrets, None).await {
        Ok(()) => first,
        Err(second) => DeployError::Custom {
            host: host.address.clone(),
            detail: format!(
                "{first}. Caddy can still route to the removed containers: the push that \
                 routes it to the running containers failed too: {second}"
            ),
        },
    }
}

/// [`push_caddy_config`], with the information whether a failed push
/// may have changed the config of Caddy.
async fn try_push_caddy_config(
    ops: &dyn DockerOps,
    host: &Host,
    config: &Config,
    secrets: Option<&SecretsBundle>,
    in_flight_upstreams: Option<(&str, &std::collections::BTreeSet<String>)>,
) -> Result<(), CaddyPushFailure> {
    // Expand any `caddy_extra_caddyfile:` snippets to JSON. Mutates a
    // local clone so the caller's view of config stays unchanged.
    let mut config = config.clone();
    crate::proxy::caddy::expand_caddyfile_snippets(&mut config)
        .await
        .map_err(|source| DeployError::Custom {
            host: host.address.clone(),
            detail: format!("expand caddy_extra_caddyfile: {source}"),
        })?;
    let config = &config;

    let routed: Vec<&ServiceConfig> = config.proxied_services();
    let upstream_names = build_upstream_names(ops, host, &routed, in_flight_upstreams).await?;

    let json = crate::proxy::caddy::render(
        config,
        |s| upstream_names.get(s).cloned().unwrap_or_default(),
        secrets,
    )
    .map_err(|source| DeployError::Custom {
        host: host.address.clone(),
        detail: format!("render Caddy config: {source}"),
    })?;
    crate::proxy::admin::push_config(host, ops, &json)
        .await
        .map_err(|source| {
            // When Caddy bounces /load, point the operator at the
            // services that have a `caddy_extra_json:` block —
            // those are the most likely sources of schema errors.
            // Unattributed users would otherwise stare at a Caddy
            // backtrace with no clue which service caused it.
            let extras: Vec<&str> = config
                .services
                .iter()
                .filter(|s| s.caddy_extra_json.is_some())
                .map(|s| s.name.as_str())
                .collect();
            let hint = if extras.is_empty() {
                String::new()
            } else {
                format!(
                    "\nhint: services with caddy_extra_json: {} — \
                     run `yoink proxy-render` to inspect the rendered config",
                    extras.join(", "),
                )
            };
            CaddyPushFailure {
                error: DeployError::Custom {
                    host: host.address.clone(),
                    detail: format!("push Caddy config: {source}{hint}"),
                },
                may_have_applied: source.may_have_applied,
            }
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
#[allow(clippy::result_large_err)] // DeployError carries diagnostic context; boxing each variant adds ceremony without callers benefitting
pub fn build_desired_spec(
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<RunSpec, DeployError> {
    let resolved_files = resolve_files(config, service)?;
    let file_binds: Vec<String> = resolved_files
        .iter()
        .map(|(m, _bytes, hash)| m.as_bind_string(hash))
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
        networks: config.networks_for(service),
        container_name,
        labels: build_labels(service, tag, spec_hash),
        env: build_env(service, secrets),
        options: service.run.options.clone(),
        entrypoint: service.run.entrypoint.clone(),
        command: service.run.cmd.clone(),
        publish: service.run.publish.clone(),
        binds,
        volumes: service.run.volumes.clone(),
        docker_healthcheck: docker_healthcheck_for(service),
    }
}

/// Pick a docker-native HEALTHCHECK directive for a service.
///
/// Today this only lights up for the synthesized `yoink-proxy`:
/// the bundled image is `caddy:2`, which doesn't ship a HEALTHCHECK
/// of its own, and the proxy is the one container yoink puts on
/// every host whether the operator wrote a yaml entry for it or not.
/// Best practice is for it to show `(healthy)` in `docker ps` and
/// in tools like lazydocker / cAdvisor / k8s-style health probes —
/// the deploy-time gate yoink already runs is invisible there.
///
/// User-defined services intentionally inherit whatever HEALTHCHECK
/// their image's Dockerfile declares (or none); yoink doesn't yet
/// expose a config knob to override that.
fn docker_healthcheck_for(service: &ServiceConfig) -> Option<bollard::models::HealthConfig> {
    use bollard::models::HealthConfig;
    if service.kind != Some(crate::config::ServiceKind::Proxy) {
        return None;
    }
    // Caddy's admin API listens on a unix socket, and no tool in the
    // caddy:2 image speaks HTTP over one (BusyBox `wget` cannot). The
    // check thus asks the shell whether the socket exists: Caddy binds
    // it when the admin API starts, and removes a stale one first. The
    // `test` builtin of `sh` needs no other binary. The deploy-time gate
    // still sends `GET /config/` through the admin relay.
    Some(HealthConfig {
        test: Some(vec![
            "CMD-SHELL".into(),
            format!("test -S {}", crate::config::ADMIN_SOCKET),
        ]),
        // 30 s is enough headroom for Caddy's admin loop without
        // making `docker ps` lag the actual state.
        interval: Some(30 * 1_000_000_000),
        timeout: Some(5 * 1_000_000_000),
        // Caddy boots in well under a second; keep the start grace
        // generous so a slow first-load (TLS bootstrap on first run)
        // doesn't get reported as unhealthy.
        start_period: Some(10 * 1_000_000_000),
        retries: Some(3),
        ..Default::default()
    })
}

/// Parse + content-hash every entry in `service.run.files`. Failure to
/// read a local file aborts the whole deploy — there's no point
/// uploading a partial set of mounts.
#[allow(clippy::result_large_err)] // DeployError carries diagnostic context; boxing each variant adds ceremony without callers benefitting
pub fn resolve_files(
    config: &Config,
    service: &ServiceConfig,
) -> Result<Vec<(FileMount, Vec<u8>, String)>, DeployError> {
    let base = config.config_dir.as_deref();
    let mut out = Vec::with_capacity(service.run.files.len());
    for spec in &service.run.files {
        let mount = FileMount::parse(spec, base).map_err(|source| DeployError::Files {
            service: service.name.clone(),
            source,
        })?;
        let (bytes, hash) = mount.read_and_hash().map_err(|source| DeployError::Files {
            service: service.name.clone(),
            source,
        })?;
        out.push((mount, bytes, hash));
    }
    Ok(out)
}

/// Upload every file and return the bind string of each one, with
/// the host path it was staged at.
async fn upload_files(
    ops: &dyn DockerOps,
    host: &Host,
    files: &[(FileMount, Vec<u8>, String)],
    service_name: &str,
) -> Result<Vec<String>, DeployError> {
    let keyfile = ops.ssh_keyfile(host);
    let mut binds = Vec::with_capacity(files.len());
    for (mount, bytes, hash) in files {
        let staging_dir = files::upload(host, keyfile.as_deref(), mount, hash, bytes)
            .await
            .map_err(|source| DeployError::Files {
                service: service_name.to_string(),
                source,
            })?;
        binds.push(mount.bind_string_at(&staging_dir, hash));
    }
    Ok(binds)
}

/// Resolve `[registry]`-configured username/password secret keys
/// against the operator's `SecretsBundle` and return them as bollard
/// `DockerCredentials` for a pull of `image`.
///
/// Uses the first configured registry (`registry:` or `registries:`)
/// whose `server` hosts `image`. Returns `None` when no registry is
/// configured (typical for public-image-only deploys), and when `image`
/// is on a registry that is not configured. That pull is anonymous: the credentials of
/// one registry make another registry answer 401, also for a public
/// image such as the default proxy image `caddy:2`.
#[must_use]
pub fn registry_credentials(
    config: &Config,
    secrets: Option<&SecretsBundle>,
    image: &str,
) -> Option<bollard::auth::DockerCredentials> {
    let reg = config
        .configured_registries()
        .find(|reg| crate::base::image::image_on_registry(image, &reg.server))?;
    let bundle = secrets?;
    let username = bundle.get(&reg.username_secret)?.to_string();
    let password = Zeroizing::new(bundle.get(&reg.password_secret)?.to_string());
    Some(bollard::auth::DockerCredentials {
        username: Some(username),
        password: Some(password.to_string()),
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
    // Cache check: if the image is already in the local daemon's
    // store, skip both the pull and the surrounding event noise.
    // Lets the prefetch pass cover N images once instead of having
    // each per-service reconcile re-poll the registry. A stat-only
    // call to the docker daemon, no network I/O.
    if ops
        .image_present(host, image, tag)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?
    {
        return Ok(());
    }

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

/// Ensure every named network exists on `host`. Idempotent (safe to
/// call repeatedly; existing networks are no-ops). Emits one
/// `NetworkReady` event per network so operators see the
/// already-exists / created status.
pub async fn ensure_host_networks(
    ops: &dyn DockerOps,
    host: &Host,
    networks: &[String],
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<(), DeployError> {
    for net in networks {
        let created = if net == crate::config::ADMIN_NETWORK {
            let ready = network::ensure_admin(ops, host)
                .await
                .map_err(|e| match e {
                    network::AdminNetworkError::Docker(source) => DeployError::Docker {
                        host: host.address.clone(),
                        source,
                    },
                    other => DeployError::Custom {
                        host: host.address.clone(),
                        detail: other.to_string(),
                    },
                })?;
            if !ready.disconnected.is_empty() {
                on_event(DeployEvent::AdminNetworkContainersDisconnected {
                    host: host.address.clone(),
                    containers: ready.disconnected,
                });
            }
            ready.created
        } else {
            network::ensure(ops, host, net)
                .await
                .map_err(|source| DeployError::Docker {
                    host: host.address.clone(),
                    source,
                })?
        };
        on_event(DeployEvent::NetworkReady {
            host: host.address.clone(),
            network: net.clone(),
            created,
        });
    }
    Ok(())
}

/// Pull every (service, host) image in parallel before reconcile starts.
///
/// Worth doing for "deploy everything" gestures (`yoink up` with no
/// `--service`, TUI's reconcile-all): on a single host this turns
/// `sum(pull_time)` into ~`max(pull_time)`, since the docker daemon
/// happily handles many concurrent pulls and registries serve them in
/// parallel. Single-service deploys don't benefit (only one image to
/// pull) so callers shouldn't bother invoking this for them.
///
/// `pull_image` itself is idempotent — the inline pulls during the
/// per-service reconcile become no-ops once this completes, so this
/// is purely additive: safe to skip, safe to run, never wrong.
///
/// Errors here are returned to the caller; the inline per-service
/// `pull_image` would have failed identically anyway.
///
/// `skip_locally_built`: callers should pass `true`. Services with a
/// `build:` block are local-only by definition — `yoink build` tags
/// them in the operator's docker daemon, and `load_images_to_hosts`
/// is responsible for shipping them. Trying to `docker pull` such an
/// image would 404 on every registry. The parameter is preserved
/// rather than hardcoded for the rare caller that has separately
/// pushed their builds and prefers to pull-everything.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub async fn prefetch_images(
    ops: Arc<dyn DockerOps>,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&SecretsBundle>,
    skip_locally_built: bool,
    rebuild_proxy: bool,
    on_event: Arc<dyn Fn(DeployEvent) + Send + Sync>,
) -> Result<(), DeployError> {
    // xcaddy builds run in their own collection so per-host failures
    // can be bundled into a "X/N succeeded; failed on host Y" message
    // — first-error-wins on a fleet hides which hosts have a fresh
    // image and which still need the build to retry. Structurally the
    // build futures still run concurrently with the regular pull
    // fan-out below via `tokio::join!`.
    let mut build_futs: Vec<
        std::pin::Pin<
            Box<dyn std::future::Future<Output = (String, Result<(), DockerError>)> + Send>,
        >,
    > = Vec::new();

    if let Some(xcaddy_cfg) = config.xcaddy()
        && let Some(svc) = config.service(crate::config::PROXY_SERVICE_NAME)
    {
        for host in config.fleet_for(svc) {
            let ops = ops.clone();
            let xcaddy_cfg = xcaddy_cfg.clone();
            build_futs.push(Box::pin(async move {
                let result = crate::proxy::xcaddy::ensure_xcaddy_image(
                    &*ops,
                    &host,
                    &xcaddy_cfg,
                    rebuild_proxy,
                )
                .await
                .map(|_tag| ());
                (host.address.clone(), result)
            }));
        }
    }

    let mut futs: Vec<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DeployError>> + Send>>,
    > = Vec::new();

    for service in &config.services {
        if let Some(filter) = services_filter
            && !filter.iter().any(|n| n == &service.name)
        {
            continue;
        }
        // With `--no-registry` the locally-built image is shipped via
        // unregistry/tarball before this phase, so trying to pull it
        // from a registry would 404 (the image doesn't exist there).
        // Skip — the reconcile step's `image_present` check will pass
        // because the unregistry push already landed it on the host.
        if skip_locally_built && service.build.is_some() {
            continue;
        }
        // Likewise the xcaddy-built proxy image only exists locally on
        // each host; pulling it from a registry would 404. The build
        // fan-out above has already produced the image.
        if service.name == crate::config::PROXY_SERVICE_NAME && config.xcaddy().is_some() {
            continue;
        }
        let tag = match tag_overrides.get(&service.name).cloned() {
            Some(t) => t,
            None => match service.tag.clone() {
                Some(t) => t,
                // Skip silently — reconcile will surface the same
                // error per-service with a useful service name.
                None => continue,
            },
        };
        let credentials = registry_credentials(config, secrets, &service.image);
        for host in config.fleet_for(service) {
            let ops = ops.clone();
            let on_event = on_event.clone();
            let creds = credentials.clone();
            let image = service.image.clone();
            let tag = tag.clone();
            futs.push(Box::pin(async move {
                on_event(DeployEvent::PullStarted {
                    host: host.address.clone(),
                    image: image.clone(),
                    tag: tag.clone(),
                });
                let res = ops
                    .pull_image(&host, &image, &tag, creds)
                    .await
                    .map_err(|source| DeployError::Docker {
                        host: host.address.clone(),
                        source,
                    });
                if res.is_ok() {
                    on_event(DeployEvent::PullFinished {
                        host: host.address.clone(),
                    });
                }
                res
            }));
        }
    }

    // Both fan-outs run concurrently: tokio::join! polls both futures
    // cooperatively, so xcaddy builds on the proxy hosts overlap with
    // image pulls on every host. After both complete, we fail-fast on
    // either kind, but build failures get a structured per-host summary
    // ("2/3 hosts succeeded; failed on h2: <reason>") since first-
    // error-wins hides which hosts already have the new image.
    let (build_outcomes, pull_results) = tokio::join!(
        futures_util::future::join_all(build_futs),
        futures_util::future::join_all(futs),
    );
    if let Some(err) = summarize_build_outcomes(build_outcomes) {
        return Err(err);
    }
    for r in pull_results {
        r?;
    }
    Ok(())
}

/// Reduce the per-host xcaddy build outcomes into a single deploy
/// error when any host failed. The bundled message preserves the
/// signal "h1 succeeded, h2 failed" — first-error-wins would surface
/// just one host's failure with no indication that other hosts already
/// have the fresh image (and don't need a retry on a re-`up`).
/// Returns `None` when every host succeeded.
fn summarize_build_outcomes(
    outcomes: Vec<(String, Result<(), DockerError>)>,
) -> Option<DeployError> {
    let total = outcomes.len();
    let mut succeeded: Vec<String> = Vec::new();
    let mut failed: Vec<(String, DockerError)> = Vec::new();
    for (host, result) in outcomes {
        match result {
            Ok(()) => succeeded.push(host),
            Err(e) => failed.push((host, e)),
        }
    }
    if failed.is_empty() {
        return None;
    }
    // Single-failure-on-single-host case: keep the original error
    // shape so existing one-host smoke tests / error parsers don't
    // see a regression. Otherwise, bundle into a structured summary.
    if total == 1 {
        let (host, source) = failed.into_iter().next().expect("len==1");
        return Some(DeployError::Docker { host, source });
    }
    let detail = failed
        .iter()
        .map(|(h, e)| format!("    {h}: {e}"))
        .collect::<Vec<_>>()
        .join("\n");
    let primary_host = failed[0].0.clone();
    let primary_source = failed.into_iter().next().expect("non-empty").1;
    let summary = format!(
        "xcaddy build: {ok}/{total} hosts succeeded; {failed_n} failed:\n{detail}",
        ok = succeeded.len(),
        failed_n = total - succeeded.len(),
    );
    // Wrap the structured summary as the same `DeployError::Docker`
    // shape with the first failed host's underlying error attached.
    // Rendering nests cleanly: caller's `e` ends up "<host>: <summary>".
    Some(DeployError::Docker {
        host: format!("{primary_host}\n{summary}"),
        source: primary_source,
    })
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

/// Best-effort cleanup of pre-created (pending) containers after a
/// pre-deploy hook fails — fanned out across hosts × replicas so
/// recovery isn't gated on N×M serial round-trips. Idempotent (404 =
/// already gone); errors are logged inside `force_remove_name` and
/// never mask the original hook error.
async fn cleanup_prepared_replicas(ops: &dyn DockerOps, prepared: &[HostPrep]) {
    let futs = prepared
        .iter()
        .map(|prep| remove_attempt_replicas(ops, &prep.host, &prep.replicas));
    futures_util::future::join_all(futs).await;
}

/// Force-remove every replica the current deploy attempt created on
/// `host`: running, exited or still in `created` state. The set is the
/// attempt's own [`ReplicaPrep`] list, not a label scan, so the
/// previous generation and replicas that were already at spec before
/// the attempt (`already_running`) are never touched. Best-effort:
/// errors are logged inside `force_remove_name` so the caller can
/// return the original deploy error.
async fn remove_attempt_replicas(ops: &dyn DockerOps, host: &Host, replicas: &[ReplicaPrep]) {
    let futs = replicas
        .iter()
        .filter(|r| !r.already_running)
        .map(|r| force_remove_name(ops, host, &r.name));
    futures_util::future::join_all(futs).await;
}

/// Create the new container in `created` state but do not start it.
/// `finalize_one_host` later issues the start, after pre-deploy hooks
/// (e.g. schema migrations) have run — minimizing the window between
/// migration completion and the runtime container actually serving.
#[allow(clippy::too_many_arguments)] // create-pending threads service + spec + tag + hooks state; each is required context
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
    let mut spec = build_run_spec(
        config,
        service,
        tag,
        secrets,
        new_name.to_string(),
        spec_hash,
        extra_binds,
    );
    // Audit labels are added here, *after* the spec_hash was computed
    // off `spec.labels`, so they never enter the hash and the
    // deploy-time timestamp doesn't masquerade as drift on the next
    // reconcile.
    spec.labels.extend(audit_labels());
    let body = docker::build_container(&spec)?;
    ops.create_container(host, new_name, body)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;
    Ok(())
}

/// The name the HTTP healthcheck probe connects to. For the proxy it
/// is the admin relay: the admin API of the proxy listens on a unix
/// socket, and the relay forwards to it. That is the path of every
/// later `/load`. The proxy deploys stop-first, so only the new
/// container serves the socket during the probe. The relay answers 502
/// until it does. The port is `run.port` of the proxy, the relay port.
fn http_probe_target<'a>(service: &ServiceConfig, new_name: &'a str) -> &'a str {
    if service.kind == Some(crate::config::ServiceKind::Proxy) {
        crate::config::ADMIN_RELAY_NAME
    } else {
        new_name
    }
}

/// The network the deploy probe joins. It must share a network with
/// the target: the first network of the service. The proxy is the
/// exception: the probe targets the admin relay, which is only on the
/// admin network.
fn probe_network<'a>(service: &ServiceConfig, effective: &'a [String]) -> &'a str {
    if service.kind == Some(crate::config::ServiceKind::Proxy) {
        return crate::config::ADMIN_NETWORK;
    }
    effective.first().map_or("bridge", String::as_str)
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
    let effective = config.networks_for(service);
    let probe_network = probe_network(service, &effective);
    let result = match service.run.healthcheck_path.as_deref() {
        Some(path) => {
            healthcheck::poll(
                ops,
                host,
                probe_network,
                http_probe_target(service, new_name),
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
            // reconcile is running in. The caller removes the
            // container right after this returns (a stopped container
            // left behind is indistinguishable from a generation that
            // was once live, and `yoink rollback` would pick it), so
            // this is the last chance to record anything. When the
            // fetch fails, say so instead of staying silent: the audit
            // log builds its `DeployFailed` record from either event.
            match ops.fetch_recent_logs(host, new_name, 30).await {
                Ok(lines) => on_event(DeployEvent::ContainerLogTail {
                    host: host.address.clone(),
                    container: new_name.to_string(),
                    lines,
                }),
                Err(e) => on_event(DeployEvent::ContainerLogTailUnavailable {
                    host: host.address.clone(),
                    container: new_name.to_string(),
                    error: e.to_string(),
                }),
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
        if let Err(e) = stop_old_container(ops, host, drain, &old.name, on_event).await {
            warn!(host = %host.address, old = %old.name, error = %e, "failed to stop old container");
            continue;
        }
        stopped.push(old.name.clone());
    }
    Ok(stopped)
}

/// Start the old containers in `stopped` again, after a stop-first
/// deploy failed. Best-effort: a failure is logged, and the caller
/// returns the original deploy error.
async fn restart_old_containers(
    ops: &dyn DockerOps,
    host: &Host,
    stopped: &[String],
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) {
    for name in stopped {
        match ops.start_container(host, name).await {
            Ok(()) => {
                on_event(DeployEvent::OldContainerRestarted {
                    host: host.address.clone(),
                    container: name.clone(),
                });
            }
            Err(e) => {
                warn!(host = %host.address, container = %name, error = %e, "could not restart the old container after the failed deploy");
            }
        }
    }
}

/// Stop one container and emit `OldContainerStopped`. The caller
/// decides what a failure means: the deploy path warns and moves on,
/// the cleanup-only route fails the service.
async fn stop_old_container(
    ops: &dyn DockerOps,
    host: &Host,
    drain: Duration,
    name: &str,
    on_event: &mut (dyn FnMut(DeployEvent) + Send),
) -> Result<(), DockerError> {
    ops.stop_container(host, name, drain).await?;
    on_event(DeployEvent::OldContainerStopped {
        host: host.address.clone(),
        container: name.to_string(),
    });
    Ok(())
}

/// Resolve a container hook's tag. Subprocess hooks have no tag —
/// callers should check `hook.is_subprocess()` first; this helper
/// returns an empty string when called on a hook with no tag set
/// (defensive fallback; should be unreachable after `validate()`).
fn resolve_hook_tag(
    hook: &HookSpec,
    overrides: &BTreeMap<String, String>,
    services: &[ServiceConfig],
) -> String {
    let Some(tag) = hook.tag.as_ref() else {
        return String::new();
    };
    match tag {
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

/// Dispatch a hook to either the container path (image+tag, runs in
/// docker via `run_one_shot`) or the subprocess path (no image, runs
/// `cmd[0]` directly on the operator's machine with the resolved env
/// merged in). Validation guarantees `image`/`tag` are both Some for
/// container hooks and both None for subprocess hooks; the dispatch
/// here only has to honor that invariant.
///
/// `service` is the owning service for a service-scoped hook, `None`
/// for a top-level one. Its name goes into the container name so two
/// services deployed in the same wave with a same-named hook at the
/// same tag don't fight over one name.
async fn run_hook(
    ops: &dyn DockerOps,
    config: &Config,
    service: Option<&ServiceConfig>,
    hook: &HookSpec,
    tag: &str,
    secrets: Option<&SecretsBundle>,
) -> Result<(), DeployError> {
    if hook.is_subprocess() {
        return run_subprocess_hook(config, hook, secrets).await;
    }
    let (host, hook_network) = hook_target(config, service).ok_or_else(|| DeployError::Hook {
        name: hook.name.clone(),
        message: "no hosts configured".into(),
    })?;

    let image = hook
        .image
        .as_deref()
        .expect("validated: container hook has image");
    let credentials = registry_credentials(config, secrets, image);
    ops.pull_image(&host, image, tag, credentials)
        .await
        .map_err(|source| DeployError::Docker {
            host: host.address.clone(),
            source,
        })?;

    let body = docker::build_hook_container(
        image,
        tag,
        &hook_network,
        hook.entrypoint.as_deref(),
        &hook.cmd,
        &hook_env(hook, secrets),
    );
    let container_name = hook_container_name(service.map(|s| s.name.as_str()), &hook.name, tag);
    // The name is deterministic, so a hook container left behind by an
    // interrupted deploy (Ctrl-C, dropped ssh link) would fail the
    // create with a 409 name conflict on every later deploy. Reap it
    // first, but only if it has stopped. A leftover that still runs
    // can be a migration that survived a dropped link, and the deploy
    // lock does not prove that nobody owns it (the lock sentinel
    // expires after 30s of missed heartbeats). Killing it could leave
    // a half-applied migration, so the operator decides.
    match ops.remove_stopped_container(&host, &container_name).await {
        Ok(StoppedRemoval::Removed | StoppedRemoval::Absent) => {}
        Ok(StoppedRemoval::Running) => {
            return Err(DeployError::Hook {
                name: hook.name.clone(),
                message: format!(
                    "a previous run of this hook is still running on host {} in container \
                     {container_name}. Wait for it to finish, or remove it with \
                     `docker rm -f {container_name}`, then rerun.",
                    host.address
                ),
            });
        }
        // Best effort, as before: if the name is still taken, the
        // create reports the conflict.
        Err(e) => {
            warn!(host = %host.address, container = %container_name, error = %e, "remove of stale hook container failed; continuing");
        }
    }
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

/// The host and network a container hook runs on. A top-level hook
/// uses the first host of the fleet. A service hook uses the first
/// host of its service, so it runs where the service runs. Both join
/// the first of `deploy.networks` (`bridge` when the list is empty).
/// `None` when there is no host.
fn hook_target(config: &Config, service: Option<&ServiceConfig>) -> Option<(Host, String)> {
    let hosts = match service {
        Some(svc) => config.fleet_for(svc),
        None => config.fleet(),
    };
    let host = hosts.into_iter().next()?;
    let network = config
        .networks()
        .first()
        .cloned()
        .unwrap_or_else(|| "bridge".to_string());
    Some((host, network))
}

/// Docker allows only `[a-zA-Z0-9_.-]` in a container name. A digest
/// tag (`sha256:<hex>`) carries a `:`, and hook names are free-form,
/// so every other character becomes `-`.
fn hook_container_name(service: Option<&str>, hook: &str, tag: &str) -> String {
    let name = match service {
        Some(service) => format!("yoink-hook-{service}-{hook}-{tag}"),
        None => format!("yoink-hook-{hook}-{tag}"),
    };
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
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

/// Run a subprocess hook on the operator's machine. Inherits the
/// parent env, applies the profile's `unset` list (via
/// `Command::env_remove`), then layers the resolved hook env on top
/// (merged from `env` + sealed `secrets` + `env_from_secrets`). Sets
/// `working_dir` if specified, falling back to the directory of
/// `yoink.yaml`. Forwards stdout/stderr to the operator's terminal —
/// terraform's plan output stays live. Non-zero exit fails the deploy.
async fn run_subprocess_hook(
    config: &Config,
    hook: &HookSpec,
    secrets: Option<&SecretsBundle>,
) -> Result<(), DeployError> {
    if hook.cmd.is_empty() {
        return Err(DeployError::Hook {
            name: hook.name.clone(),
            message: "subprocess hook has no `cmd:` to run".into(),
        });
    }
    let mut command = tokio::process::Command::new(&hook.cmd[0]);
    if hook.cmd.len() > 1 {
        command.args(&hook.cmd[1..]);
    }

    // Working dir: explicit `working_dir:` if set, else yoink.yaml's
    // directory. The latter matches how operators expect relative
    // paths in `cmd:` to resolve when they pass things like
    // `terraform -chdir=../terraform/cloudflare`.
    let base = config
        .config_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let cwd = match &hook.working_dir {
        Some(rel) if rel.is_absolute() => rel.clone(),
        Some(rel) => base.join(rel),
        None => base,
    };
    command.current_dir(&cwd);

    // Apply the profile's `unset` list — clears any conflicting
    // var the parent shell exported (devbox `init_hook` leaks,
    // operator's own `export …`) before yoink layers the new env on
    // top. Container hooks can't honor this (no parent env to
    // clear); the validation pass at config-load rejects the combo.
    if let Some(profile_name) = hook.secrets_profile.as_deref()
        && let Some(crate::config::SecretsConfig::Age { profiles, .. }) = &config.secrets
        && let Some(profile) = profiles.get(profile_name)
    {
        for key in &profile.unset {
            command.env_remove(key);
        }
    }

    // Layer the resolved hook env on top of the inherited parent env.
    for (k, v) in hook_env(hook, secrets) {
        command.env(k, v);
    }

    // Forward stdout/stderr live; the operator should see terraform's
    // plan output as it streams. Capture nothing here — yoink's deploy
    // log shows which hook ran via `HookStarted`/`HookFinished`.
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());

    let status = command.status().await.map_err(|e| DeployError::Hook {
        name: hook.name.clone(),
        message: format!("spawn `{}`: {e}", hook.cmd[0]),
    })?;
    if !status.success() {
        return Err(DeployError::Hook {
            name: hook.name.clone(),
            message: format!(
                "subprocess `{}` exited {}",
                hook.cmd[0],
                status
                    .code()
                    .map_or_else(|| "by signal".to_string(), |c| format!("with code {c}"))
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{FakeDockerOps, RecordedCall};

    /// The proxy is on `yoink-ingress` only and publishes no admin
    /// port: its admin API is on a unix socket on the config volume. The
    /// deploy probe joins the admin network and targets the relay, the
    /// path of every later `/load`. The Docker HEALTHCHECK checks that
    /// the socket exists.
    #[test]
    fn proxy_run_spec_is_on_ingress_only_and_probes_through_the_relay() {
        use crate::config::{
            ADMIN_NETWORK, ADMIN_RELAY_NAME, ADMIN_RELAY_PORT, INGRESS_NETWORK, PROXY_SERVICE_NAME,
        };
        let cfg = Config::parse_str(
            r#"
hosts: [{ address: h, user: u }]
deploy: { networks: [app] }
proxy: { email: ops@example.com }
services:
  - name: api
    image: example/api
    tag: v1
    domain: api.example.com
    run: { port: 8080 }
"#,
        )
        .expect("config parses");
        let proxy = cfg.service(PROXY_SERVICE_NAME).expect("proxy");
        let spec = build_run_spec(&cfg, proxy, "2", None, "yoink-proxy-x".into(), "h", &[]);
        assert_eq!(spec.networks, [INGRESS_NETWORK]);
        assert_eq!(spec.publish, ["80:80", "443:443"]);
        assert!(
            spec.volumes
                .contains(&"yoink_caddy_config:/config".to_string()),
            "got: {:?}",
            spec.volumes
        );
        let body = docker::build_container(&spec).expect("body");
        let endpoints = body.networking_config.unwrap().endpoints_config.unwrap();
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), [INGRESS_NETWORK]);
        assert_eq!(
            endpoints[INGRESS_NETWORK].aliases.as_deref(),
            Some(&["yoink-proxy-x".to_string()][..])
        );

        assert_eq!(http_probe_target(proxy, "yoink-proxy-x"), ADMIN_RELAY_NAME);
        assert_eq!(probe_network(proxy, &spec.networks), ADMIN_NETWORK);
        assert_eq!(proxy.run.port, Some(ADMIN_RELAY_PORT));
        assert_eq!(proxy.run.healthcheck_path.as_deref(), Some("/config/"));
        let api = cfg.service("api").unwrap();
        assert_eq!(http_probe_target(api, "api-x"), "api-x");
        assert_eq!(probe_network(api, &cfg.networks_for(api)), "app");
        let test = body.healthcheck.and_then(|h| h.test).expect("healthcheck");
        assert_eq!(test, ["CMD-SHELL", "test -S /config/caddy/admin.sock"]);
    }

    #[test]
    fn summarize_build_outcomes_all_success_is_none() {
        let outcomes = vec![("h1".into(), Ok(())), ("h2".into(), Ok(()))];
        assert!(summarize_build_outcomes(outcomes).is_none());
    }

    #[test]
    fn summarize_build_outcomes_empty_is_none() {
        assert!(summarize_build_outcomes(Vec::new()).is_none());
    }

    #[test]
    fn summarize_build_outcomes_single_host_keeps_classic_shape() {
        let outcomes = vec![(
            "h1".into(),
            Err(DockerError::Invalid("compile failure".into())),
        )];
        let err = summarize_build_outcomes(outcomes).expect("err");
        let msg = format!("{err}");
        // Single-host failure path: don't bundle, keep the host name
        // alone so existing error-parsing tests / scripts aren't
        // surprised.
        assert!(msg.contains("h1"), "got: {msg}");
        assert!(!msg.contains("hosts succeeded"), "should not bundle: {msg}");
    }

    #[test]
    fn summarize_build_outcomes_mixed_bundles_summary() {
        let outcomes = vec![
            ("h1".into(), Ok(())),
            (
                "h2".into(),
                Err(DockerError::Invalid("network stall".into())),
            ),
            ("h3".into(), Ok(())),
        ];
        let err = summarize_build_outcomes(outcomes).expect("err");
        let msg = format!("{err}");
        assert!(
            msg.contains("xcaddy build: 2/3 hosts succeeded"),
            "got: {msg}"
        );
        assert!(msg.contains("h2"), "got: {msg}");
        assert!(msg.contains("network stall"), "got: {msg}");
    }

    #[test]
    fn summarize_build_outcomes_multi_failure_lists_each_host() {
        let outcomes = vec![
            ("h1".into(), Err(DockerError::Invalid("err one".into()))),
            ("h2".into(), Err(DockerError::Invalid("err two".into()))),
        ];
        let err = summarize_build_outcomes(outcomes).expect("err");
        let msg = format!("{err}");
        assert!(msg.contains("h1: "), "got: {msg}");
        assert!(msg.contains("h2: "), "got: {msg}");
        assert!(msg.contains("err one"), "got: {msg}");
        assert!(msg.contains("err two"), "got: {msg}");
    }

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
    fn build_labels_excludes_audit_fields() {
        // Regression: deployed-by/deployed-at must NOT be in the
        // hashable label set — a per-call timestamp would make every
        // running container look drifted on the next reconcile.
        let cfg = config_one_service();
        let labels = build_labels(&cfg.services[0], "abc1234", "0123456789abcdef");
        assert!(!labels.contains_key("yoink.deployed-by"));
        assert!(!labels.contains_key("yoink.deployed-at"));
    }

    #[test]
    fn desired_spec_hash_is_stable_across_calls() {
        // The bug this guards against: build_labels used to inject
        // SystemTime::now(), so two calls a few microseconds apart
        // produced different hashes and drift detection always fired.
        let cfg = config_one_service();
        let a = build_desired_spec(&cfg, &cfg.services[0], "abc1234", None).unwrap();
        let b = build_desired_spec(&cfg, &cfg.services[0], "abc1234", None).unwrap();
        assert_eq!(
            crate::docker::compute_spec_hash(&a),
            crate::docker::compute_spec_hash(&b),
        );
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
    fn build_labels_emits_oci_description_when_set() {
        let mut cfg = config_one_service();
        cfg.services[0].description = Some("API gateway".into());
        let labels = build_labels(&cfg.services[0], "abc1234", "0123456789abcdef");
        assert_eq!(
            labels.get("org.opencontainers.image.description"),
            Some(&"API gateway".into())
        );
    }

    #[test]
    fn build_labels_omits_oci_description_when_unset_or_blank() {
        let cfg = config_one_service();
        let labels = build_labels(&cfg.services[0], "abc1234", "0123456789abcdef");
        assert!(!labels.contains_key("org.opencontainers.image.description"));

        let mut blank = config_one_service();
        blank.services[0].description = Some("   ".into());
        let labels = build_labels(&blank.services[0], "abc1234", "0123456789abcdef");
        assert!(!labels.contains_key("org.opencontainers.image.description"));
    }

    #[test]
    fn build_labels_user_override_wins_over_description_field() {
        let mut cfg = config_one_service();
        cfg.services[0].description = Some("from convenience field".into());
        cfg.services[0].labels.insert(
            "org.opencontainers.image.description".into(),
            "explicit user override".into(),
        );
        let labels = build_labels(&cfg.services[0], "abc1234", "0123456789abcdef");
        assert_eq!(
            labels.get("org.opencontainers.image.description"),
            Some(&"explicit user override".into())
        );
    }

    #[test]
    fn build_env_includes_run_env_only_when_no_bundle() {
        let cfg = config_one_service();
        let env = build_env(&cfg.services[0], None);
        assert_eq!(env.get("LOG_LEVEL"), Some(&"info".into()));
        assert!(!env.contains_key("DATABASE_URL"));
    }

    #[test]
    fn build_env_injects_only_requested_secret_keys() {
        let mut cfg = config_one_service();
        cfg.services[0].secrets = vec!["DATABASE_URL".into(), "MISSING".into()];
        let mut values = BTreeMap::new();
        values.insert(
            "DATABASE_URL".into(),
            Zeroizing::new("fake-test-fixture".to_string()),
        );
        values.insert("UNRELATED".into(), Zeroizing::new("ignored".to_string()));
        let bundle = SecretsBundle::new(values);
        let env = build_env(&cfg.services[0], Some(&bundle));
        assert_eq!(
            env.get("DATABASE_URL").map(String::as_str),
            Some("fake-test-fixture")
        );
        assert!(!env.contains_key("UNRELATED"));
        assert!(!env.contains_key("MISSING"));
    }

    /// Regression for #108: with `registry.server: ghcr.io`, the pull of
    /// the default proxy image `caddy:2` carried the ghcr.io credentials
    /// and Docker Hub answered 401.
    #[test]
    fn registry_credentials_go_only_to_the_configured_registry() {
        let mut cfg = config_one_service();
        cfg.registry = Some(crate::config::RegistryConfig {
            server: "ghcr.io".into(),
            username_secret: "GHCR_USERNAME".into(),
            password_secret: "GHCR_TOKEN".into(),
        });
        let mut values = BTreeMap::new();
        values.insert("GHCR_USERNAME".into(), Zeroizing::new("me".to_string()));
        values.insert(
            "GHCR_TOKEN".into(),
            Zeroizing::new("fake-test-fixture".to_string()),
        );
        let bundle = SecretsBundle::new(values);

        let creds = registry_credentials(&cfg, Some(&bundle), "ghcr.io/owner/app")
            .expect("credentials for an image on the configured registry");
        assert_eq!(creds.username.as_deref(), Some("me"));
        assert_eq!(creds.serveraddress.as_deref(), Some("ghcr.io"));

        assert!(registry_credentials(&cfg, Some(&bundle), "caddy").is_none());
        assert!(registry_credentials(&cfg, Some(&bundle), "quay.io/owner/app").is_none());
    }

    fn two_registry_bundle() -> SecretsBundle {
        let mut values = BTreeMap::new();
        for (key, value) in [
            ("GHCR_USERNAME", "ghcr-user"),
            ("GHCR_TOKEN", "fake-ghcr-fixture"),
            ("HUB_USERNAME", "hub-user"),
            ("HUB_TOKEN", "fake-hub-fixture"),
        ] {
            values.insert(key.into(), Zeroizing::new(value.to_string()));
        }
        SecretsBundle::new(values)
    }

    #[test]
    fn registry_credentials_pick_the_registry_that_hosts_the_image() {
        let mut cfg = config_one_service();
        cfg.registries = vec![
            crate::config::RegistryConfig {
                server: "ghcr.io".into(),
                username_secret: "GHCR_USERNAME".into(),
                password_secret: "GHCR_TOKEN".into(),
            },
            crate::config::RegistryConfig {
                server: "index.docker.io".into(),
                username_secret: "HUB_USERNAME".into(),
                password_secret: "HUB_TOKEN".into(),
            },
        ];
        let bundle = two_registry_bundle();

        let ghcr = registry_credentials(&cfg, Some(&bundle), "ghcr.io/owner/app")
            .expect("credentials for the ghcr.io image");
        assert_eq!(ghcr.username.as_deref(), Some("ghcr-user"));
        assert_eq!(ghcr.password.as_deref(), Some("fake-ghcr-fixture"));
        assert_eq!(ghcr.serveraddress.as_deref(), Some("ghcr.io"));

        // A bare image name is on Docker Hub, the second entry.
        let hub = registry_credentials(&cfg, Some(&bundle), "owner/app")
            .expect("credentials for the Docker Hub image");
        assert_eq!(hub.username.as_deref(), Some("hub-user"));
        assert_eq!(hub.password.as_deref(), Some("fake-hub-fixture"));
        assert_eq!(hub.serveraddress.as_deref(), Some("index.docker.io"));

        // A third registry has no entry: anonymous pull.
        assert!(registry_credentials(&cfg, Some(&bundle), "quay.io/owner/app").is_none());
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
        let report = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
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

    /// A container that never passed its healthcheck must not survive
    /// the failed deploy: left behind stopped, it looks like a previous
    /// generation and `yoink rollback` redeploys the broken tag.
    #[tokio::test(start_paused = true)]
    async fn deploy_service_removes_container_that_fails_healthcheck() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        ops.push_start_container(Ok(()));
        // No healthcheck response queued: the probe errors out.
        ops.push_stop_container(Ok(()));
        ops.push_force_remove(Ok(()));
        let cfg = config_one_service();
        let mut sink = |_e: DeployEvent| {};
        let err = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();
        let DeployError::Healthcheck { container, .. } = err else {
            panic!("expected healthcheck error, got {err:?}");
        };
        let calls = ops.calls();
        let stop = calls
            .iter()
            .position(|c| matches!(c, crate::docker_ops::RecordedCall::StopContainer(_, n, _) if *n == container))
            .expect("failed container is stopped");
        assert!(
            calls[stop..]
                .iter()
                .any(|c| matches!(c, crate::docker_ops::RecordedCall::ForceRemoveContainer(_, n) if *n == container)),
            "failed container must be removed after it is stopped: {calls:?}"
        );
    }

    /// A stop-first deploy (a service that publishes host ports, such
    /// as the proxy) stops the old container before the new one starts.
    /// When the new one then failed its healthcheck, yoink removed it
    /// and left the old one stopped: no container of the service ran.
    /// For the proxy, every route was down.
    #[tokio::test(start_paused = true)]
    async fn failed_stop_first_deploy_restarts_the_old_container() {
        let mut cfg = config_one_service();
        cfg.services[0].run.publish = vec!["8443:3000".into()];
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: Some("app-a".into()),
            ..container_with_state(OLD_GENERATION, "running")
        }]));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        // Stop-first: the old container stops before the new start.
        ops.push_stop_container(Ok(()));
        ops.push_start_container(Ok(()));
        // No healthcheck answer: the probe fails. The new container is
        // stopped and removed.
        ops.push_stop_container(Ok(()));
        ops.push_force_remove(Ok(()));
        // The old container starts again.
        ops.push_start_container(Ok(()));

        let (failed, events) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;
        assert_eq!(
            events.last(),
            Some(&DeployEvent::OldContainerRestarted {
                host: "host-a".into(),
                container: OLD_GENERATION.into(),
            }),
            "the restart must be reported: {events:?}"
        );
        let calls = ops.calls();
        let removed = calls
            .iter()
            .rposition(|c| matches!(c, RecordedCall::ForceRemoveContainer(_, n) if *n == failed))
            .expect("failed container removed");
        let restarted = calls
            .iter()
            .rposition(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == OLD_GENERATION))
            .expect("old container restarted");
        assert!(removed < restarted, "ports must be free first: {calls:?}");
    }

    /// A deploy that does not stop first never stopped the old
    /// container, so it has nothing to restart.
    #[tokio::test(start_paused = true)]
    async fn failed_rolling_deploy_starts_no_old_container() {
        let ops = failing_deploy_ops(1, 0);
        ops.push_force_remove(Ok(()));
        let cfg = config_one_service();
        deploy_expecting_healthcheck_failure(&ops, &cfg).await;
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == OLD_GENERATION))
        );
    }

    /// A stop-first routed service: the new container is healthy, but
    /// the Caddy push fails. yoink removed the new container and left
    /// the old one stopped, so no container of the service ran.
    #[tokio::test(start_paused = true)]
    async fn failed_caddy_push_on_stop_first_service_restarts_the_old_container() {
        const OLD: &str = "api-9f8e7d6";
        let mut cfg = config_proxied_service();
        let api = cfg.services.iter().position(|s| s.name == "api").unwrap();
        cfg.services[api].run.publish = vec!["8443:8080".into()];
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![container_with_state(OLD, "running")]));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        // Stop-first: the old container stops before the new start.
        ops.push_stop_container(Ok(()));
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        // Proxy lookup of the Caddy push: no proxy runs, the push fails.
        ops.push_list_containers(Ok(Vec::new()));
        // The new container goes, then the old one starts again.
        ops.push_force_remove(Ok(()));
        ops.push_start_container(Ok(()));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let err = deploy_service(
            &ops,
            &cfg,
            &cfg.services[api],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("push Caddy config"),
            "unexpected error: {err}"
        );
        let calls = ops.calls();
        let new_name = created_names(&ops).pop().expect("new container created");
        let removed = calls
            .iter()
            .rposition(|c| matches!(c, RecordedCall::ForceRemoveContainer(_, n) if *n == new_name))
            .expect("new container removed");
        let restarted = calls
            .iter()
            .rposition(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == OLD))
            .expect("old container restarted");
        assert!(removed < restarted, "ports must be free first: {calls:?}");
        assert!(
            events.contains(&DeployEvent::OldContainerRestarted {
                host: "host-a".into(),
                container: OLD.into(),
            }),
            "got: {events:?}"
        );
    }

    fn config_replicas(replicas: u32) -> Config {
        let mut cfg = config_one_service();
        cfg.services[0].run.replicas = replicas;
        cfg
    }

    const OLD_GENERATION: &str = "app-a-9f8e7d6";

    /// Scripted ops for a deploy of `replicas` new containers next to a
    /// running old generation, where the first `healthy` replicas pass
    /// their healthcheck and the next probe errors out. Cleanup
    /// removals are left to the caller to script.
    fn failing_deploy_ops(replicas: u32, healthy: u32) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: Some("app-a".into()),
            yoink_version: Some("9f8e7d6".into()),
            ..container_with_state(OLD_GENERATION, "running")
        }]));
        for _ in 0..replicas {
            // Prepare: force-remove-by-name, then create.
            ops.push_force_remove(Ok(()));
            ops.push_create_container(Ok("abcdef".into()));
        }
        for _ in 0..=healthy {
            ops.push_start_container(Ok(()));
        }
        for _ in 0..healthy {
            ops.push_healthcheck(Ok(200));
        }
        // No further healthcheck response: the next probe errors out.
        ops.push_stop_container(Ok(()));
        ops
    }

    async fn deploy_expecting_healthcheck_failure(
        ops: &FakeDockerOps,
        cfg: &Config,
    ) -> (String, Vec<DeployEvent>) {
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let err = deploy_service(
            ops,
            cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();
        let DeployError::Healthcheck { container, .. } = err else {
            panic!("expected the original healthcheck error, got {err:?}");
        };
        (container, events)
    }

    fn created_names(ops: &FakeDockerOps) -> Vec<String> {
        ops.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::CreateContainer(_, n) => Some(n),
                _ => None,
            })
            .collect()
    }

    /// Names force-removed after the first `start_container`, i.e. by
    /// the failure unwind rather than by prepare's remove-by-name.
    fn removed_after_start(ops: &FakeDockerOps) -> Vec<String> {
        let calls = ops.calls();
        let first_start = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::StartContainer(..)))
            .expect("a container was started");
        let mut names: Vec<String> = calls[first_start..]
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ForceRemoveContainer(_, n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names
    }

    fn assert_old_generation_untouched(ops: &FakeDockerOps) {
        let calls = ops.calls();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                RecordedCall::StopContainer(_, n, _) | RecordedCall::ForceRemoveContainer(_, n)
                    if n == OLD_GENERATION
            )),
            "the serving generation must not be stopped or removed: {calls:?}"
        );
    }

    fn config_two_hosts() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
  - { address: host-b, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap()
    }

    /// Script a successful prepare (pull, list, remove-by-name, create)
    /// for one host.
    fn push_prepare_ok(ops: &FakeDockerOps) {
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![]));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
    }

    async fn deploy_two_hosts(ops: &FakeDockerOps) -> DeployError {
        let cfg = config_two_hosts();
        let mut sink = |_e: DeployEvent| {};
        deploy_service(
            ops,
            &cfg,
            &cfg.services[0],
            "v2",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err()
    }

    /// Hosts whose replica was force-removed after `after` calls.
    fn hosts_removed_from(ops: &FakeDockerOps, after: usize) -> Vec<String> {
        ops.calls()[after..]
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ForceRemoveContainer(h, _) => Some(h.address.clone()),
                _ => None,
            })
            .collect()
    }

    /// Regression: prepare failed on host-b and the replica already
    /// created on host-a stayed behind in `created` state.
    #[tokio::test(start_paused = true)]
    async fn prepare_failure_on_later_host_removes_earlier_host_replicas() {
        let ops = FakeDockerOps::new();
        push_prepare_ok(&ops);
        ops.push_pull_image(Err(DockerError::Invalid("pull failed".into())));
        let _ = deploy_two_hosts(&ops).await;

        let calls = ops.calls();
        let created = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::CreateContainer(..)))
            .expect("host-a replica created");
        assert_eq!(
            hosts_removed_from(&ops, created),
            vec!["host-a".to_string()]
        );
    }

    /// Regression: finalize failed on host-a and the replica created on
    /// host-b, not finalized yet, stayed behind in `created` state.
    #[tokio::test(start_paused = true)]
    async fn finalize_failure_removes_replicas_of_hosts_not_finalized() {
        let ops = FakeDockerOps::new();
        push_prepare_ok(&ops);
        push_prepare_ok(&ops);
        ops.push_start_container(Err(DockerError::Invalid("start failed".into())));
        let _ = deploy_two_hosts(&ops).await;

        let calls = ops.calls();
        let start = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::StartContainer(..)))
            .expect("a start was attempted");
        let mut removed = hosts_removed_from(&ops, start);
        removed.sort();
        assert_eq!(removed, vec!["host-a".to_string(), "host-b".to_string()]);
    }

    /// Replica 0 passes, replica 1 fails. Replica 0 must not survive:
    /// it is running but unrouted, and after a later successful deploy
    /// stops it, `yoink rollback` would take it for a generation that
    /// was once live.
    #[tokio::test(start_paused = true)]
    async fn failed_deploy_removes_healthy_sibling_replicas() {
        let ops = failing_deploy_ops(2, 1);
        ops.push_fetch_logs(Ok(vec!["boom".into()]));
        ops.push_force_remove(Ok(()));
        ops.push_force_remove(Ok(()));
        let cfg = config_replicas(2);
        let (failed, events) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;

        let mut created = created_names(&ops);
        created.sort();
        assert_eq!(created.len(), 2);
        assert_eq!(failed, created[1], "replica 1 is the one that failed");
        assert_eq!(removed_after_start(&ops), created);
        assert_old_generation_untouched(&ops);

        // The log tail is captured before the failed container goes.
        let calls = ops.calls();
        let logs = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::FetchRecentLogs(_, n, _) if *n == failed))
            .expect("log tail fetched");
        assert!(
            calls[logs..]
                .iter()
                .any(|c| matches!(c, RecordedCall::ForceRemoveContainer(_, n) if *n == failed)),
            "failed container must be removed after its logs are fetched: {calls:?}"
        );
        assert!(events.contains(&DeployEvent::ContainerLogTail {
            host: "host-a".into(),
            container: failed,
            lines: vec!["boom".into()],
        }));
    }

    /// When the log tail cannot be fetched the failure is still
    /// reported (the audit log turns this event into `DeployFailed`)
    /// and the container is removed all the same.
    #[tokio::test(start_paused = true)]
    async fn log_fetch_failure_is_reported_and_container_still_removed() {
        let ops = failing_deploy_ops(1, 0);
        ops.push_fetch_logs(Err(DockerError::Invalid("ssh session closed".into())));
        ops.push_force_remove(Ok(()));
        let cfg = config_replicas(1);
        let (failed, events) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;

        assert_eq!(removed_after_start(&ops), vec![failed.clone()]);
        let unavailable: Vec<&DeployEvent> = events
            .iter()
            .filter(|e| matches!(e, DeployEvent::ContainerLogTailUnavailable { .. }))
            .collect();
        let [
            DeployEvent::ContainerLogTailUnavailable {
                host,
                container,
                error,
            },
        ] = unavailable.as_slice()
        else {
            panic!("expected one ContainerLogTailUnavailable event, got {events:?}");
        };
        assert_eq!(host, "host-a");
        assert_eq!(*container, failed);
        assert!(error.contains("ssh session closed"), "got {error:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DeployEvent::ContainerLogTail { .. }))
        );
    }

    /// Failure on the last of three replicas: the two that passed go
    /// with it.
    #[tokio::test(start_paused = true)]
    async fn failed_deploy_on_last_replica_removes_all_replicas() {
        let ops = failing_deploy_ops(3, 2);
        for _ in 0..3 {
            ops.push_force_remove(Ok(()));
        }
        let cfg = config_replicas(3);
        let (failed, _) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;

        let mut created = created_names(&ops);
        created.sort();
        assert_eq!(created.len(), 3);
        assert_eq!(failed, created[2]);
        assert_eq!(removed_after_start(&ops), created);
        assert_old_generation_untouched(&ops);
    }

    /// Failure on the first of three replicas: the two still in
    /// `created` state are removed as well.
    #[tokio::test(start_paused = true)]
    async fn failed_deploy_removes_replicas_never_started() {
        let ops = failing_deploy_ops(3, 0);
        for _ in 0..3 {
            ops.push_force_remove(Ok(()));
        }
        let cfg = config_replicas(3);
        let (failed, _) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;

        let mut created = created_names(&ops);
        created.sort();
        assert_eq!(failed, created[0]);
        assert_eq!(removed_after_start(&ops), created);
        assert_old_generation_untouched(&ops);
    }

    /// A removal that fails during the unwind is logged, every other
    /// replica is still attempted, and the caller gets the healthcheck
    /// error rather than the cleanup error.
    #[tokio::test(start_paused = true)]
    async fn failed_cleanup_does_not_mask_the_deploy_error() {
        let ops = failing_deploy_ops(2, 1);
        ops.push_force_remove(Err(DockerError::Invalid("daemon went away".into())));
        ops.push_force_remove(Err(DockerError::Invalid("daemon went away".into())));
        let cfg = config_replicas(2);
        // Panics unless the error is the original `Healthcheck` one.
        let (_, _) = deploy_expecting_healthcheck_failure(&ops, &cfg).await;

        let mut created = created_names(&ops);
        created.sort();
        assert_eq!(removed_after_start(&ops), created);
        assert_old_generation_untouched(&ops);
    }

    /// A start failure unwinds the same way as a healthcheck failure.
    #[tokio::test(start_paused = true)]
    async fn start_failure_removes_all_replicas() {
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(Vec::new()));
        for _ in 0..2 {
            ops.push_force_remove(Ok(()));
            ops.push_create_container(Ok("abcdef".into()));
        }
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        ops.push_start_container(Err(DockerError::Invalid("no such image".into())));
        ops.push_force_remove(Ok(()));
        ops.push_force_remove(Ok(()));
        let cfg = config_replicas(2);
        let mut sink = |_e: DeployEvent| {};
        let err = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DeployError::Docker { .. }), "got {err:?}");
        let mut created = created_names(&ops);
        created.sort();
        assert_eq!(removed_after_start(&ops), created);
    }

    /// When creating replica 1 fails, replica 0 is not left behind in
    /// `created` state.
    #[tokio::test(start_paused = true)]
    async fn create_failure_removes_replicas_created_so_far() {
        let ops = FakeDockerOps::new();
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("abcdef".into()));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Err(DockerError::Invalid("no space left".into())));
        ops.push_force_remove(Ok(()));
        let cfg = config_replicas(2);
        let mut sink = |_e: DeployEvent| {};
        let err = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DeployError::Docker { .. }), "got {err:?}");
        let calls = ops.calls();
        let removed: Vec<&String> = calls
            .iter()
            .filter_map(|c| match c {
                RecordedCall::ForceRemoveContainer(_, n) => Some(n),
                _ => None,
            })
            .collect();
        // Two by prepare (one per replica), then replica 0 again.
        assert_eq!(removed.len(), 3, "{calls:?}");
        assert_eq!(removed[2], removed[0]);
        assert_ne!(removed[2], removed[1]);
    }

    /// Retrying the same version after a failed attempt starts every
    /// replica again: nothing of the failed attempt is left to be
    /// mistaken for a replica that is already at spec.
    #[tokio::test(start_paused = true)]
    async fn retry_after_failed_deploy_recreates_every_replica() {
        let ops = failing_deploy_ops(2, 1);
        ops.push_force_remove(Ok(()));
        ops.push_force_remove(Ok(()));
        let cfg = config_replicas(2);
        deploy_expecting_healthcheck_failure(&ops, &cfg).await;
        let first_attempt = created_names(&ops);

        // The host now holds only the old generation again.
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: Some("app-a".into()),
            ..container_with_state(OLD_GENERATION, "running")
        }]));
        for _ in 0..2 {
            ops.push_force_remove(Ok(()));
            ops.push_create_container(Ok("abcdef".into()));
            ops.push_start_container(Ok(()));
            ops.push_healthcheck(Ok(200));
        }
        ops.push_stop_container(Ok(()));
        let mut sink = |_e: DeployEvent| {};
        let report = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            report.hosts[0].stopped_old,
            vec![OLD_GENERATION.to_string()]
        );
        let all = created_names(&ops);
        assert_eq!(all.len(), 4);
        assert_eq!(all[2..], first_attempt[..]);
    }

    /// `happy_ops` flavoured for the reconcile entry point: prepends
    /// the per-host pre-flight `list_containers` snapshot (returns
    /// empty so the snapshot doesn't trick the pre-flight check into
    /// claiming the service is already at spec).
    fn happy_reconcile_ops(existing_running: &[(&str, &str)]) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_list_containers(Ok(Vec::new()));
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
    async fn reconcile_loops_over_services() {
        // Single service still works through the reconcile entry point.
        let ops = happy_reconcile_ops(&[]);
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |_svc: Option<&str>, e: DeployEvent| events.push(e);
        let reports = reconcile(&ops, &cfg, &BTreeMap::new(), None, None, &mut sink)
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].tag, "latest");
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_applies_tag_override() {
        let ops = happy_reconcile_ops(&[]);
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |_svc: Option<&str>, e: DeployEvent| events.push(e);
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
        let report = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(report.hosts[0].container, expected_name);
        assert!(report.hosts[0].stopped_old.is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            DeployEvent::AlreadyAtSpec { container, .. } if container == &expected_name
        )));
    }

    fn snapshot_container(service: &str, name: &str, hash: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            yoink_service: Some(service.into()),
            yoink_spec_hash: Some(hash.into()),
            ..container_with_state(name, state)
        }
    }

    fn snapshot_of(
        containers: Vec<ContainerInfo>,
    ) -> std::collections::HashMap<String, Vec<ContainerInfo>> {
        std::collections::HashMap::from([("host-a".to_string(), containers)])
    }

    fn extras_of(status: &SpecStatus) -> Vec<String> {
        match status {
            SpecStatus::AtSpecWithExtras(hosts) => {
                hosts.iter().flat_map(|h| h.extras.clone()).collect()
            }
            other => panic!("expected AtSpecWithExtras, got {other:?}"),
        }
    }

    /// The pre-flight check must report a running old generation as an
    /// extra instead of a plain "at spec". A deploy that died between
    /// "new healthy" and "old stopped" left the old generation up; the
    /// next reconcile skipped the service and the final Caddy push
    /// load-balanced across both generations.
    #[test]
    fn already_at_spec_reports_running_old_generation_as_extra() {
        let cfg = config_one_service();
        let service = &cfg.services[0];
        let desired = "a".repeat(64);
        let stale = "b".repeat(64);
        let new_name = container_name("app-a", &desired, 0, 1);
        let old_name = container_name("app-a", &stale, 0, 1);

        // Only the expected replica: at spec, nothing to clean up.
        let snap = snapshot_of(vec![snapshot_container(
            "app-a", &new_name, &desired, "running",
        )]);
        assert!(matches!(
            service_already_at_spec(&snap, &cfg, service, &desired),
            SpecStatus::AtSpec(_)
        ));

        // Old generation still running next to it: cleanup.
        let snap = snapshot_of(vec![
            snapshot_container("app-a", &new_name, &desired, "running"),
            snapshot_container("app-a", &old_name, &stale, "running"),
        ]);
        let status = service_already_at_spec(&snap, &cfg, service, &desired);
        assert_eq!(extras_of(&status), vec![old_name.clone()]);

        // Old generation already stopped: not an extra.
        let snap = snapshot_of(vec![
            snapshot_container("app-a", &new_name, &desired, "running"),
            snapshot_container("app-a", &old_name, &stale, "exited"),
        ]);
        assert!(matches!(
            service_already_at_spec(&snap, &cfg, service, &desired),
            SpecStatus::AtSpec(_)
        ));

        // A running container of another service is not an extra.
        let snap = snapshot_of(vec![
            snapshot_container("app-a", &new_name, &desired, "running"),
            snapshot_container("app-b", "app-b-1234567", &stale, "running"),
        ]);
        assert!(matches!(
            service_already_at_spec(&snap, &cfg, service, &desired),
            SpecStatus::AtSpec(_)
        ));

        // Expected replica missing: a deploy, even with the old
        // generation running.
        let snap = snapshot_of(vec![snapshot_container(
            "app-a", &old_name, &stale, "running",
        )]);
        assert_eq!(
            service_already_at_spec(&snap, &cfg, service, &desired),
            SpecStatus::NotAtSpec
        );

        // Host absent from the snapshot: a deploy.
        assert_eq!(
            service_already_at_spec(&std::collections::HashMap::new(), &cfg, service, &desired),
            SpecStatus::NotAtSpec
        );
    }

    /// `replicas` is not part of the spec hash, so after scaling
    /// 3 -> 2 the `-0` / `-1` names still match. The surplus `-2` must
    /// be reported as an extra.
    #[test]
    fn already_at_spec_reports_surplus_replica_as_extra() {
        let mut cfg = config_one_service();
        cfg.services[0].run.replicas = 2;
        let service = &cfg.services[0];
        let desired = "a".repeat(64);
        let mut containers: Vec<ContainerInfo> = (0..2)
            .map(|i| {
                snapshot_container(
                    "app-a",
                    &container_name("app-a", &desired, i, 2),
                    &desired,
                    "running",
                )
            })
            .collect();
        assert!(matches!(
            service_already_at_spec(&snapshot_of(containers.clone()), &cfg, service, &desired),
            SpecStatus::AtSpec(_)
        ));

        let surplus = container_name("app-a", &desired, 2, 3);
        containers.push(snapshot_container("app-a", &surplus, &desired, "running"));
        let status = service_already_at_spec(&snapshot_of(containers), &cfg, service, &desired);
        assert_eq!(extras_of(&status), vec![surplus]);
    }

    /// `app-a` with a service-scoped `pre_deploy` hook, so the
    /// cleanup-only tests can prove the hook does not run.
    fn config_service_with_hook(replicas: u32) -> Config {
        Config::parse_str(&format!(
            r#"
hosts:
  - {{ address: host-a, user: deploy }}
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    pre_deploy:
      - name: migrate
        image: registry.example.com/app-a
        tag: {{ service: app-a }}
        cmd: ["bin/migrate"]
    run:
      replicas: {replicas}
      port: 3000
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
"#
        ))
        .unwrap()
    }

    fn desired_hash_of(cfg: &Config, service: &str) -> String {
        let svc = cfg.services.iter().find(|s| s.name == service).unwrap();
        docker::compute_spec_hash(&build_desired_spec(cfg, svc, "latest", None).unwrap())
    }

    /// Fake primed for a reconcile whose pre-flight snapshot returns
    /// `snapshot`. Nothing else is queued except what the caller adds,
    /// so an unexpected pull, hook run or stop fails the reconcile
    /// with `FakeExhausted`.
    fn ops_with_snapshot(
        cfg: &Config,
        filter: Option<&[String]>,
        snapshot: Vec<ContainerInfo>,
    ) -> FakeDockerOps {
        let ops = FakeDockerOps::new();
        for _ in &cfg.deploy.networks {
            ops.push_ensure_network(Ok(false));
        }
        if cfg.proxy_enabled() {
            push_relay_at_spec(&ops, cfg, filter);
        }
        ops.push_list_containers(Ok(snapshot));
        ops
    }

    /// The relay lookup of `ensure_admin_relays` finds a relay at spec.
    /// A run that deploys the proxy first looks up the running proxy:
    /// none runs, so the spec is the one of the configured image.
    fn push_relay_at_spec(ops: &FakeDockerOps, cfg: &Config, filter: Option<&[String]>) {
        use crate::proxy::relay::{RELAY_NAME, RelayImage, relay_spec_hash, run_selects_proxy};
        let image = RelayImage::of(cfg, &BTreeMap::new()).expect("proxy on");
        if run_selects_proxy(filter) {
            ops.push_list_containers(Ok(Vec::new()));
        }
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: None,
            yoink_spec_hash: Some(relay_spec_hash(&image)),
            ..container_with_state(RELAY_NAME, "running")
        }]));
    }

    /// `ensure_admin_relays` reports a relay that it created, and stays
    /// quiet for a relay at spec.
    #[tokio::test]
    async fn admin_relay_creation_is_reported() {
        let cfg = Config::parse_str(
            r"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: api
    image: registry.example.com/api
    domain: api.example.test
    tls: off
    run: { port: 8080 }
",
        )
        .unwrap();
        let ops = FakeDockerOps::new();
        ops.mark_image_present("caddy", "2");
        ops.push_list_containers(Ok(Vec::new())); // no proxy runs
        ops.push_list_containers(Ok(Vec::new())); // no relay
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        let mut events = Vec::new();
        relays(&ops, &cfg, None, false, &mut events).await.unwrap();
        assert_eq!(
            events,
            vec![DeployEvent::AdminRelayCreated {
                host: "host-a".into()
            }]
        );

        let ops = FakeDockerOps::new();
        push_relay_at_spec(&ops, &cfg, None);
        let mut events = Vec::new();
        relays(&ops, &cfg, None, false, &mut events).await.unwrap();
        assert!(events.is_empty(), "got: {events:?}");
    }

    fn relay_host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        }
    }

    async fn relays(
        ops: &FakeDockerOps,
        cfg: &Config,
        filter: Option<&[String]>,
        no_registry: bool,
        events: &mut Vec<DeployEvent>,
    ) -> Result<(), DeployError> {
        ensure_admin_relays(
            ops,
            cfg,
            &BTreeMap::new(),
            filter,
            None,
            no_registry,
            &mut |e| {
                events.push(e);
            },
        )
        .await
    }

    /// The relay runs the proxy image. When the image is not on the
    /// host (a new `proxy.image`), a run that deploys the proxy pulls
    /// it before it creates the relay, as for the proxy itself.
    #[tokio::test]
    async fn admin_relay_pulls_a_missing_proxy_image_first() {
        use crate::proxy::relay::RELAY_NAME;
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new())); // no proxy runs
        ops.push_list_containers(Ok(Vec::new())); // no relay
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        let mut events = Vec::new();
        relays(&ops, &cfg, None, false, &mut events).await.unwrap();
        let calls = ops.calls();
        let pull = calls
            .iter()
            .position(|c| *c == RecordedCall::PullImage(relay_host(), "caddy".into(), "2".into()))
            .expect("pull of the proxy image");
        let create = calls
            .iter()
            .position(|c| *c == RecordedCall::CreateContainer(relay_host(), RELAY_NAME.into()))
            .expect("relay created");
        assert!(pull < create, "got: {calls:?}");
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(_, i, _) if i.contains("socat"))),
            "got: {calls:?}"
        );
    }

    /// With `--no-registry`, an unscoped run ships the proxy image
    /// before the reconcile, and the relay uses it without a pull. When
    /// the image is missing, yoink stops with a clear error and pulls
    /// nothing.
    #[tokio::test]
    async fn admin_relay_with_no_registry_never_pulls() {
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.mark_image_present("caddy", "2");
        ops.push_list_containers(Ok(Vec::new())); // no proxy runs
        ops.push_list_containers(Ok(Vec::new())); // no relay
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        relays(&ops, &cfg, None, true, &mut Vec::new())
            .await
            .unwrap();
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(..))),
            "got: {:?}",
            ops.calls()
        );

        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new())); // no proxy runs
        ops.push_list_containers(Ok(Vec::new())); // no relay
        let err = relays(&ops, &cfg, None, true, &mut Vec::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("proxy image caddy:2"), "got: {err}");
        assert!(err.contains("--service yoink-proxy"), "got: {err}");
        assert!(
            !ops.calls().iter().any(|c| matches!(
                c,
                RecordedCall::PullImage(..) | RecordedCall::CreateContainer(..)
            )),
            "got: {:?}",
            ops.calls()
        );
    }

    /// With `proxy.xcaddy:`, the proxy image exists only on the hosts.
    /// A missing one is built, as in the prefetch, and not pulled.
    #[tokio::test]
    async fn admin_relay_builds_a_missing_xcaddy_image() {
        let cfg = Config::parse_str(
            r"
hosts:
  - { address: host-a, user: deploy }
proxy:
  xcaddy:
    plugins: [github.com/caddy-dns/cloudflare@v0.2.1]
services:
  - name: api
    image: registry.example.com/api
    tag: v1
    domain: api.example.test
    tls: off
    run: { port: 8080 }
",
        )
        .unwrap();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new())); // no proxy runs
        ops.push_list_containers(Ok(Vec::new())); // no relay
        ops.push_pull_image(Ok(())); // the xcaddy builder image
        ops.push_build_image(Ok(()));
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        relays(&ops, &cfg, None, false, &mut Vec::new())
            .await
            .unwrap();
        let local = cfg.xcaddy().unwrap().resolved_local_tag();
        assert!(
            ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::BuildImage(_, t, _) if *t == local)),
            "got: {:?}",
            ops.calls()
        );
    }

    /// A relay of an earlier proxy image: running, another spec hash.
    fn stale_relay() -> ContainerInfo {
        ContainerInfo {
            yoink_service: None,
            yoink_spec_hash: Some("0000".into()),
            ..container_with_state(crate::proxy::relay::RELAY_NAME, "running")
        }
    }

    fn no_pull_or_create(ops: &FakeDockerOps) {
        assert!(
            !ops.calls().iter().any(|c| matches!(
                c,
                RecordedCall::PullImage(..)
                    | RecordedCall::BuildImage(..)
                    | RecordedCall::CreateContainer(..)
                    | RecordedCall::ForceRemoveContainer(..)
            )),
            "got: {:?}",
            ops.calls()
        );
    }

    /// A run that does not deploy the proxy keeps a running relay at
    /// another spec. It does not pull the new proxy image.
    #[tokio::test]
    async fn scoped_run_keeps_a_stale_running_relay() {
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![stale_relay()]));
        let filter = ["api".to_string()];
        let mut events = Vec::new();
        relays(&ops, &cfg, Some(&filter), false, &mut events)
            .await
            .unwrap();
        assert!(events.is_empty(), "got: {events:?}");
        no_pull_or_create(&ops);
    }

    /// A run that does not deploy the proxy creates a missing relay
    /// with the image of the running proxy, not the configured image,
    /// and pulls nothing.
    #[tokio::test]
    async fn scoped_run_creates_a_missing_relay_with_the_running_proxy_image() {
        use crate::proxy::relay::{RELAY_NAME, RelayImage, relay_spec_hash};
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new()));
        let proxy = ContainerInfo {
            yoink_service: Some(crate::config::PROXY_SERVICE_NAME.into()),
            ..container_with_state("yoink-proxy-abc", "running")
        };
        ops.push_list_containers(Ok(vec![proxy]));
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: "yoink-proxy-abc".into(),
            image: Some("caddy:2.7".into()),
            ..Default::default()
        });
        ops.mark_image_present("caddy", "2.7");
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        let filter = ["api".to_string()];
        let mut events = Vec::new();
        relays(&ops, &cfg, Some(&filter), false, &mut events)
            .await
            .unwrap();
        assert_eq!(
            events,
            [DeployEvent::AdminRelayCreated {
                host: "host-a".into()
            }]
        );
        assert!(
            ops.calls().contains(&RecordedCall::CreateContainer(
                relay_host(),
                RELAY_NAME.into()
            )),
            "got: {:?}",
            ops.calls()
        );
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PullImage(..))),
            "got: {:?}",
            ops.calls()
        );
        let body = ops.created_body(RELAY_NAME).expect("relay body");
        assert_eq!(body.image.as_deref(), Some("caddy:2.7"));
        let running = RelayImage {
            image: "caddy".into(),
            tag: "2.7".into(),
        };
        assert_eq!(
            body.labels.unwrap().get("yoink.spec_hash"),
            Some(&relay_spec_hash(&running))
        );
    }

    /// When the image of the running proxy is not on the host, a run
    /// that does not deploy the proxy warns and creates no relay.
    #[tokio::test]
    async fn scoped_run_skips_the_relay_without_the_image() {
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_list_containers(Ok(Vec::new()));
        let filter = ["api".to_string()];
        let mut events = Vec::new();
        relays(&ops, &cfg, Some(&filter), true, &mut events)
            .await
            .unwrap();
        assert_eq!(
            events,
            [DeployEvent::AdminRelaySkipped {
                host: "host-a".into(),
                image: "caddy:2".into(),
            }]
        );
        no_pull_or_create(&ops);
    }

    /// The proxy container `yoink-proxy-abc` runs `image`, after its
    /// inspect.
    fn push_running_proxy(ops: &FakeDockerOps, image: &str) {
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: Some(crate::config::PROXY_SERVICE_NAME.into()),
            ..container_with_state("yoink-proxy-abc", "running")
        }]));
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: "yoink-proxy-abc".into(),
            image: Some(image.into()),
            ..Default::default()
        });
    }

    /// An unscoped run, and a run with `--service yoink-proxy`, replace
    /// a stale relay. With a running proxy, the new relay runs the
    /// image of that proxy. With no proxy, it runs the configured
    /// image, and yoink makes that image present first.
    #[tokio::test]
    async fn proxy_run_recreates_a_stale_relay() {
        use crate::proxy::relay::RELAY_NAME;
        let proxy_only = [crate::config::PROXY_SERVICE_NAME.to_string()];
        for filter in [None, Some(&proxy_only[..])] {
            for running in [Some("caddy:2.7"), None] {
                let cfg = config_proxied_service();
                let ops = FakeDockerOps::new();
                if let Some(image) = running {
                    push_running_proxy(&ops, image);
                    ops.mark_image_present("caddy", "2.7");
                } else {
                    ops.push_list_containers(Ok(Vec::new()));
                    ops.push_pull_image(Ok(()));
                }
                ops.push_list_containers(Ok(vec![stale_relay()]));
                ops.push_force_remove(Ok(()));
                ops.push_create_container(Ok("id".into()));
                ops.push_start_container(Ok(()));
                relays(&ops, &cfg, filter, false, &mut Vec::new())
                    .await
                    .unwrap();
                let calls = ops.calls();
                assert!(
                    calls.contains(&RecordedCall::ForceRemoveContainer(
                        relay_host(),
                        RELAY_NAME.into()
                    )),
                    "got: {calls:?}"
                );
                let pulled = calls.contains(&RecordedCall::PullImage(
                    relay_host(),
                    "caddy".into(),
                    "2".into(),
                ));
                assert_eq!(pulled, running.is_none(), "got: {calls:?}");
                let body = ops.created_body(RELAY_NAME).expect("relay body");
                assert_eq!(body.image.as_deref(), Some(running.unwrap_or("caddy:2")));
            }
        }
    }

    /// Regression: a run that deploys the proxy ensures the relay
    /// before the proxy deploy. With a new `proxy.image` that cannot
    /// run Caddy, the proxy deploy fails its healthcheck and starts the
    /// old proxy again. A relay on the new image crash-looped and every
    /// later Caddy push failed. The relay must use the image of the
    /// running proxy and must not get the configured image.
    #[tokio::test]
    async fn proxy_run_builds_the_relay_from_the_running_proxy_image() {
        use crate::proxy::relay::{RELAY_NAME, RelayImage, relay_spec_hash};
        let cfg = Config::parse_str(
            r"
hosts:
  - { address: host-a, user: deploy }
proxy:
  image: nginx:alpine
services:
  - name: api
    image: registry.example.com/api
    domain: api.example.test
    tls: off
    run: { port: 8080 }
",
        )
        .unwrap();
        let configured = RelayImage::of(&cfg, &BTreeMap::new()).unwrap();
        assert_eq!(configured.reference(), "nginx:alpine");
        let running = RelayImage {
            image: "caddy".into(),
            tag: "2".into(),
        };
        let ops = FakeDockerOps::new();
        push_running_proxy(&ops, "caddy:2");
        ops.mark_image_present("caddy", "2");
        ops.mark_image_present("nginx", "alpine");
        ops.push_list_containers(Ok(vec![stale_relay()]));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        relays(&ops, &cfg, None, false, &mut Vec::new())
            .await
            .unwrap();
        let body = ops.created_body(RELAY_NAME).expect("relay body");
        assert_eq!(body.image.as_deref(), Some("caddy:2"));
        assert_eq!(
            body.labels.unwrap().get("yoink.spec_hash"),
            Some(&relay_spec_hash(&running))
        );
        no_pull(&ops);

        // A relay at the spec of the running proxy stays. The run does
        // not replace it with the configured image.
        let ops = FakeDockerOps::new();
        push_running_proxy(&ops, "caddy:2");
        ops.push_list_containers(Ok(vec![ContainerInfo {
            yoink_service: None,
            yoink_spec_hash: Some(relay_spec_hash(&running)),
            ..container_with_state(RELAY_NAME, "running")
        }]));
        relays(&ops, &cfg, None, false, &mut Vec::new())
            .await
            .unwrap();
        no_pull_or_create(&ops);
    }

    /// When the image of the running proxy has no tag on the host any
    /// more, a run that deploys the proxy falls back to the configured
    /// image and makes it present, as on a fresh host.
    #[tokio::test]
    async fn proxy_run_uses_the_configured_image_without_the_running_one() {
        use crate::proxy::relay::RELAY_NAME;
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        push_running_proxy(&ops, "caddy:2.7");
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_pull_image(Ok(()));
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        relays(&ops, &cfg, None, false, &mut Vec::new())
            .await
            .unwrap();
        assert!(
            ops.calls().contains(&RecordedCall::PullImage(
                relay_host(),
                "caddy".into(),
                "2".into()
            )),
            "got: {:?}",
            ops.calls()
        );
        let body = ops.created_body(RELAY_NAME).expect("relay body");
        assert_eq!(body.image.as_deref(), Some("caddy:2"));
    }

    fn no_pull(ops: &FakeDockerOps) {
        assert!(
            !ops.calls().iter().any(|c| matches!(
                c,
                RecordedCall::PullImage(..) | RecordedCall::BuildImage(..)
            )),
            "got: {:?}",
            ops.calls()
        );
    }

    type TaggedEvents = Vec<(Option<String>, DeployEvent)>;

    async fn reconcile_collect(
        ops: &FakeDockerOps,
        cfg: &Config,
        filter: Option<&[String]>,
    ) -> (Result<Vec<ServiceDeployReport>, DeployError>, TaggedEvents) {
        let mut events: TaggedEvents = Vec::new();
        let mut sink =
            |svc: Option<&str>, e: DeployEvent| events.push((svc.map(str::to_string), e));
        let result = reconcile(ops, cfg, &BTreeMap::new(), filter, None, &mut sink).await;
        (result, events)
    }

    fn stopped_names(calls: &[crate::docker_ops::RecordedCall]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|c| match c {
                crate::docker_ops::RecordedCall::StopContainer(_, name, _) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// The cleanup-only route must not do any deploy work.
    fn assert_no_deploy_calls(calls: &[crate::docker_ops::RecordedCall]) {
        use crate::docker_ops::RecordedCall;
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                RecordedCall::PullImage(..)
                    | RecordedCall::RunOneShot(..)
                    | RecordedCall::CreateContainer(..)
                    | RecordedCall::StartContainer(..)
                    | RecordedCall::ForceRemoveContainer(..)
            )),
            "cleanup-only route did deploy work: {calls:?}"
        );
    }

    /// Service events of a successful cleanup: `Started`, one
    /// `AlreadyAtSpec` per expected replica, one `OldContainerStopped`
    /// per extra, then `Done`.
    fn assert_cleanup_events(events: &TaggedEvents, expected: &[String], extras: &[String]) {
        let got: Vec<&DeployEvent> = events
            .iter()
            .filter(|(svc, _)| svc.as_deref() == Some("app-a"))
            .map(|(_, e)| e)
            .collect();
        let mut want = vec![DeployEvent::Started {
            service: "app-a".into(),
            tag: "latest".into(),
            host: "host-a".into(),
        }];
        want.extend(expected.iter().map(|c| DeployEvent::AlreadyAtSpec {
            host: "host-a".into(),
            container: c.clone(),
        }));
        want.extend(extras.iter().map(|c| DeployEvent::OldContainerStopped {
            host: "host-a".into(),
            container: c.clone(),
        }));
        want.push(DeployEvent::Done {
            host: "host-a".into(),
            container: expected[0].clone(),
        });
        assert_eq!(got, want.iter().collect::<Vec<_>>());
    }

    /// At spec with an old generation still running: reconcile stops
    /// the old container and nothing else. No image pull and no
    /// service-scoped `pre_deploy` hook (a migration must not rerun
    /// just to stop a leftover container).
    #[tokio::test(start_paused = true)]
    async fn reconcile_stops_old_generation_without_redeploying() {
        let cfg = config_service_with_hook(1);
        let hash = desired_hash_of(&cfg, "app-a");
        let new_name = container_name("app-a", &hash, 0, 1);
        let old_name = container_name("app-a", &"b".repeat(64), 0, 1);
        let ops = ops_with_snapshot(
            &cfg,
            None,
            vec![
                snapshot_container("app-a", &new_name, &hash, "running"),
                snapshot_container("app-a", &old_name, &"b".repeat(64), "running"),
            ],
        );
        ops.push_stop_container(Ok(()));

        let (result, events) = reconcile_collect(&ops, &cfg, None).await;
        let reports = result.unwrap();

        let calls = ops.calls();
        assert_no_deploy_calls(&calls);
        assert_eq!(stopped_names(&calls), vec![old_name.clone()]);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].hosts[0].container, new_name);
        assert_eq!(reports[0].hosts[0].stopped_old, vec![old_name.clone()]);
        assert!(
            crate::features::output::format_deploy_summary(&reports).contains("stopped 1 old"),
            "summary must report the stopped container"
        );
        assert_cleanup_events(&events, &[new_name], &[old_name]);
    }

    /// At spec with a surplus replica after scaling 3 -> 2: reconcile
    /// stops `-2` and nothing else.
    #[tokio::test(start_paused = true)]
    async fn reconcile_stops_surplus_replica_without_redeploying() {
        let cfg = config_service_with_hook(2);
        let hash = desired_hash_of(&cfg, "app-a");
        let expected: Vec<String> = (0..2)
            .map(|i| container_name("app-a", &hash, i, 2))
            .collect();
        let surplus = container_name("app-a", &hash, 2, 3);
        let ops = ops_with_snapshot(
            &cfg,
            None,
            expected
                .iter()
                .chain(std::iter::once(&surplus))
                .map(|n| snapshot_container("app-a", n, &hash, "running"))
                .collect(),
        );
        ops.push_stop_container(Ok(()));

        let (result, events) = reconcile_collect(&ops, &cfg, None).await;
        let reports = result.unwrap();

        let calls = ops.calls();
        assert_no_deploy_calls(&calls);
        assert_eq!(stopped_names(&calls), vec![surplus.clone()]);
        assert_eq!(reports[0].hosts[0].container, expected[0]);
        assert_eq!(reports[0].hosts[0].stopped_old, vec![surplus.clone()]);
        assert_cleanup_events(&events, &expected, &[surplus]);
    }

    /// The cleanup-only route pushes Caddy before it stops anything.
    /// The old generation may be the only thing Caddy routes to (the
    /// earlier deploy may have died at its own Caddy push). Here the
    /// push fails because no proxy container is running, so nothing
    /// may be stopped and the error must reach the caller.
    #[tokio::test(start_paused = true)]
    async fn reconcile_cleanup_stops_nothing_when_caddy_push_fails() {
        use crate::docker_ops::RecordedCall;
        let cfg = config_proxied_service();
        let hash = desired_hash_of(&cfg, "api");
        let new_name = container_name("api", &hash, 0, 1);
        let old_name = container_name("api", &"b".repeat(64), 0, 1);
        let ops = ops_with_snapshot(
            &cfg,
            Some(&["api".to_string()]),
            vec![
                snapshot_container("api", &new_name, &hash, "running"),
                snapshot_container("api", &old_name, &"b".repeat(64), "running"),
            ],
        );
        // Proxy container lookup at the start of the push: none running.
        ops.push_list_containers(Ok(Vec::new()));
        // Would let the stop succeed if the route got that far.
        ops.push_stop_container(Ok(()));

        let (result, _events) = reconcile_collect(&ops, &cfg, Some(&["api".to_string()])).await;

        let err = result.expect_err("the failed Caddy push must fail the reconcile");
        assert!(
            err.to_string().contains("push Caddy config"),
            "unexpected error: {err}"
        );
        let calls = ops.calls();
        let proxy_label = format!("yoink.service={}", crate::config::PROXY_SERVICE_NAME);
        assert!(
            calls.iter().any(
                |c| matches!(c, RecordedCall::ListContainersByLabel(_, l) if *l == proxy_label)
            ),
            "the Caddy push was never attempted: {calls:?}"
        );
        assert_eq!(stopped_names(&calls), Vec::<String>::new());
        assert_no_deploy_calls(&calls);
    }

    /// On the cleanup-only route a failed stop is an error, not a
    /// warning: the stop is the whole job, and the error must name the
    /// host and the container.
    #[tokio::test(start_paused = true)]
    async fn reconcile_cleanup_fails_when_extra_cannot_be_stopped() {
        let cfg = config_service_with_hook(1);
        let hash = desired_hash_of(&cfg, "app-a");
        let new_name = container_name("app-a", &hash, 0, 1);
        let old_name = container_name("app-a", &"b".repeat(64), 0, 1);
        let ops = ops_with_snapshot(
            &cfg,
            None,
            vec![
                snapshot_container("app-a", &new_name, &hash, "running"),
                snapshot_container("app-a", &old_name, &"b".repeat(64), "running"),
            ],
        );
        ops.push_stop_container(Err(DockerError::Invalid("daemon said no".into())));

        let (result, events) = reconcile_collect(&ops, &cfg, None).await;

        let msg = result
            .expect_err("a failed stop must fail the reconcile")
            .to_string();
        assert!(msg.contains(&old_name), "container not named: {msg}");
        assert!(msg.contains("host-a"), "host not named: {msg}");
        assert!(msg.contains("daemon said no"), "cause not shown: {msg}");
        assert!(
            !events.iter().any(|(_, e)| matches!(
                e,
                DeployEvent::OldContainerStopped { .. } | DeployEvent::Done { .. }
            )),
            "no stop succeeded, got {events:?}"
        );
        assert_no_deploy_calls(&ops.calls());
    }

    /// At spec with no extras: the skip is unchanged. Reconcile makes
    /// no call beyond the network check and the snapshot, and emits
    /// only `AlreadyAtSpec`.
    #[tokio::test(start_paused = true)]
    async fn reconcile_skips_service_at_spec_without_extras() {
        use crate::docker_ops::RecordedCall;
        let cfg = config_service_with_hook(1);
        let hash = desired_hash_of(&cfg, "app-a");
        let new_name = container_name("app-a", &hash, 0, 1);
        let ops = ops_with_snapshot(
            &cfg,
            None,
            vec![
                snapshot_container("app-a", &new_name, &hash, "running"),
                // Stopped old generation and another service's
                // container: neither is an extra.
                snapshot_container("app-a", "app-a-0ld0ld0", &"b".repeat(64), "exited"),
                snapshot_container("app-b", "app-b-1234567", &"c".repeat(64), "running"),
            ],
        );

        let (result, events) = reconcile_collect(&ops, &cfg, None).await;
        let reports = result.unwrap();

        let calls = ops.calls();
        assert!(
            calls.iter().all(|c| matches!(
                c,
                RecordedCall::EnsureNetwork(..) | RecordedCall::ListContainersByLabel(..)
            )),
            "unexpected calls: {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, RecordedCall::ListContainersByLabel(..)))
                .count(),
            1,
            "only the snapshot may list containers: {calls:?}"
        );
        assert_eq!(reports[0].hosts[0].container, new_name);
        assert!(reports[0].hosts[0].stopped_old.is_empty());
        let service_events: Vec<&DeployEvent> = events
            .iter()
            .filter(|(svc, _)| svc.is_some())
            .map(|(_, e)| e)
            .collect();
        assert_eq!(
            service_events,
            vec![&DeployEvent::AlreadyAtSpec {
                host: "host-a".into(),
                container: new_name,
            }]
        );
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

        let ops = happy_reconcile_ops(&[]);
        ops.push_pull_image(Ok(()));
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        }));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |_svc: Option<&str>, e: DeployEvent| events.push(e);
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

    /// Config for the service-scoped hook tag regressions: `api` runs
    /// a pre-deploy hook whose tag mirrors a *different* service.
    fn config_service_hook_refs_other_service() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: migrations
    image: registry.example.com/migrations
    tag: m-1
    run: { port: 3000, healthcheck_path: /health }
  - name: api
    image: registry.example.com/api
    tag: api-1
    run: { port: 3000, healthcheck_path: /health }
    pre_deploy:
      - name: migrate
        image: registry.example.com/migrations
        tag: { service: migrations }
        cmd: ["bin/migrate"]
      - name: self-check
        image: registry.example.com/api
        tag: { service: api }
        cmd: ["bin/check"]
"#,
        )
        .unwrap()
    }

    /// Tags pulled for `image` during a `deploy_service` run of `api`
    /// from [`config_service_hook_refs_other_service`].
    async fn hook_pull_tags(
        tag: &str,
        overrides: &BTreeMap<String, String>,
        image: &str,
    ) -> Vec<String> {
        let cfg = config_service_hook_refs_other_service();
        let ops = happy_ops(&[]);
        for _ in 0..2 {
            ops.push_pull_image(Ok(()));
            ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }));
        }
        let mut sink = |_e: DeployEvent| {};
        deploy_service(
            &ops,
            &cfg,
            &cfg.services[1],
            tag,
            overrides,
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap();
        ops.calls()
            .into_iter()
            .filter_map(|c| match c {
                crate::docker_ops::RecordedCall::PullImage(_, i, t) if i == image => Some(t),
                _ => None,
            })
            .collect()
    }

    /// Regression: a service-scoped hook with `tag: { service: X }`
    /// received the tag of the service being deployed instead of X's,
    /// so `api`'s migrate hook pulled `migrations:<api-tag>`.
    #[tokio::test(start_paused = true)]
    async fn service_hook_tag_ref_resolves_referenced_service_tag() {
        let tags =
            hook_pull_tags("api-2", &BTreeMap::new(), "registry.example.com/migrations").await;
        assert_eq!(tags, vec!["m-1".to_string()]);
    }

    /// Regression: `--tag migrations=...` was ignored for the
    /// referenced service because hooks resolved against an empty
    /// overrides map.
    #[tokio::test(start_paused = true)]
    async fn service_hook_tag_ref_honours_override_for_referenced_service() {
        let mut overrides = BTreeMap::new();
        overrides.insert("migrations".to_string(), "m-2".to_string());
        let tags = hook_pull_tags("api-2", &overrides, "registry.example.com/migrations").await;
        assert_eq!(tags, vec!["m-2".to_string()]);
    }

    /// A self-reference still mirrors the tag this deploy was called
    /// with (service pull + hook pull), not the configured `tag:`.
    #[tokio::test(start_paused = true)]
    async fn service_hook_tag_self_ref_uses_deploy_tag() {
        let tags = hook_pull_tags("api-2", &BTreeMap::new(), "registry.example.com/api").await;
        assert_eq!(tags, vec!["api-2".to_string(), "api-2".to_string()]);
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
        // ensure_host_networks runs once at the top of reconcile,
        // followed by a pre-flight `list_containers` per host.
        ops.push_ensure_network(Ok(false));
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_pull_image(Ok(()));
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: "applying 0001_init...".into(),
            stderr: "ERROR: connection refused".into(),
        }));

        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |_svc: Option<&str>, e: DeployEvent| events.push(e);
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

    /// Asserts the stopped-only remove of the hook container `name`
    /// comes before the hook runs under that same name, and that the
    /// name is never force-removed.
    fn assert_reaped_before_run(ops: &FakeDockerOps, host: &Host, name: &str) {
        use crate::docker_ops::RecordedCall;
        let calls = ops.calls();
        let reap = calls
            .iter()
            .position(|c| *c == RecordedCall::RemoveStoppedContainer(host.clone(), name.into()))
            .expect("stale hook container is removed if stopped");
        let run = calls
            .iter()
            .position(|c| *c == RecordedCall::RunOneShot(host.clone(), name.into()))
            .expect("hook runs under the expected name");
        assert!(reap < run, "reap must precede the create: {calls:?}");
        assert!(
            !calls.contains(&RecordedCall::ForceRemoveContainer(
                host.clone(),
                name.into()
            )),
            "hook container must not be force-removed: {calls:?}"
        );
    }

    const TOP_LEVEL_HOOK_CONFIG: &str = r#"
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
"#;

    /// Runs a reconcile whose pre-deploy hook finds `leftover` under
    /// its container name. The hook exits non-zero so the reconcile
    /// stops right after it.
    async fn reconcile_with_hook_leftover(
        leftover: StoppedRemoval,
    ) -> (FakeDockerOps, Host, DeployError) {
        let cfg = Config::parse_str(TOP_LEVEL_HOOK_CONFIG).unwrap();
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_pull_image(Ok(()));
        ops.push_remove_stopped(Ok(leftover));
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "hook ran".into(),
        }));

        let mut sink = |_svc: Option<&str>, _e: DeployEvent| {};
        let err = reconcile(&ops, &cfg, &BTreeMap::new(), None, None, &mut sink)
            .await
            .unwrap_err();
        let host = Host::from(&cfg.hosts[0]);
        (ops, host, err)
    }

    /// A hook container leaked by an interrupted deploy keeps its
    /// deterministic name; without a reap before the create, every
    /// later deploy at that tag fails with a 409 name conflict.
    #[tokio::test(start_paused = true)]
    async fn hook_reaps_stale_container_before_run() {
        let (ops, host, err) = reconcile_with_hook_leftover(StoppedRemoval::Removed).await;
        assert_reaped_before_run(&ops, &host, "yoink-hook-migrate-latest");
        assert!(err.to_string().contains("hook ran"), "{err}");
    }

    /// With no leftover the hook runs as usual.
    #[tokio::test(start_paused = true)]
    async fn hook_runs_when_no_leftover_container_exists() {
        let (ops, host, err) = reconcile_with_hook_leftover(StoppedRemoval::Absent).await;
        assert_reaped_before_run(&ops, &host, "yoink-hook-migrate-latest");
        assert!(err.to_string().contains("hook ran"), "{err}");
    }

    /// A leftover that still runs can be a migration that survived a
    /// dropped ssh link. The deploy must not kill it and must not start
    /// a second run; it fails and tells the operator what to do.
    #[tokio::test(start_paused = true)]
    async fn hook_fails_without_touching_a_leftover_that_still_runs() {
        use crate::docker_ops::RecordedCall;
        let (ops, host, err) = reconcile_with_hook_leftover(StoppedRemoval::Running).await;

        let name = "yoink-hook-migrate-latest";
        let calls = ops.calls();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                RecordedCall::ForceRemoveContainer(_, n) | RecordedCall::RunOneShot(_, n)
                    if n == name
            )),
            "running hook container must be left alone and the hook must not run: {calls:?}"
        );
        assert!(calls.contains(&RecordedCall::RemoveStoppedContainer(
            host.clone(),
            name.into()
        )));

        let DeployError::Hook {
            name: hook,
            message,
        } = err
        else {
            panic!("expected Hook error, got {err:?}");
        };
        assert_eq!(hook, "migrate");
        assert!(message.contains("still running"), "{message}");
        assert!(message.contains("host-a"), "{message}");
        assert!(
            message.contains("docker rm -f yoink-hook-migrate-latest"),
            "{message}"
        );
    }

    /// Services in one wave deploy concurrently. Two of them with a
    /// same-named hook at the same tag must not share a container name.
    #[test]
    fn hook_container_name_is_scoped_by_service() {
        assert_eq!(
            hook_container_name(None, "migrate", "v42"),
            "yoink-hook-migrate-v42"
        );
        assert_eq!(
            hook_container_name(Some("api"), "migrate", "v42"),
            "yoink-hook-api-migrate-v42"
        );
        assert_ne!(
            hook_container_name(Some("api"), "migrate", "v42"),
            hook_container_name(Some("web"), "migrate", "v42")
        );
    }

    /// A digest tag carries a `:`, which Docker rejects in a container
    /// name ("Invalid container name"). The name must stay creatable.
    #[test]
    fn hook_container_name_replaces_characters_docker_rejects() {
        assert_eq!(
            hook_container_name(Some("api"), "migrate", "sha256:abc123"),
            "yoink-hook-api-migrate-sha256-abc123"
        );
        assert_eq!(
            hook_container_name(None, "run migrations", "v42"),
            "yoink-hook-run-migrations-v42"
        );
    }

    /// The service-scoped path end to end: `deploy_service` must hand
    /// its own service name to the hook, so the reap and the run both
    /// use `yoink-hook-{service}-{hook}-{tag}`.
    #[tokio::test(start_paused = true)]
    async fn service_hook_reaps_and_runs_under_service_scoped_name() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
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
        ops.push_remove_stopped(Ok(StoppedRemoval::Removed));
        // Exit non-zero so the deploy stops right after the hook.
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }));

        let mut sink = |_e: DeployEvent| {};
        let _ = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "v42",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();

        assert_reaped_before_run(
            &ops,
            &Host::from(&cfg.hosts[0]),
            "yoink-hook-app-a-migrate-v42",
        );
    }

    /// Regression: a service hook ran on the first host of the whole
    /// fleet, even when its service runs on other hosts. It now runs
    /// on the first host of the service. The network stays the first
    /// of `deploy.networks`, the same as for a top-level hook.
    #[test]
    fn service_hook_targets_the_service_host_and_the_first_deploy_network() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: web-1, user: deploy }
  - { address: db-1, user: deploy }
deploy:
  networks: [edge, data]
services:
  - name: api
    image: registry.example.com/api
    tag: v1
    hosts: [db-1]
    networks: [data]
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap();
        let (host, network) = hook_target(&cfg, Some(&cfg.services[0])).unwrap();
        assert_eq!(host.address, "db-1");
        assert_eq!(network, "edge");
        let (host, network) = hook_target(&cfg, None).unwrap();
        assert_eq!(host.address, "web-1");
        assert_eq!(network, "edge");
    }

    /// Regression: a service with `hosts: []` and a container hook
    /// failed the deploy with "no hosts configured". It deploys
    /// nothing, so it runs no hooks.
    #[tokio::test(start_paused = true)]
    async fn service_with_no_hosts_runs_no_hooks() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: web-1, user: deploy }
services:
  - name: api
    image: registry.example.com/api
    tag: v1
    hosts: []
    run: { port: 3000, healthcheck_path: /health }
    pre_deploy:
      - name: migrate
        image: registry.example.com/api
        tag: { service: api }
        cmd: ["bin/migrate"]
"#,
        )
        .unwrap();

        let ops = FakeDockerOps::new();
        let mut events = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let report = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "v1",
            &BTreeMap::new(),
            None,
            true,
            &mut sink,
        )
        .await
        .unwrap();

        assert!(report.hosts.is_empty());
        assert!(ops.calls().is_empty(), "{:?}", ops.calls());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DeployEvent::HookStarted { .. })),
            "{events:?}"
        );
    }

    /// End to end: `deploy_service` runs the hook of a service pinned
    /// to `db-1` on `db-1`, not on the first host of the fleet.
    #[tokio::test(start_paused = true)]
    async fn service_hook_runs_on_a_host_of_its_service() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: web-1, user: deploy }
  - { address: db-1, user: deploy }
services:
  - name: api
    image: registry.example.com/api
    tag: v1
    hosts: [db-1]
    run: { port: 3000, healthcheck_path: /health }
    pre_deploy:
      - name: migrate
        image: registry.example.com/api
        tag: { service: api }
        cmd: ["bin/migrate"]
"#,
        )
        .unwrap();

        let ops = happy_ops(&[]);
        ops.push_pull_image(Ok(()));
        ops.push_remove_stopped(Ok(StoppedRemoval::Absent));
        // Exit non-zero so the deploy stops right after the hook.
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }));

        let mut sink = |_e: DeployEvent| {};
        let _ = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "v1",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .unwrap_err();

        let hook_hosts: Vec<String> = ops
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                crate::docker_ops::RecordedCall::RunOneShot(h, _) => Some(h.address),
                _ => None,
            })
            .collect();
        assert_eq!(hook_hosts, vec!["db-1".to_string()]);
    }

    /// #108 end to end: a service on the configured registry pulls with
    /// credentials, and its hook image on Docker Hub pulls anonymously.
    #[tokio::test(start_paused = true)]
    async fn pulls_send_credentials_only_for_images_on_the_registry() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
registry:
  server: registry.example.com
  username_secret: REG_USER
  password_secret: REG_TOKEN
services:
  - name: app-a
    image: registry.example.com/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
    pre_deploy:
      - name: migrate
        image: busybox
        tag: "1"
        cmd: ["true"]
"#,
        )
        .unwrap();
        let mut values = BTreeMap::new();
        values.insert("REG_USER".into(), Zeroizing::new("me".to_string()));
        values.insert(
            "REG_TOKEN".into(),
            Zeroizing::new("fake-test-fixture".to_string()),
        );
        let bundle = SecretsBundle::new(values);

        let ops = happy_ops(&[]);
        ops.push_pull_image(Ok(()));
        ops.push_remove_stopped(Ok(StoppedRemoval::Removed));
        // Exit non-zero so the deploy stops right after the hook.
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }));

        let mut sink = |_e: DeployEvent| {};
        let _ = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "v42",
            &BTreeMap::new(),
            Some(&bundle),
            false,
            &mut sink,
        )
        .await
        .unwrap_err();

        assert_eq!(
            ops.pull_auth(),
            vec![
                (
                    "registry.example.com/app-a".to_string(),
                    Some("registry.example.com".to_string()),
                ),
                ("busybox".to_string(), None),
            ],
        );
    }

    /// Two registries end to end: the service image and its hook image
    /// are on different configured registries, and each pull carries the
    /// credentials of its own registry.
    #[tokio::test(start_paused = true)]
    async fn pulls_send_each_image_the_credentials_of_its_own_registry() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
registries:
  - server: ghcr.io
    username_secret: GHCR_USERNAME
    password_secret: GHCR_TOKEN
  - server: docker.io
    username_secret: HUB_USERNAME
    password_secret: HUB_TOKEN
services:
  - name: app-a
    image: ghcr.io/owner/app-a
    tag: latest
    run: { port: 3000, healthcheck_path: /health }
    pre_deploy:
      - name: migrate
        image: owner/migrate
        tag: "1"
        cmd: ["true"]
"#,
        )
        .unwrap();
        let bundle = two_registry_bundle();

        let ops = happy_ops(&[]);
        ops.push_pull_image(Ok(()));
        ops.push_remove_stopped(Ok(StoppedRemoval::Removed));
        // Exit non-zero so the deploy stops right after the hook.
        ops.push_one_shot(Ok(crate::docker_ops::OneShotResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }));

        let mut sink = |_e: DeployEvent| {};
        let _ = deploy_service(
            &ops,
            &cfg,
            &cfg.services[0],
            "v42",
            &BTreeMap::new(),
            Some(&bundle),
            false,
            &mut sink,
        )
        .await
        .unwrap_err();

        assert_eq!(
            ops.pull_auth(),
            vec![
                (
                    "ghcr.io/owner/app-a".to_string(),
                    Some("ghcr.io".to_string()),
                ),
                ("owner/migrate".to_string(), Some("docker.io".to_string())),
            ],
        );
    }

    /// Routed-service fixture: api has `domain:` so `is_proxied(api)` is
    /// true. Bare config — no proxy block, but `proxy_enabled` returns
    /// true for any service with a domain.
    fn config_proxied_service() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: api
    image: registry.example.com/api
    tag: latest
    domain: api.example.test
    tls: off
    run:
      port: 8080
      healthcheck_path: /health
      healthcheck_timeout: 60s
      drain_timeout: 10s
"#,
        )
        .unwrap()
    }

    fn container_with_state(name: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: name.into(),
            image: String::new(),
            state: state.into(),
            status_text: String::new(),
            created_unix: None,
            yoink_service: Some("api".into()),
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    /// Regression: during a routed-service reconcile, the upstream
    /// pool pushed to Caddy must contain ONLY the new (expected)
    /// replica names — never the old containers that are about to be
    /// stopped. Including the old names left them in Caddy's pool
    /// after `swap_out_old`, where they became unresolvable in Docker
    /// DNS and the active health checker logged "server misbehaving"
    /// indefinitely (no subsequent /load ever cleaned the pool).
    #[tokio::test]
    async fn build_upstream_names_excludes_old_replicas_for_in_flight_service() {
        let cfg = config_proxied_service();
        let routed = cfg.proxied_services();

        // Both old AND new replicas show up as `running` if we ever
        // queried — the override should bypass the query entirely.
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![
            container_with_state("api-OLDHASH-0", "running"),
            container_with_state("api-OLDHASH-1", "running"),
            container_with_state("api-NEWHASH-0", "running"),
            container_with_state("api-NEWHASH-1", "running"),
        ]));

        let mut expected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        expected.insert("api-NEWHASH-0".into());
        expected.insert("api-NEWHASH-1".into());

        let host = Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        };
        let names = build_upstream_names(&ops, &host, &routed, Some(("api", &expected)))
            .await
            .unwrap();

        assert_eq!(
            names.get("api"),
            Some(&vec![
                "api-NEWHASH-0".to_string(),
                "api-NEWHASH-1".to_string()
            ]),
            "in-flight service must use expected_names, not the live container list"
        );
        // `list_containers_by_label` must NOT have been called for
        // the in-flight service (the override short-circuits the
        // daemon query).
        let calls = ops.calls();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                crate::docker_ops::RecordedCall::ListContainersByLabel(_, label)
                if label == "yoink.service=api"
            )),
            "expected no list_containers_by_label call for in-flight service, got {calls:?}"
        );
    }

    /// Counterpart: services NOT being reconciled keep their existing
    /// behaviour (live `list_containers_by_label` filtered to running).
    #[tokio::test]
    async fn build_upstream_names_queries_other_services_live() {
        let cfg = Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: api
    image: img/api
    tag: latest
    domain: api.example.test
    tls: off
    run: { port: 8080, healthcheck_path: /health }
  - name: web
    image: img/web
    tag: latest
    domain: web.example.test
    tls: off
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap();
        let routed = cfg.proxied_services();

        let ops = FakeDockerOps::new();
        // api: in-flight, query should be skipped via override.
        // web: not in-flight, query should fire and filter to running.
        ops.push_list_containers(Ok(vec![
            container_with_state("web-OLDHASH-0", "running"),
            container_with_state("web-OLDHASH-1", "exited"),
        ]));

        let mut expected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        expected.insert("api-NEWHASH-0".into());

        let host = Host {
            user: "deploy".into(),
            address: "host-a".into(),
            port: None,
        };
        let names = build_upstream_names(&ops, &host, &routed, Some(("api", &expected)))
            .await
            .unwrap();

        assert_eq!(names.get("api"), Some(&vec!["api-NEWHASH-0".to_string()]),);
        assert_eq!(
            names.get("web"),
            Some(&vec!["web-OLDHASH-0".to_string()]),
            "exited containers must be filtered out for non-in-flight services"
        );
    }

    /// Regression: the cleanup-only branch of `finalize_one_host`
    /// (every replica already running, an old generation still up)
    /// must push Caddy before it stops the old container. The old
    /// generation is there because an earlier deploy died after the
    /// new one became healthy, possibly at the Caddy push, so Caddy
    /// may still route only to the old container. Here the push
    /// fails (no proxy container), so nothing may be stopped.
    #[tokio::test]
    async fn cleanup_only_finalize_pushes_caddy_before_stopping_old() {
        let cfg = config_proxied_service();
        let service = &cfg.services[0];
        let ops = FakeDockerOps::new();
        // `discover_admin_target` lookup: no proxy container running.
        ops.push_list_containers(Ok(Vec::new()));

        let prep = HostPrep {
            host: Host {
                user: "deploy".into(),
                address: "host-a".into(),
                port: None,
            },
            replicas: vec![ReplicaPrep {
                name: "api-NEWHASH".into(),
                already_running: true,
            }],
            existing: vec![
                container_with_state("api-NEWHASH", "running"),
                container_with_state("api-OLDHASH", "running"),
            ],
            stop_first: false,
        };
        let mut sink = |_e: DeployEvent| {};
        let result = finalize_one_host(&ops, &cfg, service, prep, None, &mut sink).await;

        assert!(result.is_err(), "the failed Caddy push must abort");
        let calls = ops.calls();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, crate::docker_ops::RecordedCall::StopContainer(..))),
            "old container stopped before Caddy was pushed: {calls:?}"
        );
    }

    /// The cleanup-only branch must end with `Done` once it stopped
    /// an old container. Without it the last event for the service is
    /// `OldContainerStopped` and the TUI leaves it on "swapping".
    #[tokio::test]
    async fn cleanup_only_finalize_emits_done_after_stopping_old() {
        let cfg = config_one_service();
        let service = &cfg.services[0];
        let ops = FakeDockerOps::new();
        ops.push_stop_container(Ok(()));

        let prep = HostPrep {
            host: Host {
                user: "deploy".into(),
                address: "host-a".into(),
                port: None,
            },
            replicas: vec![ReplicaPrep {
                name: "app-a-NEWHASH".into(),
                already_running: true,
            }],
            existing: vec![
                container_with_state("app-a-NEWHASH", "running"),
                container_with_state("app-a-OLDHASH", "running"),
            ],
            stop_first: false,
        };
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let result = finalize_one_host(&ops, &cfg, service, prep, None, &mut sink)
            .await
            .unwrap();

        assert_eq!(result.stopped_old, vec!["app-a-OLDHASH".to_string()]);
        assert!(
            matches!(events.last(), Some(DeployEvent::Done { container, .. }) if container == "app-a-NEWHASH"),
            "expected Done as the last event, got {events:?}"
        );
    }

    /// A reconcile with the proxy on creates the admin relay on each
    /// proxy host before the container snapshot and any Caddy push, also
    /// when the run is scoped. A scoped run does not pull: the proxy
    /// image must be on the host.
    #[tokio::test(start_paused = true)]
    async fn reconcile_creates_the_admin_relay_first() {
        use crate::proxy::relay::RELAY_NAME;
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        for _ in &cfg.deploy.networks {
            ops.push_ensure_network(Ok(false));
        }
        // No relay yet and no proxy: create and start the relay with
        // the configured image.
        ops.push_list_containers(Ok(Vec::new()));
        ops.push_list_containers(Ok(Vec::new()));
        ops.mark_image_present("caddy", "2");
        ops.push_create_container(Ok("id".into()));
        ops.push_start_container(Ok(()));
        // The snapshot.
        ops.push_list_containers(Ok(Vec::new()));
        let (result, _) = reconcile_collect(&ops, &cfg, Some(&["nonexistent".into()])).await;
        // The final push finds no proxy container.
        assert!(result.is_err());
        let calls = ops.calls();
        let start = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == RELAY_NAME))
            .expect("relay started");
        let snapshot = calls
            .iter()
            .position(
                |c| matches!(c, RecordedCall::ListContainersByLabel(_, l) if l == "yoink.managed=true"),
            )
            .expect("snapshot");
        assert!(start < snapshot, "got: {calls:?}");
    }

    const OLD_PROXY: &str = "yoink-proxy-old";
    const NEW_PROXY: &str = "yoink-proxy-new";
    const RELAY_HOST_PORT: u16 = 40000;
    const LEGACY_HOST_PORT: u16 = 32768;

    fn proxy_container(name: &str, state: &str) -> ContainerInfo {
        ContainerInfo {
            yoink_service: Some(crate::config::PROXY_SERVICE_NAME.into()),
            ..container_with_state(name, state)
        }
    }

    /// The v0.22 proxy keeps its published admin port and its HEALTHCHECK
    /// on port 2019 until a deploy recreates it, so Docker shows it as
    /// unhealthy after the move. A run that leaves it in place warns once,
    /// also when the run fails. Here the run fails at its first step.
    #[tokio::test]
    async fn run_that_keeps_an_old_proxy_warns_also_on_error() {
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container("yoink-proxy-old", "running")]));
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: "yoink-proxy-old".into(),
            ports: vec![format!("{LEGACY_HOST_PORT}:2019/tcp")],
            ..Default::default()
        });
        let (result, events) = reconcile_collect(&ops, &cfg, None).await;
        assert!(result.is_err());
        let warnings: Vec<_> = events
            .iter()
            .filter(|(_, e)| matches!(e, DeployEvent::ProxyAdminUpgradePending { .. }))
            .collect();
        assert_eq!(
            warnings,
            [&(
                None,
                DeployEvent::ProxyAdminUpgradePending {
                    hosts: vec!["host-a".into()],
                }
            )]
        );
    }

    /// A proxy of this version, no proxy, and a host that yoink cannot
    /// inspect give no warning.
    #[tokio::test]
    async fn new_or_missing_proxy_gives_no_upgrade_warning() {
        let cfg = config_proxied_service();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![proxy_container(NEW_PROXY, "running")]));
        assert!(legacy_proxy_hosts(&ops, &cfg).await.is_empty());
        ops.push_list_containers(Ok(Vec::new()));
        assert!(legacy_proxy_hosts(&ops, &cfg).await.is_empty());
        // The fake has no more listings: the call fails.
        assert!(legacy_proxy_hosts(&ops, &cfg).await.is_empty());
    }

    fn relay_container() -> ContainerInfo {
        ContainerInfo {
            yoink_service: None,
            ..container_with_state(crate::proxy::relay::RELAY_NAME, "running")
        }
    }

    /// Script the list calls of one Caddy push in `config_proxied_service`
    /// while `proxy` runs: the `api` upstreams, the proxy, the relay.
    fn script_push(ops: &FakeDockerOps, proxy: &str) {
        ops.push_list_containers(Ok(vec![container_with_state("api-1", "running")]));
        ops.push_list_containers(Ok(vec![proxy_container(proxy, "running")]));
        ops.push_list_containers(Ok(vec![relay_container()]));
    }

    /// A host with a proxy of yoink v0.22 (`OLD_PROXY` publishes the
    /// admin port 2019), the relay, and a new proxy `NEW_PROXY` that the
    /// deploy created. The proxy and the relay answer on `legacy` and
    /// `relay`.
    fn host_with_an_old_proxy(
        legacy: &crate::proxy::admin::fake_caddy::FakeCaddy,
        relay: &crate::proxy::admin::fake_caddy::FakeCaddy,
    ) -> (FakeDockerOps, HostPrep) {
        let ops = FakeDockerOps::new();
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: OLD_PROXY.into(),
            ports: vec![
                "80:80/tcp".into(),
                "443:443/tcp".into(),
                format!("{LEGACY_HOST_PORT}:2019/tcp"),
            ],
            ..Default::default()
        });
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: crate::proxy::relay::RELAY_NAME.into(),
            ports: vec![format!("{RELAY_HOST_PORT}:2020/tcp")],
            ..Default::default()
        });
        ops.forward_loopback_to(LEGACY_HOST_PORT, legacy.port);
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        // The admin API is still on port 2019: the relay finds no socket.
        relay.probe_statuses(&[502]);
        let prep = HostPrep {
            host: relay_host(),
            replicas: vec![ReplicaPrep {
                name: NEW_PROXY.into(),
                already_running: false,
            }],
            existing: vec![proxy_container(OLD_PROXY, "running")],
            stop_first: true,
        };
        (ops, prep)
    }

    fn position(calls: &[RecordedCall], wanted: &RecordedCall) -> usize {
        calls
            .iter()
            .position(|c| c == wanted)
            .unwrap_or_else(|| panic!("no {wanted:?} in {calls:?}"))
    }

    /// The old proxy listens on `0.0.0.0:2019`. If yoink stopped it
    /// first, the new proxy would resume that address from
    /// `autosave.json`: open to `yoink-ingress`, out of reach of the
    /// relay. The deploy pushes the socket admin block to the old proxy
    /// on its own port before it stops it. The probe of the new proxy
    /// goes through the relay.
    #[tokio::test]
    async fn proxy_deploy_moves_the_old_admin_api_to_the_socket_before_the_stop() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        let cfg = config_proxied_service();
        let proxy = cfg.service(crate::config::PROXY_SERVICE_NAME).unwrap();
        let (legacy, relay) = (FakeCaddy::start(), FakeCaddy::start());
        let (ops, prep) = host_with_an_old_proxy(&legacy, &relay);
        script_push(&ops, OLD_PROXY);
        ops.push_stop_container(Ok(()));
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        // The push after the healthcheck finds the new proxy.
        script_push(&ops, NEW_PROXY);

        let mut sink = |_e: DeployEvent| {};
        finalize_one_host(&ops, &cfg, proxy, prep, None, &mut sink)
            .await
            .expect("deploy");

        let calls = ops.calls();
        let legacy_push = position(
            &calls,
            &RecordedCall::ForwardLoopback(relay_host(), LEGACY_HOST_PORT),
        );
        let stop = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::StopContainer(_, n, _) if n == OLD_PROXY))
            .expect("old proxy stopped");
        assert!(legacy_push < stop, "got: {calls:?}");
        assert_eq!(legacy.loads().len(), 1);
        assert_eq!(
            legacy.loads()[0]["admin"],
            serde_json::json!({ "listen": "unix//config/caddy/admin.sock" })
        );
        // The read-back of the first push, then the push for the new
        // proxy: both through the relay.
        assert_eq!(relay.loads().len(), 2);
        assert!(calls.contains(&RecordedCall::Healthcheck(
            relay_host(),
            crate::config::ADMIN_NETWORK.into(),
            crate::config::ADMIN_RELAY_NAME.into(),
            crate::config::ADMIN_RELAY_PORT,
            "/config/".into(),
        )));
    }

    /// The new proxy fails its healthcheck: yoink starts the old proxy
    /// again. Its admin API moved to the socket before the stop, so it
    /// resumes onto the socket, where the relay reaches it.
    #[tokio::test]
    async fn failed_proxy_deploy_restarts_the_old_proxy_after_the_move() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        let cfg = config_proxied_service();
        let proxy = cfg.service(crate::config::PROXY_SERVICE_NAME).unwrap();
        let (legacy, relay) = (FakeCaddy::start(), FakeCaddy::start());
        let (ops, prep) = host_with_an_old_proxy(&legacy, &relay);
        script_push(&ops, OLD_PROXY);
        ops.push_stop_container(Ok(()));
        ops.push_start_container(Ok(()));
        // A daemon rejection ends the probe at once.
        ops.push_healthcheck(Err(DockerError::Bollard {
            host: "host-a".into(),
            source: bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message: "no such image".into(),
            },
        }));
        ops.push_stop_container(Ok(()));
        ops.push_fetch_logs(Ok(Vec::new()));
        ops.push_force_remove(Ok(()));
        ops.push_start_container(Ok(()));

        let mut events = Vec::new();
        let mut sink = |e: DeployEvent| events.push(e);
        let err = finalize_one_host(&ops, &cfg, proxy, prep, None, &mut sink)
            .await
            .expect_err("healthcheck fails");
        assert!(matches!(err, DeployError::Healthcheck { .. }), "got: {err}");

        let calls = ops.calls();
        let legacy_push = position(
            &calls,
            &RecordedCall::ForwardLoopback(relay_host(), LEGACY_HOST_PORT),
        );
        let stop = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::StopContainer(_, n, _) if n == OLD_PROXY))
            .expect("old proxy stopped");
        let removed = position(
            &calls,
            &RecordedCall::ForceRemoveContainer(relay_host(), NEW_PROXY.into()),
        );
        let restarted = calls
            .iter()
            .rposition(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == OLD_PROXY))
            .expect("old proxy restarted");
        assert!(legacy_push < stop && stop < removed && removed < restarted);
        assert_eq!(legacy.loads().len(), 1);
        assert!(events.contains(&DeployEvent::OldContainerRestarted {
            host: "host-a".into(),
            container: OLD_PROXY.into(),
        }));
    }

    /// Caddy rejects the push to the old proxy. Nothing is stopped: the
    /// old proxy keeps serving, with its old config.
    #[tokio::test]
    async fn rejected_move_of_the_old_admin_api_stops_nothing() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        let cfg = config_proxied_service();
        let proxy = cfg.service(crate::config::PROXY_SERVICE_NAME).unwrap();
        let (legacy, relay) = (FakeCaddy::start(), FakeCaddy::start());
        legacy.load_statuses(&[400]);
        let (ops, prep) = host_with_an_old_proxy(&legacy, &relay);
        script_push(&ops, OLD_PROXY);
        ops.push_force_remove(Ok(()));

        let mut sink = |_e: DeployEvent| {};
        let err = finalize_one_host(&ops, &cfg, proxy, prep, None, &mut sink)
            .await
            .expect_err("rejected");
        assert!(err.to_string().contains("status 400"), "got: {err}");
        let calls = ops.calls();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                RecordedCall::StopContainer(..) | RecordedCall::StartContainer(..)
            )),
            "got: {calls:?}"
        );
        assert!(calls.contains(&RecordedCall::ForceRemoveContainer(
            relay_host(),
            NEW_PROXY.into()
        )));
        assert!(relay.loads().is_empty());
    }

    const OLD_API: &str = "api-9f8e7d6";

    /// A deploy of `api` next to the running `OLD_API`, with a running
    /// proxy of this version and the relay on `relay`. The new replica
    /// is healthy. `stop_first` makes `api` publish a host port.
    fn api_deploy_with_relay(
        relay: &crate::proxy::admin::fake_caddy::FakeCaddy,
        stop_first: bool,
    ) -> (FakeDockerOps, Config) {
        let mut cfg = config_proxied_service();
        if stop_first {
            let api = cfg.services.iter().position(|s| s.name == "api").unwrap();
            cfg.services[api].run.publish = vec!["8443:8080".into()];
        }
        let ops = FakeDockerOps::new();
        ops.set_inspect(crate::docker_ops::ContainerDetail {
            name: crate::proxy::relay::RELAY_NAME.into(),
            ports: vec![format!("{RELAY_HOST_PORT}:2020/tcp")],
            ..Default::default()
        });
        ops.forward_loopback_to(RELAY_HOST_PORT, relay.port);
        ops.push_pull_image(Ok(()));
        ops.push_list_containers(Ok(vec![container_with_state(OLD_API, "running")]));
        ops.push_force_remove(Ok(()));
        ops.push_create_container(Ok("id".into()));
        if stop_first {
            ops.push_stop_container(Ok(()));
        }
        ops.push_start_container(Ok(()));
        ops.push_healthcheck(Ok(200));
        // The push of the new replica: the proxy and the relay.
        ops.push_list_containers(Ok(vec![proxy_container("yoink-proxy-x", "running")]));
        ops.push_list_containers(Ok(vec![relay_container()]));
        // The new replica goes, the old one starts again (stop-first).
        ops.push_force_remove(Ok(()));
        if stop_first {
            ops.push_start_container(Ok(()));
        }
        (ops, cfg)
    }

    async fn deploy_api(ops: &FakeDockerOps, cfg: &Config) -> DeployError {
        let mut sink = |_e: DeployEvent| {};
        deploy_service(
            ops,
            cfg,
            cfg.service("api").unwrap(),
            "a1b2c3d",
            &BTreeMap::new(),
            None,
            false,
            &mut sink,
        )
        .await
        .expect_err("the push fails")
    }

    /// The upstreams of `api` in a `/load` body.
    fn api_upstreams(load: &serde_json::Value) -> Vec<String> {
        let mut dials: Vec<String> = Vec::new();
        let text = load.to_string();
        for part in text.split("\"dial\":\"").skip(1) {
            if let Some(dial) = part.split('"').next() {
                dials.push(dial.to_string());
            }
        }
        dials.retain(|d| d.starts_with("api-"));
        dials
    }

    /// Caddy took the `/load` with the new replica, then the route
    /// count did not match. yoink removed the new replica, but Caddy
    /// still routed to it: 502 until the next run. Now yoink pushes
    /// again with the containers that run, for a rolling and for a
    /// stop-first deploy.
    #[tokio::test]
    async fn push_that_may_have_applied_routes_caddy_back_to_the_old_container() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        for stop_first in [false, true] {
            let relay = FakeCaddy::start();
            relay.report_routes_once(0);
            let (ops, cfg) = api_deploy_with_relay(&relay, stop_first);
            // The second push: `api` runs the old container again.
            ops.push_list_containers(Ok(vec![container_with_state(OLD_API, "running")]));
            ops.push_list_containers(Ok(vec![proxy_container("yoink-proxy-x", "running")]));
            ops.push_list_containers(Ok(vec![relay_container()]));

            let err = deploy_api(&ops, &cfg).await;
            assert!(
                err.to_string().contains("route count mismatch"),
                "got: {err}"
            );
            let loads = relay.loads();
            assert_eq!(loads.len(), 2, "stop_first {stop_first}");
            let new_name = created_names(&ops).pop().expect("new replica");
            assert_eq!(api_upstreams(&loads[0]), [format!("{new_name}:8080")]);
            assert_eq!(api_upstreams(&loads[1]), [format!("{OLD_API}:8080")]);
            let calls = ops.calls();
            let removed = calls
                .iter()
                .rposition(
                    |c| matches!(c, RecordedCall::ForceRemoveContainer(_, n) if *n == new_name),
                )
                .expect("new replica removed");
            let second_push = calls
                .iter()
                .rposition(|c| matches!(c, RecordedCall::ForwardLoopback(..)))
                .expect("second push");
            assert!(removed < second_push, "got: {calls:?}");
            if stop_first {
                let restarted = calls
                    .iter()
                    .rposition(|c| matches!(c, RecordedCall::StartContainer(_, n) if n == OLD_API))
                    .expect("old container restarted");
                assert!(restarted < second_push, "got: {calls:?}");
            }
        }
    }

    /// Caddy rejected the `/load`: it kept the old config, which routes
    /// to the old container. No second push.
    #[tokio::test]
    async fn rejected_push_needs_no_second_push() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        for stop_first in [false, true] {
            let relay = FakeCaddy::start();
            relay.load_statuses(&[400]);
            let (ops, cfg) = api_deploy_with_relay(&relay, stop_first);
            let err = deploy_api(&ops, &cfg).await;
            assert!(err.to_string().contains("status 400"), "got: {err}");
            assert_eq!(relay.loads().len(), 1);
        }
    }

    /// The second push fails too: the error names both pushes.
    #[tokio::test]
    async fn failed_second_push_reports_both_errors() {
        use crate::proxy::admin::fake_caddy::FakeCaddy;
        let relay = FakeCaddy::start();
        relay.report_routes_once(0);
        relay.load_statuses(&[200, 400]);
        let (ops, cfg) = api_deploy_with_relay(&relay, false);
        ops.push_list_containers(Ok(vec![container_with_state(OLD_API, "running")]));
        ops.push_list_containers(Ok(vec![proxy_container("yoink-proxy-x", "running")]));
        ops.push_list_containers(Ok(vec![relay_container()]));
        let err = deploy_api(&ops, &cfg).await.to_string();
        assert!(err.contains("route count mismatch"), "got: {err}");
        assert!(
            err.contains("the push that routes it to the running containers failed too"),
            "got: {err}"
        );
        assert!(err.contains("status 400"), "got: {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn services_filter_skips_unselected() {
        let ops = FakeDockerOps::new();
        // Networks are ensured before the per-service loop, so even
        // when the filter excludes everything the host-level setup
        // still runs (plus the pre-flight container snapshot).
        ops.push_ensure_network(Ok(false));
        ops.push_list_containers(Ok(Vec::new()));
        let cfg = config_one_service();
        let mut events: Vec<DeployEvent> = Vec::new();
        let mut sink = |_svc: Option<&str>, e: DeployEvent| events.push(e);
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
