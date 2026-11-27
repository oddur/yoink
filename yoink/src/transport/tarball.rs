//! The tarball transport: `docker save` on the operator's machine,
//! streamed into the host's daemon through `POST /images/load`. Whole
//! image every time; needs nothing on the host but docker.

use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
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
    /// The load error prints as the source.
    #[error("`docker save` for {image:?} exited with status {status}:\n{stderr}")]
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
    if let Err(load) = ops.load_image(host, body).await {
        return Err(load_failure(child, stderr_task, image_ref, load).await);
    }

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

/// How long a failed load waits for `docker save` to exit. A save
/// that failed on its own has exited already (its exit closed the
/// pipe, and the load failed on the short tar). A save that is still
/// writing gets killed: the load error is the cause.
const SAVE_EXIT_GRACE: Duration = Duration::from_millis(500);

/// The error for a failed load. It names the save only when the save
/// failed on its own: a non-zero exit with stderr that is not a
/// consequence of the closed pipe. Otherwise the load error returns.
async fn load_failure(
    mut child: Child,
    stderr_task: JoinHandle<String>,
    image_ref: &str,
    load: DockerError,
) -> TarballError {
    let Ok(Ok(status)) = tokio::time::timeout(SAVE_EXIT_GRACE, child.wait()).await else {
        // The save still runs (or its status is unknown): it can block
        // on a pipe that never closes, so do not wait on it.
        let _ = child.start_kill();
        stderr_task.abort();
        return TarballError::Docker(load);
    };
    let stderr = stderr_task.await.unwrap_or_default().trim().to_string();
    if status.success() || stderr.is_empty() || died_on_closed_pipe(status, &stderr) {
        return TarballError::Docker(load);
    }
    TarballError::SaveAndLoadFailed {
        image: image_ref.to_string(),
        status: status.to_string(),
        stderr,
        load,
    }
}

/// True when the save exit is a consequence of the load closing the
/// pipe: SIGPIPE, or a "broken pipe" write error.
fn died_on_closed_pipe(status: ExitStatus, stderr: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(libc::SIGPIPE) {
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = status;
    stderr.to_ascii_lowercase().contains("broken pipe")
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

    /// Regression: the load failed on its own and closed the pipe.
    /// `docker save` died on the broken pipe, and the error blamed the
    /// save. The load error must return.
    #[tokio::test]
    async fn load_failure_returns_the_load_error_when_the_save_dies_on_the_pipe() {
        let mut save = Command::new("sh");
        save.arg("-c").arg("exec cat /dev/zero");

        let ops = FakeDockerOps::new();
        let host = Host::local();
        let err = save_and_load_with(save, "app:v1", &ops, &host, |_| ())
            .await
            .unwrap_err();

        assert!(matches!(err, TarballError::Docker(_)), "{err:?}");
    }

    /// A save that never exits (it blocks on a pipe that stays open)
    /// gets killed: the load error returns without a wait on the save.
    #[tokio::test]
    async fn load_failure_kills_a_save_that_does_not_exit() {
        let mut save = Command::new("sh");
        save.arg("-c").arg("exec sleep 30");

        let ops = FakeDockerOps::new();
        let host = Host::local();
        let started = std::time::Instant::now();
        let err = save_and_load_with(save, "app:v1", &ops, &host, |_| ())
            .await
            .unwrap_err();

        assert!(matches!(err, TarballError::Docker(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// The load error prints once: as the source, not in the message.
    #[test]
    fn save_and_load_failed_prints_the_load_error_once() {
        let err = anyhow::Error::from(TarballError::SaveAndLoadFailed {
            image: "app:v1".into(),
            status: "exit status: 1".into(),
            stderr: "No such image".into(),
            load: DockerError::Invalid("short tar".into()),
        });
        let text = format!("{err:#}");
        assert_eq!(text.matches("short tar").count(), 1, "{text}");
    }
}
