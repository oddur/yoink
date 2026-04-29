#![allow(
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::needless_pass_by_value,
    clippy::single_match_else,
    clippy::unnecessary_wraps,
    clippy::must_use_candidate
)]
//! `yoink init` — zero-prompt onboarding wizard.
//!
//! Detects cwd, Dockerfile, git remote, and `~/.ssh/config`; renders a
//! complete `yoink.yaml` with every field inferable filled in;
//! validates against the config schema before writing. Operators see
//! a summary of what was inferred so they can spot the one or two
//! things that need fixing.
//!
//! When something genuinely can't be inferred (no ssh host, no
//! positional HOST arg, no `~/.ssh/config` non-wildcard entry), init
//! fails with a one-line "pass HOST or use --interactive."
//! `--interactive` engages a small set of stdio prompts as a fallback.
//!
//! Cut from scope on purpose: secrets bootstrap (operators run
//! `yoink secrets key generate` separately), multi-service / multi-host
//! flows, registry credentials, host preflight.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Config;

/// Default container port when no Dockerfile EXPOSE is found.
const DEFAULT_PORT: u16 = 8080;

/// Default operator-facing fallback for the host-side ssh user when
/// `~/.ssh/config` doesn't say.
const DEFAULT_SSH_USER: &str = "deploy";

/// Args from clap.
#[derive(Debug, Clone)]
pub struct InitOpts {
    pub host: Option<String>,
    pub force: bool,
    pub interactive: bool,
    pub service: Option<String>,
    pub port: Option<u16>,
    pub no_port: bool,
    pub image: Option<String>,
    /// Skip generating an age identity. Use when an existing key
    /// should keep being used (e.g. you already have a teammate's
    /// `recipients:` block to drop in) or when the project will use
    /// `provider: command` instead.
    pub no_secrets: bool,
    /// When set, also generate an ed25519 SSH keypair and seal the
    /// private half into the new `secrets.age` under this name. The
    /// rendered yoink.yaml's host entry references it via
    /// `ssh_key_secret:`, and the `include:` glob is widened to
    /// `hosts/*.yaml` so subsequent `yoink hosts add` calls compose.
    /// Mutually exclusive with `no_secrets`.
    pub create_ssh_key: Option<String>,
}

