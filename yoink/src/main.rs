use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use yoink::config::Config;
use yoink::deploy;
use yoink::docker_ops::{DockerOps, Host, RealDockerOps};
use yoink::git;
use yoink::output;
use yoink::secrets::{self, SecretsBundle};
use yoink::status::StatusReport;
use yoink::tui::{self, Mode};

#[derive(Parser)]
#[command(
    name = "yoink",
    version,
    about = "Small, opinionated container deploy CLI."
)]
struct Cli {
    /// Path to the yoink.yaml config file.
    #[arg(short, long, default_value = "yoink.yaml", global = true)]
    config: PathBuf,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a starter `yoink.yaml` for the current repo with
    /// best-practices defaults. Detects cwd / Dockerfile / git remote /
    /// `~/.ssh/config` and writes a complete validated config with
    /// zero prompts in the happy path. Pass HOST as a positional arg
    /// when ssh config can't infer one. `--interactive` engages a
    /// stdio prompt fallback.
    Init {
        /// Ssh target (e.g. `deploy@prod-eu-1` or just `prod-eu-1`).
        /// Optional when `~/.ssh/config` has a non-wildcard Host
        /// yoink can use.
        host: Option<String>,
        /// Overwrite an existing `yoink.yaml`.
        #[arg(long)]
        force: bool,
        /// Prompt for every field (defaults match the inferred
        /// values; hit Enter to accept each).
        #[arg(long)]
        interactive: bool,
        /// Override the inferred service name.
        #[arg(long, value_name = "NAME")]
        service: Option<String>,
        /// Override the inferred port (default: Dockerfile EXPOSE
        /// or 8080).
        #[arg(long)]
        port: Option<u16>,
        /// Don't include `port:` / `healthcheck_path:` in the config
        /// (for services with no HTTP surface).
        #[arg(long)]
        no_port: bool,
        /// Override the inferred image reference.
        #[arg(long, value_name = "PATH")]
        image: Option<String>,
    },
    /// Verify Docker is reachable on each configured host.
    Preflight,
    /// Reconcile every service in the config to its desired spec.
    /// Drifted containers (image, env, mounts, options) are swapped
    /// via the healthcheck-gated loop.
    Up {
        /// Restrict to one or more services. Repeatable.
        #[arg(long = "service", short = 's', value_name = "NAME")]
        services: Vec<String>,
        /// Override a service's tag. Format: `name=tag`. Repeatable.
        /// Bare `--tag <sha>` applies to every selected service.
        #[arg(long = "tag", value_name = "[NAME=]TAG")]
        tag: Vec<String>,
        /// Allow operating with a dirty git working tree.
        #[arg(long)]
        allow_dirty: bool,
        /// Print the planned actions (per-host: which services would
        /// be created, updated, or left untouched, plus orphans) and
        /// exit. Connects to each host to inspect current state, but
        /// never mutates anything.
        #[arg(long)]
        dry_run: bool,
        /// Output format for `--dry-run`. `text` is the default
        /// human-readable form; `markdown` is suitable for posting as
        /// a sticky PR comment from CI; `json` is for machine
        /// consumers (e.g. assert with `jq` that no service would be
        /// updated). Ignored when `--dry-run` isn't set.
        #[arg(long, value_enum, default_value_t = DryRunFormat::Text)]
        format: DryRunFormat,
        /// Skip the registry-pull step. For each (host, image) pair,
        /// ship the locally-built image to the host directly (no
        /// external registry). Default transport is `unregistry` (see
        /// `--transport`). Pair with `--build` for the one-shot
        /// "edit Dockerfile, deploy" loop without CI or a registry.
        #[arg(long)]
        no_registry: bool,
        /// How `--no-registry` ships images to each host.
        /// `unregistry` (default via `auto`) spins up an ephemeral
        /// `ghcr.io/psviderski/unregistry` sidecar on the host, opens
        /// an SSH-tunnelled local port, and pushes layers over the
        /// OCI registry protocol — only the layers the host doesn't
        /// already have cross the wire (massive win on redeploys).
        /// Push runs from the yoink process directly, so it works
        /// even on macOS Docker Desktop where the daemon lives in a
        /// VM. `tarball` opts out: streams the entire `docker save`
        /// tarball over ssh — slower, no dedup, but zero dependencies
        /// on the host beyond docker. `auto` tries unregistry first
        /// and falls back to tarball with a warning if anything in
        /// the unregistry setup fails (e.g. ghcr unreachable, ssh
        /// forward refused). Ignored without `--no-registry`.
        #[arg(long, value_enum, default_value_t = TransportMode::Auto)]
        transport: TransportMode,
        /// Run `docker build` for any selected service with a
        /// `build:` block before deploying. Eliminates the separate
        /// `yoink build && yoink up` two-step for the standalone
        /// workflow — `yoink up --build --no-registry --service my-tool`
        /// is the indie one-shot. With a registry-prefixed image, you
        /// still need to push (`yoink build --push`) — `--build` here
        /// builds without pushing, so this combination is most useful
        /// alongside `--no-registry`.
        #[arg(long)]
        build: bool,
    },
    /// Build one or more services' images via `docker build` against
    /// the operator's local docker daemon. Tags the result as
    /// `<image>:<tag>`. Pair with `yoink up --no-registry` to deploy
    /// the freshly-built image without any registry. Requires the
    /// service to declare a `build:` block.
    Build {
        /// Restrict to one or more services (those with a `build:`
        /// block). Empty = build every service that has one.
        #[arg(long = "service", short = 's', value_name = "NAME")]
        services: Vec<String>,
        /// Override the resolved tag (same shape as `up --tag`).
        #[arg(long = "tag", value_name = "[NAME=]TAG")]
        tag: Vec<String>,
        /// Allow operating with a dirty git working tree.
        #[arg(long)]
        allow_dirty: bool,
        /// Forward `--no-cache` to `docker build`.
        #[arg(long)]
        no_cache: bool,
        /// Run `docker push <image>:<tag>` after a successful build.
        /// Use this when `image:` points at a real remote registry
        /// (`ghcr.io/you/api`, `4db05qgnlk.registry.depot.dev/api`,
        /// etc.) and you want the kamal-style "build locally, deploy
        /// from registry" loop in a single command. Operator must
        /// already be `docker login`'d to the target registry.
        #[arg(long)]
        push: bool,
    },
    /// Show what's running where (across all services).
    Status {
        /// Emit a structured JSON object instead of the default
        /// human-readable table. Use this from CI to assert post-deploy
        /// state programmatically:
        ///   `task yoink:status -- --json | jq '.hosts[] | .containers[] | select(.state != "running")'`
        #[arg(long)]
        json: bool,
    },
    /// Roll a service back to the most recent previously-deployed tag.
    /// Reads `yoink.version` off the host's exited containers labeled
    /// with this service, picks the newest, and dispatches through the
    /// usual `up` flow (rolling swap + healthcheck + migrations).
    /// Pass `--tag <value>` to skip the discovery step and pin to a
    /// specific tag.
    Rollback {
        /// Service name as declared in the config.
        service: String,
        /// Skip discovery; deploy this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Remove yoink-managed containers that the current config no
    /// longer describes (renamed/removed services) plus stale exited
    /// containers from previous reconciles.
    Prune {
        /// Print what would be removed without actually removing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a one-shot command inside a running container for a service.
    /// Output is captured + printed; exit code mirrors the command's.
    /// For an interactive shell with PTY (bash, etc.) use `yoink shell`.
    Exec {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts; otherwise yoink errors with the candidate list.
        #[arg(long)]
        host: Option<String>,
        /// Command to run, after `--`. e.g. `yoink exec api -- ls -la /app`.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Stream or tail logs from a service's container.
    Logs {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
        /// Follow new log lines until Ctrl-C.
        #[arg(long, short)]
        follow: bool,
        /// Lines of history to show before following (default 50).
        #[arg(long, default_value_t = 50)]
        tail: u32,
    },
    /// Print the currently-running tag(s) for a service across hosts.
    /// One line per replica.
    Version { service: String },
    /// Drop into an interactive PTY shell inside a service's container
    /// (k9s-style, terminal-side). Picks `bash` when present, falls
    /// back to `sh`. Exit with `exit` or Ctrl-D.
    Shell {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
    },
    /// Like `shell` but spawns an `alpine` debug sidecar in the
    /// target's pid+net namespaces — for distroless / shell-less
    /// images. The sidecar is `--rm` and is force-removed on exit.
    Debug {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas across
        /// multiple hosts.
        #[arg(long)]
        host: Option<String>,
        /// Image to use for the sidecar. Defaults to alpine.
        #[arg(long, default_value = "alpine")]
        image: String,
    },
    /// Launch the interactive ratatui dashboard.
    Tui {
        /// Initial mode to open.
        #[arg(long, value_enum, default_value_t = Mode::Dashboard)]
        mode: Mode,
        /// Enable mouse capture — scroll wheel selects rows / scrolls
        /// log views. Off by default because mouse capture takes the
        /// mouse away from the host terminal's native text-selection.
        #[arg(long)]
        mouse: bool,
    },
    /// Bounce a service's container without re-deploying. Stops the
    /// container with the configured drain, then starts it again.
    Restart {
        /// Service name as declared in the config.
        service: String,
        /// Pin to a specific host when the service has replicas.
        #[arg(long)]
        host: Option<String>,
    },
    /// SIGKILL a service's container (no graceful drain). Container
    /// stays around for inspect/logs; use `up` or `restart` to bring
    /// it back. Use this when the configured drain isn't fast enough.
    Kill {
        service: String,
        #[arg(long)]
        host: Option<String>,
        /// Skip the "are you sure?" confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Pre-warm a service's image on every host (or a specific one)
    /// without deploying. Useful right before a low-window deploy
    /// where the pull is the longest leg.
    Pull {
        service: String,
        /// Override the tag — same format as `up --tag`.
        #[arg(long)]
        tag: Option<String>,
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Show the deploy history for a service — every yoink-managed
    /// container (running + exited) labeled with the service, sorted
    /// newest-first by deploy time.
    History {
        service: String,
        /// Maximum entries to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// `htop`-style snapshot of every running yoink-managed container
    /// across all hosts, sorted by CPU% descending. Single shot —
    /// wrap in \`watch -n 2 yoink top\` for a live display.
    Top {
        /// Maximum rows to print.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// List every docker network across all configured hosts.
    Networks {
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// List every docker volume across all configured hosts.
    Volumes {
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Dense JSON dump of everything yoink can observe — config,
    /// per-host docker info, every yoink-managed container with
    /// inspect data, stats, log tail, and drift status. Designed to
    /// be piped into an LLM/agent for diagnosis: `yoink dump | pbcopy`.
    /// Env values containing secret-ish substrings are redacted.
    Dump {
        /// Lines of recent logs to include per container.
        #[arg(long, default_value_t = 50)]
        log_tail: u32,
    },
    /// Lint the config and (optionally) ping each host's docker daemon.
    /// Use this in CI before merging a yoink.yaml change.
    Validate {
        /// Also test the ssh+docker connection to every configured host.
        #[arg(long)]
        check_hosts: bool,
    },
    /// Render the Caddy admin-API JSON yoink would push for the
    /// current config. Read-only; useful for inspecting the proxy
    /// config or piping into `caddy adapt` / a debug Caddy's `/load`
    /// for schema-validation. Container upstream lookups are skipped
    /// (uses service-name fallbacks) so this works without a host
    /// connection.
    ProxyRender,
    /// Inspect / release the per-host deploy lock. Useful after a
    /// crashed deploy left a sentinel container running.
    Lock {
        #[command(subcommand)]
        action: LockAction,
    },
    /// Show what would change between the running container and the
    /// target spec for `service` (image SHA, env vars, ports, mounts).
    Diff {
        /// Service name as declared in the config.
        service: String,
        /// Override the target tag the same way `up --tag` does.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Generate shell completions. e.g.
    ///   `yoink completions zsh > ~/.config/zsh/completions/_yoink`
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Manage `age`-sealed secrets (the batteries-included default).
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },
}

/// Subcommands for `yoink secrets`.
#[derive(clap::Subcommand)]
enum SecretsAction {
    /// Generate a fresh age identity. By default the secret key is
    /// printed to stdout — operator decides where to save it
    /// (typically a gitignored `age.key` next to the project's
    /// `yoink.yaml`, OR pasted into a CI secret). The public
    /// recipient is also printed for committing to `yoink.yaml`
    /// under `secrets.recipients:`. Pass `--out PATH` to write the
    /// secret to a specific file with mode 0o600 instead.
    ///
    /// Yoink intentionally does NOT default to a global location
    /// like `~/.config/yoink/age.key` — multiple projects with
    /// distinct identities would collide there.
    Keygen {
        /// Write the secret key to PATH (mode 0o600) instead of
        /// printing it to stdout. Refuses to overwrite an existing
        /// file unless `--force`. Make sure PATH is gitignored.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite an existing identity at `--out`.
        #[arg(long)]
        force: bool,
    },
    /// Decrypt the sealed file into `$EDITOR`, then re-seal on save.
    /// Creates the file if it doesn't exist yet.
    Edit,
    /// Decrypt and print the sealed file. Values are masked unless
    /// `--reveal` is passed; keys are always shown.
    Show {
        /// Print the actual secret values instead of masks.
        #[arg(long)]
        reveal: bool,
    },
    /// One-shot: read a plaintext dotenv from `--in` (or stdin),
    /// seal against the recipients in `yoink.yaml`, write to
    /// `secrets.age` (or `--out`).
    Seal {
        /// Plaintext dotenv input. `-` (or unset) reads from stdin.
        #[arg(long, value_name = "PATH")]
        r#in: Option<PathBuf>,
        /// Output path. Defaults to the configured `secrets.file:`
        /// (or `secrets.age` next to `yoink.yaml`).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Generate a new identity and re-seal `secrets.age` against
    /// both the existing recipients AND the new public key. Prints
    /// the new secret for pasting into a GitHub Actions secret. After
    /// CI is updated to the new key, edit `yoink.yaml` to remove the
    /// old recipient and run `yoink secrets edit` (just save without
    /// changes) to re-seal under the new recipients only.
    Rotate,
}

/// Subcommands for `yoink lock`.
#[derive(clap::Subcommand)]
enum LockAction {
    /// Print holder + age of the deploy lock on each configured host.
    Status,
    /// Force-remove the deploy lock sentinel on each host. Use with
    /// care — only safe when no operator is actually deploying.
    Release {
        /// Restrict to one host instead of all.
        #[arg(long)]
        host: Option<String>,
        /// Skip the "are you sure?" confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let is_tui = matches!(cli.command, Command::Tui { .. });
    init_logging(cli.verbose, is_tui);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("yoink: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8, is_tui: bool) {
    let default_level = match verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if is_tui {
        // The TUI takes over the alt-screen; tracing on stderr would
        // scribble straight onto the rendered widgets. Send logs to
        // a file under XDG state dir so the operator can `tail -f`
        // them in another terminal during a TUI session.
        let path = log_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let writer = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        match writer {
            Some(file) => {
                let _ = builder.with_writer(std::sync::Mutex::new(file)).try_init();
            }
            None => {
                // Fall back to a no-op writer — anything's better
                // than letting tracing scribble over the TUI.
                let _ = builder.with_writer(io::sink).try_init();
            }
        }
        eprintln!("yoink tui: logs → {}", path.display());
    } else {
        let _ = builder.with_writer(io::stderr).try_init();
    }
}

/// Per-OS state directory for the TUI log file. Falls back to
/// `/tmp/yoink-tui.log` if neither `XDG_STATE_HOME` nor `HOME` are
/// set.
fn log_file_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("yoink").join("tui.log");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/state/yoink")
            .join("tui.log");
    }
    PathBuf::from("/tmp/yoink-tui.log")
}

async fn run(cli: Cli) -> Result<()> {
    if let Some(result) = run_bootstrap(&cli.command) {
        return result;
    }

    let config = Config::load_from_path(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Preflight => cmd_preflight(&config).await,
        Command::Up {
            services,
            tag,
            allow_dirty,
            dry_run,
            format,
            no_registry,
            transport,
            build,
        } => {
            cmd_up(
                &config,
                UpOptions {
                    services: &services,
                    tag_args: &tag,
                    allow_dirty,
                    dry_run,
                    format,
                    no_registry,
                    transport: transport.into(),
                    build,
                },
            )
            .await
        }
        Command::Build {
            services,
            tag,
            allow_dirty,
            no_cache,
            push,
        } => {
            cmd_build(
                &config,
                BuildOptions {
                    services: &services,
                    tag_args: &tag,
                    allow_dirty,
                    no_cache,
                    push,
                },
            )
            .await
        }
        Command::Status { json } => cmd_status(&config, json).await,
        Command::Rollback { service, tag } => cmd_rollback(&config, service, tag).await,
        Command::Prune { dry_run } => cmd_prune(&config, dry_run).await,
        Command::Exec { service, host, cmd } => {
            cmd_exec(&config, &service, host.as_deref(), cmd).await
        }
        Command::Logs {
            service,
            host,
            follow,
            tail,
        } => cmd_logs(&config, &service, host.as_deref(), follow, tail).await,
        Command::Version { service } => cmd_version(&config, &service).await,
        Command::Shell { service, host } => {
            cmd_pty(&config, &service, host.as_deref(), PtyMode::Exec).await
        }
        Command::Debug {
            service,
            host,
            image,
        } => cmd_pty(&config, &service, host.as_deref(), PtyMode::Debug { image }).await,
        Command::Restart { service, host } => cmd_restart(&config, &service, host.as_deref()).await,
        Command::Kill { service, host, yes } => {
            cmd_kill(&config, &service, host.as_deref(), yes).await
        }
        Command::Pull { service, tag, host } => {
            cmd_pull(&config, &service, tag.as_deref(), host.as_deref()).await
        }
        Command::History { service, limit } => cmd_history(&config, &service, limit).await,
        Command::Top { limit } => cmd_top(&config, limit).await,
        Command::Networks { host } => cmd_networks(&config, host.as_deref()).await,
        Command::Volumes { host } => cmd_volumes(&config, host.as_deref()).await,
        Command::Dump { log_tail } => cmd_dump(&config, log_tail).await,
        Command::Validate { check_hosts } => cmd_validate(&config, check_hosts).await,
        Command::ProxyRender => cmd_proxy_render(&config).await,
        Command::Lock { action } => cmd_lock(&config, action).await,
        Command::Diff { service, tag } => cmd_diff(&config, &service, tag.as_deref()).await,
        Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
        Command::Secrets { action } => cmd_secrets(&config, action),
        Command::Tui { mode, mouse } => cmd_tui(&config, cli.config.clone(), mode, mouse).await,
        Command::Init { .. } => unreachable!("init handled by run_bootstrap"),
    }
}

async fn cmd_preflight(config: &Config) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let mut had_error = false;
    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        match ops.version(&host).await {
            Ok(v) => println!(
                "✓ {}: docker {} (api {}) on {}/{}",
                host.address,
                v.server_version.as_deref().unwrap_or("?"),
                v.api_version.as_deref().unwrap_or("?"),
                v.os.as_deref().unwrap_or("?"),
                v.arch.as_deref().unwrap_or("?"),
            ),
            Err(e) => {
                had_error = true;
                eprintln!("✗ {}: {e:#}", host.address);
            }
        }
    }
    if had_error {
        anyhow::bail!("preflight failed for one or more hosts");
    }
    Ok(())
}

// `UpOptions` mirrors the `up` subcommand's flags 1:1. The bool count
// is the actual CLI surface; rolling them into an enum would just hide
// the same surface area at higher cognitive cost.
#[allow(clippy::struct_excessive_bools)]
struct UpOptions<'a> {
    services: &'a [String],
    tag_args: &'a [String],
    allow_dirty: bool,
    dry_run: bool,
    format: DryRunFormat,
    no_registry: bool,
    transport: yoink::transport::Transport,
    build: bool,
}

#[allow(clippy::too_many_lines)] // borderline (6 lines over); split if it grows further
async fn cmd_up(config: &Config, up: UpOptions<'_>) -> Result<()> {
    use yoink::docker_ops::Host;
    use yoink::lock::HostLock;
    let UpOptions {
        services,
        tag_args,
        allow_dirty,
        dry_run,
        format,
        no_registry,
        transport,
        build,
    } = up;

    // Load bundle first so build_real_ops can reuse it for any
    // per-host ssh_key_secret resolution.
    let bundle = load_secrets_bundle(config).await?;
    // Wrap in Arc so the heartbeat tasks (one per host lock) can hold
    // their own clone for the duration of the deploy.
    let ops: std::sync::Arc<dyn DockerOps> =
        std::sync::Arc::new(build_real_ops(config, bundle.as_ref()).await?);
    let tag_overrides = parse_tag_overrides(tag_args, services, allow_dirty)?;

    let services_filter = services_filter(services);

    if dry_run {
        return run_dry_run(
            ops.as_ref(),
            config,
            &tag_overrides,
            services_filter,
            bundle.as_ref(),
            format,
        )
        .await;
    }

    // Optional `--build` pre-flight: rebuild any selected service that
    // declares a `build:` block before deploying. Lets the indie
    // "drop yoink.yaml in repo and `yoink up --build --no-registry`"
    // loop work as a one-shot without remembering to run `yoink build`
    // separately. Push is intentionally not auto-engaged — kamal-style
    // flows still go through the explicit `yoink build --push` step.
    if build {
        for svc in config.selected_services(services_filter) {
            if svc.build.is_none() {
                continue;
            }
            let tag = yoink::build::resolve_service_tag(svc, &tag_overrides)?;
            yoink::build::build_service(config, svc, &tag, false, false)
                .await
                .with_context(|| format!("build {}", svc.name))?;
        }
    }

    // No-registry pre-flight: save+load every selected image to every
    // applicable host so the reconcile loop's `image_present` check
    // short-circuits any registry pull. Failures here block the
    // deploy ("forgot to `yoink build`?" / local daemon down).
    if no_registry {
        yoink::build::load_images_to_hosts(
            ops.as_ref(),
            config,
            &tag_overrides,
            services_filter,
            transport,
        )
        .await?;
    }

    // Per-host advisory locks. Sentinel container holds the lock; a
    // tokio heartbeat task touches it every few seconds. If yoink
    // dies, heartbeats stop, sentinel self-exits within ~30s, and
    // the next deploy reaps it as an orphan. See `lock.rs`.
    let acquire_futs = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let ops = ops.clone();
        async move {
            let lock = HostLock::acquire(&*ops, host.clone())
                .await
                .with_context(|| format!(
                    "another `yoink up` appears to be in progress on {} (lock container running). \
                     If you're sure no operator is deploying, the sentinel will self-exit within ~30s; \
                     retry then. To force-clear: `docker rm -f yoink-deploy-lock` on the host.",
                    host.address,
                ))?;
            anyhow::Ok(lock)
        }
    });
    let mut locks: Vec<HostLock> = futures_util::future::try_join_all(acquire_futs).await?;
    for lock in &mut locks {
        lock.spawn_heartbeat(ops.clone());
    }

    let mut sink = |service: Option<&str>, event: deploy::DeployEvent| {
        eprintln!("{}", output::format_deploy_event(service, &event));
    };

    // Phase 0: prefetch all service images in parallel when this is
    // a "deploy everything" run. Single-service deploys would gain
    // nothing (one image to pull) so skip the wrapper.
    if services_filter.is_none() {
        let prefetch_cb: std::sync::Arc<dyn Fn(deploy::DeployEvent) + Send + Sync> =
            std::sync::Arc::new(|e| {
                eprintln!("{}", output::format_deploy_event(None, &e));
            });
        if let Err(e) = deploy::prefetch_images(
            ops.clone(),
            config,
            &tag_overrides,
            services_filter,
            bundle.as_ref(),
            prefetch_cb,
        )
        .await
        {
            for lock in locks {
                lock.release(&*ops).await;
            }
            return Err(e).context("prefetch images");
        }
    }

    let reconcile_result = deploy::reconcile(
        &*ops,
        config,
        &tag_overrides,
        services_filter,
        bundle.as_ref(),
        &mut sink,
    )
    .await;

    // Always release the locks, even on reconcile failure — keeping
    // them around blocks the operator's next attempt until the
    // sentinel self-exits.
    for lock in locks {
        lock.release(&*ops).await;
    }

    let reports = reconcile_result.context("reconcile")?;
    println!("{}", output::format_deploy_summary(&reports));
    Ok(())
}

/// Convert the CLI's `--service` repeat into the `Option<&[String]>`
/// shape every downstream caller wants: empty list → no filter →
/// "every service"; non-empty → filter to those.
fn services_filter(services: &[String]) -> Option<&[String]> {
    if services.is_empty() {
        None
    } else {
        Some(services)
    }
}

/// Build the `service_name → tag` override map. Each `--tag` argument
/// is either `name=tag` (override that named service's tag) or a bare
/// `tag` (applies to every `--service`-selected service). A bare tag
/// without `--service` is rejected as ambiguous in multi-service mode.
fn parse_tag_overrides(
    tag_args: &[String],
    services_filter: &[String],
    allow_dirty: bool,
) -> Result<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;
    let mut out = BTreeMap::new();
    let mut bare: Option<String> = None;
    for arg in tag_args {
        if let Some((name, tag)) = arg.split_once('=') {
            out.insert(name.to_string(), tag.to_string());
        } else if bare.is_some() {
            anyhow::bail!("multiple bare --tag values; use --tag <name>=<tag> per service");
        } else {
            bare = Some(arg.clone());
        }
    }
    if let Some(t) = bare {
        if services_filter.is_empty() {
            // No service filter: bare tag is treated as "current commit"
            // applied to every service in the config that uses git
            // versioning. We don't know which from here, so the user is
            // expected to pair `--tag` with `--service` for now.
            anyhow::bail!(
                "bare --tag without --service is ambiguous in multi-service mode; \
                 use --service <name> --tag <sha> per service or --tag <name>=<tag>"
            );
        }
        for service in services_filter {
            out.insert(service.clone(), t.clone());
        }
    } else if !out.is_empty() {
        // user provided per-service tags only; nothing else to do.
    } else if !services_filter.is_empty() && !allow_dirty {
        // fall back to git version for selected services. Helpful in CI
        // where you'd otherwise just `--tag $GIT_SHA`. Operator can
        // always opt out by passing tags explicitly.
        if let Ok(version) = git::version(std::path::Path::new("."), allow_dirty) {
            for service in services_filter {
                out.insert(service.clone(), version.clone());
            }
        }
    }
    Ok(out)
}

struct BuildOptions<'a> {
    services: &'a [String],
    tag_args: &'a [String],
    allow_dirty: bool,
    no_cache: bool,
    push: bool,
}

/// `yoink build` — run `docker build` for every selected service that
/// declares a `build:` block. Tags the result on the operator's
/// local daemon as `<image>:<tag>`, ready for `yoink up --no-registry`.
async fn cmd_build(config: &Config, build: BuildOptions<'_>) -> Result<()> {
    let BuildOptions {
        services,
        tag_args,
        allow_dirty,
        no_cache,
        push,
    } = build;
    let tag_overrides = parse_tag_overrides(tag_args, services, allow_dirty)?;
    let services_filter = services_filter(services);
    let explicit_services = services_filter.is_some();

    let mut built_any = false;
    for svc in config.selected_services(services_filter) {
        if svc.build.is_none() {
            // Explicit `--service api` against a service without a
            // `build:` block is operator error — they expected a
            // build. Bare `yoink build` (no filter) just skips
            // registry-only services silently.
            if explicit_services {
                anyhow::bail!(
                    "service {:?} has no `build:` block — add one or build the image yourself",
                    svc.name
                );
            }
            continue;
        }
        let tag = yoink::build::resolve_service_tag(svc, &tag_overrides)?;
        yoink::build::build_service(config, svc, &tag, no_cache, push)
            .await
            .with_context(|| format!("build {}", svc.name))?;
        built_any = true;
    }
    if !built_any {
        anyhow::bail!(
            "no services with a `build:` block matched. \
             Add `build:` to a service or pass --service to one that has it."
        );
    }
    Ok(())
}

async fn cmd_status(config: &Config, json: bool) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status")?;
    if json {
        // serde_json::to_string_pretty over StatusReport (derives Serialize on
        // each level) — stable contract for CI parsing.
        let out = serde_json::to_string_pretty(&report).context("serialize status as json")?;
        println!("{out}");
    } else {
        println!("{}", output::format_status_table(&report));
    }
    Ok(())
}

async fn cmd_rollback(config: &Config, service: String, tag: Option<String>) -> Result<()> {
    use yoink::docker_ops::Host;
    let ops = build_real_ops(config, None).await?;

    // Verify the service exists in config; otherwise the user typo'd.
    if !config.services.iter().any(|s| s.name == service) {
        anyhow::bail!(
            "service {service:?} is not declared in the loaded config; \
             pass --service to one that is"
        );
    }

    // Resolve the tag — either user-supplied or discovered.
    let resolved_tag = if let Some(t) = tag {
        t
    } else {
        // Walk every configured host, gather containers labeled with this
        // service, find the newest non-running one. That's the tag we
        // rolled away from. Skip the currently-running container even if
        // its `created_unix` would otherwise win.
        let label = format!("yoink.service={service}");
        let mut candidates: Vec<(i64, String)> = Vec::new();
        let mut current_versions: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for host_cfg in &config.hosts {
            let host = Host::from(host_cfg);
            let containers = ops
                .list_containers_by_label(&host, &label)
                .await
                .with_context(|| format!("list {label} on {}", host.address))?;
            for c in containers {
                if let Some(v) = &c.yoink_version {
                    if c.is_running() {
                        current_versions.insert(v.clone());
                    } else if let Some(ts) = c.created_unix {
                        candidates.push((ts, v.clone()));
                    }
                }
            }
        }
        candidates.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
        candidates
            .into_iter()
            .find(|(_, v)| !current_versions.contains(v))
            .map(|(_, v)| v)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no previous version of {service:?} found on any host; \
                     pass --tag explicitly to deploy a specific image"
                )
            })?
    };

    eprintln!("rolling {service} back to tag {resolved_tag}");
    let services_arg = std::slice::from_ref(&service);
    let tag_arg = format!("{service}={resolved_tag}");
    let tag_args = std::slice::from_ref(&tag_arg);
    cmd_up(
        config,
        UpOptions {
            services: services_arg,
            tag_args,
            allow_dirty: true,
            dry_run: false,
            format: DryRunFormat::Text,
            no_registry: false,
            transport: yoink::transport::Transport::Auto,
            build: false,
        },
    )
    .await
}

