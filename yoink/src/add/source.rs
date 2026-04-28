//! Source resolution for `yoink add`.
//!
//! Parses the user-supplied `<ref>` (a bare template name like
//! `postgres`, or a `gh:owner/repo[@ref]/path` form), resolves the
//! ref to a commit SHA via the GitHub API, fetches a gzipped tarball
//! from `codeload.github.com`, extracts it to a content-addressed
//! cache directory, and returns the path to the template root.
//!
//! The cache key is the resolved SHA, so branch refs like `main`
//! benefit from caching too. The user-visible "Template: …" header
//! shows the resolved short SHA so it's clear what version was used.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use thiserror::Error;

const DEFAULT_OWNER: &str = "oddur";
const DEFAULT_REPO: &str = "yoink";
const DEFAULT_REF: &str = "main";
const DEFAULT_SUBPATH_PREFIX: &str = "templates";

/// Cap on the gzipped tarball download. yoink's own repo tarball is
/// ~1–2 MB; even a sprawling 3rd-party templates repo should fit. Higher
/// than this and we're either staring at a monorepo or being attacked.
const TARBALL_DOWNLOAD_CAP: u64 = 50 * 1024 * 1024;
/// Cap on cumulative *unpacked* bytes. Defends against gzip bombs:
/// a 50 MB gzipped payload could expand to many GB.
const TARBALL_UNPACK_CAP: u64 = 200 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("invalid template ref `{0}`: {1}")]
    BadRef(String, &'static str),
    #[error("template `{0}` not found at {1} — checked {checked}", checked = .2)]
    NotFound(String, String, String),
}

/// Fully-qualified template ref. Constructed by [`parse_ref`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    pub owner: String,
    pub repo: String,
    /// Branch, tag, or short/long SHA. Resolved to a commit SHA before
    /// fetching.
    pub git_ref: String,
    /// Path inside the repo to the template directory. Always uses
    /// forward slashes.
    pub subpath: String,
    /// Original user-supplied string, kept for error messages.
    pub original: String,
}

impl TemplateRef {
    pub fn display_short(&self, sha: &str) -> String {
        let short = sha.chars().take(7).collect::<String>();
        if self.is_default_source() {
            format!("{} ({short})", self.subpath_basename())
        } else {
            format!("gh:{}/{}@{short}/{}", self.owner, self.repo, self.subpath)
        }
    }

    fn is_default_source(&self) -> bool {
        self.owner == DEFAULT_OWNER
            && self.repo == DEFAULT_REPO
            && self.subpath.starts_with(&format!("{DEFAULT_SUBPATH_PREFIX}/"))
    }

    fn subpath_basename(&self) -> &str {
        self.subpath
            .rsplit('/')
            .next()
            .unwrap_or(self.subpath.as_str())
    }
}

/// Parse `<ref>` into a [`TemplateRef`].
///
/// Forms:
/// - `postgres` → default repo, default ref, `templates/postgres`
/// - `postgres@v0.1.0` → default repo, ref `v0.1.0`, `templates/postgres`
/// - `gh:acme/templates/clickhouse` → ref `main`
/// - `gh:acme/templates@a1b2c3d/clickhouse` → ref `a1b2c3d`
pub fn parse_ref(raw: &str) -> Result<TemplateRef, SourceError> {
    let original = raw.to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SourceError::BadRef(original, "empty ref"));
    }

    if let Some(rest) = trimmed.strip_prefix("gh:") {
        // owner / repo[@ref] / path...
        let mut parts = rest.splitn(3, '/');
        let owner = parts.next().unwrap_or("").to_string();
        let repo_and_ref = parts.next().unwrap_or("");
        let subpath = parts.next().unwrap_or("").trim_matches('/').to_string();
        let (repo, git_ref) = match repo_and_ref.split_once('@') {
            Some((r, g)) => (r.to_string(), g.to_string()),
            None => (repo_and_ref.to_string(), DEFAULT_REF.to_string()),
        };
        if owner.is_empty() || repo.is_empty() || subpath.is_empty() || git_ref.is_empty() {
            return Err(SourceError::BadRef(
                original,
                "expected gh:owner/repo[@ref]/path",
            ));
        }
        return Ok(TemplateRef {
            owner,
            repo,
            git_ref,
            subpath,
            original,
        });
    }

    // Bare form: <name>[@ref]
    let (name, git_ref) = match trimmed.split_once('@') {
        Some((n, g)) => (n.to_string(), g.to_string()),
        None => (trimmed.to_string(), DEFAULT_REF.to_string()),
    };
    if git_ref.is_empty() {
        return Err(SourceError::BadRef(original, "ref after `@` is empty"));
    }
    if name.contains('/') {
        return Err(SourceError::BadRef(
            original,
            "bare template names can't contain `/` — use `gh:owner/repo/path` for external sources",
        ));
    }
    Ok(TemplateRef {
        owner: DEFAULT_OWNER.to_string(),
        repo: DEFAULT_REPO.to_string(),
        git_ref,
        subpath: format!("{DEFAULT_SUBPATH_PREFIX}/{name}"),
        original,
    })
}

