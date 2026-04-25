//! Detail view for a single host. Lists every running container on the
//! host (regardless of yoink labels) with live CPU/mem stats. Up/Down
//! selects; Enter opens that container's live log stream.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, LineGauge, Paragraph, Row, Table, TableState};

use crate::docker_ops::{ContainerInfo, ContainerStats, DockerOps, Host};
use crate::output::format_bytes;

use super::ui::{
    bold, clamp_selection, filter_footer, gauge_color, health_style, inline_gauge, state_style,
    FilterState,
};

pub struct HostDetailRefresh {
    pub containers: Vec<ContainerInfo>,
    pub stats: HashMap<String, ContainerStats>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct HostDetailState {
    host: Option<Host>,
    containers: Vec<ContainerInfo>,
    stats: HashMap<String, ContainerStats>,
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
            self.stats.clear();
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
    pub fn apply(&mut self, data: HostDetailRefresh) {
        self.last_error = data.error;
        if self.last_error.is_none() {
            self.containers = data.containers;
            self.stats = data.stats;
            self.loaded = true;
        }
        clamp_selection(&mut self.table, self.containers.len());
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

    /// Indices into `self.containers` that pass the current filter
    /// (by index so the borrow ends with the call — needed because
    /// `render_stateful_widget` later wants `&mut self.table`).
    fn visible_indices(&self) -> Vec<usize> {
        self.containers
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
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

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        // 4-section layout: header (1) · summary panel (4) · table (rest) · footer (1).
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);
        let header_area = layout[0];
        let summary_area = layout[1];
        let table_area = layout[2];
        let footer_area = layout[3];

        let header_text = match &self.host {
            Some(h) => format!(
                "yoink host · {}@{} · ↑↓ select · enter for logs",
                h.user, h.address
            ),
            None => "yoink host · (no host selected)".into(),
        };
        let header = Paragraph::new(header_text).style(bold());
        frame.render_widget(header, header_area);

        self.render_summary(frame, summary_area);

        let widths = [
            Constraint::Length(14), // service
            Constraint::Length(28), // container
            Constraint::Length(32), // image
            Constraint::Length(22), // status
            Constraint::Length(10), // state
            Constraint::Length(10), // health
            Constraint::Length(20), // cpu (value + bracketed gauge)
            Constraint::Min(28),    // mem (value/limit + bracketed gauge)
        ];
        let visible = self.visible_indices();
        clamp_selection(&mut self.table, visible.len());
        let rows: Vec<Row<'_>> = if !self.loaded && self.last_error.is_none() {
            vec![Row::new(vec![Cell::from("(loading…)")])]
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
                    let stats = self.stats.get(&c.name);
                    let health = c.health_hint().unwrap_or("-");
                    Row::new(vec![
                        Cell::from(c.yoink_service.clone().unwrap_or_else(|| "-".into())),
                        Cell::from(c.name.clone()),
                        Cell::from(short_image(&c.image)),
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
            "q quit · esc back · ↑↓ select · enter logs · ! shell · D debug · r refresh",
        );
        frame.render_widget(footer, footer_area);
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
    fn render_summary(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" host summary ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(inner);

        let cpu_total: f32 = self.stats.values().map(|s| s.cpu_pct as f32).sum();
        let mem_total: u64 = self.stats.values().map(|s| s.mem_used.max(0) as u64).sum();

        // CPU scale is one core-worth (100%) by default; if the host
        // is busier than that the bar fills then clips — that's the
        // signal the operator wants ("we're past one core's worth").
        let cpu_ratio = (cpu_total / 100.0).clamp(0.0, 1.0) as f64;
        let cpu_label = format!("CPU  {cpu_total:>6.1}%");

        let mem_label = format!("MEM  {}", format_bytes(i64::try_from(mem_total).unwrap_or(i64::MAX)));
        // Mem ratio needs a denominator we don't have at host level
        // here — the per-container `mem_limit`s sum is meaningless.
        // Show the bar against host RAM by leaving it at zero unless
        // we surface host-total memory in a future refresh.
        let mem_ratio: f64 = 0.0;

        frame.render_widget(line_gauge(cpu_label, cpu_ratio), chunks[0]);
        frame.render_widget(line_gauge(mem_label, mem_ratio), chunks[1]);
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

/// Squeeze a docker image reference into a column-friendly form:
/// drop the registry prefix (everything up to the last `/`) and
/// truncate `sha256:…` digests to 12 chars. `caddy/4db05qgnlk.registry.depot.dev/…`
/// becomes `caddy:tag`; `sha256:abcdef…` becomes `sha256:abcdef…` truncated.
fn short_image(image: &str) -> String {
    if image.is_empty() {
        return "-".into();
    }
    if let Some(rest) = image.strip_prefix("sha256:") {
        let head: String = rest.chars().take(12).collect();
        return format!("sha256:{head}");
    }
    let (path, tag) = image.split_once('@').unwrap_or_else(|| {
        image.rsplit_once(':').map_or((image, ""), |(p, t)| (p, t))
    });
    let last = path.rsplit('/').next().unwrap_or(path);
    if tag.is_empty() {
        last.to_string()
    } else if tag.starts_with("sha256:") {
        let head: String = tag.chars().take(19).collect();
        format!("{last}@{head}…")
    } else {
        format!("{last}:{tag}")
    }
}

/// Background-friendly fetch — owned inputs so the future is `'static + Send`.
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, host: Host) -> HostDetailRefresh {
    fetch(ops.as_ref(), &host).await
}

async fn fetch(ops: &dyn DockerOps, host: &Host) -> HostDetailRefresh {
    let containers = match ops.list_running_containers(host).await {
        Ok(c) => c,
        Err(e) => {
            return HostDetailRefresh {
                containers: Vec::new(),
                stats: HashMap::new(),
                error: Some(format!("{e:#}")),
            };
        }
    };
    let stat_futs = containers.iter().map(|c| {
        let name = c.name.clone();
        async move { (name.clone(), ops.container_stats(host, &name).await) }
    });
    let stats: HashMap<String, ContainerStats> = join_all(stat_futs)
        .await
        .into_iter()
        .filter_map(|(name, r)| r.ok().map(|s| (name, s)))
        .collect();
    HostDetailRefresh {
        containers,
        stats,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{ContainerStats, FakeDockerOps};
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
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_populates_containers_and_stats() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container("a"), container("b")]));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 100.0,
            mem_used: 64 * 1024 * 1024,
            mem_limit: None,
        }));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 50.0,
            mem_used: 32 * 1024 * 1024,
            mem_limit: None,
        }));

        let mut state = HostDetailState::new();
        state.set_host(host());
        state.refresh(&ops).await;
        assert_eq!(state.containers.len(), 2);
        assert_eq!(state.stats.len(), 2);
        assert_eq!(state.selected_container().as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn select_next_and_prev_clamp_to_bounds() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![container("a"), container("b")]));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 0.0,
            mem_used: 0,
            mem_limit: None,
        }));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 0.0,
            mem_used: 0,
            mem_limit: None,
        }));
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
