//! TUI entry point. Async event loop driven by tokio + crossterm
//! `EventStream`. Three top-level views accessible via shortcut keys
//! and two drill-down views reachable via Enter from a parent:
//!
//! - `Dashboard` (`d`) — status grid for every yoink-managed service.
//! - `Hosts` (`h`) — all configured hosts; up/down + enter opens host detail.
//! - `HostDetail` — running containers on the host; up/down + enter opens container logs.
//! - `ContainerLogs` — live `docker logs -f` for one container.
//! - `Logs` (`l`) — multiplexed logs for every yoink-managed container.
//!
//! Refreshes (which talk to remote Docker over ssh) run on background
//! tokio tasks; the event loop only awaits the channel that delivers
//! their results, so input always feels responsive even when a refresh
//! is in flight.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::warn;

use crate::config::Config;
use crate::docker_ops::{DockerEvent, DockerEventKind, DockerOps, Host, LogLine};
use crate::status::StatusReport;

use super::container_detail::{self, ContainerDetailRefresh, ContainerDetailState, StatsHistory};
use super::dashboard::{self, DashboardRefresh, DashboardState};
use super::host_detail::{self, HostDetailRefresh, HostDetailState};
use super::hosts::{self, HostRow, HostsState};
use super::logs::{LogsState, RenderedLine};
use super::resources::{self, ResourceTab, ResourceTarget, ResourcesRefresh, ResourcesState};
use super::services::{ServiceDetailState, ServicesState};
use super::shell::{SessionResult, ShellState};

/// Backstop polling cadence when no `docker events` push lands. The
/// realtime updates ride on the events stream (see `subscribe_host_events`);
/// this tick exists so the UI converges even if events are filtered out
/// or the stream drops.
const FAST_TICK: Duration = Duration::from_secs(2);
/// How long a docker-event toast lingers in the breadcrumb's
/// right-side info slot before fading.
const TOAST_TTL: Duration = Duration::from_secs(5);
/// Cap on toast ring buffer — older events fall off.
const TOAST_CAP: usize = 8;
const HOSTS_TICK: Duration = Duration::from_secs(10);
/// How often to refresh the Resources pane (images / volumes / networks)
/// while it's visible. Slower than the dashboard tick: this data
/// changes far less often (image pulls, volume creates) and the
/// per-host fan-out is the heaviest list query in the codebase.
const RESOURCES_TICK: Duration = Duration::from_secs(15);
/// Per-host docker-events ring buffer cap. Each event is a single
/// formatted line — generous so an operator returning to a `HostDetail`
/// pane after lunch sees recent context.
const EVENT_HISTORY_PER_HOST: usize = 200;
/// Cadence of the always-on stats-history poller (one task per host).
/// Matches `FAST_TICK` — by the time the operator opens a container
/// detail pane, the chart has whatever back-history the poller managed
/// to collect at this rate. 2s is the same cadence dashboard uses, so
/// the docker daemon load is comparable.
const STATS_HISTORY_TICK: Duration = Duration::from_secs(2);
/// How often to re-stat + re-parse the on-disk config so a `vim
/// services/api.yaml` is reflected without restarting the TUI. Polling
/// (vs notify/inotify) keeps the dep tree small; 2s latency is fine
/// for an operator editing a YAML file.
const CONFIG_RELOAD_TICK: Duration = Duration::from_secs(2);
const LOG_BACKFILL_LINES: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Mode {
    Dashboard,
    Hosts,
    Services,
    Logs,
    Resources,
    Secrets,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Hosts,
    HostDetail(Host),
    Services,
    ServiceDetail(String),
    /// Per-service deploy history (every yoink-managed container with
    /// `yoink.service=<name>` across every host, sorted newest-first).
    /// `r` on a row triggers the reconcile-confirm modal pinned to
    /// that row's tag — same flow as `yoink rollback --tag <value>`.
    ServiceHistory(String),
    Logs,
    ContainerLogs {
        host: Host,
        container: String,
    },
    /// Container detail: labels, state, version, live CPU/mem.
    ContainerDetail {
        host: Host,
        container: String,
    },
    /// Embedded PTY shell for one container — k9s-style "drop into the
    /// container" without leaving the TUI.
    ContainerShell {
        host: Host,
        container: String,
        /// True when this is a debug sidecar (alpine container in the
        /// target's pid+net namespaces) rather than an in-container
        /// `docker exec`. Used for distroless / shell-less images.
        debug: bool,
    },
    /// Sealed-secrets management — view / add / edit / remove
    /// individual KEY=value entries without leaving the TUI.
    Secrets,
    /// Per-host introspection of docker resources beyond yoink-managed
    /// containers — Images / Volumes / Networks tabs. Reaches lazydocker
    /// feature parity for browsing local docker state.
    Resources,
}

impl View {
    /// Index into the top-level tab bar (Dashboard / Hosts / Services
    /// / Logs) that this view belongs to.
    #[must_use]
    pub fn top_section(&self) -> usize {
        match self {
            View::Dashboard => 0,
            View::Hosts
            | View::HostDetail(_)
            | View::ContainerLogs { .. }
            | View::ContainerShell { .. }
            | View::ContainerDetail { .. } => 1,
            View::Services | View::ServiceDetail(_) | View::ServiceHistory(_) => 2,
            View::Logs => 3,
            View::Resources => 4,
            View::Secrets => 5,
        }
    }

    /// Lines for the `?` help overlay. Per-view so the operator only
    /// sees the keybinds that actually do something here.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn help_lines(&self) -> Vec<&'static str> {
        let global = vec![
            "global",
            "  q / Ctrl-C    quit yoink",
            "  d h s l       dashboard / hosts / services / logs",
            "  R e           resources / encrypted-secrets",
            "  D             doctor — diagnose deploy-blockers",
            "  E             edit config in $EDITOR (jumps to focused service/host)",
            "  ~             show drift detail for the focused service",
            "  f             port-forward the focused service (auto: published or sidecar)",
            "  o / O         open the active port-forward URL in the browser (any view)",
            "  F             close every active port-forward",
            "  v             open VS Code in browser, rooted in the focused service's container",
            "  V             close every active vscode session",
            "  Tab / S-Tab   cycle modes forward / backward",
            "  ?             toggle this help overlay",
            "",
        ];
        let view_specific: Vec<&'static str> = match self {
            View::Dashboard => vec![
                "dashboard",
                "  ↑↓ / j k     select row",
                "  enter        container detail",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  A            reconcile ALL services (with confirmation)",
                "  P            prune stale + orphan containers (with confirmation)",
                "  /            filter substring · esc to clear",
                "  r            refresh",
                "  x            toggle eXited containers",
            ],
            View::Hosts => vec![
                "hosts",
                "  ↑↓ / j k     select host",
                "  enter        host detail",
                "  /            filter substring · esc to clear",
                "  r            refresh",
            ],
            View::HostDetail(_) => vec![
                "host detail",
                "  ↑↓ / j k     select container",
                "  enter        live logs",
                "  i            container detail (labels, env, live cpu/mem)",
                "  !            shell into container (bash/sh)",
                "  B            debug sidecar (alpine, target's pid+net ns)",
                "  S            start · X stop · R restart container",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  /            filter substring · esc to clear",
                "  r            refresh",
                "  esc          back to hosts (when no active filter)",
                "",
                "events panel (bottom of pane) auto-collects start /",
                "die / health-change events as they fire on the daemon",
            ],
            View::ContainerDetail { .. } => vec![
                "container detail",
                "  enter / l    live logs",
                "  !            shell · B debug sidecar",
                "  S            start · X stop · R restart container",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  p            processes (docker top)",
                "  r            refresh",
                "  esc          back to host detail",
                "",
                "5-minute history charts (collected for every container",
                "across every host, even when not viewing them):",
                "  cpu%   ·   mem   ·   net (tx ↑ above / rx ↓ below)",
                "current values appear in each chart title.",
            ],
            View::Services => vec![
                "services",
                "  ↑↓ / j k     select service",
                "  enter        service detail",
                "  /            filter substring · esc to clear",
                "  r            refresh",
            ],
            View::ServiceDetail(_) => vec![
                "service detail",
                "  ↑↓ / j k     select replica",
                "  enter        live logs",
                "  i            container detail",
                "  !            shell · B debug sidecar",
                "  H            deploy history (with rollback)",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  /            filter substring · esc to clear",
                "  r            refresh · esc back",
            ],
            View::ServiceHistory(_) => vec![
                "service history",
                "  ↑↓ / j k     select past deploy",
                "  r            rollback to selected (with confirmation)",
                "  R            refresh",
                "  esc          back to service detail",
            ],
            View::Logs => vec![
                "logs (multiplexed)",
                "  /            filter substring",
                "  ↑↓ / PgUp PgDn  scroll · g top · G bottom",
                "  y            yank visible buffer to system clipboard",
                "  k            clear · r restart streams",
            ],
            View::ContainerLogs { .. } => vec![
                "container logs",
                "  /            filter substring",
                "  ↑↓ / PgUp PgDn  scroll · g top · G bottom",
                "  !            shell · B debug sidecar",
                "  y            yank visible buffer to system clipboard",
                "  k            clear · esc back",
            ],
            View::Secrets => vec![
                "secrets",
                "  ↑↓ / j k     select key",
                "  /            filter substring",
                "  r            reveal/mask values",
                "  a            add a new secret (age provider only)",
                "  e            edit selected value",
                "  d            delete selected (with confirmation)",
                "  esc          back",
            ],
            View::Resources => vec![
                "resources (images / volumes / networks)",
                "  Tab / S-Tab  cycle Images → Volumes → Networks",
                "  ↑↓ / j k     select row",
                "  d            remove selected (with confirmation)",
                "  P            prune unused (with confirmation)",
                "  A            (Images only) prune ALL unused, not just dangling",
                "  /            filter substring · esc to clear",
                "  r            refresh",
                "",
                "rows fan out across every configured host. dangling",
                "images sort first; partial fetch errors per host appear",
                "in red at the bottom rather than blanking the table.",
            ],
            View::ContainerShell { .. } => vec![
                "shell",
                "  Ctrl-Q       exit shell, back to host detail",
                "  Ctrl-C/D     forwarded into the in-shell process",
                "  exit / Ctrl-D end the in-container shell",
            ],
        };
        let mut out = global;
        out.extend(view_specific);
        out
    }

    /// Path of crumbs for the global breadcrumb header — e.g.
    /// `["yoink", "Hosts", "my-server", "api-xyz", "shell"]`.
    /// Rendered with `›` separators by the App so the operator always
    /// knows where they are without reading the pane title.
    #[must_use]
    pub fn breadcrumb(&self) -> Vec<String> {
        let root = "yoink".to_string();
        match self {
            View::Dashboard => vec![root, "Dashboard".into()],
            View::Hosts => vec![root, "Hosts".into()],
            View::HostDetail(h) => vec![root, "Hosts".into(), h.address.clone()],
            View::Services => vec![root, "Services".into()],
            View::ServiceDetail(name) => vec![root, "Services".into(), name.clone()],
            View::ServiceHistory(name) => {
                vec![root, "Services".into(), name.clone(), "history".into()]
            }
            View::Logs => vec![root, "Logs".into()],
            View::ContainerLogs { host, container } => vec![
                root,
                "Hosts".into(),
                host.address.clone(),
                container.clone(),
                "logs".into(),
            ],
            View::ContainerShell {
                host,
                container,
                debug,
            } => vec![
                root,
                "Hosts".into(),
                host.address.clone(),
                container.clone(),
                if *debug { "debug shell" } else { "shell" }.into(),
            ],
            View::ContainerDetail { host, container } => vec![
                root,
                "Hosts".into(),
                host.address.clone(),
                container.clone(),
                "detail".into(),
            ],
            View::Secrets => vec![root, "Secrets".into()],
            View::Resources => vec![root, "Resources".into()],
        }
    }
}

impl From<Mode> for View {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Dashboard => View::Dashboard,
            Mode::Hosts => View::Hosts,
            Mode::Services => View::Services,
            Mode::Logs => View::Logs,
            Mode::Secrets => View::Secrets,
            Mode::Resources => View::Resources,
        }
    }
}

/// Result of a background refresh. The variant tells `App::apply_update`
/// which pane to swap in.
enum Update {
    Hosts(Vec<HostRow>),
    HostDetail {
        host: Host,
        data: HostDetailRefresh,
    },
    ContainerDetail {
        host: Host,
        container: String,
        data: Box<ContainerDetailRefresh>,
    },
    Dashboard(DashboardRefresh),
    /// Result of `schedule_history_refresh` — per-service container
    /// list across every host, drives the History pane's table.
    /// `errors` carries per-host failure strings so the pane can
    /// surface them instead of silently dropping unreachable hosts.
    ServiceHistory {
        service: String,
        rows: Vec<super::history::HistoryRow>,
        errors: Vec<String>,
    },
    /// Push notification from a host's `docker events` stream. Triggers
    /// an immediate refresh of whichever pane is currently visible.
    Event {
        host: Host,
        event: DockerEvent,
    },
    /// Background task wants to surface a one-line message in the
    /// toast ring (top-right of the breadcrumb). Used for silent
    /// failures the operator otherwise wouldn't see — log streams
    /// dying, event subscriptions failing, secrets loader blowing up.
    Toast(String),
    /// Result of `schedule_resources_refresh` — drives the new
    /// Resources pane.
    Resources(ResourcesRefresh),
    /// Result of a `docker top` background fetch. The receiver
    /// pushes the table onto `ContainerDetailState` (or surfaces an
    /// error toast on the failure path).
    Top {
        host: Host,
        container: String,
        result: Result<crate::docker_ops::ProcessTable, String>,
    },
    /// Result of a drift-modal fetch — the matching `ServiceDiff`
    /// for `(host, service)` produced by `diff::compute`.
    Drift {
        host: Host,
        service: String,
        result: super::drift::DriftRefresh,
    },
    /// Result of a doctor modal load — the full set of findings from
    /// `crate::doctor::run_doctor`. The runner always returns a vec
    /// (errors surface as `Severity::Error` findings, not as a
    /// run-level failure), so no Result wrapper is needed.
    Doctor(Vec<crate::doctor::Finding>),
    /// Batch of `(host_address, container_name, stats)` samples produced
    /// by the always-on background stats poller. Each sample is folded
    /// into the per-container `StatsHistory` so that opening a
    /// container's detail pane shows the rolling 5-minute history
    /// regardless of which view the operator was on while it was being
    /// collected.
    StatsBatch(Vec<(String, String, crate::docker_ops::ContainerStats)>),
    /// Background `f`-key port-forward succeeded — registers the
    /// `SshTunnel` (and optional `SidecarHandle` for the non-published
    /// path) into the App's `forwards` map. Both must land in
    /// main-loop state because dropping them on the background task
    /// would kill the ssh child / force-remove the sidecar
    /// immediately.
    PortForwardOpened {
        host: Host,
        service: String,
        endpoint: crate::pf::PublishedEndpoint,
        local_port: u16,
        url: String,
        tunnel: crate::transport::tunnel::SshTunnel,
        /// `Some` when the resolution went through the sidecar
        /// path. The handle's Drop force-removes the alpine/socat
        /// container.
        sidecar: Option<crate::pf::SidecarHandle>,
        /// Specific replica the operator was on when they pressed
        /// `f`. `None` for CLI invocations or views that don't have
        /// a focused container — the row marker falls back to per-
        /// service in that case.
        target_container: Option<String>,
    },
    /// Background `v`-key vscode session succeeded — registers the
    /// `SshTunnel` + `SidecarHandle` into the App's `vscode` state
    /// so they outlive the spawning task.
    VscodeOpened {
        host: String,
        service: String,
        url: String,
        tunnel: crate::transport::tunnel::SshTunnel,
        sidecar: crate::pf::SidecarHandle,
    },
}

/// Auto-pop error overlay carrying the full text (including URLs
/// that wouldn't fit in the one-line footer). Title goes in the
/// modal border; body is the multi-line message.
struct ErrorModal {
    title: String,
    lines: Vec<String>,
}

/// Background-task → run-loop messages. Events stream into the
/// progress modal as they fire; `Done` flips the modal into a
/// dismissible "finished" state with the final ✓/✗ summary.
enum JobUpdate {
    Event(String),
    /// Per-service event from a wave-parallel reconcile. Drives the
    /// status table at the top of the progress modal — the operator
    /// gets a quick "where are all 6 services right now?" view in
    /// addition to the interleaved scrolling log.
    ServiceEvent(String, crate::deploy::DeployEvent),
    Done(Result<String, String>),
}

/// Which long-running job is feeding the progress modal. Drives
/// the modal title and the "X failed" wording in the done line.
enum JobKind {
    Reconcile {
        service: String,
        tag: String,
    },
    ReconcileAll {
        statuses: std::collections::BTreeMap<String, ReconcileServiceStatus>,
    },
    Prune,
}

/// One-shot container lifecycle action — start / stop / restart.
/// Routed through `App::spawn_lifecycle`; the docker-events stream
/// drives the corresponding UI refresh, so there's no progress modal.
#[derive(Debug, Clone, Copy)]
enum LifecycleOp {
    Start,
    Stop,
    Restart,
}

impl LifecycleOp {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// Per-service state machine for the reconcile-all status table.
/// Driven by `DeployEvent`s — see `update_service_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReconcileServiceStatus {
    Pending,
    Pulling,
    Pulled,
    Healthchecking { container: String },
    Swapping,
    Synced,
    Done,
    Failed(String),
}

