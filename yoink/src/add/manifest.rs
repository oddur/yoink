//! Schema for `template.yaml` — the per-template manifest read after
//! fetching from GitHub. Describes the variables to prompt for, the
//! files to render, and the secrets to seal.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("read template manifest {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse template manifest {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: yaml_serde::Error,
    },
    #[error("template manifest invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateKind {
    /// A service other services depend on (postgres, redis, …). Default.
    #[default]
    Accessory,
    /// A complete deployable application (openclaw, ghost, …). Tweaks
    /// the "deploy now?" prompt default.
    App,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateManifest {
    pub name: String,
    #[serde(default)]
    pub kind: TemplateKind,
    #[serde(default)]
    pub description: Option<String>,
    /// Minimum yoink version required to render this template. Free-form
    /// SemVer-ish string; a mismatch only emits a warning, not an error
    /// (templates that need newer features can fail at render time
    /// with a clearer message).
    #[serde(default)]
    pub yoink_min_version: Option<String>,
    #[serde(default)]
    pub variables: Vec<VariableSpec>,
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    /// Glob to add to the main `yoink.yaml`'s `include:` list if no
    /// existing entry already matches the rendered file destinations.
    /// Optional — when unset, no auto-include.
    #[serde(default)]
    pub include_glob: Option<String>,
    /// Markdown blob, rendered, printed after a successful add. Used
    /// for follow-up instructions ("set `depends_on:` in your app").
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional structured wiring block. Renders as paste-ready YAML
    /// after the add — `depends_on:`, `env:`, `env_from_secrets:` —
    /// using only fields that already exist in `services[]`. Keys are
    /// the manifest author's choice; the user pastes and renames to
    /// fit their app.
    #[serde(default)]
    pub connection: Option<ConnectionSpec>,
}

/// Wiring hints printed after `yoink add` succeeds. Pure data —
/// rendered with the same context as everything else, then surfaced
/// as a yaml block the operator can paste into their app's service
/// definition.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionSpec {
    /// Plain env vars (host, port, db name, user). Keys print in
    /// sorted order — pick names that read well alphabetically
    /// (`POSTGRES_DB`, `POSTGRES_HOST`, `POSTGRES_PORT`, `POSTGRES_USER`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Secret bindings: env var name → sealed-secret name. Same
    /// shape as `services[].env_from_secrets`.
    #[serde(default)]
    pub env_from_secrets: BTreeMap<String, String>,
    /// Service names the consumer should list under `depends_on:`.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariableSpec {
    pub name: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    /// Limit answers to these literal strings. Presented as a numeric
    /// pick in interactive mode.
    #[serde(default)]
    pub choices: Vec<String>,
    /// Regex the answer must match. Validated *after* `choices`.
    #[serde(default)]
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// Destination path relative to the operator's repo root. Rendered
    /// with the same variable context as `template`.
    pub dest: String,
    /// Path inside the template directory. Read verbatim, then
    /// rendered with minijinja.
    pub template: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    /// Final key name, after rendering. Becomes the dotenv key inside
    /// the sealed bundle.
    pub name: String,
    /// How to produce the value. Currently only `random:N` (N bytes,
    /// emitted as base32 without padding).
    pub generate: String,
}

impl TemplateManifest {
    pub fn parse_str(text: &str, source_label: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            yaml_serde::from_str(text).map_err(|source| ManifestError::Parse {
                path: source_label.to_string(),
                source,
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn parse_file(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse_str(&text, &path.display().to_string())
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid("`name:` must not be empty".into()));
        }
        if self.files.is_empty() {
            return Err(ManifestError::Invalid(
                "`files:` must list at least one file".into(),
            ));
        }
        for file in &self.files {
            check_relative_path(&file.dest, "files[].dest")?;
            check_relative_path(&file.template, "files[].template")?;
        }
        for var in &self.variables {
            if var.name.trim().is_empty() {
                return Err(ManifestError::Invalid(
                    "variables[].name must not be empty".into(),
                ));
            }
            if let Some(pat) = &var.pattern
                && pat.is_empty()
            {
                return Err(ManifestError::Invalid(format!(
                    "variables[{}].pattern is empty",
                    var.name
                )));
            }
        }
        for secret in &self.secrets {
            if !secret.generate.starts_with("random:") {
                return Err(ManifestError::Invalid(format!(
                    "secrets[{}].generate must be `random:N` (got {:?})",
                    secret.name, secret.generate
                )));
            }
        }
        Ok(())
    }
}

/// Reject `..` components and absolute paths — both `dest` (operator's
/// filesystem) and `template` (sandbox inside the cached template dir)
/// must stay inside their respective roots.
fn check_relative_path(p: &str, field: &str) -> Result<(), ManifestError> {
    let path = Path::new(p);
    if path.is_absolute() {
        return Err(ManifestError::Invalid(format!(
            "{field} must be relative (got {p:?})"
        )));
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ManifestError::Invalid(format!(
            "{field} must not contain `..` (got {p:?})"
        )));
    }
    Ok(())
}

/// Match a string against a manifest pattern. Same semantics as
/// `^pattern$` — anchored on both ends. Without a `regex` dep we
/// implement a tiny subset: `^…$` plus `[a-z0-9-]` character classes,
/// `+`, `*`. Sufficient for service-name validation; richer patterns
/// can move to a real regex crate later.
pub fn regex_match(pat: &str, value: &str) -> bool {
    let stripped = pat
        .strip_prefix('^')
        .unwrap_or(pat)
        .strip_suffix('$')
        .unwrap_or(pat);
    tiny_regex::matches(stripped, value)
}

