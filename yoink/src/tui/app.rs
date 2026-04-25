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
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
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

use super::dashboard::{self, DashboardRefresh, DashboardState};
use super::host_detail::{self, HostDetailRefresh, HostDetailState};
use super::hosts::{self, HostRow, HostsState};
use super::logs::{LogsState, RenderedLine};
use super::services::{ServiceDetailState, ServicesState};
use super::container_detail::{self, ContainerDetailRefresh, ContainerDetailState};
use super::shell::{SessionResult, ShellState};

/// Backstop polling cadence when no `docker events` push lands. The
/// realtime updates ride on the events stream (see `subscribe_host_events`);
/// this tick exists so the UI converges even if events are filtered out
/// or the stream drops.
const FAST_TICK: Duration = Duration::from_secs(2);
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
    Logs,
    ContainerLogs { host: Host, container: String },
    /// Container detail: labels, state, version, live CPU/mem.
    ContainerDetail { host: Host, container: String },
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
            View::Services | View::ServiceDetail(_) => 2,
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
                "  /            filter substring · esc to clear",
                "  r            refresh",
                "  esc          back to hosts (when no active filter)",
            ],
            View::ContainerDetail { .. } => vec![
                "container detail",
                "  enter / l    live logs",
                "  !            shell · D debug sidecar",
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
                "  /            filter substring · esc to clear",
                "  r            refresh · esc back",
            ],
            View::Logs => vec![
                "logs (multiplexed)",
                "  /            filter substring",
                "  ↑↓ / PgUp PgDn  scroll · g top · G bottom",
                "  k            clear · r restart streams",
            ],
            View::ContainerLogs { .. } => vec![
                "container logs",
                "  /            filter substring",
                "  ↑↓ / PgUp PgDn  scroll · g top · G bottom",
                "  !            shell · D debug sidecar",
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
    /// `["yoink", "Hosts", "backtrack-eu-1", "bt-api-xyz", "shell"]`.
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
    /// Push notification from a host's `docker events` stream. Triggers
    /// an immediate refresh of whichever pane is currently visible.
    Event {
        host: Host,
        event: DockerEvent,
    },
}

pub async fn run(
    config: &Config,
    config_path: std::path::PathBuf,
    ops: Arc<dyn DockerOps>,
    mode: Mode,
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
    let mut terminal = setup_terminal().context("setup terminal")?;
    let result = run_loop(
        &mut terminal,
        Arc::new(config.clone()),
        config_path,
        ops,
        mode,
        hl_available,
    )
    .await;
    let restore = restore_terminal(&mut terminal);
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

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("init terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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
    pub container_detail: ContainerDetailState,
    pub logs: LogsState,
    /// `Some` while a `ContainerShell` view is active; cleared on exit.
    shell: Option<ShellState>,
    /// `?` toggles a modal help overlay listing keybinds for the
    /// current view. Cleared on Esc and on any view transition.
    show_help: bool,
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
        Self {
            view,
            config,
            ops,
            dashboard: DashboardState::new(),
            hosts: HostsState::new(),
            host_detail: HostDetailState::new(),
            services: ServicesState::new(),
            service_detail: ServiceDetailState::new(),
            container_detail: ContainerDetailState::new(),
            logs: LogsState::new(),
            shell: None,
            show_help: false,
            shell_bytes_tx,
            shell_bytes_rx,
            shell_session_tx,
            shell_session_rx,
            hosts_in_flight: false,
            host_detail_in_flight: false,
            dashboard_in_flight: false,
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
        let new_config = match Config::load_from_path(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, path = %self.config_path.display(), "config reload failed");
                return;
            }
        };
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
                KeyCode::Esc => self.transition(View::Hosts).await,
                KeyCode::Char('r') => self.schedule_host_detail_refresh(),
                _ => {}
            },
            View::ContainerDetail { host, container } => match key.code {
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
                KeyCode::Char('r') => self.schedule_dashboard_refresh(),
                KeyCode::Char('e') => self.dashboard.toggle_show_exited(),
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
                KeyCode::Esc => self.transition(View::Services).await,
                KeyCode::Char('r') => self.schedule_dashboard_refresh(),
                _ => {}
            },
            View::ContainerShell { .. } => {} // handled above
            View::Logs => match key.code {
                KeyCode::Char('r') => {
                    self.stop_log_streams();
                    self.start_service_log_streams().await;
                }
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
        if matches!(key.code, KeyCode::Char('c'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
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
                self.container_detail.set_target(host.clone(), container.clone());
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
        }
        self.view = new_view;
    }

    /// The background `start_debug_sidecar` / `exec_interactive` task
    /// returned. Drop the result if the user has navigated away from
    /// the matching shell view (sidecar is `auto_remove` so it'll get
    /// reaped on its own); otherwise wire it into the shell panel or
    /// surface the error.
    fn apply_shell_session(
        &mut self,
        origin_view: &View,
        result: SessionResult,
        banner: &str,
    ) {
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
        if exit_shell
            && let View::ContainerShell { host, .. } = self.view.clone()
        {
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
        let Some((host, container)) = self.container_detail.target().cloned() else {
            return;
        };
        let ops = self.ops.clone();
        let tx = self.update_tx.clone();
        tokio::spawn(async move {
            let data =
                container_detail::fetch_owned(ops, host.clone(), container.clone()).await;
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
            Update::Event { host, event } => self.on_docker_event(&host, &event),
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

    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let (header_area, pane_area) = super::ui::split_with_header(frame.area());
        let crumbs = self.view.breadcrumb();
        let right = format!(
            "{} hosts · {} services",
            self.config.hosts.len(),
            self.config.services.len()
        );
        let tabs = ["Dashboard", "Hosts", "Services", "Logs"];
        let selected_tab = Some(self.view.top_section());
        super::ui::render_header(frame, header_area, &tabs, selected_tab, &crumbs, &right);

        match &self.view {
            View::Dashboard => self.dashboard.render(frame, pane_area, &self.config),
            View::Hosts => self.hosts.render(frame, pane_area, &self.config),
            View::HostDetail(_) => self.host_detail.render(frame, pane_area),
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
            View::ServiceDetail(_) => self.service_detail.render(frame, pane_area),
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
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_log_streams();
        self.stop_event_subscriptions();
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
