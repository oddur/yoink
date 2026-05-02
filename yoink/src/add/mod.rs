//! `yoink add <ref>` — fetch a template from GitHub, run a small
//! variable-collection wizard, and drop a sealed-secret-and-fragment
//! pair into the operator's repo. The "poor man's helm" surface.
//!
//! Everything user-facing flows through [`cmd_add`]. The submodules
//! handle one concern each:
//!   - [`source`]: ref parsing, GitHub API, tarball fetch, cache.
//!   - [`manifest`]: parse `template.yaml`.
//!   - [`wizard`]: collect variable values from CLI overrides, prompts,
//!     or defaults.
//!   - [`render`]: minijinja substitution into files, secret names,
//!     dest paths, and notes.
//!   - [`include`]: extend the main `yoink.yaml`'s `include:` list when
//!     the rendered fragments aren't already glob-matched.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use crate::config::{Config, SecretsConfig};
use crate::sealed;

pub mod include;
pub mod manifest;
pub mod render;
pub mod secrets_setup;
pub mod source;
pub mod wizard;

#[derive(Debug, Clone)]
pub struct AddOpts {
    /// Either `r#ref` (GitHub-fetched) or `from_path` (local dir)
    /// must be set; both being set is rejected by the caller.
    pub r#ref: Option<String>,
    pub from_path: Option<PathBuf>,
    pub yes: bool,
    pub up: bool,
    pub refresh: bool,
    /// Repeated `--var key=value`.
    pub vars: Vec<String>,
}

/// Outcome of [`cmd_add`]. The caller decides whether to immediately
/// invoke `yoink up` — keeps `cmd_add` testable and avoids a circular
/// dependency on the deploy module's CLI plumbing.
#[derive(Debug, Clone, Default)]
pub struct AddOutcome {
    pub deploy_requested: bool,
}

