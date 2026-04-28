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

use std::collections::BTreeMap;
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
) -> Result<String, DockerError> {
    let tag = cfg.resolved_local_tag();
    let (image_part, tag_part) = split_image_tag(&tag);
    if ops.image_present(host, image_part, tag_part).await? {
        return Ok(tag);
    }
    let dockerfile = render_dockerfile(cfg);
    let context = build_tar_context(&dockerfile);
    ops.build_image(host, &tag, context, BTreeMap::new())
        .await?;
    Ok(tag)
}

/// Render the two-stage Dockerfile that bakes `cfg.plugins` into a
/// custom caddy binary. Pure: same input → same bytes, suitable for
/// both the actual build context and the `yoink proxy-dockerfile`
/// debug command.
#[must_use]
pub fn render_dockerfile(cfg: &XcaddyConfig) -> String {
    let plugins = cfg.sorted_plugins();
    let mut s = String::new();
    s.push_str("# syntax=docker/dockerfile:1\n");
    s.push_str(&format!(
        "FROM {} AS builder\n",
        cfg.resolved_builder_image()
    ));
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
    s.push_str("\n\n");
    s.push_str(&format!("FROM {}\n", cfg.resolved_base_image()));
    s.push_str("COPY --from=builder /usr/bin/caddy /usr/bin/caddy\n");
    s.push_str(&format!("LABEL yoink.caddy.xcaddy_hash={}\n", cfg.hash()));
    s.push_str(&format!(
        "LABEL yoink.caddy.plugins=\"{}\"\n",
        plugins.join(",")
    ));
    s
}

/// Pack a single-entry tar of `Dockerfile` for `bollard::build_image`.
/// The context is small (a few hundred bytes), so a sync `Vec<u8>` is
/// fine on the tokio executor — no spawn_blocking needed.
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

        let returned = ensure_xcaddy_image(&ops, &host, &c).await.expect("ok");
        assert_eq!(returned, tag);
        assert!(
            !ops.calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::BuildImage(_, _, _))),
            "build should not run when image already present"
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
        ops.push_build_image(Ok(()));

        let returned = ensure_xcaddy_image(&ops, &host, &c).await.expect("ok");
        assert_eq!(returned, c.resolved_local_tag());
        let built: Vec<_> = ops
            .calls()
            .into_iter()
            .filter_map(|c| {
                if let RecordedCall::BuildImage(_, t, _) = c {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(built, vec![c.resolved_local_tag()]);
    }
}