async fn cmd_prune(config: &Config, dry_run: bool) -> Result<()> {
    use yoink::prune::{self, PruneReason};
    let ops = build_real_ops(config, None).await?;
    let report = prune::run(&ops, config, dry_run).await.context("prune")?;
    let items = if dry_run {
        &report.planned
    } else {
        &report.removed
    };
    if items.is_empty() {
        println!("nothing to prune");
        return Ok(());
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for item in items {
        let svc = item.service.as_deref().unwrap_or("?");
        let reason = match item.reason {
            PruneReason::ServiceNotInConfig => "service not in config",
            PruneReason::StaleExited => "stale exited",
        };
        println!(
            "[{}] {verb} {} (service={svc}, {reason})",
            item.host, item.container
        );
    }
    Ok(())
}

async fn load_secrets_bundle(config: &Config) -> Result<Option<SecretsBundle>> {
    secrets::load_bundle(config)
        .await
        .context("load secrets bundle")
}

/// Build a `RealDockerOps` aware of any `hosts[].ssh_key_secret`
/// entries — decrypts them into 0600 tempfiles held inside the ops
/// object and threads the paths into bollard + `ssh_probe`.
///
/// `bundle` is the caller's pre-loaded secrets bundle (e.g. `cmd_up`
/// already loads it for service-level secrets). Pass `None` when the
/// caller doesn't otherwise need the bundle — it'll be loaded on
/// demand only if a host actually declares `ssh_key_secret:`.
async fn build_real_ops(
    config: &Config,
    bundle: Option<&SecretsBundle>,
) -> Result<RealDockerOps> {
    // Fast path: no host needs a managed key. Skip bundle access
    // entirely so commands that don't otherwise touch secrets pay
    // nothing.
    if config.hosts.iter().all(|h| h.ssh_key_secret.is_none()) {
        return Ok(RealDockerOps::new());
    }
    let owned_bundle;
    let bundle = if let Some(b) = bundle {
        Some(b)
    } else {
        owned_bundle = load_secrets_bundle(config).await?;
        owned_bundle.as_ref()
    };
    let km = yoink::ssh_keys::prepare(config, bundle)
        .context("prepare per-host ssh keys")?;
    Ok(RealDockerOps::with_key_manager(km.map(std::sync::Arc::new)))
}

/// Resolve `(service, optional host)` to exactly one running container
/// via the `yoink.service=<name>` label. Errors with the candidate set
/// when the user doesn't pin a host on a multi-replica service, so an
/// `exec` or `logs` command can't accidentally hit the wrong replica.
async fn resolve_running_container(
    ops: &dyn DockerOps,
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
) -> Result<(yoink::docker_ops::Host, String)> {
    use yoink::docker_ops::Host;
    let label = format!("yoink.service={service}");
    let mut candidates: Vec<(Host, String)> = Vec::new();
    for host_cfg in &config.hosts {
        if let Some(filter) = host_filter
            && filter != host_cfg.address
        {
            continue;
        }
        let host = Host::from(host_cfg);
        let containers = ops
            .list_containers_by_label(&host, &label)
            .await
            .with_context(|| format!("list containers on {}", host.address))?;
        for c in containers {
            if c.is_running() {
                candidates.push((host.clone(), c.name));
            }
        }
    }
    match candidates.len() {
        0 => anyhow::bail!(
            "no running container with yoink.service={service}{}",
            host_filter
                .map(|h| format!(" on host {h}"))
                .unwrap_or_default()
        ),
        1 => Ok(candidates.into_iter().next().expect("len == 1")),
        _ => {
            let listing = candidates
                .iter()
                .map(|(h, n)| format!("  {} → {}", h.address, n))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "service {service:?} has multiple replicas; pin one with --host:\n{listing}"
            )
        }
    }
}

