//! Detail view for a single host. Lists every running container on the
//! host (regardless of yoink labels) with live CPU/mem stats. Up/Down
//! selects; Enter opens that container's live log stream.

use std::collections::HashMap;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, LineGauge, Paragraph, Row, Table, TableState};

use crate::config::Config;
use crate::docker_ops::{ContainerInfo, ContainerStats, DockerOps, Host, HostInfo};
use crate::output::format_bytes;
use crate::secrets::SecretsBundle;

use super::container_detail::StatsHistory;
use super::ui::{
    FilterState, bold, clamp_selection, filter_footer, gauge_color, health_style, inline_gauge,
    render_drift_cell, short_image, state_style,
};

pub struct HostDetailRefresh {
    pub containers: Vec<ContainerInfo>,
    pub host_info: Option<HostInfo>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct HostDetailState {
    host: Option<Host>,
    containers: Vec<ContainerInfo>,
    host_info: Option<HostInfo>,
    last_error: Option<String>,
    table: TableState,
    loaded: bool,
    pub filter: FilterState,
}

impl HostDetailState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch this pane to a new host. Caller is expected to follow with
    /// `refresh` (or schedule one).
    pub fn set_host(&mut self, host: Host) {
        if self.host.as_ref() != Some(&host) {
            self.containers.clear();
            self.table.select(None);
            self.loaded = false;
        }
        self.host = Some(host);
    }