pub fn cmd_init(opts: InitOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("read cwd")?;
    let yaml_path = cwd.join("yoink.yaml");

    if yaml_path.exists() && !opts.force {
        anyhow::bail!(
            "{} already exists — pass --force to overwrite",
            yaml_path.display()
        );
    }

    let detection = detect(&cwd);
    let mut plan = if opts.interactive {
        if !io::stdin().is_terminal() {
            anyhow::bail!("--interactive requires a terminal (stdin is not a tty)");
        }
        prompt_plan(&detection, &opts)?
    } else {
        infer_plan(&detection, &opts)?
    };

    // Bootstrap an age identity unless the operator opted out. We do
    // this BEFORE rendering yoink.yaml so the recipient lands in
    // `secrets:` from the start — a freshly-init'd repo is ready for
    // `yoink secrets edit` and `yoink add postgres` with zero further
    // setup. Cognitive overhead reduction is the whole point.
    let bootstrap = if opts.no_secrets {
        None
    } else {
        Some(bootstrap_age_identity()?)
    };
    plan.age_recipient = bootstrap.as_ref().map(|b| b.public.clone());

    // If the operator asked for a sealed deploy SSH key, generate it
    // alongside the AGE identity so the rendered yoink.yaml can
    // reference it via `ssh_key_secret:`. We do this BEFORE rendering
    // so the field lands in the file from the start.
    if let Some(seal_as) = opts.create_ssh_key.as_deref().filter(|s| !s.is_empty()) {
        let recipients = plan
            .age_recipient
            .as_ref()
            .map(std::slice::from_ref)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--create-ssh-key requires an age identity to seal against; remove --no-secrets or provide one"
                )
            })?;
        let secrets_path = cwd.join("secrets.age");
        let pub_openssh = crate::sealed::ssh_keygen_into_bundle(
            &secrets_path,
            recipients,
            seal_as,
            Some("yoink-init"),
        )?;
        plan.ssh_key_secret = Some(seal_as.to_string());
        eprintln!(
            "✓ sealed deploy SSH key as {seal_as:?} into {} (public: {})",
            secrets_path.display(),
            pub_openssh
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let yaml = render(&plan);
    // `Config::parse_str` runs `validate()` internally, so a clean
    // parse means parsed AND valid. A failure here is an internal
    // bug — the wizard owns every field — so the error message
    // points at the issue tracker rather than blaming the operator.
    Config::parse_str(&yaml).map_err(|e| {
        anyhow::anyhow!(
            "internal: rendered yoink.yaml failed validation — {e}\n\nPlease file an issue at https://github.com/oddur/yoink/issues with the inferred plan:\n{plan:?}"
        )
    })?;

    crate::sealed::write_atomically(&yaml_path, yaml.as_bytes())
        .with_context(|| format!("write yoink config to {}", yaml_path.display()))?;

    print_summary(&plan, &yaml_path, yaml.lines().count());
    if let Some(b) = &bootstrap {
        print_backup_warning(b);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct AgeBootstrap {
    public: String,
    key_path: PathBuf,
}

/// Generate a fresh age identity and save it to the multi-identity
/// keys dir (`~/.config/yoink/keys/<recipient>.key`, mode 0600). The
/// public recipient is returned so the caller can render it into
/// `yoink.yaml`.
fn bootstrap_age_identity() -> Result<AgeBootstrap> {
    let dir = crate::sealed::keys_dir().context("resolve ~/.config/yoink/keys/")?;
    bootstrap_age_identity_in_dir(&dir)
}

/// Inner: writes into an explicit dir so tests can drive it against
/// a tempdir without mutating `HOME`.
fn bootstrap_age_identity_in_dir(dir: &Path) -> Result<AgeBootstrap> {
    let (secret, public) = crate::sealed::keygen();
    write_age_identity(dir, &secret, &public)
}

/// Innermost: deterministic keypair → on-disk file. Lets tests pin
/// the public recipient and exercise the duplicate-detection branch.
fn write_age_identity(dir: &Path, secret: &str, public: &str) -> Result<AgeBootstrap> {
    use crate::sealed;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let key_path = dir.join(format!("{public}.key"));
    if key_path.exists() {
        // x25519 collisions are vanishingly unlikely; if this fires
        // it's almost certainly a `--force` re-init in the same
        // process. Refuse to clobber rather than silently overwrite.
        anyhow::bail!(
            "{} already exists — refusing to overwrite an existing identity",
            key_path.display()
        );
    }
    let body = format!("# created by `yoink init`\n# public: {public}\n{secret}\n");
    sealed::write_atomically_secret(&key_path, body.as_bytes())
        .with_context(|| format!("write {}", key_path.display()))?;
    Ok(AgeBootstrap {
        public: public.to_string(),
        key_path,
    })
}

fn print_backup_warning(b: &AgeBootstrap) {
    let key_str = b.key_path.display();
    let bar = "─".repeat(72);
    eprintln!();
    eprintln!("{bar}");
    eprintln!("  ⚠  BACK UP THIS KEY  —  do this BEFORE you seal any secrets");
    eprintln!("{bar}");
    eprintln!("  identity: {key_str}");
    eprintln!("  public:   {}", b.public);
    eprintln!();
    eprintln!("  This key is the ONLY thing that can decrypt your sealed");
    eprintln!("  secrets. Lose it and every sealed value in this repo");
    eprintln!("  becomes unrecoverable. Pick at least one of:");
    eprintln!();
    eprintln!("    1) Password manager (recommended):");
    eprintln!("         cat {key_str}");
    eprintln!("       …then paste the contents into a 1Password / Bitwarden /");
    eprintln!("       Keychain item titled e.g. \"yoink: <project>\".");
    eprintln!();
    eprintln!("    2) Encrypted backup volume:");
    eprintln!("         cp {key_str} ~/Backups/");
    eprintln!();
    eprintln!("    3) Teammate handoff (also good defence in depth):");
    eprintln!("       Add a teammate's `age1...` recipient to yoink.yaml's");
    eprintln!("       secrets.recipients, then `yoink secrets edit` to re-seal.");
    eprintln!();
    eprintln!("  A fresh repo with no sealed values is recoverable; one with");
    eprintln!("  weeks of secrets isn't. Do this now, not later.");
    eprintln!("{bar}");
}

#[derive(Debug, Clone, Default)]
struct Detection {
    cwd_basename: Option<String>,
    dockerfile: Option<DockerfileHints>,
    git_remote: Option<String>,
    ssh_host: Option<SshHostHint>,
}

#[derive(Debug, Clone, Default)]
struct DockerfileHints {
    expose: Option<u16>,
    user: Option<String>,
    has_healthcheck: bool,
}

#[derive(Debug, Clone)]
struct SshHostHint {
    address: String,
    user: Option<String>,
}

fn detect(cwd: &Path) -> Detection {
    Detection {
        cwd_basename: cwd.file_name().and_then(|s| s.to_str()).map(str::to_string),
        dockerfile: parse_dockerfile_hints(&cwd.join("Dockerfile")),
        git_remote: read_git_remote(cwd),
        ssh_host: read_ssh_config_first_host(),
    }
}

fn parse_dockerfile_hints(path: &Path) -> Option<DockerfileHints> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut hints = DockerfileHints::default();
    // Multi-stage shadowing: last matching directive wins, mirroring
    // docker's runtime semantics where the final stage is what runs.
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let lower_first = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let rest = line
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ");
        match lower_first.as_str() {
            "EXPOSE" => {
                if let Some(p) = rest.split_whitespace().next() {
                    let bare = p.split('/').next().unwrap_or(p);
                    if let Ok(n) = bare.parse::<u16>() {
                        hints.expose = Some(n);
                    }
                }
            }
            "USER" => {
                let token = rest.trim().to_string();
                if !token.is_empty() {
                    hints.user = Some(token);
                }
            }
            "HEALTHCHECK" => {
                hints.has_healthcheck = !rest.trim().eq_ignore_ascii_case("NONE");
            }
            _ => {}
        }
    }
    Some(hints)
}

