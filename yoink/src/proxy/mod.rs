//! First-class reverse-proxy integration.
//!
//! Yoink bundles Caddy as a managed service when any service in the
//! config declares `domain:`. The proxy is just a regular yoink
//! service (`name = "yoink-proxy"`, `kind = ServiceKind::Proxy`) — it
//! reconciles through the same code path as everything else, drift-
//! detects via the same `spec_hash` label, and shows up in the same
//! TUI. The only special handling is:
//!
//! 1. Auto-injection at config-load time so users never write the
//!    `_proxy` block themselves ([`crate::config`]).
//! 2. Caddy JSON config rendered from the user-facing schema and
//!    pushed via Caddy's admin API at deterministic points in the
//!    rolling deploy ([`caddy`] + [`admin`]).
//!
//! Application-layer concerns (auth, rate limits, headers, CORS) are
//! deliberately not modeled. Use [`crate::config::ServiceConfig::caddy_extra_json`]
//! to drop in a JSON snippet of additional Caddy handlers.

pub mod admin;
pub mod caddy;
pub mod xcaddy;
