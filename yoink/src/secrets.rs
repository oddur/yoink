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
/// Cap on stderr / parser-detail bytes embedded in error variants.
/// A misconfigured upstream can echo secret values into its error
/// output ("failed to read secret 'DB_PASS' = '<value>'"); capping
/// the slice we surface to the operator (and to any deploy-log
/// archive) bounds that leak. The tail is kept rather than the
/// head — error tails are usually more diagnostic.
const ERROR_DETAIL_CAP: usize = 4096;
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
    // git, gpg, age, and a number of other CLIs honor XDG_CONFIG_HOME
    // for config-file resolution. Operators with custom dotfiles
    // setups otherwise hit subtle "config not found" failures.
    "XDG_CONFIG_HOME",
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

    // Drain stdout + stderr concurrently so a child that fills the
    // stderr pipe before exiting doesn't deadlock on us (we'd
    // otherwise block reading stdout while it blocks on a full
    // stderr pipe). The whole thing is wrapped in a timeout so a
    // wedged provider can't hang the deploy.
    let drain = async {
        let stdout_fut = async {
            match stdout_pipe.as_mut() {
                Some(out) => read_capped(out, COMMAND_OUTPUT_CAP).await,
                None => Ok(Vec::new()),
            }
        };
        let stderr_fut = async {
            match stderr_pipe.as_mut() {
                Some(err) => read_capped(err, COMMAND_OUTPUT_CAP).await,
                None => Ok(Vec::new()),
            }
        };
        tokio::join!(stdout_fut, stderr_fut)
    };
    let drain_result = tokio::time::timeout(COMMAND_TIMEOUT, drain).await;

    // Always reap the child, no matter how the drain ended. Tokio's
    // process::Child doesn't kill-on-drop by default; if we returned
    // here without wait()-ing we'd leak a zombie + (in the cap/timeout
    // cases) a still-running process blocked on closed pipes.
    let (stdout_buf, stderr_buf, status) = match drain_result {
        Ok((stdout_res, stderr_res)) => {
            // If either drain hit the cap, kill the child before
            // wait()ing — for natural producers (head -c) the
            // process is exiting on its own, but a slow producer
            // that drip-feeds bytes after exceeding the cap would
            // otherwise block our wait() until COMMAND_TIMEOUT.
            // Symmetric with the timeout branch below.
            if stdout_res.is_err() || stderr_res.is_err() {
                let _ = child.start_kill();
            }
            let status = child
                .wait()
                .await
                .map_err(|source| SecretsError::CommandSpawn {
                    command: pretty.clone(),
                    source,
                })?;
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
            (stdout_buf, stderr_buf, status)
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(SecretsError::CommandTimeout {
                command: pretty,
                timeout: COMMAND_TIMEOUT,
            });
        }
    };

    if !status.success() {
        return Err(SecretsError::CommandExit {
            command: pretty,
            status: status.code().unwrap_or(-1),
            stderr: truncate_for_error(
                String::from_utf8_lossy(&stderr_buf).trim(),
                ERROR_DETAIL_CAP,
            ),
        });
    }

    let bundle =
        parse_bundle_bytes(&stdout_buf, format).map_err(|(format, detail)| {
            SecretsError::CommandParse {
                command: pretty.clone(),
                format,
                detail: truncate_for_error(&detail, ERROR_DETAIL_CAP),
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

/// Truncate a string to at most `cap` bytes for inclusion in an
/// error variant. Keeps the tail (where parser/CLI errors usually
/// have their useful diagnostic) and prepends a marker when we
/// had to cut. Char-boundary safe.
fn truncate_for_error(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Walk forward from `s.len() - cap` until we hit a char boundary,
    // so we never split a multi-byte char.
    let mut start = s.len().saturating_sub(cap);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(truncated){}", &s[start..])
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
    // Iterator-based: each `iter.next()` advances exactly once, so we
    // can't accidentally fail to advance and spin (the previous
    // index-mutation shape had four explicit `i += 1` sites and any
    // missing one was an infinite loop).
    let mut iter = text.lines().enumerate();
    while let Some((line_idx, raw_line)) = iter.next() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let after_export = trimmed
            .strip_prefix("export ")
            .unwrap_or(trimmed)
            .trim_start();
        let (key, first_value) = after_export.split_once('=').ok_or_else(|| {
            format!(
                "line {}: expected `KEY=VALUE`, got {raw_line:?}",
                line_idx + 1
            )
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", line_idx + 1));
        }

        let value_start = first_value.trim_start();
        let value = if let Some(quote) = opening_unmatched_quote(value_start) {
            consume_multiline_value(quote, value_start, key, line_idx, &mut iter)?
        } else {
            parse_single_line_value(value_start, key, line_idx)?
        };

        out.insert(key.to_string(), value);
    }
    Ok(SecretsBundle::new(out))
}

/// Single-line value: strip surrounding quotes if cleanly bracketed.
///
/// Rejects the most common footgun: `KEY='val' garbage` where the
/// closing quote is mid-line and what follows contains no further
/// matching quote at all. Cases where another matching quote
/// appears later (`"hello \"world\""` from Doppler-style escapes,
/// or `'val' 'more'` shell-concat-shaped input) fall through to
/// `strip_quotes`, preserving the "verbatim with surrounding
/// quotes stripped if first==last, else verbatim" behavior of the
/// pre-existing parser. The heuristic is intentionally narrow —
/// false-rejecting a Doppler value would be worse than under-
/// rejecting an operator typo, since the typo case isn't
/// security-relevant (the parser is operator-facing, not a trust
/// boundary).
fn parse_single_line_value(
    value_start: &str,
    key: &str,
    line_idx: usize,
) -> Result<String, String> {
    let trimmed = value_start.trim_end();
    let bytes = trimmed.as_bytes();
    if let Some(&first) = bytes.first()
        && (first == b'\'' || first == b'"')
        && let Some(close_offset) = trimmed[1..].find(first as char)
    {
        let close_idx = close_offset + 1;
        let after_close = &trimmed[close_idx + 1..];
        if after_close.is_empty() {
            return Ok(trimmed[1..close_idx].to_string());
        }
        // Heuristic: if there's another matching quote later in the
        // line, this looks like an escaped-quote or concat case;
        // preserve verbatim. Otherwise, the trailing chars are junk.
        if !after_close.contains(first as char) {
            return Err(format!(
                "line {}: trailing content after closing {} in value for {key:?}",
                line_idx + 1,
                first as char,
            ));
        }
    }
    Ok(strip_quotes(trimmed))
}

/// Pulled out of `parse_dotenv_bundle` to keep the main loop one
/// page tall. Consumes lines from `iter` until we see the closing
/// `quote`; returns the accumulated value (without surrounding
/// quotes). The opening line `value_start` contributes everything
/// after its leading quote char.
fn consume_multiline_value<'a, I>(
    quote: char,
    value_start: &str,
    key: &str,
    start_line_idx: usize,
    iter: &mut I,
) -> Result<String, String>
where
    I: Iterator<Item = (usize, &'a str)>,
{
    let mut acc = String::from(&value_start[1..]);
    for (line_idx, l) in iter.by_ref() {
        acc.push('\n');
        if let Some(idx) = l.find(quote) {
            acc.push_str(&l[..idx]);
            if !l[idx + 1..].trim().is_empty() {
                return Err(format!(
                    "line {}: trailing content after closing {quote} in multi-line value for {key:?}",
                    line_idx + 1
                ));
            }
            return Ok(acc);
        }
        acc.push_str(l);
    }
    Err(format!(
        "line {}: unterminated {quote}-quoted value for {key:?}",
        start_line_idx + 1
    ))
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
    fn parse_dotenv_bundle_double_quoted_multiline() {
        let bytes = b"KEY=\"line one\nline two\nline three\"\nNEXT=ok\n";
        let bundle = parse_dotenv_bundle(bytes).unwrap();
        assert_eq!(bundle.get("KEY"), Some("line one\nline two\nline three"));
        assert_eq!(bundle.get("NEXT"), Some("ok"));
    }

    #[test]
    fn parse_dotenv_bundle_rejects_trailing_after_close() {
        let bytes = b"CERT='-----BEGIN-----\nbody\n-----END-----' something_else\nNEXT=ok\n";
        let err = parse_dotenv_bundle(bytes).unwrap_err();
        assert!(err.contains("trailing content"), "got: {err}");
    }

    #[test]
    fn parse_dotenv_bundle_rejects_single_line_trailing_after_close() {
        // Unambiguous footgun: quote pair followed by junk that
        // contains no further matching quote. Previously silently
        // accepted as the literal string with embedded quotes.
        let bytes = b"KEY='val' garbage\n";
        let err = parse_dotenv_bundle(bytes).unwrap_err();
        assert!(err.contains("trailing content"), "got: {err}");

        let bytes = b"KEY='val' # comment-shaped garbage\n";
        let err = parse_dotenv_bundle(bytes).unwrap_err();
        assert!(err.contains("trailing content"), "got: {err}");
    }

    #[test]
    fn parse_dotenv_bundle_preserves_doppler_style_escaped_quotes() {
        // Real-world: Doppler / similar emit values containing
        // literal quotes via `\"` escape. Our parser keeps the
        // backslashes verbatim (documented limitation), but we
        // must NOT misclassify this as the trailing-content
        // footgun above.
        let bytes = b"K=\"hello \\\"world\\\"\"\n";
        let bundle = parse_dotenv_bundle(bytes).unwrap();
        // Either inner-stripped or full-literal is acceptable as
        // long as we didn't error.
        assert!(bundle.get("K").is_some());
    }

    #[test]
    fn truncate_for_error_keeps_short_input_intact() {
        assert_eq!(truncate_for_error("hello", 100), "hello");
        assert_eq!(truncate_for_error("", 100), "");
    }

    #[test]
    fn truncate_for_error_marks_truncated_input() {
        let s = "x".repeat(5000);
        let out = truncate_for_error(&s, 4096);
        assert!(out.starts_with("…(truncated)"));
        // Tail kept: the truncation marker is prepended to the last
        // ~cap bytes, so total length is cap + marker bytes.
        assert!(out.len() < s.len());
        assert!(out.ends_with("xxxxx"));
    }

    #[test]
    fn truncate_for_error_respects_char_boundary() {
        // 3-byte char (•) repeated past the cap. Cap of 100 bytes
        // doesn't land on a char boundary cleanly; truncate_for_error
        // must walk forward to the next valid boundary.
        let s = "•".repeat(2000);
        let out = truncate_for_error(&s, 100);
        // Must be a valid str (no panic) and end on a boundary.
        assert!(out.starts_with("…(truncated)"));
        assert!(out.is_char_boundary(out.len()));
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
    async fn load_command_bundle_caps_oversized_stdout() {
        // Producer emits 11 MiB — over the 10 MiB COMMAND_OUTPUT_CAP.
        // Verifies cap-hit returns CommandOutputCap *and* doesn't leak
        // the child (the test runner would hang on an unwaited child
        // if our refactor regressed).
        let err = load_command_bundle(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "yes A | head -c 11534336".into(),
            ],
            SecretsFormat::Auto,
            true,
        )
        .await
        .unwrap_err();
        let SecretsError::CommandOutputCap { stream, .. } = err else {
            panic!("expected CommandOutputCap, got: {err:?}");
        };
        assert_eq!(stream, "stdout");
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