fn read_git_remote(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

fn read_ssh_config_first_host() -> Option<SshHostHint> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".ssh/config");
    let text = std::fs::read_to_string(path).ok()?;
    parse_ssh_config_first_host(&text)
}

fn parse_ssh_config_first_host(text: &str) -> Option<SshHostHint> {
    let mut current_host: Option<String> = None;
    let mut current_user: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("").to_ascii_lowercase();
        let value = parts.next().map_or("", str::trim);
        match key.as_str() {
            "host" => {
                if let Some(prev) = current_host.take() {
                    return Some(SshHostHint {
                        address: prev,
                        user: current_user.take(),
                    });
                }
                let first = value.split_whitespace().next().unwrap_or("");
                if first.is_empty() || first.contains('*') || first.contains('?') {
                    continue;
                }
                current_host = Some(first.to_string());
                current_user = None;
            }
            "user" if current_host.is_some() => {
                current_user = Some(value.to_string());
            }
            _ => {}
        }
    }
    current_host.map(|address| SshHostHint {
        address,
        user: current_user,
    })
}

#[derive(Debug, Clone)]
struct WizardPlan {
    service: String,
    image: ImageSource,
    tag: String,
    host_address: String,
    host_user: String,
    host_user_origin: HostUserOrigin,
    port: Option<u16>,
    user_override: Option<String>,
    sources: InferredSources,
    /// `Some(public_recipient)` when init bootstrapped an age identity
    /// — render emits a `secrets:` block. `None` when the operator
    /// passed `--no-secrets`.
    age_recipient: Option<String>,
    /// `Some(seal_name)` when init also generated a sealed SSH key
    /// for the host. Render adds `ssh_key_secret: <name>` to the host
    /// entry and widens `include:` to pick up `hosts/*.yaml`.
    ssh_key_secret: Option<String>,
}

#[derive(Debug, Clone)]
enum ImageSource {
    Registry(String),
    Bare(String),
}

#[derive(Debug, Clone, Copy)]
enum HostUserOrigin {
    SshConfig,
    HostArg,
    Default,
}

#[derive(Debug, Clone, Default)]
struct InferredSources {
    service: &'static str,
    image: &'static str,
    host: &'static str,
    port: &'static str,
    user_override: Option<&'static str>,
    healthcheck_dup: bool,
}

fn infer_plan(detection: &Detection, opts: &InitOpts) -> Result<WizardPlan> {
    let (service, service_origin) = resolve_service(opts, detection)?;
    let dockerfile = detection.dockerfile.as_ref();
    let (image, image_origin) = resolve_image(opts, detection, &service, dockerfile.is_some());

    let (host_address, host_user, host_user_origin, host_origin) = resolve_host(opts, detection)?;

    let port = if opts.no_port {
        None
    } else if let Some(p) = opts.port {
        Some(p)
    } else if let Some(p) = dockerfile.and_then(|d| d.expose) {
        Some(p)
    } else {
        Some(DEFAULT_PORT)
    };
    let port_origin: &'static str = if opts.no_port {
        ""
    } else if opts.port.is_some() {
        "--port flag"
    } else if dockerfile.and_then(|d| d.expose).is_some() {
        "Dockerfile EXPOSE"
    } else {
        "default"
    };

    let user_override = dockerfile.and_then(|d| d.user.clone());
    let user_origin = user_override.as_ref().map(|_| "Dockerfile USER");

    let healthcheck_dup = dockerfile.is_some_and(|d| d.has_healthcheck);

    Ok(WizardPlan {
        service,
        image,
        tag: "latest".to_string(),
        host_address,
        host_user,
        host_user_origin,
        port,
        user_override,
        sources: InferredSources {
            service: service_origin,
            image: image_origin,
            host: host_origin,
            port: port_origin,
            user_override: user_origin,
            healthcheck_dup,
        },
        age_recipient: None,
        ssh_key_secret: None,
    })
}

