//! Bootstrap helper for `yoink add` when the active template seals
//! secrets but the operator's `yoink.yaml` has nothing configured.
//!
//! Without this, a first-time user runs `yoink add postgres`, the
//! sealing step quietly skips ("no `secrets:` block configured"), and
//! the resulting fragment references env-from-secrets keys that don't
//! exist. The wizard now offers to:
//!
//! 1. Generate an age identity (`age1...`).
//! 2. Save the private half to `./age.key` (mode 0600).
//! 3. Append `age.key` to `.gitignore`.
//! 4. Append a `secrets:` block to `yoink.yaml`.
//!
//! Project-local rather than global (`~/.config/yoink/age.key`) for
//! the same reason `yoink secrets key generate` defaults that way:
//! one identity per project, no collisions across repos.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{Config, SecretsConfig};
use crate::sealed;

use super::manifest::TemplateManifest;

/// Whether (and how) the wizard needs to intervene before sealing.
///
/// The "secrets block present but empty recipients" case is intentionally
/// missing — `Config::load_from_path` rejects that state at validation
/// time, so `cmd_add` never sees a config in that shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupNeed {
    /// Sealing will work as-is.
    Ready,
    /// Template seals nothing; secrets state is irrelevant.
    NotApplicable,
    /// `secrets:` block missing entirely — offer to bootstrap.
    BootstrapAge,
    /// External `provider: command` is configured. yoink can't write
    /// the value itself; the operator's tool owns it.
    ExternalProvider,
}

#[derive(Debug, Clone)]
pub struct BootstrapResult {
    pub key_path: PathBuf,
    pub public: String,
    pub gitignore_updated: bool,
}

pub fn assess(config: &Config, manifest: &TemplateManifest) -> SetupNeed {
    if manifest.secrets.is_empty() {
        return SetupNeed::NotApplicable;
    }
    match &config.secrets {
        None => SetupNeed::BootstrapAge,
        Some(SecretsConfig::Age { .. }) => SetupNeed::Ready,
        Some(SecretsConfig::Command { .. }) => SetupNeed::ExternalProvider,
    }
}

/// Default location for the generated age private key. Project-local
/// to mirror `cmd_secrets_key_generate`'s no-global-default convention.
pub fn default_key_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("age.key")
}

/// Generate keypair, write private key, edit yoink.yaml + .gitignore.
/// Refuses to overwrite an existing key file — caller should detect
/// that case beforehand if it wants different semantics.
pub fn bootstrap(config_path: &Path, key_path: &Path) -> Result<BootstrapResult> {
    if key_path.exists() {
        anyhow::bail!(
            "{} already exists — refusing to overwrite. Add `secrets:` to yoink.yaml manually, or remove the file and re-run.",
            key_path.display()
        );
    }

    let (secret, public) = sealed::keygen();
    let body = format!("# created by `yoink add`\n# public key: {public}\n{secret}\n");
    sealed::write_atomically_secret(key_path, body.as_bytes())
        .with_context(|| format!("write {}", key_path.display()))?;

    let gitignore_updated =
        ensure_gitignored(config_path, key_path).context("update .gitignore")?;

    append_secrets_block(config_path, &public)
        .with_context(|| format!("append secrets: block to {}", config_path.display()))?;

    Ok(BootstrapResult {
        key_path: key_path.to_path_buf(),
        public,
        gitignore_updated,
    })
}

fn ensure_gitignored(config_path: &Path, key_path: &Path) -> Result<bool> {
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let gitignore = dir.join(".gitignore");

    let key_basename = key_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("age.key")
        .to_string();

    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    let already = existing
        .lines()
        .map(str::trim)
        .any(|l| l == key_basename || l == format!("/{key_basename}"));
    if already {
        return Ok(false);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&key_basename);
    next.push('\n');
    sealed::write_atomically(&gitignore, next.as_bytes())
        .with_context(|| format!("write {}", gitignore.display()))?;
    Ok(true)
}

