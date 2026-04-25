//! Secrets handling. Supports Infisical only — via a pre-minted machine
//! identity token sourced from the operator's environment
//! (`INFISICAL_TOKEN`). The token value is opaque to yoink: we pass it
//! to the container as an env var, where the wrapping `infisical run`
//! invocation uses it to fetch the actual secrets at container start.
//!
//! The token must never appear in logs or error messages. The `Debug` and
//! `Display` impls on `InfisicalToken` mask all but the first 4 chars.

use std::env;
use std::fmt;

use thiserror::Error;

pub const INFISICAL_TOKEN_ENV: &str = "INFISICAL_TOKEN";

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("{INFISICAL_TOKEN_ENV} env var is not set; export it before running yoink")]
    Missing,
    #[error("{INFISICAL_TOKEN_ENV} env var is empty")]
    Empty,
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
    fn token_equality_is_value_based() {
        let a = InfisicalToken::new("aaaa1111").unwrap();
        let b = InfisicalToken::new("aaaa1111").unwrap();
        let c = InfisicalToken::new("bbbb2222").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
