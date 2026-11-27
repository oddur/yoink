//! yoink as a library, in layers. A module uses only modules on its
//! own layer or below:
//!
//! - `tui`: the terminal UI (the `yoink` binary holds the commands).
//! - `features`: what the commands do besides deploying: status,
//!   diff, doctor, build, audit, port-forwards, templates.
//! - `deploy`: reconcile services onto hosts.
//! - `proxy`, `transport`: Caddy on the hosts, and shipping images.
//! - `docker_ops`, `docker`, `ssh`: the Docker API, container specs,
//!   and reaching a host.
//! - `config`, `secrets`: `yoink.yaml` and its secrets provider.
//! - `sealed`, `base`: the sealed file, and the leaves every layer
//!   shares.

pub mod base;
pub mod config;
pub mod deploy;
pub mod docker;
pub mod docker_ops;
pub mod features;
pub mod proxy;
pub mod sealed;
pub mod secrets;
pub mod ssh;
pub mod transport;
pub mod tui;