    /// Convenience for tests + sync paths.
    #[cfg(test)]
    pub async fn refresh(&mut self, ops: &dyn DockerOps) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let data = fetch(ops, &host).await;
        self.apply(data);
    }

    /// Apply background-fetched results, preserving selection where possible.
    /// Always flips `loaded = true` — even on error — so the pane
    /// renders an error footer instead of looping on `(loading…)`.
    /// Last-known good `containers`/`stats`/`host_info` are kept on
    /// error so a transient blip doesn't blank the table.
    pub fn apply(&mut self, data: HostDetailRefresh) {
        self.last_error = data.error;
        self.loaded = true;
        if self.last_error.is_none() {
            self.containers = data.containers;
            self.host_info = data.host_info;
        }
        // Clamp against the *visible* count, not the unfiltered total.
        // With an active filter, `table.selected()` indexes into the
        // `visible_indices()` slice (see `render`), so clamping to the
        // larger unfiltered count would let `selected` point past the
        // filtered list for a frame.
        let n = self.visible_indices().len();
        clamp_selection(&mut self.table, n);
    }

    pub fn select_next(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(n - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.visible_indices().is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    /// Currently-selected container name, if any.
    pub fn selected_container(&self) -> Option<String> {
        let visible = self.visible_indices();
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .and_then(|src| self.containers.get(src))
            .map(|c| c.name.clone())
    }

    /// `yoink.service` label of the currently-selected container.
    /// `None` for non-yoink-managed containers (no label).
    pub fn selected_service(&self) -> Option<String> {
        let visible = self.visible_indices();
        self.table
            .selected()
            .and_then(|i| visible.get(i).copied())
            .and_then(|src| self.containers.get(src))
            .and_then(|c| c.yoink_service.clone())
    }

    /// Indices into `self.containers` that pass the current filter
    /// (by index so the borrow ends with the call — needed because
    /// `render_stateful_widget` later wants `&mut self.table`).
    fn visible_indices(&self) -> Vec<usize> {
        self.containers
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                // Hide pf-sidecar containers from the host detail
                // pane — same rationale as the dashboard. The name
                // prefix is the reliable signal (we control it
                // in `pf::unique_sidecar_name`); the `yoink.kind`
                // label isn't extracted into ContainerInfo yet.
                if c.name.starts_with("yoink-pf-") {
                    return None;
                }
                let searchable = format!(
                    "{} {} {} {}",
                    c.name,
                    c.yoink_service.as_deref().unwrap_or(""),
                    c.state,
                    c.status_text,
                );
                self.filter.matches(&searchable).then_some(i)
            })
            .collect()
    }

    pub fn host(&self) -> Option<&Host> {
        self.host.as_ref()
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        config: &Config,
        secrets: Option<&SecretsBundle>,
        events: &[String],
        history: &HashMap<(String, String), StatsHistory>,
        forwards: &super::pf::PortForwardState,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        // 5-section layout: header · summary · containers (Min) · events (Length 8 when present) · footer.
        // The event panel collapses to 0 when no events are recorded yet.
        let event_height: u16 = if events.is_empty() { 0 } else { 8 };
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(5),
                Constraint::Min(0),
                Constraint::Length(event_height),
                Constraint::Length(1),
            ])
            .split(area);
        let header_area = layout[0];
        let summary_area = layout[1];
        let table_area = layout[2];
        let events_area = layout[3];
        let footer_area = layout[4];

        let header_text = match &self.host {
            Some(h) => format!(
                "yoink host · {}@{} · ↑↓ select · enter for logs",
                h.user, h.address
            ),
            None => "yoink host · (no host selected)".into(),
        };
        let header = Paragraph::new(header_text).style(bold());
        frame.render_widget(header, header_area);

        self.render_summary(frame, summary_area, history, throbber);

        let widths = [
            Constraint::Length(14), // service
            Constraint::Length(28), // container
            Constraint::Length(28), // image
            Constraint::Length(22), // networks
            Constraint::Length(7),  // drift
            Constraint::Length(20), // status
            Constraint::Length(10), // state
            Constraint::Length(10), // health
            Constraint::Length(20), // cpu (value + bracketed gauge)
            Constraint::Min(28),    // mem (value/limit + bracketed gauge)
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());
        let rows: Vec<Row<'_>> = if !self.loaded && self.last_error.is_none() {
            vec![Row::new(vec![Cell::from(super::ui::loading_line(throbber))])]
        } else if visible.is_empty() && !self.containers.is_empty() {
            vec![Row::new(vec![Cell::from("(no containers match filter)")])]
        } else if self.containers.is_empty() {
            vec![Row::new(vec![Cell::from(
                self.last_error
                    .as_deref()
                    .unwrap_or("(no running containers)"),
            )])]
        } else {
            visible
                .iter()
                .map(|i| {
                    let c = &self.containers[*i];
                    let host_addr = self
                        .host
                        .as_ref()
                        .map(|h| h.address.clone())
                        .unwrap_or_default();
                    let stats = history
                        .get(&(host_addr.clone(), c.name.clone()))
                        .and_then(StatsHistory::latest);
                    let health = c.health_hint().unwrap_or("-");
                    let service_cell = match c.yoink_service.as_deref() {
                        Some(svc)
                            if forwards.is_container_forwarded(&host_addr, &c.name)
                                || forwards.is_service_forwarded_unscoped(svc) =>
                        {
                            Cell::from(format!("↦ {svc}")).style(
                                ratatui::style::Style::default().fg(ratatui::style::Color::Cyan),
                            )
                        }
                        Some(svc) => Cell::from(svc.to_string()),
                        None => Cell::from("-"),
                    };
                    Row::new(vec![
                        service_cell,
                        Cell::from(c.name.clone()),
                        Cell::from(short_image(&c.image)),
                        Cell::from(if c.networks.is_empty() {
                            "-".to_string()
                        } else {
                            c.networks.join(", ")
                        }),
                        render_drift_cell(c, config, secrets),
                        Cell::from(c.status_text.clone()),
                        Cell::from(c.state.clone()).style(state_style(&c.state)),
                        Cell::from(health.to_string()).style(health_style(health)),
                        cell_cpu(stats),
                        cell_mem(stats),
                    ])
                })
                .collect()
        };
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("service").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("image").style(bold()),
                Cell::from("networks").style(bold()),
                Cell::from("drift").style(bold()),
                Cell::from("status").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("cpu (cores)").style(bold()),
                Cell::from("memory").style(bold()),
            ]))
            .row_highlight_style(super::ui::table_highlight_style())
            .highlight_symbol(super::ui::TABLE_HIGHLIGHT_SYMBOL)
            .block(Block::default().borders(Borders::ALL).title("containers"));
        frame.render_stateful_widget(table, table_area, &mut self.table);

        if let Some(err) = &self.last_error {
            let footer = Paragraph::new(format!(
                "error: {err}  ·  q quit · esc back · enter logs · r refresh"
            ))
            .style(Style::default().fg(Color::Red));
            frame.render_widget(footer, footer_area);
            return;
        }
        let footer = filter_footer(
            &self.filter,
            "↑↓ select · enter logs · i inspect · ! shell · D debug · S start · X stop · R restart · K kill · U reconcile · r refresh",
        );
        frame.render_widget(footer, footer_area);

        if !events.is_empty() && event_height > 0 {
            Self::render_events(frame, events_area, events);
        }
    }

    /// Bottom-of-pane scrolling event log showing the most recent
    /// docker events for this host (start / die / health-change /
    /// network create / volume mount …). Newest at the top — same
    /// reading order as `docker events --since` truncated to the
    /// most operator-relevant `EVENT_HISTORY_PER_HOST` lines.
    fn render_events(frame: &mut Frame<'_>, area: ratatui::layout::Rect, events: &[String]) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " events (newest first) ",
                Style::default().fg(Color::Gray),
            ));
        let inner_h = block.inner(area).height as usize;
        let lines: Vec<Line<'static>> = events
            .iter()
            .rev()
            .take(inner_h.max(1))
            .map(|s| Line::from(s.clone()))
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// Host-aggregate summary panel: bordered block hosting two
    /// `LineGauge` rows (CPU / Mem) so the operator can see total
    /// load at a glance without scanning every container row.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_lossless
    )]
    fn render_summary(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        history: &HashMap<(String, String), StatsHistory>,
        throbber: &throbber_widgets_tui::ThrobberState,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" host summary ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // CPU LineGauge
                Constraint::Length(1), // Mem LineGauge
                Constraint::Length(1), // System info line
            ])
            .split(inner);

        // Sum across the latest stats sample of every container we
        // know is on this host. The history store is keyed by
        // (host_address, container_name) so we filter to the rows that
        // belong to the current host.
        let host_addr = self
            .host
            .as_ref()
            .map(|h| h.address.as_str())
            .unwrap_or_default();
        let host_stats: Vec<&ContainerStats> = self
            .containers
            .iter()
            .filter_map(|c| history.get(&(host_addr.to_string(), c.name.clone())))
            .filter_map(StatsHistory::latest)
            .collect();
        let cpu_total: f32 = host_stats.iter().map(|s| s.cpu_pct as f32).sum();
        let mem_total: u64 = host_stats.iter().map(|s| s.mem_used.max(0) as u64).sum();
        let n_cpu = self
            .host_info
            .as_ref()
            .and_then(|i| i.n_cpu)
            .unwrap_or(1)
            .max(1);
        let mem_total_host = self
            .host_info
            .as_ref()
            .and_then(|i| i.mem_total)
            .unwrap_or(0);

        // Scale CPU against the host's actual core count when known —
        // a single core's worth on a 16-core box should be ~6%, not
        // 100%. Falls back to 1 core when host info hasn't loaded yet.
        let cpu_ratio = (cpu_total / (n_cpu as f32 * 100.0)).clamp(0.0, 1.0) as f64;
        let cpu_label = format!("CPU  {cpu_total:>6.1}% / {n_cpu} cores");

        let (mem_label, mem_ratio) = if mem_total_host > 0 {
            let ratio = (mem_total as f32 / mem_total_host as f32).clamp(0.0, 1.0) as f64;
            (
                format!(
                    "MEM  {} / {}",
                    format_bytes(i64::try_from(mem_total).unwrap_or(i64::MAX)),
                    format_bytes(mem_total_host),
                ),
                ratio,
            )
        } else {
            (
                format!(
                    "MEM  {}",
                    format_bytes(i64::try_from(mem_total).unwrap_or(i64::MAX))
                ),
                0.0,
            )
        };

        frame.render_widget(line_gauge(cpu_label, cpu_ratio), chunks[0]);
        frame.render_widget(line_gauge(mem_label, mem_ratio), chunks[1]);

        // Third row: kernel + OS + container counts. Dim so it sits
        // quietly under the gauges.
        match &self.host_info {
            Some(i) => {
                let os = i.operating_system.as_deref().unwrap_or("?");
                let kernel = i.kernel.as_deref().unwrap_or("?");
                let running = i.containers_running.unwrap_or(0);
                let total = i.containers.unwrap_or(0);
                let images = i.images.unwrap_or(0);
                let info_text = format!(
                    "{os} · {kernel} · {running}/{total} containers · {images} images"
                );
                frame.render_widget(
                    Paragraph::new(info_text).style(Style::default().fg(Color::DarkGray)),
                    chunks[2],
                );
            }
            None => {
                frame.render_widget(
                    Paragraph::new(super::ui::throbber_with_label(throbber, " host info…")),
                    chunks[2],
                );
            }
        }
    }
}

