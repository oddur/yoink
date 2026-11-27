//! bollard ssh transport for hosts that set `port:`.
//!
//! bollard's own ssh connector (0.20.x and 0.21.1) hands the URL
//! authority to ssh as the destination. For `ssh://deploy@host:2200`
//! that is `deploy@host:2200`, and ssh reads `host:2200` as a
//! hostname: "Could not resolve hostname". The fix is on bollard's
//! main branch but not in a release, so `Docker::connect_with_ssh`
//! cannot reach a non-default port.
//!
//! This module is the same connector with one change: it sets user
//! and port on the `openssh` session builder from the [`Host`] and
//! passes ssh the bare address. `RealDockerOps` uses it only when the
//! host has a port; hosts without one stay on `connect_with_ssh`.
//! Delete this module once a bollard release parses the port.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bollard::{BollardRequest, Docker};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::host::Host;

/// Connect to `host`'s docker daemon over `ssh -p <port>`. Same
/// contract as `Docker::connect_with_ssh`: lazy, the first request
/// opens the ssh master and runs `docker system dial-stdio`.
pub fn connect(
    host: &Host,
    port: u16,
    keyfile: Option<String>,
    timeout_secs: u64,
) -> Result<Docker, bollard::errors::Error> {
    let connector = SshConnector {
        user: host.user.clone(),
        address: host.address.clone(),
        port,
        keyfile,
    };
    let client = Client::builder(TokioExecutor::new()).build(connector);
    Docker::connect_with_custom_transport(
        move |req: BollardRequest| {
            let client = client.clone();
            async move {
                client
                    .request(req)
                    .await
                    .map_err(bollard::errors::Error::from)
            }
        },
        // Only the hyper pool key and bollard's request URLs read
        // this; the connector dials from its own fields.
        Some(host.ssh_url()),
        timeout_secs,
        bollard::API_DEFAULT_VERSION,
    )
}

#[derive(Clone)]
struct SshConnector {
    user: String,
    address: String,
    port: u16,
    keyfile: Option<String>,
}

struct SshStream {
    _child: openssh::Child<Arc<openssh::Session>>,
    stdin: Option<TokioIo<openssh::ChildStdin>>,
    stdout: TokioIo<openssh::ChildStdout>,
}

impl tower_service::Service<hyper::Uri> for SshConnector {
    type Response = SshStream;
    type Error = openssh::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _destination: hyper::Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let mut builder = openssh::SessionBuilder::default();
            builder.user(this.user).port(this.port);
            if let Some(keyfile) = this.keyfile {
                builder.keyfile(keyfile);
            }
            let tempdir = builder.launch_master(&this.address).await?;
            let session = Arc::new(openssh::Session::new_process_mux(tempdir));

            let mut child = session
                .arc_command("docker")
                .arg("system")
                .arg("dial-stdio")
                .stdin(openssh::Stdio::piped())
                .stdout(openssh::Stdio::piped())
                .spawn()
                .await?;

            let stdin = child.stdin().take().map(TokioIo::new);
            let stdout = child
                .stdout()
                .take()
                .map(TokioIo::new)
                .ok_or_else(|| openssh::Error::Connect(io::ErrorKind::BrokenPipe.into()))?;
            Ok(SshStream {
                stdin,
                stdout,
                _child: child,
            })
        })
    }
}

impl SshStream {
    fn stdin(self: Pin<&mut Self>) -> io::Result<Pin<&mut TokioIo<openssh::ChildStdin>>> {
        self.get_mut()
            .stdin
            .as_mut()
            .map(Pin::new)
            .ok_or_else(|| io::ErrorKind::BrokenPipe.into())
    }
}

impl hyper::rt::Read for SshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for SshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.stdin()?.poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.stdin()?.poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stdin()?.poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // The child's stdin shutdown is a no-op in tokio, so drop it
        // to close the input half.
        ready!(self.as_mut().stdin()?.poll_shutdown(cx))?;
        drop(self.get_mut().stdin.take());
        Poll::Ready(Ok(()))
    }
}

impl Connection for SshStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No live host needed: nothing listens on port 1, so ssh fails
    /// fast, and its error names the port it dialed. That shows the
    /// request went through this connector and that `-p` reached ssh.
    #[tokio::test]
    async fn request_dials_the_configured_port() {
        if std::process::Command::new("ssh")
            .arg("-V")
            .output()
            .is_err()
        {
            return; // no ssh client on this machine
        }
        let host = Host {
            user: "nobody".into(),
            address: "127.0.0.1".into(),
            port: Some(1),
        };
        let docker = connect(&host, 1, None, 20).expect("lazy connect");
        let err = docker.version().await.expect_err("port 1 is closed");
        let msg = format!("{err:?}");
        assert!(msg.contains("port 1"), "got: {msg}");
    }
}
