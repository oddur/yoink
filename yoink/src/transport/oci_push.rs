//! Direct OCI registry push, bypassing the local docker daemon.
//!
//! Why: when the operator runs Docker Desktop on macOS (or Rancher
//! Desktop / Colima), the docker daemon lives inside a Linux VM. A
//! plain `docker push 127.0.0.1:<port>/...` runs from inside that VM,
//! so `127.0.0.1` is the VM's loopback — not the macOS host where the
//! SSH tunnel to the remote unregistry lives. Same problem applies to
//! bollard's `image_push` (also goes through the daemon).
//!
//! Workaround used by uncloud: spin up a `socat` proxy container in
//! the VM that bridges back out to `host.docker.internal`. We avoid
//! that by skipping the daemon for the push step entirely: read the
//! image bytes via `docker save` (daemon-side, no networking), then
//! push each blob over plain HTTP from the operator process. Since
//! reqwest runs *on the operator host* (not in the VM), `127.0.0.1`
//! resolves to the SSH tunnel correctly on every platform.
//!
//! What this implements:
//! - Read a `docker save` tarball (modern OCI layout: `index.json` +
//!   `blobs/sha256/<digest>` files).
//! - Locate the single-platform image manifest. Errors out clearly on
//!   multi-arch manifest lists — yoink builds are single-platform.
//! - For each blob (config + each layer): HEAD to check existence,
//!   then POST + PUT to upload if missing. Layer-level dedup is the
//!   whole point of using a registry transport over `docker save |
//!   docker load`.
//! - PUT the manifest under `<repo>:<tag>` to register the image.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use bytes::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;
use thiserror::Error;

/// How many blob uploads run concurrently against unregistry. Each
/// connection is an SSH tunnel multiplex stream — modest concurrency
/// helps overlap the per-blob HEAD round-trip with the next blob's
/// PUT, without saturating the (single-stream) tunnel.
const BLOB_CONCURRENCY: usize = 4;

