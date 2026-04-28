//! Helpers behind the hidden `yoink __complete` subcommand —
//! the dynamic-completion plumbing the bash/zsh snippets in
//! `docs/content/docs/reference/cli.md` shell out to.
//!
//! Currently only the `configs` walker lives here; service / host
//! listing is trivial enough to stay inline in `main.rs` next to
//! the loaded `Config`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_DEPTH: usize = 8;

/// Directories the walker prunes. These are the heavyweight noise
/// dirs where a yoink config almost never lives — pruning them keeps
/// even a deep monorepo scan well under 100 ms.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".terraform",
    ".cache",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".venv",
    "venv",
    "__pycache__",
    "node_modules",
    "target",
    "dist",
    "build",
];

/// Walk the cwd subtree and print one yoink config path per line.
/// Bare paths so the bash `complete -F` snippet can pipe them
/// straight through `compgen -W`.
pub fn print_yoink_configs() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for path in find_yoink_configs(&cwd) {
        println!("{}", path.display());
    }
}

/// Names that mark a yoink config: the canonical `yoink.yaml`, plus
/// the multi-environment convention `<env>.yoink.yaml` (e.g.
/// `staging.yoink.yaml`). `services/*.yaml` fragments are *not*
/// matched — they aren't valid `-c` targets on their own.
#[must_use]
pub fn is_yoink_config_name(name: &str) -> bool {
    name == "yoink.yaml" || name.ends_with(".yoink.yaml")
}

/// Find every yoink config reachable from `root`, ranked by mtime
/// (newest first — the file the operator just edited bubbles to the
/// top of the completion list). Capped at [`MAX_DEPTH`] so a
/// misconfigured symlink loop can't hang the shell on every `<TAB>`.
#[must_use]
pub fn find_yoink_configs(root: &Path) -> Vec<PathBuf> {
    let mut hits: Vec<(PathBuf, SystemTime)> = Vec::new();
    walk(root, root, &mut hits, 0);
    // `Reverse` flips the natural ascending sort to descending.
    hits.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    hits.into_iter().map(|(p, _)| p).collect()
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ft.is_dir() {
            if SKIP_DIRS.iter().any(|s| *s == name.as_ref()) {
                continue;
            }
            walk(root, &entry.path(), out, depth + 1);
        } else if ft.is_file() && is_yoink_config_name(&name) {
            let path = entry.path();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            // Print paths relative to the walk root so completions
            // render cleanly (`./staging/yoink.yaml` not the full
            // absolute path).
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            out.push((rel, mtime));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn config_name_recognises_canonical_and_multi_env() {
        assert!(is_yoink_config_name("yoink.yaml"));
        assert!(is_yoink_config_name("staging.yoink.yaml"));
        assert!(is_yoink_config_name("prod.yoink.yaml"));
    }

    #[test]
    fn config_name_rejects_fragments_and_unrelated_yaml() {
        assert!(!is_yoink_config_name("api.yaml"));
        assert!(!is_yoink_config_name("docker-compose.yaml"));
        assert!(!is_yoink_config_name("yoink.yml"));
    }

    #[test]
    fn walker_finds_configs_skipping_noise_dirs_and_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("yoink.yaml"), "hosts: []\nservices: []\n").unwrap();
        fs::create_dir_all(root.join("staging")).unwrap();
        fs::write(
            root.join("staging").join("yoink.yaml"),
            "hosts: []\nservices: []\n",
        )
        .unwrap();
        fs::write(
            root.join("prod.yoink.yaml"),
            "hosts: []\nservices: []\n",
        )
        .unwrap();

        // Fragment under `services/` — should NOT be picked up.
        fs::create_dir_all(root.join("services")).unwrap();
        fs::write(root.join("services").join("api.yaml"), "name: api\n").unwrap();

        // Noise dirs that should be skipped entirely.
        for noise in [".git", "target", "node_modules"] {
            fs::create_dir_all(root.join(noise)).unwrap();
            fs::write(root.join(noise).join("yoink.yaml"), "ignored\n").unwrap();
        }

        let mut found: Vec<String> = find_yoink_configs(root)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                "prod.yoink.yaml".to_string(),
                "staging/yoink.yaml".to_string(),
                "yoink.yaml".to_string(),
            ],
        );
    }

    #[test]
    fn walker_ranks_by_modification_time_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("yoink.yaml"), "hosts: []\n").unwrap();
        // Make the second file's mtime strictly newer.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("staging.yoink.yaml"), "hosts: []\n").unwrap();

        let found: Vec<String> = find_yoink_configs(root)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Newest (staging) bubbles up first — the file the operator
        // most recently edited is almost always the one they want.
        assert_eq!(found.first().map(String::as_str), Some("staging.yoink.yaml"));
        assert_eq!(found.get(1).map(String::as_str), Some("yoink.yaml"));
    }
}
