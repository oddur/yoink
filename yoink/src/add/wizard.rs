//! Interactive (TTY) and non-interactive (--var / defaults only)
//! variable collection for `yoink add`.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};

use anyhow::Result;

use super::manifest::{regex_match, VariableSpec};

/// Collect values for every manifest variable.
///
/// Resolution order (first match wins):
/// 1. `--var key=value` overrides
/// 2. Interactive prompt (if `interactive` is true and stdin is a tty)
/// 3. `default:` from the manifest
///
/// Errors when a variable has no override, no default, and no tty.
pub fn collect_variables(
    specs: &[VariableSpec],
    overrides: &BTreeMap<String, String>,
    interactive: bool,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let tty_in = io::stdin().is_terminal();
    let tty_out = io::stdout().is_terminal();
    let can_prompt = interactive && tty_in && tty_out;

    for spec in specs {
        if let Some(v) = overrides.get(&spec.name) {
            validate(v, spec)?;
            out.insert(spec.name.clone(), v.clone());
            continue;
        }

        let value = if can_prompt {
            ask_for(spec)?
        } else if let Some(d) = &spec.default {
            d.clone()
        } else {
            anyhow::bail!(
                "variable `{}` has no default; pass `--var {}=<value>` or run interactively",
                spec.name,
                spec.name
            );
        };
        validate(&value, spec)?;
        out.insert(spec.name.clone(), value);
    }
    Ok(out)
}

fn ask_for(spec: &VariableSpec) -> Result<String> {
    let prompt_label = spec
        .prompt
        .as_deref()
        .unwrap_or(spec.name.as_str())
        .to_string();
    if !spec.choices.is_empty() {
        return ask_choice(&prompt_label, spec);
    }
    let mut out = io::stdout();
    loop {
        match &spec.default {
            Some(d) => write!(out, "{prompt_label} [{d}]: ")?,
            None => write!(out, "{prompt_label}: ")?,
        }
        out.flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let answer = buf.trim();
        let resolved = if answer.is_empty() {
            spec.default.clone().unwrap_or_default()
        } else {
            answer.to_string()
        };
        match validate(&resolved, spec) {
            Ok(()) => return Ok(resolved),
            Err(e) => {
                eprintln!("  ✗ {e}");
            }
        }
    }
}

fn ask_choice(prompt_label: &str, spec: &VariableSpec) -> Result<String> {
    let default_idx = spec
        .default
        .as_ref()
        .and_then(|d| spec.choices.iter().position(|c| c == d));
    let mut out = io::stdout();
    loop {
        writeln!(out, "{prompt_label}:")?;
        for (i, choice) in spec.choices.iter().enumerate() {
            let marker = if Some(i) == default_idx { " (default)" } else { "" };
            writeln!(out, "  {}) {choice}{marker}", i + 1)?;
        }
        match default_idx {
            Some(i) => write!(out, "pick [1-{}, default {}]: ", spec.choices.len(), i + 1)?,
            None => write!(out, "pick [1-{}]: ", spec.choices.len())?,
        }
        out.flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let answer = buf.trim();
        let resolved = if answer.is_empty() {
            match default_idx {
                Some(i) => spec.choices[i].clone(),
                None => continue,
            }
        } else if let Ok(n) = answer.parse::<usize>() {
            if (1..=spec.choices.len()).contains(&n) {
                spec.choices[n - 1].clone()
            } else {
                eprintln!("  ✗ out of range");
                continue;
            }
        } else if spec.choices.iter().any(|c| c == answer) {
            answer.to_string()
        } else {
            eprintln!("  ✗ pick a number 1-{}", spec.choices.len());
            continue;
        };
        return Ok(resolved);
    }
}

fn validate(value: &str, spec: &VariableSpec) -> Result<()> {
    if !spec.choices.is_empty() && !spec.choices.iter().any(|c| c == value) {
        anyhow::bail!(
            "{} must be one of: {}",
            spec.name,
            spec.choices.join(", ")
        );
    }
    if let Some(pat) = &spec.pattern
        && !regex_match(pat, value)
    {
        anyhow::bail!("{} doesn't match required pattern `{pat}`", spec.name);
    }
    Ok(())
}

/// Parse `--var key=value` strings into a map. Duplicate keys take the
/// last value (last-write-wins matches CLI conventions).
pub fn parse_var_overrides(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in raw {
        let (k, v) = entry
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--var must be `key=value` (got `{entry}`)"))?;
        out.insert(k.trim().to_string(), v.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, default: Option<&str>, choices: &[&str], pattern: Option<&str>) -> VariableSpec {
        VariableSpec {
            name: name.into(),
            prompt: None,
            default: default.map(str::to_string),
            choices: choices.iter().map(|s| (*s).to_string()).collect(),
            pattern: pattern.map(str::to_string),
        }
    }

    #[test]
    fn defaults_used_when_no_override_no_tty() {
        let specs = vec![spec("x", Some("5"), &[], None)];
        let out = collect_variables(&specs, &BTreeMap::new(), false).unwrap();
        assert_eq!(out.get("x").unwrap(), "5");
    }

    #[test]
    fn override_beats_default() {
        let specs = vec![spec("x", Some("5"), &[], None)];
        let mut overrides = BTreeMap::new();
        overrides.insert("x".into(), "9".into());
        let out = collect_variables(&specs, &overrides, false).unwrap();
        assert_eq!(out.get("x").unwrap(), "9");
    }

    #[test]
    fn missing_default_fails_without_tty() {
        let specs = vec![spec("x", None, &[], None)];
        let err = collect_variables(&specs, &BTreeMap::new(), false).unwrap_err();
        assert!(err.to_string().contains("no default"));
    }

    #[test]
    fn override_validated_against_pattern() {
        let specs = vec![spec("name", None, &[], Some("^[a-z]+$"))];
        let mut overrides = BTreeMap::new();
        overrides.insert("name".into(), "BadName".into());
        let err = collect_variables(&specs, &overrides, false).unwrap_err();
        assert!(err.to_string().contains("pattern"));
    }

    #[test]
    fn override_validated_against_choices() {
        let specs = vec![spec("v", Some("16"), &["15", "16", "17"], None)];
        let mut overrides = BTreeMap::new();
        overrides.insert("v".into(), "99".into());
        let err = collect_variables(&specs, &overrides, false).unwrap_err();
        assert!(err.to_string().contains("one of"));
    }

    #[test]
    fn parse_overrides_basic() {
        let raw = vec!["a=1".to_string(), "b=hello world".into()];
        let parsed = parse_var_overrides(&raw).unwrap();
        assert_eq!(parsed.get("a").unwrap(), "1");
        assert_eq!(parsed.get("b").unwrap(), "hello world");
    }

    #[test]
    fn parse_overrides_rejects_no_equals() {
        let raw = vec!["bad".to_string()];
        assert!(parse_var_overrides(&raw).is_err());
    }
}
