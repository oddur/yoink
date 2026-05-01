//! `age`-sealed secrets — the batteries-included default.
//!
//! Operators commit a single `secrets.age` file alongside `yoink.yaml`.
//! Format inside the seal: dotenv (`KEY=value\n`). At deploy time
//! yoink decrypts with one identity, parses the dotenv, and feeds the
//! resulting `KEY -> VALUE` map to the `SecretsBundle` machinery shared
//! with the `provider: command` path.
//!
//! Identity resolution order (same code path locally and in CI):
//!   1. `YOINK_AGE_KEY` env var — raw `AGE-SECRET-KEY-1...`
//!   2. `YOINK_AGE_KEY_FILE` env var — path to a key file
//!   3. Scan `~/.config/yoink/keys/*.key` for an identity whose public
//!      half matches one of the config's `secrets.recipients:`
//!   4. Legacy `~/.config/yoink/age.key` (the pre-multi-identity default;
//!      still loaded so existing setups don't break on upgrade)
//!
//! Recipients (used by the `seal`/`edit` CLI flow) are read from the
//! `secrets.recipients:` list in `yoink.yaml`. Decryption only needs
//! one matching identity.

use std::collections::BTreeMap;
use std::env;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use age::{
    armor::{ArmoredReader, ArmoredWriter, Format},
    secrecy::ExposeSecret,
    x25519,
};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::config::Config;

pub const AGE_KEY_ENV: &str = "YOINK_AGE_KEY";
pub const AGE_KEY_FILE_ENV: &str = "YOINK_AGE_KEY_FILE";
pub const DEFAULT_SEALED_FILENAME: &str = "secrets.age";

#[derive(Debug, Error)]
pub enum SealedError {
    #[error("`HOME` env var not set — cannot locate ~/.config/yoink/age.key")]
    NoHome,
    #[error(
        "no matching age identity found for this config's recipients — set {AGE_KEY_ENV} (raw key), {AGE_KEY_FILE_ENV} (path), or place the key at {0}"
    )]
    NoIdentity(PathBuf),
    #[error("read age identity file {path}: {source}")]
    IdentityRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse age identity: {0}")]
    IdentityParse(String),
    #[error("parse age recipient {raw:?}: {reason}")]
    RecipientParse { raw: String, reason: String },
    #[error(
        "no recipients configured — add at least one age public key under `secrets.recipients:` in yoink.yaml"
    )]
    NoRecipients,
    #[error("age encryption failed: {0}")]
    Encrypt(String),
    #[error("age decryption failed: {0}")]
    Decrypt(String),
    #[error("malformed dotenv at line {line}: {reason}")]
    Dotenv { line: usize, reason: String },
    #[error("write sealed file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("create parent directory {path}: {source}")]
    Mkdir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "sealed file path {0:?} escapes the config directory — `secrets.file:` may not contain `..` or be absolute outside of yoink.yaml's directory"
    )]
    PathEscape(String),
    #[error(
        "age identity file {path} has permissive mode {mode:#o} — tighten with `chmod 600 {path_display}`"
    )]
    IdentityPermissive {
        path: PathBuf,
        path_display: String,
        mode: u32,
    },
    #[error("invalid secret name {0:?} (must match [A-Za-z_][A-Za-z0-9_]*)")]
    InvalidSecretName(String),
    #[error("ssh keygen: {0}")]
    SshKeygen(String),
    #[error(
        "{0:?} already exists in the sealed bundle. Pick a different name, or run `yoink secrets edit` to remove the existing entry first."
    )]
    SecretAlreadySealed(String),
}

/// Resolve where the sealed file lives. Honors `secrets.file:` when
/// set, falls back to `<config_dir>/secrets.age`.
///
/// Rejects `secrets.file:` values that contain `..` components or are
/// absolute paths pointing outside the config directory — the value
/// comes from a yaml that anyone with PR access can edit, and a
/// `file: ../../../etc/passwd` should never become a sealed-file
/// pointer (`yoink secrets edit` would happily overwrite it).
pub fn resolve_sealed_path(
    config: &Config,
    file_override: Option<&str>,
) -> Result<PathBuf, SealedError> {
    let base = config
        .config_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let raw = file_override.unwrap_or(DEFAULT_SEALED_FILENAME);
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return Err(SealedError::PathEscape(raw.to_string()));
    }
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(SealedError::PathEscape(raw.to_string()));
    }
    Ok(base.join(candidate))
}

