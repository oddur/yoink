//! Dashboard pane: auto-refreshing service/host status grid with
//! per-container CPU% and memory snapshots.

use std::collections::HashMap;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::base::bundle::SecretsBundle;
use crate::config::Config;
use crate::docker_ops::{ContainerStats, DockerOps};
use crate::features::output::{format_bytes, format_relative_time};
use crate::features::status::StatusReport;

use super::container_detail::StatsHistory;
use crate::tui::ui::{
    FilterState, bold, clamp_selection, filter_footer, gauge_color, health_style, inline_gauge,
    pane_layout, render_drift_cell, state_style,
};

pub struct DashboardRefresh {
    pub report: Option<StatusReport>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DashboardState {
    report: Option<StatusReport>,
    last_error: Option<String>,
    loaded: bool,
    /// When `true`, render also includes containers in the `exited` /
    /// `created` / `dead` / `restarting` states alongside running ones.
    /// Toggle with `e` from the Dashboard view. Useful for drilling
    /// into the logs of the just-replaced container after a deploy.
    show_exited: bool,
    pub filter: FilterState,
    /// Cursor over the rendered (host, container, service) tuples,
    /// in the same order `build_rows` produces them. Persists across
    /// refreshes — survives a list reorder by clamping to len.
    table: TableState,
}

/// One selectable Dashboard row — what `selected_*` accessors return
/// to the App for routing K / U / Enter actions to the right
/// container.
#[derive(Debug, Clone)]
pub struct DashboardRow {
    pub host: String,
    pub container: String,
    pub service: Option<String>,
}

impl DashboardState {
    pub fn toggle_show_exited(&mut self) {
        self.show_exited = !self.show_exited;
    }

    /// The `yoink.service` label of the container named `container`,
    /// on any host of the last report.
    #[must_use]
    pub fn service_of(&self, container: &str) -> Option<String> {
        self.report
            .as_ref()
            .and_then(|r| {
                r.hosts
                    .iter()
                    .flat_map(|h| h.containers.iter())
                    .find(|c| c.name == container)
            })
            .and_then(|c| c.yoink_service.clone())
    }

    /// Selectable rows in the same order `build_rows` would render
    /// them: filtered, exited-included-or-not, sorted by host then
    /// container. Empty-host placeholder rows are excluded since
    /// there's nothing useful to act on there.
    fn selectable(&self) -> Vec<DashboardRow> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for host in &report.hosts {
            for c in &host.containers {
                if !self.show_exited && !c.is_running() {
                    continue;
                }
                // Hide pf-sidecar containers from the overview — they're
                // ephemeral implementation detail of an active port-forward.
                // The name prefix is the reliable signal (we control it in
                // `pf::unique_sidecar_name`); target service rows get a `↦`
                // marker below, and the footer band carries the URLs.
                if c.name.starts_with("yoink-pf-") {
                    continue;
                }
                let searchable = format!(
                    "{} {} {} {} {} {}",
                    host.host,
                    c.yoink_service.as_deref().unwrap_or(""),
                    c.name,
                    c.state,
                    c.yoink_version.as_deref().unwrap_or(""),
                    c.networks.join(","),
                );
                if !self.filter.matches(&searchable) {
                    continue;
                }
                out.push(DashboardRow {
                    host: host.host.clone(),
                    container: c.name.clone(),
                    service: c.yoink_service.clone(),
                });
            }
        }
        out
    }

