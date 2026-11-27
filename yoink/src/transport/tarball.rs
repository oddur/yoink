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
    /// `docker save` failed and the load on the host failed with it
    /// (it got a short or empty tar). The save stderr names the cause.
    #[error("`docker save` for {image:?} exited with status {status}{stderr}\nthe load on the host failed too: {load}", stderr = if .stderr.is_empty() { String::new() } else { format!(":\n{}", .stderr) })]
    SaveAndLoadFailed {
        image: String,
        status: String,
        stderr: String,
        #[source]
        load: DockerError,
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
    let mut save = Command::new("docker");
    save.arg("save").arg(image_ref);
    save_and_load_with(save, image_ref, ops, host, on_progress).await
}

/// [`save_and_load_to_host`] with the `docker save` command as a
/// parameter, so tests can run a stand-in.
async fn save_and_load_with(
    mut save: Command,
    image_ref: &str,
    ops: &dyn DockerOps,
    host: &Host,
    on_progress: impl FnMut(u64) + Send + 'static,
) -> Result<u64, TarballError> {
    let mut child = save
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
    // Wait for `docker save` also when the load fails: a failed save
    // makes the load fail on a short tar, and only the save stderr
    // says why (e.g. "No such image").
    let load_result = ops.load_image(host, body).await;

    let status = child.wait().await.map_err(|source| TarballError::Spawn {
        program: "docker".into(),
        source,
    })?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let image = image_ref.to_string();
        let status = status.to_string();
        let stderr = stderr_text.trim().to_string();
        return Err(match load_result {
            Ok(()) => TarballError::DockerSaveFailed {
                image,
                status,
                stderr,
            },
            Err(load) => TarballError::SaveAndLoadFailed {
                image,
                status,
                stderr,
                load,
            },
        });
    }
    load_result?;
    Ok(total.load(Ordering::Relaxed))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;

    /// Regression: when `docker save` failed, the load error returned
    /// first and the save stderr was lost. The user saw only a generic
    /// load error.
    #[tokio::test]
    async fn failed_save_surfaces_its_stderr_when_the_load_fails() {
        let mut save = Command::new("sh");
        save.arg("-c")
            .arg("echo 'Error response from daemon: reference does not exist' >&2; exit 1");

        // No scripted load result: the fake load fails, as a real
        // daemon does on an empty tar.
        let ops = FakeDockerOps::new();
        let host = Host::local();
        let err = save_and_load_with(save, "app:v1", &ops, &host, |_| ())
            .await
            .unwrap_err();

        assert!(
            matches!(err, TarballError::SaveAndLoadFailed { .. }),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("reference does not exist"),
            "{err}"
        );
    }
}
