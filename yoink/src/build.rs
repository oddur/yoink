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

use anyhow::Context as _;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::{Config, ServiceConfig};
use crate::docker;
use crate::docker_ops::{DockerOps, Host};

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
    #[error("`docker save` for {image:?} exited with status {status}")]
    DockerSaveFailed { image: String, status: String },
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read `docker save` output: {0}")]
    ReadSave(#[source] std::io::Error),
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

/// Spawn `docker save <image_ref>` against the operator's local
/// docker daemon and slurp the resulting tarball into memory. The
/// caller hands this byte buffer to `DockerOps::load_image` per host
/// — bollard's `import_image` body has to be a single Bytes today, so
/// streaming is a future optimization (see `bollard::body_stream`).
pub async fn save_image_locally(image_ref: &str) -> Result<bytes::Bytes, BuildError> {
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
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout should always be available");
    let mut buf: Vec<u8> = Vec::new();
    stdout
        .read_to_end(&mut buf)
        .await
        .map_err(BuildError::ReadSave)?;
    let status = child.wait().await.map_err(|source| BuildError::Spawn {
        program: "docker".into(),
        source,
    })?;
    if !status.success() {
        return Err(BuildError::DockerSaveFailed {
            image: image_ref.to_string(),
            status: status.to_string(),
        });
    }
    Ok(bytes::Bytes::from(buf))
}

/// Pre-flight for `yoink up --no-registry`: for every selected
/// service × applicable host, `docker save` the local image and
/// stream it into the host's docker daemon. After this returns, the
/// host has the image cached and the reconcile's `image_present`
/// check short-circuits the would-be-pull.
///
/// Per-image: one local `docker save` (deduped via the `BTreeMap`),
/// then a parallel `try_join_all` of `load_image` calls fan-out to
/// every applicable host. Sequential per-image (one save in flight
/// at a time) keeps the operator's local docker daemon from racing
/// itself.
pub async fn load_images_to_hosts(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &BTreeMap<String, String>,
    services_filter: Option<&[String]>,
) -> anyhow::Result<()> {
    // image_ref → applicable HostConfigs. BTreeMap keys dedupe shared
    // images; the inner Vec keeps `&HostConfig` so `Host::from` does
    // the right thing without re-scanning by address.
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

    for (image_ref, host_cfgs) in &by_image {
        eprintln!(
            "yoink up --no-registry: docker save {image_ref} → {} host(s)",
            host_cfgs.len()
        );
        let tar = save_image_locally(image_ref)
            .await
            .with_context(|| format!("docker save {image_ref}"))?;
        let loads = host_cfgs.iter().map(|host_cfg| {
            let host = Host::from(*host_cfg);
            let tar = tar.clone();
            async move {
                ops.load_image(&host, tar)
                    .await
                    .with_context(|| format!("docker load on {}", host.address))
            }
        });
        futures_util::future::try_join_all(loads).await?;
    }
    Ok(())
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