impl ReconcileServiceStatus {
    fn label(&self) -> String {
        match self {
            Self::Pending => "· waiting".into(),
            Self::Pulling => "⏵ pulling".into(),
            Self::Pulled => "⏵ pulled".into(),
            Self::Healthchecking { .. } => "⏵ healthcheck".into(),
            Self::Swapping => "⏵ swapping old container".into(),
            Self::Synced => "✓ synced (no-op)".into(),
            Self::Done => "✓ done".into(),
            Self::Failed(why) => format!("✗ failed: {why}"),
        }
    }

    fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            Self::Pending => Color::Gray,
            Self::Pulling | Self::Pulled => Color::Cyan,
            Self::Healthchecking { .. } | Self::Swapping => Color::Yellow,
            Self::Synced | Self::Done => Color::Green,
            Self::Failed(_) => Color::Red,
        }
    }
}

/// Live state for the job progress modal (reconcile or prune).
/// `lines` is the streamed event log (capped so a hung job doesn't
/// grow unbounded). `finished` is `None` while the task is still
/// running; once it's `Some`, the operator can press Esc to dismiss.
struct JobProgress {
    kind: JobKind,
    lines: std::collections::VecDeque<String>,
    finished: Option<Result<String, String>>,
}

const JOB_LINE_CAP: usize = 200;

pub async fn run(
    config: &Config,
    config_path: std::path::PathBuf,
    ops: Arc<dyn DockerOps>,
    mode: Mode,
    mouse: bool,
) -> Result<()> {
    let hl_disabled = std::env::var_os("YOINK_NO_HL").is_some();
    let hl_available = !hl_disabled && probe_hl().await;
    tracing::info!(
        hl_disabled,
        hl_available,
        "log forwarder pipeline initialized"
    );
    let mut terminal = setup_terminal(mouse).with_context(|| {
        format!(
            "initialize TUI (TERM={})",
            std::env::var("TERM").unwrap_or_else(|_| "<unset>".into())
        )
    })?;
    // Restore the terminal on panic before chaining to the previous
    // hook — without this a panic in render unwinds past
    // `restore_terminal` and leaves the operator stuck in raw+alt mode.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal_raw(mouse);
        prev_hook(info);
    }));
    let result = run_loop(
        &mut terminal,
        Arc::new(config.clone()),
        config_path,
        ops,
        mode,
        hl_available,
        mouse,
    )
    .await;
    let restore = restore_terminal(&mut terminal, mouse);
    let _ = std::panic::take_hook();
    result.and(restore)
}

/// Restore from a panic context — operates on raw stdout because we
/// don't own the `Terminal<Backend>` here. Skips the cursor-show step
/// of the normal-exit `restore_terminal` (not needed for usability).
fn restore_terminal_raw(mouse: bool) -> std::io::Result<()> {
    let mut stdout = io::stdout();
    if mouse {
        let _ = execute!(stdout, DisableMouseCapture);
    }
    let _ = disable_raw_mode();
    execute!(stdout, LeaveAlternateScreen)?;
    Ok(())
}

