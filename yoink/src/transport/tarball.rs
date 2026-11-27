//! The tarball transport: `docker save` on the operator's machine,
//! streamed into the host's daemon through `POST /images/load`. Whole
//! image every time; needs nothing on the host but docker.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use thiserror::Error;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::base::host::Host;
use crate::docker_ops::{DockerError, DockerOps, ImageTarStream};

#[derive(Debug, Error)]
pub enum TarballError {
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
) -> Result<u64, TarballError> {
    let mut child = Command::new("docker")
        .arg("save")
        .arg(image_ref)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| TarballError::Spawn {
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

    let status = child.wait().await.map_err(|source| TarballError::Spawn {
        program: "docker".into(),
        source,
    })?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(TarballError::DockerSaveFailed {
            image: image_ref.to_string(),
            status: status.to_string(),
            stderr: stderr_text.trim().to_string(),
        });
    }
    Ok(total.load(Ordering::Relaxed))
}
