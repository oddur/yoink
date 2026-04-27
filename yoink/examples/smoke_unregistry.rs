//! End-to-end smoke test for the unregistry transport.
//!
//! Pulls `alpine:3.19` locally, re-tags it as `yoink-smoketest:<unix>`,
//! pushes that tag to the target host via the new unregistry transport,
//! verifies the image landed in the host's docker store, then cleans
//! up both sides.
//!
//! Usage:
//!   `cargo run --example smoke_unregistry -- <user>@<host>`
//! e.g.:
//!   `cargo run --example smoke_unregistry -- root@backtrack-eu-1`
//!
//! Exits 0 on success, 1 on any failure (with a diagnostic).

use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::query_parameters::RemoveImageOptionsBuilder;
use yoink::docker_ops::{DockerOps, Host, RealDockerOps};
use yoink::transport::unregistry;

const SOURCE_IMAGE: &str = "alpine";
const SOURCE_TAG: &str = "3.19";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let arg = std::env::args().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: smoke_unregistry <user>@<host>")
    })?;
    let (user, address) = arg.split_once('@').ok_or_else(|| {
        anyhow::anyhow!("expected <user>@<host>, got {arg:?}")
    })?;

    let host = Host {
        user: user.to_string(),
        address: address.to_string(),
    };
    let ops = RealDockerOps::new();

    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let smoke_image = "yoink-smoketest";
    let smoke_tag = format!("smoke-{unix}");
    let smoke_ref = format!("{smoke_image}:{smoke_tag}");

    eprintln!("== smoke unregistry: target={user}@{address}, ref={smoke_ref} ==");

    // 1. Pull alpine locally if missing.
    eprintln!("[1/6] docker pull {SOURCE_IMAGE}:{SOURCE_TAG}");
    run_docker(&["pull", &format!("{SOURCE_IMAGE}:{SOURCE_TAG}")])
        .context("local docker pull")?;

    // 2. Re-tag locally so the push lands on a unique image:tag the host
    //    can't already have. Using a fresh tag is the only reliable
    //    "did the dedup actually work" signal — alpine layers are
    //    ubiquitous and would skew first-run vs. subsequent-run.
    eprintln!("[2/6] docker tag {SOURCE_IMAGE}:{SOURCE_TAG} {smoke_ref}");
    run_docker(&["tag", &format!("{SOURCE_IMAGE}:{SOURCE_TAG}"), &smoke_ref])
        .context("local docker tag")?;

    // 3. Push via unregistry transport.
    eprintln!("[3/6] unregistry::push → {address}");
    let push_start = std::time::Instant::now();
    if let Err(e) = unregistry::push(&ops, &host, &smoke_ref).await {
        let _ = run_docker(&["rmi", &smoke_ref]);
        bail!("unregistry::push failed after {:?}: {e}", push_start.elapsed());
    }
    eprintln!("    ✓ push completed in {:?}", push_start.elapsed());

    // 4. Verify the image is present on the host's docker store.
    eprintln!("[4/6] verify image_present({smoke_image}, {smoke_tag}) on host");
    let present = ops
        .image_present(&host, smoke_image, &smoke_tag)
        .await
        .context("inspect on host")?;
    if !present {
        let _ = run_docker(&["rmi", &smoke_ref]);
        bail!(
            "image {smoke_ref} reports as NOT present on {address} after push — \
             unregistry sidecar didn't register it correctly"
        );
    }
    eprintln!("    ✓ image present on host");

    // 5. Second push: should be a no-op layer-wise (every layer already
    //    on host). Times this leg so the operator can eyeball the
    //    redeploy speedup vs. the first push.
    eprintln!("[5/6] re-push (should hit registry-protocol dedup)");
    let repush_start = std::time::Instant::now();
    unregistry::push(&ops, &host, &smoke_ref)
        .await
        .context("second push")?;
    eprintln!("    ✓ re-push completed in {:?}", repush_start.elapsed());

    // 6. Cleanup: drop the local re-tag and ask the host to drop the
    //    image too, so this script is idempotent across runs.
    eprintln!("[6/6] cleanup (local rmi + remote rmi via bollard ssh)");
    let _ = run_docker(&["rmi", &smoke_ref]);
    let _ = remove_remote_image(&host, &smoke_ref).await;

    eprintln!("\n✓ smoke unregistry passed on {address}");
    eprintln!("  first push:  {:?}", push_start.elapsed());
    eprintln!("  second push: {:?}", repush_start.elapsed());
    eprintln!("  (second push should be much faster; that's the dedup at work)");

    Ok(())
}

fn run_docker(args: &[&str]) -> Result<()> {
    let status = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("spawn docker {args:?}"))?;
    if !status.success() {
        bail!("docker {args:?} exited {status}");
    }
    Ok(())
}

async fn remove_remote_image(host: &Host, image_ref: &str) -> Result<()> {
    // Use bollard's `ssh://` transport rather than shelling out to ssh
    // — same path the rest of yoink uses to talk to the remote daemon.
    let docker = Docker::connect_with_ssh(
        &format!("ssh://{}@{}", host.user, host.address),
        120,
        bollard::API_DEFAULT_VERSION,
        None,
    )
    .with_context(|| format!("connect bollard ssh:// to {}", host.address))?;
    let opts = RemoveImageOptionsBuilder::default().force(true).build();
    docker
        .remove_image(image_ref, Some(opts), None)
        .await
        .with_context(|| format!("remove remote image {image_ref}"))?;
    Ok(())
}
