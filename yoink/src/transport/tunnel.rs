//! Thin wrapper around a child `ssh -N -L <local>:127.0.0.1:<remote>`
//! process. bollard's ssh transport speaks the docker engine API only —
//! arbitrary TCP forwards aren't exposed — so the smallest addition is
//! a child `ssh` process. The tunnel lives for the duration of one
//! image push and is killed via `Drop` (also on panic).

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::process::Stdio;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tracing::warn;

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("failed to bind a local port for ssh tunnel: {0}")]
    Bind(#[source] std::io::Error),
    #[error("failed to spawn `ssh`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("ssh tunnel to {host}:{remote_port} did not become reachable within {timeout:?}")]
    NotReady {
        host: String,
        remote_port: u16,
        timeout: Duration,
    },
    #[error("ssh process exited before the tunnel was ready (status: {status}, stderr: {stderr})")]
    SshExited { status: String, stderr: String },
}

/// A live `ssh -L` tunnel. Drop kills the child unconditionally; the
/// only state held is the child handle and the local port number for
/// the caller to point a docker push at.
pub struct SshTunnel {
    child: Child,
    local_port: u16,
}

impl SshTunnel {
    /// `127.0.0.1:<local_port>` on the operator forwards to
    /// `127.0.0.1:<remote_port>` on `<user>@<host>`. Picks a free local
    /// port via a transient `bind(0)`, then asks the kernel to release
    /// it before handing the number to ssh — there's a short race window
    /// where another process could grab the same port; in practice the
    /// gap is small enough that we accept it (ssh would fail loudly).
    ///
    /// Returns once a TCP connect to the local port succeeds, so the
    /// caller can immediately `docker push` against it.
    pub async fn open(
        user: &str,
        host: &str,
        remote_port: u16,
        ready_timeout: Duration,
        keyfile: Option<&std::path::Path>,
    ) -> Result<Self, TunnelError> {
        Self::open_to(user, host, "127.0.0.1", remote_port, ready_timeout, keyfile).await
    }

    /// Generalized form: forward `local:0` to an arbitrary
    /// `remote_dial_host:remote_port` reachable from the SSH host.
    /// Used by the proxy admin client to dial the Caddy container's
    /// IP on the host's docker bridge (loopback wouldn't reach it).
    pub async fn open_to(
        user: &str,
        host: &str,
        remote_dial_host: &str,
        remote_port: u16,
        ready_timeout: Duration,
        keyfile: Option<&std::path::Path>,
    ) -> Result<Self, TunnelError> {
        Self::open_with_local_port(
            user,
            host,
            remote_dial_host,
            remote_port,
            None,
            ready_timeout,
            keyfile,
        )
        .await
    }

    /// Caller-controlled local port. `None` picks a free one via
    /// `bind(0)`. Used by `yoink pf` so the operator can ask for a
    /// specific laptop-side port (e.g. `pf pgadmin 5050:80` binds
    /// 5050 explicitly so muscle-memory bookmarks stay valid across
    /// sessions); `None` is the implicit-port shortcut and the
    /// shape every other yoink caller uses.
    pub async fn open_with_local_port(
        user: &str,
        host: &str,
        remote_dial_host: &str,
        remote_port: u16,
        local_port: Option<u16>,
        ready_timeout: Duration,
        keyfile: Option<&std::path::Path>,
    ) -> Result<Self, TunnelError> {
        let local_port = match local_port {
            Some(p) => p,
            None => pick_free_port().map_err(TunnelError::Bind)?,
        };

        let mut cmd = Command::new("ssh");
        if let Some(key) = keyfile {
            // Match the key the bollard daemon connection + ssh_probe
            // are using for this host (see `RealDockerOps::ssh_keyfile`).
            // `IdentitiesOnly=yes` stops ssh from trying agent keys
            // first.
            cmd.arg("-i").arg(key).arg("-o").arg("IdentitiesOnly=yes");
        }
        cmd.arg("-N")
            // Fail fast on stale ControlMaster sockets / unreachable host.
            .arg("-o")
            .arg("ConnectTimeout=10")
            // Prevent ssh from prompting on first connection — caller's
            // ssh_probe should have run first to seed known_hosts.
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!("{local_port}:{remote_dial_host}:{remote_port}"))
            .arg(format!("{user}@{host}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Capture stderr so a forwarding error (port already in use,
            // remote refused, etc.) makes it into the eventual error
            // message rather than vanishing into /dev/null.
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(TunnelError::Spawn)?;
        let mut tunnel = Self { child, local_port };

        wait_until_ready(&mut tunnel, host, remote_port, ready_timeout).await?;
        Ok(tunnel)
    }

    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        // `kill_on_drop(true)` already arms tokio to reap on drop, but
        // belt-and-braces: try a synchronous start_kill so we don't
        // depend on the runtime still being alive at drop time.
        if let Err(e) = self.child.start_kill() {
            warn!("failed to kill ssh tunnel child: {e}");
        }
    }
}

/// Bind to `127.0.0.1:0` to let the kernel pick a free port, then drop
/// the listener so ssh can claim the same port. There's a tiny race
/// window between drop and ssh's bind; we accept it.
fn pick_free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn wait_until_ready(
    tunnel: &mut SshTunnel,
    host: &str,
    remote_port: u16,
    timeout: Duration,
) -> Result<(), TunnelError> {
    let deadline = Instant::now() + timeout;
    let local = SocketAddrV4::new(Ipv4Addr::LOCALHOST, tunnel.local_port);
    loop {
        if let Some(status) = tunnel
            .child
            .try_wait()
            .map_err(|e| TunnelError::SshExited {
                status: format!("io error: {e}"),
                stderr: String::new(),
            })?
        {
            // Drain stderr so the failure mode (e.g. "Permission denied
            // (publickey)", "bind: Address already in use") is visible.
            let stderr = drain_stderr(tunnel).await;
            return Err(TunnelError::SshExited {
                status: status.to_string(),
                stderr,
            });
        }
        // TCP-level readiness only: succeeds the instant ssh starts
        // listening locally, which can be BEFORE the remote forward is
        // fully wired (ssh hasn't finished auth yet on a cold tunnel).
        // Callers should do their own end-to-end probe (e.g. an HTTP
        // request) before treating the tunnel as fully usable.
        if TcpStream::connect(local).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(TunnelError::NotReady {
                host: host.to_string(),
                remote_port,
                timeout,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn drain_stderr(tunnel: &mut SshTunnel) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut stderr) = tunnel.child.stderr.take() else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = stderr.read_to_string(&mut buf).await;
    buf.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_free_port_returns_nonzero_distinct_ports() {
        let a = pick_free_port().expect("first bind");
        let b = pick_free_port().expect("second bind");
        assert!(a > 0);
        assert!(b > 0);
        // Not strictly required by the kernel, but in practice
        // back-to-back ephemeral picks differ — this guards against an
        // accidental constant return.
        assert_ne!(a, b);
    }
}