fn prompt_plan(detection: &Detection, opts: &InitOpts) -> Result<WizardPlan> {
    // Interactive mode reuses the inference defaults; the operator
    // hits Enter to accept each one.
    let mut plan = match infer_plan(detection, opts) {
        Ok(p) => p,
        Err(_) => fallback_plan(detection)?,
    };

    let service = ask("service name", &plan.service)?;
    plan.service = sanitize_service_name(&service).unwrap_or(plan.service);

    let image_str = match &plan.image {
        ImageSource::Registry(s) | ImageSource::Bare(s) => s.clone(),
    };
    let image = ask(
        "image (registry path or bare name for local-build)",
        &image_str,
    )?;
    plan.image = if image.contains('/') {
        ImageSource::Registry(image)
    } else {
        ImageSource::Bare(image)
    };

    let host = ask("ssh host (address)", &plan.host_address)?;
    plan.host_address = host;
    let user = ask("ssh user", &plan.host_user)?;
    plan.host_user = user;
    plan.host_user_origin = HostUserOrigin::HostArg;

    if !opts.no_port {
        let suggested = plan.port.map(|p| p.to_string()).unwrap_or_default();
        let answer = ask("http port (blank = no healthcheck)", &suggested)?;
        plan.port = if answer.is_empty() {
            None
        } else {
            Some(answer.parse().context("port must be a number")?)
        };
    }
    Ok(plan)
}

fn fallback_plan(detection: &Detection) -> Result<WizardPlan> {
    Ok(WizardPlan {
        service: detection
            .cwd_basename
            .as_deref()
            .and_then(sanitize_service_name)
            .unwrap_or_else(|| "app".into()),
        image: ImageSource::Bare("app".into()),
        tag: "latest".into(),
        host_address: String::new(),
        host_user: DEFAULT_SSH_USER.into(),
        host_user_origin: HostUserOrigin::Default,
        port: Some(DEFAULT_PORT),
        user_override: None,
        sources: InferredSources::default(),
        age_recipient: None,
        ssh_key_secret: None,
    })
}

fn resolve_service(opts: &InitOpts, detection: &Detection) -> Result<(String, &'static str)> {
    if let Some(name) = opts.service.as_deref() {
        let normalized =
            sanitize_service_name(name).context("--service name must be a DNS label")?;
        return Ok((normalized, "--service flag"));
    }
    let from_cwd = detection
        .cwd_basename
        .as_deref()
        .and_then(sanitize_service_name);
    Ok((from_cwd.unwrap_or_else(|| "app".into()), "cwd"))
}

fn resolve_image(
    opts: &InitOpts,
    detection: &Detection,
    service: &str,
    has_dockerfile: bool,
) -> (ImageSource, &'static str) {
    if let Some(img) = opts.image.as_deref() {
        let s = img.to_string();
        let source = if s.contains('/') {
            ImageSource::Registry(s)
        } else {
            ImageSource::Bare(s)
        };
        return (source, "--image flag");
    }
    if let Some(remote) = detection.git_remote.as_deref()
        && let Some(suggestion) = parse_image_from_remote(remote)
    {
        return (ImageSource::Registry(suggestion), "git remote");
    }
    if has_dockerfile {
        return (
            ImageSource::Bare(service.to_string()),
            "Dockerfile (bare name)",
        );
    }
    (
        ImageSource::Bare(service.to_string()),
        "service name (bare)",
    )
}

