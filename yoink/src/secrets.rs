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
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::{Config, SecretsConfig, SecretsFormat};
use crate::sealed;

/// Cap on stdout (and stderr) bytes read from a `provider: command`
/// child. Real bundles are well under 100 KB; the limit defends
/// against a buggy or malicious provider that runs away with stdout
/// and OOMs the deploy / CI runner.
const COMMAND_OUTPUT_CAP: usize = 10 * 1024 * 1024;
/// Hard wall-clock deadline for a `provider: command` invocation. A
/// stuck `op read` / `vault kv get` would otherwise hang `yoink up`
/// indefinitely. Picked to comfortably cover slow KMS round-trips
/// without surprising operators.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Env vars that survive `env_clear()` when spawning a secrets
/// provider. Most CLIs need at least PATH; HOME / USER are commonly
/// referenced for default-config-file resolution. Anything else
/// (`DOPPLER_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`, `AWS_*`, `VAULT_*`) is the
/// operator's responsibility to wire through their environment —
/// see the docs.
const COMMAND_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TERM",
    "TZ",
    // Per-tool auth tokens commonly needed; passing through is
    // safer than forcing operators to wrap every CLI in a script
    // that re-exports them.
    "DOPPLER_TOKEN",
    "INFISICAL_TOKEN",
    "INFISICAL_CLIENT_ID",
    "INFISICAL_CLIENT_SECRET",
    "OP_SERVICE_ACCOUNT_TOKEN",
    "OP_CONNECT_HOST",
    "OP_CONNECT_TOKEN",
    "VAULT_ADDR",
    "VAULT_TOKEN",
    "VAULT_NAMESPACE",
    "VAULT_CACERT",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
    "BWS_ACCESS_TOKEN",
    "SOPS_AGE_KEY",
    "SOPS_AGE_KEY_FILE",
];

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
    #[error(
        "`{command}` did not finish within {timeout:?} — increase the wait or fix the upstream"
    )]
    CommandTimeout { command: String, timeout: Duration },
    #[error(
        "`{command}` produced more than {cap} bytes on {stream} — provider misbehaving or output not bundle-shaped"
    )]
    CommandOutputCap {
        command: String,
        stream: &'static str,
        cap: usize,
    },
    #[error(
        "secrets bundle from `{command}` is empty, but services declare `secrets:` keys — check provider auth + scope"
    )]
    CommandEmpty { command: String },
    #[error(
        "sealed bundle decrypted to an empty map, but services declare `secrets:` keys — re-seal with `yoink secrets edit`"
    )]
    SealedEmpty,
    #[error(transparent)]
    Sealed(#[from] sealed::SealedError),
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
///
/// An empty bundle is a hard error when *any* service in the config
/// references secret keys (`secrets:` / `env_from_secrets:`) — that
/// scenario almost always means the provider auth/scope is wrong
/// rather than "you legitimately have zero secrets". `provider:
/// command` returns its own `CommandEmpty` (we have the command
/// string for the error message); the age path returns `SealedEmpty`.
pub async fn load_bundle(config: &Config) -> Result<Option<SecretsBundle>, SecretsError> {
    let Some(cfg) = &config.secrets else {
        return Ok(None);
    };
    let any_secrets_referenced = config_references_secrets(config);
    let bundle = match cfg {
        SecretsConfig::Age { file, .. } => {
            let path = sealed::resolve_sealed_path(config, file.as_deref())?;
            load_age_bundle(&path, any_secrets_referenced)?
        }
        SecretsConfig::Command { command, format } => {
            load_command_bundle(command, *format, any_secrets_referenced).await?
        }
    };
    Ok(Some(bundle))
}

fn config_references_secrets(config: &Config) -> bool {
    let any_in = |s: &crate::config::ServiceConfig| -> bool {
        !s.secrets.is_empty() || !s.env_from_secrets.is_empty()
    };
    if config.services.iter().any(any_in) {
        return true;
    }
    if let Some(reg) = &config.registry
        && (!reg.username_secret.is_empty() || !reg.password_secret.is_empty())
    {
        return true;
    }
    config
        .hooks
        .pre_deploy
        .iter()
        .any(|h| !h.secrets.is_empty() || !h.env_from_secrets.is_empty())
}

fn load_age_bundle(
    path: &Path,
    any_secrets_referenced: bool,
) -> Result<SecretsBundle, SecretsError> {
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
    if map.is_empty() && any_secrets_referenced {
        return Err(SecretsError::SealedEmpty);
    }
    Ok(SecretsBundle::new(map))
}

async fn load_command_bundle(
    command: &[String],
    format: SecretsFormat,
    any_secrets_referenced: bool,
) -> Result<SecretsBundle, SecretsError> {
    let Some((bin, args)) = command.split_first() else {
        return Err(SecretsError::CommandMissing);
    };
    let pretty = command.join(" ");

    // Lock the child's environment down to the explicit allowlist —
    // without env_clear, every yoink-process env var (including
    // YOINK_AGE_KEY when both providers are in play) flows into the
    // third-party CLI. PATH stays so the binary can resolve its own
    // sub-tools; per-tool auth tokens are forwarded explicitly.
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for key in COMMAND_ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }

    let mut child = cmd.spawn().map_err(|source| SecretsError::CommandSpawn {
        command: pretty.clone(),
        source,
    })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Wrap the whole drain+wait in a timeout so a wedged provider
    // can't hang the deploy. On expiry, kill the child and surface
    // a CommandTimeout — operator gets a clear "X did not finish
    // within Ns" instead of a silent hang.
    let drain = async {
        // Drain stdout + stderr concurrently so a child that fills
        // the stderr pipe before exiting doesn't deadlock on us
        // (we'd otherwise block reading stdout while it blocks on
        // a full stderr pipe).
        let stdout_fut = async {
            if let Some(out) = stdout_pipe.as_mut() {
                read_capped(out, COMMAND_OUTPUT_CAP).await
            } else {
                Ok(Vec::new())
            }
        };
        let stderr_fut = async {
            if let Some(err) = stderr_pipe.as_mut() {
                read_capped(err, COMMAND_OUTPUT_CAP).await
            } else {
                Ok(Vec::new())
            }
        };
        let (stdout_res, stderr_res) = tokio::join!(stdout_fut, stderr_fut);
        let stdout_buf = stdout_res.map_err(|()| SecretsError::CommandOutputCap {
            command: pretty.clone(),
            stream: "stdout",
            cap: COMMAND_OUTPUT_CAP,
        })?;
        let stderr_buf = stderr_res.map_err(|()| SecretsError::CommandOutputCap {
            command: pretty.clone(),
            stream: "stderr",
            cap: COMMAND_OUTPUT_CAP,
        })?;
        let status = child
            .wait()
            .await
            .map_err(|source| SecretsError::CommandSpawn {
                command: pretty.clone(),
                source,
            })?;
        Ok::<_, SecretsError>((stdout_buf, stderr_buf, status))
    };
    let (stdout_buf, stderr_buf, status) = if let Ok(res) =
        tokio::time::timeout(COMMAND_TIMEOUT, drain).await
    {
        res?
    } else {
        // Best-effort kill; child may have already exited.
        let _ = child.start_kill();
        return Err(SecretsError::CommandTimeout {
            command: pretty,
            timeout: COMMAND_TIMEOUT,
        });
    };

    if !status.success() {
        return Err(SecretsError::CommandExit {
            command: pretty,
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr_buf).trim().to_string(),
        });
    }

    let bundle =
        parse_bundle_bytes(&stdout_buf, format).map_err(|(format, detail)| {
            SecretsError::CommandParse {
                command: pretty.clone(),
                format,
                detail,
            }
        })?;
    if bundle.is_empty() && any_secrets_referenced {
        // Provider exited 0 but produced nothing parseable as a
        // key=value. Most commonly: wrong project / wrong scope /
        // expired token returning an empty list. Better to fail
        // loud than silently substitute zeros for every secret a
        // service expects. Skipped when the config doesn't actually
        // reference any secrets — early-config validation runs
        // load_bundle just to check the provider works.
        return Err(SecretsError::CommandEmpty { command: pretty });
    }
    Ok(bundle)
}