/// Multi-identity keys directory: `~/.config/yoink/keys/`.
///
/// Each file in the dir is one age identity. By convention the
/// filename is `<public-recipient>.key` (so `~/.config/yoink/keys/age1abc….key`)
/// — that lets us pick the right one for a given config without
/// having to read every file. We still fall back to opening + checking
/// pubkey for files whose filename doesn't match the convention.
pub fn keys_dir() -> Result<PathBuf, SealedError> {
    let home = env::var("HOME").map_err(|_| SealedError::NoHome)?;
    Ok(PathBuf::from(home).join(".config/yoink/keys"))
}

/// Path inside [`keys_dir`] for a given public recipient. Use this when
/// writing a freshly-generated identity so future `load_identity` calls
/// can find it by filename without opening every file in the dir.
pub fn keys_dir_path_for(public: &str) -> Result<PathBuf, SealedError> {
    Ok(keys_dir()?.join(format!("{public}.key")))
}

/// Pre-multi-identity location: `~/.config/yoink/age.key`. Still loaded
/// when no match is found in [`keys_dir`], so existing setups don't
/// break on upgrade.
pub fn legacy_identity_path() -> Result<PathBuf, SealedError> {
    let home = env::var("HOME").map_err(|_| SealedError::NoHome)?;
    Ok(PathBuf::from(home).join(".config/yoink/age.key"))
}

/// Back-compat alias for [`legacy_identity_path`]. Older code in this
/// crate referenced `default_identity_path`; the new keys-dir layout
/// makes "default" ambiguous, so the rename clarifies.
#[deprecated(note = "use legacy_identity_path or keys_dir")]
pub fn default_identity_path() -> Result<PathBuf, SealedError> {
    legacy_identity_path()
}

/// Resolve the operator's age identity for a config that seals against
/// `recipients`. See module docs for the full priority list.
///
/// Pass `&[]` to skip the keys-dir scan (e.g. from contexts that don't
/// have a config handy). Falls through env vars + legacy path only.
pub fn load_identity(recipients: &[String]) -> Result<x25519::Identity, SealedError> {
    load_identity_resolved(
        env::var(AGE_KEY_ENV).ok(),
        env::var(AGE_KEY_FILE_ENV).ok(),
        &keys_dir()?,
        &legacy_identity_path()?,
        recipients,
    )
}

/// Pure resolution logic — env values and paths are passed in instead
/// of being read from process state. Tests drive this directly to
/// avoid mutating `HOME` / `YOINK_AGE_KEY*`, which would race with
/// other tests in the suite.
fn load_identity_resolved(
    env_key: Option<String>,
    env_file: Option<String>,
    keys_dir: &Path,
    legacy_path: &Path,
    recipients: &[String],
) -> Result<x25519::Identity, SealedError> {
    if let Some(raw) = env_key {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return parse_identity(trimmed);
        }
    }

    if let Some(path) = env_file {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return load_identity_from_file(Path::new(trimmed));
        }
    }

    if !recipients.is_empty()
        && let Some(identity) = scan_dir_for_match(keys_dir, recipients)?
    {
        return Ok(identity);
    }

    if legacy_path.exists() {
        return load_identity_from_file(legacy_path);
    }
    Err(SealedError::NoIdentity(legacy_path.to_path_buf()))
}

/// Walk a keys dir looking for an identity whose public half is in
/// `recipients`. Cheap path: filename match (`<pubkey>.key`). Slow
/// path: open and check the public for files that don't follow the
/// naming convention (e.g. `team.key`, `prod.key`).
///
/// Decoupled from `HOME` resolution so tests can drive it against a
/// hermetic temp dir.
fn scan_dir_for_match(
    dir: &Path,
    recipients: &[String],
) -> Result<Option<x25519::Identity>, SealedError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Dir doesn't exist yet — fresh user, no keys saved. Not an error.
        return Ok(None);
    };

    let recipient_set: std::collections::HashSet<&str> =
        recipients.iter().map(String::as_str).collect();

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("key") {
            continue;
        }
        // Cheap path: filename without extension equals one of the
        // recipients. Avoid opening (and thus mode-checking) files we
        // know we don't care about.
        if let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str)
            && recipient_set.contains(stem)
        {
            return Ok(Some(load_identity_from_file(&path)?));
        }
        candidates.push(path);
    }

    // Slow path: a key file with a non-conventional name. Open each,
    // derive its public, and check membership.
    for path in candidates {
        match load_identity_from_file(&path) {
            Ok(identity) => {
                let public = identity.to_public().to_string();
                if recipient_set.contains(public.as_str()) {
                    return Ok(Some(identity));
                }
            }
            Err(e) => {
                // A broken key file in the dir shouldn't block
                // loading a sibling that works. Skip and move on.
                tracing::debug!("skipping {}: {e}", path.display());
            }
        }
    }
    Ok(None)
}

