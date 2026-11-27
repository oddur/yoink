//! Content hashes yoink writes into labels and image tags.

/// First 64 bits of the SHA-256 of `s`, as 16 lowercase hex chars.
/// Short enough for a label value or an image tag, long enough that
/// two different inputs do not collide in practice.
#[must_use]
pub(crate) fn short_sha256(s: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = String::with_capacity(16);
    for b in &d[..8] {
        let _ = write!(out, "{b:02x}");
    }
    out
}