fn json_value_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Read at most `cap` bytes; returns `Err(())` if the producer
/// exceeds the cap (we stop reading and signal cap-hit so the caller
/// surfaces a clear error rather than silently truncating).
async fn read_capped<R: AsyncReadExt + Unpin>(reader: &mut R, cap: usize) -> Result<Vec<u8>, ()> {
    let mut out = Vec::with_capacity(8 * 1024);
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = match reader.read(&mut buf).await {
            // Both EOF and a mid-read error mean "producer is done";
            // surface what we have rather than failing — the caller
            // separately verifies the child's exit status.
            Ok(0) | Err(_) => return Ok(out),
            Ok(n) => n,
        };
        if out.len().saturating_add(n) > cap {
            // Drain remaining bytes to avoid the producer blocking
            // on a full pipe before we kill it. Bounded scratch.
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
            }
            return Err(());
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Pick a parser for `bytes`: `auto` looks at the first non-whitespace
/// byte (`{` → JSON, anything else → dotenv); `dotenv` / `json` force
/// the parser. Returns `(format_label, detail)` on parse failure so
/// the caller can build an error.
fn parse_bundle_bytes(
    bytes: &[u8],
    format: SecretsFormat,
) -> Result<SecretsBundle, (&'static str, String)> {
    // Strip a UTF-8 BOM if present — Windows tooling emits one, and
    // both of our parsers would otherwise choke on it (auto-detect
    // would fall through to dotenv since BOM ≠ `{`, then dotenv would
    // reject the leading non-ASCII bytes).
    let bytes = bytes
        .strip_prefix(&[0xEF_u8, 0xBB, 0xBF])
        .unwrap_or(bytes);
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
            // Reject non-string scalars + nested objects/arrays
            // rather than silently stringifying. A boolean `true`
            // would otherwise become the string `"true"` which most
            // consumers handle by accident; a number would lose
            // precision via Display formatting; nested structures
            // would round-trip as JSON text the operator never
            // intended.
            other => {
                return Err(format!(
                    "key {k:?} has non-string value (got {kind}); secrets bundles must be a flat string→string map",
                    kind = json_value_kind(other),
                ));
            }
        };
        out.insert(k.clone(), value);
    }
    Ok(SecretsBundle::new(out))
}