/// Build a `LineGauge` with a bright-gray unfilled track so the
/// bounds (start of bar, position of "100%") stay visible even on
/// terminals where `DarkGray` melts into the background.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn line_gauge(label: String, ratio: f64) -> LineGauge<'static> {
    let color = gauge_color(ratio as f32);
    LineGauge::default()
        .label(Line::from(Span::raw(label)))
        .ratio(ratio.clamp(0.0, 1.0))
        .filled_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .unfilled_style(Style::default().fg(Color::Gray))
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn cell_cpu(stats: Option<&ContainerStats>) -> Cell<'static> {
    let Some(s) = stats else {
        return Cell::from("-").style(Style::default().fg(Color::DarkGray));
    };
    let pct = s.cpu_pct as f32;
    let ratio = (pct / 100.0).clamp(0.0, 1.0);
    let mut spans = vec![Span::raw(format!("{pct:>5.1}% "))];
    spans.extend(inline_gauge(ratio, 10, gauge_color(ratio)));
    Cell::from(Line::from(spans))
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn cell_mem(stats: Option<&ContainerStats>) -> Cell<'static> {
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
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, host: Host) -> HostDetailRefresh {
    fetch(ops.as_ref(), &host).await
}

/// List running containers + host info, in parallel. Live stats are
/// owned by the App-level always-on poller and read from
/// `App::container_history` at render time.
async fn fetch(ops: &dyn DockerOps, host: &Host) -> HostDetailRefresh {
    let (containers_res, host_info_res) =
        tokio::join!(ops.list_running_containers(host), ops.host_info(host));
    match containers_res {
        Ok(containers) => HostDetailRefresh {
            containers,
            host_info: host_info_res.ok(),
            error: None,
        },
        Err(e) => HostDetailRefresh {
            containers: Vec::new(),
            host_info: None,
            error: Some(format!("{e:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::FakeDockerOps;
    use std::collections::BTreeMap;

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
        }
    }

    fn container(name: &str) -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: name.into(),
            image: String::new(),
            state: "running".into(),
            status_text: "Up 1h (healthy)".into(),
            created_unix: None,
            yoink_service: None,
            yoink_version: None,
            yoink_spec_hash: None,
            yoink_deployed_by: None,
            yoink_deployed_at: None,
            networks: Vec::new(),
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_populates_containers() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container("a"), container("b")]));
        // Stats are owned by the always-on poller now; refresh just
        // collects the listing.
        let mut state = HostDetailState::new();
        state.set_host(host());
        state.refresh(&ops).await;
        assert_eq!(state.containers.len(), 2);
        assert_eq!(state.selected_container().as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn select_next_and_prev_clamp_to_bounds() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container("a"), container("b")]));
        let mut state = HostDetailState::new();
        state.set_host(host());
        state.refresh(&ops).await;

        state.select_next();
        assert_eq!(state.selected_container().as_deref(), Some("b"));
        state.select_next(); // clamp at last
        assert_eq!(state.selected_container().as_deref(), Some("b"));
        state.select_prev();
        assert_eq!(state.selected_container().as_deref(), Some("a"));
        state.select_prev(); // clamp at 0
        assert_eq!(state.selected_container().as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn refresh_captures_list_error() {
        let ops = FakeDockerOps::new();
        let mut state = HostDetailState::new();
        state.set_host(host());
        state.refresh(&ops).await;
        assert!(state.last_error.is_some());
    }
}