fn resolve_host(
    opts: &InitOpts,
    detection: &Detection,
) -> Result<(String, String, HostUserOrigin, &'static str)> {
    if let Some(host) = opts.host.as_deref() {
        let (user, addr) = match host.split_once('@') {
            Some((u, a)) => (u.to_string(), a.to_string()),
            None => {
                let user = detection
                    .ssh_host
                    .as_ref()
                    .filter(|h| h.address == host)
                    .and_then(|h| h.user.clone())
                    .unwrap_or_else(|| DEFAULT_SSH_USER.to_string());
                (user, host.to_string())
            }
        };
        let origin = if host.contains('@') {
            HostUserOrigin::HostArg
        } else if detection
            .ssh_host
            .as_ref()
            .is_some_and(|h| h.address == host && h.user.is_some())
        {
            HostUserOrigin::SshConfig
        } else {
            HostUserOrigin::Default
        };
        return Ok((addr, user, origin, "positional arg"));
    }
    if let Some(hint) = detection.ssh_host.as_ref() {
        // Don't silently use the first ssh-config entry — it's almost
        // never what the operator wants. On a TTY, confirm or override
        // it interactively. Otherwise (CI, no tty) bail with the same
        // helpful message as the no-detection path.
        if io::stdin().is_terminal() {
            let suggested = match &hint.user {
                Some(u) => format!("{u}@{}", hint.address),
                None => hint.address.clone(),
            };
            let answer = ask("ssh host (user@address)", &suggested)?;
            let (user, addr) = match answer.split_once('@') {
                Some((u, a)) => (u.to_string(), a.to_string()),
                None => (
                    hint.user.clone().unwrap_or_else(|| DEFAULT_SSH_USER.into()),
                    answer,
                ),
            };
            return Ok((addr, user, HostUserOrigin::HostArg, "interactive prompt"));
        }
        anyhow::bail!(
            "found `{}` in ~/.ssh/config but not picking it silently — pass one as positional arg, e.g. `yoink init deploy@{}`, or run interactively in a terminal",
            hint.address,
            hint.address,
        );
    }
    anyhow::bail!(
        "couldn't infer an ssh host — pass one as positional arg, e.g. `yoink init deploy@my-server`, or use --interactive"
    )
}

/// Best-effort: turn a git remote URL into a registry-friendly image path.
/// Returns None when ambiguous so the caller can fall back to a bare name.
pub fn parse_image_from_remote(url: &str) -> Option<String> {
    // SSH form: git@github.com:owner/repo.git
    let stripped = url.strip_suffix(".git").unwrap_or(url);
    if let Some(rest) = stripped.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some(format_image_path(host, path));
    }
    // HTTPS form: https://github.com/owner/repo[.git]
    if let Some(rest) = stripped
        .strip_prefix("https://")
        .or_else(|| stripped.strip_prefix("http://"))
    {
        let (host, path) = rest.split_once('/')?;
        return Some(format_image_path(host, path));
    }
    None
}

fn format_image_path(host: &str, path: &str) -> String {
    let host_lc = host.to_ascii_lowercase();
    let path_lc = path.to_ascii_lowercase();
    match host_lc.as_str() {
        "github.com" => format!("ghcr.io/{path_lc}"),
        "gitlab.com" => format!("registry.gitlab.com/{path_lc}"),
        _ => format!("{host_lc}/{path_lc}"),
    }
}

/// Normalize a string into a yoink service name (DNS-label-ish:
/// `[a-z0-9-]+`, no leading digit, non-empty).
pub fn sanitize_service_name(input: &str) -> Option<String> {
    let lower = input.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if matches!(c, '-' | '_' | '.' | '/' | ' ') && !out.is_empty() && !out.ends_with('-')
        {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().next()?.is_ascii_digit() {
        return None;
    }
    Some(trimmed.to_string())
}

fn ask(prompt: &str, default: &str) -> Result<String> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("{prompt} required (stdin is not a tty)");
    }
    let mut out = io::stdout();
    if default.is_empty() {
        write!(out, "{prompt}: ")?;
    } else {
        write!(out, "{prompt} [{default}]: ")?;
    }
    out.flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    let answer = buf.trim();
    if answer.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(answer.to_string())
    }
}