    pub fn select_next(&mut self) {
        let n = self.selectable().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.selectable().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    /// Currently-highlighted row, if any. App routes Enter / U / K
    /// through this.
    #[must_use]
    pub fn selected(&self) -> Option<DashboardRow> {
        self.table
            .selected()
            .and_then(|i| self.selectable().into_iter().nth(i))
    }

    /// First non-empty `yoink_version` found for any running
    /// container of `service` across the latest report. Used by the
    /// reconcile flow to fall back to "what's currently deployed"
    /// when the config doesn't pin a tag.
    #[must_use]
    pub fn running_tag_for_service(&self, service: &str) -> Option<String> {
        self.report.as_ref().and_then(|r| {
            r.hosts
                .iter()
                .flat_map(|h| h.containers.iter())
                .find_map(|c| {
                    if c.is_running()
                        && c.yoink_service.as_deref() == Some(service)
                        && c.yoink_version.is_some()
                    {
                        c.yoink_version.clone()
                    } else {
                        None
                    }
                })
        })
    }
}

impl DashboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests + sync paths.
    #[cfg(test)]
    pub async fn refresh(&mut self, ops: &dyn DockerOps, config: &Config) {
        let mut data = fetch(ops, config).await;
        self.keep_failed_hosts(&mut data, config);
        self.apply(data);
    }

    /// On a partial failure, give each host that did not answer its
    /// last-known rows, so a one-tick ssh blip does not hide them. The
    /// error stays in `data`, so the footer still shows it. Call before
    /// [`Self::apply`], and hand the same report to the other screens.
    pub fn keep_failed_hosts(&self, data: &mut DashboardRefresh, config: &Config) {
        if let (Some(report), Some(previous)) = (data.report.as_mut(), &self.report) {
            report.keep_failed_hosts_from(previous, config);
        }
    }

    /// Apply background-fetched results. `loaded` flips
    /// unconditionally — when `report` is `None` (host unreachable,
    /// ssh probe failed, etc.), the existing render path uses
    /// `last_error` to surface the failure instead of looping on
    /// `(loading…)`. Last-known good `report`/`stats` are kept on
    /// error to ride out transient blips (see also
    /// [`Self::keep_failed_hosts`] for a partial failure).
    pub fn apply(&mut self, data: DashboardRefresh) {
        self.last_error = data.error;
        self.loaded = true;
        if let Some(report) = data.report {
            self.report = Some(report);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        config: &Config,
        secrets: Option<&SecretsBundle>,
        history: &HashMap<(String, String), StatsHistory>,
        forwards: &crate::tui::pf::PortForwardState,
        vscode: &crate::tui::vscode::VscodeSessionState,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let layout = pane_layout(area);
        // With a report, a fetch error means some hosts did not answer.
        // It gets its own line above the footer, so the filter prompt
        // and the active filter stay visible.
        let (table_area, failed_area) = match (&self.report, &self.last_error) {
            (Some(_), Some(_)) => {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(0), Constraint::Length(1)])
                    .split(layout[1]);
                (split[0], Some(split[1]))
            }
            _ => (layout[1], None),
        };

        let services = config.service_names().collect::<Vec<_>>().join(", ");
        let header = Paragraph::new(format!(
            "yoink dashboard · services: {services} · ↑↓ select · enter detail · ? help"
        ))
        .style(bold());
        frame.render_widget(header, layout[0]);