/// Resolved tarball location on disk.
#[derive(Debug, Clone)]
pub struct FetchedTemplate {
    /// Full commit SHA the ref resolved to.
    pub sha: String,
    /// Path to the template root (the directory containing
    /// `template.yaml`).
    pub root: PathBuf,
}

/// Use a local directory as the template source. Skips the
/// fetch + extract pipeline entirely — for template authors iterating
/// on a manifest without push-pull cycles to GitHub.
pub fn from_local_path(path: &Path) -> Result<FetchedTemplate> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("resolve --from-path {}", path.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!(
            "--from-path {} is not a directory",
            canonical.display()
        );
    }
    if !canonical.join("template.yaml").exists() {
        anyhow::bail!(
            "no `template.yaml` in {} — is this a yoink template directory?",
            canonical.display()
        );
    }
    Ok(FetchedTemplate {
        sha: "local".to_string(),
        root: canonical,
    })
}

/// Fetch a template, using cache if possible. Network calls only happen
/// when `refresh` is true or when the cache doesn't already hold the
/// resolved SHA.
pub async fn fetch(template: &TemplateRef, refresh: bool) -> Result<FetchedTemplate> {
    let client = reqwest::Client::builder()
        .user_agent(format!("yoink/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;

    let sha = if looks_like_sha(&template.git_ref) {
        template.git_ref.clone()
    } else if refresh {
        resolve_sha(&client, template).await?
    } else if let Some(cached_sha) = cache_lookup_sha(template)? {
        cached_sha
    } else {
        resolve_sha(&client, template).await?
    };

    let extract_root = cache_dir(&template.owner, &template.repo, &sha)?;
    if !extract_root.exists() {
        let bytes = download_tarball(&client, template, &sha).await?;
        // We deliberately extract the whole tarball, not just the
        // requested subpath. A subpath filter would mean the cache
        // dir is only good for *that* template; a second `yoink add`
        // against the same SHA but a different template would silently
        // miss its files. Tarballs are capped at 50 MB on download.
        extract_tarball(&bytes, &extract_root).with_context(|| {
            format!("extract tarball for {}@{}", template.original, &sha[..7])
        })?;
    }

    let template_root = locate_template_root(&extract_root, template, &sha)?;
    persist_branch_mapping(template, &sha)?;
    Ok(FetchedTemplate {
        sha,
        root: template_root,
    })
}

fn looks_like_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Deserialize)]
struct GhCommit {
    sha: String,
}

async fn resolve_sha(client: &reqwest::Client, template: &TemplateRef) -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        template.owner, template.repo, template.git_ref
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!(
            "couldn't resolve {}/{}@{} — GitHub returned {}",
            template.owner,
            template.repo,
            template.git_ref,
            status
        );
    }
    let commit: GhCommit = resp.json().await.context("parse GitHub commit response")?;
    Ok(commit.sha)
}

async fn download_tarball(
    client: &reqwest::Client,
    template: &TemplateRef,
    sha: &str,
) -> Result<Vec<u8>> {
    let url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/{}",
        template.owner, template.repo, sha
    );
    let mut resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "couldn't download tarball for {}/{}@{} — got {}",
            template.owner,
            template.repo,
            sha,
            resp.status()
        );
    }
    if let Some(declared) = resp.content_length()
        && declared > TARBALL_DOWNLOAD_CAP
    {
        anyhow::bail!(
            "tarball for {}/{}@{} is {declared} bytes — refusing to download more than {TARBALL_DOWNLOAD_CAP}",
            template.owner,
            template.repo,
            sha
        );
    }

    let mut buf = Vec::with_capacity(1024 * 1024);
    while let Some(chunk) = resp.chunk().await.context("read tarball chunk")? {
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > TARBALL_DOWNLOAD_CAP {
            anyhow::bail!(
                "tarball for {}/{}@{} exceeded {TARBALL_DOWNLOAD_CAP} bytes mid-stream",
                template.owner,
                template.repo,
                sha
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Extract every entry in the tarball to `dest`. Caps cumulative
/// unpacked bytes against gzip bombs and rejects symlinks, absolute
/// paths, and `..` components.
fn extract_tarball(gz_bytes: &[u8], dest: &Path) -> Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::Archive;

    std::fs::create_dir_all(dest)
        .with_context(|| format!("create cache dir {}", dest.display()))?;
    let gz = GzDecoder::new(Cursor::new(gz_bytes));
    let mut archive = Archive::new(gz);
    archive.set_overwrite(true);
    let mut unpacked: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute() {
            anyhow::bail!("tarball contains absolute path {}", path.display());
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("tarball contains `..` component in {}", path.display());
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            continue;
        }
        unpacked = unpacked.saturating_add(entry.size());
        if unpacked > TARBALL_UNPACK_CAP {
            anyhow::bail!(
                "tarball expansion exceeded {TARBALL_UNPACK_CAP} bytes (gzip bomb?)"
            );
        }
        entry.unpack_in(dest)?;
    }
    Ok(())
}

fn cache_root() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(xdg).join("yoink").join("templates"));
    }
    let home = std::env::var_os("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".cache/yoink/templates"))
}

