use std::io::{self, Write};
use std::path::PathBuf;
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
        /// Print the planned actions (per-host: image to pull, what
        /// would be swapped/created/removed) without executing.
        #[arg(long)]
        dry_run: bool,
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
    Version {
        service: String,
    },
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
    let config = Config::load_from_path(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Preflight => cmd_preflight(&config).await,
        Command::Up {
            services,
            tag,
            allow_dirty,
            dry_run,
        } => cmd_up(&config, &services, &tag, allow_dirty, dry_run).await,
        Command::Status { json } => cmd_status(&config, json).await,
        Command::Rollback { service, tag } => cmd_rollback(&config, service, tag).await,
        Command::Prune { dry_run } => cmd_prune(&config, dry_run).await,
        Command::Exec {
            service,
            host,
            cmd,
        } => cmd_exec(&config, &service, host.as_deref(), cmd).await,
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
        Command::Restart { service, host } => {
            cmd_restart(&config, &service, host.as_deref()).await
        }
        Command::Kill {
            service,
            host,
            yes,
        } => cmd_kill(&config, &service, host.as_deref(), yes).await,
        Command::Pull {
            service,
            tag,
            host,
        } => cmd_pull(&config, &service, tag.as_deref(), host.as_deref()).await,
        Command::History { service, limit } => cmd_history(&config, &service, limit).await,
        Command::Top { limit } => cmd_top(&config, limit).await,
        Command::Networks { host } => cmd_networks(&config, host.as_deref()).await,
        Command::Volumes { host } => cmd_volumes(&config, host.as_deref()).await,
        Command::Dump { log_tail } => cmd_dump(&config, log_tail).await,
        Command::Validate { check_hosts } => cmd_validate(&config, check_hosts).await,
        Command::Lock { action } => cmd_lock(&config, action).await,
        Command::Diff { service, tag } => cmd_diff(&config, &service, tag.as_deref()).await,
        Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
        Command::Tui { mode, mouse } => cmd_tui(&config, cli.config.clone(), mode, mouse).await,
    }
}