fn render(plan: &WizardPlan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# generated by yoink {}; edit freely.\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str("# https://oddur.github.io/yoink/docs/reference/config\n\n");
    out.push_str("hosts:\n");
    if let Some(name) = &plan.ssh_key_secret {
        out.push_str(&format!("  - address: {}\n", plan.host_address));
        out.push_str(&format!("    user: {}\n", plan.host_user));
        out.push_str(&format!("    ssh_key_secret: {name}\n\n"));
    } else {
        out.push_str(&format!(
            "  - {{ address: {}, user: {} }}\n\n",
            plan.host_address, plan.host_user
        ));
    }
    if let Some(public) = &plan.age_recipient {
        out.push_str("secrets:\n");
        out.push_str("  provider: age\n");
        out.push_str("  recipients:\n");
        out.push_str(&format!("    - {public}\n"));
        out.push('\n');
    }
    if plan.ssh_key_secret.is_some() {
        out.push_str("# `yoink hosts add` writes per-host fragments here.\n");
        out.push_str("include:\n");
        out.push_str("  - hosts/*.yaml\n\n");
    }
    out.push_str("services:\n");
    out.push_str(&format!("  - name: {}\n", plan.service));
    let image = match &plan.image {
        ImageSource::Registry(s) | ImageSource::Bare(s) => s.clone(),
    };
    out.push_str(&format!("    image: {image}\n"));
    let tag_comment = if is_git_repo() {
        format!(
            "# use --tag {}=$(git rev-parse HEAD) at deploy",
            plan.service
        )
    } else {
        "# pin a stable tag before going to production".to_string()
    };
    out.push_str(&format!(
        "    tag: {}                   {tag_comment}\n",
        plan.tag
    ));
    out.push_str("    run:\n");
    if let Some(p) = plan.port {
        out.push_str(&format!("      port: {p}\n"));
        out.push_str("      healthcheck_path: /health  # remove if your app has no healthcheck\n");
    }
    if let Some(user) = &plan.user_override {
        out.push_str("      options:\n");
        out.push_str(&format!(
            "        user: {user:?}    # matches Dockerfile USER\n"
        ));
    }
    out
}

/// True if the current directory is inside a git working tree. Used
/// to gate the `--tag $(git rev-parse HEAD)` hint so non-git repos
/// don't see suggestions that would error. Memoised: the answer
/// can't change between the yaml-render and summary-print sites
/// inside a single `cmd_init` run.
fn is_git_repo() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let mut p = std::env::current_dir().unwrap_or_default();
        loop {
            if p.join(".git").exists() {
                return true;
            }
            if !p.pop() {
                return false;
            }
        }
    })
}