fn cache_dir(owner: &str, repo: &str, sha: &str) -> Result<PathBuf> {
    Ok(cache_root()?.join(format!("{owner}__{repo}__{sha}")))
}

/// Locate the template root inside the extracted tarball. GitHub's
/// codeload tarballs always wrap contents in `<repo>-<sha>/`.
fn locate_template_root(
    extract_root: &Path,
    template: &TemplateRef,
    sha: &str,
) -> Result<PathBuf> {
    let template_root = extract_root
        .join(format!("{}-{}", template.repo, sha))
        .join(&template.subpath);
    if !template_root.join("template.yaml").exists() {
        return Err(anyhow::anyhow!(
            "template `{}` not found at {} (looked for template.yaml in {})",
            template.original,
            template.subpath,
            template_root.display()
        ));
    }
    Ok(template_root)
}

/// Persist the `(owner, repo, ref) → sha` mapping so subsequent runs
/// without `--refresh` can serve from cache without hitting the GitHub
/// API. Writes a small `branches/<owner>__<repo>__<ref>` file
/// containing the SHA — skipped when the value is unchanged to avoid
/// touching mtime on every cache-hit run.
fn persist_branch_mapping(template: &TemplateRef, sha: &str) -> Result<()> {
    if looks_like_sha(&template.git_ref) {
        return Ok(());
    }
    let path = branch_mapping_path(template)?;
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing.trim() == sha) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, sha).ok();
    Ok(())
}

fn cache_lookup_sha(template: &TemplateRef) -> Result<Option<String>> {
    if looks_like_sha(&template.git_ref) {
        return Ok(Some(template.git_ref.clone()));
    }
    let path = branch_mapping_path(template)?;
    match std::fs::read_to_string(&path) {
        Ok(sha) => {
            let trimmed = sha.trim();
            if looks_like_sha(trimmed) {
                Ok(Some(trimmed.to_string()))
            } else {
                Ok(None)
            }
        }
        Err(_) => Ok(None),
    }
}

fn branch_mapping_path(template: &TemplateRef) -> Result<PathBuf> {
    Ok(cache_root()?.join("branches").join(format!(
        "{}__{}__{}",
        sanitize(&template.owner),
        sanitize(&template.repo),
        sanitize(&template.git_ref)
    )))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_name() {
        let r = parse_ref("postgres").unwrap();
        assert_eq!(r.owner, "oddur");
        assert_eq!(r.repo, "yoink");
        assert_eq!(r.git_ref, "main");
        assert_eq!(r.subpath, "templates/postgres");
    }

    #[test]
    fn parse_bare_with_ref() {
        let r = parse_ref("postgres@v0.12.0").unwrap();
        assert_eq!(r.git_ref, "v0.12.0");
        assert_eq!(r.subpath, "templates/postgres");
    }

    #[test]
    fn parse_gh_with_path() {
        let r = parse_ref("gh:acme/templates/clickhouse").unwrap();
        assert_eq!(r.owner, "acme");
        assert_eq!(r.repo, "templates");
        assert_eq!(r.git_ref, "main");
        assert_eq!(r.subpath, "clickhouse");
    }

    #[test]
    fn parse_gh_with_ref_and_path() {
        let r = parse_ref("gh:acme/templates@a1b2c3d/clickhouse").unwrap();
        assert_eq!(r.owner, "acme");
        assert_eq!(r.repo, "templates");
        assert_eq!(r.git_ref, "a1b2c3d");
        assert_eq!(r.subpath, "clickhouse");
    }

    #[test]
    fn parse_gh_nested_path() {
        let r = parse_ref("gh:acme/templates/databases/postgres").unwrap();
        assert_eq!(r.subpath, "databases/postgres");
    }

    #[test]
    fn parse_rejects_bare_with_slash() {
        assert!(parse_ref("foo/bar").is_err());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_ref("").is_err());
        assert!(parse_ref("   ").is_err());
    }

    #[test]
    fn parse_rejects_gh_without_path() {
        assert!(parse_ref("gh:acme/templates").is_err());
    }

    #[test]
    fn parse_rejects_empty_ref_after_at() {
        assert!(parse_ref("postgres@").is_err());
        assert!(parse_ref("gh:acme/templates@/path").is_err());
    }

    #[test]
    fn looks_like_sha_recognises_valid() {
        assert!(looks_like_sha("a1b2c3d"));
        assert!(looks_like_sha("a1b2c3d4e5f6"));
        assert!(looks_like_sha(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!looks_like_sha("main"));
        assert!(!looks_like_sha("v1.0.0"));
        assert!(!looks_like_sha("abc"));
    }

    #[test]
    fn display_short_default() {
        let r = parse_ref("postgres").unwrap();
        assert_eq!(r.display_short("a1b2c3def"), "postgres (a1b2c3d)");
    }

    #[test]
    fn display_short_third_party() {
        let r = parse_ref("gh:acme/templates/clickhouse").unwrap();
        assert_eq!(
            r.display_short("a1b2c3def"),
            "gh:acme/templates@a1b2c3d/clickhouse"
        );
    }
}
