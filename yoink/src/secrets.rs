//! Secrets handling. Talks to Infisical's REST API directly — yoink no
//! longer requires the `infisical` CLI binary to be installed at deploy
//! time. Three auth modes, tried in order:
//!
//!   1. Universal Auth (machine identity) — `INFISICAL_CLIENT_ID` +
//!      `INFISICAL_CLIENT_SECRET` env vars. Recommended for CI.
//!   2. Raw bearer token — `INFISICAL_TOKEN` env var. For one-off /
//!      power-user runs where the operator already has a token.
//!   3. Cached browser-flow login — read by `infisical login`'s persisted
//!      session in `~/.infisical/infisical-config.json` + the OS keyring
//!      entry it wrote. The recommended laptop dev path: run
//!      `infisical login` once, then yoink reuses the same session.
//!
//! Tokens must never appear in logs or error messages. The `Debug` and
//! `Display` impls on `InfisicalToken` mask all but the first 4 chars.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::path::PathBuf;

use base64::Engine;
use serde::Deserialize;
use thiserror::Error;

use crate::config::SecretsConfig;

pub const INFISICAL_TOKEN_ENV: &str = "INFISICAL_TOKEN";
pub const INFISICAL_CLIENT_ID_ENV: &str = "INFISICAL_CLIENT_ID";
pub const INFISICAL_CLIENT_SECRET_ENV: &str = "INFISICAL_CLIENT_SECRET";

const DEFAULT_BASE_URL: &str = "https://app.infisical.com";
const KEYRING_SERVICE: &str = "infisical-cli";

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error(
        "no Infisical credentials available — set {INFISICAL_CLIENT_ID_ENV}+{INFISICAL_CLIENT_SECRET_ENV}, set {INFISICAL_TOKEN_ENV}, or run `infisical login`"
    )]
    NoAuth,
    #[error("{INFISICAL_TOKEN_ENV} env var is empty")]
    EmptyToken,
    #[error("failed to read infisical config at {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse infisical config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("infisical config at {0} has no `loggedInUserEmail` — run `infisical login`")]
    NoLoggedInUser(PathBuf),
    #[error("OS keyring lookup failed for `{KEYRING_SERVICE}`/{email}: {source}")]
    Keyring {
        email: String,
        #[source]
        source: keyring::Error,
    },
    #[error("cached infisical session for {0} not found — run `infisical login`")]
    KeyringNotFound(String),
    #[error("cached infisical session for {email} is malformed: {source}")]
    KeyringParse {
        email: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("cached infisical session for {0} has no JWT — run `infisical login`")]
    KeyringNoToken(String),
    #[error("HTTP request to Infisical failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Infisical API returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("`HOME` env var not set — cannot locate `~/.infisical/infisical-config.json`")]
    NoHome,
}

#[derive(Clone, PartialEq, Eq)]
pub struct InfisicalToken(String);

impl InfisicalToken {
    /// Read the token from the operator's environment.
    pub fn from_env() -> Result<Self, SecretsError> {
        let raw = env::var(INFISICAL_TOKEN_ENV).map_err(|_| SecretsError::NoAuth)?;
        Self::new(raw)
    }

    /// Construct directly. Whitespace-trimmed input must be non-empty.
    pub fn new(raw: impl Into<String>) -> Result<Self, SecretsError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(SecretsError::EmptyToken);
        }
        Ok(Self(raw))
    }

    /// The unmasked value — only call when handing it to a child process
    /// (e.g. as a `docker run --env` value). Never log or print it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// First 4 chars + `…(masked)` for safe display in logs.
    #[must_use]
    pub fn masked(&self) -> String {
        let prefix: String = self.0.chars().take(4).collect();
        format!("{prefix}…(masked)")
    }
}

/// In-memory map of resolved secret keys → values. Built once at the
/// start of a reconcile by calling Infisical's REST API. Each service
/// then picks the keys it needs out of the bundle.
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