async fn probe_hl() -> bool {
    let res = tokio::process::Command::new("hl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    matches!(res, Ok(s) if s.success())
}

fn setup_terminal(mouse: bool) -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    if mouse {
        // Mouse capture disables native terminal text-selection — only
        // turn it on when the operator explicitly opts in via --mouse.
        execute!(stdout, EnableMouseCapture).context("enable mouse capture")?;
    }
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("init terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>, mouse: bool) -> Result<()> {
    if mouse {
        let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    }
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: Arc<Config>,
    config_path: std::path::PathBuf,
    ops: Arc<dyn DockerOps>,
    mode: Mode,
    hl_available: bool,
    mouse: bool,
) -> Result<()> {
    let mut app = App::new(config, config_path, ops, View::from(mode), hl_available);
    // Schedule the first round of background fetches so each pane has data
    // by the time the user navigates to it.
    app.schedule_hosts_refresh();
    app.schedule_dashboard_refresh();
    app.start_event_subscriptions();
    app.start_stats_history_pollers();
    app.spawn_secrets_loader();
    if matches!(app.view, View::Logs) {
        app.start_service_log_streams().await;
    }

    let mut events = EventStream::new();
    let mut fast_tick = interval(FAST_TICK);
    let mut hosts_tick = interval(HOSTS_TICK);
    let mut resources_tick = interval(RESOURCES_TICK);
    let mut config_tick = interval(CONFIG_RELOAD_TICK);
    // Drives the "starting shell…" spinner animation. Fires only
    // matters when the shell view is up and `inner` is None; the
    // branch is gated so it's a no-op otherwise.
    let mut shell_spin_tick = interval(Duration::from_millis(125));
    // 10fps spinner animation; cheap because ratatui's diff renderer
    // only writes changed cells, but the redraw rate is the floor.
    let mut throbber_tick = interval(Duration::from_millis(100));
    fast_tick.tick().await;
    hosts_tick.tick().await;
    resources_tick.tick().await;
    config_tick.tick().await;
    shell_spin_tick.tick().await;
    throbber_tick.tick().await;

    loop {
        terminal.draw(|f| app.render(f))?;
        tokio::select! {
            biased;
            input = events.next() => {
                match input {
                    Some(Ok(Event::Key(key))) => {
                        if app.on_key(key).await {
                            // Quit. Drain port-forwards explicitly
                            // so each sidecar's docker remove
                            // completes before the runtime tears
                            // down (the spawn-from-Drop fallback
                            // races with shutdown). Synchronous
                            // SshTunnel children get killed by
                            // their own Drop on App teardown.
                            app.forwards.close_all_async().await;
                            app.vscode.close_all_async().await;
                            return Ok(());
                        }
                        if let Some(target) = app.take_pending_editor() {
                            // Run the editor synchronously — the operator
                            // is *editing*; nothing else should fire while
                            // the alt-screen is torn down.
                            restore_terminal(terminal, mouse)
                                .context("restore terminal for $EDITOR")?;
                            let status = super::editor::run_editor(&target);
                            *terminal = setup_terminal(mouse)
                                .context("re-setup terminal after $EDITOR")?;
                            terminal.clear().context("clear terminal after $EDITOR")?;
                            match status {
                                Ok(s) if !s.success() => app.push_toast(format!(
                                    "✗ editor exited with status {s}"
                                )),
                                Err(e) => app.push_toast(format!("✗ editor failed: {e}")),
                                _ => {}
                            }
                            // Pick up edits immediately rather than
                            // waiting for the next reload tick.
                            app.maybe_reload_config();
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            _ = fast_tick.tick() => {
                app.fast_tick();
            }
            _ = hosts_tick.tick() => {
                // Only refresh while the Hosts pane is visible — the
                // refresh fans out into N+1 ssh calls per configured host
                // and there's no point doing that for a hidden pane.
                // Re-entry to Hosts schedules a fresh refresh anyway.
                if matches!(app.view, View::Hosts) {
                    app.schedule_hosts_refresh();
                }
            }
            _ = resources_tick.tick() => {
                // Same lazy guard as `hosts_tick`: the Resources pane's
                // 3-way fan-out per host is the heaviest list query,
                // so only fire while it's actually visible.
                if matches!(app.view, View::Resources) {
                    app.schedule_resources_refresh();
                }
            }
            _ = config_tick.tick() => {
                app.maybe_reload_config();
            }
            _ = throbber_tick.tick() => {
                app.tick_throbber();
            }
            Some(update) = app.update_rx.recv() => {
                app.apply_update(update);
            }
            Some(line) = app.log_rx.recv() => {
                app.logs.push_rendered(line);
            }
            Some(bytes) = app.shell_bytes_rx.recv() => {
                if let Some(shell) = app.shell.as_mut() {
                    shell.process_bytes(&bytes);
                }
            }
            Some((origin_view, res, banner)) = app.shell_session_rx.recv() => {
                app.apply_shell_session(&origin_view, res, &banner);
            }
            Some(update) = app.job_rx.recv() => {
                app.apply_job_update(update);
            }
            _ = shell_spin_tick.tick() => {
                // Tick exists purely to wake the loop so the spinner
                // animates while the shell is still spinning up.
                // Every loop iteration redraws, so the no-op tick does
                // its job just by being a select arm.
            }
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent in-flight flags + capability flag.
pub struct App {
    view: View,
    config: Arc<Config>,
    ops: Arc<dyn DockerOps>,
    pub dashboard: DashboardState,
    pub hosts: HostsState,
    pub host_detail: HostDetailState,
    pub services: ServicesState,
    pub service_detail: ServiceDetailState,
    pub history: super::history::HistoryState,
    pub container_detail: ContainerDetailState,
    pub logs: LogsState,
    pub resources: ResourcesState,
    pub secrets_state: super::secrets::SecretsState,
    /// `Some` while a `ContainerShell` view is active; cleared on exit.
    shell: Option<ShellState>,
    /// `?` toggles a modal help overlay listing keybinds for the
    /// current view. Cleared on Esc and on any view transition.
    show_help: bool,
    /// Auto-pop modal for the first occurrence of a long error
    /// string that doesn't fit in the footer (e.g. an ssh-probe
    /// failure carrying a Tailscale auth URL). Dismissed with Esc;
    /// fingerprint is added to `shown_errors` so the same error
    /// won't re-pop on every refresh.
    error_modal: Option<ErrorModal>,
    shown_errors: std::collections::HashSet<String>,
    /// `Some` while a kill-confirmation modal is open over the
    /// current view. The user confirms with `y` (or Enter) and
    /// cancels with anything else. Cleared on transition.
    kill_target: Option<(Host, String)>,
    /// Drift inspection modal — populated by pressing `~` on a
    /// drifted row. The fetch runs in the background and lands via
    /// `Update::Drift`; the modal is rendered on top of whatever
    /// view was active.
    drift: super::drift::DriftState,
    /// Doctor modal — same overlay shape as drift. Opens with `D`,
    /// runs `crate::doctor::run_doctor` async, lands findings via
    /// `Update::Doctor`. `r` rerun; Esc closes.
    doctor: super::doctor::DoctorState,
    /// Active port-forward tunnels keyed by `(host, service,
    /// container_port)`. Created by `f`, closed by `Shift-F` (all)
    /// or by dropping the App. Each entry owns its `SshTunnel`
    /// child; Drop kills the ssh subprocess on session exit.
    forwards: super::pf::PortForwardState,
    vscode: super::vscode::VscodeSessionState,
    /// `Some((service, tag))` while a reconcile-confirmation modal
    /// is open. `y` / Enter confirms; anything else cancels.
    reconcile_target: Option<(String, String)>,
    /// `true` while a prune-confirmation modal is open. `y` / Enter
    /// confirms; anything else cancels.
    prune_target: bool,
    /// `true` while a reconcile-all confirmation modal is open.
    /// Equivalent of `yoink up` (no `--service` filter): every service
    /// in config order across every host. `y` / Enter confirms.
    reconcile_all_target: bool,
    /// `Some` while a resource-remove confirmation modal is open
    /// over the Resources pane. Confirm with `y` / Enter; anything
    /// else dismisses.
    resource_remove_target: Option<ResourceTarget>,
    /// `Some((tab, dangling_only))` while a resource-prune modal is
    /// open. `dangling_only=false` is the aggressive "docker image
    /// prune -a" form; ignored for non-image tabs.
    resource_prune_target: Option<(ResourceTab, bool)>,
    /// `Some` while a reconcile is in flight or its progress modal
    /// is still on screen. Owns the streaming event log; cleared
    /// when the operator presses Esc after completion.
    job_progress: Option<JobProgress>,
    /// Channel for the spawned reconcile task to push progress and
    /// completion back to the run loop.
    job_tx: UnboundedSender<JobUpdate>,
    job_rx: UnboundedReceiver<JobUpdate>,
    /// Cached secrets bundle for drift detection in the Dashboard
    /// pane. Populated lazily by a background task at startup so the
    /// TUI doesn't block on a slow `provider: command` fetch (an
    /// external CLI can take 1–3 s). `None` means "not loaded yet" —
    /// drift cells render as `?` until the loader finishes.
    secrets: Arc<tokio::sync::RwLock<Option<Arc<crate::secrets::SecretsBundle>>>>,
    /// Ring of recent docker-event toasts: `(deadline, line)`. The
    /// most-recent line displaces the right-side host/service count
    /// in the breadcrumb header for `TOAST_TTL`.
    toasts: std::collections::VecDeque<(std::time::Instant, String)>,
    /// Byte chunks from the embedded shell's exec output stream. The
    /// run loop selects on this so each chunk wakes the render
    /// immediately — without it the only drain point would be the 2 s
    /// fast tick, which is way too slow for an interactive terminal.
    shell_bytes_tx: UnboundedSender<Vec<u8>>,
    shell_bytes_rx: UnboundedReceiver<Vec<u8>>,
    /// Result channel for the background "spin up the shell" task.
    /// Bollard's create+attach+start round-trip can take a few seconds
    /// (image pull, network setup) — running it on the event loop
    /// freezes the TUI, so we spawn it and route the `ExecSession`
    /// back through here.
    shell_session_tx: UnboundedSender<(View, SessionResult, String)>,
    shell_session_rx: UnboundedReceiver<(View, SessionResult, String)>,

    // Refresh-in-flight flags coalesce ticks: a tick that fires while the
    // previous refresh hasn't finished is dropped, so a slow daemon can
    // never queue up a backlog.
    hosts_in_flight: bool,
    host_detail_in_flight: bool,
    dashboard_in_flight: bool,
    container_detail_in_flight: bool,
    history_in_flight: bool,
    resources_in_flight: bool,
    /// Per-host docker-event ring buffer (formatted for display).
    /// Newest entries pushed at the back. Drives the events panel
    /// at the bottom of the `HostDetail` pane.
    host_events: std::collections::HashMap<String, std::collections::VecDeque<String>>,
    /// Per-container stats history, populated by the always-on
    /// background stats poller. Keyed by `(host_address,
    /// container_name)` so opening any container's detail pane
    /// renders the rolling 5-minute history immediately rather than
    /// starting from zero. Last-update tracking is implicit in each
    /// `StatsHistory`'s elapsed-time anchor; entries are GC'd when
    /// their newest sample falls outside `2 × HISTORY_WINDOW_SECS`.
    container_history:
        std::collections::HashMap<(String, String), super::container_detail::StatsHistory>,
    /// Per-host `JoinHandle`s for the always-on stats poller. Aborted
    /// on Drop and re-spawned on `hosts:` config reload.
    stats_history_tasks: Vec<JoinHandle<()>>,
    update_tx: UnboundedSender<Update>,
    update_rx: UnboundedReceiver<Update>,

    log_tasks: Vec<JoinHandle<()>>,
    log_tx: UnboundedSender<RenderedLine>,
    log_rx: UnboundedReceiver<RenderedLine>,
    /// One per host. Subscribed at startup; aborted on Drop. Events
    /// flow into `update_tx` and trigger an immediate refresh of
    /// whichever pane is visible.
    event_tasks: Vec<JoinHandle<()>>,
    /// Path to the root `yoink.yaml`. Re-read every `CONFIG_RELOAD_TICK`
    /// so on-disk edits flow into the running TUI.
    config_path: std::path::PathBuf,
    /// Cached source-of-truth label for the chrome (e.g. "yoink(*)") —
    /// shell-outs to `git` happen on the config-reload tick, not every
    /// render. `None` = not in a git repo, or git isn't installed.
    config_source: Option<String>,
    /// Set when the operator pressed `E` to edit the config in
    /// `$EDITOR`. Drained by `run_loop`, which suspends the alt-
    /// screen, runs the editor synchronously, and re-enters the TUI.
    pending_editor: Option<super::editor::EditorTarget>,
    /// Drives the spinner glyph in `loading…` rows. Advanced on a
    /// dedicated 100 ms tick so the animation looks alive even when
    /// the slower data ticks aren't firing.
    throbber_state: throbber_widgets_tui::ThrobberState,
    hl_available: bool,
}

impl App {
    pub fn new(
        config: Arc<Config>,
        config_path: std::path::PathBuf,
        ops: Arc<dyn DockerOps>,
        view: View,
        hl_available: bool,
    ) -> Self {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        let (shell_bytes_tx, shell_bytes_rx) = mpsc::unbounded_channel();
        let (shell_session_tx, shell_session_rx) = mpsc::unbounded_channel();
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        Self {
            view,
            config,
            ops,
            dashboard: DashboardState::new(),
            hosts: HostsState::new(),
            host_detail: HostDetailState::new(),
            services: ServicesState::new(),
            service_detail: ServiceDetailState::new(),
            history: super::history::HistoryState::new(),
            container_detail: ContainerDetailState::new(),
            logs: LogsState::new(),
            resources: ResourcesState::new(),
            secrets_state: super::secrets::SecretsState::new(),
            shell: None,
            show_help: false,
            error_modal: None,
            shown_errors: std::collections::HashSet::new(),
            kill_target: None,
            drift: super::drift::DriftState::default(),
            doctor: super::doctor::DoctorState::default(),
            forwards: super::pf::PortForwardState::default(),
            vscode: super::vscode::VscodeSessionState::default(),
            reconcile_target: None,
            prune_target: false,
            reconcile_all_target: false,
            resource_remove_target: None,
            resource_prune_target: None,
            job_progress: None,
            job_tx,
            job_rx,
            secrets: Arc::new(tokio::sync::RwLock::new(None)),
            toasts: std::collections::VecDeque::new(),
            shell_bytes_tx,
            shell_bytes_rx,
            shell_session_tx,
            shell_session_rx,
            hosts_in_flight: false,
            host_detail_in_flight: false,
            dashboard_in_flight: false,
            container_detail_in_flight: false,
            history_in_flight: false,
            resources_in_flight: false,
            host_events: std::collections::HashMap::new(),
            container_history: std::collections::HashMap::new(),
            stats_history_tasks: Vec::new(),
            update_tx,
            update_rx,
            log_tasks: Vec::new(),
            log_tx,
            log_rx,
            event_tasks: Vec::new(),
            config_source: super::chrome::config_source(&config_path),
            config_path,
            pending_editor: None,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            hl_available,
        }
    }

    /// Drained by `run_loop` after each key event. When `Some`, the
    /// outer loop suspends the TUI, runs `$EDITOR` on the target,
    /// and re-enters the alt-screen + raw mode.
    pub fn take_pending_editor(&mut self) -> Option<super::editor::EditorTarget> {
        self.pending_editor.take()
    }

    /// Advance the loading-spinner glyph one frame.
    pub fn tick_throbber(&mut self) {
        self.throbber_state.calc_next();
    }

    /// Build the editor jump target for the current view. Falls back
    /// to "open at line 1" when the focused object isn't a service or
    /// host (e.g. the dashboard or the secrets pane), or when the
    /// matching block can't be located in the root config file.
    fn editor_target_for_current_view(&self) -> super::editor::EditorTarget {
        let yaml = std::fs::read_to_string(&self.config_path).unwrap_or_default();
        let line = match &self.view {
            View::ServiceDetail(name) | View::ServiceHistory(name) => {
                super::editor::find_service_line(&yaml, name)
            }
            View::HostDetail(host) => super::editor::find_host_line(&yaml, &host.address),
            View::ContainerDetail { container, .. } | View::ContainerLogs { container, .. } => {
                // Look up the service this container belongs to: try
                // each declared service name as a `<name>-` prefix and
                // pick the longest match. Service names in the same
                // file are unique, so there's at most one valid hit.
                self.config
                    .services
                    .iter()
                    .filter(|s| {
                        container == s.name.as_str()
                            || container.starts_with(&format!("{}-", s.name))
                    })
                    .max_by_key(|s| s.name.len())
                    .and_then(|s| super::editor::find_service_line(&yaml, &s.name))
            }
            _ => None,
        };
        super::editor::EditorTarget {
            path: self.config_path.clone(),
            line,
        }
    }

    /// Resolve the focused `(host, service)` pair for the drift
    /// modal. Each list view exposes its selected row; the container
    /// detail view falls back to the inspected container's
    /// `yoink.service` label. `None` when the current view has no
    /// notion of a focused row (Logs / Resources / Secrets).
    fn drift_focus(&self) -> Option<(Host, String)> {
        let host_for = |addr: &str| -> Option<Host> {
            self.config
                .hosts
                .iter()
                .find(|h| h.address == addr)
                .map(Host::from)
        };
        match &self.view {
            View::Dashboard => {
                let row = self.dashboard.selected()?;
                let service = row.service?;
                host_for(&row.host).map(|h| (h, service))
            }
            View::HostDetail(host) => self
                .host_detail
                .selected_service()
                .map(|s| (host.clone(), s)),
            View::ServiceDetail(svc) => {
                let row = self.service_detail.selected_row()?;
                Some((row.host, svc.clone()))
            }
            View::ContainerDetail { host, .. } => {
                let labels = &self.container_detail.inspect()?.labels;
                labels
                    .get("yoink.service")
                    .cloned()
                    .map(|s| (host.clone(), s))
            }
            _ => None,
        }
    }

    /// Specific container the operator's row is on, when one
    /// applies. Used to scope the `↦` marker to the exact replica
    /// being forwarded instead of lighting up every replica of the
    /// service. Returns `None` for views that don't have a focused
    /// container (Services list, Hosts list, …); the resolver falls
    /// back to per-service semantics in that case.
    fn pf_focused_container(&self) -> Option<String> {
        match &self.view {
            View::Dashboard => self.dashboard.selected().map(|r| r.container),
            View::HostDetail(_) => self.host_detail.selected_container(),
            View::ContainerDetail { container, .. } => Some(container.clone()),
            _ => None,
        }
    }

    /// Resolve which service the operator's currently looking at,
    /// for the global `o` (open port-forward URL) gesture. Wider
    /// reach than `drift_focus` — covers the Services list (no
    /// host context) and unwraps `yoink-pf-*` sidecar containers
    /// back to their target service via the
    /// `yoink.pf.target_service` label. Returns `None` for views
    /// that don't have any service notion (Hosts list, Logs,
    /// Resources, Secrets); the `o` handler then falls back to
    /// "first active forward."
    fn pf_focused_service(&self) -> Option<String> {
        match &self.view {
            View::Dashboard => self.dashboard.selected().and_then(|r| r.service),
            View::HostDetail(_) => self.host_detail.selected_service(),
            View::ServiceDetail(svc) => Some(svc.clone()),
            View::Services => self.services.selected_service(),
            View::ContainerDetail { .. } => {
                let labels = &self.container_detail.inspect()?.labels;
                labels
                    .get("yoink.service")
                    .or_else(|| labels.get("yoink.pf.target_service"))
                    .cloned()
            }
            _ => None,
        }
    }

    /// Re-read the config from disk and swap it in if it parses + has
    /// actually changed. Silent no-op on parse errors so a half-saved
    /// edit doesn't blank the dashboard; the next tick will catch the
    /// finished edit. If `hosts:` changed we tear down and respawn the
    /// docker-events subscriptions. Also refreshes the chrome's git
    /// "from <repo>(*)" label, which piggybacks on this slow tick so
    /// `git status` doesn't run at render rate.
    fn maybe_reload_config(&mut self) {
        self.config_source = super::chrome::config_source(&self.config_path);
        let mut new_config = match Config::load_from_path(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                // Toast once per unique message so the operator sees
                // their typo; dedup keeps the ring quiet while the
                // file stays broken across many reload ticks.
                let msg = format!("✗ config reload failed: {e}");
                if !self
                    .toasts
                    .iter()
                    .any(|(_, line)| line.as_str() == msg.as_str())
                {
                    self.push_toast(msg);
                }
                tracing::debug!(error = %e, path = %self.config_path.display(), "config reload failed");
                return;
            }
        };
        // Re-apply the magical local-host injection — load_from_path
        // returns a fresh disk-shaped config that doesn't know about
        // it, so without this the synthetic `local` host disappears
        // every CONFIG_RELOAD_TICK.
        new_config.push_local_host_if_socket();
        // If the config has any `address_secret:` hosts, resolve them
        // against the cached secrets bundle. The TUI loads the bundle
        // asynchronously via the `secrets` Arc; until that bundle
        // lands, sealed-address hosts will render with empty
        // addresses — which is a strong visual signal that the bundle
        // hasn't loaded yet. Once the bundle is cached, every reload
        // tick re-applies the resolution and the hosts populate.
        if new_config.any_host_address_sealed()
            && let Some(bundle) = self.secrets.try_read().ok().and_then(|g| g.clone())
            && let Err(e) = new_config.resolve_host_addresses(Some(bundle.as_ref()))
        {
            tracing::debug!(error = %e, "tui: resolve_host_addresses failed");
        }
        if new_config == *self.config {
            return;
        }
        let hosts_changed = new_config.hosts != self.config.hosts;
        self.config = Arc::new(new_config);
        if hosts_changed {
            self.stop_event_subscriptions();
            self.start_event_subscriptions();
            self.stop_stats_history_pollers();
            self.start_stats_history_pollers();
            // Drop event rings for hosts no longer in the config so a
            // re-added address doesn't inherit stale events.
            let active: std::collections::HashSet<&str> = self
                .config
                .hosts
                .iter()
                .map(|h| h.address.as_str())
                .collect();
            self.host_events
                .retain(|addr, _| active.contains(addr.as_str()));
            self.schedule_hosts_refresh();
        }
        self.schedule_dashboard_refresh();
    }

    /// Copy the currently-visible log buffer (filtered, in render
    /// order) to the host terminal's clipboard via OSC-52. Toasts
    /// the result so the operator knows it worked. Bound to `y`
    /// from Logs / `ContainerLogs`.
    fn copy_logs_to_clipboard(&mut self) {
        let text = self.logs.copy_text();
        self.copy_text_to_clipboard(&text, "logs");
    }

    fn copy_text_to_clipboard(&mut self, text: &str, label: &str) {
        if text.is_empty() {
            self.push_toast(format!("nothing to copy ({label} buffer empty)"));
            return;
        }
        match super::ui::copy_to_clipboard(text) {
            Ok(n) => self.push_toast(format!("✓ copied {label} ({n} bytes)")),
            Err(e) => self.push_toast(format!("✗ copy failed: {e}")),
        }
    }

    /// Push a toast onto the ring with the standard TTL — used by
    /// the various copy / one-off action paths.
    fn push_toast(&mut self, line: String) {
        self.toasts
            .push_back((std::time::Instant::now() + TOAST_TTL, line));
        while self.toasts.len() > TOAST_CAP {
            self.toasts.pop_front();
        }
    }

    /// Show an auto-pop error modal — once per unique error. Used
    /// when an error string is too long to fit in the one-line
    /// footer (Tailscale auth URL, multi-line ssh-probe output,
    /// etc.). Operator dismisses with Esc; the same error won't
    /// re-pop on subsequent refresh ticks.
    ///
    /// **Why first-line fingerprinting:** the first line is the
    /// classified hint (e.g. `ssh probe failed: Tailscale SSH
    /// requires an additional check — open this URL...`), which
    /// stays stable across token regenerations in the URL on
    /// subsequent lines. Hashing the full body would make every
    /// token rotation re-pop the same modal the operator just
    /// dismissed.
    fn show_error_modal_once(&mut self, title: &str, body: &str) {
        let first_line = body.lines().next().unwrap_or("").to_string();
        let fp = format!("{title}|{first_line}");
        if !self.shown_errors.insert(fp) {
            return;
        }
        let lines: Vec<String> = body.lines().map(str::to_string).collect();
        self.error_modal = Some(ErrorModal {
            title: title.to_string(),
            lines,
        });
    }

    /// Open the reconcile-confirm modal for `service`. Resolves the
    /// tag the same way the dashboard's drift cell does — config
    /// tag if present, else the running container's tag — so the
    /// confirm prompt shows what would actually deploy.
    fn open_reconcile_modal(&mut self, service: &str) {
        if self.job_progress.is_some() {
            return;
        }
        let Some(svc) = self.config.services.iter().find(|s| s.name == service) else {
            return;
        };
        let tag = svc.tag.clone().or_else(|| {
            self.dashboard
                .running_tag_for_service(service)
                .or_else(|| self.service_detail.running_tag_for_service(service))
        });
        let Some(tag) = tag else {
            return;
        };
        self.reconcile_target = Some((service.to_string(), tag));
    }

    fn confirm_reconcile(&mut self) {
        let Some((service, tag)) = self.reconcile_target.take() else {
            return;
        };
        self.job_progress = Some(JobProgress {
            kind: JobKind::Reconcile {
                service: service.clone(),
                tag: tag.clone(),
            },
            lines: std::collections::VecDeque::new(),
            finished: None,
        });
        let config = (*self.config).clone();
        let ops = self.ops.clone();
        let secrets = self.secrets.try_read().ok().and_then(|g| g.clone());
        let tx = self.job_tx.clone();
        tokio::spawn(reconcile_one(config, ops, secrets, service, tag, tx));
    }

    fn open_prune_modal(&mut self) {
        if self.job_progress.is_some() {
            return;
        }
        self.prune_target = true;
    }

    fn open_reconcile_all_modal(&mut self) {
        if self.job_progress.is_some() {
            return;
        }
        self.reconcile_all_target = true;
    }

    fn confirm_reconcile_all(&mut self) {
        if !self.reconcile_all_target {
            return;
        }
        self.reconcile_all_target = false;

        // Tag overrides: services without a config-pinned tag (api,
        // web) need a `--tag` override to deploy. Fall back to the
        // running container's tag — same heuristic as the per-service
        // reconcile modal — so this gesture is "redeploy what's
        // currently live, with the current config".
        let mut overrides: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for svc in &self.config.services {
            if svc.tag.is_some() {
                continue;
            }
            if let Some(tag) = self
                .dashboard
                .running_tag_for_service(&svc.name)
                .or_else(|| self.service_detail.running_tag_for_service(&svc.name))
            {
                overrides.insert(svc.name.clone(), tag);
            }
            // If still no tag, the deploy will fail loudly — caller
            // sees the per-service error in the streaming log.
        }

        // Seed the per-service status map: every configured service
        // starts as Pending; the wave loop flips them to Pulling /
        // Done / etc. as events arrive. Using BTreeMap keeps the
        // table render order stable (alphabetical by service name).
        let statuses = self
            .config
            .services
            .iter()
            .map(|s| (s.name.clone(), ReconcileServiceStatus::Pending))
            .collect();
        self.job_progress = Some(JobProgress {
            kind: JobKind::ReconcileAll { statuses },
            lines: std::collections::VecDeque::new(),
            finished: None,
        });
        let config = (*self.config).clone();
        let ops = self.ops.clone();
        let secrets = self.secrets.try_read().ok().and_then(|g| g.clone());
        let tx = self.job_tx.clone();
        tokio::spawn(reconcile_all(config, ops, secrets, overrides, tx));
    }

    fn confirm_prune(&mut self) {
        if !self.prune_target {
            return;
        }
        self.prune_target = false;
        self.job_progress = Some(JobProgress {
            kind: JobKind::Prune,
            lines: std::collections::VecDeque::new(),
            finished: None,
        });
        let config = (*self.config).clone();
        let ops = self.ops.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(prune_all(config, ops, tx));
    }

    fn apply_job_update(&mut self, update: JobUpdate) {
        let Some(progress) = self.job_progress.as_mut() else {
            return;
        };
        match update {
            JobUpdate::Event(line) => {
                // Some events (container log tail) are multi-line —
                // split so each render row is one line.
                for one in line.split('\n') {
                    progress.lines.push_back(one.to_string());
                    while progress.lines.len() > JOB_LINE_CAP {
                        progress.lines.pop_front();
                    }
                }
            }
            JobUpdate::ServiceEvent(name, event) => {
                if let JobKind::ReconcileAll { statuses } = &mut progress.kind {
                    update_service_status(statuses, &name, &event);
                }
            }
            JobUpdate::Done(result) => {
                let line = match &result {
                    Ok(s) => format!("✓ {s}"),
                    Err(e) => format!("✗ {e}"),
                };
                progress.lines.push_back(line);
                if result.is_err()
                    && let JobKind::ReconcileAll { statuses } = &mut progress.kind
                {
                    // Mark every still-running service as failed so
                    // the status table reflects the abort, not its
                    // last seen mid-flight state.
                    for status in statuses.values_mut() {
                        if !matches!(
                            status,
                            ReconcileServiceStatus::Done
                                | ReconcileServiceStatus::Synced
                                | ReconcileServiceStatus::Failed(_)
                        ) {
                            *status = ReconcileServiceStatus::Failed("aborted".into());
                        }
                    }
                }
                progress.finished = Some(result);
                self.schedule_dashboard_refresh();
            }
        }
    }

    /// Kick off the secrets bundle fetch in the background. The
    /// Dashboard drift column needs the bundle to compute the
    /// `spec_hash` that matches what `yoink up` would produce. We
    /// don't block startup on it — the column shows `?` for the few
    /// seconds the loader takes, then resolves to ✓/⚠ once the
    /// bundle lands.
    fn spawn_secrets_loader(&self) {
        let config = self.config.clone();
        let slot = self.secrets.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            match crate::secrets::load_bundle(&config).await {
                Ok(Some(bundle)) => {
                    *slot.write().await = Some(Arc::new(bundle));
                }
                Ok(None) => {
                    // No `[secrets]` block — nothing to load. Drift
                    // detection still works for services without
                    // secrets-derived env.
                }
                Err(e) => {
                    warn!(error = %e, "secrets load for drift detection failed; column will stay '?'");
                    let _ = tx.send(Update::Toast(format!(
                        "✗ secrets load failed: {e} (drift column → ?)"
                    )));
                }
            }
        });
    }

    /// Spawn one task per configured host that subscribes to the docker
    /// events stream and forwards each event into `update_tx`. Cheap —
    /// one connection per host, multiplexed across all panes.
    fn start_event_subscriptions(&mut self) {
        for host_cfg in &self.config.hosts {
            let host = Host::from(host_cfg);
            let ops = self.ops.clone();
            let tx = self.update_tx.clone();
            let task = tokio::spawn(async move {
                let mut rx = match ops.subscribe_events(&host).await {
                    Ok(rx) => rx,
                    Err(e) => {
                        warn!(host = %host.address, error = %e, "subscribe_events failed");
                        let _ = tx.send(Update::Toast(format!(
                            "✗ events stream {}: {e}",
                            host.address
                        )));
                        return;
                    }
                };
                while let Some(event) = rx.recv().await {
                    if tx
                        .send(Update::Event {
                            host: host.clone(),
                            event,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                // Stream ended without an explicit error — usually
                // means the host went away. Surface it.
                let _ = tx.send(Update::Toast(format!(
                    "✗ events stream {} ended (host probably unreachable)",
                    host.address
                )));
            });
            self.event_tasks.push(task);
        }
    }

    fn stop_event_subscriptions(&mut self) {
        for task in self.event_tasks.drain(..) {
            task.abort();
        }
    }

    /// Spawn one always-on stats-history poller per configured host.
    /// The poller lists running containers on its host every
    /// `STATS_HISTORY_TICK` and fetches `container_stats` for each
    /// one in parallel, then ships the batch back through `update_tx`
    /// as `Update::StatsBatch`. This keeps the rolling 5-minute
    /// window alive for every container regardless of which view
    /// the operator is currently on, so opening a container detail
    /// pane shows immediate context instead of an empty chart.
    ///
    /// Idempotent on re-spawn: callers must invoke
    /// `stop_stats_history_pollers` first (we do that on `hosts:`
    /// config-reload before respawning, and on Drop).
    fn start_stats_history_pollers(&mut self) {
        for host_cfg in &self.config.hosts {
            let host = Host::from(host_cfg);
            let ops = self.ops.clone();
            let tx = self.update_tx.clone();
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(STATS_HISTORY_TICK);
                // First tick fires immediately; eat it so we don't
                // hammer the daemon during App startup when nothing's
                // visible yet.
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let containers = match ops.list_running_containers(&host).await {
                        Ok(cs) => cs,
                        Err(e) => {
                            tracing::debug!(
                                host = %host.address,
                                error = %e,
                                "stats poller: list_running_containers failed",
                            );
                            continue;
                        }
                    };
                    if containers.is_empty() {
                        // Still send an empty batch so the receiver
                        // gets a heartbeat (useful for future GC
                        // strategies that key off "we polled at T").
                        if tx.send(Update::StatsBatch(Vec::new())).is_err() {
                            return;
                        }
                        continue;
                    }
                    let stat_futs = containers.iter().map(|c| {
                        let ops = ops.clone();
                        let host = host.clone();
                        let name = c.name.clone();
                        async move {
                            let res = ops.container_stats(&host, &name).await.ok();
                            (host.address.clone(), name, res)
                        }
                    });
                    let results = futures_util::future::join_all(stat_futs).await;
                    let batch: Vec<(String, String, crate::docker_ops::ContainerStats)> = results
                        .into_iter()
                        .filter_map(|(h, n, s)| s.map(|stats| (h, n, stats)))
                        .collect();
                    if tx.send(Update::StatsBatch(batch)).is_err() {
                        return;
                    }
                }
            });
            self.stats_history_tasks.push(task);
        }
    }

    fn stop_stats_history_pollers(&mut self) {
        for task in self.stats_history_tasks.drain(..) {
            task.abort();
        }
    }

    /// Push one stats sample into the per-container history store.
    /// Cheap — each container gets its own bounded ring buffer.
    fn record_stats_sample(
        &mut self,
        host: &str,
        container: &str,
        stats: &crate::docker_ops::ContainerStats,
    ) {
        let entry = self
            .container_history
            .entry((host.to_string(), container.to_string()))
            .or_default();
        entry.push(stats);
    }

    /// Drop history entries whose newest sample is older than
    /// `2 × HISTORY_WINDOW_SECS`. Called from the slow ticks so a
    /// long-running TUI doesn't slowly accumulate ghost containers.
    fn gc_container_history(&mut self) {
        // The `StatsHistory` window-trim logic already drops samples
        // that have aged out, so an entry whose ring is empty has
        // had no fresh sample for at least HISTORY_WINDOW_SECS — that's
        // the GC signal.
        self.container_history.retain(|_, h| !h.cpu_pct_is_empty());
    }

    /// Returns true when the loop should exit.
    #[allow(clippy::too_many_lines)]
    async fn on_key(&mut self, key: KeyEvent) -> bool {
        // Embedded shell: forward everything to the PTY. The only
        // yoink-side gestures are Ctrl-Q (exit shell) and the EOF
        // signal from the container side; both bounce us back to
        // host detail.
        if matches!(self.view, View::ContainerShell { .. }) {
            // `?` toggles the help overlay even from inside a shell —
            // it's the one yoink-side gesture (besides Ctrl-Q) the
            // shell view doesn't forward.
            if key.code == KeyCode::Char('?') {
                self.show_help = !self.show_help;
                return false;
            }
            if self.show_help && key.code == KeyCode::Esc {
                self.show_help = false;
                return false;
            }
            return self.handle_shell_key(key).await;
        }

        // Help overlay: `?` toggles, Esc dismisses. Captured before
        // any other key handling so the help can be opened/closed from
        // any non-shell view.
        if key.code == KeyCode::Char('?') {
            self.show_help = !self.show_help;
            return false;
        }
        if self.show_help && key.code == KeyCode::Esc {
            self.show_help = false;
            return false;
        }

        // Auto-pop error modal: Esc / Enter dismisses. Captured
        // before view-specific keys so the operator can't
        // accidentally drive the underlying view while the modal's
        // open. The fingerprint stays in `shown_errors`, so the
        // same error won't re-pop after dismissal.
        if self.error_modal.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.error_modal = None;
            }
            return false;
        }

        // Drift modal: Esc dismisses, anything else falls through so
        // the operator can keep typing into the underlying view (e.g.
        // press `~` again on a different focus, or navigate away).
        if self.drift.is_visible() && matches!(key.code, KeyCode::Esc) {
            self.drift.clear();
            return false;
        }

        // Doctor modal: capture keys while open so the underlying view
        // doesn't see them. Esc closes; r reruns; ↑↓ navigates the
        // findings list.
        if self.doctor.is_open() {
            match key.code {
                KeyCode::Esc => {
                    self.doctor.close();
                    return false;
                }
                KeyCode::Char('r') => {
                    self.open_doctor();
                    return false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.doctor.select_prev();
                    return false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.doctor.select_next();
                    return false;
                }
                _ => return false, // swallow everything else while open
            }
        }

        // Kill-confirmation modal: `y` / Enter confirms, anything else
        // dismisses. Captured before view-specific keys so a stray `j`
        // can't both dismiss and select-next.
        if let Some((host, container)) = self.kill_target.clone() {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            self.kill_target = None;
            if confirm {
                let ops = self.ops.clone();
                tokio::spawn(async move {
                    if let Err(e) = ops.kill_container(&host, &container).await {
                        warn!(host = %host.address, container, error = %e, "kill failed");
                    }
                });
            }
            return false;
        }

        // Reconcile-confirmation modal: same shape as kill.
        if self.reconcile_target.is_some() {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            if confirm {
                self.confirm_reconcile();
            } else {
                self.reconcile_target = None;
            }
            return false;
        }

        // Prune-confirmation modal.
        if self.prune_target {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            if confirm {
                self.confirm_prune();
            } else {
                self.prune_target = false;
            }
            return false;
        }

        // Reconcile-all confirmation modal.
        if self.reconcile_all_target {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            if confirm {
                self.confirm_reconcile_all();
            } else {
                self.reconcile_all_target = false;
            }
            return false;
        }

        // Resource remove confirmation modal — same gesture as kill.
        if self.resource_remove_target.is_some() {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            let target = self.resource_remove_target.take();
            if confirm && let Some(t) = target {
                self.spawn_resource_remove(t);
                self.schedule_resources_refresh();
            }
            return false;
        }

        // Resource prune confirmation modal.
        if self.resource_prune_target.is_some() {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            let target = self.resource_prune_target.take();
            if confirm && let Some((kind, dangling_only)) = target {
                self.spawn_resource_prune(kind, dangling_only);
                self.schedule_resources_refresh();
            }
            return false;
        }

        // Secrets remove-confirmation modal: same shape as kill.
        if self.secrets_state.confirming_remove() {
            let confirm = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
            if confirm {
                if let Some(key) = self.secrets_state.confirm_remove()
                    && self.secrets_state.apply_remove(key)
                {
                    self.spawn_secrets_loader();
                }
            } else {
                self.secrets_state.cancel_edit();
            }
            return false;
        }

        // Secrets add/edit input mode: capture all printable input.
        // Enter commits, Esc cancels. Captured before tab nav so
        // typing `s` (Services tab) doesn't fire while the operator
        // is in the middle of a value.
        if matches!(self.view, View::Secrets) && self.secrets_state.input_mode() {
            match key.code {
                KeyCode::Esc => self.secrets_state.cancel_edit(),
                KeyCode::Enter => {
                    if let Some(commit) = self.secrets_state.commit_input()
                        && self.secrets_state.apply_commit(commit)
                    {
                        self.spawn_secrets_loader();
                    }
                }
                KeyCode::Backspace => self.secrets_state.backspace(),
                KeyCode::Char(c) => self.secrets_state.push_char(c),
                _ => {}
            }
            return false;
        }

        // Reconcile-progress modal: while running, eat all keys
        // (Ctrl-C/Q already handled above) so a stray j/k can't
        // navigate the underlying view. `y` always works to yank
        // the current log to the clipboard. Once finished, Esc
        // dismisses; any other key is also eaten so the operator
        // doesn't accidentally fire something else off.
        if let Some(progress) = self.job_progress.as_ref() {
            if matches!(key.code, KeyCode::Char('y')) {
                let text: String = progress
                    .lines
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                self.copy_text_to_clipboard(&text, "reconcile log");
                return false;
            }
            if progress.finished.is_some() && matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.job_progress = None;
            }
            return false;
        }

        // Filter input mode in either logs view captures all printable
        // input — only Ctrl-C escapes to quit.
        let logs_view = matches!(self.view, View::Logs | View::ContainerLogs { .. });
        if logs_view && self.logs.input_mode() {
            return self.handle_filter_input_key(key);
        }

        // Filter input mode in selectable list/table panes — same
        // shape as the logs filter, but the buffer lives on each
        // pane's `FilterState`. Captured before navigation so the
        // user can type any character into the filter.
        if self.pane_filter_input_mode() {
            return self.handle_pane_filter_input_key(key);
        }

        if matches!(key.code, KeyCode::Char('q'))
            || (matches!(key.code, KeyCode::Char('c'))
                && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return true;
        }

        // `/` enters filter input mode for the current pane (when it
        // supports filtering — Logs handled separately above).
        if key.code == KeyCode::Char('/') {
            self.begin_pane_filter_input();
            return false;
        }
        // Esc clears an active filter in panes that support it.
        if key.code == KeyCode::Esc && self.pane_has_active_filter() {
            self.clear_pane_filter();
            return false;
        }
        match key.code {
            // Lowercase `d` is dashboard nav from anywhere EXCEPT the
            // Secrets and Resources panes — both bind `d` to delete the
            // selected item, and a delete key shouldn't surprise-route
            // to a pane switch when the operator's intent is "remove."
            KeyCode::Char('d') if !matches!(self.view, View::Secrets | View::Resources) => {
                self.transition(View::Dashboard).await;
                return false;
            }
            KeyCode::Char('h') => {
                self.transition(View::Hosts).await;
                return false;
            }
            KeyCode::Char('s') => {
                self.transition(View::Services).await;
                return false;
            }
            KeyCode::Char('l') => {
                self.transition(View::Logs).await;
                return false;
            }
            KeyCode::Char('e') => {
                self.transition(View::Secrets).await;
                return false;
            }
            KeyCode::Char('R') if !matches!(self.view, View::Resources) => {
                // Capital `R` enters the Resources pane from any other
                // top-level view. While *inside* Resources, we let the
                // per-view match below own the `R` key (lower-case is
                // already used for refresh; the per-view handler can
                // map capital R to other things if needed).
                self.transition(View::Resources).await;
                return false;
            }
            // Capital `D` opens the doctor modal — runs the same checks
            // `yoink doctor` does, on top of the current view.
            KeyCode::Char('D') => {
                self.open_doctor();
                return false;
            }
            // Capital `E` jumps into `$EDITOR` at the focused service /
            // host's line in the config (lowercase `e` is taken by
            // Secrets). The actual suspend + resume happens in
            // `run_loop` so the alt-screen plumbing stays in one place.
            KeyCode::Char('E') => {
                self.pending_editor = Some(self.editor_target_for_current_view());
                return false;
            }
            // `~` opens the drift modal for the focused (host, service).
            // Shows the same `+`/`-`/`~` per-field diff `yoink up --plan`
            // produces — without leaving the TUI. Esc closes.
            KeyCode::Char('~') => {
                if let Some((host, service)) = self.drift_focus() {
                    self.open_drift(host, service);
                }
                return false;
            }
            // `f` opens an SSH-tunnel port-forward to the focused
            // service. Same machinery as `yoink pf` on the CLI; resolves
            // the service's `publish:` block, picks the first matching
            // entry (or errors with a hint when there are 0/many), and
            // prints the URL into a toast. The tunnel lives until
            // `Shift-F` closes all of them or the TUI exits. `o` while
            // a tunnel exists for the focused service opens its URL in
            // the system browser.
            KeyCode::Char('f') => {
                if let Some((host, service)) = self.drift_focus() {
                    let target_container = self.pf_focused_container();
                    self.open_port_forward(host, service, target_container);
                }
                return false;
            }
            KeyCode::Char('F') => {
                let n = self.forwards.len();
                self.forwards.close_all_async().await;
                if n > 0 {
                    self.push_toast(format!("✓ closed {n} port-forward(s)"));
                }
                return false;
            }
            // `v` opens VS Code in the browser, rooted in the focused
            // service's container filesystem. Resources view binds `v`
            // to the Volumes sub-tab; we let that win there.
            KeyCode::Char('v') if !matches!(self.view, View::Resources) => {
                if let Some((host, service)) = self.drift_focus() {
                    self.open_vscode_session(host, service);
                }
                return false;
            }
            KeyCode::Char('V') => {
                let n = self.vscode.len();
                self.vscode.close_all_async().await;
                if n > 0 {
                    self.push_toast(format!("✓ closed {n} vscode session(s)"));
                }
                return false;
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                // 1. Try to resolve a focused service in the current view
                //    (Dashboard / Services / ServiceDetail / HostDetail /
                //    ContainerDetail). 2. Fall back to "any active forward"
                //    so the operator can press O on the Hosts pane and
                //    still pop the most-recently-opened tunnel — same
                //    "always available" feel as the footer band.
                let target_url = self
                    .pf_focused_service()
                    .and_then(|s| self.forwards.first_for_service(&s).map(|f| f.url.clone()))
                    .or_else(|| self.forwards.first().map(|f| f.url.clone()));
                let Some(url) = target_url else {
                    if !self.forwards.is_empty() {
                        self.push_toast("no port-forward URL resolved for this row".to_string());
                    }
                    return false;
                };
                if let Err(e) = crate::pf::open_in_browser(&url) {
                    self.push_toast(format!("✗ open browser failed: {e} — {url}"));
                } else {
                    self.push_toast(format!("→ opened {url}"));
                }
                return false;
            }
            // Tab / Shift-Tab cycle through the top-level modes
            // (k9s-friendly alternative to direct-letter access).
            // Inside Resources we override Tab to cycle sub-tabs
            // (Images / Volumes / Networks); that match arm runs
            // before the global one because we early-return below.
            KeyCode::Tab if matches!(self.view, View::Resources) => {
                self.resources.cycle_tab_forward();
                return false;
            }
            KeyCode::BackTab if matches!(self.view, View::Resources) => {
                self.resources.cycle_tab_backward();
                return false;
            }
            KeyCode::Tab => {
                let next = match self.view.top_section() {
                    0 => View::Hosts,
                    1 => View::Services,
                    2 => View::Logs,
                    3 => View::Resources,
                    4 => View::Secrets,
                    _ => View::Dashboard,
                };
                self.transition(next).await;
                return false;
            }
            KeyCode::BackTab => {
                let prev = match self.view.top_section() {
                    0 => View::Secrets,
                    1 => View::Dashboard,
                    2 => View::Hosts,
                    3 => View::Services,
                    4 => View::Logs,
                    _ => View::Resources,
                };
                self.transition(prev).await;
                return false;
            }
            _ => {}
        }
        match &self.view {
            View::Hosts => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.hosts.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.hosts.select_next(),
                KeyCode::Enter => {
                    if let Some(host) = self.hosts.selected_host() {
                        self.transition(View::HostDetail(host)).await;
                    }
                }
                KeyCode::Char('r') => self.schedule_hosts_refresh(),
                _ => {}
            },
            View::HostDetail(_) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.host_detail.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.host_detail.select_next(),
                KeyCode::Enter => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.transition(View::ContainerLogs { host, container })
                            .await;
                    }
                }
                KeyCode::Char('!') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.transition(View::ContainerShell {
                            host,
                            container,
                            debug: false,
                        })
                        .await;
                    }
                }
                KeyCode::Char('B') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.transition(View::ContainerShell {
                            host,
                            container,
                            debug: true,
                        })
                        .await;
                    }
                }
                KeyCode::Char('i') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.transition(View::ContainerDetail { host, container })
                            .await;
                    }
                }
                KeyCode::Char('K') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.kill_target = Some((host, container));
                    }
                }
                KeyCode::Char('S') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.spawn_lifecycle(host, container, LifecycleOp::Start);
                    }
                }
                KeyCode::Char('X') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.spawn_lifecycle(host, container, LifecycleOp::Stop);
                    }
                }
                KeyCode::Char('R') => {
                    if let (Some(host), Some(container)) = (
                        self.host_detail.host().cloned(),
                        self.host_detail.selected_container(),
                    ) {
                        self.spawn_lifecycle(host, container, LifecycleOp::Restart);
                    }
                }
                KeyCode::Char('U') => {
                    if let Some(service) = self.host_detail.selected_service() {
                        self.open_reconcile_modal(&service);
                    }
                }
                KeyCode::Esc => self.transition(View::Hosts).await,
                KeyCode::Char('r') => self.schedule_host_detail_refresh(),
                _ => {}
            },
            View::ContainerDetail { host, container } => match key.code {
                KeyCode::Esc if self.container_detail.top_visible() => {
                    self.container_detail.dismiss_top();
                }
                KeyCode::Char('p') => {
                    self.schedule_top_fetch();
                }
                KeyCode::Char('S') => {
                    self.spawn_lifecycle(host.clone(), container.clone(), LifecycleOp::Start);
                }
                KeyCode::Char('X') => {
                    self.spawn_lifecycle(host.clone(), container.clone(), LifecycleOp::Stop);
                }
                KeyCode::Char('R') => {
                    self.spawn_lifecycle(host.clone(), container.clone(), LifecycleOp::Restart);
                }
                KeyCode::Char('K') => {
                    self.kill_target = Some((host.clone(), container.clone()));
                }
                KeyCode::Char('U') => {
                    let svc = self.container_detail.target().and_then(|(_, name)| {
                        self.dashboard
                            .report_ref()
                            .and_then(|r| {
                                r.hosts
                                    .iter()
                                    .flat_map(|h| h.containers.iter())
                                    .find(|c| &c.name == name)
                            })
                            .and_then(|c| c.yoink_service.clone())
                    });
                    if let Some(name) = svc {
                        self.open_reconcile_modal(&name);
                    }
                }
                KeyCode::Esc => {
                    let host = host.clone();
                    self.transition(View::HostDetail(host)).await;
                }
                KeyCode::Char('l') | KeyCode::Enter => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerLogs { host, container })
                        .await;
                }
                KeyCode::Char('!') => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerShell {
                        host,
                        container,
                        debug: false,
                    })
                    .await;
                }
                KeyCode::Char('B') => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerShell {
                        host,
                        container,
                        debug: true,
                    })
                    .await;
                }
                KeyCode::Char('r') => self.schedule_container_detail_refresh(),
                _ => {}
            },
            View::ContainerLogs { host, container } => match key.code {
                KeyCode::Esc => {
                    let host = host.clone();
                    self.transition(View::HostDetail(host)).await;
                }
                KeyCode::Char('!') => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerShell {
                        host,
                        container,
                        debug: false,
                    })
                    .await;
                }
                KeyCode::Char('B') => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerShell {
                        host,
                        container,
                        debug: true,
                    })
                    .await;
                }
                KeyCode::Char('c') => self.logs.clear(),
                KeyCode::Char('y') => self.copy_logs_to_clipboard(),
                KeyCode::Char('/') => self.logs.begin_filter_input(),
                KeyCode::Up => self.logs.scroll_up(1),
                KeyCode::Down => self.logs.scroll_down(1),
                KeyCode::PageUp => self.logs.scroll_up(10),
                KeyCode::PageDown => self.logs.scroll_down(10),
                KeyCode::Char('g') => self.logs.jump_to_top(),
                KeyCode::Char('G') | KeyCode::End => self.logs.jump_to_bottom(),
                _ => {}
            },
            View::Dashboard => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.dashboard.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.dashboard.select_next(),
                KeyCode::Enter => {
                    if let Some(row) = self.dashboard.selected() {
                        let host = config_host(&self.config, &row.host).unwrap_or(Host {
                            user: String::new(),
                            address: row.host,
                        });
                        self.transition(View::ContainerDetail {
                            host,
                            container: row.container,
                        })
                        .await;
                    }
                }
                KeyCode::Char('K') => {
                    if let Some(row) = self.dashboard.selected() {
                        let host = config_host(&self.config, &row.host).unwrap_or(Host {
                            user: String::new(),
                            address: row.host,
                        });
                        self.kill_target = Some((host, row.container));
                    }
                }
                KeyCode::Char('U') => {
                    if let Some(row) = self.dashboard.selected()
                        && let Some(svc) = row.service
                    {
                        self.open_reconcile_modal(&svc);
                    }
                }
                KeyCode::Char('r') => self.schedule_dashboard_refresh(),
                KeyCode::Char('x') => self.dashboard.toggle_show_exited(),
                KeyCode::Char('P') => self.open_prune_modal(),
                KeyCode::Char('A') => self.open_reconcile_all_modal(),
                _ => {}
            },
            View::Services => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.services.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.services.select_next(),
                KeyCode::Enter => {
                    if let Some(name) = self.services.selected_service() {
                        self.transition(View::ServiceDetail(name)).await;
                    }
                }
                KeyCode::Char('r') => self.schedule_dashboard_refresh(),
                _ => {}
            },
            View::ServiceDetail(_) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.service_detail.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.service_detail.select_next(),
                KeyCode::Enter => {
                    if let Some(row) = self.service_detail.selected_row() {
                        self.transition(View::ContainerLogs {
                            host: row.host,
                            container: row.container.name,
                        })
                        .await;
                    }
                }
                KeyCode::Char('!') => {
                    if let Some(row) = self.service_detail.selected_row() {
                        self.transition(View::ContainerShell {
                            host: row.host,
                            container: row.container.name,
                            debug: false,
                        })
                        .await;
                    }
                }
                KeyCode::Char('B') => {
                    if let Some(row) = self.service_detail.selected_row() {
                        self.transition(View::ContainerShell {
                            host: row.host,
                            container: row.container.name,
                            debug: true,
                        })
                        .await;
                    }
                }
                KeyCode::Char('i') => {
                    if let Some(row) = self.service_detail.selected_row() {
                        self.transition(View::ContainerDetail {
                            host: row.host,
                            container: row.container.name,
                        })
                        .await;
                    }
                }
                KeyCode::Char('K') => {
                    if let Some(row) = self.service_detail.selected_row() {
                        self.kill_target = Some((row.host, row.container.name));
                    }
                }
                KeyCode::Char('U') => {
                    if let Some(name) = self.service_detail.current_service().map(str::to_string) {
                        self.open_reconcile_modal(&name);
                    }
                }
                KeyCode::Char('H') => {
                    if let Some(name) = self.service_detail.current_service().map(str::to_string) {
                        self.transition(View::ServiceHistory(name)).await;
                    }
                }
                KeyCode::Esc => self.transition(View::Services).await,
                KeyCode::Char('r') => self.schedule_dashboard_refresh(),
                _ => {}
            },
            View::ServiceHistory(name) => {
                let svc = name.clone();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.history.select_prev(),
                    KeyCode::Down | KeyCode::Char('j') => self.history.select_next(),
                    KeyCode::Char('r') => {
                        // Rollback the selected row by hijacking the
                        // existing reconcile-confirm flow with an
                        // explicit (service, tag) target.
                        if let Some((service, tag)) = self.history.selected_rollback_target() {
                            self.reconcile_target = Some((service, tag));
                        }
                    }
                    KeyCode::Esc => self.transition(View::ServiceDetail(svc)).await,
                    KeyCode::Char('R') => self.schedule_history_refresh(svc),
                    _ => {}
                }
            }
            View::ContainerShell { .. } => {} // handled above
            View::Logs => match key.code {
                KeyCode::Char('r') => {
                    self.stop_log_streams();
                    self.start_service_log_streams().await;
                }
                KeyCode::Char('y') => self.copy_logs_to_clipboard(),
                KeyCode::Char('c') => self.logs.clear(),
                KeyCode::Char('/') => self.logs.begin_filter_input(),
                KeyCode::Up => self.logs.scroll_up(1),
                KeyCode::Down => self.logs.scroll_down(1),
                KeyCode::PageUp => self.logs.scroll_up(10),
                KeyCode::PageDown => self.logs.scroll_down(10),
                KeyCode::Char('g') => self.logs.jump_to_top(),
                KeyCode::Char('G') | KeyCode::End => self.logs.jump_to_bottom(),
                _ => {}
            },
            View::Secrets => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.secrets_state.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.secrets_state.select_next(),
                KeyCode::Char('r') => self.secrets_state.toggle_reveal(),
                KeyCode::Char('a') => self.secrets_state.begin_add(),
                KeyCode::Char('e') | KeyCode::Enter => {
                    self.secrets_state.begin_edit_selected();
                }
                KeyCode::Char('d') => self.secrets_state.begin_remove_selected(),
                _ => {}
            },
            View::Resources => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.resources.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => self.resources.select_next(),
                KeyCode::Char('i') => self.resources.set_tab(ResourceTab::Images),
                KeyCode::Char('v') => self.resources.set_tab(ResourceTab::Volumes),
                KeyCode::Char('n') => self.resources.set_tab(ResourceTab::Networks),
                KeyCode::Char('d') => {
                    if let Some(target) = self.resources.selected_target(&self.config) {
                        self.resource_remove_target = Some(target);
                    }
                }
                KeyCode::Char('P') => {
                    self.resource_prune_target = Some((self.resources.current_tab(), true));
                }
                KeyCode::Char('A') if self.resources.current_tab() == ResourceTab::Images => {
                    // Aggressive image prune ("docker image prune -a")
                    // — only meaningful on the Images tab.
                    self.resource_prune_target = Some((ResourceTab::Images, false));
                }
                KeyCode::Char('r') => self.schedule_resources_refresh(),
                _ => {}
            },
        }
        false
    }

    /// Route mouse events. Currently only scroll wheel: maps to
    /// `select_prev`/`select_next` on selectable views, and scroll on the logs
    /// view. Click/drag are ignored (that would require tracking
    /// per-render Rects we don't currently emit).
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => match &self.view {
                View::Hosts => self.hosts.select_prev(),
                View::HostDetail(_) => self.host_detail.select_prev(),
                View::Services => self.services.select_prev(),
                View::ServiceDetail(_) => self.service_detail.select_prev(),
                View::Logs | View::ContainerLogs { .. } => self.logs.scroll_up(3),
                _ => {}
            },
            MouseEventKind::ScrollDown => match &self.view {
                View::Hosts => self.hosts.select_next(),
                View::HostDetail(_) => self.host_detail.select_next(),
                View::Services => self.services.select_next(),
                View::ServiceDetail(_) => self.service_detail.select_next(),
                View::Logs | View::ContainerLogs { .. } => self.logs.scroll_down(3),
                _ => {}
            },
            _ => {}
        }
    }

    /// Per-pane filter helpers — Logs has its own (older) filter that
    /// pre-dates the shared `FilterState`; everything else routes here.
    fn pane_filter(&mut self) -> Option<&mut super::ui::FilterState> {
        match &self.view {
            View::Dashboard => Some(&mut self.dashboard.filter),
            View::Hosts => Some(&mut self.hosts.filter),
            View::HostDetail(_) => Some(&mut self.host_detail.filter),
            View::Services => Some(&mut self.services.filter),
            View::ServiceDetail(_) => Some(&mut self.service_detail.filter),
            View::Secrets => Some(&mut self.secrets_state.filter),
            _ => None,
        }
    }

    fn pane_filter_input_mode(&mut self) -> bool {
        self.pane_filter().is_some_and(|f| f.input_mode())
    }

    fn pane_has_active_filter(&mut self) -> bool {
        self.pane_filter().is_some_and(|f| f.current().is_some())
    }

    fn begin_pane_filter_input(&mut self) {
        if let Some(f) = self.pane_filter() {
            f.begin_input();
        }
    }

    fn clear_pane_filter(&mut self) {
        if let Some(f) = self.pane_filter() {
            f.clear();
        }
    }

    fn handle_pane_filter_input_key(&mut self, key: KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if let Some(f) = self.pane_filter() {
            match key.code {
                KeyCode::Esc => f.cancel(),
                KeyCode::Enter => f.apply(),
                KeyCode::Backspace => f.backspace(),
                KeyCode::Char(c) => f.push_char(c),
                _ => {}
            }
        }
        false
    }

    fn handle_filter_input_key(&mut self, key: KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match key.code {
            KeyCode::Esc => self.logs.filter_cancel(),
            KeyCode::Enter => self.logs.filter_apply(),
            KeyCode::Backspace => self.logs.filter_backspace(),
            KeyCode::Char(c) => self.logs.filter_push_char(c),
            _ => {}
        }
        false
    }

    async fn transition(&mut self, new_view: View) {
        if self.view == new_view {
            return;
        }
        self.stop_log_streams();
        self.logs.clear();
        self.show_help = false;
        self.kill_target = None;
        self.reconcile_target = None;
        self.prune_target = false;
        self.reconcile_all_target = false;
        self.resource_remove_target = None;
        self.resource_prune_target = None;
        // Drop the `docker top` modal so a re-entry to ContainerDetail
        // doesn't resurrect a stale process list.
        self.container_detail.dismiss_top();
        // Don't clear job_progress on transition — operator
        // may want to navigate around with the deploy still in flight.
        // It clears itself on Esc-after-finished.
        // Always tear down any in-flight shell when leaving its view —
        // the spawned exec will drop its bridge task on Drop, and a
        // sidecar shell needs an explicit force-remove of its
        // alpine container so we don't leave debug containers running.
        if !matches!(new_view, View::ContainerShell { .. })
            && let Some(mut shell) = self.shell.take()
        {
            shell.cleanup(self.ops.clone());
        }

        match &new_view {
            View::HostDetail(host) => {
                self.host_detail.set_host(host.clone());
                self.schedule_host_detail_refresh();
            }
            View::ContainerLogs { host, container } => {
                self.spawn_log_forwarder(host.clone(), container.clone())
                    .await;
            }
            View::ContainerDetail { host, container } => {
                self.container_detail
                    .set_target(host.clone(), container.clone());
                self.schedule_container_detail_refresh();
                // Tail logs in the bottom panel using the same
                // forwarder ContainerLogs uses — backfill +
                // continuous stream.
                self.spawn_log_forwarder(host.clone(), container.clone())
                    .await;
            }
            View::ContainerShell {
                host,
                container,
                debug,
            } => {
                // Render the spinner immediately, then kick off the
                // bollard create+attach+start in the background. The
                // session lands via `shell_session_rx` and is wired
                // into the panel without blocking the event loop.
                let label = if *debug {
                    "spinning up debug sidecar (alpine)".to_string()
                } else {
                    "starting in-container shell".to_string()
                };
                let state = ShellState::new(host.clone(), container.clone(), label);
                let banner = if *debug {
                    state.debug_banner("alpine")
                } else {
                    state.exec_banner()
                };
                self.shell = Some(state);
                let key = new_view.clone();
                let tx = self.shell_session_tx.clone();
                let ops = self.ops.clone();
                let host = host.clone();
                let container = container.clone();
                let is_debug = *debug;
                tokio::spawn(async move {
                    let res = if is_debug {
                        ShellState::build_debug_future(
                            ops,
                            host,
                            container,
                            "alpine".into(),
                            24,
                            80,
                        )
                        .await
                    } else {
                        ShellState::build_exec_future(ops, host, container, 24, 80).await
                    };
                    let _ = tx.send((key, res, banner));
                });
            }
            View::Logs => self.start_service_log_streams().await,
            View::Dashboard | View::Services => self.schedule_dashboard_refresh(),
            View::Hosts => self.schedule_hosts_refresh(),
            View::ServiceDetail(name) => {
                self.service_detail.set_service(name.clone());
                self.schedule_dashboard_refresh();
            }
            View::ServiceHistory(name) => {
                self.history.set_service(name.clone());
                self.schedule_history_refresh(name.clone());
            }
            View::Secrets => {
                // Decrypt + populate happens lazily on first render
                // via `ensure_loaded`. Clear any half-finished edit
                // from a previous visit so the operator starts fresh.
                self.secrets_state.cancel_edit();
            }
            View::Resources => {
                self.schedule_resources_refresh();
            }
        }
        self.view = new_view;
    }

    /// Fan out across hosts and collect every container labeled
    /// `yoink.service=<name>` (running + exited). Posts an
    /// `Update::ServiceHistory` back to the run loop. Mirrors the CLI
    /// `cmd_history` query.
    fn schedule_history_refresh(&mut self, service: String) {
        if self.history_in_flight {
            return;
        }
        self.history_in_flight = true;
        let ops = self.ops.clone();
        let hosts: Vec<Host> = self.config.hosts.iter().map(Host::from).collect();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let label = format!("yoink.service={service}");
            // Fan out per-host fetches in parallel — sequential here
            // multiplies the ssh-probe timeout by the host count
            // (3 hosts down × 8s = 24s total before the operator
            // sees anything). `StatusReport::collect` already does
            // the same thing for the dashboard.
            let futs = hosts.into_iter().map(|host| {
                let ops = ops.clone();
                let label = label.clone();
                async move {
                    let result = ops.list_containers_by_label(&host, &label).await;
                    (host, result)
                }
            });
            let results = futures_util::future::join_all(futs).await;
            let mut rows: Vec<super::history::HistoryRow> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            for (host, result) in results {
                match result {
                    Ok(containers) => {
                        for c in containers {
                            rows.push(super::history::HistoryRow::from_container(
                                &host.address,
                                &c,
                            ));
                        }
                    }
                    Err(e) => {
                        errors.push(format!("{}: {e}", host.address));
                    }
                }
            }
            let _ = tx.send(Update::ServiceHistory {
                service,
                rows,
                errors,
            });
        });
    }

    /// The background `start_debug_sidecar` / `exec_interactive` task
    /// returned. Drop the result if the user has navigated away from
    /// the matching shell view (sidecar is `auto_remove` so it'll get
    /// reaped on its own); otherwise wire it into the shell panel or
    /// surface the error.
    fn apply_shell_session(&mut self, origin_view: &View, result: SessionResult, banner: &str) {
        if &self.view != origin_view {
            return;
        }
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        match result {
            Ok(session) => {
                shell.wire_session(session, self.shell_bytes_tx.clone(), 24, 80, banner);
            }
            Err(msg) => shell.set_error(msg),
        }
    }

    /// Forward a key event into the embedded shell. Returns true if
    /// the host outer loop should exit (Ctrl-Q is *intercepted* and
    /// returns the user to the host-detail pane, not exits yoink).
    async fn handle_shell_key(&mut self, key: KeyEvent) -> bool {
        let exit_shell = if let Some(shell) = self.shell.as_mut() {
            shell.handle_key(key, self.ops.clone())
        } else {
            true
        };
        if exit_shell && let View::ContainerShell { host, .. } = self.view.clone() {
            self.transition(View::HostDetail(host)).await;
        }
        false
    }

    /// 3-second tick: schedule a background refresh for whichever pane
    /// shows live container data. The fetch task runs concurrently with
    /// the event loop; the result lands via `update_rx`.
    fn fast_tick(&mut self) {
        match &self.view {
            View::Dashboard | View::Services | View::ServiceDetail(_) => {
                self.schedule_dashboard_refresh();
            }
            View::HostDetail(_) => self.schedule_host_detail_refresh(),
            View::ContainerDetail { .. } => self.schedule_container_detail_refresh(),
            View::ContainerShell { .. } => self.shell_tick(),
            // Resources fetches are heavier (3 list calls × N hosts);
            // gate behind the dedicated `resources_tick` that fires
            // less often. The fast tick is a no-op for the rest.
            _ => {}
        }
    }

    /// Pump bytes from the shell bridge into the vt100 parser, and
    /// bounce back to host-detail if the shell exited (Ctrl-D / `exit`).
    /// Synchronous because it's called from `fast_tick`; the transition
    /// here is a direct view swap (the shell view has no log streams or
    /// other async cleanup that the full `transition` would handle).
    fn shell_tick(&mut self) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        shell.poll_exit();
        if !shell.exited() {
            return;
        }
        let View::ContainerShell { host, .. } = self.view.clone() else {
            return;
        };
        self.shell = None;
        self.host_detail.set_host(host.clone());
        self.view = View::HostDetail(host);
        self.schedule_host_detail_refresh();
    }

    fn schedule_hosts_refresh(&mut self) {
        if self.hosts_in_flight {
            return;
        }
        self.hosts_in_flight = true;
        let ops = self.ops.clone();
        let hosts = self.config.hosts.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let rows = hosts::fetch_rows_owned(ops, hosts).await;
            let _ = tx.send(Update::Hosts(rows));
        });
    }

    fn schedule_host_detail_refresh(&mut self) {
        let Some(host) = self.host_detail.host().cloned() else {
            return;
        };
        if self.host_detail_in_flight {
            return;
        }
        self.host_detail_in_flight = true;
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let data = host_detail::fetch_owned(ops, host.clone()).await;
            let _ = tx.send(Update::HostDetail { host, data });
        });
    }

    fn schedule_container_detail_refresh(&mut self) {
        if self.container_detail_in_flight {
            return;
        }
        let Some((host, container)) = self.container_detail.target().cloned() else {
            return;
        };
        self.container_detail_in_flight = true;
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let data = container_detail::fetch_owned(ops, host.clone(), container.clone()).await;
            let _ = tx.send(Update::ContainerDetail {
                host,
                container,
                data: Box::new(data),
            });
        });
    }

    fn schedule_resources_refresh(&mut self) {
        if self.resources_in_flight {
            return;
        }
        self.resources_in_flight = true;
        let ops = self.ops.clone();
        let config = self.config.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let data = resources::fetch_owned(ops, config).await;
            let _ = tx.send(Update::Resources(data));
        });
    }

    /// Spawn a background `docker top` for the currently-selected
    /// container in `ContainerDetail`. Result lands via `Update::Top`
    /// and is pushed onto `container_detail` for modal rendering.
    fn schedule_top_fetch(&mut self) {
        let Some((host, container)) = self.container_detail.target().cloned() else {
            return;
        };
        self.container_detail.begin_top_load();
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        let host_for_msg = host.clone();
        let container_for_msg = container.clone();
        tokio::spawn(async move {
            let result = container_detail::fetch_top(ops, host, container).await;
            let _ = tx.send(Update::Top {
                host: host_for_msg,
                container: container_for_msg,
                result,
            });
        });
    }

    /// Send a one-off lifecycle command (start / stop / restart) for
    /// the given container; show a toast on failure. Stays on the
    /// event loop because docker's lifecycle endpoints are usually
    /// fast enough not to need a progress modal — and the operator
    /// gets immediate feedback via the docker-events stream which
    /// repaints the dashboard within milliseconds.
    fn spawn_lifecycle(&self, host: Host, container: String, op: LifecycleOp) {
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let res = match op {
                LifecycleOp::Start => ops.start_container(&host, &container).await,
                LifecycleOp::Stop => {
                    ops.stop_container(&host, &container, Duration::from_secs(10))
                        .await
                }
                LifecycleOp::Restart => {
                    ops.restart_container(&host, &container, Duration::from_secs(10))
                        .await
                }
            };
            if let Err(e) = res {
                let _ = tx.send(Update::Toast(format!(
                    "✗ {} {}/{container}: {e}",
                    op.label(),
                    host.address
                )));
            } else {
                let _ = tx.send(Update::Toast(format!("✓ {} {}", op.label(), container)));
            }
        });
    }

    /// Background remove-resource. Same toast UX as `spawn_lifecycle`.
    fn spawn_resource_remove(&self, target: ResourceTarget) {
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        let label = target.label.clone();
        tokio::spawn(async move {
            let res = match target.kind {
                ResourceTab::Images => ops.remove_image(&target.host, &target.id, false).await,
                ResourceTab::Volumes => ops.remove_volume(&target.host, &target.id, false).await,
                ResourceTab::Networks => ops.remove_network(&target.host, &target.id).await,
            };
            match res {
                Ok(()) => {
                    let _ = tx.send(Update::Toast(format!("✓ removed {label}")));
                }
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!("✗ remove {label}: {e}")));
                }
            }
        });
    }

    /// Background prune for whichever resource tab is active. Each
    /// branch maps to the equivalent `docker {kind} prune` call
    /// fanned out across every host.
    fn spawn_resource_prune(&self, kind: ResourceTab, dangling_only: bool) {
        let ops = self.ops.clone();
        let hosts: Vec<Host> = self.config.hosts.iter().map(Host::from).collect();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let mut summaries: Vec<String> = Vec::new();
            for host in hosts {
                let res = match kind {
                    ResourceTab::Images => ops.prune_images(&host, dangling_only).await,
                    ResourceTab::Volumes => ops.prune_volumes(&host).await,
                    ResourceTab::Networks => ops.prune_networks(&host).await,
                };
                match res {
                    Ok(report) => summaries.push(format!(
                        "{}: -{} ({} items)",
                        host.address,
                        crate::output::format_bytes(report.space_reclaimed_bytes),
                        report.reclaimed.len()
                    )),
                    Err(e) => summaries.push(format!("{}: failed: {e}", host.address)),
                }
            }
            let kind_label = match kind {
                ResourceTab::Images => "image prune",
                ResourceTab::Volumes => "volume prune",
                ResourceTab::Networks => "network prune",
            };
            let _ = tx.send(Update::Toast(format!(
                "✓ {kind_label} · {}",
                summaries.join(" · ")
            )));
        });
    }

    fn schedule_dashboard_refresh(&mut self) {
        if self.dashboard_in_flight {
            return;
        }
        self.dashboard_in_flight = true;
        let ops = self.ops.clone();
        let config = self.config.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let data = dashboard::fetch_owned(ops, config).await;
            let _ = tx.send(Update::Dashboard(data));
        });
    }

    fn apply_update(&mut self, update: Update) {
        match update {
            Update::Hosts(rows) => {
                self.hosts.apply(rows);
                self.hosts_in_flight = false;
            }
            Update::HostDetail { host, data } => {
                // Drop stale results from a prior host the user has since
                // navigated away from.
                if self.host_detail.host() == Some(&host) {
                    self.host_detail.apply(data);
                }
                self.host_detail_in_flight = false;
            }
            Update::ContainerDetail {
                host,
                container,
                data,
            } => {
                if self.container_detail.target().map(|(h, c)| (h, c.as_str()))
                    == Some((&host, container.as_str()))
                {
                    self.container_detail.apply(*data);
                }
                self.container_detail_in_flight = false;
            }
            Update::Dashboard(data) => {
                // Long error strings (ssh-probe with a Tailscale auth
                // URL, multi-line bollard errors) get auto-popped as a
                // modal so the URL is actually visible — the one-line
                // footer truncates anything longer than the terminal.
                if let Some(err) = data.error.as_deref() {
                    self.show_error_modal_once("connection error", err);
                }
                // Services & ServiceDetail share the same StatusReport
                // as Dashboard. Clone it into both before handing the
                // original off to dashboard's apply (which moves it).
                let report_clone = data.report.clone();
                let service_names: Vec<String> = self
                    .config
                    .services
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                self.services.apply(report_clone.clone(), service_names);
                self.service_detail
                    .apply(report_clone.map(Arc::new), &self.config);
                self.dashboard.apply(data);
                self.dashboard_in_flight = false;
            }
            Update::ServiceHistory {
                service,
                rows,
                errors,
            } => {
                self.history.apply(&service, rows, errors);
                self.history_in_flight = false;
            }
            Update::Event { host, event } => self.on_docker_event(&host, &event),
            Update::Toast(msg) => self.push_toast(msg),
            Update::Resources(data) => {
                self.resources.apply(data);
                self.resources_in_flight = false;
            }
            Update::StatsBatch(samples) => {
                for (host, container, stats) in samples {
                    self.record_stats_sample(&host, &container, &stats);
                }
                self.gc_container_history();
            }
            Update::Top {
                host,
                container,
                result,
            } => match result {
                Ok(table) => {
                    if self.container_detail.target().map(|(h, c)| (h, c.as_str()))
                        == Some((&host, container.as_str()))
                    {
                        self.container_detail.set_top(table);
                    }
                }
                Err(e) => {
                    self.container_detail.dismiss_top();
                    self.push_toast(format!("✗ docker top {container}: {e}"));
                }
            },
            Update::Drift {
                host,
                service,
                result,
            } => {
                self.drift.apply(&host, &service, result);
            }
            Update::Doctor(findings) => self.doctor.store(findings),
            Update::PortForwardOpened {
                host,
                service,
                endpoint,
                local_port,
                url,
                tunnel,
                sidecar,
                target_container,
            } => {
                self.forwards.insert(super::pf::ActiveForward::new(
                    &host,
                    &service,
                    endpoint,
                    local_port,
                    url,
                    target_container,
                    tunnel,
                    sidecar,
                ));
            }
            Update::VscodeOpened {
                host,
                service,
                url,
                tunnel,
                sidecar,
            } => {
                self.push_toast(format!("◊ vscode {service} → {url}"));
                if let Err(e) = crate::pf::open_in_browser(&url) {
                    self.push_toast(format!("✗ open browser: {e} — paste {url}"));
                }
                self.vscode.insert(super::vscode::ActiveSession::new(
                    &host, &service, url, tunnel, sidecar,
                ));
            }
        }
    }

    /// Open the doctor modal and spawn the check-runner. Findings
    /// land via `Update::Doctor`; the modal renders Loading until
    /// they arrive. Idempotent — re-pressing `D` (or `r` while
    /// open) just kicks off another run.
    fn open_doctor(&mut self) {
        self.doctor.mark_loading();
        let ops = self.ops.clone();
        let config = self.config.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let findings = crate::doctor::run_doctor(&config, ops).await;
            let _ = tx.send(Update::Doctor(findings));
        });
    }

    /// Open the drift modal for `(host, service)` and spawn the
    /// background fetch. The fetch reuses `diff::compute` so the
    /// modal shows exactly what `yoink up --plan --service <name>`
    /// would print on the CLI side. Tag overrides default to the
    /// running container's `yoink.version` for git-versioned
    /// services where `service.tag` is absent — otherwise
    /// `compute` would error with `TagMissing`, which is correct
    /// for `yoink up` but useless for "what changed".
    fn open_drift(&mut self, host: Host, service: String) {
        self.drift.set_target(host.clone(), service.clone());
        let ops = self.ops.clone();
        let config = self.config.clone();
        let secrets = self.secrets.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            // Fresh bundle per modal open. The cached one is loaded
            // ONCE at startup (spawn_secrets_loader); a long-running
            // TUI that outlives an Infisical edit / rotation would
            // otherwise compute desired_hash from stale env and show
            // bogus drift forever. On refresh failure, fall back to
            // the cached bundle so the modal still renders something
            // and surface the staleness via a toast.
            let bundle = match crate::secrets::load_bundle(&config).await {
                Ok(Some(fresh)) => {
                    let arc = std::sync::Arc::new(fresh);
                    *secrets.write().await = Some(arc.clone());
                    Some(arc)
                }
                Ok(None) => None,
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!(
                        "✗ secrets refresh failed: {e} (drift may use stale values)"
                    )));
                    secrets.read().await.clone()
                }
            };
            let mut overrides = std::collections::BTreeMap::new();
            // For services with no `tag:` in config, fall back to the
            // running replica's tag so the diff isolates the env/label
            // change instead of erroring on a missing tag.
            if let Some(svc) = config.services.iter().find(|s| s.name == service)
                && svc.tag.is_none()
                && let Ok(containers) = ops
                    .list_containers_by_label(&host, &format!("yoink.service={service}"))
                    .await
                && let Some(running) = containers
                    .iter()
                    .find(|c| c.is_running())
                    .and_then(|c| c.yoink_version.clone())
            {
                overrides.insert(service.clone(), running);
            }
            let services_filter = std::slice::from_ref(&service);
            let result = match crate::diff::compute(
                ops.as_ref(),
                &config,
                &overrides,
                Some(services_filter),
                bundle.as_deref(),
            )
            .await
            {
                Ok(report) => Ok(report
                    .services
                    .into_iter()
                    .find(|d| d.host == host.address && d.service == service)),
                Err(e) => Err(format!("{e}")),
            };
            let _ = tx.send(Update::Drift {
                host,
                service,
                result,
            });
        });
    }

    /// Open a port-forward to the focused (host, service) and register
    /// it in `self.forwards`. Same auto-mode dispatch the CLI uses:
    /// published-port fast path when available, sidecar fallback for
    /// secure-by-default services with no `publish:` block (api/web).
    /// No-op when an active forward already exists for the same key.
    fn open_port_forward(
        &mut self,
        host: Host,
        service_name: String,
        target_container: Option<String>,
    ) {
        let Some(service) = self
            .config
            .services
            .iter()
            .find(|s| s.name == service_name)
            .cloned()
        else {
            self.push_toast(format!("✗ no service named {service_name}"));
            return;
        };

        // Container port: unique publish if there's exactly one,
        // else `service.run.port` (the healthcheck port — right
        // default for non-published services).
        let container_port = if let Some(ep) = crate::pf::sole_publish(&service) {
            ep.container_port
        } else if let Some(p) = service.run.port {
            p
        } else {
            self.push_toast(format!(
                "✗ {service_name} has no `publish:` and no `run.port:` — pick a port via `yoink pf` on the CLI"
            ));
            return;
        };

        if let Some(existing) = self
            .forwards
            .get(&host.address, &service_name, container_port)
        {
            self.push_toast(format!("→ already up: {}", existing.url));
            return;
        }

        let host_for_task = host.clone();
        let service_name_for_task = service_name.clone();
        let service_for_task = service.clone();
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        let toast_prefix = format!("→ {service_name} :{container_port}");
        tokio::spawn(async move {
            let keyfile = ops.ssh_keyfile(&host_for_task);
            if let Err(e) =
                crate::ssh_probe::probe(&host_for_task, keyfile.as_deref().and_then(|p| p.to_str()))
                    .await
            {
                let _ = tx.send(Update::Toast(format!(
                    "✗ ssh probe to {} failed: {e}",
                    host_for_task.address
                )));
                return;
            }
            let resolved = match crate::pf::resolve_target(
                ops.clone(),
                &host_for_task,
                &service_for_task,
                container_port,
                crate::pf::Mode::Auto,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!("✗ port-forward failed: {e}")));
                    return;
                }
            };
            let (dial_host, dial_port, endpoint, sidecar) = match resolved {
                crate::pf::ResolvedTarget::Published(ep) => {
                    (ep.host_ip.clone(), ep.host_port, ep, None)
                }
                crate::pf::ResolvedTarget::Sidecar(handle) => {
                    let host_port = handle.host_port();
                    (
                        crate::pf::SIDECAR_DIAL_HOST.to_string(),
                        host_port,
                        crate::pf::PublishedEndpoint {
                            host_ip: crate::pf::SIDECAR_DIAL_HOST.into(),
                            host_port,
                            container_port,
                        },
                        Some(handle),
                    )
                }
            };
            match crate::transport::tunnel::SshTunnel::open_with_local_port(
                &host_for_task.user,
                &host_for_task.address,
                &dial_host,
                dial_port,
                None, // OS-assigned local port
                crate::pf::TUNNEL_READY_TIMEOUT,
                keyfile.as_deref(),
            )
            .await
            {
                Ok(tunnel) => {
                    let local_port = tunnel.local_port();
                    let url = crate::pf::forward_url(
                        local_port,
                        container_port,
                        crate::pf::SchemeOverride::Auto,
                    );
                    let _ = tx.send(Update::PortForwardOpened {
                        host: host_for_task,
                        service: service_name_for_task,
                        endpoint,
                        local_port,
                        url: url.clone(),
                        tunnel,
                        sidecar,
                        target_container,
                    });
                    let _ = tx.send(Update::Toast(format!(
                        "{toast_prefix} → {url}  [o] open  [F] close"
                    )));
                }
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!("✗ port-forward failed: {e}")));
                }
            }
        });
    }

    /// Spawn a code-server sidecar for the focused (host, service)
    /// and open VS Code in the browser. No-op when a session is
    /// already up for that pair (the toast says so).
    fn open_vscode_session(&mut self, host: Host, service_name: String) {
        if let Some(existing) = self.vscode.get(&host.address, &service_name) {
            self.push_toast(format!("◊ already up: {}", existing.url));
            return;
        }
        let Some(service) = self
            .config
            .services
            .iter()
            .find(|s| s.name == service_name)
            .cloned()
        else {
            self.push_toast(format!("✗ no service named {service_name}"));
            return;
        };

        let host_for_task = host.clone();
        let service_for_task = service.clone();
        let service_name_for_task = service_name.clone();
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        self.push_toast(format!("◊ vscode {service_name}: starting sidecar …"));
        tokio::spawn(async move {
            let keyfile = ops.ssh_keyfile(&host_for_task);
            if let Err(e) =
                crate::ssh_probe::probe(&host_for_task, keyfile.as_deref().and_then(|p| p.to_str()))
                    .await
            {
                let _ = tx.send(Update::Toast(format!(
                    "✗ ssh probe to {} failed: {e}",
                    host_for_task.address
                )));
                return;
            }
            let (target_container, network) = match crate::vscode::resolve_target(
                ops.as_ref(),
                &host_for_task,
                &service_for_task,
            )
            .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!("✗ vscode: {e}")));
                    return;
                }
            };
            let handle = match crate::vscode::spawn_codeserver_sidecar(
                ops.clone(),
                host_for_task.clone(),
                &service_name_for_task,
                &target_container,
                &network,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(Update::Toast(format!("✗ vscode: {e}")));
                    return;
                }
            };
            let host_port = handle.host_port();
            let tunnel = match crate::transport::tunnel::SshTunnel::open_with_local_port(
                &host_for_task.user,
                &host_for_task.address,
                crate::pf::SIDECAR_DIAL_HOST,
                host_port,
                None,
                crate::pf::TUNNEL_READY_TIMEOUT,
                keyfile.as_deref(),
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    handle.close().await;
                    let _ = tx.send(Update::Toast(format!("✗ vscode tunnel: {e}")));
                    return;
                }
            };
            let local_port = tunnel.local_port();
            if let Err(e) = crate::vscode::wait_for_http(local_port).await {
                handle.close().await;
                drop(tunnel);
                let _ = tx.send(Update::Toast(format!("✗ vscode: {e}")));
                return;
            }
            let url = format!("http://127.0.0.1:{local_port}/?folder=/proc/1/root");
            let _ = tx.send(Update::VscodeOpened {
                host: host_for_task.address,
                service: service_name_for_task,
                url,
                tunnel,
                sidecar: handle,
            });
        });
    }

    /// React to a `docker events` push by refreshing whichever pane is
    /// visible. Container start/stop/die/health-status are the events
    /// that mean what the user sees on screen has changed; we ignore
    /// the chatty ones (`exec_create`, `exec_start`, `attach`, …) so we don't
    /// thrash on them.
    fn on_docker_event(&mut self, host: &Host, event: &DockerEvent) {
        if event.kind != DockerEventKind::Container {
            return;
        }
        let interesting = matches!(
            event.action.as_str(),
            "start"
                | "stop"
                | "die"
                | "kill"
                | "create"
                | "destroy"
                | "rename"
                | "restart"
                | "pause"
                | "unpause"
                | "health_status"
                | "health_status: healthy"
                | "health_status: unhealthy"
                | "health_status: starting"
                | "oom"
        );
        if !interesting {
            return;
        }
        // Surface the event as a toast in the breadcrumb header.
        let container = event.container.as_deref().unwrap_or("?");
        let line = format!("{} · {} {}", host.address, container, event.action);
        // Append to the per-host ring so HostDetail's events panel
        // can show recent activity. Format includes a relative
        // timestamp ("now") that ages on each render — kept simple
        // for now (just the action), but the entry is timestamped
        // below for future "5m ago" rendering.
        let ring = self.host_events.entry(host.address.clone()).or_default();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let stamped = format!(
            "{}  {} {}",
            crate::output::format_relative_time(i64::try_from(now_secs).ok()),
            container,
            event.action
        );
        ring.push_back(stamped);
        while ring.len() > EVENT_HISTORY_PER_HOST {
            ring.pop_front();
        }
        self.toasts
            .push_back((std::time::Instant::now() + TOAST_TTL, line));
        while self.toasts.len() > TOAST_CAP {
            self.toasts.pop_front();
        }
        match &self.view {
            View::Dashboard | View::Services | View::ServiceDetail(_) => {
                self.schedule_dashboard_refresh();
            }
            View::Hosts => self.schedule_hosts_refresh(),
            View::HostDetail(active) if active == host => self.schedule_host_detail_refresh(),
            _ => {}
        }
    }

    async fn start_service_log_streams(&mut self) {
        let report = match StatusReport::collect(self.ops.as_ref(), &self.config).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to collect status for log streams");
                self.push_toast(format!("✗ log collect failed: {e}"));
                return;
            }
        };
        for host_status in report.hosts {
            for container in host_status.containers.iter().filter(|c| c.is_running()) {
                let host = Host {
                    user: self
                        .config
                        .hosts
                        .iter()
                        .find(|h| h.address == host_status.host)
                        .map(|h| h.user.clone())
                        .unwrap_or_default(),
                    address: host_status.host.clone(),
                };
                self.spawn_log_forwarder(host, container.name.clone()).await;
            }
        }
    }

    async fn spawn_log_forwarder(&mut self, host: Host, container: String) {
        let docker_rx = match self
            .ops
            .open_log_stream(&host, &container, LOG_BACKFILL_LINES)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                warn!(host = %host.address, container = %container, error = %e, "open_log_stream failed");
                self.push_toast(format!("✗ logs {}/{container}: {e}", host.address));
                return;
            }
        };
        let tx = self.log_tx.clone();
        let task = if self.hl_available {
            tokio::spawn(forward_through_hl(container, docker_rx, tx))
        } else {
            tokio::spawn(forward_raw(docker_rx, tx))
        };
        self.log_tasks.push(task);
    }

    fn stop_log_streams(&mut self) {
        for task in self.log_tasks.drain(..) {
            task.abort();
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let (header_area, mut pane_area) = super::ui::split_with_header(frame.area());
        // Slice a single row off the bottom for the port-forward footer
        // when any tunnels are active. Goal: operators can't forget they
        // have an open tunnel — the band stays visible across every pane.
        let footer_rows: u16 =
            u16::from(!self.forwards.is_empty()) + u16::from(!self.vscode.is_empty());
        let (pf_footer_area, vscode_footer_area) =
            if footer_rows > 0 && pane_area.height >= footer_rows + 1 {
                let mut constraints = vec![ratatui::layout::Constraint::Min(0)];
                for _ in 0..footer_rows {
                    constraints.push(ratatui::layout::Constraint::Length(1));
                }
                let split = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints(constraints)
                    .split(pane_area);
                pane_area = split[0];
                let mut idx = 1;
                let pf = if !self.forwards.is_empty() {
                    let a = Some(split[idx]);
                    idx += 1;
                    a
                } else {
                    None
                };
                let vs = if !self.vscode.is_empty() {
                    Some(split[idx])
                } else {
                    None
                };
                (pf, vs)
            } else {
                (None, None)
            };
        let crumbs = self.view.breadcrumb();
        // Drop expired toasts and pick the freshest live one for the
        // right-side info slot. When nothing's live, fall back to the
        // host/service counts.
        let now = std::time::Instant::now();
        while self.toasts.front().is_some_and(|(d, _)| *d <= now) {
            self.toasts.pop_front();
        }
        let right = self.toasts.back().map_or_else(
            || {
                format!(
                    "{} hosts · {} services",
                    self.config.hosts.len(),
                    self.config.services.len()
                )
            },
            |(_, msg)| format!("● {msg}"),
        );
        let tabs = [
            "Dashboard",
            "Hosts",
            "Services",
            "Logs",
            "Resources",
            "Secrets",
        ];
        let selected_tab = Some(self.view.top_section());
        super::ui::render_header(
            frame,
            header_area,
            &tabs,
            selected_tab,
            &crumbs,
            &right,
            self.config.slug.as_deref(),
            self.config_source.as_deref(),
        );

        let secrets = self.secrets.try_read().ok().and_then(|g| g.clone());
        match &self.view {
            View::Dashboard => {
                self.dashboard.render(
                    frame,
                    pane_area,
                    &self.config,
                    secrets.as_deref(),
                    &self.container_history,
                    &self.forwards,
                    &self.vscode,
                    &self.throbber_state,
                );
            }
            View::Hosts => self
                .hosts
                .render(frame, pane_area, &self.config, &self.throbber_state),
            View::HostDetail(host) => {
                let events: Vec<String> = self
                    .host_events
                    .get(&host.address)
                    .map(|q| q.iter().cloned().collect())
                    .unwrap_or_default();
                self.host_detail.render(
                    frame,
                    pane_area,
                    &self.config,
                    secrets.as_deref(),
                    &events,
                    &self.container_history,
                    &self.forwards,
                    &self.vscode,
                    &self.throbber_state,
                );
            }
            View::ContainerDetail { host, container } => {
                // Split: top 2/3 = inspect data, bottom 1/3 = live log tail.
                let split = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints([
                        ratatui::layout::Constraint::Min(0),
                        ratatui::layout::Constraint::Length(12),
                    ])
                    .split(pane_area);
                let history: Option<&StatsHistory> = self
                    .container_history
                    .get(&(host.address.clone(), container.clone()));
                self.container_detail
                    .render(frame, split[0], history, &self.throbber_state);
                self.logs.render(frame, split[1], &self.config);
            }
            View::Services => self.services.render(
                frame,
                pane_area,
                &self.config,
                &self.forwards,
                &self.vscode,
                &self.throbber_state,
            ),
            View::ServiceDetail(_) => {
                self.service_detail.render(
                    frame,
                    pane_area,
                    &self.config,
                    secrets.as_deref(),
                    &self.throbber_state,
                );
            }
            View::ServiceHistory(_) => {
                self.history.render(frame, pane_area, &self.throbber_state);
            }
            View::Logs | View::ContainerLogs { .. } => {
                self.logs.render(frame, pane_area, &self.config);
            }
            View::ContainerShell { .. } => {
                if let Some(shell) = self.shell.as_mut() {
                    // Send the current panel size to the daemon (no-op
                    // if unchanged) so the in-shell view re-flows on
                    // window resize. Inside-border = panel minus the
                    // header row + footer row + box borders (2 each).
                    let rows = pane_area.height.saturating_sub(4);
                    let cols = pane_area.width.saturating_sub(2);
                    shell.apply_size(self.ops.clone(), rows, cols);
                    shell.render(frame, pane_area);
                }
            }
            View::Secrets => {
                self.secrets_state.ensure_loaded(&self.config, false);
                self.secrets_state
                    .render(frame, pane_area, &self.throbber_state);
            }
            View::Resources => {
                self.resources
                    .render(frame, pane_area, &self.config, &self.throbber_state);
            }
        }

        if self.show_help {
            let lines = self.view.help_lines();
            super::ui::render_modal(frame, "yoink help (? to close)", &lines);
        }
        if let Some((host, container)) = &self.kill_target {
            let host_line = format!("host:        {}", host.address);
            let container_line = format!("container:   {container}");
            let lines = vec![
                "About to send SIGKILL.",
                "",
                host_line.as_str(),
                container_line.as_str(),
                "",
                "Container stays around for `docker logs` / `inspect`.",
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, "kill container?", &lines);
        }
        if let Some((title, body)) = self.secrets_state.confirm_modal_lines() {
            let lines: Vec<&str> = body.iter().map(String::as_str).collect();
            super::ui::render_modal(frame, title, &lines);
        }
        if let Some((service, tag)) = &self.reconcile_target {
            let svc_line = format!("service:  {service}");
            let tag_line = format!("tag:      {tag}");
            let host_count = format!(
                "hosts:    {} (synthetic local skipped)",
                self.config.hosts.len(),
            );
            let lines = vec![
                "About to run `yoink up` for one service.",
                "",
                svc_line.as_str(),
                tag_line.as_str(),
                host_count.as_str(),
                "",
                "Acquires the deploy lock per host. Drift-only",
                "containers (matching spec already running) are no-ops;",
                "anything that needs swapping goes through the",
                "healthcheck-gated rolling deploy.",
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, "reconcile service?", &lines);
        }
        if self.reconcile_all_target {
            let svc_count = format!("services: {}", self.config.services.len());
            let host_count = format!("hosts:    {}", self.config.hosts.len());
            let lines = vec![
                "About to run `yoink up` for every service.",
                "",
                svc_count.as_str(),
                host_count.as_str(),
                "",
                "Reconciles each service in config order. Services",
                "without a config-pinned tag fall back to the running",
                "container's tag (no version bump). Drift-free services",
                "are no-ops; anything that needs swapping goes through",
                "the per-host healthcheck-gated rolling deploy.",
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, "reconcile ALL services?", &lines);
        }
        if let Some(target) = &self.resource_remove_target {
            let kind = match target.kind {
                ResourceTab::Images => "image",
                ResourceTab::Volumes => "volume",
                ResourceTab::Networks => "network",
            };
            let host_line = format!("host:   {}", target.host.address);
            let target_line = format!("target: {}", target.label);
            let title = format!("remove {kind}?");
            let lines = vec![
                "About to remove the selected resource.",
                "",
                host_line.as_str(),
                target_line.as_str(),
                "",
                "Container references that point at the resource will",
                "block removal — the daemon returns an error and the",
                "row is left in place. Force removal isn't wired up;",
                "kill referencing containers first.",
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, &title, &lines);
        }
        if let Some((kind, dangling_only)) = &self.resource_prune_target {
            let host_count = format!("hosts:  {}", self.config.hosts.len());
            let title = match kind {
                ResourceTab::Images => {
                    if *dangling_only {
                        "prune dangling images?"
                    } else {
                        "prune ALL unused images?"
                    }
                }
                ResourceTab::Volumes => "prune unused volumes?",
                ResourceTab::Networks => "prune unused networks?",
            };
            let body_line = match kind {
                ResourceTab::Images => {
                    if *dangling_only {
                        "Removes <none>:<none> layers (no live tag) on every host."
                    } else {
                        "Removes every image with NO live container reference."
                    }
                }
                ResourceTab::Volumes => "Removes named volumes with no attached container.",
                ResourceTab::Networks => {
                    "Removes user-defined networks with no attached container."
                }
            };
            let lines = vec![
                body_line,
                "",
                host_count.as_str(),
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, title, &lines);
        }
        if self.prune_target {
            let host_count = format!("hosts:    {}", self.config.hosts.len());
            let lines = vec![
                "About to run `yoink prune`.",
                "",
                host_count.as_str(),
                "",
                "Removes yoink-managed containers whose service is no",
                "longer in config (renamed/deleted) and stale exited",
                "containers from previous deploys. Running containers",
                "of known services are kept.",
                "",
                "[y] / Enter   confirm",
                "[any]         cancel",
            ];
            super::ui::render_modal(frame, "prune containers?", &lines);
        }
        if let Some(progress) = self.job_progress.as_ref() {
            let lines: Vec<String> = progress.lines.iter().cloned().collect();
            let header = match &progress.kind {
                JobKind::Reconcile { service, tag } => format!("reconcile · {service}:{tag}"),
                JobKind::ReconcileAll { .. } => "reconcile · all services".to_string(),
                JobKind::Prune => "prune".to_string(),
            };
            let title = format!(
                "{header}{}",
                match &progress.finished {
                    None => " · running",
                    Some(Ok(_)) => " · done — esc to close",
                    Some(Err(_)) => " · failed — esc to close",
                },
            );
            let success = matches!(progress.finished, Some(Ok(_)));
            let failure = matches!(progress.finished, Some(Err(_)));
            let running_throbber = progress.finished.is_none().then_some(&self.throbber_state);
            // ReconcileAll gets a status table on top of the scrolling
            // log so concurrent waves don't make the operator hunt for
            // "where is service X right now?".
            if let JobKind::ReconcileAll { statuses } = &progress.kind {
                let status_rows: Vec<(String, String, ratatui::style::Color)> = statuses
                    .iter()
                    .map(|(name, status)| (name.clone(), status.label(), status.color()))
                    .collect();
                super::ui::render_status_log_modal(
                    frame,
                    &title,
                    &status_rows,
                    &lines,
                    success,
                    failure,
                    running_throbber,
                );
            } else {
                super::ui::render_log_modal(
                    frame,
                    &title,
                    &lines,
                    success,
                    failure,
                    running_throbber,
                );
            }
        }

        if let Some(area) = pf_footer_area {
            self.forwards.render_footer(frame, area);
        }
        if let Some(area) = vscode_footer_area {
            self.vscode.render_footer(frame, area);
        }

        if self.drift.is_visible() {
            super::drift::render_modal(frame, &self.drift, &self.throbber_state);
        }

        if self.doctor.is_open() {
            super::doctor::render(frame, frame.area(), &mut self.doctor, &self.throbber_state);
        }

        // Render last so it sits on top of everything else when a
        // long error needs the operator's attention. Lines stay
        // verbatim — the modal sizes to the longest line so URLs
        // are guaranteed to fit (clamped by terminal width).
        if let Some(modal) = &self.error_modal {
            let mut body: Vec<&str> = modal.lines.iter().map(String::as_str).collect();
            body.push("");
            body.push("[Esc] / Enter   dismiss");
            super::ui::render_modal(frame, &modal.title, &body);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_log_streams();
        self.stop_event_subscriptions();
        self.stop_stats_history_pollers();
        // Sidecar cleanup on the panic-drop path (the transition path
        // already does this on normal exit). `cleanup` spawns a force-
        // remove; if the runtime is gone, `auto_remove: true` + the
        // dropped SSH connection still reaps the alpine container.
        if let Some(mut shell) = self.shell.take() {
            shell.cleanup(self.ops.clone());
        }
    }
}

/// Step the per-service status machine in response to one `DeployEvent`.
/// Events that don't carry a status implication (`NetworkReady`, hooks,
/// `ContainerLogTail`) are no-ops here.
fn update_service_status(
    map: &mut std::collections::BTreeMap<String, ReconcileServiceStatus>,
    service: &str,
    event: &crate::deploy::DeployEvent,
) {
    use crate::deploy::DeployEvent;
    let next = match event {
        DeployEvent::Started { .. } | DeployEvent::PullStarted { .. } => {
            ReconcileServiceStatus::Pulling
        }
        DeployEvent::PullFinished { .. } => ReconcileServiceStatus::Pulled,
        DeployEvent::ContainerStarted { container, .. } => ReconcileServiceStatus::Healthchecking {
            container: container.clone(),
        },
        DeployEvent::HealthcheckHealthy { .. } | DeployEvent::HealthcheckSkipped { .. } => {
            ReconcileServiceStatus::Swapping
        }
        DeployEvent::OldContainerStopped { .. } => ReconcileServiceStatus::Swapping,
        DeployEvent::AlreadyAtSpec { .. } => ReconcileServiceStatus::Synced,
        DeployEvent::Done { .. } => ReconcileServiceStatus::Done,
        // Top-level / non-service events leave the status alone.
        DeployEvent::HookStarted { .. }
        | DeployEvent::HookFinished { .. }
        | DeployEvent::NetworkReady { .. }
        | DeployEvent::ContainerLogTail { .. } => return,
    };
    map.insert(service.to_string(), next);
}

/// Look up `(user, address)` from the loaded config by address —
/// used so Dashboard's host names resolve to a full `Host` for
/// transitions that need the ssh user (`HostDetail`, kill modal).
fn config_host(config: &Config, address: &str) -> Option<Host> {
    config
        .hosts
        .iter()
        .find(|h| h.address == address)
        .map(Host::from)
}

/// Background reconcile task. Acquires the per-host advisory lock,
/// runs `deploy::reconcile` filtered to one service, pipes each
/// `DeployEvent` back to the run loop as a `JobUpdate::Event`,
/// then sends a final `JobUpdate::Done` regardless of outcome.
/// Same shape as `cmd_up` in main.rs but without the printlns.
async fn reconcile_one(
    mut config: Config,
    ops: Arc<dyn DockerOps>,
    secrets: Option<Arc<crate::secrets::SecretsBundle>>,
    service: String,
    tag: String,
    tx: UnboundedSender<JobUpdate>,
) {
    use crate::lock::HostLock;

    let send_event = |line: String| {
        let _ = tx.send(JobUpdate::Event(line));
    };
    let send_done = |result: Result<String, String>| {
        let _ = tx.send(JobUpdate::Done(result));
    };

    // Drop the synthetic `local` host (auto-injected for read-only
    // TUI browsing) before any destructive code runs. Operators who
    // genuinely want to deploy to local can add `address: local` to
    // yoink.yaml — that entry survives because it was on disk.
    config.hosts.retain(|h| h.address != Host::LOCAL_ADDRESS);
    if config.hosts.is_empty() {
        send_done(Err(
            "no real hosts to deploy to (only the synthetic local host exists)".into(),
        ));
        return;
    }

    // Acquire one lock per configured host. Per-host parallel like
    // cmd_up; on any failure release whatever we already grabbed and
    // bail.
    let acquire_futs = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let ops = ops.clone();
        async move { HostLock::acquire(&*ops, host.clone()).await }
    });
    let mut locks = match futures_util::future::try_join_all(acquire_futs).await {
        Ok(ls) => ls,
        Err(e) => {
            send_done(Err(format!("acquire deploy lock: {e}")));
            return;
        }
    };
    for lock in &mut locks {
        lock.spawn_heartbeat(ops.clone());
    }

    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert(service.clone(), tag.clone());
    let services_filter = vec![service.clone()];
    let secrets_ref = secrets.as_deref();
    let mut on_event = |svc: Option<&str>, e: crate::deploy::DeployEvent| {
        send_event(crate::output::format_deploy_event(svc, &e));
    };
    let result = crate::deploy::reconcile(
        &*ops,
        &config,
        &overrides,
        Some(&services_filter),
        secrets_ref,
        &mut on_event,
    )
    .await;
    for lock in locks {
        lock.release(&*ops).await;
    }
    match result {
        Ok(reports) => {
            let n = reports.first().map_or(0, |r| r.hosts.len());
            send_done(Ok(format!("reconciled {service}:{tag} on {n} host(s)")));
        }
        Err(e) => send_done(Err(format!("reconcile {service}: {e}"))),
    }
}