/// Minimal dotenv parser. Comments (`#…`) and blank lines are
/// dropped; values may be wrapped in single or double quotes;
/// quoted values may span multiple lines (tools like `infisical
/// export --format=dotenv`, `doppler secrets download`, and shell
/// `set` emit PEM certs / private keys this way — the value runs
/// from the opening quote to the matching closing quote across
/// however many `\n`s sit between).
///
/// Stricter than the sealed-file parser (`crate::sealed::parse_dotenv`)
/// in that we support multi-line — sealed files are operator-authored
/// and stay single-line on purpose; bundle output comes from arbitrary
/// upstream tools where we don't control the format.
fn parse_dotenv_bundle(bytes: &[u8]) -> Result<SecretsBundle, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not valid UTF-8: {e}"))?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let raw_line = lines[i];
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let after_export = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .trim_start();
        let (key, first_value) = after_export.split_once('=').ok_or_else(|| {
            format!("line {}: expected `KEY=VALUE`, got {raw_line:?}", i + 1)
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", i + 1));
        }

        let value_start = first_value.trim_start();
        let value = if let Some(quote) = opening_unmatched_quote(value_start) {
            // Multi-line quoted value: keep consuming lines until
            // we find the closing quote on its own. The opening
            // line contributes everything *after* the quote char.
            let mut acc = String::from(&value_start[1..]);
            let start_line = i;
            i += 1;
            let mut closed = false;
            while i < lines.len() {
                acc.push('\n');
                let l = lines[i];
                if let Some(idx) = l.find(quote) {
                    acc.push_str(&l[..idx]);
                    if !l[idx + 1..].trim().is_empty() {
                        return Err(format!(
                            "line {}: trailing content after closing {quote} in multi-line value for {key:?}",
                            i + 1
                        ));
                    }
                    closed = true;
                    i += 1;
                    break;
                }
                acc.push_str(l);
                i += 1;
            }
            if !closed {
                return Err(format!(
                    "line {}: unterminated {quote}-quoted value for {key:?}",
                    start_line + 1
                ));
            }
            acc
        } else {
            i += 1;
            strip_quotes(value_start.trim_end())
        };

        out.insert(key.to_string(), value);
    }
    Ok(SecretsBundle::new(out))
}

