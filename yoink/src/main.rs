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
    /// Launch the interactive ratatui dashboard.
    Tui {
        /// Initial mode to open.
        #[arg(long, value_enum, default_value_t = Mode::Dashboard)]
        mode: Mode,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("yoink: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8) {
    let default_level = match verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.to_string()));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
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
        } => cmd_up(&config, &services, &tag, allow_dirty).await,
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
        Command::Tui { mode } => cmd_tui(&config, cli.config.clone(), mode).await,
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

    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let mut sink = |event: deploy::DeployEvent| {
        let _ = writeln!(handle, "{}", output::format_deploy_event(&event));
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
    cmd_up(config, services_arg, tag_args, true).await
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

async fn cmd_tui(config: &Config, config_path: PathBuf, mode: Mode) -> Result<()> {
    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(RealDockerOps::new());
    tui::run(config, config_path, ops, mode)
        .await
        .context("run TUI")
}