async fn cmd_exec(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    cmd: Vec<String>,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    let result = ops
        .exec_oneshot(&host, &container, cmd)
        .await
        .with_context(|| format!("exec in {}@{container}", host.address))?;
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    if result.exit_code != 0 {
        anyhow::bail!("command exited with code {}", result.exit_code);
    }
    Ok(())
}

async fn cmd_logs(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    follow: bool,
    tail: u32,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    if follow {
        let mut rx = ops
            .open_log_stream(&host, &container, tail)
            .await
            .with_context(|| format!("open log stream {}@{container}", host.address))?;
        // Honor SIGINT cleanly so Ctrl-C doesn't dump a panic.
        let ctrlc = tokio::signal::ctrl_c();
        tokio::pin!(ctrlc);
        loop {
            tokio::select! {
                line = rx.recv() => match line {
                    Some(l) => println!("{}", l.message),
                    None => break,
                },
                _ = &mut ctrlc => break,
            }
        }
    } else {
        let lines = ops
            .fetch_recent_logs(&host, &container, tail)
            .await
            .with_context(|| format!("fetch logs {}@{container}", host.address))?;
        for l in lines {
            print!("{l}");
        }
    }
    Ok(())
}

async fn cmd_version(config: &Config, service: &str) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect_for_service(&ops, config, service)
        .await
        .context("collect status")?;
    let mut printed = false;
    for host in &report.hosts {
        for c in &host.containers {
            if !c.is_running() {
                continue;
            }
            let version = c.yoink_version.as_deref().unwrap_or("?");
            println!("{}: {} → {} ({})", host.host, c.name, version, c.state);
            printed = true;
        }
    }
    if !printed {
        anyhow::bail!("no running container for service {service:?}");
    }
    Ok(())
}