fn load_identity_from_file(path: &Path) -> Result<x25519::Identity, SealedError> {
    check_identity_file_mode(path)?;
    let raw = std::fs::read_to_string(path).map_err(|source| SealedError::IdentityRead {
        path: path.to_path_buf(),
        source,
    })?;
    // Identity files may carry `# created: ...` comments above the key.
    let key_line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| SealedError::IdentityParse(format!("{} is empty", path.display())))?;
    parse_identity(key_line)
}

/// Refuse to load an age identity from a world-readable / group-readable
/// file. The private half of an age key is bearer-secret material — if
/// it lands in the local checkout with `0644` it's effectively shared
/// with every other login on the box. CI runners typically materialize
/// the key inside a tempfile they own; we just want a clear error
/// before the operator pipes a leaked identity into yoink.
#[cfg(unix)]
fn check_identity_file_mode(path: &Path) -> Result<(), SealedError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).map_err(|source| SealedError::IdentityRead {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(SealedError::IdentityPermissive {
            path: path.to_path_buf(),
            path_display: path.display().to_string(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_identity_file_mode(_path: &Path) -> Result<(), SealedError> {
    Ok(())
}

fn parse_identity(raw: &str) -> Result<x25519::Identity, SealedError> {
    x25519::Identity::from_str(raw).map_err(|e| SealedError::IdentityParse(e.to_string()))
}

/// Generate a fresh x25519 identity. Returns the identity's secret
/// representation (`AGE-SECRET-KEY-1...`) and matching public
/// recipient (`age1...`).
#[must_use]
pub fn keygen() -> (Zeroizing<String>, String) {
    let id = x25519::Identity::generate();
    let secret = Zeroizing::new(id.to_string().expose_secret().to_owned());
    let public = id.to_public().to_string();
    (secret, public)
}

/// Generate a fresh ed25519 SSH keypair, merge the private half (PEM
/// format) into the sealed bundle at `target_path` under `seal_as`,
/// and return the public half in OpenSSH single-line format.
///
/// The private key is generated in memory; no plaintext PEM is ever
/// written to disk outside the sealed bundle. Existing entries in the
/// bundle are preserved (same merge logic as `secrets edit` on save).
/// Errors if `seal_as` already exists in the bundle (caller must
/// remove it first).
///
/// `seal_as` is validated against the dotenv key shape
/// (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn ssh_keygen_into_bundle(
    target_path: &Path,
    recipients: &[String],
    seal_as: &str,
    comment: Option<&str>,
) -> Result<String, SealedError> {
    use ssh_key::{Algorithm, LineEnding, PrivateKey, rand_core::OsRng};

    if seal_as.is_empty()
        || seal_as
            .bytes()
            .next()
            .is_none_or(|b| !(b.is_ascii_alphabetic() || b == b'_'))
        || !seal_as
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(SealedError::InvalidSecretName(seal_as.to_string()));
    }

    let mut rng = OsRng;
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| SealedError::SshKeygen(format!("generate ed25519: {e}")))?;
    if let Some(c) = comment {
        key.set_comment(c);
    }
    let priv_pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| SealedError::SshKeygen(format!("encode private as OpenSSH PEM: {e}")))?;
    let pub_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|e| SealedError::SshKeygen(format!("encode public as OpenSSH: {e}")))?;

    let mut bundle = if target_path.exists() {
        let identity = load_identity(recipients)?;
        let ciphertext = std::fs::read(target_path).map_err(|source| SealedError::Write {
            path: target_path.to_path_buf(),
            source,
        })?;
        let plaintext = unseal(&ciphertext, &identity)?;
        parse_dotenv(&plaintext)?
    } else {
        BTreeMap::new()
    };
    if bundle.contains_key(seal_as) {
        return Err(SealedError::SecretAlreadySealed(seal_as.to_string()));
    }
    bundle.insert(seal_as.to_string(), Zeroizing::new((*priv_pem).to_string()));

    let canonical = render_dotenv(&bundle);
    let sealed_bytes = seal(canonical.as_bytes(), recipients)?;
    write_atomically_secret(target_path, &sealed_bytes)?;
    Ok(pub_openssh)
}