/// Fetch all secrets in the configured project + environment + path,
/// returning a `SecretsBundle`. `domain_override` (typically
/// `cfg.domain`) wins over the cached login's stored domain when both
/// are present.
pub async fn fetch_secrets(
    cfg: &SecretsConfig,
    domain_override: Option<&str>,
) -> Result<SecretsBundle, SecretsError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("yoink/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let (base_url, bearer) = resolve_auth(&http, domain_override).await?;

    let path = cfg.path.as_deref().unwrap_or("/");
    let url = format!("{base_url}/api/v3/secrets/raw");
    let resp = http
        .get(&url)
        .bearer_auth(&bearer)
        .query(&[
            ("workspaceId", cfg.project_id.as_str()),
            ("environment", cfg.environment.as_str()),
            ("secretPath", path),
        ])
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(SecretsError::Api {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: RawSecretsResponse = resp.json().await?;
    let map = parsed
        .secrets
        .into_iter()
        .map(|s| (s.secret_key, s.secret_value))
        .collect();
    Ok(SecretsBundle::new(map))
}

#[derive(Deserialize)]
struct RawSecretsResponse {
    secrets: Vec<RawSecret>,
}

#[derive(Deserialize)]
struct RawSecret {
    #[serde(rename = "secretKey")]
    secret_key: String,
    #[serde(rename = "secretValue")]
    secret_value: String,
}

/// Resolve `(base_url, bearer_token)` using the auth-mode priority
/// documented at the top of this file.
async fn resolve_auth(
    http: &reqwest::Client,
    domain_override: Option<&str>,
) -> Result<(String, String), SecretsError> {
    if let (Ok(client_id), Ok(client_secret)) = (
        env::var(INFISICAL_CLIENT_ID_ENV),
        env::var(INFISICAL_CLIENT_SECRET_ENV),
    ) && !client_id.trim().is_empty()
        && !client_secret.trim().is_empty()
    {
        let base = normalize_base(domain_override.unwrap_or(DEFAULT_BASE_URL));
        let token =
            universal_auth_login(http, &base, client_id.trim(), client_secret.trim()).await?;
        return Ok((base, token));
    }

    if let Ok(raw) = env::var(INFISICAL_TOKEN_ENV) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SecretsError::EmptyToken);
        }
        let base = normalize_base(domain_override.unwrap_or(DEFAULT_BASE_URL));
        return Ok((base, trimmed.to_string()));
    }

    let cached = load_cached_session()?;
    let base = normalize_base(
        domain_override
            .or(cached.domain.as_deref())
            .unwrap_or(DEFAULT_BASE_URL),
    );
    Ok((base, cached.jwt))
}

#[derive(Deserialize)]
struct UniversalAuthResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
}

async fn universal_auth_login(
    http: &reqwest::Client,
    base: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<String, SecretsError> {
    let url = format!("{base}/api/v1/auth/universal-auth/login");
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "clientId": client_id,
            "clientSecret": client_secret,
        }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(SecretsError::Api {
            status: status.as_u16(),
            body,
        });
    }
    Ok(resp.json::<UniversalAuthResponse>().await?.access_token)
}

struct CachedSession {
    jwt: String,
    /// Whatever the CLI stored in `LoggedInUserDomain` — may include a
    /// trailing `/api`. `normalize_base` strips it.
    domain: Option<String>,
}

#[derive(Deserialize)]
struct InfisicalConfigFile {
    #[serde(default, rename = "loggedInUserEmail")]
    logged_in_user_email: String,
    #[serde(default, rename = "LoggedInUserDomain")]
    logged_in_user_domain: String,
}

#[derive(Deserialize)]
struct CachedUserCreds {
    /// Yes, the upstream CLI stores this as `JTWToken` (a typo of JWT).
    /// We follow the typo so deserialization matches the on-disk shape.
    #[serde(default, rename = "JTWToken")]
    jwt_token: String,
}

