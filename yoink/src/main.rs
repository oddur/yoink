use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::EnvFilter;

use yoink::config::Config;
use yoink::deploy::{self, DeployEvent};
use yoink::docker_ops::{DockerOps, Host, RealDockerOps};
use yoink::git;
use yoink::output;
use yoink::secrets::InfisicalToken;
use yoink::status::StatusReport;
use yoink::tui::{self, Mode};

#[derive(Parser)]
#[command(
    name = "yoink",
    version,
    about = "Small, opinionated container deploy CLI."
)]
struct Cli {
    /// Path to the yoink.toml config file.
    #[arg(short, long, default_value = "yoink.toml", global = true)]
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
    /// Deploy a new image tag.
    Deploy {
        /// Override the tag. Defaults to the current short git SHA.
        #[arg(long)]
        tag: Option<String>,

        /// Allow deploys with a dirty working tree.
        #[arg(long)]
        allow_dirty: bool,
    },
    /// Show what's running where.
    Status,
    /// Re-deploy the previous version.
    Rollback,
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
        Command::Deploy { tag, allow_dirty } => {
            cmd_deploy(&config, tag.as_deref(), allow_dirty).await
        }
        Command::Status => cmd_status(&config).await,
        Command::Rollback => cmd_rollback(&config).await,
        Command::Tui { mode } => cmd_tui(&config, mode).await,
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

async fn cmd_deploy(config: &Config, tag: Option<&str>, allow_dirty: bool) -> Result<()> {
    let version = match tag {
        Some(t) => t.to_string(),
        None => git::version(std::path::Path::new("."), allow_dirty)
            .context("resolve version from git")?,
    };
    run_deploy(config, &version).await
}

async fn cmd_status(config: &Config) -> Result<()> {
    let ops = RealDockerOps::new();
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status")?;
    println!("{}", output::format_status_table(&report));
    Ok(())
}

async fn cmd_rollback(config: &Config) -> Result<()> {
    let ops = RealDockerOps::new();
    let report = StatusReport::collect(&ops, config)
        .await
        .context("collect status for rollback")?;

    let current = report
        .hosts
        .iter()
        .flat_map(|h| h.containers.iter())
        .find(|c| c.is_running())
        .and_then(|c| c.yoink_version.clone())
        .ok_or_else(|| anyhow::anyhow!("no running container found to roll back from"))?;

    let previous = report
        .previous_version(&current)
        .ok_or_else(|| anyhow::anyhow!("no previous version found to roll back to"))?;

    eprintln!("yoink: rolling back from {current} to {previous}");
    run_deploy(config, &previous).await
}

async fn run_deploy(config: &Config, version: &str) -> Result<()> {
    let ops = RealDockerOps::new();
    let token = load_secrets_token(config)?;

    let stderr = io::stderr();
    let mut handle = stderr.lock();
    let mut sink = |event: DeployEvent| {
        let _ = writeln!(handle, "{}", output::format_deploy_event(&event));
    };

    let report = deploy::deploy(&ops, config, version, token.as_ref(), &mut sink)
        .await
        .context("deploy")?;
    println!("{}", output::format_deploy_summary(&report));
    Ok(())
}

fn load_secrets_token(config: &Config) -> Result<Option<InfisicalToken>> {
    if config.secrets.is_some() {
        Ok(Some(
            InfisicalToken::from_env().context("load INFISICAL_TOKEN")?,
        ))
    } else {
        Ok(None)
    }
}

async fn cmd_tui(config: &Config, mode: Mode) -> Result<()> {
    let ops: std::sync::Arc<dyn yoink::docker_ops::DockerOps> =
        std::sync::Arc::new(RealDockerOps::new());
    tui::run(config, ops, mode).await.context("run TUI")
}