/// Which flavor of PTY session `cmd_pty` opens — same byte-pump
/// otherwise, the `Debug` arm just exec's an alpine sidecar in the
/// target's pid+net namespaces (and gets force-removed on exit).
enum PtyMode {
    Exec,
    Debug { image: String },
}

async fn cmd_pty(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    mode: PtyMode,
) -> Result<()> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

    let ops: std::sync::Arc<dyn DockerOps> = std::sync::Arc::new(build_real_ops(config, None).await?);
    let (host, container) =
        resolve_running_container(ops.as_ref(), config, service, host_filter).await?;

    let (cols, rows) = size().context("query terminal size")?;

    enable_raw_mode().context("enable raw mode")?;
    let result = pty_session(ops.clone(), &host, &container, &mode, rows, cols).await;
    let _ = disable_raw_mode();
    // Newline so the operator's next shell prompt isn't glued to the
    // last line of in-container output.
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\r\n");
    let _ = stdout.flush();
    result
}

async fn pty_session(
    ops: std::sync::Arc<dyn DockerOps>,
    host: &Host,
    container: &str,
    mode: &PtyMode,
    rows: u16,
    cols: u16,
) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use yoink::docker_ops::ExecKind;

    let session = match mode {
        PtyMode::Exec => {
            let cmd = vec![
                "/bin/sh".into(),
                "-c".into(),
                "if command -v bash >/dev/null 2>&1; then exec bash; else exec /bin/sh; fi".into(),
            ];
            ops.exec_interactive(host, container, cmd, rows, cols)
                .await
                .with_context(|| format!("exec interactive {}@{container}", host.address))?
        }
        PtyMode::Debug { image } => ops
            .start_debug_sidecar(host, container, image, rows, cols)
            .await
            .with_context(|| format!("start debug sidecar on {}", host.address))?,
    };

    let session_id = session.id.clone();
    let session_kind = session.kind;
    let yoink::docker_ops::ExecSession {
        mut stdin,
        mut output,
        ..
    } = session;

    // Pump exec output → terminal stdout. Runs in a background task so
    // the main task can pump stdin → exec.stdin concurrently. Returns
    // when the docker stream EOFs (container shell exited).
    let output_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(item) = output.next().await {
            match item {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    if stdout.write_all(&bytes).await.is_err() {
                        return;
                    }
                    let _ = stdout.flush().await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "exec output stream error");
                    return;
                }
            }
        }
    });

    // Pump terminal stdin → exec.stdin. We treat stdin as raw bytes
    // (the terminal is in raw mode already, so each keypress is the
    // exact byte sequence the in-container shell expects).
    let stdin_task = tokio::spawn(async move {
        let mut stdin_in = tokio::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin_in.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if stdin.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                    let _ = stdin.flush().await;
                }
            }
        }
    });

    // SIGWINCH → resize_exec / resize_container_tty. macOS + Linux
    // both have it; the docker daemon needs the new size so apps
    // like `top` and `vim` re-flow.
    let resize_ops = ops.clone();
    let resize_host = host.clone();
    let resize_id = session_id.clone();
    let resize_task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut sig) = signal(SignalKind::window_change()) else {
            return;
        };
        while sig.recv().await.is_some() {
            let Ok((cols, rows)) = crossterm::terminal::size() else {
                continue;
            };
            let res = match session_kind {
                ExecKind::Exec => {
                    resize_ops
                        .resize_exec(&resize_host, &resize_id, rows, cols)
                        .await
                }
                ExecKind::Sidecar => {
                    resize_ops
                        .resize_container_tty(&resize_host, &resize_id, rows, cols)
                        .await
                }
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, "resize failed");
            }
        }
    });

    // The output task ending means the container shell exited (Ctrl-D
    // / `exit`). The stdin task only ends when our process's stdin is
    // closed, which won't happen normally — abort it on output EOF.
    let _ = output_task.await;
    stdin_task.abort();
    resize_task.abort();

    // Sidecar cleanup — `auto_remove` should handle it but belt + braces.
    if matches!(session_kind, ExecKind::Sidecar)
        && let Err(e) = ops.force_remove_container(host, &session_id).await
    {
        tracing::debug!(error = %e, "sidecar cleanup (probably already auto-removed)");
    }

    Ok(())
}