#[allow(clippy::too_many_lines)] // single linear orchestration; splitting fragments the flow.
pub async fn cmd_add(config: &Config, config_path: &Path, opts: AddOpts) -> Result<AddOutcome> {
    let overrides = wizard::parse_var_overrides(&opts.vars)?;
    let interactive = !opts.yes && io::stdin().is_terminal();

    // `yoink add` with no positional ref and no `--from-path`: pick
    // from the bundled index. Interactive mode pops a numbered
    // picker; non-interactive prints the list and returns cleanly
    // (handy as a `yoink add | head` discovery probe in scripts).
    let resolved_ref = if opts.r#ref.is_none() && opts.from_path.is_none() {
        if interactive {
            Some(pick_from_index().await?)
        } else {
            print_index().await?;
            return Ok(AddOutcome::default());
        }
    } else {
        opts.r#ref.clone()
    };

    let (template_label, fetched) = match (&resolved_ref, &opts.from_path) {
        (Some(_), Some(_)) => {
            anyhow::bail!("pass either a template ref or `--from-path`, not both")
        }
        (None, None) => unreachable!("resolved above"),
        (Some(r), None) => {
            let template_ref = source::parse_ref(r)?;
            eprintln!(
                "fetching template `{}` from {}/{}@{}…",
                template_ref.subpath.rsplit('/').next().unwrap_or("?"),
                template_ref.owner,
                template_ref.repo,
                template_ref.git_ref
            );
            let fetched = source::fetch(&template_ref, opts.refresh).await?;
            (template_ref.display_short(&fetched.sha), fetched)
        }
        (None, Some(path)) => {
            let fetched = source::from_local_path(path)?;
            eprintln!("using template from local path: {}", fetched.root.display());
            let label = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("local")
                .to_string();
            (format!("{label} (local)"), fetched)
        }
    };

    let manifest_path = fetched.root.join("template.yaml");
    let manifest = manifest::TemplateManifest::parse_file(&manifest_path)?;

    if let Some(min) = &manifest.yoink_min_version
        && version_lt(env!("CARGO_PKG_VERSION"), min)
    {
        anyhow::bail!(
            "template `{}` requires yoink >= {min}, you have {} — upgrade yoink or pin the template to an older version",
            manifest.name,
            env!("CARGO_PKG_VERSION")
        );
    }

    // Bootstrap sealed secrets up-front when the template needs them
    // and yoink.yaml has nothing configured. Reload the in-memory
    // config so the rest of the flow (sealing, validation) sees the
    // newly-added recipients.
    let config_reloaded;
    let working_config = match secrets_setup::assess(config, &manifest) {
        secrets_setup::SetupNeed::Ready | secrets_setup::SetupNeed::NotApplicable => config,
        secrets_setup::SetupNeed::BootstrapAge => {
            if !interactive {
                anyhow::bail!(
                    "template `{}` seals {} secret(s) but `yoink.yaml` has no `secrets:` block. Run `yoink add` interactively to bootstrap, or configure `secrets:` first (`yoink secrets key generate`).",
                    manifest.name,
                    manifest.secrets.len()
                );
            }
            eprintln!();
            eprintln!(
                "this template seals {} secret(s) but `yoink.yaml` has no `secrets:` block.",
                manifest.secrets.len()
            );
            if !confirm("set up sealed secrets now?", true)? {
                anyhow::bail!(
                    "skipped — set up `secrets:` and re-run, or use a template that doesn't seal secrets"
                );
            }
            let key_path = secrets_setup::default_key_path(config_path);
            let result = secrets_setup::bootstrap(config_path, &key_path)?;
            eprintln!(
                "  ✓ wrote identity to {} (mode 0600)",
                result.key_path.display()
            );
            if result.gitignore_updated {
                eprintln!(
                    "  ✓ added {} to .gitignore",
                    result
                        .key_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                );
            }
            eprintln!("  ✓ added secrets: block to {}", config_path.display());
            eprintln!();
            eprintln!("    set this in your shell so future yoink commands find the key:");
            eprintln!(
                "      export YOINK_AGE_KEY_FILE=$(pwd)/{}",
                result
                    .key_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
            eprintln!();
            config_reloaded = Config::load_from_path(config_path).with_context(|| {
                format!(
                    "reloading {} after secrets bootstrap",
                    config_path.display()
                )
            })?;
            &config_reloaded
        }
        secrets_setup::SetupNeed::ExternalProvider => {
            anyhow::bail!(
                "template `{}` generates {} secret(s), but your `yoink.yaml` uses `provider: command` (external secrets). Generated values can't be written to an external store from yoink. Either set the secret(s) in your provider, or temporarily switch to age sealing.",
                manifest.name,
                manifest.secrets.len()
            );
        }
    };
    let config = working_config;

    let variables = wizard::collect_variables(&manifest.variables, &overrides, interactive)?;

    let rendered = render::render(&manifest, &fetched.root, &variables)?;

    // Validate the rendered fragment files parse as valid yoink config
    // fragments. Catches manifest bugs (typos in the .tmpl) before we
    // touch the operator's filesystem.
    for file in &rendered.files {
        validate_fragment(&file.contents).with_context(|| {
            format!(
                "rendered file `{}` failed yoink validation",
                file.dest.display()
            )
        })?;
    }

    let dests: Vec<PathBuf> = rendered.files.iter().map(|f| f.dest.clone()).collect();
    let include_plan = include::plan(config, rendered.include_glob.as_deref(), &dests);

    let target_dir = config
        .config_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let absolute_dests: Vec<PathBuf> = rendered
        .files
        .iter()
        .map(|f| target_dir.join(&f.dest))
        .collect();

    print_confirmation(
        &template_label,
        manifest.kind,
        &rendered,
        &absolute_dests,
        &include_plan,
        config,
    );

    if !opts.yes && interactive && !confirm("Proceed?", true)? {
        eprintln!("  ✗ aborted");
        return Ok(AddOutcome::default());
    }

    for (file, abs) in rendered.files.iter().zip(&absolute_dests) {
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        sealed::write_atomically(abs, file.contents.as_bytes())
            .with_context(|| format!("write {}", abs.display()))?;
        eprintln!("  ✓ wrote {}", abs.display());
    }

    if !rendered.secrets.is_empty() {
        match seal_new_secrets(config, &rendered.secrets) {
            Ok(SealReport::Sealed { count, path }) => {
                eprintln!("  ✓ sealed {count} secret(s) into {}", path.display());
            }
            Ok(SealReport::Skipped { reason }) => {
                eprintln!("  ! skipped sealing secrets: {reason}");
                eprintln!("    keys to add manually:");
                for s in &rendered.secrets {
                    eprintln!("      - {}", s.name);
                }
            }
            Err(e) => {
                eprintln!("  ✗ failed to seal secrets: {e}");
                eprintln!(
                    "    fragment files were written; re-run after fixing the secrets config"
                );
                return Err(e);
            }
        }
    }

    if let include::IncludePlan::AddGlob { glob } = &include_plan {
        let do_apply = if opts.yes {
            true
        } else if interactive {
            confirm(
                &format!("Add `include: [\"{glob}\"]` to {}?", config_path.display()),
                true,
            )?
        } else {
            false
        };
        if do_apply {
            include::apply(config_path, glob)?;
            eprintln!("  ✓ updated {} include:", config_path.display());
        } else {
            eprintln!(
                "  ! skipped include edit — add this to {} yourself:",
                config_path.display()
            );
            eprintln!("      include:\n        - \"{glob}\"");
        }
    }

    if let Some(conn) = rendered.connection.as_ref()
        && !connection_is_empty(conn)
    {
        eprintln!();
        eprintln!("connect another service to {}:", manifest.name);
        eprintln!("  # paste under the consuming service in yoink.yaml,");
        eprintln!("  # rename keys to whatever your app expects.");
        for line in render_connection_block(conn) {
            eprintln!("  {line}");
        }
    }

    if let Some(notes) = &rendered.notes
        && !notes.trim().is_empty()
    {
        eprintln!();
        eprintln!("notes:");
        for line in notes.lines() {
            eprintln!("  {line}");
        }
    }

    if !rendered.secrets.is_empty() {
        eprintln!();
        eprintln!("to inspect a generated value (e.g. to paste into a 3rd-party UI):");
        eprintln!("  yoink secrets show {} --reveal", rendered.secrets[0].name);
    }

    let deploy = if opts.up {
        true
    } else if interactive {
        let app_default = matches!(manifest.kind, manifest::TemplateKind::App);
        confirm("Deploy now?", app_default)?
    } else {
        false
    };

    if !deploy {
        eprintln!();
        eprintln!("next:");
        eprintln!("  yoink up");
    }

    Ok(AddOutcome {
        deploy_requested: deploy,
    })
}