fn load_cached_session() -> Result<CachedSession, SecretsError> {
    let home = env::var("HOME").map_err(|_| SecretsError::NoHome)?;
    let path = PathBuf::from(home).join(".infisical/infisical-config.json");
    let raw = std::fs::read_to_string(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            SecretsError::NoAuth
        } else {
            SecretsError::ConfigRead {
                path: path.clone(),
                source,
            }
        }
    })?;
    let cfg: InfisicalConfigFile =
        serde_json::from_str(&raw).map_err(|source| SecretsError::ConfigParse {
            path: path.clone(),
            source,
        })?;
    if cfg.logged_in_user_email.is_empty() {
        return Err(SecretsError::NoLoggedInUser(path));
    }
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, &cfg.logged_in_user_email).map_err(|source| {
            SecretsError::Keyring {
                email: cfg.logged_in_user_email.clone(),
                source,
            }
        })?;
    let stored = match entry.get_password() {
        Ok(s) => s,
        Err(keyring::Error::NoEntry) => {
            return Err(SecretsError::KeyringNotFound(cfg.logged_in_user_email));
        }
        Err(source) => {
            return Err(SecretsError::Keyring {
                email: cfg.logged_in_user_email,
                source,
            });
        }
    };
    let json = decode_go_keyring_value(&stored).map_err(|source| SecretsError::KeyringParse {
        email: cfg.logged_in_user_email.clone(),
        source,
    })?;
    let creds: CachedUserCreds =
        serde_json::from_str(&json).map_err(|source| SecretsError::KeyringParse {
            email: cfg.logged_in_user_email.clone(),
            source,
        })?;
    if creds.jwt_token.trim().is_empty() {
        return Err(SecretsError::KeyringNoToken(cfg.logged_in_user_email));
    }
    let domain = if cfg.logged_in_user_domain.is_empty() {
        None
    } else {
        Some(cfg.logged_in_user_domain)
    };
    Ok(CachedSession {
        jwt: creds.jwt_token,
        domain,
    })
}

/// `go-keyring` (used by the infisical CLI) wraps stored values with one
/// of two prefixes before handing them to the OS keychain — the macOS
/// keychain mangles non-ASCII bytes, so the Go library encodes
/// everything. We unwrap both prefixes so callers see the original
/// JSON regardless of which the CLI version chose.
fn decode_go_keyring_value(raw: &str) -> Result<String, serde_json::Error> {
    if let Some(rest) = raw.strip_prefix("go-keyring-base64:") {
        return base64::engine::general_purpose::STANDARD
            .decode(rest)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| serde::de::Error::custom("malformed go-keyring-base64 value"));
    }
    if let Some(rest) = raw.strip_prefix("go-keyring-encoded:") {
        return decode_hex(rest)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| serde::de::Error::custom("malformed go-keyring-encoded value"));
    }
    Ok(raw.to_string())
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Strip a trailing `/api` so callers can append `/api/...` paths
/// uniformly. The CLI's cached `LoggedInUserDomain` includes `/api`;
/// the bare `domain` setting in `yoink.yaml` does not.
fn normalize_base(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    trimmed
        .strip_suffix("/api")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
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
        assert!(matches!(
            InfisicalToken::new(""),
            Err(SecretsError::EmptyToken)
        ));
        assert!(matches!(
            InfisicalToken::new("   "),
            Err(SecretsError::EmptyToken)
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
        let tok = InfisicalToken::new("ab").unwrap();
        let debug = format!("{tok:?}");
        assert!(!debug.contains("InfisicalToken(ab)"));
        assert!(debug.contains("ab"));
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

    #[test]
    fn decode_go_keyring_value_handles_all_prefixes() {
        assert_eq!(decode_go_keyring_value("plain").unwrap(), "plain");
        // base64 of `{"a":1}`
        assert_eq!(
            decode_go_keyring_value("go-keyring-base64:eyJhIjoxfQ==").unwrap(),
            r#"{"a":1}"#
        );
        // hex of `{"a":1}`
        assert_eq!(
            decode_go_keyring_value("go-keyring-encoded:7b2261223a317d").unwrap(),
            r#"{"a":1}"#
        );
        assert!(decode_go_keyring_value("go-keyring-base64:!!!not-base64!!!").is_err());
    }

    #[test]
    fn normalize_base_strips_trailing_api() {
        assert_eq!(normalize_base("https://example.com"), "https://example.com");
        assert_eq!(
            normalize_base("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(
            normalize_base("https://example.com/api"),
            "https://example.com"
        );
        assert_eq!(
            normalize_base("https://example.com/api/"),
            "https://example.com"
        );
    }
}
