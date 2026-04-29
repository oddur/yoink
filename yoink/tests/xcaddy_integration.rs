//! End-to-end xcaddy build flow against a real local Docker daemon.
//!
//! Gated behind the `integration_xcaddy` Cargo feature so it only runs
//! in the dedicated CI workflow (`.github/workflows/integration-xcaddy.yml`)
//! and on operator-side opt-in (`cargo test -p yoink --features
//! integration_xcaddy --test xcaddy_integration`). The unit tests in
//! `proxy::xcaddy::tests` cover the pure logic against `FakeDockerOps`;
//! this exercise is specifically about the bollard `POST /build` round
//! trip, the rendered tar context shape, and the post-build
//! image-presence state.
//!
//! Compiles `caddy-ratelimit` (one of the lighter plugins in the
//! ecosystem; ~1-2 min on a warm cache) and asserts the resulting
//! `yoink-caddy:<hash>` image is reachable via `image_present`.

#![cfg(feature = "integration_xcaddy")]

use std::collections::BTreeMap;
use std::sync::Arc;

use yoink::config::XcaddyConfig;
use yoink::docker_ops::{DockerOps, Host, RealDockerOps};
use yoink::proxy::xcaddy::ensure_xcaddy_image;

/// Lightweight plugin to keep CI runtime reasonable. Pinned to a known
/// release tag so the build is reproducible across runs.
const TEST_PLUGIN: &str = "github.com/mholt/caddy-ratelimit@v0.1.0";

#[tokio::test]
async fn xcaddy_image_builds_against_local_daemon() {
    let ops: Arc<dyn DockerOps> = Arc::new(RealDockerOps::new());
    let host = Host {
        user: String::new(),
        address: Host::LOCAL_ADDRESS.to_string(),
    };

    let cfg = XcaddyConfig {
        plugins: vec![TEST_PLUGIN.to_string()],
        caddy_version: None,
        base_image: None,
        builder_image: None,
        env: BTreeMap::new(),
        replace: Vec::new(),
    };

    let tag = ensure_xcaddy_image(&*ops, &host, &cfg, /*force_rebuild=*/ false)
        .await
        .expect("ensure_xcaddy_image succeeded");

    assert!(
        tag.starts_with("yoink-caddy:"),
        "expected yoink-caddy:<hash>, got {tag}",
    );

    // Image present on the host's local daemon now.
    let (image_part, tag_part) = tag.rsplit_once(':').expect("tag has ':'");
    assert!(
        ops.image_present(&host, image_part, tag_part)
            .await
            .expect("image_present query succeeded"),
        "{tag} should be cached locally after build",
    );

    // Second call short-circuits via image_present.
    let second = ensure_xcaddy_image(&*ops, &host, &cfg, /*force_rebuild=*/ false)
        .await
        .expect("second ensure_xcaddy_image succeeded");
    assert_eq!(tag, second, "tag is content-deterministic");
}