async fn cmd_tui(config: &Config, config_path: PathBuf, mode: Mode, mouse: bool) -> Result<()> {
    let mut config = config.clone();
    config.push_local_host_if_socket();
    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(build_real_ops(&config, None).await?);
    tui::run(&config, config_path, ops, mode, mouse)
        .await
        .context("run TUI")
}

/// Output format for `yoink up --dry-run`. Mirrors `diff::Format` but
/// is `clap::ValueEnum`-derivable so the CLI surface stays at the
/// binary boundary.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum DryRunFormat {
    #[default]
    Text,
    Markdown,
    Json,
}

/// CLI shape for `--transport`; mirrors `yoink::transport::Transport` so
/// the value-enum stays bound to the binary surface.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum TransportMode {
    #[default]
    Auto,
    Unregistry,
    Tarball,
}

impl From<TransportMode> for yoink::transport::Transport {
    fn from(m: TransportMode) -> Self {
        match m {
            TransportMode::Auto => Self::Auto,
            TransportMode::Unregistry => Self::Unregistry,
            TransportMode::Tarball => Self::Tarball,
        }
    }
}

impl From<DryRunFormat> for yoink::diff::Format {
    fn from(f: DryRunFormat) -> Self {
        match f {
            DryRunFormat::Text => Self::Text,
            DryRunFormat::Markdown => Self::Markdown,
            DryRunFormat::Json => Self::Json,
        }
    }
}

async fn run_dry_run(
    ops: &dyn DockerOps,
    config: &Config,
    tag_overrides: &std::collections::BTreeMap<String, String>,
    services_filter: Option<&[String]>,
    secrets: Option<&yoink::secrets::SecretsBundle>,
    format: DryRunFormat,
) -> Result<()> {
    let report = yoink::diff::compute(ops, config, tag_overrides, services_filter, secrets)
        .await
        .context("compute dry-run diff")?;
    print!("{}", report.render(format.into()));
    Ok(())
}

async fn cmd_restart(config: &Config, service: &str, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    let drain = std::time::Duration::from_secs(10);
    eprintln!("stopping {}@{container} (drain {drain:?})…", host.address);
    ops.stop_container(&host, &container, drain)
        .await
        .with_context(|| format!("stop {}@{container}", host.address))?;
    eprintln!("starting {}@{container}…", host.address);
    ops.start_container(&host, &container)
        .await
        .with_context(|| format!("start {}@{container}", host.address))?;
    eprintln!("ok");
    Ok(())
}

async fn cmd_kill(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
    yes: bool,
) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let (host, container) = resolve_running_container(&ops, config, service, host_filter).await?;
    if !yes {
        eprintln!(
            "about to SIGKILL {}/{container} — the in-process drain is skipped. \
             pass --yes to confirm.",
            host.address
        );
        anyhow::bail!("aborted");
    }
    ops.kill_container(&host, &container)
        .await
        .with_context(|| format!("kill {}@{container}", host.address))?;
    eprintln!("killed {}/{container}", host.address);
    Ok(())
}

async fn cmd_pull(
    config: &Config,
    service: &str,
    tag_override: Option<&str>,
    host_filter: Option<&str>,
) -> Result<()> {
    let svc_cfg = config
        .services
        .iter()
        .find(|s| s.name == service)
        .with_context(|| format!("service {service:?} not in config"))?;
    let tag = tag_override
        .map(str::to_string)
        .or_else(|| svc_cfg.tag.clone())
        .unwrap_or_else(|| {
            // Bare `git` version — same fallback `up` uses when no tag is given.
            git::version(std::path::Path::new("."), true).unwrap_or_else(|_| "latest".into())
        });
    let bundle = load_secrets_bundle(config).await?;
    let credentials = deploy::registry_credentials(config, bundle.as_ref());
    let ops = build_real_ops(config, bundle.as_ref()).await?;
    // Fan out across hosts so a slow daemon doesn't block the others.
    let pulls = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|host_cfg| {
            let host = Host::from(host_cfg);
            let image = svc_cfg.image.clone();
            let tag = tag.clone();
            let credentials = credentials.clone();
            let ops = &ops;
            async move {
                eprintln!(
                    "→ {}: pulling {}",
                    host.address,
                    yoink::docker::image_reference(&image, &tag),
                );
                ops.pull_image(&host, &image, &tag, credentials)
                    .await
                    .with_context(|| format!("pull on {}", host.address))
                    .map(|()| host.address)
            }
        });
    let results = futures_util::future::join_all(pulls).await;
    let mut had_err = false;
    for r in results {
        match r {
            Ok(addr) => println!("✓ {addr}"),
            Err(e) => {
                eprintln!("✗ {e:#}");
                had_err = true;
            }
        }
    }
    if had_err {
        anyhow::bail!("one or more pulls failed");
    }
    Ok(())
}

async fn cmd_history(config: &Config, service: &str, limit: usize) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let label = format!("yoink.service={service}");
    // Fan out across hosts. Each call returns running + exited
    // containers labeled with this service.
    let probes = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let label = label.clone();
        let ops = &ops;
        async move {
            let containers = ops.list_containers_by_label(&host, &label).await?;
            anyhow::Ok((host.address, containers))
        }
    });
    let mut entries: Vec<(i64, String, String, String, String, String)> = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        let (host_addr, containers) = r?;
        for c in containers {
            // Sort key = deployed-at when present, else created_unix,
            // else zero (puts it at the bottom).
            let when = c.yoink_deployed_at.or(c.created_unix).unwrap_or(0);
            entries.push((
                when,
                host_addr.clone(),
                c.name,
                c.yoink_version.unwrap_or_else(|| "?".into()),
                c.state,
                c.yoink_deployed_by.unwrap_or_else(|| "?".into()),
            ));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    if entries.is_empty() {
        anyhow::bail!("no yoink-managed containers found for service {service:?}");
    }
    println!(
        "{:<22}  {:<28}  {:<10}  {:<10}  {:<10}  when",
        "host", "container", "version", "state", "deployed-by"
    );
    for (when, host_addr, name, version, state, by) in entries.into_iter().take(limit) {
        let when_str = if when > 0 {
            output::format_relative_time(Some(when))
        } else {
            "?".into()
        };
        println!("{host_addr:<22}  {name:<28}  {version:<10}  {state:<10}  {by:<10}  {when_str}");
    }
    Ok(())
}

struct TopRow {
    cpu_pct: f64,
    host: String,
    service: String,
    container: String,
    mem_used: i64,
    mem_limit: Option<i64>,
    created: Option<i64>,
}