        let rows = self.build_rows(config, secrets, history, forwards, vscode, throbber);
        let widths = [
            Constraint::Length(18), // host
            Constraint::Length(14), // service
            Constraint::Length(26), // container
            Constraint::Length(9),  // state
            Constraint::Length(10), // health
            Constraint::Length(10), // version
            Constraint::Length(18), // networks
            Constraint::Length(7),  // drift
            Constraint::Length(20), // cpu% (value + bracketed gauge)
            Constraint::Length(34), // mem (value / limit + bracketed gauge)
            Constraint::Min(20),    // created
        ];
        let selectable_count = self.selectable().len();
        clamp_selection(&mut self.table, selectable_count);
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("service").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("version").style(bold()),
                Cell::from("networks").style(bold()),
                Cell::from("drift").style(bold()),
                Cell::from("cpu").style(bold()),
                Cell::from("mem").style(bold()),
                Cell::from("created").style(bold()),
            ]))
            .row_highlight_style(crate::tui::ui::table_highlight_style())
            .highlight_symbol(crate::tui::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("status"));
        frame.render_stateful_widget(table, table_area, &mut self.table);

        let exited_indicator = if self.show_exited {
            "[exited: on]"
        } else {
            "[exited: off]"
        };
        let help = format!("q quit · r refresh · e {exited_indicator} · ? help");
        if let (Some(err), Some(failed_area)) = (&self.last_error, failed_area) {
            let failed = Paragraph::new(format!("unreachable: {err}"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(failed, failed_area);
        } else if let Some(err) = &self.last_error {
            let footer = Paragraph::new(format!("error: {err}  ·  {help}"))
                .style(Style::default().fg(Color::Red));
            frame.render_widget(footer, layout[2]);
            return;
        }
        let footer = filter_footer(&self.filter, &help);
        frame.render_widget(footer, layout[2]);
    }

    fn build_rows(
        &self,
        config: &Config,
        secrets: Option<&SecretsBundle>,
        history: &HashMap<(String, String), StatsHistory>,
        forwards: &crate::tui::pf::PortForwardState,
        vscode: &crate::tui::vscode::VscodeSessionState,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) -> Vec<Row<'static>> {
        let Some(report) = &self.report else {
            // No cached report. If the first fetch already finished
            // and errored (loaded=true), don't pretend we're still
            // loading — the error footer carries the detail.
            if self.loaded {
                return vec![Row::new(vec![Cell::from(
                    "(no data — see error in footer)",
                )])];
            }
            return vec![Row::new(vec![Cell::from(crate::tui::ui::loading_line(
                throbber,
            ))])];
        };
        let mut rows: Vec<Row<'_>> = Vec::new();
        for host in &report.hosts {
            if host.containers.is_empty() {
                rows.push(Row::new(vec![
                    Cell::from(host.host.clone()),
                    Cell::from("-"),
                    Cell::from("(none)"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                    Cell::from("-"),
                ]));
                continue;
            }
            for c in &host.containers {
                if !self.show_exited && !c.is_running() {
                    continue;
                }
                // Hide pf-sidecar containers from the overview — they're
                // ephemeral implementation detail of an active port-forward.
                // The name prefix is the reliable signal (we control it in
                // `pf::unique_sidecar_name`); target service rows get a `↦`
                // marker below, and the footer band carries the URLs.
                if c.name.starts_with("yoink-pf-") {
                    continue;
                }
                let searchable = format!(
                    "{} {} {} {} {} {}",
                    host.host,
                    c.yoink_service.as_deref().unwrap_or(""),
                    c.name,
                    c.state,
                    c.yoink_version.as_deref().unwrap_or(""),
                    c.networks.join(","),
                );
                if !self.filter.matches(&searchable) {
                    continue;
                }
                let health = c
                    .health_hint()
                    .map_or("-", crate::docker_ops::HealthStatus::as_str);
                let stats = history
                    .get(&(host.host.clone(), c.name.clone()))
                    .and_then(StatsHistory::latest);

                let cpu_cell = render_cpu_cell(stats);
                let mem_cell = render_mem_cell(stats);
                let drift_cell = render_drift_cell(c, config, secrets);

                let service_cell = match c.yoink_service.as_deref() {
                    Some(svc) => {
                        let pf = forwards.is_container_forwarded(&host.host, &c.name)
                            || forwards.is_service_forwarded_unscoped(svc);
                        crate::tui::ui::build_marker_cell(svc, pf, vscode)
                    }
                    None => Cell::from("-"),
                };
                rows.push(Row::new(vec![
                    Cell::from(host.host.clone()),
                    service_cell,
                    Cell::from(c.name.clone()),
                    Cell::from(c.state.as_str()).style(state_style(c.state)),
                    Cell::from(health.to_string()).style(health_style(health)),
                    Cell::from(c.yoink_version.clone().unwrap_or_else(|| "-".into())),
                    Cell::from(format_networks(&c.networks)),
                    drift_cell,
                    cpu_cell,
                    mem_cell,
                    Cell::from(format_relative_time(c.created_unix)),
                ]));
            }
        }
        rows
    }
}