fn validate_fragment(text: &str) -> Result<()> {
    use crate::config::ConfigFragment;
    let _: ConfigFragment = yaml_serde::from_str(text)?;
    Ok(())
}

fn connection_is_empty(c: &manifest::ConnectionSpec) -> bool {
    c.env.is_empty() && c.env_from_secrets.is_empty() && c.depends_on.is_empty()
}

/// Format a `ConnectionSpec` as paste-ready yaml lines using the same
/// fields the consuming service already accepts (`depends_on`, `env`,
/// `env_from_secrets`). No new schema — neutral keys come from the
/// manifest, the user pastes and renames as needed.
fn render_connection_block(c: &manifest::ConnectionSpec) -> Vec<String> {
    let mut out = Vec::new();
    if !c.depends_on.is_empty() {
        let list = c
            .depends_on
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("depends_on: [{list}]"));
    }
    if !c.env.is_empty() {
        out.push("env:".into());
        for (k, v) in &c.env {
            out.push(format!("  {k}: {}", quote_if_needed(v)));
        }
    }
    if !c.env_from_secrets.is_empty() {
        out.push("env_from_secrets:".into());
        for (k, v) in &c.env_from_secrets {
            out.push(format!("  {k}: {v}"));
        }
    }
    out
}

/// Quote yaml scalars that look like numbers or contain reserved
/// characters, so `5432` round-trips as a string and `app:s3://…`
/// doesn't trip the parser. Conservative — quotes anything that
/// isn't unambiguously a plain string identifier.
fn quote_if_needed(v: &str) -> String {
    let needs_quote = v.is_empty()
        || v.chars().next().is_some_and(|c| c.is_ascii_digit())
        || v.contains([':', '#', '@', '\'', '"', '\\']);
    if needs_quote {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

#[derive(Debug)]
enum SealReport {
    Sealed { count: usize, path: PathBuf },
    Skipped { reason: String },
}

fn seal_new_secrets(config: &Config, new_secrets: &[render::RenderedSecret]) -> Result<SealReport> {
    let Some(SecretsConfig::Age {
        file, recipients, ..
    }) = &config.secrets
    else {
        return Ok(SealReport::Skipped {
            reason: "no `secrets:` block configured (run `yoink secrets key generate` first)"
                .into(),
        });
    };
    if recipients.is_empty() {
        return Ok(SealReport::Skipped {
            reason: "`secrets.recipients:` is empty".into(),
        });
    }
    let path = sealed::resolve_sealed_path(config, file.as_deref())?;

    // Merge with existing sealed contents if the file already exists.
    // Try-read directly (avoids a TOCTOU race between exists() and read).
    let mut values: BTreeMap<String, Zeroizing<String>> = match std::fs::read(&path) {
        Ok(bytes) => {
            let identity = sealed::load_identity(recipients)?;
            let plaintext = sealed::unseal(&bytes, &identity)?;
            sealed::parse_dotenv(&plaintext)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("read sealed file {}", path.display()));
        }
    };

    let mut added = 0;
    for s in new_secrets {
        if values.contains_key(&s.name) {
            eprintln!(
                "  ! secret `{}` already sealed — keeping existing value",
                s.name
            );
            continue;
        }
        values.insert(s.name.clone(), Zeroizing::new(s.value.clone()));
        added += 1;
    }

    if added == 0 {
        return Ok(SealReport::Sealed { count: 0, path });
    }

    let canonical = sealed::render_dotenv(&values);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    sealed::write_atomically_secret(&path, &sealed_bytes)?;
    Ok(SealReport::Sealed { count: added, path })
}

fn print_confirmation(
    template_label: &str,
    kind: manifest::TemplateKind,
    rendered: &render::Rendered,
    absolute_dests: &[PathBuf],
    include_plan: &include::IncludePlan,
    config: &Config,
) {
    let kind_label = match kind {
        manifest::TemplateKind::Accessory => "accessory",
        manifest::TemplateKind::App => "app",
    };
    eprintln!();
    eprintln!("Template: {template_label} ({kind_label})");
    eprintln!("Files to write:");
    for abs in absolute_dests {
        let exists = if abs.exists() {
            " (overwrites existing)"
        } else {
            ""
        };
        eprintln!("  + {}{exists}", abs.display());
    }
    if !rendered.secrets.is_empty() {
        match &config.secrets {
            Some(SecretsConfig::Age { file, .. }) => {
                let path = sealed::resolve_sealed_path(config, file.as_deref())
                    .unwrap_or_else(|_| PathBuf::from("secrets.age"));
                eprintln!("Secrets to seal into {}:", path.display());
            }
            _ => {
                eprintln!("Secrets to generate (no sealing — set up `secrets:` first):");
            }
        }
        for s in &rendered.secrets {
            eprintln!("  + {} (random)", s.name);
        }
    }
    if let include::IncludePlan::AddGlob { glob } = include_plan {
        eprintln!("Update yoink.yaml:");
        eprintln!("  + include: [\"{glob}\"]");
    }
    eprintln!();
}

/// Print the bundled-templates index to stderr in a stable, scannable
/// table form. Used when `yoink add` runs without a ref in a
/// non-interactive context — discovery without commitment.
async fn print_index() -> Result<()> {
    let index = source::fetch_index().await?;
    eprintln!();
    eprintln!("Available templates (yoink add <name>):");
    let name_w = index
        .templates
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0);
    for entry in &index.templates {
        eprintln!(
            "  {:<name_w$}  ({:<9}) {}",
            entry.name, entry.kind, entry.summary,
        );
    }
    eprintln!();
    eprintln!("Run `yoink add <name>` to drop one in.");
    Ok(())
}

