//! Per-host xcaddy build orchestration.
//!
//! When a user sets `proxy.xcaddy.plugins:` in their config, yoink
//! synthesizes a small two-stage Dockerfile (caddy-builder → caddy)
//! and asks each proxy host's docker daemon to build it locally,
//! tagging the result as `yoink-caddy:<hash>`. The proxy service
//! then runs from that local tag — no registry needed.
//!
//! The hash is derived from the build inputs, so identical config
//! across runs short-circuits via `image_present`. Edit the plugin
//! list and the hash flips, triggering a rebuild on next `up`.

use std::fmt::Write as _;
use std::io::Write as _;

use bytes::Bytes;

use crate::config::XcaddyConfig;
use crate::docker_ops::{DockerError, DockerOps, Host};

/// Make sure the xcaddy-built image is present on `host`. Idempotent:
/// returns immediately if the tag is already in the host's image cache.
/// Otherwise renders the Dockerfile, packs it into a tar context, and
/// kicks off a build on the host's docker daemon. Returns the local
/// tag the proxy service should run from.
pub async fn ensure_xcaddy_image(
    ops: &dyn DockerOps,
    host: &Host,
    cfg: &XcaddyConfig,
    force_rebuild: bool,
) -> Result<String, DockerError> {
    let tag = cfg.resolved_local_tag();
    let (image_part, tag_part) = split_image_tag(&tag);
    // `force_rebuild` (`yoink up --rebuild-proxy`) skips the
    // `image_present` short-circuit, e.g. when an unpinned plugin's
    // upstream module changed but the config-derived hash hasn't.
    if !force_rebuild && ops.image_present(host, image_part, tag_part).await? {
        return Ok(tag);
    }
    // Refresh the builder image before running the build. `caddy:2-builder`
    // is a floating tag, so a daemon with a stale cached digest would
    // produce semantically older binaries than the hash claims; the
    // explicit pull-then-build keeps the hash determinism honest. The
    // pull is a no-op when the daemon already has the latest digest.
    let (builder_image, builder_tag) = split_image_tag(cfg.resolved_builder_image());
    ops.pull_image(host, builder_image, builder_tag, None)
        .await?;
    let dockerfile = render_dockerfile(cfg);
    let context = build_tar_context(&dockerfile);
    ops.build_image(host, &tag, context).await?;
    Ok(tag)
}

/// Render the two-stage Dockerfile that bakes `cfg.plugins` into a
/// custom caddy binary. Pure: same input → same bytes, suitable for
/// both the actual build context and the `yoink proxy-dockerfile`
/// debug command.
#[must_use]
pub fn render_dockerfile(cfg: &XcaddyConfig) -> String {
    let plugins = cfg.sorted_plugins();
    let replace = cfg.sorted_replace();
    let mut s = String::new();
    s.push_str("# syntax=docker/dockerfile:1\n");
    let _ = writeln!(s, "FROM {} AS builder", cfg.resolved_builder_image());
    // `BTreeMap` iter is already key-sorted; emit one ENV per entry so
    // GOPRIVATE/GOPROXY/NETRC overrides reach the `xcaddy build` shell.
    for (k, v) in &cfg.env {
        let _ = writeln!(s, "ENV {k}={v}");
    }
    // `xcaddy build` takes the Caddy version (a git tag like `v2.8.4`)
    // as a positional arg. Omitted → xcaddy uses the latest tagged
    // release. Only emit the arg when the user pinned it explicitly,
    // otherwise we'd push a bogus value (`2`, `latest`, …) at git.
    s.push_str("RUN xcaddy build");
    if let Some(ver) = cfg.caddy_version.as_deref() {
        s.push(' ');
        s.push_str(ver);
    }
    for plugin in &plugins {
        s.push_str(" \\\n    --with ");
        s.push_str(plugin);
    }
    for entry in &replace {
        s.push_str(" \\\n    --replace ");
        s.push_str(entry);
    }
    s.push_str("\n\n");
    let _ = writeln!(s, "FROM {}", cfg.resolved_base_image());
    s.push_str("COPY --from=builder /usr/bin/caddy /usr/bin/caddy\n");
    let _ = writeln!(s, "LABEL yoink.caddy.xcaddy_hash={}", cfg.hash());
    let _ = writeln!(s, "LABEL yoink.caddy.plugins=\"{}\"", plugins.join(","));
    s
}

