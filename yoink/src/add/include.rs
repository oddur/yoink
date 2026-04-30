//! Auto-edit (or instruct the operator to edit) the main `yoink.yaml`
//! `include:` list when a freshly-added template's fragment files
//! aren't already glob-matched by an existing entry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

/// Plan: what (if anything) to do to `yoink.yaml`'s include list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludePlan {
    /// Existing include already covers the dests (or no glob requested).
    AlreadyCovered,
    /// Need to add this glob — exact-line text patch ready to apply.
    AddGlob { glob: String },
}

/// Decide whether the given dests are already covered by the config's
/// existing `include:` entries. Doesn't mutate the file.
#[must_use]
pub fn plan(config: &Config, desired_glob: Option<&str>, dests: &[PathBuf]) -> IncludePlan {
    let Some(glob) = desired_glob else {
        return IncludePlan::AlreadyCovered;
    };
    if config.include.iter().any(|existing| existing == glob) {
        return IncludePlan::AlreadyCovered;
    }
    let base = config
        .config_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    if dests
        .iter()
        .all(|d| dest_matched_by_existing_globs(&base, d, &config.include))
    {
        return IncludePlan::AlreadyCovered;
    }
    IncludePlan::AddGlob {
        glob: glob.to_string(),
    }
}

fn dest_matched_by_existing_globs(base: &Path, dest: &Path, globs: &[String]) -> bool {
    let abs_dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        base.join(dest)
    };
    globs.iter().any(|pattern| {
        let pat_path = if Path::new(pattern).is_absolute() {
            pattern.clone()
        } else {
            base.join(pattern).to_string_lossy().into_owned()
        };
        match glob::Pattern::new(&pat_path) {
            Ok(p) => p.matches_path(&abs_dest),
            Err(_) => false,
        }
    })
}

/// Apply an [`IncludePlan::AddGlob`] to `yoink.yaml` on disk by editing
/// the file as text — preserving comments and existing formatting.
///
/// Strategy:
/// - If a top-level `include:` block already exists, append a new bullet
///   to its list using the same indentation as existing bullets.
/// - Otherwise append a new top-level block to the end of the file.
///
/// Bails when the existing `include:` is in flow-style (`include: [...]`)
/// — text-patching a flow-style list without a real YAML parser would
/// either produce duplicate keys or mangle the list. The operator gets
/// a clear "edit by hand" error instead.
pub fn apply(config_path: &Path, glob: &str) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    if has_flow_style_include(&text) {
        anyhow::bail!(
            "{} uses flow-style `include: [...]` — add `\"{glob}\"` manually",
            config_path.display()
        );
    }
    let next = render_with_glob(&text, glob);
    crate::sealed::write_atomically(config_path, next.as_bytes())
        .with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

fn has_flow_style_include(text: &str) -> bool {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("include:"))
        .any(|rest| rest.trim_start().starts_with('['))
}

/// Pure helper: produce the new file text given the old text and the
/// glob to add. Exposed for testing.
#[must_use]
pub fn render_with_glob(text: &str, glob: &str) -> String {
    let bullet = format!("\"{glob}\"");
    if let Some(updated) = append_to_existing_block(text, &bullet) {
        return updated;
    }
    append_new_block(text, &bullet)
}

