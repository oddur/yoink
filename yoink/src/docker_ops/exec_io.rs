//! Stdin and output plumbing of a non-TTY `docker exec`, apart from
//! the bollard calls in `real.rs` so it can be tested without a daemon.

use futures_util::StreamExt;

use super::{DockerError, OneShotResult};

/// Drain the attached output of an exec into `(stdout, stderr)`. With
/// `stdin`, a spawned task writes it to `input` at the same time and
/// then closes it, so a process that writes before it reads cannot
/// stall the write. The third value is the result of that write. A
/// failed write does not stop the drain: a process that exits early
/// breaks the pipe, and its own output and exit code explain more.
pub(super) async fn collect_attached<S, W>(
    mut output: S,
    input: W,
    stdin: Option<Vec<u8>>,
) -> Result<(String, String, std::io::Result<()>), bollard::errors::Error>
where
    S: futures_util::Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>>
        + Unpin,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    // Without stdin, keep `input` open until the drain ends, as before.
    let (writer, _idle_input) = match stdin {
        Some(bytes) => {
            let task = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let mut input = input;
                input.write_all(&bytes).await?;
                input.shutdown().await
            });
            (Some(task), None)
        }
        None => (None, Some(input)),
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    while let Some(item) = output.next().await {
        match item {
            Ok(
                bollard::container::LogOutput::StdOut { message }
                | bollard::container::LogOutput::Console { message },
            ) => stdout.push_str(&String::from_utf8_lossy(&message)),
            Ok(bollard::container::LogOutput::StdErr { message }) => {
                stderr.push_str(&String::from_utf8_lossy(&message));
            }
            Ok(bollard::container::LogOutput::StdIn { .. }) => {}
            Err(e) => {
                if let Some(task) = &writer {
                    task.abort();
                }
                return Err(e);
            }
        }
    }
    let written = match writer {
        Some(task) => task.await.unwrap_or_else(|e| Err(std::io::Error::other(e))),
        None => Ok(()),
    };
    Ok((stdout, stderr, written))
}

/// Number of `inspect_exec` calls before an exec that has no exit code
/// is an error. The output stream has ended, so the process is about to
/// exit; this gives it about one second.
const EXIT_CODE_POLLS: u32 = 10;
const EXIT_CODE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The exit code of an exec whose output stream has ended. `inspect`
/// calls `inspect_exec`. While the exec runs or has no exit code, poll
/// it again. If it still has no exit code after [`EXIT_CODE_POLLS`]
/// calls, return an error: an unknown result is not a success.
pub(super) async fn exec_exit_code<F, Fut>(mut inspect: F) -> Result<i64, DockerError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bollard::models::ExecInspectResponse, DockerError>>,
{
    for poll in 1..=EXIT_CODE_POLLS {
        let resp = inspect().await?;
        if resp.running != Some(true)
            && let Some(code) = resp.exit_code
        {
            return Ok(code);
        }
        if poll < EXIT_CODE_POLLS {
            tokio::time::sleep(EXIT_CODE_POLL_INTERVAL).await;
        }
    }
    Err(DockerError::Invalid(
        "exec output ended but the daemon reports no exit code; the result is unknown".into(),
    ))
}

/// Combine the stdin write result with the process result. A process
/// that failed explains itself better than the `Broken pipe` of its
/// stdin, so its result wins. A process that exited 0 without all of
/// its stdin is an error.
pub(super) fn settle_stdin_write(
    written: std::io::Result<()>,
    result: OneShotResult,
) -> Result<OneShotResult, DockerError> {
    match written {
        Ok(()) => Ok(result),
        Err(e) if result.exit_code == 0 => {
            Err(DockerError::Invalid(format!("write exec stdin: {e}")))
        }
        Err(e) => {
            tracing::debug!(error = %e, exit = result.exit_code, "exec stdin write failed; reporting the process result");
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes nothing: every write fails with `BrokenPipe`, as when
    /// the process exits before it reads its stdin.
    struct BrokenPipe;

    impl tokio::io::AsyncWrite for BrokenPipe {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn stderr_frame(text: &str) -> bollard::container::LogOutput {
        bollard::container::LogOutput::StdErr {
            message: bytes::Bytes::copy_from_slice(text.as_bytes()),
        }
    }

    /// The stdin write ran before the drain, and a write error
    /// returned at once. The caller then saw `Broken pipe`, not the
    /// error that the process wrote to stderr.
    #[tokio::test]
    async fn a_broken_stdin_keeps_the_process_output() {
        let output = futures_util::stream::iter(vec![Ok(stderr_frame("Error: bad config\n"))]);
        let (stdout, stderr, written) = collect_attached(output, BrokenPipe, Some(b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(stdout, "");
        assert_eq!(stderr, "Error: bad config\n");
        assert_eq!(written.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn collect_attached_writes_all_of_stdin() {
        let (input, mut peer) = tokio::io::duplex(4);
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut got = Vec::new();
            peer.read_to_end(&mut got).await.unwrap();
            got
        });
        let output = futures_util::stream::iter(Vec::new());
        let (_, _, written) = collect_attached(output, input, Some(b"0123456789".to_vec()))
            .await
            .unwrap();
        written.unwrap();
        assert_eq!(reader.await.unwrap(), b"0123456789");
    }

    fn inspected(
        running: Option<bool>,
        exit_code: Option<i64>,
    ) -> bollard::models::ExecInspectResponse {
        bollard::models::ExecInspectResponse {
            running,
            exit_code,
            ..Default::default()
        }
    }

    /// `exit_code: None` became exit 0, so a `caddy reload` with an
    /// unknown result counted as a successful push.
    #[tokio::test(start_paused = true)]
    async fn an_exec_with_no_exit_code_is_an_error_not_exit_0() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let err = exec_exit_code(|| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(inspected(Some(true), None)) }
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no exit code"), "got {err}");
        assert_eq!(calls.into_inner(), EXIT_CODE_POLLS);

        let err = exec_exit_code(|| async { Ok(inspected(Some(false), None)) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no exit code"), "got {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn exec_exit_code_waits_until_the_exec_stops() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let code = exec_exit_code(|| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(if n < 2 {
                    inspected(Some(true), None)
                } else {
                    inspected(Some(false), Some(3))
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(code, 3);
        assert_eq!(calls.into_inner(), 3);
    }

    fn result(exit_code: i64) -> OneShotResult {
        OneShotResult {
            exit_code,
            stdout: String::new(),
            stderr: "Error: bad config".into(),
        }
    }

    #[test]
    fn a_failed_process_wins_over_a_broken_stdin() {
        let broken = || Err(std::io::ErrorKind::BrokenPipe.into());
        let r = settle_stdin_write(broken(), result(1)).unwrap();
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.stderr, "Error: bad config");
        // Exit 0 without all of stdin is not a success.
        let err = settle_stdin_write(broken(), result(0)).unwrap_err();
        assert!(err.to_string().contains("write exec stdin"), "got {err}");
        assert_eq!(settle_stdin_write(Ok(()), result(0)).unwrap().exit_code, 0);
    }
}