/// If `s` starts with `'` or `"` but the matching closing quote
/// isn't on the same line (e.g. a PEM cert that wraps), return the
/// quote char so the caller knows what to look for on subsequent
/// lines. `None` for unquoted values or single-line quoted values.
fn opening_unmatched_quote(s: &str) -> Option<char> {
    let bytes = s.as_bytes();
    let first = *bytes.first()? as char;
    if first != '\'' && first != '"' {
        return None;
    }
    if s[1..].contains(first) {
        return None;
    }
    Some(first)
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
    fn parse_json_bundle_drops_nulls_keeps_strings() {
        let bytes = br#"{"NAME":"x","NIL":null,"OTHER":"y"}"#;
        let bundle = parse_json_bundle(bytes).unwrap();
        assert_eq!(bundle.get("NAME"), Some("x"));
        assert_eq!(bundle.get("NIL"), None);
        assert_eq!(bundle.get("OTHER"), Some("y"));
    }

    #[test]
    fn parse_json_bundle_rejects_non_string_scalars() {
        let bytes = br#"{"PORT":8080}"#;
        let err = parse_json_bundle(bytes).unwrap_err();
        assert!(err.contains("non-string"), "got: {err}");
        let bytes = br#"{"FLAG":true}"#;
        let err = parse_json_bundle(bytes).unwrap_err();
        assert!(err.contains("non-string"), "got: {err}");
    }

    #[test]
    fn parse_bundle_strips_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"FOO=bar\n");
        let bundle = parse_bundle_bytes(&bytes, SecretsFormat::Auto).unwrap();
        assert_eq!(bundle.get("FOO"), Some("bar"));
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
    fn parse_dotenv_bundle_handles_multiline_quoted_value() {
        // Mirrors `infisical export --format=dotenv` output for a PEM
        // cert: opening single quote, value spans many lines, closing
        // quote on its own line.
        let bytes = b"FOO=1\nCERT='-----BEGIN CERTIFICATE-----\nABCDEF\nGHIJKL\n-----END CERTIFICATE-----'\nBAR=2\n";
        let bundle = parse_dotenv_bundle(bytes).unwrap();
        assert_eq!(bundle.get("FOO"), Some("1"));
        assert_eq!(bundle.get("BAR"), Some("2"));
        let cert = bundle.get("CERT").unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(cert.contains("ABCDEF"));
        assert!(cert.ends_with("-----END CERTIFICATE-----"));
    }

    #[test]
    fn parse_dotenv_bundle_unterminated_multiline_errors() {
        let bytes = b"CERT='-----BEGIN CERT-----\nMIIE\nMIIE\n";
        let err = parse_dotenv_bundle(bytes).unwrap_err();
        assert!(err.contains("unterminated"), "got: {err}");
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
            true,
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
            true,
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