fn print_summary(plan: &WizardPlan, path: &Path, line_count: usize) {
    use std::fmt::Write as _;
    let host_user_note = match plan.host_user_origin {
        HostUserOrigin::SshConfig => " (user from ~/.ssh/config)",
        HostUserOrigin::HostArg => "",
        HostUserOrigin::Default => " (default user)",
    };
    let image_str = match &plan.image {
        ImageSource::Registry(s) | ImageSource::Bare(s) => s.clone(),
    };
    let mut summary = String::new();
    let _ = writeln!(
        summary,
        "✓ wrote {} ({line_count} lines, validates clean)",
        path.display()
    );
    let _ = writeln!(summary);
    let _ = writeln!(summary, "inferred:");
    let _ = writeln!(
        summary,
        "  service   {:<32}({})",
        plan.service, plan.sources.service
    );
    let _ = writeln!(
        summary,
        "  image     {:<32}({})",
        image_str, plan.sources.image
    );
    let host_target = format!("{}@{}", plan.host_user, plan.host_address);
    let _ = writeln!(
        summary,
        "  host      {:<32}({})",
        host_target, plan.sources.host
    );
    if !host_user_note.is_empty() {
        let _ = writeln!(summary, "            {}", host_user_note.trim_start());
    }
    if let Some(p) = plan.port {
        let _ = writeln!(
            summary,
            "  port      {:<32}({})",
            format!("{p} with /health healthcheck"),
            plan.sources.port
        );
    } else {
        let _ = writeln!(summary, "  port      none (--no-port)");
    }
    if let Some(user) = &plan.user_override {
        let _ = writeln!(
            summary,
            "  user      {:<32}({})",
            user,
            plan.sources.user_override.unwrap_or("dockerfile")
        );
    }
    if plan.sources.healthcheck_dup {
        let _ = writeln!(summary);
        let _ = writeln!(
            summary,
            "note: your Dockerfile has a HEALTHCHECK directive too — yoink's HTTP probe is independent; consolidate when convenient."
        );
    }
    let _ = writeln!(summary);
    let _ = writeln!(summary, "next:");
    let _ = writeln!(summary, "  yoink validate");
    if is_git_repo() {
        let _ = writeln!(
            summary,
            "  yoink up --tag {}=$(git rev-parse HEAD)",
            plan.service
        );
    } else {
        let _ = writeln!(summary, "  yoink up");
    }
    let _ = writeln!(summary);
    let _ = writeln!(
        summary,
        "docs: https://oddur.github.io/yoink/docs/start/first-deploy"
    );
    print!("{summary}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_image_remote_github_ssh() {
        assert_eq!(
            parse_image_from_remote("git@github.com:Oddur/My-App.git").as_deref(),
            Some("ghcr.io/oddur/my-app")
        );
    }

    #[test]
    fn parse_image_remote_github_https() {
        assert_eq!(
            parse_image_from_remote("https://github.com/oddur/my-app.git").as_deref(),
            Some("ghcr.io/oddur/my-app")
        );
        assert_eq!(
            parse_image_from_remote("https://github.com/oddur/my-app").as_deref(),
            Some("ghcr.io/oddur/my-app")
        );
    }

    #[test]
    fn parse_image_remote_gitlab() {
        assert_eq!(
            parse_image_from_remote("git@gitlab.com:group/proj.git").as_deref(),
            Some("registry.gitlab.com/group/proj")
        );
    }

    #[test]
    fn parse_image_remote_other_host() {
        assert_eq!(
            parse_image_from_remote("git@bitbucket.org:user/repo.git").as_deref(),
            Some("bitbucket.org/user/repo")
        );
    }

    #[test]
    fn parse_image_remote_ambiguous_returns_none() {
        assert_eq!(parse_image_from_remote("not-a-url"), None);
        assert_eq!(parse_image_from_remote("ssh://weird-thing"), None);
    }

    #[test]
    fn parse_dockerfile_extracts_expose_and_user() {
        let df = "FROM rust:1.95\nEXPOSE 3000\nUSER hono\nHEALTHCHECK CMD curl /\n";
        let h = parse_dockerfile_text(df);
        assert_eq!(h.expose, Some(3000));
        assert_eq!(h.user.as_deref(), Some("hono"));
        assert!(h.has_healthcheck);
    }

    #[test]
    fn parse_dockerfile_strips_proto_suffix() {
        let df = "EXPOSE 8080/tcp\n";
        assert_eq!(parse_dockerfile_text(df).expose, Some(8080));
    }

    #[test]
    fn parse_dockerfile_multistage_last_wins() {
        let df = "FROM a AS builder\nEXPOSE 9999\nFROM b\nEXPOSE 8080\nUSER app\n";
        let h = parse_dockerfile_text(df);
        assert_eq!(h.expose, Some(8080));
        assert_eq!(h.user.as_deref(), Some("app"));
    }

    #[test]
    fn parse_dockerfile_healthcheck_none() {
        let df = "FROM x\nHEALTHCHECK NONE\n";
        assert!(!parse_dockerfile_text(df).has_healthcheck);
    }

    #[test]
    fn ssh_config_picks_first_non_wildcard() {
        let cfg = "Host *\n  User shared\n\nHost prod-eu-1\n  HostName 1.2.3.4\n  User deploy\n";
        let hint = parse_ssh_config_first_host(cfg).expect("host");
        assert_eq!(hint.address, "prod-eu-1");
        assert_eq!(hint.user.as_deref(), Some("deploy"));
    }

    #[test]
    fn ssh_config_skips_wildcards_and_question_marks() {
        let cfg = "Host *\nHost ?-prod\nHost actual\n  User me\n";
        let hint = parse_ssh_config_first_host(cfg).expect("host");
        assert_eq!(hint.address, "actual");
    }

    #[test]
    fn ssh_config_empty_returns_none() {
        assert!(parse_ssh_config_first_host("").is_none());
        assert!(parse_ssh_config_first_host("# just comments\n").is_none());
    }

    #[test]
    fn sanitize_service_name_basic() {
        assert_eq!(sanitize_service_name("My App").as_deref(), Some("my-app"));
        assert_eq!(
            sanitize_service_name("oddur/yoink-secrets").as_deref(),
            Some("oddur-yoink-secrets")
        );
        assert_eq!(sanitize_service_name("my-app").as_deref(), Some("my-app"));
    }

    #[test]
    fn sanitize_service_name_rejects_invalid() {
        assert_eq!(sanitize_service_name(""), None);
        assert_eq!(sanitize_service_name("---"), None);
        assert_eq!(sanitize_service_name("123abc"), None);
    }

    #[test]
    fn render_round_trips_through_validate() {
        let plan = WizardPlan {
            service: "demo".into(),
            image: ImageSource::Registry("ghcr.io/oddur/demo".into()),
            tag: "latest".into(),
            host_address: "prod-eu-1".into(),
            host_user: "deploy".into(),
            host_user_origin: HostUserOrigin::SshConfig,
            port: Some(8080),
            user_override: Some("hono".into()),
            sources: InferredSources::default(),
            age_recipient: None,
            ssh_key_secret: None,
        };
        let yaml = render(&plan);
        // `parse_str` validates internally, so a clean parse implies
        // a valid config.
        Config::parse_str(&yaml).expect("parse + validate");
    }

    #[test]
    fn render_no_port_round_trips() {
        let plan = WizardPlan {
            service: "demo".into(),
            image: ImageSource::Bare("demo".into()),
            tag: "latest".into(),
            host_address: "h".into(),
            host_user: "u".into(),
            host_user_origin: HostUserOrigin::Default,
            port: None,
            user_override: None,
            sources: InferredSources::default(),
            age_recipient: None,
            ssh_key_secret: None,
        };
        let yaml = render(&plan);
        // `parse_str` validates internally, so a clean parse implies
        // a valid config.
        Config::parse_str(&yaml).expect("parse + validate");
    }

    #[test]
    fn bootstrap_writes_key_with_pubkey_filename_and_safe_mode() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("keys");

        let result = bootstrap_age_identity_in_dir(&dir).unwrap();

        assert!(result.public.starts_with("age1"));
        assert_eq!(
            result.key_path.parent().unwrap(),
            dir,
            "key landed inside the dir we asked for"
        );
        assert_eq!(
            result.key_path.file_stem().unwrap().to_str().unwrap(),
            result.public,
            "filename equals the public recipient — load_identity scan relies on this"
        );

        // The on-disk file contains the secret yoink generated. Reading
        // it back and matching the public half is the round-trip
        // contract that lets `yoink secrets edit` find this key after
        // `yoink init` exits.
        let body = std::fs::read_to_string(&result.key_path).unwrap();
        assert!(
            body.contains(&format!("# public: {}", result.public)),
            "header should record the public recipient"
        );
        assert!(
            body.lines().any(|l| l.starts_with("AGE-SECRET-KEY-1")),
            "body should contain the AGE secret key"
        );

        // File mode is 0600 on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let mode = std::fs::metadata(&result.key_path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o600, "private key file must be 0600");
        }
    }

    #[test]
    fn bootstrap_creates_keys_dir_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Two-level missing path — must be created without error.
        let dir = tmp.path().join("config").join("yoink").join("keys");
        assert!(!dir.exists());

        let result = bootstrap_age_identity_in_dir(&dir).unwrap();

        assert!(dir.is_dir());
        assert!(result.key_path.exists());
    }

    #[test]
    fn bootstrap_refuses_to_overwrite_existing_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("keys");
        let (secret, public) = crate::sealed::keygen();

        write_age_identity(&dir, &secret, &public).unwrap();

        let err = write_age_identity(&dir, &secret, &public).unwrap_err();
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "bail message should make the safety check obvious; got {err}"
        );
    }

    #[test]
    fn render_with_age_recipient_round_trips_and_includes_secrets_block() {
        let plan = WizardPlan {
            service: "demo".into(),
            image: ImageSource::Bare("demo".into()),
            tag: "latest".into(),
            host_address: "h".into(),
            host_user: "u".into(),
            host_user_origin: HostUserOrigin::Default,
            port: Some(8080),
            user_override: None,
            sources: InferredSources::default(),
            age_recipient: Some(
                "age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45".into(),
            ),
            ssh_key_secret: None,
        };
        let yaml = render(&plan);
        assert!(yaml.contains("secrets:\n  provider: age\n"));
        assert!(yaml.contains("age1w8jcq22re378p38nxrudmjqdkyh42cyzsge7snwzqxlzyqt7fgkqmmvy45"));
        let cfg = Config::parse_str(&yaml).expect("parse + validate");
        match cfg.secrets.expect("secrets block") {
            crate::config::SecretsConfig::Age { recipients, .. } => {
                assert_eq!(recipients.len(), 1);
                assert!(recipients[0].starts_with("age1"));
            }
            crate::config::SecretsConfig::Command { .. } => {
                panic!("expected age provider")
            }
        }
    }

    /// Test helper: parse_dockerfile_hints reads from disk; this is
    /// the in-memory variant used in the unit tests above.
    fn parse_dockerfile_text(text: &str) -> DockerfileHints {
        let mut hints = DockerfileHints::default();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let kw = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            let rest = line
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ");
            match kw.as_str() {
                "EXPOSE" => {
                    if let Some(p) = rest.split_whitespace().next() {
                        let bare = p.split('/').next().unwrap_or(p);
                        if let Ok(n) = bare.parse::<u16>() {
                            hints.expose = Some(n);
                        }
                    }
                }
                "USER" => {
                    let token = rest.trim().to_string();
                    if !token.is_empty() {
                        hints.user = Some(token);
                    }
                }
                "HEALTHCHECK" => {
                    hints.has_healthcheck = !rest.trim().eq_ignore_ascii_case("NONE");
                }
                _ => {}
            }
        }
        hints
    }
}