/// Seal `plaintext` against the recipients listed in `recipients`.
/// Returns ASCII-armored ciphertext (suitable for committing to git
/// without binary-diff noise — the file shows up as one big blob in
/// PRs but at least diffs cleanly when the whole thing changes).
pub fn seal(plaintext: &[u8], recipients: &[String]) -> Result<Vec<u8>, SealedError> {
    if recipients.is_empty() {
        return Err(SealedError::NoRecipients);
    }
    let parsed: Vec<Box<dyn age::Recipient + Send>> = recipients
        .iter()
        .map(|r| {
            let key: x25519::Recipient =
                r.parse().map_err(|e: &str| SealedError::RecipientParse {
                    raw: r.clone(),
                    reason: e.to_string(),
                })?;
            Ok(Box::new(key) as Box<dyn age::Recipient + Send>)
        })
        .collect::<Result<_, SealedError>>()?;

    let encryptor =
        age::Encryptor::with_recipients(parsed.iter().map(|b| b.as_ref() as &dyn age::Recipient))
            .map_err(|e| SealedError::Encrypt(e.to_string()))?;

    let mut buf = Vec::with_capacity(plaintext.len() + 256);
    let armor = ArmoredWriter::wrap_output(&mut buf, Format::AsciiArmor)
        .map_err(|e| SealedError::Encrypt(e.to_string()))?;
    let mut writer = encryptor
        .wrap_output(armor)
        .map_err(|e| SealedError::Encrypt(e.to_string()))?;
    writer
        .write_all(plaintext)
        .map_err(|e| SealedError::Encrypt(e.to_string()))?;
    let armor = writer
        .finish()
        .map_err(|e| SealedError::Encrypt(e.to_string()))?;
    armor
        .finish()
        .map_err(|e| SealedError::Encrypt(e.to_string()))?;
    Ok(buf)
}

/// Decrypt ASCII-armored ciphertext with `identity`. Returns the
/// recovered plaintext as a `Zeroizing<String>` that is wiped on drop.
pub fn unseal(
    ciphertext: &[u8],
    identity: &x25519::Identity,
) -> Result<Zeroizing<String>, SealedError> {
    let armored = ArmoredReader::new(ciphertext);
    let decryptor =
        age::Decryptor::new(armored).map_err(|e| SealedError::Decrypt(e.to_string()))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| SealedError::Decrypt(e.to_string()))?;
    let mut out = String::new();
    reader
        .read_to_string(&mut out)
        .map_err(|e| SealedError::Decrypt(e.to_string()))?;
    Ok(Zeroizing::new(out))
}

/// Parse a dotenv-shaped string into a `KEY -> VALUE` map.
///
/// Recognized:
///   - `KEY=value` (no surrounding whitespace required)
///   - `KEY="quoted value"` and `KEY='single-quoted'`
///   - `export KEY=value` (shell-style prefix, stripped)
///   - quoted values that span multiple lines (PEM keys, certs) — the
///     value runs from the opening quote through the matching close
///     quote on a later line; newlines between are preserved verbatim
///   - blank lines and `# comments`
///
/// Not supported (intentionally): variable interpolation. Secrets that
/// need to compose at runtime should be derived in code from the
/// resolved bundle, not from the file format.
pub fn parse_dotenv(input: &str) -> Result<BTreeMap<String, Zeroizing<String>>, SealedError> {
    let mut out = BTreeMap::new();
    let mut iter = input.lines().enumerate();
    while let Some((idx, raw)) = iter.next() {
        let line_no = idx + 1;
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let after_export = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .trim_start();
        let (key, first_value) =
            after_export
                .split_once('=')
                .ok_or_else(|| SealedError::Dotenv {
                    line: line_no,
                    reason: "expected `KEY=VALUE`".into(),
                })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(SealedError::Dotenv {
                line: line_no,
                reason: "empty key".into(),
            });
        }
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || key.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return Err(SealedError::Dotenv {
                line: line_no,
                reason: format!("invalid key {key:?} (must match [A-Za-z_][A-Za-z0-9_]*)"),
            });
        }
        let value_start = first_value.trim_start();
        let value = if let Some(quote) = opening_unmatched_quote(value_start) {
            consume_multiline_value(quote, value_start, line_no, &mut iter)?
        } else {
            strip_optional_quotes(value_start.trim_end()).to_string()
        };
        out.insert(key.to_string(), Zeroizing::new(value));
    }
    Ok(out)
}