async fn cmd_top(config: &Config, limit: usize) -> Result<()> {
    use yoink::output::{format_bytes, format_relative_time};
    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status")?;

    let mut targets: Vec<(String, String)> = Vec::new();
    for h in &report.hosts {
        for c in &h.containers {
            if c.is_running() {
                targets.push((h.host.clone(), c.name.clone()));
            }
        }
    }
    let stat_futs = targets.iter().map(|(host_addr, name)| {
        let host = config
            .hosts
            .iter()
            .find(|h| h.address == *host_addr)
            .map(yoink::docker_ops::Host::from);
        let ops = &ops;
        async move {
            let h = host?;
            ops.container_stats(&h, name).await.ok()
        }
    });
    let stats: Vec<Option<yoink::docker_ops::ContainerStats>> =
        futures_util::future::join_all(stat_futs).await;

    let mut rows: Vec<TopRow> = targets
        .into_iter()
        .zip(stats)
        .filter_map(|((host_addr, name), s)| {
            let s = s?;
            let container_info = report
                .hosts
                .iter()
                .find(|h| h.host == host_addr)
                .and_then(|h| h.containers.iter().find(|c| c.name == name));
            Some(TopRow {
                cpu_pct: s.cpu_pct,
                host: host_addr,
                service: container_info
                    .and_then(|c| c.yoink_service.clone())
                    .unwrap_or_else(|| "-".into()),
                container: name,
                mem_used: s.mem_used,
                mem_limit: s.mem_limit,
                created: container_info.and_then(|c| c.created_unix),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!(
        "{:<22}  {:<14}  {:<28}  {:>6}  {:>20}  created",
        "host", "service", "container", "cpu%", "mem"
    );
    for r in rows.into_iter().take(limit) {
        let mem_str = match r.mem_limit {
            Some(limit) if limit > 0 => {
                format!("{} / {}", format_bytes(r.mem_used), format_bytes(limit))
            }
            _ => format_bytes(r.mem_used),
        };
        let created_str = format_relative_time(r.created);
        let cpu = r.cpu_pct;
        let host = r.host;
        let service = r.service;
        let container = r.container;
        println!(
            "{host:<22}  {service:<14}  {container:<28}  {cpu:>5.1}%  {mem_str:>20}  {created_str}"
        );
    }
    Ok(())
}

async fn cmd_networks(config: &Config, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let probes = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|h| {
            let host = Host::from(h);
            let ops = &ops;
            async move {
                let nets = ops.list_networks(&host).await?;
                anyhow::Ok(nets)
            }
        });
    let mut all = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        all.extend(r?);
    }
    all.sort_by(|a, b| (a.host.as_str(), a.name.as_str()).cmp(&(b.host.as_str(), b.name.as_str())));
    println!(
        "{:<22}  {:<28}  {:<8}  {:<8}  {:>4}  internal",
        "host", "name", "driver", "scope", "ctrs"
    );
    for n in all {
        let internal = if n.internal { "yes" } else { "no" };
        let host = n.host;
        let name = n.name;
        let driver = n.driver;
        let scope = n.scope;
        let count = n.container_count;
        println!("{host:<22}  {name:<28}  {driver:<8}  {scope:<8}  {count:>4}  {internal}");
    }
    Ok(())
}

async fn cmd_volumes(config: &Config, host_filter: Option<&str>) -> Result<()> {
    let ops = build_real_ops(config, None).await?;
    let probes = config
        .hosts
        .iter()
        .filter(|h| host_filter.is_none_or(|f| f == h.address))
        .map(|h| {
            let host = Host::from(h);
            let ops = &ops;
            async move {
                let vols = ops.list_volumes(&host).await?;
                anyhow::Ok(vols)
            }
        });
    let mut all = Vec::new();
    for r in futures_util::future::join_all(probes).await {
        all.extend(r?);
    }
    all.sort_by(|a, b| (a.host.as_str(), a.name.as_str()).cmp(&(b.host.as_str(), b.name.as_str())));
    println!(
        "{:<22}  {:<40}  {:<10}  mountpoint",
        "host", "name", "driver"
    );
    for v in all {
        let host = v.host;
        let name = v.name;
        let driver = v.driver;
        let mp = v.mountpoint;
        println!("{host:<22}  {name:<40}  {driver:<10}  {mp}");
    }
    Ok(())
}

/// Substrings in env-var keys that mark the value as secret-ish —
/// those values render as `"<redacted>"` in the dump. Same heuristic
/// the TUI's container-detail pane uses; mirrored here so the dump
/// stays paste-safe.
const SECRET_KEY_HINTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASS",
    "KEY",
    "DSN",
    "CREDENTIAL",
    "SIGNING",
    "JWT",
    "COOKIE",
    "WEBHOOK",
    "BEARER",
    "PRIVATE",
    "SALT",
];

fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_HINTS.iter().any(|h| upper.contains(h))
}

/// Redact userinfo (`user:password@`) from URL-shaped values so a
/// credential-bearing URL doesn't leak the password when the env var
/// name itself doesn't trip `is_secret_key`.
fn redact_value(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let after = &value[scheme_end + 3..];
    let Some(at) = after.find('@') else {
        return value.to_string();
    };
    let host_part = &after[at..];
    let userinfo = &after[..at];
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    format!(
        "{}://{}:<redacted>{}",
        &value[..scheme_end],
        user,
        host_part
    )
}

#[allow(clippy::too_many_lines)]
async fn cmd_dump(config: &Config, log_tail: u32) -> Result<()> {
    use serde_json::json;
    use yoink::deploy;
    use yoink::docker;
    use yoink::lock::LOCK_NAME;

    // Best-effort secrets load — drift hashes are accurate when it
    // succeeds, marked "?" otherwise. Failure is logged via tracing
    // (silenced inside dump output). Loaded first so build_real_ops
    // can reuse it for any per-host ssh_key_secret resolution.
    let secrets = yoink::secrets::load_bundle(config).await.ok().flatten();
    let ops = std::sync::Arc::new(build_real_ops(config, secrets.as_ref()).await?)
        as std::sync::Arc<dyn DockerOps>;

    let mut hosts_json = Vec::new();
    let mut issues: Vec<String> = Vec::new();

    for host_cfg in &config.hosts {
        let host = Host::from(host_cfg);
        let address = host.address.clone();
        let mut host_obj = json!({
            "address": address,
            "user": host.user,
            "is_local": host.is_local(),
        });

        match ops.version(&host).await {
            Ok(v) => {
                host_obj["docker"] = json!({
                    "server_version": v.server_version,
                    "api_version": v.api_version,
                    "os": v.os,
                    "arch": v.arch,
                });
                host_obj["reachable"] = json!(true);
            }
            Err(e) => {
                host_obj["reachable"] = json!(false);
                host_obj["unreachable_error"] = json!(format!("{e:#}"));
                issues.push(format!("host {address} unreachable: {e}"));
                hosts_json.push(host_obj);
                continue;
            }
        }

        host_obj["host_info"] = match ops.host_info(&host).await {
            Ok(i) => json!({
                "n_cpu": i.n_cpu,
                "mem_total": i.mem_total,
                "containers": i.containers,
                "containers_running": i.containers_running,
                "images": i.images,
                "kernel": i.kernel,
                "operating_system": i.operating_system,
            }),
            Err(_) => serde_json::Value::Null,
        };
        host_obj["networks"] =
            serde_json::to_value(ops.list_networks(&host).await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null);
        host_obj["volumes"] =
            serde_json::to_value(ops.list_volumes(&host).await.unwrap_or_default())
                .unwrap_or(serde_json::Value::Null);

        // Lock state — find the sentinel by name.
        let lock_state = ops
            .list_running_containers(&host)
            .await
            .ok()
            .and_then(|cs| cs.into_iter().find(|c| c.name == LOCK_NAME))
            .map(|c| {
                json!({
                    "held": true,
                    "acquired_unix": c.created_unix,
                })
            })
            .unwrap_or(json!({ "held": false }));
        host_obj["deploy_lock"] = lock_state;

        // All yoink-managed containers (running + exited).
        let containers = ops
            .list_containers_by_label(&host, "yoink.managed=true")
            .await
            .unwrap_or_default();
        let mut container_objs = Vec::new();
        for c in containers {
            // Per-container stats (skip if not running).
            let stats = if c.is_running() {
                ops.container_stats(&host, &c.name).await.ok().map(|s| {
                    json!({
                        "cpu_pct": s.cpu_pct,
                        "mem_used": s.mem_used,
                        "mem_limit": s.mem_limit,
                    })
                })
            } else {
                None
            };
            let inspect = ops.inspect_container(&host, &c.name).await.ok();
            let log_tail_lines = ops
                .fetch_recent_logs(&host, &c.name, log_tail)
                .await
                .ok()
                .map(|ls| {
                    ls.into_iter()
                        .map(|l| l.trim_end_matches('\n').to_string())
                        .collect::<Vec<_>>()
                });

            // Drift hash for yoink-managed containers we have a config
            // for. Tag fallback: config's tag if set, else the
            // container's own yoink_version.
            let drift = c.yoink_service.as_deref().and_then(|name| {
                let svc = config.services.iter().find(|s| s.name == name)?;
                let tag = svc.tag.clone().or_else(|| c.yoink_version.clone())?;
                let desired =
                    deploy::build_desired_spec(config, svc, &tag, secrets.as_ref()).ok()?;
                let desired_hash = docker::compute_spec_hash(&desired);
                let running_hash = c.yoink_spec_hash.clone()?;
                Some(json!({
                    "status": if desired_hash == running_hash { "sync" } else { "drift" },
                    "desired_spec_hash": desired_hash,
                    "running_spec_hash": running_hash,
                    "tag_used_for_desired": tag,
                }))
            });
            if let Some(d) = &drift
                && d["status"] == "drift"
            {
                issues.push(format!(
                    "container {} ({}) is drifted from current config",
                    c.name,
                    c.yoink_service.as_deref().unwrap_or("?")
                ));
            }
            if !c.is_running() {
                issues.push(format!(
                    "container {} state={} (status: {})",
                    c.name, c.state, c.status_text
                ));
            }

            // Inspect with env redacted.
            let inspect_json = inspect.map(|i| {
                let env: Vec<serde_json::Value> = i
                    .env
                    .iter()
                    .map(|kv| {
                        let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
                        if is_secret_key(k) {
                            json!({"key": k, "value": "<redacted>"})
                        } else {
                            json!({"key": k, "value": redact_value(v)})
                        }
                    })
                    .collect();
                json!({
                    "image": i.image,
                    "image_id": i.image_id,
                    "command": i.command,
                    "working_dir": i.working_dir,
                    "state": i.state,
                    "status": i.status,
                    "started_at": i.started_at,
                    "finished_at": i.finished_at,
                    "exit_code": i.exit_code,
                    "restart_count": i.restart_count,
                    "restart_policy": i.restart_policy,
                    "pid": i.pid,
                    "ports": i.ports,
                    "mounts": i.mounts,
                    "networks": i.networks,
                    "labels": i.labels,
                    "env": env,
                })
            });

            container_objs.push(json!({
                "name": c.name,
                "image": c.image,
                "state": c.state,
                "status_text": c.status_text,
                "created_unix": c.created_unix,
                "yoink_service": c.yoink_service,
                "yoink_version": c.yoink_version,
                "yoink_spec_hash": c.yoink_spec_hash,
                "yoink_deployed_by": c.yoink_deployed_by,
                "yoink_deployed_at": c.yoink_deployed_at,
                "networks": c.networks,
                "other_labels": c.other_labels,
                "stats": stats,
                "inspect": inspect_json,
                "log_tail": log_tail_lines,
                "drift": drift,
            }));
        }
        host_obj["containers"] = json!(container_objs);

        hosts_json.push(host_obj);
    }

    let dump = json!({
        "yoink": {
            "version": env!("CARGO_PKG_VERSION"),
            "generated_at_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            "secrets_bundle_loaded": secrets.is_some(),
        },
        "config": {
            "deploy": {
                "networks": config.deploy.networks,
            },
            "hosts": config.hosts.iter().map(|h| {
                json!({"address": h.address, "user": h.user})
            }).collect::<Vec<_>>(),
            "services": config.services.iter().map(|s| {
                let networks = s.networks.clone().unwrap_or_else(|| config.deploy.networks.clone());
                json!({
                    "name": s.name,
                    "image": s.image,
                    "tag_pinned": s.tag,
                    "networks": networks,
                    "env_keys": s.env.keys().collect::<Vec<_>>(),
                    "secret_keys": s.secrets,
                    "env_from_secret_keys": s.env_from_secrets.keys().collect::<Vec<_>>(),
                    "host_filter": s.hosts,
                    "replicas": s.run.replicas,
                    "port": s.run.port,
                    "healthcheck_path": s.run.healthcheck_path,
                    "publish": s.run.publish,
                    "binds": s.run.binds,
                    "volumes": s.run.volumes,
                    "files": s.run.files,
                    "labels": s.labels,
                })
            }).collect::<Vec<_>>(),
            "registry_server": config.registry.as_ref().map(|r| r.server.clone()),
            "secrets_provider": config.secrets.as_ref().map(|s| match s {
                yoink::config::SecretsConfig::Age { .. } => "age",
                yoink::config::SecretsConfig::Infisical { .. } => "infisical",
            }),
        },
        "hosts": hosts_json,
        "issues": issues,
    });

    println!("{}", serde_json::to_string_pretty(&dump)?);
    Ok(())
}