fn append_secrets_block(config_path: &Path, public: &str) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let mut next = text;
    if !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("\nsecrets:\n  provider: age\n  recipients:\n    - ");
    next.push_str(public);
    next.push('\n');
    sealed::write_atomically(config_path, next.as_bytes())
        .with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn assess_no_secrets_in_template() {
        let cfg = parse_config("secrets:\n  provider: age\n  recipients: [age1xyz]\n");
        let manifest = manifest_with_secrets(&[]);
        assert_eq!(assess(&cfg, &manifest), SetupNeed::NotApplicable);
    }

    #[test]
    fn assess_no_secrets_block() {
        let cfg = parse_config("");
        let manifest = manifest_with_secrets(&["A"]);
        assert_eq!(assess(&cfg, &manifest), SetupNeed::BootstrapAge);
    }

    #[test]
    fn assess_ready() {
        let cfg = parse_config("secrets:\n  provider: age\n  recipients: [age1xyz]\n");
        let manifest = manifest_with_secrets(&["A"]);
        assert_eq!(assess(&cfg, &manifest), SetupNeed::Ready);
    }

    #[test]
    fn assess_external_provider() {
        let cfg = parse_config("secrets:\n  provider: command\n  command: [op]\n");
        let manifest = manifest_with_secrets(&["A"]);
        assert_eq!(assess(&cfg, &manifest), SetupNeed::ExternalProvider);
    }

    #[test]
    fn bootstrap_writes_key_and_edits_yaml() {
        let tmp = TempDir::new().unwrap();
        let yaml_path = tmp.path().join("yoink.yaml");
        fs::write(
            &yaml_path,
            "deploy:\n  networks: [public]\nhosts:\n  - { address: h, user: u }\nservices:\n  - name: s\n    image: i\n    run: {}\n",
        )
        .unwrap();
        let key_path = default_key_path(&yaml_path);

        let result = bootstrap(&yaml_path, &key_path).unwrap();
        assert!(key_path.exists());
        assert!(result.public.starts_with("age1"));
        assert!(result.gitignore_updated);

        let updated_yaml = fs::read_to_string(&yaml_path).unwrap();
        assert!(updated_yaml.contains("secrets:\n  provider: age\n"));
        assert!(updated_yaml.contains(&result.public));

        let gitignore = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("age.key"));

        // Round-trip: parsed config now sees the recipient.
        let reloaded = Config::load_from_path(&yaml_path).unwrap();
        match reloaded.secrets {
            Some(SecretsConfig::Age { recipients, .. }) => {
                assert_eq!(recipients.len(), 1);
                assert!(recipients[0].starts_with("age1"));
            }
            _ => panic!("expected age block"),
        }
    }

    #[test]
    fn bootstrap_refuses_to_overwrite_existing_key() {
        let tmp = TempDir::new().unwrap();
        let yaml_path = tmp.path().join("yoink.yaml");
        fs::write(&yaml_path, "deploy:\n  networks: [public]\nhosts:\n  - { address: h, user: u }\nservices:\n  - name: s\n    image: i\n    run: {}\n").unwrap();
        let key_path = tmp.path().join("age.key");
        fs::write(&key_path, "preexisting\n").unwrap();

        let err = bootstrap(&yaml_path, &key_path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn ensure_gitignored_skips_when_already_present() {
        let tmp = TempDir::new().unwrap();
        let yaml_path = tmp.path().join("yoink.yaml");
        fs::write(tmp.path().join(".gitignore"), "age.key\n*.log\n").unwrap();
        let key_path = tmp.path().join("age.key");
        let updated = ensure_gitignored(&yaml_path, &key_path).unwrap();
        assert!(!updated);
    }

    fn parse_config(secrets_block: &str) -> Config {
        let yaml = format!(
            "deploy:\n  networks: [public]\nhosts:\n  - {{ address: h, user: u }}\n{secrets_block}services:\n  - name: s\n    image: i\n    run: {{}}\n"
        );
        Config::parse_str(&yaml).unwrap()
    }

    fn manifest_with_secrets(names: &[&str]) -> TemplateManifest {
        TemplateManifest {
            name: "t".into(),
            kind: super::super::manifest::TemplateKind::Accessory,
            description: None,
            yoink_min_version: None,
            variables: Vec::new(),
            files: vec![super::super::manifest::FileSpec {
                dest: "x.yaml".into(),
                template: "x.tmpl".into(),
            }],
            secrets: names
                .iter()
                .map(|n| super::super::manifest::SecretSpec {
                    name: (*n).to_string(),
                    generate: "random:32".into(),
                })
                .collect(),
            include_glob: None,
            notes: None,
            connection: None,
        }
    }
}