/// Background "reconcile every service" task — equivalent of
/// `yoink up` with no `--service` filter. Per-service errors stream
/// in but the run continues; the final Done line summarizes counts.
async fn reconcile_all(
    mut config: Config,
    ops: Arc<dyn DockerOps>,
    secrets: Option<Arc<crate::secrets::SecretsBundle>>,
    overrides: std::collections::BTreeMap<String, String>,
    tx: UnboundedSender<JobUpdate>,
) {
    use crate::lock::HostLock;

    let send_event = |line: String| {
        let _ = tx.send(JobUpdate::Event(line));
    };
    let send_done = |result: Result<String, String>| {
        let _ = tx.send(JobUpdate::Done(result));
    };

    config.hosts.retain(|h| h.address != Host::LOCAL_ADDRESS);
    if config.hosts.is_empty() {
        send_done(Err(
            "no real hosts to deploy to (only the synthetic local host exists)".into(),
        ));
        return;
    }

    let acquire_futs = config.hosts.iter().map(|host_cfg| {
        let host = Host::from(host_cfg);
        let ops = ops.clone();
        async move { HostLock::acquire(&*ops, host.clone()).await }
    });
    let mut locks = match futures_util::future::try_join_all(acquire_futs).await {
        Ok(ls) => ls,
        Err(e) => {
            send_done(Err(format!("acquire deploy lock: {e}")));
            return;
        }
    };
    for lock in &mut locks {
        lock.spawn_heartbeat(ops.clone());
    }

    let secrets_ref = secrets.as_deref();

    // services are already in deploy order — Config::topo_sort_services
    // ran at load time using each service's depends_on. Reconcile
    // walks them in that order; prefetch can fan out independently
    // since pulls don't care about ordering.
    // Phase 0: parallel image prefetch. Pulls dominate reconcile
    // time; doing them concurrently up-front turns sum-of-pulls
    // into max-of-pulls. Inline pulls during the per-service
    // reconcile then become docker-cache no-ops.
    let tx_prefetch = tx.clone();
    let prefetch_cb: std::sync::Arc<dyn Fn(crate::deploy::DeployEvent) + Send + Sync> =
        std::sync::Arc::new(move |e| {
            let _ = tx_prefetch.send(JobUpdate::Event(crate::output::format_deploy_event(
                None, &e,
            )));
        });
    if let Err(e) = crate::deploy::prefetch_images(
        ops.clone(),
        &config,
        &overrides,
        None,
        secrets_ref,
        true,  // never try to pull `build:` services from a registry — they're local-only
        false, // TUI doesn't expose --rebuild-proxy; CLI is the path for that
        prefetch_cb,
    )
    .await
    {
        for lock in locks {
            lock.release(&*ops).await;
        }
        send_done(Err(format!("prefetch images: {e}")));
        return;
    }

    // Forward each event through the per-service status tracker
    // (so the modal's table reflects live state) AND into the
    // scrolling log buffer prefixed with `[svc]` for readability.
    let mut on_event = |svc: Option<&str>, e: crate::deploy::DeployEvent| {
        if let Some(name) = svc {
            let _ = tx.send(JobUpdate::ServiceEvent(name.to_string(), e.clone()));
        }
        send_event(crate::output::format_deploy_event(svc, &e));
    };
    let result =
        crate::deploy::reconcile(&*ops, &config, &overrides, None, secrets_ref, &mut on_event)
            .await;
    for lock in locks {
        lock.release(&*ops).await;
    }
    match result {
        Ok(reports) => {
            let svc_count = reports.len();
            let host_count = reports.first().map_or(0, |r| r.hosts.len());
            send_done(Ok(format!(
                "reconciled {svc_count} service(s) on {host_count} host(s)"
            )));
        }
        Err(e) => send_done(Err(format!("reconcile all: {e}"))),
    }
}