/// Interactive picker: list bundled templates, accept a numeric pick,
/// return the chosen name as a ref string suitable for `parse_ref`.
async fn pick_from_index() -> Result<String> {
    let index = source::fetch_index().await?;
    if index.templates.is_empty() {
        anyhow::bail!("templates index is empty");
    }
    let name_w = index
        .templates
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0);
    eprintln!();
    eprintln!("Available templates:");
    for (i, entry) in index.templates.iter().enumerate() {
        eprintln!(
            "  {:>2}) {:<name_w$}  ({:<9}) {}",
            i + 1,
            entry.name,
            entry.kind,
            entry.summary,
        );
    }
    loop {
        eprint!("\npick [1-{}]: ", index.templates.len());
        io::stderr().flush().ok();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed.parse::<usize>() {
            Ok(n) if (1..=index.templates.len()).contains(&n) => {
                return Ok(index.templates[n - 1].name.clone());
            }
            _ => eprintln!("  ✗ pick a number 1-{}", index.templates.len()),
        }
    }
}

fn confirm(prompt: &str, default_yes: bool) -> Result<bool> {
    let kind = if default_yes {
        crate::prompt::ConfirmKind::DefaultYes
    } else {
        crate::prompt::ConfirmKind::DefaultNo
    };
    // Callers gate this on their own `interactive` flag, so the TTY
    // case is the only one that reaches here in practice — but routing
    // through the shared helper means a non-TTY slip-through gets the
    // safe default rather than blocking on a dead stdin.
    crate::prompt::confirm(prompt, kind, false)
}

/// Loose semver `<` for the "you need a newer yoink" warning. Treats
/// version strings as dot-separated decimal segments; non-numeric
/// suffixes (`-rc1`) are ignored. Returns false on parse failure to
/// avoid spurious warnings.
fn version_lt(have: &str, need: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    }
    let h = parts(have);
    let n = parts(need);
    let len = h.len().max(n.len());
    for i in 0..len {
        let a = h.get(i).copied().unwrap_or(0);
        let b = n.get(i).copied().unwrap_or(0);
        if a < b {
            return true;
        }
        if a > b {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(version_lt("0.10.0", "0.12.0"));
        assert!(!version_lt("0.12.0", "0.12.0"));
        assert!(!version_lt("0.13.0", "0.12.0"));
        assert!(version_lt("0.11.0", "0.12.0-rc1"));
        assert!(!version_lt("1.0.0", "0.99.0"));
    }
}