/// Pack a single-entry tar of `Dockerfile` for `bollard::build_image`.
/// The context is small (a few hundred bytes), so a sync `Vec<u8>` is
/// fine on the tokio executor — no `spawn_blocking` needed.
fn build_tar_context(dockerfile: &str) -> Bytes {
    let mut buf = Vec::with_capacity(dockerfile.len() + 1024);
    {
        let mut tar = tar::Builder::new(&mut buf);
        let bytes = dockerfile.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("Dockerfile").expect("tar path utf8");
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, bytes).expect("tar write in-memory");
        tar.finish().expect("tar finish in-memory");
    }
    let _ = std::io::sink().flush();
    Bytes::from(buf)
}

fn split_image_tag(reference: &str) -> (&str, &str) {
    reference.rsplit_once(':').unwrap_or((reference, "latest"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(plugins: &[&str]) -> XcaddyConfig {
        XcaddyConfig {
            plugins: plugins.iter().map(|p| (*p).to_string()).collect(),
            caddy_version: None,
            base_image: None,
            builder_image: None,
            env: std::collections::BTreeMap::new(),
            replace: Vec::new(),
        }
    }

    #[test]
    fn hash_is_deterministic_across_plugin_order() {
        let a = cfg(&["github.com/a/one", "github.com/b/two", "github.com/c/three"]);
        let b = cfg(&["github.com/c/three", "github.com/a/one", "github.com/b/two"]);
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn hash_changes_on_plugin_change() {
        let a = cfg(&["github.com/a/one"]);
        let b = cfg(&["github.com/a/one", "github.com/b/two"]);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn hash_changes_on_caddy_version_change() {
        let mut a = cfg(&["github.com/a/one"]);
        let mut b = cfg(&["github.com/a/one"]);
        a.caddy_version = Some("2.7.6".into());
        b.caddy_version = Some("2.8.4".into());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn dockerfile_alphabetizes_plugins() {
        let c = cfg(&["github.com/zeta/last", "github.com/alpha/first"]);
        let df = render_dockerfile(&c);
        let alpha_idx = df.find("github.com/alpha/first").unwrap();
        let zeta_idx = df.find("github.com/zeta/last").unwrap();
        assert!(alpha_idx < zeta_idx, "plugins must be sorted: {df}");
    }

    #[test]
    fn dockerfile_renders_expected_shape_pinned_version() {
        let mut c = cfg(&["github.com/mholt/caddy-ratelimit"]);
        c.caddy_version = Some("v2.8.4".into());
        let df = render_dockerfile(&c);
        assert!(df.starts_with("# syntax=docker/dockerfile:1"));
        assert!(df.contains("FROM caddy:2-builder AS builder"));
        assert!(df.contains("xcaddy build v2.8.4 \\"));
        assert!(df.contains("--with github.com/mholt/caddy-ratelimit"));
        assert!(df.contains("FROM caddy:2"));
        assert!(df.contains("COPY --from=builder /usr/bin/caddy /usr/bin/caddy"));
        assert!(df.contains("LABEL yoink.caddy.xcaddy_hash="));
        assert!(df.contains("LABEL yoink.caddy.plugins=\"github.com/mholt/caddy-ratelimit\""));
    }

    #[test]
    fn dockerfile_omits_positional_when_caddy_version_unset() {
        let c = cfg(&["github.com/mholt/caddy-ratelimit"]);
        let df = render_dockerfile(&c);
        // Bare `RUN xcaddy build \` (no positional arg) → xcaddy resolves
        // caddy's latest tagged release.
        assert!(df.contains("RUN xcaddy build \\\n    --with"), "got: {df}");
    }

    #[test]
    fn hash_changes_on_env_or_replace_change() {
        let mut a = cfg(&["github.com/a/one"]);
        let mut b = cfg(&["github.com/a/one"]);
        a.env.insert("GOPRIVATE".into(), "*.example.com".into());
        let h_with_env = a.hash();
        assert_ne!(b.hash(), h_with_env);
        b.replace
            .push("github.com/foo/bar=github.com/me/bar-fork@v1.0.0".into());
        assert_ne!(b.hash(), h_with_env);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn dockerfile_emits_env_and_replace() {
        let mut c = cfg(&["github.com/foo/bar"]);
        c.env.insert("GOPRIVATE".into(), "*.example.com".into());
        c.env.insert("NETRC".into(), "/run/secrets/netrc".into());
        c.replace
            .push("github.com/x/y=github.com/me/y-fork@v0.2.0".into());
        let df = render_dockerfile(&c);
        // ENV lines emit between `FROM ... AS builder` and `RUN xcaddy build`.
        assert!(df.contains("ENV GOPRIVATE=*.example.com"), "got: {df}");
        assert!(df.contains("ENV NETRC=/run/secrets/netrc"), "got: {df}");
        // BTreeMap iteration is key-sorted → GOPRIVATE before NETRC.
        let goprivate_idx = df.find("ENV GOPRIVATE").unwrap();
        let netrc_idx = df.find("ENV NETRC").unwrap();
        let run_idx = df.find("RUN xcaddy build").unwrap();
        assert!(goprivate_idx < netrc_idx);
        assert!(netrc_idx < run_idx);
        // `--replace` flag appears on the `xcaddy build` line.
        assert!(
            df.contains("--replace github.com/x/y=github.com/me/y-fork@v0.2.0"),
            "got: {df}",
        );
    }

    #[test]
    fn tar_context_has_dockerfile_entry() {
        let bytes = build_tar_context("FROM scratch\n");
        // tar entries start at offset 0; first 100 bytes are the name.
        let name = std::str::from_utf8(&bytes[0..10]).unwrap();
        assert!(name.starts_with("Dockerfile"), "got name {name:?}");
    }

    #[tokio::test]
    async fn ensure_image_skips_build_when_already_present() {
        use crate::docker_ops::{FakeDockerOps, RecordedCall};
        let ops = FakeDockerOps::new();
        let host = Host {
            user: "u".into(),
            address: "h".into(),
        };
        let c = cfg(&["github.com/a/one"]);
        let tag = c.resolved_local_tag();
        let (image, tag_part) = split_image_tag(&tag);
        ops.mark_image_present(image, tag_part);

        let returned = ensure_xcaddy_image(&ops, &host, &c, false)
            .await
            .expect("ok");
        assert_eq!(returned, tag);
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::BuildImage(_, _, _))),
            "build should not run when image already present"
        );
    }

    #[tokio::test]
    async fn ensure_image_force_rebuild_ignores_image_present() {
        use crate::docker_ops::{FakeDockerOps, RecordedCall};
        let ops = FakeDockerOps::new();
        let host = Host {
            user: "u".into(),
            address: "h".into(),
        };
        let c = cfg(&["github.com/a/one"]);
        let tag = c.resolved_local_tag();
        let (image, tag_part) = split_image_tag(&tag);
        ops.mark_image_present(image, tag_part);
        ops.push_pull_image(Ok(()));
        ops.push_build_image(Ok(()));

        // force_rebuild=true ignores the cached image and re-runs the build.
        ensure_xcaddy_image(&ops, &host, &c, true)
            .await
            .expect("ok");
        assert!(
            ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::BuildImage(_, _, _))),
            "build should run when force_rebuild is set"
        );
    }

    #[tokio::test]
    async fn ensure_image_builds_when_absent() {
        use crate::docker_ops::{FakeDockerOps, RecordedCall};
        let ops = FakeDockerOps::new();
        let host = Host {
            user: "u".into(),
            address: "h".into(),
        };
        let c = cfg(&["github.com/a/one"]);
        ops.push_pull_image(Ok(())); // builder pull
        ops.push_build_image(Ok(())); // xcaddy build

        let returned = ensure_xcaddy_image(&ops, &host, &c, false)
            .await
            .expect("ok");
        assert_eq!(returned, c.resolved_local_tag());

        // Pull-then-build is the contract — recorded call order matters
        // because a stale builder image breaks hash determinism.
        let calls = ops.calls();
        let pull_idx = calls.iter().position(|c| {
            matches!(
                c,
                RecordedCall::PullImage(_, image, tag) if image == "caddy" && tag == "2-builder"
            )
        });
        let build_idx = calls
            .iter()
            .position(|c| matches!(c, RecordedCall::BuildImage(_, _, _)));
        assert!(pull_idx.is_some(), "builder pull recorded");
        assert!(build_idx.is_some(), "build recorded");
        assert!(pull_idx < build_idx, "pull before build");
    }
}