/// Background prune task. Mirrors `cmd_prune` but pipes each removed
/// container into the progress modal as a streamed event line.
async fn prune_all(mut config: Config, ops: Arc<dyn DockerOps>, tx: UnboundedSender<JobUpdate>) {
    use crate::prune::{self, PruneReason};

    let send_event = |line: String| {
        let _ = tx.send(JobUpdate::Event(line));
    };
    let send_done = |result: Result<String, String>| {
        let _ = tx.send(JobUpdate::Done(result));
    };

    config.hosts.retain(|h| h.address != Host::LOCAL_ADDRESS);
    if config.hosts.is_empty() {
        send_done(Err("no real hosts to prune".into()));
        return;
    }

    send_event(format!("scanning {} host(s)…", config.hosts.len()));
    match prune::run(&*ops, &config, false).await {
        Ok(report) => {
            if report.removed.is_empty() {
                send_event("nothing to prune".into());
                send_done(Ok("nothing to prune".into()));
                return;
            }
            for item in &report.removed {
                let svc = item.service.as_deref().unwrap_or("?");
                let reason = match item.reason {
                    PruneReason::ServiceNotInConfig => "service not in config",
                    PruneReason::StaleExited => "stale exited",
                };
                send_event(format!(
                    "[{}] removed {} (service={svc}, {reason})",
                    item.host, item.container,
                ));
            }
            send_done(Ok(format!("removed {} container(s)", report.removed.len())));
        }
        Err(e) => send_done(Err(format!("prune: {e}"))),
    }
}