#[derive(Debug, Error)]
pub enum OciPushError {
    #[error("failed to read image tarball entry: {0}")]
    TarRead(#[source] std::io::Error),
    #[error("invalid OCI image tarball: {0}")]
    InvalidTar(String),
    #[error("expected single-platform image, found manifest list with {count} entries — \
             multi-arch images aren't yet supported by the unregistry transport")]
    MultiArch { count: usize },
    #[error("blob {digest} referenced by manifest but not found in tarball")]
    MissingBlob { digest: String },
    #[error("HTTP error talking to unregistry at {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("registry rejected request to {url} with status {status}: {body}")]
    RegistryStatus {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("registry returned {status} but no Location header for upload: {url}")]
    NoLocationHeader {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("failed to parse JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Push an image from a docker-save tarball into the registry served at
/// `base_url` (e.g. `http://127.0.0.1:53337`), registering it as
/// `<repo>:<tag>`.
///
/// The tarball is fully read into memory so we can do two passes: one
/// to extract the index/manifest, then one to fish out each blob the
/// manifest references. For images yoink ships (a few hundred MB at
/// most), that's fine; if we ever need to support multi-GB images we
/// can switch to a file-backed two-pass.
pub async fn push_image(
    base_url: &str,
    repo: &str,
    tag: &str,
    tarball: Bytes,
) -> Result<PushSummary, OciPushError> {
    let layout = extract_oci_layout(&tarball)?;
    let manifest_descriptor = pick_image_manifest(&layout)?;
    let manifest_blob = layout
        .blobs
        .get(&manifest_descriptor.digest)
        .ok_or_else(|| OciPushError::MissingBlob {
            digest: manifest_descriptor.digest.clone(),
        })?
        .clone();

    let manifest: OciManifest = serde_json::from_slice(&manifest_blob)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("reqwest client builder with default config");

    // Config + every layer can push in any order; the manifest goes
    // last (it references all of them and the registry rejects it
    // until the referents exist). Run blob pushes with bounded
    // concurrency to overlap HEAD round-trips with PUT bodies.
    let mut blobs: Vec<&str> = Vec::with_capacity(manifest.layers.len() + 1);
    blobs.push(manifest.config.digest.as_str());
    for layer in &manifest.layers {
        blobs.push(layer.digest.as_str());
    }

    let per_blob: Vec<Result<PushSummary, OciPushError>> = futures_util::stream::iter(blobs)
        .map(|digest| {
            let client = &client;
            let layout = &layout;
            async move { push_one_blob(client, base_url, repo, digest, layout).await }
        })
        .buffer_unordered(BLOB_CONCURRENCY)
        .collect()
        .await;
    let mut summary = PushSummary::default();
    for r in per_blob {
        let s = r?;
        summary.blobs_uploaded += s.blobs_uploaded;
        summary.blobs_skipped += s.blobs_skipped;
        summary.bytes_uploaded += s.bytes_uploaded;
    }

    // Push manifest under <repo>:<tag>.
    let manifest_url = format!("{base_url}/v2/{repo}/manifests/{tag}");
    let resp = client
        .put(&manifest_url)
        .header(reqwest::header::CONTENT_TYPE, manifest_descriptor.media_type.as_str())
        .body(manifest_blob)
        .send()
        .await
        .map_err(|source| OciPushError::Http {
            url: manifest_url.clone(),
            source,
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(OciPushError::RegistryStatus {
            url: manifest_url,
            status,
            body,
        });
    }

    Ok(summary)
}

/// Stats reported back by [`push_image`] so the caller can render a
/// "n bytes pushed, k layers skipped" line. The whole point of this
/// transport is dedup, so the skipped-layer count is the headline.
#[derive(Debug, Default, Clone, Copy)]
pub struct PushSummary {
    pub blobs_uploaded: u32,
    pub blobs_skipped: u32,
    pub bytes_uploaded: u64,
}

async fn push_one_blob(
    client: &reqwest::Client,
    base_url: &str,
    repo: &str,
    digest: &str,
    layout: &OciLayout,
) -> Result<PushSummary, OciPushError> {
    let head_url = format!("{base_url}/v2/{repo}/blobs/{digest}");
    let head_resp = client
        .head(&head_url)
        .send()
        .await
        .map_err(|source| OciPushError::Http {
            url: head_url.clone(),
            source,
        })?;
    if head_resp.status().is_success() {
        return Ok(PushSummary {
            blobs_uploaded: 0,
            blobs_skipped: 1,
            bytes_uploaded: 0,
        });
    }
    if head_resp.status() != reqwest::StatusCode::NOT_FOUND {
        let status = head_resp.status();
        let body = head_resp.text().await.unwrap_or_default();
        return Err(OciPushError::RegistryStatus {
            url: head_url,
            status,
            body,
        });
    }

    let blob_bytes = layout
        .blobs
        .get(digest)
        .ok_or_else(|| OciPushError::MissingBlob {
            digest: digest.to_string(),
        })?
        .clone();

    let upload_init_url = format!("{base_url}/v2/{repo}/blobs/uploads/");
    let init_resp = client
        .post(&upload_init_url)
        .send()
        .await
        .map_err(|source| OciPushError::Http {
            url: upload_init_url.clone(),
            source,
        })?;
    if init_resp.status() != reqwest::StatusCode::ACCEPTED {
        let status = init_resp.status();
        let body = init_resp.text().await.unwrap_or_default();
        return Err(OciPushError::RegistryStatus {
            url: upload_init_url,
            status,
            body,
        });
    }
    let location = init_resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| OciPushError::NoLocationHeader {
            url: upload_init_url.clone(),
            status: init_resp.status(),
        })?;

    // Location header may be relative or absolute per the OCI spec.
    let upload_url = if location.starts_with("http://") || location.starts_with("https://") {
        location
    } else {
        format!("{base_url}{location}")
    };

    // Monolithic upload (PUT with ?digest=) — simpler than chunked
    // PATCH; unregistry accepts both.
    let separator = if upload_url.contains('?') { '&' } else { '?' };
    let put_url = format!("{upload_url}{separator}digest={digest}");
    let blob_len = blob_bytes.len() as u64;
    let put_resp = client
        .put(&put_url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(blob_bytes)
        .send()
        .await
        .map_err(|source| OciPushError::Http {
            url: put_url.clone(),
            source,
        })?;
    if !put_resp.status().is_success() {
        let status = put_resp.status();
        let body = put_resp.text().await.unwrap_or_default();
        return Err(OciPushError::RegistryStatus {
            url: put_url,
            status,
            body,
        });
    }

    Ok(PushSummary {
        blobs_uploaded: 1,
        blobs_skipped: 0,
        bytes_uploaded: blob_len,
    })
}

/// In-memory view of an OCI image layout extracted from a docker-save
/// tarball. `blobs` keys are the OCI digest form (`sha256:<hex>`).
struct OciLayout {
    index: OciIndex,
    blobs: HashMap<String, Bytes>,
}

#[derive(Debug, Deserialize)]
struct OciIndex {
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Deserialize, Clone)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Debug, Deserialize, Clone)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Debug, Deserialize)]
struct OciManifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

fn extract_oci_layout(tar_bytes: &[u8]) -> Result<OciLayout, OciPushError> {
    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let mut index_bytes: Option<Vec<u8>> = None;
    let mut blobs: HashMap<String, Bytes> = HashMap::new();
    let mut saw_layout_marker = false;

    for entry in archive.entries().map_err(OciPushError::TarRead)? {
        let mut entry = entry.map_err(OciPushError::TarRead)?;
        let path = entry.path().map_err(OciPushError::TarRead)?.to_path_buf();
        let path_str = path.to_string_lossy().to_string();

        if path_str == "oci-layout" {
            saw_layout_marker = true;
            continue;
        }
        if path_str == "index.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(OciPushError::TarRead)?;
            index_bytes = Some(buf);
            continue;
        }
        // Blob path looks like `blobs/sha256/<hex>`. Skip directory
        // entries and any other top-level files (manifest.json etc.).
        if let Some(rest) = path_str.strip_prefix("blobs/sha256/") {
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(OciPushError::TarRead)?;
            blobs.insert(format!("sha256:{rest}"), Bytes::from(buf));
        }
    }

    if !saw_layout_marker {
        return Err(OciPushError::InvalidTar(
            "missing `oci-layout` marker — tarball isn't in OCI image layout format. \
             yoink requires Docker 25+ (containerd image store enabled in Docker Desktop)."
                .to_string(),
        ));
    }
    let index_bytes = index_bytes.ok_or_else(|| {
        OciPushError::InvalidTar("missing `index.json` in image tarball".into())
    })?;
    let index: OciIndex = serde_json::from_slice(&index_bytes)?;

    Ok(OciLayout { index, blobs })
}

/// Walk the index until we land on an image manifest the operator's
/// local docker actually has all the blobs for.
///
/// Tarballs from `docker save image:tag` typically have a top-level
/// index whose first entry is either:
/// - a single image manifest (single-arch image — common for `docker
///   build` output), or
/// - an OCI image index (manifest list — common for official multi-arch
///   images like `alpine`).
///
/// In the manifest-list case `docker save` only writes the blobs for
/// the platform actually pulled to the local cache, even though the
/// manifest descriptors for every platform are referenced. We pick
/// the entry whose config + every layer is present in the tarball;
/// that's the platform the operator actually has and the only one we
/// can push.
fn pick_image_manifest(layout: &OciLayout) -> Result<Descriptor, OciPushError> {
    let first = layout.index.manifests.first().ok_or_else(|| {
        OciPushError::InvalidTar("index.json has empty manifests array".into())
    })?;

    if is_image_manifest(&first.media_type) {
        return Ok(first.clone());
    }

    // It's an index — descend.
    let nested_bytes = layout
        .blobs
        .get(&first.digest)
        .ok_or_else(|| OciPushError::MissingBlob {
            digest: first.digest.clone(),
        })?;
    let nested: OciIndex = serde_json::from_slice(nested_bytes)?;

    // Skip attestation manifests (platform.arch == "unknown") — they
    // describe SLSA provenance, not an image.
    let real: Vec<&Descriptor> = nested
        .manifests
        .iter()
        .filter(|d| {
            d.platform
                .as_ref()
                .is_none_or(|p| p.architecture != "unknown" && p.os != "unknown")
        })
        .collect();

    // Among real manifests, pick the first one whose blobs are fully
    // present in the tarball. For a single-arch local image, only one
    // platform's blobs exist; for multi-arch (rare from `docker build`),
    // we'd need explicit platform selection.
    for m in &real {
        if manifest_blobs_present(m, layout).unwrap_or(false) {
            return Ok((*m).clone());
        }
    }

    Err(OciPushError::MultiArch { count: real.len() })
}

/// True iff every blob referenced by `descriptor`'s manifest (config +
/// all layers) is present in the tarball's blob set. Used to pick the
/// platform-specific manifest from a manifest list whose other
/// platforms' blobs were pruned by `docker save`.
fn manifest_blobs_present(descriptor: &Descriptor, layout: &OciLayout) -> Option<bool> {
    let manifest_bytes = layout.blobs.get(&descriptor.digest)?;
    let manifest: OciManifest = serde_json::from_slice(manifest_bytes).ok()?;
    if !layout.blobs.contains_key(&manifest.config.digest) {
        return Some(false);
    }
    Some(manifest.layers.iter().all(|l| layout.blobs.contains_key(&l.digest)))
}

fn is_image_manifest(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_image_manifest_recognizes_oci_and_docker_v2() {
        assert!(is_image_manifest("application/vnd.oci.image.manifest.v1+json"));
        assert!(is_image_manifest(
            "application/vnd.docker.distribution.manifest.v2+json"
        ));
        assert!(!is_image_manifest("application/vnd.oci.image.index.v1+json"));
    }
}