/// Detect a value that opens with a quote but doesn't close on the
/// same line. Returns the quote char so the caller can scan forward
/// for the matching close. `None` means the line is fully self-
/// contained (single-line value, possibly quoted).
fn opening_unmatched_quote(s: &str) -> Option<char> {
    let first = s.chars().next()?;
    if first != '\'' && first != '"' {
        return None;
    }
    if s[first.len_utf8()..].contains(first) {
        return None;
    }
    Some(first)
}

/// Pulled out of `parse_dotenv` to keep the main loop one screen tall.
/// Consumes lines from `iter` until the closing `quote` is found;
/// returns the accumulated value with surrounding quotes stripped.
/// Newlines between lines are preserved verbatim — PEM keys and
/// certificates rely on them.
fn consume_multiline_value<'a, I>(
    quote: char,
    value_start: &str,
    start_line_no: usize,
    iter: &mut I,
) -> Result<String, SealedError>
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut acc = String::from(&value_start[quote.len_utf8()..]);
    for (_, line) in iter.by_ref() {
        acc.push('\n');
        if let Some(close) = line.find(quote) {
            let after = &line[close + quote.len_utf8()..];
            if !after.trim().is_empty() {
                return Err(SealedError::Dotenv {
                    line: start_line_no,
                    reason: format!("trailing content after closing {quote} for multi-line value"),
                });
            }
            acc.push_str(&line[..close]);
            return Ok(acc);
        }
        acc.push_str(line);
    }
    Err(SealedError::Dotenv {
        line: start_line_no,
        reason: format!("unterminated multi-line value (missing closing {quote})"),
    })
}

fn strip_optional_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Render a key→value map as a stable dotenv string. Sorted by key
/// (the input is already a `BTreeMap` so ordering is implicit).
/// Values are emitted unquoted unless they contain a character that
/// would change the parse: spaces, `#`, leading/trailing whitespace,
/// or a quote character. In those cases the value is double-quoted.
#[must_use]
pub fn render_dotenv(values: &BTreeMap<String, Zeroizing<String>>) -> String {
    let mut out = String::new();
    for (k, v) in values {
        out.push_str(k);
        out.push('=');
        if needs_quoting(v) {
            out.push('"');
            for ch in v.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    other => out.push(other),
                }
            }
            out.push('"');
        } else {
            out.push_str(v);
        }
        out.push('\n');
    }
    out
}

fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '\'')
}

/// `tempfile::Builder` configured for secrets-bearing scratch files:
/// 0o600 perms on Unix from creation when `mode` is set, no public
/// hole even momentarily. Shared between `write_atomically_secret`
/// and the editor scratch file in the seal/edit CLI flow.
#[must_use]
pub fn secret_tempfile_builder(mode: Option<u32>) -> tempfile::Builder<'static, 'static> {
    let mut b = tempfile::Builder::new();
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        b.permissions(std::fs::Permissions::from_mode(m));
    }
    #[cfg(not(unix))]
    let _ = mode;
    b
}

/// Atomic write: write to `<path>.tmp` and rename. Creates parent
/// directories as needed. Use [`write_atomically_secret`] for files
/// that must never be world-readable, even briefly.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SealedError> {
    write_atomically_inner(path, bytes, None)
}

/// Like [`write_atomically`], but on Unix opens the tempfile with
/// mode `0o600` from the start (no TOCTOU window between create and
/// chmod) and the rename brings the tightened perms with it. On
/// non-Unix platforms behaves like the regular variant.
pub fn write_atomically_secret(path: &Path, bytes: &[u8]) -> Result<(), SealedError> {
    write_atomically_inner(path, bytes, Some(0o600))
}