/// Plain (non-`hl`) forwarder: emit one unstyled `RenderedLine` per log
/// line via the shared formatter.
async fn forward_raw(
    mut docker_rx: mpsc::UnboundedReceiver<LogLine>,
    out: UnboundedSender<RenderedLine>,
) {
    while let Some(line) = docker_rx.recv().await {
        if out.send(RenderedLine::from_log_line(&line)).is_err() {
            break;
        }
    }
}

/// `hl` forwarder: spawn `hl --color=always --paginate=never`, write each
/// docker log message to its stdin, read the formatted (ANSI-colored)
/// lines from its stdout, convert escapes to ratatui spans, and forward
/// to the render channel. Owns the child via `kill_on_drop`.
async fn forward_through_hl(
    container: String,
    mut docker_rx: mpsc::UnboundedReceiver<LogLine>,
    out: UnboundedSender<RenderedLine>,
) {
    use ansi_to_tui::IntoText;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;

    // pamburus/hl flag conventions:
    //   --paging=never    pager off (it'd block our pipe otherwise)
    //   --follow          treat stdin as a live stream and flush as
    //                     entries arrive instead of buffering until EOF
    //   --sync-interval-ms 100  cadence at which the follow loop drains
    //   --input-info=none drop the leading "[in:0]" prefix hl adds when
    //                     it thinks there could be multiple inputs
    // Honour NO_COLOR (clig.dev) even inside the TUI's log pane:
    // when set, ask `hl` for plain text so the panes render uncoloured.
    let hl_color = if std::env::var_os("NO_COLOR").is_some() {
        "--color=never"
    } else {
        "--color=always"
    };
    let mut child = match Command::new("hl")
        .arg(hl_color)
        .arg("--paging=never")
        .arg("--follow")
        .arg("--sync-interval-ms=100")
        .arg("--input-info=none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(container = %container, error = %e, "hl spawn failed; falling back to raw");
            return forward_raw(docker_rx, out).await;
        }
    };
    let mut stdin = child.stdin.take(); // Option<ChildStdin>; None once we close it
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stdout).lines();
    // Drain stderr so a chatty hl can't fill its pipe; surface anything
    // it writes via tracing.
    let stderr_container = container.clone();
    tokio::spawn(async move {
        let mut err_reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = err_reader.next_line().await {
            warn!(container = %stderr_container, hl_stderr = %line);
        }
    });
    let prefix = format!("[{container}] ");
    let prefix_style = Style::default().fg(Color::Cyan);

    loop {
        tokio::select! {
            biased;
            line_res = reader.next_line() => {
                match line_res {
                    Ok(Some(text)) => {
                        let parsed = text.into_text().unwrap_or_default();
                        // hl emits one input line as one output line; if it
                        // ever splits, join the spans onto one rendered line.
                        let mut spans: Vec<Span<'static>> =
                            vec![Span::styled(prefix.clone(), prefix_style)];
                        let mut plain_part = String::new();
                        for parsed_line in parsed.lines {
                            for span in parsed_line.spans {
                                plain_part.push_str(&span.content);
                                spans.push(Span::styled(span.content.into_owned(), span.style));
                            }
                        }
                        let line = RenderedLine {
                            plain: format!("{prefix}{plain_part}"),
                            styled: Line::from(spans),
                        };
                        if out.send(line).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            line_opt = docker_rx.recv(), if stdin.is_some() => {
                match line_opt {
                    Some(line) => {
                        if let Some(s) = stdin.as_mut()
                            && (s.write_all(line.message.as_bytes()).await.is_err()
                                || s.write_all(b"\n").await.is_err())
                        {
                            stdin = None;
                        }
                    }
                    None => {
                        // Docker stream ended; closing stdin flushes hl,
                        // which then EOFs reader and we exit naturally.
                        stdin = None;
                    }
                }
            }
        }
    }
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn config() -> Arc<Config> {
        Arc::new(
            Config::parse_str(
                r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: img
    tag: v1
    run: { port: 3000, healthcheck_path: /health }
"#,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn app_constructs_idle() {
        let ops: Arc<dyn DockerOps> = Arc::new(FakeDockerOps::new());
        let app = App::new(
            config(),
            std::path::PathBuf::from("yoink.yaml"),
            ops,
            View::Dashboard,
            false,
        );
        assert_eq!(app.view, View::Dashboard);
    }

    #[tokio::test]
    async fn docker_event_schedules_dashboard_refresh() {
        let ops: Arc<dyn DockerOps> = Arc::new(FakeDockerOps::new());
        let mut app = App::new(
            config(),
            std::path::PathBuf::from("yoink.yaml"),
            ops,
            View::Dashboard,
            false,
        );
        assert!(!app.dashboard_in_flight);
        app.on_docker_event(
            &Host {
                user: "deploy".into(),
                address: "host-a".into(),
            },
            &DockerEvent {
                kind: DockerEventKind::Container,
                action: "start".into(),
                container: Some("app-a-deadbeef".into()),
            },
        );
        assert!(app.dashboard_in_flight, "start event must schedule refresh");
    }

    #[tokio::test]
    async fn docker_event_ignores_chatty_actions() {
        let ops: Arc<dyn DockerOps> = Arc::new(FakeDockerOps::new());
        let mut app = App::new(
            config(),
            std::path::PathBuf::from("yoink.yaml"),
            ops,
            View::Dashboard,
            false,
        );
        app.on_docker_event(
            &Host {
                user: "deploy".into(),
                address: "host-a".into(),
            },
            &DockerEvent {
                kind: DockerEventKind::Container,
                action: "exec_create".into(),
                container: Some("c".into()),
            },
        );
        assert!(
            !app.dashboard_in_flight,
            "exec_create is too chatty to refresh on"
        );
    }

    #[tokio::test]
    async fn dashboard_renders_loading_to_test_backend() {
        let ops: Arc<dyn DockerOps> = Arc::new(FakeDockerOps::new());
        let mut app = App::new(
            config(),
            std::path::PathBuf::from("yoink.yaml"),
            ops,
            View::Dashboard,
            false,
        );

        let backend = TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("yoink dashboard"));
    }
}