fn append_to_existing_block(text: &str, bullet_value: &str) -> Option<String> {
    // Find a top-level `include:` line (no leading whitespace).
    let lines: Vec<&str> = text.lines().collect();
    let include_line_idx = lines
        .iter()
        .position(|l| l.starts_with("include:") && !l.starts_with("include: ["))?;

    // Find existing bullets indented under it. Stop at the next non-empty
    // non-bullet line at <= column 0.
    let mut last_bullet_idx = None;
    let mut bullet_indent = "  ".to_string();
    for (i, line) in lines.iter().enumerate().skip(include_line_idx + 1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        if indent.is_empty() {
            // Next top-level key — block has ended.
            break;
        }
        if trimmed.starts_with("- ") {
            last_bullet_idx = Some(i);
            bullet_indent = indent.to_string();
        }
    }

    let new_bullet_line = format!("{bullet_indent}- {bullet_value}");
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    let insert_at = match last_bullet_idx {
        Some(i) => i + 1,
        None => include_line_idx + 1,
    };
    out.insert(insert_at, new_bullet_line);
    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

fn append_new_block(text: &str, bullet_value: &str) -> String {
    let mut out = text.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\ninclude:\n  - ");
    out.push_str(bullet_value);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_new_block_when_missing() {
        let yaml = "hosts:\n  - { address: x, user: y }\n";
        let out = render_with_glob(yaml, "services/*.yaml");
        assert!(out.contains("\ninclude:\n  - \"services/*.yaml\"\n"));
    }

    #[test]
    fn appends_to_existing_block() {
        let yaml = "hosts:\n  - { address: x, user: y }\n\
                    include:\n  - \"databases/*.yaml\"\n\nservices:\n  - name: api\n    image: api\n";
        let out = render_with_glob(yaml, "services/*.yaml");
        let expected = "hosts:\n  - { address: x, user: y }\n\
                        include:\n  - \"databases/*.yaml\"\n  - \"services/*.yaml\"\n\n\
                        services:\n  - name: api\n    image: api\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn appends_to_empty_existing_block() {
        let yaml = "include:\nservices:\n  - name: a\n    image: a\n";
        let out = render_with_glob(yaml, "services/*.yaml");
        assert!(out.contains("include:\n  - \"services/*.yaml\"\nservices:"));
    }

    #[test]
    fn matches_indent_of_existing_bullet() {
        let yaml = "include:\n    - \"a.yaml\"\n";
        let out = render_with_glob(yaml, "b.yaml");
        assert!(out.contains("    - \"a.yaml\"\n    - \"b.yaml\""));
    }

    #[test]
    fn skips_when_glob_already_present() {
        // The plan() check is what guards against adding duplicates; the
        // pure text helper would happily duplicate, but plan returns
        // AlreadyCovered first. Verify plan logic:
        let mut config = empty_config();
        config.include = vec!["services/*.yaml".to_string()];
        let p = plan(&config, Some("services/*.yaml"), &[]);
        assert_eq!(p, IncludePlan::AlreadyCovered);
    }

    #[test]
    fn detects_existing_glob_covers_dest() {
        let mut config = empty_config();
        config.include = vec!["services/*.yaml".to_string()];
        config.config_dir = Some(PathBuf::from("/repo"));
        let p = plan(
            &config,
            Some("services/*.yaml"),
            &[PathBuf::from("services/postgres.yaml")],
        );
        assert_eq!(p, IncludePlan::AlreadyCovered);
    }

    #[test]
    fn detects_flow_style_include() {
        assert!(has_flow_style_include("include: [\"a.yaml\"]\n"));
        assert!(has_flow_style_include("include:[\"a.yaml\"]\n"));
        assert!(has_flow_style_include("hosts: x\ninclude: [a, b]\n"));
        assert!(!has_flow_style_include("include:\n  - a.yaml\n"));
        assert!(!has_flow_style_include(""));
    }

    #[test]
    fn flags_when_glob_missing() {
        let mut config = empty_config();
        config.include = vec!["other/*.yaml".into()];
        config.config_dir = Some(PathBuf::from("/repo"));
        let p = plan(
            &config,
            Some("services/*.yaml"),
            &[PathBuf::from("services/postgres.yaml")],
        );
        assert_eq!(
            p,
            IncludePlan::AddGlob {
                glob: "services/*.yaml".into(),
            }
        );
    }

    fn empty_config() -> Config {
        // Minimal config that passes validate(); we only read .include
        // and .config_dir from it.
        let yaml = "deploy:\n  networks: [public]\nhosts:\n  - { address: h, user: u }\nservices:\n  - name: s\n    image: i\n    run: {}\n";
        Config::parse_str(yaml).unwrap()
    }
}