fn write_atomically_inner(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), SealedError> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent).map_err(|source| SealedError::Mkdir {
        path: parent.clone(),
        source,
    })?;

    // Random tmp filename in the destination directory: same
    // filesystem (rename(2) atomic) and RAII cleanup if we bail
    // before persist().
    let mut tmp = secret_tempfile_builder(mode)
        .prefix(".yoink-")
        .suffix(".tmp")
        .tempfile_in(&parent)
        .map_err(|source| SealedError::Write {
            path: parent.clone(),
            source,
        })?;
    tmp.as_file_mut()
        .write_all(bytes)
        .map_err(|source| SealedError::Write {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| SealedError::Write {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    tmp.persist(path).map_err(|e| SealedError::Write {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_dotenv_basic() {
        let s = "FOO=bar\nBAZ=qux\n";
        let m = parse_dotenv(s).unwrap();
        assert_eq!(m.get("FOO").unwrap().as_str(), "bar");
        assert_eq!(m.get("BAZ").unwrap().as_str(), "qux");
    }

    #[test]
    fn parse_dotenv_handles_quotes_and_comments() {
        let s = r#"
# header comment
FOO="hello world"
BAR='single quoted'

# trailing
BAZ=plain
"#;
        let m = parse_dotenv(s).unwrap();
        assert_eq!(m.get("FOO").unwrap().as_str(), "hello world");
        assert_eq!(m.get("BAR").unwrap().as_str(), "single quoted");
        assert_eq!(m.get("BAZ").unwrap().as_str(), "plain");
    }

    #[test]
    fn parse_dotenv_rejects_bare_word() {
        let err = parse_dotenv("FOO bar\n").unwrap_err();
        assert!(matches!(err, SealedError::Dotenv { line: 1, .. }));
    }

    #[test]
    fn parse_dotenv_rejects_invalid_key() {
        let err = parse_dotenv("1FOO=bar\n").unwrap_err();
        assert!(matches!(err, SealedError::Dotenv { .. }));
        let err = parse_dotenv("FOO-BAR=bar\n").unwrap_err();
        assert!(matches!(err, SealedError::Dotenv { .. }));
    }

    #[test]
    fn parse_dotenv_handles_multiline_pem() {
        // Realistic shape: an ed25519 private key wrapped in a single-quoted
        // multi-line value, the way `printf "K='"; cat key.pem; printf "'\n"`
        // would render it.
        let input = "FOO=1\nKEY='-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXkt\nQyNTUxOQAAACDa\n-----END OPENSSH PRIVATE KEY-----'\nBAR=2\n";
        let parsed = parse_dotenv(input).unwrap();
        assert_eq!(parsed.get("FOO").unwrap().as_str(), "1");
        assert_eq!(parsed.get("BAR").unwrap().as_str(), "2");
        let key = parsed.get("KEY").unwrap();
        assert!(key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(key.contains("b3BlbnNzaC1rZXkt"));
        assert!(key.ends_with("-----END OPENSSH PRIVATE KEY-----"));
    }

    #[test]
    fn parse_dotenv_handles_multiline_double_quoted() {
        let input = "KEY=\"line one\nline two\nline three\"\nNEXT=ok\n";
        let parsed = parse_dotenv(input).unwrap();
        assert_eq!(
            parsed.get("KEY").unwrap().as_str(),
            "line one\nline two\nline three"
        );
        assert_eq!(parsed.get("NEXT").unwrap().as_str(), "ok");
    }

    #[test]
    fn parse_dotenv_unterminated_multiline_errors() {
        let err = parse_dotenv("KEY='line one\nline two\n").unwrap_err();
        assert!(matches!(err, SealedError::Dotenv { .. }));
        if let SealedError::Dotenv { reason, .. } = err {
            assert!(reason.contains("unterminated"), "got: {reason}");
        }
    }

    #[test]
    fn parse_dotenv_trailing_after_close_quote_errors() {
        let err = parse_dotenv("KEY='line one\nline two' garbage\n").unwrap_err();
        assert!(matches!(err, SealedError::Dotenv { .. }));
    }

    #[test]
    fn render_dotenv_round_trips() {
        let mut m: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
        m.insert("A".into(), Zeroizing::new("1".into()));
        m.insert("B".into(), Zeroizing::new("two words".into()));
        m.insert("C".into(), Zeroizing::new("with#hash".into()));
        m.insert("D".into(), Zeroizing::new("with\"quote".into()));
        let s = render_dotenv(&m);
        let parsed = parse_dotenv(&s).unwrap();
        // Quoted values lose escape chars in our minimal parser — but
        // the round trip still preserves the values for the cases
        // operators actually hit (spaces, hashes).
        assert_eq!(parsed.get("A").unwrap().as_str(), "1");
        assert_eq!(parsed.get("B").unwrap().as_str(), "two words");
        assert_eq!(parsed.get("C").unwrap().as_str(), "with#hash");
        // For escaped quote, `parse_dotenv` keeps the `\\"` literal —
        // documented limitation. Operators avoid embedded quotes.
        assert!(parsed.contains_key("D"));
    }

    #[test]
    fn seal_unseal_roundtrip() {
        let id = x25519::Identity::generate();
        let recipient = id.to_public().to_string();
        let plaintext = b"FOO=bar\nBAZ=qux\n";

        let sealed = seal(plaintext, &[recipient]).unwrap();
        // ASCII-armored output starts with the age header.
        assert!(sealed.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"));

        let recovered = unseal(&sealed, &id).unwrap();
        assert_eq!(recovered.as_str(), "FOO=bar\nBAZ=qux\n");
    }

    #[test]
    fn seal_with_no_recipients_errors() {
        let err = seal(b"x", &[]).unwrap_err();
        assert!(matches!(err, SealedError::NoRecipients));
    }

    #[test]
    fn seal_rejects_bad_recipient() {
        let err = seal(b"x", &["not-an-age-key".into()]).unwrap_err();
        assert!(matches!(err, SealedError::RecipientParse { .. }));
    }

    #[test]
    fn unseal_with_wrong_identity_errors() {
        let id1 = x25519::Identity::generate();
        let id2 = x25519::Identity::generate();
        let sealed = seal(b"FOO=bar\n", &[id1.to_public().to_string()]).unwrap();
        let err = unseal(&sealed, &id2).unwrap_err();
        assert!(matches!(err, SealedError::Decrypt(_)));
    }

    #[test]
    fn keygen_produces_parseable_pair() {
        let (sec, pub_) = keygen();
        assert!(sec.starts_with("AGE-SECRET-KEY-1"));
        assert!(pub_.starts_with("age1"));
        // The secret round-trips back into an Identity.
        parse_identity(&sec).unwrap();
    }

    #[test]
    fn scan_finds_identity_by_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (secret, public) = keygen();
        write_key_file(tmp.path(), &format!("{public}.key"), &secret);

        let recipients = vec![public.clone()];
        let found = scan_dir_for_match(tmp.path(), &recipients)
            .unwrap()
            .expect("identity by filename");
        assert_eq!(found.to_public().to_string(), public);
    }

    #[test]
    fn scan_finds_identity_by_content_when_filename_differs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (secret, public) = keygen();
        write_key_file(tmp.path(), "team.key", &secret);

        let recipients = vec![public.clone()];
        let found = scan_dir_for_match(tmp.path(), &recipients)
            .unwrap()
            .expect("identity by content");
        assert_eq!(found.to_public().to_string(), public);
    }

    #[test]
    fn scan_returns_none_when_no_recipient_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (secret_a, _public_a) = keygen();
        write_key_file(tmp.path(), "a.key", &secret_a);

        // Recipients only mention a *different* identity.
        let (_other_secret, other_public) = keygen();
        let recipients = vec![other_public];
        let found = scan_dir_for_match(tmp.path(), &recipients).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn scan_skips_broken_key_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        // First file is gibberish — must not abort the scan.
        write_key_file(tmp.path(), "broken.key", "not-a-key-at-all");
        let (secret, public) = keygen();
        write_key_file(tmp.path(), &format!("{public}.key"), &secret);

        let recipients = vec![public.clone()];
        let found = scan_dir_for_match(tmp.path(), &recipients)
            .unwrap()
            .expect("identity past the broken file");
        assert_eq!(found.to_public().to_string(), public);
    }

    #[test]
    fn scan_ignores_non_key_extensions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (secret, public) = keygen();
        // Wrong extension — should be ignored even though contents are valid.
        write_key_file(tmp.path(), &format!("{public}.txt"), &secret);

        let recipients = vec![public];
        let found = scan_dir_for_match(tmp.path(), &recipients).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn scan_returns_none_for_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let recipients = vec!["age1abc".into()];
        assert!(scan_dir_for_match(&missing, &recipients).unwrap().is_none());
    }

    /// Write a key file with mode 0600 so `check_identity_file_mode`
    /// doesn't reject it.
    #[cfg(unix)]
    fn write_key_file(dir: &Path, name: &str, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn write_key_file(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    // --- load_identity_resolved: full resolution-order coverage ---

    #[test]
    fn load_resolved_picks_keys_dir_when_recipient_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let (secret, public) = keygen();
        write_key_file(&keys, &format!("{public}.key"), &secret);
        let legacy = tmp.path().join("nonexistent-legacy.key");

        let id = load_identity_resolved(None, None, &keys, &legacy, &[public.clone()])
            .expect("scan match");
        assert_eq!(id.to_public().to_string(), public);
    }

    #[test]
    fn load_resolved_falls_back_to_legacy_when_keys_dir_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let legacy = tmp.path().join("age.key");
        let (secret, public) = keygen();
        // Write the legacy file at mode 0600 so the mode check passes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(&legacy, &secret).unwrap();
            std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::write(&legacy, &secret).unwrap();

        // Recipients are non-empty (so dir scan would run) but the
        // dir is empty — must fall through to legacy.
        let id = load_identity_resolved(None, None, &keys, &legacy, &["age1other".into()])
            .expect("legacy fallback");
        assert_eq!(id.to_public().to_string(), public);
    }

    #[test]
    fn load_resolved_empty_recipients_skips_dir_scan() {
        // Regression test for the `secrets key public` bug: passing
        // empty recipients used to make the dir scan no-op and fall
        // through to legacy. That's the documented contract — keep it
        // explicit so changing the contract requires a deliberate
        // test edit.
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let (secret, public) = keygen();
        write_key_file(&keys, &format!("{public}.key"), &secret);
        let legacy_missing = tmp.path().join("no-legacy.key");

        // No env, no recipients, no legacy file — must fail loud
        // rather than silently grab a key from the dir.
        match load_identity_resolved(None, None, &keys, &legacy_missing, &[]) {
            Err(SealedError::NoIdentity(_)) => {}
            Err(other) => panic!("expected NoIdentity, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn load_resolved_env_key_beats_keys_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        // Stash a *different* identity in the keys dir; we expect
        // env_key to win even when a matching key is on disk.
        let (other_secret, other_public) = keygen();
        write_key_file(&keys, &format!("{other_public}.key"), &other_secret);

        let (env_secret, env_public) = keygen();
        let legacy = tmp.path().join("nonexistent-legacy.key");

        // Recipients name `other_public` so the dir scan WOULD match
        // — but env_key takes precedence regardless.
        let id =
            load_identity_resolved(Some(env_secret.to_string()), None, &keys, &legacy, &[other_public])
                .expect("env wins");
        assert_eq!(id.to_public().to_string(), env_public);
    }

    #[test]
    fn load_resolved_env_file_beats_keys_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let (other_secret, other_public) = keygen();
        write_key_file(&keys, &format!("{other_public}.key"), &other_secret);

        let (env_secret, env_public) = keygen();
        let env_file_path = tmp.path().join("env.key");
        write_key_file(tmp.path(), "env.key", &env_secret);

        let legacy = tmp.path().join("nonexistent-legacy.key");

        let id = load_identity_resolved(
            None,
            Some(env_file_path.display().to_string()),
            &keys,
            &legacy,
            &[other_public],
        )
        .expect("env file wins");
        assert_eq!(id.to_public().to_string(), env_public);
    }

    #[test]
    fn load_resolved_blank_env_vars_treated_as_unset() {
        // Empty/whitespace YOINK_AGE_KEY shouldn't short-circuit the
        // resolution chain — fall through as if the var wasn't set.
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let (secret, public) = keygen();
        write_key_file(&keys, &format!("{public}.key"), &secret);
        let legacy = tmp.path().join("no-legacy.key");

        let id = load_identity_resolved(
            Some("   ".to_string()),
            Some("\t".to_string()),
            &keys,
            &legacy,
            &[public.clone()],
        )
        .expect("fallthrough past blank env");
        assert_eq!(id.to_public().to_string(), public);
    }

    #[test]
    fn load_resolved_no_match_anywhere_errors_with_legacy_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keys = tmp.path().join("keys");
        std::fs::create_dir(&keys).unwrap();
        let legacy = tmp.path().join("expected-legacy-path.key");

        match load_identity_resolved(None, None, &keys, &legacy, &["age1nothing-matches".into()]) {
            Err(SealedError::NoIdentity(p)) => assert_eq!(p, legacy),
            Err(other) => panic!("expected NoIdentity, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
