//! Transports for `yoink up --no-registry`: how to ship a locally-built
//! image to a remote host without a registry in the middle.
//!
//! Two implementations:
//!
//! - [`Transport::Tarball`] — `docker save | docker load` over the
//!   existing bollard ssh transport. Whole image every time. No host
//!   dependencies. Lives in [`crate::build::save_and_load_to_host`].
//! - [`Transport::Unregistry`] — spin up an ephemeral
//!   `ghcr.io/psviderski/unregistry` sidecar on the host, tunnel a
//!   local port to it, `docker push` over the registry protocol so
//!   only missing layers cross the wire. Lives in [`unregistry`].
//!
//! [`Transport::Auto`] tries unregistry, falls back to tarball with a
//! single warning line if anything in the unregistry setup fails (image
//! pull blocked, ssh forward refused, etc.). Hard `--transport=unregistry`
//! errors out instead.

pub mod oci_push;
pub mod tunnel;
pub mod unregistry;

/// Transport selection for the `--no-registry` image-delivery step.
/// Mirrors the CLI's `TransportMode` (which carries the clap value-enum
/// derive) so the binary surface stays at the binary boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Transport {
    /// Try unregistry first; fall back to tarball with a warning if any
    /// unregistry-side setup step fails.
    #[default]
    Auto,
    /// Push to an ephemeral unregistry sidecar via SSH-tunnelled port.
    /// Hard error on setup failure.
    Unregistry,
    /// Stream the whole image as a tarball into the host's docker
    /// daemon via `POST /images/load`.
    Tarball,
}