/// Comma-joined network list for the dashboard cell. Truncates with
/// `…` when the joined string would overflow the column width so
/// row alignment stays put on services with many attachments.
const NETWORKS_CELL_MAX: usize = 17;

fn format_networks(networks: &[String]) -> String {
    if networks.is_empty() {
        return "-".into();
    }
    let joined = networks.join(", ");
    if joined.chars().count() <= NETWORKS_CELL_MAX {
        joined
    } else {
        let mut s: String = joined.chars().take(NETWORKS_CELL_MAX - 1).collect();
        s.push('…');
        s
    }
}

/// CPU cell: percentage + bracketed inline gauge. Empty/no-stats
/// containers get a dim "-" so the column alignment doesn't shift.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_cpu_cell(stats: Option<&ContainerStats>) -> Cell<'static> {
    let Some(s) = stats else {
        return Cell::from("-").style(Style::default().fg(Color::DarkGray));
    };
    let pct_text = format!("{:>5.1}% ", s.cpu_pct);
    // Gauge ratio: assume 1 core = 100%; clamps inside `inline_gauge`.
    let ratio = (s.cpu_pct as f32 / 100.0).clamp(0.0, 1.0);
    let mut spans = vec![Span::raw(pct_text)];
    spans.extend(inline_gauge(ratio, 10, gauge_color(ratio)));
    Cell::from(Line::from(spans))
}

/// Mem cell: "used / limit" + bracketed inline gauge against the
/// limit when known. Containers with no limit show usage text only
/// (no gauge — there's nothing to fill against).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_mem_cell(stats: Option<&ContainerStats>) -> Cell<'static> {
    let Some(s) = stats else {
        return Cell::from("-").style(Style::default().fg(Color::DarkGray));
    };
    match s.mem_limit {
        Some(limit) if limit > 0 => {
            let ratio = ((s.mem_used as f32) / (limit as f32)).clamp(0.0, 1.0);
            let label = format!(
                "{:>8} / {:<8} ",
                format_bytes(s.mem_used),
                format_bytes(limit)
            );
            let mut spans = vec![Span::raw(label)];
            spans.extend(inline_gauge(ratio, 10, gauge_color(ratio)));
            Cell::from(Line::from(spans))
        }
        _ => Cell::from(format_bytes(s.mem_used)),
    }
}

/// Background-friendly fetch — owned inputs so the future is `'static + Send`.
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, config: Arc<Config>) -> DashboardRefresh {
    fetch(ops.as_ref(), config.as_ref()).await
}

