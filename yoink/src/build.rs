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
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use thiserror::Error;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::config::{Config, ServiceConfig};
use crate::docker;
use crate::docker_ops::{DockerError, DockerOps, Host, ImageTarStream};
use crate::output::format_bytes;
use crate::transport::{
    Transport,
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
    #[error("`docker save` for {image:?} exited with status {status}{stderr}", stderr = if .stderr.is_empty() { String::new() } else { format!(":\n{}", .stderr) })]
    DockerSaveFailed {
        image: String,
        status: String,
        stderr: String,
    },
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
    let image_ref = docker::image_reference(&service.image, tag);
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

/// Stream `docker save <image_ref>` from the operator's local docker
/// daemon directly into the host's docker daemon via bollard's
/// `import_image` (`POST /images/load`). Memory stays bounded by
/// `ReaderStream`'s chunk size (8 KiB by default) regardless of how
/// large the image is — a 2GB Java/Node app costs 8 KiB of buffer,
/// not 2GB of RAM, on the operator's machine.
///
/// `on_progress` is called with each chunk's byte count as it flows
/// through the pipe — wire it to a progress bar (CLI) or an event
/// channel (TUI). Use `|_| ()` for silent transfers.
///
/// Multi-host: each call spawns its own `docker save` process and
/// holds its own client connection to the host. Run multiple calls
/// concurrently via `try_join_all` for full parallel fan-out.
pub async fn save_and_load_to_host(
    image_ref: &str,
    ops: &dyn DockerOps,
    host: &Host,
    on_progress: impl FnMut(u64) + Send + 'static,
) -> Result<u64, BuildError> {
    let mut child = Command::new("docker")
        .arg("save")
        .arg(image_ref)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| BuildError::Spawn {
            program: "docker".into(),
            source,
        })?;
    let stdout = child
        .stdout
        .take()
        .expect("piped stdout should always be available");
    // Drain stderr concurrently into a buffer — surfaces "no such image"
    // and similar diagnostics in the failure path; otherwise the caller
    // only sees the exit code.
    let stderr = child
        .stderr
        .take()
        .expect("piped stderr should always be available");
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        let _ = tokio::io::BufReader::new(stderr)
            .read_to_string(&mut buf)
            .await;
        buf
    });

    let total = Arc::new(AtomicU64::new(0));
    let total_clone = Arc::clone(&total);
    let mut on_progress = on_progress;
    let stream = ReaderStream::new(stdout).inspect(move |chunk| {
        if let Ok(bytes) = chunk {
            let n = bytes.len() as u64;
            total_clone.fetch_add(n, Ordering::Relaxed);
            on_progress(n);
        }
    });
    let body: ImageTarStream = Box::pin(stream);
    ops.load_image(host, body).await?;

    let status = child.wait().await.map_err(|source| BuildError::Spawn {
        program: "docker".into(),
        source,
    })?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(BuildError::DockerSaveFailed {
            image: image_ref.to_string(),
            status: status.to_string(),
            stderr: stderr_text.trim().to_string(),
        });
    }
    Ok(total.load(Ordering::Relaxed))
}

/// Pre-flight for `yoink up --no-registry`: for every selected
/// service × applicable host, stream `docker save` from the
/// operator's local daemon into the host's daemon via
/// `import_image`. Memory stays bounded regardless of image size
/// (`ReaderStream` chunk size, 8 KiB).
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
) -> anyhow::Result<()> {
    let mut by_image: BTreeMap<String, Vec<&crate::config::HostConfig>> = BTreeMap::new();
    for svc in config.selected_services(services_filter) {
        let tag = resolve_service_tag(svc, tag_overrides)?;
        let image_ref = docker::image_reference(&svc.image, &tag);
        let entry = by_image.entry(image_ref).or_default();
        for host_cfg in svc.applicable_hosts(&config.hosts) {
            if !entry.iter().any(|h| h.address == host_cfg.address) {
                entry.push(host_cfg);
            }
        }
    }

    // Best-effort sweep of leaked unregistry sidecars from previous
    // crashed deploys. Skipped for the tarball transport since it can't
    // create them. Errors here are logged-and-ignored — sweeping is a
    // hygiene step, not a correctness gate.
    if !matches!(transport, Transport::Tarball) {
        let unique_hosts: BTreeMap<String, &crate::config::HostConfig> = by_image
            .values()
            .flatten()
            .map(|h| (h.address.clone(), *h))
            .collect();
        for host_cfg in unique_hosts.values() {
            sweep_leaked_sidecars(ops, &Host::from(*host_cfg)).await;
        }
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    for (image_ref, host_cfgs) in &by_image {
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

        let loads = host_cfgs.iter().map(|host_cfg| {
            let host = Host::from(*host_cfg);
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
            let total = save_and_load_to_host(image_ref, ops, host, on_progress).await?;
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
                let total = save_and_load_to_host(image_ref, ops, host, on_progress).await?;
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