async fn cmd_preflight(config: &Config) -> Result<()> {
    let ops = RealDockerOps::new();
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

async fn cmd_up(
    config: &Config,
    services: &[String],
    tag_args: &[String],
    allow_dirty: bool,
    dry_run: bool,
) -> Result<()> {
    use yoink::docker_ops::Host;
    use yoink::lock::HostLock;

    // Wrap in Arc so the heartbeat tasks (one per host lock) can hold
    // their own clone for the duration of the deploy.
    let ops: std::sync::Arc<dyn DockerOps> = std::sync::Arc::new(RealDockerOps::new());
    let bundle = load_secrets_bundle(config).await?;
    let tag_overrides = parse_tag_overrides(tag_args, services, allow_dirty)?;

    let services_filter = if services.is_empty() {
        None
    } else {
        Some(services)
    };

    if dry_run {
        return print_dry_run_plan(config, &tag_overrides, services_filter);
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

    let mut sink = |event: deploy::DeployEvent| {
        eprintln!("{}", output::format_deploy_event(&event));
    };

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

async fn cmd_status(config: &Config, json: bool) -> Result<()> {
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();

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
    cmd_up(config, services_arg, tag_args, true, false).await
}

async fn cmd_prune(config: &Config, dry_run: bool) -> Result<()> {
    use yoink::prune::{self, PruneReason};
    let ops = RealDockerOps::new();
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
    let Some(cfg) = &config.secrets else {
        return Ok(None);
    };
    let bundle = secrets::fetch_secrets(cfg, cfg.domain.as_deref())
        .await
        .context("fetch secrets via infisical CLI")?;
    Ok(Some(bundle))
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
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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

    let ops: std::sync::Arc<dyn DockerOps> = std::sync::Arc::new(RealDockerOps::new());
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
                "if command -v bash >/dev/null 2>&1; then exec bash; else exec /bin/sh; fi"
                    .into(),
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
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut sig) = signal(SignalKind::window_change()) else {
            return;
        };
        while sig.recv().await.is_some() {
            let Ok((cols, rows)) = crossterm::terminal::size() else {
                continue;
            };
            let res = match session_kind {
                ExecKind::Exec => resize_ops.resize_exec(&resize_host, &resize_id, rows, cols).await,
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
        std::sync::Arc::new(RealDockerOps::new());
    tui::run(&config, config_path, ops, mode, mouse)
        .await
        .context("run TUI")
}

fn print_dry_run_plan(
    config: &Config,
    tag_overrides: &std::collections::BTreeMap<String, String>,
    services_filter: Option<&[String]>,
) -> Result<()> {
    println!("yoink up — dry run (no changes will be made)\n");
    println!("hosts:");
    for h in &config.hosts {
        println!("  - {}@{}", h.user, h.address);
    }
    println!("\nservices that would be reconciled:");
    let mut any = false;
    for svc in &config.services {
        if let Some(filter) = services_filter
            && !filter.iter().any(|s| s == &svc.name)
        {
            continue;
        }
        any = true;
        let tag = tag_overrides
            .get(&svc.name)
            .cloned()
            .or_else(|| svc.tag.clone())
            .unwrap_or_else(|| "<git>".into());
        println!("  - {}: {}:{}", svc.name, svc.image, tag);
    }
    if !any {
        anyhow::bail!("no services match the --service filter");
    }
    println!(
        "\nrun without --dry-run to actually pull, swap, and run hooks. \
         Existing containers whose spec matches will be left running."
    );
    Ok(())
}

async fn cmd_restart(
    config: &Config,
    service: &str,
    host_filter: Option<&str>,
) -> Result<()> {
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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
                eprintln!("→ {}: pulling {image}:{tag}", host.address);
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
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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
    rows.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));

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
    let ops = RealDockerOps::new();
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
    let ops = RealDockerOps::new();
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
    "TOKEN", "SECRET", "PASSWORD", "PASS", "API_KEY", "PRIVATE_KEY", "DSN",
];

fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_KEY_HINTS.iter().any(|h| upper.contains(h))
}

#[allow(clippy::too_many_lines)]
async fn cmd_dump(config: &Config, log_tail: u32) -> Result<()> {
    use serde_json::json;
    use yoink::deploy;
    use yoink::docker;
    use yoink::lock::LOCK_NAME;

    let ops = std::sync::Arc::new(RealDockerOps::new()) as std::sync::Arc<dyn DockerOps>;
    // Best-effort secrets load — drift hashes are accurate when it
    // succeeds, marked "?" otherwise. Failure is logged via tracing
    // (silenced inside dump output).
    let secrets = yoink::secrets::load_bundle(config).await.ok().flatten();

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
        host_obj["networks"] = serde_json::to_value(
            ops.list_networks(&host).await.unwrap_or_default(),
        )
        .unwrap_or(serde_json::Value::Null);
        host_obj["volumes"] = serde_json::to_value(
            ops.list_volumes(&host).await.unwrap_or_default(),
        )
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
                let tag = svc
                    .tag
                    .clone()
                    .or_else(|| c.yoink_version.clone())?;
                let desired = deploy::build_desired_spec(config, svc, &tag, secrets.as_ref())
                    .ok()?;
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
                            json!({"key": k, "value": v})
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
            "secrets_provider": config.secrets.as_ref().map(|s| s.provider.clone()),
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
    if check_hosts {
        cmd_preflight(config).await?;
    }
    Ok(())
}

async fn cmd_lock(config: &Config, action: LockAction) -> Result<()> {
    use yoink::lock::LOCK_NAME;
    let ops = RealDockerOps::new();
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

    let ops = RealDockerOps::new();
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
        "\nspec: {}:{} (env, ports, mounts compared at deploy time via spec hash)",
        svc_cfg.image, target_tag
    );
    Ok(())
}

fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut io::stdout());
}