/// Just the container listing — live CPU/Mem stats arrive separately
/// via the always-on stats poller and live in `App::container_history`.
/// This keeps the docker daemon from being asked for the same stats
/// twice on every fast tick.
async fn fetch(ops: &dyn DockerOps, config: &Config) -> DashboardRefresh {
    match StatusReport::collect(ops, config).await {
        Ok(r) => {
            let error = (!r.failed.is_empty()).then(|| {
                r.failed
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" · ")
            });
            DashboardRefresh {
                report: Some(r),
                error,
            }
        }
        Err(e) => DashboardRefresh {
            report: None,
            error: Some(format!("{e:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{ContainerInfo, FakeDockerOps};
    use std::collections::BTreeMap;

    fn config() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
services:
  - name: app-a
    image: x
    tag: v1
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap()
    }

    fn config_two_hosts() -> Config {
        Config::parse_str(
            r#"
hosts:
  - { address: host-a, user: deploy }
  - { address: host-b, user: deploy }
services:
  - name: app-a
    image: x
    tag: v1
    run: { port: 3000, healthcheck_path: /health }
"#,
        )
        .unwrap()
    }

    fn running_container() -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: "app-a-a1b2c3d".into(),
            image: String::new(),
            state: "running".into(),
            status_text: "Up 1h (healthy)".into(),
            created_unix: Some(1_735_128_000),
            yoink_service: Some("app-a".into()),
            yoink_version: Some("a1b2c3d".into()),
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_populates_report() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        // Stats fetching no longer happens here — the always-on
        // poller in App owns it. Refresh just collects the listing.
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.report.is_some());
    }

    #[tokio::test]
    async fn refresh_captures_error_into_last_error_field() {
        let ops = FakeDockerOps::new();
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.last_error.is_some());
        assert!(state.report.is_none());
    }

    #[tokio::test]
    async fn refresh_shows_reachable_hosts_and_reports_unreachable_one() {
        let cfg = config_two_hosts();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        ops.push_list_containers(Err(crate::docker_ops::DockerError::SshProbe {
            host: "host-b".into(),
            detail: "connection refused".into(),
        }));
        let mut state = DashboardState::new();
        state.refresh(&ops, &cfg).await;
        let report = state.report.as_ref().expect("reachable host is shown");
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].host, "host-a");
        let err = state
            .last_error
            .as_deref()
            .expect("unreachable host is reported");
        assert!(err.contains("host-b"), "{err}");
    }

    fn render_to_text(state: &mut DashboardState, cfg: &Config) -> String {
        let backend = ratatui::backend::TestBackend::new(200, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let forwards = crate::tui::pf::PortForwardState::default();
        let vscode = crate::tui::vscode::VscodeSessionState::default();
        let throbber = throbber_widgets_tui::ThrobberState::default();
        terminal
            .draw(|frame| {
                state.render(
                    frame,
                    frame.area(),
                    cfg,
                    None,
                    &HashMap::new(),
                    &forwards,
                    &vscode,
                    &throbber,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Regression: while one host was down, the footer showed only the
    /// error. The `/` prompt and the active filter were hidden. The
    /// unreachable hosts now get their own line above the footer.
    #[tokio::test]
    async fn a_down_host_does_not_hide_the_filter_footer() {
        let cfg = config_two_hosts();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        ops.push_list_containers(Err(crate::docker_ops::DockerError::SshProbe {
            host: "host-b".into(),
            detail: "connection refused".into(),
        }));
        let mut state = DashboardState::new();
        state.refresh(&ops, &cfg).await;
        assert!(state.last_error.is_some());

        let text = render_to_text(&mut state, &cfg);
        assert!(text.contains("/ filter"), "{text}");
        assert!(text.contains("unreachable:"), "{text}");
        assert!(text.contains("host-b"), "{text}");

        state.filter.begin_input();
        state.filter.push_char('a');
        let text = render_to_text(&mut state, &cfg);
        assert!(text.contains("/a_"), "{text}");
        state.filter.apply();
        let text = render_to_text(&mut state, &cfg);
        assert!(text.contains("filter: a"), "{text}");
        assert!(text.contains("host-b"), "{text}");
    }

    /// Regression: on a partial failure the new report replaced the
    /// last-known one, so a one-tick ssh blip made the rows of the
    /// failed host vanish. Its rows stay and the footer shows the error.
    #[tokio::test]
    async fn partial_failure_keeps_the_rows_of_the_failed_host() {
        let cfg = config_two_hosts();
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        ops.push_list_containers(Ok(vec![running_container()]));
        let mut state = DashboardState::new();
        state.refresh(&ops, &cfg).await;

        ops.push_list_containers(Err(crate::docker_ops::DockerError::SshProbe {
            host: "host-a".into(),
            detail: "connection refused".into(),
        }));
        ops.push_list_containers(Ok(vec![]));
        state.refresh(&ops, &cfg).await;

        let report = state.report.as_ref().expect("report is kept");
        let hosts: Vec<(&str, usize)> = report
            .hosts
            .iter()
            .map(|h| (h.host.as_str(), h.containers.len()))
            .collect();
        assert_eq!(hosts, vec![("host-a", 1), ("host-b", 0)]);
        let err = state.last_error.as_deref().expect("the error shows");
        assert!(err.contains("host-a"), "{err}");
    }
}
