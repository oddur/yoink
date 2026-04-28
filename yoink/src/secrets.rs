//! Secrets handling. Two providers, dispatched by `secrets.provider`
//! in `yoink.yaml`:
//!
//!   - **`age`** (the batteries-included default) — a single sealed
//!     dotenv file committed to the repo, decrypted at deploy time
//!     with one identity resolved from `YOINK_AGE_KEY` (raw),
//!     `YOINK_AGE_KEY_FILE` (path), or `~/.config/yoink/age.key`.
//!     Encrypt-side helpers live in [`crate::sealed`]; key-management
//!     CLI is `yoink secrets key …`.
//!   - **`command`** — yoink invokes the configured command and reads
//!     a secrets bundle from its stdout. Auto-detects between dotenv
//!     and JSON. Lets operators wire any external manager (1Password
//!     `op inject`, Doppler `doppler secrets download --format env`,
//!     Vault, AWS Secrets Manager, the Infisical CLI, …) without
//!     yoink growing first-party integrations for each.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::{Config, SecretsConfig, SecretsFormat};
use crate::sealed;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("read sealed secrets file {path}: {source}")]
    SealedRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("decrypt sealed secrets file {path}: {source}")]
    SealedDecrypt {
        path: PathBuf,
        #[source]
        source: sealed::SealedError,
    },
    #[error("parse sealed dotenv contents from {path}: {source}")]
    SealedParse {
        path: PathBuf,
        #[source]
        source: sealed::SealedError,
    },
    #[error("locate age identity for sealed secrets: {0}")]
    NoAgeIdentity(#[source] sealed::SealedError),
    #[error("`secrets.command:` is empty — needs at least the binary name")]
    CommandMissing,
    #[error("spawn `{command}`: {source}")]
    CommandSpawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{command}` exited with status {status}: {stderr}")]
    CommandExit {
        command: String,
        status: i32,
        stderr: String,
    },
    #[error("parse secrets bundle from `{command}` (treated as {format}): {detail}")]
    CommandParse {
        command: String,
        format: &'static str,
        detail: String,
    },
}

/// In-memory map of resolved secret keys → values. Built once at the
/// start of a reconcile. Each service then picks the keys it needs.
#[derive(Clone, Default)]
pub struct SecretsBundle {
    values: BTreeMap<String, String>,
}

impl SecretsBundle {
    #[must_use]
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl fmt::Debug for SecretsBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretsBundle")
            .field("len", &self.values.len())
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Load whatever the operator configured. `Ok(None)` when no
/// `[secrets]` block is declared (services that don't need secrets).
pub async fn load_bundle(config: &Config) -> Result<Option<SecretsBundle>, SecretsError> {
    let Some(cfg) = &config.secrets else {
        return Ok(None);
    };
    let bundle = match cfg {
        SecretsConfig::Age { file, .. } => {
            let path = sealed::resolve_sealed_path(config, file.as_deref());
            load_age_bundle(&path)?
        }
        SecretsConfig::Command { command, format } => {
            load_command_bundle(command, *format).await?
        }
    };
    Ok(Some(bundle))
}

fn load_age_bundle(path: &Path) -> Result<SecretsBundle, SecretsError> {
    let bytes = std::fs::read(path).map_err(|source| SecretsError::SealedRead {
        path: path.to_path_buf(),
        source,
    })?;
    let identity = sealed::load_identity().map_err(SecretsError::NoAgeIdentity)?;
    let plaintext =
        sealed::unseal(&bytes, &identity).map_err(|source| SecretsError::SealedDecrypt {
            path: path.to_path_buf(),
            source,
        })?;
    let map = sealed::parse_dotenv(&plaintext).map_err(|source| SecretsError::SealedParse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(SecretsBundle::new(map))
}

async fn load_command_bundle(
    command: &[String],
    format: SecretsFormat,
) -> Result<SecretsBundle, SecretsError> {
    let Some((bin, args)) = command.split_first() else {
        return Err(SecretsError::CommandMissing);
    };
    let pretty = command.join(" ");

    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| SecretsError::CommandSpawn {
            command: pretty.clone(),
            source,
        })?;

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut stdout_buf).await.ok();
    }
    if let Some(mut err) = child.stderr.take() {
        err.read_to_end(&mut stderr_buf).await.ok();
    }
    let status = child
        .wait()
        .await
        .map_err(|source| SecretsError::CommandSpawn {
            command: pretty.clone(),
            source,
        })?;
    if !status.success() {
        return Err(SecretsError::CommandExit {
            command: pretty,
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr_buf).trim().to_string(),
        });
    }

    parse_bundle_bytes(&stdout_buf, format).map_err(|(format, detail)| {
        SecretsError::CommandParse {
            command: pretty,
            format,
            detail,
        }
    })
}

/// Pick a parser for `bytes`: `auto` looks at the first non-whitespace
/// byte (`{` → JSON, anything else → dotenv); `dotenv` / `json` force
/// the parser. Returns `(format_label, detail)` on parse failure so
/// the caller can build an error.
fn parse_bundle_bytes(
    bytes: &[u8],
    format: SecretsFormat,
) -> Result<SecretsBundle, (&'static str, String)> {
    let chosen = match format {
        SecretsFormat::Json => SecretsFormat::Json,
        SecretsFormat::Dotenv => SecretsFormat::Dotenv,
        SecretsFormat::Auto => {
            if bytes
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| *b == b'{')
            {
                SecretsFormat::Json
            } else {
                SecretsFormat::Dotenv
            }
        }
    };
    match chosen {
        SecretsFormat::Json => parse_json_bundle(bytes).map_err(|d| ("json", d)),
        SecretsFormat::Dotenv | SecretsFormat::Auto => {
            parse_dotenv_bundle(bytes).map_err(|d| ("dotenv", d))
        }
    }
}

fn parse_json_bundle(bytes: &[u8]) -> Result<SecretsBundle, String> {
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = raw.as_object().ok_or_else(|| {
        "expected a top-level JSON object of \"KEY\":\"VALUE\" pairs".to_string()
    })?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in obj {
        let value = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => continue,
            other => other.to_string(),
        };
        out.insert(k.clone(), value);
    }
    Ok(SecretsBundle::new(out))
}

/// Minimal dotenv parser — same shape `crate::sealed::parse_dotenv`
/// uses on the age plaintext. Comments (`#…`) and blank lines are
/// dropped; values may be wrapped in single or double quotes; surrounding
/// whitespace is trimmed; an unrecognised line is a parse error
/// (better to fail loudly than silently drop a malformed export).
fn parse_dotenv_bundle(bytes: &[u8]) -> Result<SecretsBundle, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not valid UTF-8: {e}"))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (i, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip an optional leading `export ` (dotenv tools sometimes
        // emit it for shell-source ergonomics).
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (key, value) = line.split_once('=').ok_or_else(|| {
            format!("line {}: expected `KEY=VALUE`, got {raw_line:?}", i + 1)
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", i + 1));
        }
        let value = strip_quotes(value.trim());
        out.insert(key.to_string(), value);
    }
    Ok(SecretsBundle::new(out))
}

fn strip_quotes(raw: &str) -> String {
    if raw.len() >= 2
        && let (Some(first), Some(last)) = (raw.chars().next(), raw.chars().last())
        && first == last
        && (first == '"' || first == '\'')
    {
        return raw[1..raw.len() - 1].to_string();
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_bundle_basic() {
        let bytes = br#"{"FOO":"1","BAR":"two words"}"#;
        let bundle = parse_json_bundle(bytes).unwrap();
        assert_eq!(bundle.get("FOO"), Some("1"));
        assert_eq!(bundle.get("BAR"), Some("two words"));
    }

    #[test]
    fn parse_json_bundle_drops_nulls_and_stringifies_numbers() {
        let bytes = br#"{"PORT":8080,"NIL":null,"NAME":"x"}"#;
        let bundle = parse_json_bundle(bytes).unwrap();
        assert_eq!(bundle.get("PORT"), Some("8080"));
        assert_eq!(bundle.get("NIL"), None);
        assert_eq!(bundle.get("NAME"), Some("x"));
    }

    #[test]
    fn parse_json_bundle_rejects_top_level_array() {
        let bytes = br#"[{"K":"V"}]"#;
        assert!(parse_json_bundle(bytes).is_err());
    }

    #[test]
    fn parse_dotenv_bundle_basic() {
        let bytes = b"FOO=1\n# comment\nBAR=\"hello\"\nexport BAZ='spaced value'\n";
        let bundle = parse_dotenv_bundle(bytes).unwrap();
        assert_eq!(bundle.get("FOO"), Some("1"));
        assert_eq!(bundle.get("BAR"), Some("hello"));
        assert_eq!(bundle.get("BAZ"), Some("spaced value"));
    }

    #[test]
    fn parse_dotenv_bundle_rejects_malformed_lines() {
        let bytes = b"VALID=1\nthisisnotanassignment\n";
        let err = parse_dotenv_bundle(bytes).unwrap_err();
        assert!(err.contains("expected `KEY=VALUE`"), "got: {err}");
    }

    #[test]
    fn auto_detect_picks_json_for_brace_prefix() {
        let bytes = br#"  {"K":"V"}"#;
        let bundle = parse_bundle_bytes(bytes, SecretsFormat::Auto).unwrap();
        assert_eq!(bundle.get("K"), Some("V"));
    }

    #[test]
    fn auto_detect_falls_back_to_dotenv() {
        let bytes = b"# header\nFOO=bar\n";
        let bundle = parse_bundle_bytes(bytes, SecretsFormat::Auto).unwrap();
        assert_eq!(bundle.get("FOO"), Some("bar"));
    }

    #[test]
    fn explicit_dotenv_overrides_brace_prefix_value() {
        // A dotenv value that legitimately starts with `{` (e.g. JSON
        // payload as an env value) — operator should set
        // `format: dotenv` to disambiguate. Verifies the explicit
        // setting wins over the auto-detect heuristic.
        let bytes = b"PAYLOAD={\"k\":1}\n";
        let bundle = parse_bundle_bytes(bytes, SecretsFormat::Dotenv).unwrap();
        assert_eq!(bundle.get("PAYLOAD"), Some(r#"{"k":1}"#));
    }

    #[tokio::test]
    async fn load_command_bundle_runs_command_and_parses_dotenv() {
        let bundle = load_command_bundle(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "printf 'A=1\\nB=two\\n'".into(),
            ],
            SecretsFormat::Auto,
        )
        .await
        .unwrap();
        assert_eq!(bundle.get("A"), Some("1"));
        assert_eq!(bundle.get("B"), Some("two"));
    }

    #[tokio::test]
    async fn load_command_bundle_surfaces_nonzero_exit() {
        let err = load_command_bundle(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "echo oh no >&2; exit 7".into(),
            ],
            SecretsFormat::Auto,
        )
        .await
        .unwrap_err();
        let SecretsError::CommandExit { status, stderr, .. } = err else {
            panic!("expected CommandExit, got: {err:?}");
        };
        assert_eq!(status, 7);
        assert!(stderr.contains("oh no"), "stderr: {stderr}");
    }
}