async fn cmd_validate(config: &Config, check_hosts: bool) -> Result<()> {
    // `Config::load_from_path` (called from `run`) already ran the
    // structural checks — duplicate names/addresses, missing fields,
    // unknown YAML keys. If we got this far the config is well-formed.
    println!(
        "✓ config OK — {} services across {} hosts",
        config.services.len(),
        config.hosts.len()
    );
    // Caddy schema validation: render the proxy config and run it
    // through `caddy validate` in an ephemeral container. Catches
    // schema errors (invalid `caddy_extra_json`, bad TLS config,
    // unknown handler module) BEFORE deploy time. Best-effort —
    // skips silently if docker isn't on the operator's machine.
    if yoink::proxy::proxy_enabled(config) {
        validate_proxy_render(config).await?;
    }
    if check_hosts {
        cmd_preflight(config).await?;
    }
    Ok(())
}

async fn validate_proxy_render(config: &Config) -> Result<()> {
    let bundle = load_secrets_bundle(config).await?;
    let json = match yoink::proxy::caddy::render(config, |_| Vec::new(), bundle.as_ref()) {
        Ok(j) => j,
        Err(e) => {
            anyhow::bail!("render Caddy config: {e}");
        }
    };
    let body = serde_json::to_string(&json).context("serialize rendered config")?;

    // Probe for a local docker daemon. Skip gracefully if absent.
    let docker_avail = tokio::process::Command::new("docker")
        .arg("version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !docker_avail {
        eprintln!(
            "  (skipping Caddy schema check — docker not available locally; \
             pass --check-hosts or run on a host with docker installed)"
        );
        return Ok(());
    }

    let image = config
        .proxy
        .as_ref()
        .map_or_else(|| "caddy:2".to_string(), yoink::config::ProxyConfig::resolved_image);
    let mut child = tokio::process::Command::new("docker")
        // JSON is Caddy's native config format — no `--adapter` flag.
        // (`--adapter caddyfile` would convert from Caddyfile syntax;
        // we feed JSON directly.)
        .args([
            "run", "--rm", "-i",
            &image,
            "caddy", "validate", "--config", "/dev/stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn `docker run caddy validate`")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(body.as_bytes())
            .await
            .context("write rendered config to caddy validate stdin")?;
        // Drop closes stdin → caddy reads EOF → validation runs.
    }
    let output = child
        .wait_with_output()
        .await
        .context("wait on `docker run caddy validate`")?;
    if output.status.success() {
        println!("✓ Caddy config schema valid");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let extras: Vec<&str> = config
            .services
            .iter()
            .filter(|s| s.caddy_extra_json.is_some())
            .map(|s| s.name.as_str())
            .collect();
        let hint = if extras.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nhint: services with caddy_extra_json: {} — \
                 run `yoink proxy-render` to inspect the rendered config",
                extras.join(", "),
            )
        };
        anyhow::bail!("Caddy rejected the rendered config:\n{stderr}{hint}");
    }
}

async fn cmd_proxy_render(config: &Config) -> Result<()> {
    if !yoink::proxy::proxy_enabled(config) {
        anyhow::bail!(
            "proxy is not enabled — no service has `domain:` and `proxy.enabled` is unset"
        );
    }
    // Loads the secrets bundle when configured so `tls_*_secret` and
    // `client_auth.trust_pool_secret` references resolve. Empty
    // upstream lookup → render uses service-name fallbacks.
    let bundle = load_secrets_bundle(config).await?;
    let json = yoink::proxy::caddy::render(config, |_| Vec::new(), bundle.as_ref())
        .context("render Caddy config")?;
    let pretty = serde_json::to_string_pretty(&json).context("serialize rendered config")?;
    println!("{pretty}");
    Ok(())
}

async fn cmd_lock(config: &Config, action: LockAction) -> Result<()> {
    use yoink::lock::LOCK_NAME;
    let ops = build_real_ops(config, None).await?;
    match action {
        LockAction::Status => {
            // Fan out across hosts — one slow daemon shouldn't make
            // the others wait. Per-host errors surface as Err inline.
            let probes = config.hosts.iter().map(|host_cfg| {
                let host = Host::from(host_cfg);
                let ops = &ops;
                async move {
                    let containers = ops.list_running_containers(&host).await?;
                    let held = containers
                        .iter()
                        .find(|c| c.name == LOCK_NAME)
                        .map(|c| output::format_relative_time(c.created_unix));
                    anyhow::Ok((host.address, held))
                }
            });
            let results = futures_util::future::join_all(probes).await;
            for r in results {
                match r {
                    Ok((addr, Some(age))) => println!("{addr}: HELD (acquired {age})"),
                    Ok((addr, None)) => println!("{addr}: free"),
                    Err(e) => eprintln!("{e:#}"),
                }
            }
        }
        LockAction::Release { host, yes } => {
            if !yes {
                eprintln!(
                    "force-releasing the deploy lock while another operator is deploying \
                     will corrupt that deploy. pass --yes to confirm."
                );
                anyhow::bail!("aborted");
            }
            for host_cfg in &config.hosts {
                if let Some(filter) = &host
                    && filter != &host_cfg.address
                {
                    continue;
                }
                let h = Host::from(host_cfg);
                match ops.force_remove_container(&h, LOCK_NAME).await {
                    Ok(()) => println!("{}: released", h.address),
                    Err(e) => eprintln!("{}: {e:#}", h.address),
                }
            }
        }
    }
    Ok(())
}

async fn cmd_diff(config: &Config, service: &str, tag_override: Option<&str>) -> Result<()> {
    let svc_cfg = config
        .services
        .iter()
        .find(|s| s.name == service)
        .with_context(|| format!("service {service:?} not in config"))?;
    let target_tag = tag_override
        .map(str::to_string)
        .or_else(|| svc_cfg.tag.clone())
        .unwrap_or_else(|| "<git>".into());

    let ops = build_real_ops(config, None).await?;
    let report = StatusReport::collect_for_service(&ops, config, service)
        .await
        .context("collect status")?;
    let mut printed = false;
    for host in &report.hosts {
        for c in &host.containers {
            if !c.is_running() {
                continue;
            }
            let current = c.yoink_version.as_deref().unwrap_or("?");
            let same = current == target_tag;
            let arrow = if same { "==" } else { "→" };
            println!(
                "{}/{}: {} {} {}{}",
                host.host,
                c.name,
                current,
                arrow,
                target_tag,
                if same { " (no change)" } else { "" }
            );
            printed = true;
        }
    }
    if !printed {
        println!(
            "no running container for {service:?} — `up` would create the first one with tag {target_tag}"
        );
    }
    println!(
        "\nspec: {} (env, ports, mounts compared at deploy time via spec hash)",
        yoink::docker::image_reference(&svc_cfg.image, &target_tag),
    );
    Ok(())
}

fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut io::stdout());
}

/// Subcommands that don't need a `yoink.yaml`. Returns `Some(result)`
/// to short-circuit `run`'s config-load step, or `None` to fall
/// through.
fn run_bootstrap(command: &Command) -> Option<Result<()>> {
    match command {
        Command::Completions { shell } => {
            cmd_completions(*shell);
            Some(Ok(()))
        }
        Command::Secrets {
            action: SecretsAction::Keygen { out, force },
        } => Some(cmd_secrets_keygen(out.clone(), *force)),
        Command::Init {
            host,
            force,
            interactive,
            service,
            port,
            no_port,
            image,
        } => Some(yoink::init::cmd_init(yoink::init::InitOpts {
            host: host.clone(),
            force: *force,
            interactive: *interactive,
            service: service.clone(),
            port: *port,
            no_port: *no_port,
            image: image.clone(),
        })),
        _ => None,
    }
}