mod tiny_regex {
    /// Minimal anchored matcher. Supports literal chars, `.`,
    /// `[a-zA-Z0-9-_]` character classes, `+`, `*`. Anything else
    /// is treated as a literal — keep manifest patterns simple.
    pub fn matches(pat: &str, s: &str) -> bool {
        let pat: Vec<char> = pat.chars().collect();
        let s: Vec<char> = s.chars().collect();
        match_at(&pat, 0, &s, 0)
    }

    fn match_at(pat: &[char], pi: usize, s: &[char], si: usize) -> bool {
        if pi >= pat.len() {
            return si >= s.len();
        }
        let (atom, atom_len) = parse_atom(pat, pi);
        let next_pi = pi + atom_len;
        let quantifier = pat.get(next_pi).copied();
        match quantifier {
            Some('+') => {
                if si >= s.len() || !atom.matches(s[si]) {
                    return false;
                }
                let mut i = si + 1;
                while i <= s.len() {
                    if match_at(pat, next_pi + 1, s, i) {
                        return true;
                    }
                    if i >= s.len() || !atom.matches(s[i]) {
                        return false;
                    }
                    i += 1;
                }
                false
            }
            Some('*') => {
                let mut i = si;
                loop {
                    if match_at(pat, next_pi + 1, s, i) {
                        return true;
                    }
                    if i >= s.len() || !atom.matches(s[i]) {
                        return false;
                    }
                    i += 1;
                }
            }
            _ => {
                if si >= s.len() || !atom.matches(s[si]) {
                    return false;
                }
                match_at(pat, next_pi, s, si + 1)
            }
        }
    }

    enum Atom {
        Any,
        Lit(char),
        Class(Vec<(char, char)>, bool /* negated */),
    }

    impl Atom {
        fn matches(&self, c: char) -> bool {
            match self {
                Self::Any => true,
                Self::Lit(l) => *l == c,
                Self::Class(ranges, negated) => {
                    let hit = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi);
                    hit ^ negated
                }
            }
        }
    }

    fn parse_atom(pat: &[char], pi: usize) -> (Atom, usize) {
        let c = pat[pi];
        if c == '.' {
            return (Atom::Any, 1);
        }
        if c == '[' {
            let mut j = pi + 1;
            let negated = pat.get(j).copied() == Some('^');
            if negated {
                j += 1;
            }
            let mut ranges = Vec::new();
            while j < pat.len() && pat[j] != ']' {
                let lo = pat[j];
                let (range_lo, range_hi) =
                    if pat.get(j + 1).copied() == Some('-') && pat.get(j + 2).is_some_and(|c| *c != ']') {
                        let hi = pat[j + 2];
                        j += 3;
                        (lo, hi)
                    } else {
                        j += 1;
                        (lo, lo)
                    };
                ranges.push((range_lo, range_hi));
            }
            // skip closing ']'
            if j < pat.len() {
                j += 1;
            }
            return (Atom::Class(ranges, negated), j - pi);
        }
        (Atom::Lit(c), 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let yaml = r#"
name: postgres
description: Test
files:
  - dest: services/postgres.yaml
    template: service.yaml.tmpl
"#;
        let m = TemplateManifest::parse_str(yaml, "test.yaml").unwrap();
        assert_eq!(m.name, "postgres");
        assert_eq!(m.kind, TemplateKind::Accessory);
        assert_eq!(m.files.len(), 1);
    }

    #[test]
    fn rejects_dest_with_parent() {
        let yaml = r#"
name: bad
files:
  - dest: ../escape.yaml
    template: x.tmpl
"#;
        let err = TemplateManifest::parse_str(yaml, "test.yaml").unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn rejects_absolute_template_path() {
        let yaml = r#"
name: bad
files:
  - dest: ok.yaml
    template: /etc/passwd
"#;
        let err = TemplateManifest::parse_str(yaml, "test.yaml").unwrap_err();
        assert!(err.to_string().contains("relative"));
    }

    #[test]
    fn rejects_unknown_generator() {
        let yaml = r#"
name: x
files:
  - dest: a.yaml
    template: a.tmpl
secrets:
  - name: TOKEN
    generate: secret-from-the-sky
"#;
        let err = TemplateManifest::parse_str(yaml, "test.yaml").unwrap_err();
        assert!(err.to_string().contains("random:N"));
    }

    #[test]
    fn regex_match_basic() {
        assert!(regex_match("^[a-z][a-z0-9-]*$", "postgres"));
        assert!(regex_match("^[a-z][a-z0-9-]*$", "p1"));
        assert!(!regex_match("^[a-z][a-z0-9-]*$", "1bad"));
        assert!(!regex_match("^[a-z][a-z0-9-]*$", "Bad"));
        assert!(regex_match("^v[0-9]+$", "v17"));
        assert!(!regex_match("^v[0-9]+$", "v"));
    }

    #[test]
    fn full_manifest_round_trip() {
        let yaml = r#"
name: postgres
kind: accessory
description: PostgreSQL with sealed credentials
yoink_min_version: "0.12.0"
variables:
  - name: service_name
    prompt: Service name
    default: postgres
    pattern: "^[a-z][a-z0-9-]*$"
  - name: version
    default: "16"
    choices: ["15", "16", "17"]
files:
  - dest: "services/{{ service_name }}.yaml"
    template: service.yaml.tmpl
secrets:
  - name: "{{ service_name | upper }}_PASSWORD"
    generate: "random:32"
include_glob: "services/*.yaml"
notes: |
  Add depends_on: [{{ service_name }}].
"#;
        let m = TemplateManifest::parse_str(yaml, "test.yaml").unwrap();
        assert_eq!(m.variables.len(), 2);
        assert_eq!(m.secrets.len(), 1);
        assert_eq!(m.include_glob.as_deref(), Some("services/*.yaml"));
    }
}
