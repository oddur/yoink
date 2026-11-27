//! Local image build + deliver-without-registry pipeline.
//!
//! `yoink build` shells out to `docker build` against the operator's
//! local docker daemon, producing a tagged image on the operator's
//! machine. Pair it with `yoink up --no-registry` to ship that image
//! to each host via `docker save | docker load` (over the existing
//! ssh+bollard transport) — no registry needed.
//!
//! This is the "I just want to get this thing running on a host"
//! path: no CI, no registry, no auth dance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use thiserror::Error;
use tokio::process::Command;

use crate::base::host::Host;
use crate::base::image;
use crate::config::{Config, ServiceConfig};
use crate::docker_ops::{DockerError, DockerOps};
use crate::features::output::format_bytes;
use crate::transport::{
    Transport, tarball,
    unregistry::{self, SIDECAR_LABEL, SIDECAR_LABEL_VALUE},
};

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("service {0:?} has no `build:` block — add one or build the image yourself")]
    NoBuildBlock(String),
    #[error("service {0:?} not found in config")]
    UnknownService(String),
    #[error("`docker build` for {service:?} exited with status {status}")]
    DockerBuildFailed { service: String, status: String },
    #[error(
        "`docker push` for {service:?} exited with status {status} \
         (is the operator logged into the registry? `docker login <registry>`)"
    )]
    DockerPushFailed { service: String, status: String },
    #[error(transparent)]
    Docker(#[from] DockerError),
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Resolve a path declared in the config (`build.context`,
/// `build.dockerfile`) relative to the directory the yoink config
/// file lives in. Same convention as `files:` mounts and `include:`.
fn resolve_relative_to_config(config: &Config, p: &str) -> PathBuf {
    let raw = Path::new(p);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let base = config
            .config_dir
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        base.join(raw)
    }
}

/// Resolve a service's effective tag for a build/deploy: a `--tag`
/// override wins, otherwise the config-pinned `tag:`. Errors with a
/// helpful message if neither is present.
pub fn resolve_service_tag(
    svc: &ServiceConfig,
    overrides: &BTreeMap<String, String>,
) -> anyhow::Result<String> {
    overrides
        .get(&svc.name)
        .cloned()
        .or_else(|| svc.tag.clone())
        .with_context(|| {
            format!(
                "service {:?} has no `tag:` set and no `--tag {}=…` override",
                svc.name, svc.name
            )
        })
}

/// Return whether a tag can be resolved without building first.
#[must_use]
pub fn has_tag(svc: &ServiceConfig, overrides: &BTreeMap<String, String>) -> bool {
    overrides.contains_key(&svc.name) || svc.tag.is_some()
}

/// Build a service and return the tag derived from the resulting image's
/// content digest. Uses `--iidfile` to capture the image ID that Docker
/// computes, extracts the first 12 hex chars of the SHA256, and retags
/// the image under that short digest. The tag is deterministic: the same
/// Dockerfile + context produces the same hash, so unchanged images cause
/// no `spec_hash` diff and yoink skips the container restart automatically.
pub async fn build_and_capture_tag(
    config: &Config,
    service: &ServiceConfig,
    no_cache: bool,
) -> Result<String, BuildError> {
    let build = service
        .build
        .as_ref()
        .ok_or_else(|| BuildError::NoBuildBlock(service.name.clone()))?;
    let context_path = resolve_relative_to_config(config, &build.context);

    let iidfile = tempfile::Builder::new()
        .prefix("yoink-iid-")
        .suffix(".txt")
        .tempfile()
        .map_err(|source| BuildError::Spawn {
            program: "tempfile".into(),
            source,
        })?;

    // Probe tag used only long enough to retag with the digest below.
    let probe_ref = image::image_reference(&service.image, "yoink-build-probe");
    let mut cmd = Command::new("docker");
    cmd.arg("build")
        .arg("--tag")
        .arg(&probe_ref)
        .arg("--iidfile")
        .arg(iidfile.path());

    if let Some(df) = &build.dockerfile {
        cmd.arg("--file")
            .arg(resolve_relative_to_config(config, df));
    }
    for (k, v) in &build.args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    if let Some(target) = &build.target {
        cmd.arg("--target").arg(target);
    }
    if no_cache {
        cmd.arg("--no-cache");
    }
    for extra in &build.extra_args {
        cmd.arg(extra);
    }
    cmd.arg(&context_path);

    eprintln!(
        "yoink build {}: docker build (auto-tag from digest, context: {})",
        service.name,
        context_path.display()
    );

    let status = cmd.status().await.map_err(|source| BuildError::Spawn {
        program: "docker".into(),
        source,
    })?;
    if !status.success() {
        return Err(BuildError::DockerBuildFailed {
            service: service.name.clone(),
            status: status.to_string(),
        });
    }

    // Docker writes "sha256:<hex>" into the iidfile.
    let iid = std::fs::read_to_string(iidfile.path()).map_err(|source| BuildError::Spawn {
        program: "iidfile".into(),
        source,
    })?;
    let short_tag: String = iid
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(iid.trim())
        .chars()
        .take(12)
        .collect();

    let final_ref = image::image_reference(&service.image, &short_tag);
    let tag_status = Command::new("docker")
        .args(["tag", &probe_ref, &final_ref])
        .status()
        .await
        .map_err(|source| BuildError::Spawn {
            program: "docker".into(),
            source,
        })?;
    if !tag_status.success() {
        return Err(BuildError::DockerBuildFailed {
            service: service.name.clone(),
            status: tag_status.to_string(),
        });
    }

    eprintln!("yoink build {}: tagged → {final_ref}", service.name);
    Ok(short_tag)
}

/// Build one service, tagging the result on the operator's local
/// daemon as `<image>:<tag>`. When `push` is `true`, follow the
/// build with `docker push <image>:<tag>` so the image lands in a
/// real registry — that's the kamal-style "build locally, deploy
/// from registry" loop. Stderr surfaces verbatim on failure.
pub async fn build_service(
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    no_cache: bool,
    push: bool,
) -> Result<(), BuildError> {
    let build = service
        .build
        .as_ref()
        .ok_or_else(|| BuildError::NoBuildBlock(service.name.clone()))?;
    let image_ref = image::image_reference(&service.image, tag);
    let context_path = resolve_relative_to_config(config, &build.context);

    let mut cmd = Command::new("docker");
    cmd.arg("build").arg("--tag").arg(&image_ref);

    if let Some(df) = &build.dockerfile {
        let df_path = resolve_relative_to_config(config, df);
        cmd.arg("--file").arg(df_path);
    }
    for (k, v) in &build.args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    if let Some(target) = &build.target {
        cmd.arg("--target").arg(target);
    }
    if no_cache {
        cmd.arg("--no-cache");
    }
    for extra in &build.extra_args {
        cmd.arg(extra);
    }
    cmd.arg(&context_path);

    eprintln!(
        "yoink build {}: docker build → {image_ref} (context: {})",
        service.name,
        context_path.display()
    );

    let status = cmd.status().await.map_err(|source| BuildError::Spawn {
        program: "docker".into(),
        source,
    })?;
    if !status.success() {
        return Err(BuildError::DockerBuildFailed {
            service: service.name.clone(),
            status: status.to_string(),
        });
    }

    if push {
        eprintln!("yoink build {}: docker push {image_ref}", service.name);
        let push_status = Command::new("docker")
            .arg("push")
            .arg(&image_ref)
            .status()
            .await
            .map_err(|source| BuildError::Spawn {
                program: "docker".into(),
                source,
            })?;
        if !push_status.success() {
            return Err(BuildError::DockerPushFailed {
                service: service.name.clone(),
                status: push_status.to_string(),
            });
        }
    }
    Ok(())
}

/// Pre-flight for any deploy that has locally-built images: for every
/// selected service × applicable host, stream `docker save` from the
/// operator's local daemon into the host's daemon via `import_image`.
/// Memory stays bounded regardless of image size (`ReaderStream`
/// chunk size, 8 KiB).
///
/// `only_buildable` controls which services participate:
/// - `true` (the default for plain `yoink up`): only services with a
///   `build:` block are shipped — their image:tag exists only in the
///   operator's local daemon, so this is the only way to reach the
///   host. Services without `build:` are pulled normally by the host
///   from their registry.
/// - `false` (the `--no-registry` mode): every selected service is
///   shipped from local, regardless of whether it has a `build:`
///   block. Used for offline / airgapped workflows where the operator
///   wants to bypass all registry pulls. The operator must have every
///   image available locally (`docker pull` ahead of time, or pre-
///   loaded tarballs).
///
/// Per-image: parallel fan-out across all applicable hosts via
/// `try_join_all`. Each host gets its own `docker save` process
/// (so streams are independent) and its own progress bar. Multiple
/// images run sequentially so the operator's local docker daemon
/// isn't fork-bombed.
pub async fn load_images_to_hosts(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    transport: Transport,
    only_buildable: bool,
) -> anyhow::Result<()> {
    let mut by_image: BTreeMap<String, Vec<Host>> = BTreeMap::new();
    for svc in config.selected_services(services_filter) {
        if only_buildable && svc.build.is_none() {
            continue;
        }
        let tag = resolve_service_tag(svc, tag_overrides)?;
        let image_ref = image::image_reference(&svc.image, &tag);
        let entry = by_image.entry(image_ref).or_default();
        for host in config.fleet_for(svc) {
            if !entry.iter().any(|h| h.address == host.address) {
                entry.push(host);
            }
        }
    }
    if by_image.is_empty() {
        return Ok(());
    }

    // Best-effort sweep of leaked unregistry sidecars from previous
    // crashed deploys. Skipped for the tarball transport since it can't
    // create them. Errors here are logged-and-ignored — sweeping is a
    // hygiene step, not a correctness gate.
    if !matches!(transport, Transport::Tarball) {
        let unique_hosts: BTreeMap<String, &Host> = by_image
            .values()
            .flatten()
            .map(|h| (h.address.clone(), h))
            .collect();
        for host in unique_hosts.values() {
            sweep_leaked_sidecars(ops, host).await;
        }
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    for (image_ref, hosts) in &by_image {
        // Hide the progress draw target on non-TTY (CI logs); the
        // steady-tick wakeups still happen but indicatif batches them
        // off-screen so it doesn't pollute the captured output.
        let multi = if interactive {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };
        let style = ProgressStyle::with_template(
            "{spinner:.green} {prefix:<24} {bytes:>10} @ {bytes_per_sec:>10}  {wide_msg}",
        )
        .expect("static progress template parses");

        let loads = hosts.iter().map(|host| {
            let host = host.clone();
            let bar = multi.add(ProgressBar::new_spinner().with_style(style.clone()));
            bar.set_prefix(format!("{} → {}", short_image(image_ref), host.address));
            if interactive {
                bar.enable_steady_tick(std::time::Duration::from_millis(100));
            }
            let bar_for_progress = bar.clone();
            let image_ref = image_ref.clone();
            async move {
                deliver_image(transport, ops, &host, &image_ref, &bar, move |n| {
                    bar_for_progress.inc(n);
                })
                .await
                .with_context(|| format!("deliver {image_ref} → {}", host.address))?;
                anyhow::Ok(())
            }
        });
        futures_util::future::try_join_all(loads).await?;
    }
    Ok(())
}

/// Per-host dispatch for one image. Returns the bytes-on-wire for the
/// tarball path (used to drive the progress bar's "done · N MiB" final
/// label); the unregistry path defers to `docker push`'s own progress
/// rendering on stderr — bytes-on-wire isn't easily harvested without
/// parsing, and the registry-protocol dedup means the metric is less
/// interesting than for tarball anyway.
async fn deliver_image(
    transport: Transport,
    ops: &dyn DockerOps,
    host: &Host,
    image_ref: &str,
    bar: &ProgressBar,
    on_progress: impl FnMut(u64) + Send + 'static,
) -> anyhow::Result<()> {
    match transport {
        Transport::Tarball => {
            let total = tarball::save_and_load_to_host(image_ref, ops, host, on_progress).await?;
            let signed = i64::try_from(total).unwrap_or(i64::MAX);
            bar.finish_with_message(format!("done · {} (tarball)", format_bytes(signed)));
            Ok(())
        }
        Transport::Unregistry => {
            unregistry::push(ops, host, image_ref).await?;
            bar.finish_with_message("done · unregistry");
            Ok(())
        }
        Transport::Auto => match unregistry::push(ops, host, image_ref).await {
            Ok(()) => {
                bar.finish_with_message("done · unregistry");
                Ok(())
            }
            Err(e) => {
                eprintln!(
                    "warn: unregistry transport failed for {image_ref} → {}: {e}; \
                     falling back to tarball",
                    host.address,
                );
                let total =
                    tarball::save_and_load_to_host(image_ref, ops, host, on_progress).await?;
                let signed = i64::try_from(total).unwrap_or(i64::MAX);
                bar.finish_with_message(format!(
                    "done · {} (tarball fallback)",
                    format_bytes(signed)
                ));
                Ok(())
            }
        },
    }
}

/// Force-remove every container labeled
/// `yoink.role=unregistry-ephemeral` on `host`. Catches sidecars
/// leaked by a prior `yoink up` that crashed mid-push. Errors are
/// logged-and-swallowed — this is hygiene, not a gate. The label
/// scope is narrow enough that we don't bother filtering by age.
async fn sweep_leaked_sidecars(ops: &dyn DockerOps, host: &Host) {
    let label = format!("{SIDECAR_LABEL}={SIDECAR_LABEL_VALUE}");
    let containers = match ops.list_containers_by_label(host, &label).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                "sweep: list_containers_by_label({label}) failed on {}: {e}",
                host.address
            );
            return;
        }
    };
    for c in containers {
        if let Err(e) = ops.force_remove_container(host, &c.name).await {
            tracing::warn!(
                "sweep: failed to remove leaked unregistry sidecar {} on {}: {e}",
                c.name,
                host.address
            );
        }
    }
}

/// Trim a registry-prefixed image to its short form for progress
/// labels (`ghcr.io/oddur/api:dev` → `api:dev`, `bt-api:abc` →
/// `bt-api:abc`). Keeps the prefix-or-tag axis the operator cares
/// about without eating the whole terminal width.
fn short_image(image_ref: &str) -> String {
    image_ref
        .rsplit_once('/')
        .map_or_else(|| image_ref.to_string(), |(_, tail)| tail.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_for_service_without_build_block_is_clear() {
        // Smoke: error message includes the service name so an
        // operator can grep their config directly.
        let e = BuildError::NoBuildBlock("api".into());
        assert!(format!("{e}").contains("\"api\""));
    }
}