fn cmd_secrets(config: &Config, action: SecretsAction) -> Result<()> {
    match action {
        SecretsAction::Keygen { out, force } => cmd_secrets_keygen(out, force),
        SecretsAction::Edit => cmd_secrets_edit(config),
        SecretsAction::Show { reveal } => cmd_secrets_show(config, reveal),
        SecretsAction::Seal { r#in, out } => cmd_secrets_seal(config, r#in.as_deref(), out),
        SecretsAction::Rotate => cmd_secrets_rotate(config),
    }
}

fn cmd_secrets_keygen(out: Option<PathBuf>, force: bool) -> Result<()> {
    use yoink::sealed;
    let (secret, public) = sealed::keygen();
    let recipient_block =
        format!("  secrets:\n    provider: age\n    recipients:\n      - {public}");
    let Some(path) = out else {
        // No --out: print the secret to stdout. Operator decides
        // where to save (typically a gitignored file alongside the
        // project's yoink.yaml, or pasted into a CI secret).
        // Yoink intentionally does NOT default-write to a global
        // path like ~/.config/yoink/age.key because multiple
        // projects with distinct identities would collide there.
        println!("New age identity. Save the secret somewhere — yoink won't.");
        println!();
        println!("Secret (private — never commit; gitignore the file you save it to):");
        println!();
        println!("{secret}");
        println!();
        println!("Public recipient (add to yoink.yaml):");
        println!();
        println!("{recipient_block}");
        println!();
        println!("Suggested next steps:");
        println!("  • Save the secret to ./age.key (gitignored), then:");
        println!("      export YOINK_AGE_KEY_FILE=$(pwd)/age.key");
        println!("  • Or paste it into a CI secret named YOINK_AGE_KEY.");
        println!("  • Clear your terminal scrollback when done.");
        return Ok(());
    };
    if path.exists() && !force {
        return Err(anyhow::anyhow!(
            "{} already exists — pass --force to overwrite",
            path.display()
        ));
    }
    let body = format!(
        "# created: {}\n# public key: {public}\n{secret}\n",
        chrono_like_now(),
    );
    sealed::write_atomically_secret(&path, body.as_bytes())?;
    println!("wrote identity to {} (mode 0600)", path.display());
    println!("public recipient: {public}");
    println!();
    println!("Add this to yoink.yaml:");
    println!();
    println!("{recipient_block}");
    println!();
    println!("Make sure {} is gitignored.", path.display());
    Ok(())
}

fn cmd_secrets_edit(config: &Config) -> Result<()> {
    use yoink::sealed;
    let (file_override, recipients) = expect_age_block(config)?;
    let path = sealed::resolve_sealed_path(config, file_override.as_deref());
    let plaintext = if path.exists() {
        let bytes =
            std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
        let identity = sealed::load_identity()?;
        sealed::unseal(&bytes, &identity)?
    } else {
        String::from("# yoink secrets — KEY=value, one per line\n")
    };
    let edited = open_in_editor(&plaintext)?;
    let parsed = sealed::parse_dotenv(&edited)?;
    let canonical = sealed::render_dotenv(&parsed);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    sealed::write_atomically(&path, &sealed_bytes)?;
    println!("sealed {} key(s) to {}", parsed.len(), path.display());
    Ok(())
}

fn cmd_secrets_show(config: &Config, reveal: bool) -> Result<()> {
    use yoink::config::SecretsConfig;
    use yoink::sealed;
    let SecretsConfig::Age { file, .. } = expect_secrets_provider_age(config)? else {
        unreachable!()
    };
    let path = sealed::resolve_sealed_path(config, file.as_deref());
    let bytes =
        std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
    let identity = sealed::load_identity()?;
    let plaintext = sealed::unseal(&bytes, &identity)?;
    let parsed = sealed::parse_dotenv(&plaintext)?;
    for (k, v) in &parsed {
        if reveal {
            println!("{k}={v}");
        } else {
            println!("{k}={}", mask_value(v));
        }
    }
    Ok(())
}

fn cmd_secrets_seal(config: &Config, input: Option<&Path>, out: Option<PathBuf>) -> Result<()> {
    use yoink::sealed;
    let (file_override, recipients) = expect_age_block(config)?;
    let plaintext = match input {
        Some(p) if p.as_os_str() != "-" => {
            std::fs::read_to_string(p).with_context(|| format!("read input {}", p.display()))?
        }
        _ => {
            use std::io::Read;
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let parsed = sealed::parse_dotenv(&plaintext)?;
    let canonical = sealed::render_dotenv(&parsed);
    let sealed_bytes = sealed::seal(canonical.as_bytes(), recipients)?;
    let target =
        out.unwrap_or_else(|| sealed::resolve_sealed_path(config, file_override.as_deref()));
    sealed::write_atomically(&target, &sealed_bytes)?;
    println!("sealed {} key(s) to {}", parsed.len(), target.display());
    Ok(())
}

fn cmd_secrets_rotate(config: &Config) -> Result<()> {
    use yoink::sealed;

    let (file_override, current_recipients) = expect_age_block(config)?;
    let path = sealed::resolve_sealed_path(config, file_override.as_deref());
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "{} doesn't exist — nothing to rotate. `yoink secrets edit` to create it first",
            path.display()
        ));
    }

    // Decrypt with the current identity before generating the new key.
    let bytes =
        std::fs::read(&path).with_context(|| format!("read sealed file {}", path.display()))?;
    let identity = sealed::load_identity()?;
    let plaintext = sealed::unseal(&bytes, &identity)?;
    let parsed = sealed::parse_dotenv(&plaintext)?;
    let canonical = sealed::render_dotenv(&parsed);

    // Generate the new identity. The secret never lands on disk.
    let (new_secret, new_public) = sealed::keygen();

    // Re-seal under [current recipients ∪ new_public].
    let mut next: Vec<String> = current_recipients.clone();
    if !next.iter().any(|r| r == &new_public) {
        next.push(new_public.clone());
    }
    let resealed = sealed::seal(canonical.as_bytes(), &next)?;
    sealed::write_atomically(&path, &resealed)?;

    println!(
        "re-sealed {} key(s) to {} ({} recipients)",
        parsed.len(),
        path.display(),
        next.len()
    );
    println!();
    println!("New CI identity (paste into GitHub Actions secret YOINK_AGE_KEY):");
    println!();
    println!("{new_secret}");
    println!();
    println!("New public recipient (add to yoink.yaml under `secrets.recipients:`):");
    println!();
    println!("  - {new_public}");
    println!();
    println!("Next steps:");
    println!("  1. Add the new recipient to yoink.yaml so future edits include it.");
    println!("  2. Update the YOINK_AGE_KEY GitHub secret to the value above.");
    println!("  3. Once CI is happily decrypting with the new key, remove the OLD");
    println!("     recipient from yoink.yaml and run `yoink secrets edit` (save");
    println!("     without changes) to drop it from the sealed file.");
    Ok(())
}

fn expect_secrets_provider_age(config: &Config) -> Result<&yoink::config::SecretsConfig> {
    use yoink::config::SecretsConfig;
    match config.secrets.as_ref() {
        Some(s @ SecretsConfig::Age { .. }) => Ok(s),
        Some(SecretsConfig::Infisical { .. }) => Err(anyhow::anyhow!(
            "yoink.yaml configures `provider: infisical` — `yoink secrets` only manages age-sealed files"
        )),
        None => Err(anyhow::anyhow!(
            "no `secrets:` block in yoink.yaml — add `secrets: {{ provider: age, recipients: [...] }}` first (see `yoink secrets keygen`)"
        )),
    }
}

fn expect_age_block(config: &Config) -> Result<(Option<String>, &Vec<String>)> {
    use yoink::config::SecretsConfig;
    let SecretsConfig::Age { file, recipients } = expect_secrets_provider_age(config)? else {
        unreachable!()
    };
    if recipients.is_empty() {
        return Err(anyhow::anyhow!(
            "no `secrets.recipients:` configured — add at least one age public key (`age1...`) to yoink.yaml"
        ));
    }
    Ok((file.clone(), recipients))
}

fn open_in_editor(initial: &str) -> Result<String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    // `$EDITOR` commonly carries flags (`code --wait`, `nvim --noplugin`,
    // `emacsclient -nw`). Treat the whole string as a shell-style cmd
    // by splitting on ASCII whitespace; the first token is the program
    // and the rest are forwarded as args before the scratch path.
    let mut tokens = editor.split_ascii_whitespace();
    let program = tokens
        .next()
        .ok_or_else(|| anyhow::anyhow!("EDITOR/VISUAL is empty"))?;
    let editor_args: Vec<&str> = tokens.collect();

    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let path = dir.join(format!("yoink-secrets-{pid}.env"));

    // Open the scratch file with mode 0600 from creation — never let
    // the plaintext sit on disk world-readable, even briefly. On
    // non-Unix the regular create path applies (no perm model).
    write_scratch_file(&path, initial.as_bytes())
        .with_context(|| format!("create scratch file {}", path.display()))?;

    let status = std::process::Command::new(program)
        .args(&editor_args)
        .arg(&path)
        .status()
        .with_context(|| format!("launch editor {editor:?}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(anyhow::anyhow!(
            "editor {editor:?} exited with {status} — aborting"
        ));
    }

    let edited = std::fs::read_to_string(&path)
        .with_context(|| format!("read edited file {}", path.display()))?;
    let _ = std::fs::remove_file(&path);
    Ok(edited)
}

/// Create the scratch file for the editor flow with mode 0600 from
/// creation on Unix (no chmod-after-write window). `O_EXCL` rejects
/// pre-existing files — defends against a symlink-in-/tmp attack
/// pointing yoink at a privileged path. Stale scratch files from a
/// crashed previous run are removed first.
fn write_scratch_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    // A previous yoink crash could leave the file around — same PID
    // collisions are vanishingly unlikely but `create_new` would
    // refuse, so clear first. Removing a symlink an attacker planted
    // is fine: the subsequent `create_new` proves we own the inode.
    let _ = std::fs::remove_file(path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create scratch file {}", path.display()))?;
        f.write_all(contents)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

fn mask_value(s: &str) -> String {
    let visible: usize = if s.len() <= 4 { 0 } else { 2 };
    let prefix: String = s.chars().take(visible).collect();
    let masked = "•".repeat(s.chars().count().saturating_sub(visible).min(16));
    format!("{prefix}{masked}")
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("unix={secs}")
}
