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

use super::container_detail::{self, ContainerDetailRefresh, ContainerDetailState};
use super::dashboard::{self, DashboardRefresh, DashboardState};
use super::host_detail::{self, HostDetailRefresh, HostDetailState};
use super::hosts::{self, HostRow, HostsState};
use super::logs::{LogsState, RenderedLine};
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
        }
    }

    /// Lines for the `?` help overlay. Per-view so the operator only
    /// sees the keybinds that actually do something here.
    #[must_use]
    pub fn help_lines(&self) -> Vec<&'static str> {
        let global = vec![
            "global",
            "  q / Ctrl-C   quit yoink",
            "  d / h / s / l   dashboard / hosts / services / logs",
            "  ?            toggle this help overlay",
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
                "  e            toggle exited containers",
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
                "  i            container detail (labels, env-ish, live cpu/mem)",
                "  !            shell into container (bash/sh)",
                "  D            debug sidecar (alpine, target's pid+net ns)",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  /            filter substring · esc to clear",
                "  r            refresh",
                "  esc          back to hosts (when no active filter)",
            ],
            View::ContainerDetail { .. } => vec![
                "container detail",
                "  enter / l    live logs",
                "  !            shell · D debug sidecar",
                "  K            SIGKILL container (with confirmation)",
                "  U            reconcile this service (with confirmation)",
                "  r            refresh",
                "  esc          back to host detail",
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
                "  !            shell · D debug sidecar",
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
                "  !            shell · D debug sidecar",
                "  y            yank visible buffer to system clipboard",
                "  k            clear · esc back",
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
    if hl_disabled {
        tracing::info!("YOINK_NO_HL set; skipping hl pipeline");
    } else if hl_available {
        tracing::info!("hl detected on PATH; piping log streams through it");
    } else {
        tracing::info!("hl not on PATH; using raw log forwarder");
    }
    let mut terminal = setup_terminal(mouse).context("setup terminal")?;
    let result = run_loop(
        &mut terminal,
        Arc::new(config.clone()),
        config_path,
        ops,
        mode,
        hl_available,
    )
    .await;
    let restore = restore_terminal(&mut terminal, mouse);
    result.and(restore)
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
) -> Result<()> {
    let mut app = App::new(config, config_path, ops, View::from(mode), hl_available);
    // Schedule the first round of background fetches so each pane has data
    // by the time the user navigates to it.
    app.schedule_hosts_refresh();
    app.schedule_dashboard_refresh();
    app.start_event_subscriptions();
    app.spawn_secrets_loader();
    if matches!(app.view, View::Logs) {
        app.start_service_log_streams().await;
    }

    let mut events = EventStream::new();
    let mut fast_tick = interval(FAST_TICK);
    let mut hosts_tick = interval(HOSTS_TICK);
    let mut config_tick = interval(CONFIG_RELOAD_TICK);
    // Drives the "starting shell…" spinner animation. Fires only
    // matters when the shell view is up and `inner` is None; the
    // branch is gated so it's a no-op otherwise.
    let mut shell_spin_tick = interval(Duration::from_millis(125));
    fast_tick.tick().await;
    hosts_tick.tick().await;
    config_tick.tick().await;
    shell_spin_tick.tick().await;

    loop {
        terminal.draw(|f| app.render(f))?;
        tokio::select! {
            biased;
            input = events.next() => {
                match input {
                    Some(Ok(Event::Key(key))) => {
                        if app.on_key(key).await {
                            return Ok(());
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
            _ = config_tick.tick() => {
                app.maybe_reload_config();
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
    /// `Some` while a `ContainerShell` view is active; cleared on exit.
    shell: Option<ShellState>,
    /// `?` toggles a modal help overlay listing keybinds for the
    /// current view. Cleared on Esc and on any view transition.
    show_help: bool,
    /// `Some` while a kill-confirmation modal is open over the
    /// current view. The user confirms with `y` (or Enter) and
    /// cancels with anything else. Cleared on transition.
    kill_target: Option<(Host, String)>,
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
    /// TUI doesn't block on the Infisical fetch (which can take 1–3 s).
    /// `None` means "not loaded yet" — drift cells render as `?`
    /// until the loader finishes.
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
            shell: None,
            show_help: false,
            kill_target: None,
            reconcile_target: None,
            prune_target: false,
            reconcile_all_target: false,
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
            update_tx,
            update_rx,
            log_tasks: Vec::new(),
            log_tx,
            log_rx,
            event_tasks: Vec::new(),
            config_path,
            hl_available,
        }
    }

    /// Re-read the config from disk and swap it in if it parses + has
    /// actually changed. Silent no-op on parse errors so a half-saved
    /// edit doesn't blank the dashboard; the next tick will catch the
    /// finished edit. If `hosts:` changed we tear down and respawn the
    /// docker-events subscriptions.
    fn maybe_reload_config(&mut self) {
        let mut new_config = match Config::load_from_path(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, path = %self.config_path.display(), "config reload failed");
                return;
            }
        };
        // Re-apply the magical local-host injection — load_from_path
        // returns a fresh disk-shaped config that doesn't know about
        // it, so without this the synthetic `local` host disappears
        // every CONFIG_RELOAD_TICK.
        new_config.push_local_host_if_socket();
        if new_config == *self.config {
            return;
        }
        let hosts_changed = new_config.hosts != self.config.hosts;
        self.config = Arc::new(new_config);
        if hosts_changed {
            self.stop_event_subscriptions();
            self.start_event_subscriptions();
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

    /// Kick off the Infisical secrets fetch in the background. The Dashboard
    /// drift column needs the bundle to compute `spec_hashes` that
    /// match what `yoink up` would produce. We don't block startup
    /// on it — the column shows `?` for the few seconds the loader
    /// takes, then resolves to ✓/⚠ once the bundle lands.
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
            KeyCode::Char('d') => {
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
                KeyCode::Char('D') => {
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
                KeyCode::Char('D') => {
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
                KeyCode::Char('D') => {
                    let host = host.clone();
                    let container = container.clone();
                    self.transition(View::ContainerShell {
                        host,
                        container,
                        debug: true,
                    })
                    .await;
                }
                KeyCode::Char('k') => self.logs.clear(),
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
                KeyCode::Char('e') => self.dashboard.toggle_show_exited(),
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
                KeyCode::Char('D') => {
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
                KeyCode::Char('k') => self.logs.clear(),
                KeyCode::Char('/') => self.logs.begin_filter_input(),
                KeyCode::Up => self.logs.scroll_up(1),
                KeyCode::Down => self.logs.scroll_down(1),
                KeyCode::PageUp => self.logs.scroll_up(10),
                KeyCode::PageDown => self.logs.scroll_down(10),
                KeyCode::Char('g') => self.logs.jump_to_top(),
                KeyCode::Char('G') | KeyCode::End => self.logs.jump_to_bottom(),
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
            let mut rows: Vec<super::history::HistoryRow> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            for host in hosts {
                match ops.list_containers_by_label(&host, &label).await {
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
        }
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
                self.push_toast(format!(
                    "✗ logs {}/{container}: {e}",
                    host.address
                ));
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
        let (header_area, pane_area) = super::ui::split_with_header(frame.area());
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
        let tabs = ["Dashboard", "Hosts", "Services", "Logs"];
        let selected_tab = Some(self.view.top_section());
        super::ui::render_header(frame, header_area, &tabs, selected_tab, &crumbs, &right);

        let secrets = self.secrets.try_read().ok().and_then(|g| g.clone());
        match &self.view {
            View::Dashboard => {
                self.dashboard
                    .render(frame, pane_area, &self.config, secrets.as_deref());
            }
            View::Hosts => self.hosts.render(frame, pane_area, &self.config),
            View::HostDetail(_) => {
                self.host_detail
                    .render(frame, pane_area, &self.config, secrets.as_deref());
            }
            View::ContainerDetail { .. } => {
                // Split: top 2/3 = inspect data, bottom 1/3 = live log tail.
                let split = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints([
                        ratatui::layout::Constraint::Min(0),
                        ratatui::layout::Constraint::Length(12),
                    ])
                    .split(pane_area);
                self.container_detail.render(frame, split[0]);
                self.logs.render(frame, split[1], &self.config);
            }
            View::Services => self.services.render(frame, pane_area, &self.config),
            View::ServiceDetail(_) => {
                self.service_detail
                    .render(frame, pane_area, &self.config, secrets.as_deref());
            }
            View::ServiceHistory(_) => {
                self.history.render(frame, pane_area);
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
                " {header}{} ",
                match &progress.finished {
                    None => " (running…)".to_string(),
                    Some(Ok(_)) => " (done — esc to close)".to_string(),
                    Some(Err(_)) => " (failed — esc to close)".to_string(),
                },
            );
            let success = matches!(progress.finished, Some(Ok(_)));
            let failure = matches!(progress.finished, Some(Err(_)));
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
                );
            } else {
                super::ui::render_log_modal(frame, &title, &lines, success, failure);
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_log_streams();
        self.stop_event_subscriptions();
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
    let mut child = match Command::new("hl")
        .arg("--color=always")
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
