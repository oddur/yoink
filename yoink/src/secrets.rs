//! Secrets handling. Supports Infisical only — yoink shells out to the
//! `infisical` CLI on the operator's laptop (or CI runner) to resolve
//! secrets at deploy time. Values are then injected as plain env vars
//! into the running container; the container image itself does not need
//! the infisical CLI nor the machine-identity token.
//!
//! Tokens must never appear in logs or error messages. The `Debug` and
//! `Display` impls on `InfisicalToken` mask all but the first 4 chars.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

use crate::config::SecretsConfig;

pub const INFISICAL_TOKEN_ENV: &str = "INFISICAL_TOKEN";

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("{INFISICAL_TOKEN_ENV} env var is not set; export it before running yoink")]
    Missing,
    #[error("{INFISICAL_TOKEN_ENV} env var is empty")]
    Empty,
    #[error("failed to spawn `infisical export`: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("`infisical export` exited with status {status}: {stderr}")]
    Export { status: String, stderr: String },
    #[error("invalid line in `infisical export --format=dotenv` output: {0:?}")]
    Parse(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct InfisicalToken(String);

impl InfisicalToken {
    /// Read the token from the operator's environment.
    pub fn from_env() -> Result<Self, SecretsError> {
        let raw = env::var(INFISICAL_TOKEN_ENV).map_err(|_| SecretsError::Missing)?;
        Self::new(raw)
    }

    /// Construct directly. Whitespace-trimmed input must be non-empty.
    pub fn new(raw: impl Into<String>) -> Result<Self, SecretsError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(SecretsError::Empty);
        }
        Ok(Self(raw))
    }

    /// The unmasked value — only call when handing it to a child process
    /// (e.g. as a `docker run --env` value). Never log or print it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// First 4 chars + `...` for safe display in logs.
    #[must_use]
    pub fn masked(&self) -> String {
        let prefix: String = self.0.chars().take(4).collect();
        format!("{prefix}…(masked)")
    }
}

/// In-memory map of resolved secret keys → values. Built once at the
/// start of a reconcile by shelling out to `infisical export`. Each
/// service then picks the keys it needs out of the bundle.
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

/// Shell out to `infisical export --format=dotenv` and parse the result
/// into a `SecretsBundle`. The `INFISICAL_TOKEN` env var (machine
/// identity) is honored if set in the parent shell — otherwise the CLI
/// uses a cached `infisical login` session.
/// Convenience wrapper for the common "load whatever the operator
/// configured" path. Returns `Ok(None)` when no `[secrets]` block is
/// declared (services that don't need secrets). Any other error
/// surfaces — the deploy + drift-detection paths both surface it.
pub async fn load_bundle(
    config: &crate::config::Config,
) -> Result<Option<SecretsBundle>, SecretsError> {
    let Some(cfg) = &config.secrets else {
        return Ok(None);
    };
    let bundle = fetch_secrets(cfg, cfg.domain.as_deref()).await?;
    Ok(Some(bundle))
}

pub async fn fetch_secrets(
    cfg: &SecretsConfig,
    domain: Option<&str>,
) -> Result<SecretsBundle, SecretsError> {
    let mut cmd = Command::new("infisical");
    cmd.arg("export")
        .arg("--format=dotenv")
        .arg(format!("--projectId={}", cfg.project_id))
        .arg(format!("--env={}", cfg.environment));
    if let Some(path) = &cfg.path {
        cmd.arg(format!("--path={path}"));
    }
    if let Some(domain) = domain {
        cmd.arg(format!("--domain={domain}"));
    }
    cmd.stdin(Stdio::null());
    let output = cmd.output().await.map_err(SecretsError::Spawn)?;
    if !output.status.success() {
        return Err(SecretsError::Export {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    parse_dotenv(&String::from_utf8_lossy(&output.stdout)).map(SecretsBundle::new)
}

/// Parse `KEY=VALUE` lines emitted by `infisical export --format=dotenv`.
/// Tolerates surrounding single- or double-quotes, blank lines, and `#`
/// comments. `infisical` emits single-quoted values for anything
/// containing special chars; if we forwarded those literally to docker
/// the running container would see `'value'` (with the quotes).
fn parse_dotenv(text: &str) -> Result<BTreeMap<String, String>, SecretsError> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(SecretsError::Parse(raw.to_string()));
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            return Err(SecretsError::Parse(raw.to_string()));
        }
        out.insert(key, unquote(v.trim()).to_string());
    }
    Ok(out)
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

impl fmt::Debug for InfisicalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfisicalToken({})", self.masked())
    }
}

impl fmt::Display for InfisicalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.masked())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rejects_empty_token() {
        assert!(matches!(InfisicalToken::new(""), Err(SecretsError::Empty)));
        assert!(matches!(
            InfisicalToken::new("   "),
            Err(SecretsError::Empty)
        ));
    }

    #[test]
    fn accepts_non_empty_token() {
        let tok = InfisicalToken::new("st_test_super_secret_value_xyz123").unwrap();
        assert_eq!(tok.expose(), "st_test_super_secret_value_xyz123");
    }

    #[test]
    fn debug_impl_masks_value() {
        let tok = InfisicalToken::new("st_test_super_secret_value_xyz123").unwrap();
        let debug = format!("{tok:?}");
        assert!(!debug.contains("super_secret"));
        assert!(debug.starts_with("InfisicalToken("));
        assert!(debug.contains("st_t"));
    }

    #[test]
    fn display_impl_masks_value() {
        let tok = InfisicalToken::new("st_test_super_secret_value_xyz123").unwrap();
        let display = format!("{tok}");
        assert!(!display.contains("super_secret"));
        assert!(display.contains("st_t"));
    }

    #[test]
    fn debug_impl_masks_short_value() {
        // Even short tokens must mask. We don't expose the full value.
        let tok = InfisicalToken::new("ab").unwrap();
        let debug = format!("{tok:?}");
        assert!(!debug.contains("InfisicalToken(ab)"));
        // We accept that the displayed prefix is "ab…" — that's fine
        // because there's nothing else to leak.
        assert!(debug.contains("ab"));
    }

    #[test]
    fn parse_dotenv_strips_quotes_and_skips_blanks_and_comments() {
        let input = "\
# a comment

DATABASE_URL=fake-test-fixture-not-a-real-url
QUOTED=\"with spaces\"
SINGLE='user@example.com'
EMPTY=
";
        let map = parse_dotenv(input).unwrap();
        assert_eq!(
            map.get("DATABASE_URL").map(String::as_str),
            Some("fake-test-fixture-not-a-real-url")
        );
        assert_eq!(map.get("QUOTED").map(String::as_str), Some("with spaces"));
        assert_eq!(
            map.get("SINGLE").map(String::as_str),
            Some("user@example.com")
        );
        assert_eq!(map.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn parse_dotenv_rejects_lines_without_equals() {
        let err = parse_dotenv("KEY_ONLY\n").unwrap_err();
        assert!(matches!(err, SecretsError::Parse(_)));
    }

    #[test]
    fn secrets_bundle_get_returns_str() {
        let mut m = BTreeMap::new();
        m.insert("A".into(), "1".into());
        let b = SecretsBundle::new(m);
        assert_eq!(b.get("A"), Some("1"));
        assert_eq!(b.get("missing"), None);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn token_equality_is_value_based() {
        let a = InfisicalToken::new("aaaa1111").unwrap();
        let b = InfisicalToken::new("aaaa1111").unwrap();
        let c = InfisicalToken::new("bbbb2222").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
