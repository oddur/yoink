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

use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::{Config, ServiceConfig};
use crate::docker;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("service {0:?} has no `build:` block — add one or build the image yourself")]
    NoBuildBlock(String),
    #[error("service {0:?} not found in config")]
    UnknownService(String),
    #[error("`docker build` for {service:?} exited with status {status}")]
    DockerBuildFailed { service: String, status: String },
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

/// Run `docker build` for one service. The image is tagged as
/// `<image>:<tag>` on the operator's local docker daemon. Returns
/// when the build succeeds; surfaces stderr verbatim on failure.
pub async fn build_service(
    config: &Config,
    service: &ServiceConfig,
    tag: &str,
    no_cache: bool,
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
