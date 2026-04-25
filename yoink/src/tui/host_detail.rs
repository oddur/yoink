//! Detail view for a single host. Lists every running container on the
//! host (regardless of yoink labels) with live CPU/mem stats. Up/Down
//! selects; Enter opens that container's live log stream.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, LineGauge, Paragraph, Row, Sparkline, Table, TableState};

use crate::docker_ops::{ContainerInfo, ContainerStats, DockerOps, Host};
use crate::output::format_bytes;

use super::ui::{bold, clamp_selection, health_style, state_style};

const HISTORY_LEN: usize = 60;

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
    /// Ring buffer of host-aggregate (`cpu_pct_total`,
    /// `mem_bytes_total`) samples — drives the `LineGauge` + Sparkline
    /// summary panel above
    /// the container table.
    history: VecDeque<(f32, f32)>,
    last_error: Option<String>,
    table: TableState,
    loaded: bool,
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
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn apply(&mut self, data: HostDetailRefresh) {
        self.last_error = data.error;
        if self.last_error.is_none() {
            self.containers = data.containers;
            self.stats = data.stats;
            // Snapshot host-aggregate (sum of containers) for the
            // top summary panel's LineGauge + Sparkline.
            let cpu_total: f32 = self.stats.values().map(|s| s.cpu_pct as f32).sum();
            let mem_total: f32 = self.stats.values().map(|s| s.mem_used as f32).sum();
            self.history.push_back((cpu_total, mem_total));
            if self.history.len() > HISTORY_LEN {
                self.history.pop_front();
            }
            self.loaded = true;
        }
        clamp_selection(&mut self.table, self.containers.len());
    }

    pub fn select_next(&mut self) {
        if self.containers.is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table
            .select(Some((i + 1).min(self.containers.len() - 1)));
    }

    pub fn select_prev(&mut self) {
        if self.containers.is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(1)));
    }

    /// Currently-selected container name, if any.
    pub fn selected_container(&self) -> Option<String> {
        self.table
            .selected()
            .and_then(|i| self.containers.get(i))
            .map(|c| c.name.clone())
    }

    pub fn host(&self) -> Option<&Host> {
        self.host.as_ref()
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        // 4-section layout: header (1) · summary panel (6) · table (rest) · footer (1).
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(6),
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
            Constraint::Length(28), // status
            Constraint::Length(10), // state
            Constraint::Length(10), // health
            Constraint::Length(11), // cpu (cores)
            Constraint::Min(20),    // mem
        ];
        let rows: Vec<Row<'_>> = if !self.loaded && self.last_error.is_none() {
            vec![Row::new(vec![Cell::from("(loading…)")])]
        } else if self.containers.is_empty() {
            vec![Row::new(vec![Cell::from(
                self.last_error
                    .as_deref()
                    .unwrap_or("(no running containers)"),
            )])]
        } else {
            self.containers
                .iter()
                .map(|c| {
                    let stats = self.stats.get(&c.name);
                    let cpu =
                        stats.map_or_else(|| "-".into(), |s| format!("{:.2}", s.cpu_pct / 100.0));
                    let mem = stats.map_or_else(
                        || "-".into(),
                        |s| match s.mem_limit {
                            Some(limit) if limit > 0 => {
                                format!("{} / {}", format_bytes(s.mem_used), format_bytes(limit))
                            }
                            _ => format_bytes(s.mem_used),
                        },
                    );
                    let health = c.health_hint().unwrap_or("-");
                    Row::new(vec![
                        Cell::from(c.yoink_service.clone().unwrap_or_else(|| "-".into())),
                        Cell::from(c.name.clone()),
                        Cell::from(c.status_text.clone()),
                        Cell::from(c.state.clone()).style(state_style(&c.state)),
                        Cell::from(health.to_string()).style(health_style(health)),
                        Cell::from(cpu),
                        Cell::from(mem),
                    ])
                })
                .collect()
        };
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("service").style(bold()),
                Cell::from("container").style(bold()),
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

        let footer_text = if let Some(err) = &self.last_error {
            format!("error: {err}  ·  q quit · esc back · enter logs · r refresh")
        } else {
            "q quit · esc back · ↑↓ select · enter logs · r refresh".into()
        };
        let footer = Paragraph::new(footer_text).style(if self.last_error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        });
        frame.render_widget(footer, footer_area);
    }

    /// Host-aggregate summary panel: bordered block hosting a CPU
    /// `LineGauge` + Mem `LineGauge` (current snapshot) over twin
    /// `Sparkline` widgets (~2 minutes of trail at 2 s ticks).
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
            .constraints([
                Constraint::Length(1), // CPU LineGauge
                Constraint::Length(1), // Mem LineGauge
                Constraint::Min(1),    // Twin sparklines (CPU/Mem) side by side
            ])
            .split(inner);

        let cpu_total: f32 = self.stats.values().map(|s| s.cpu_pct as f32).sum();
        let mem_total: u64 = self.stats.values().map(|s| s.mem_used.max(0) as u64).sum();

        // Cap CPU at the highest historical value (or at least 100%) so
        // the bar has a meaningful scale even with multi-core spikes.
        let cpu_max = self
            .history
            .iter()
            .map(|(c, _)| *c)
            .fold(100.0_f32, f32::max);
        let cpu_ratio = (cpu_total / cpu_max).clamp(0.0, 1.0) as f64;
        let mem_max = self
            .history
            .iter()
            .map(|(_, m)| *m)
            .fold(mem_total as f32, f32::max)
            .max(1.0);
        let mem_ratio = ((mem_total as f32) / mem_max).clamp(0.0, 1.0) as f64;

        let cpu_label = format!("CPU  {cpu_total:>6.1}% / {cpu_max:.0}%");
        let mem_label = format!(
            "MEM  {} / {}",
            format_bytes(i64::try_from(mem_total).unwrap_or(i64::MAX)),
            format_bytes(mem_max as i64)
        );

        let cpu_gauge = LineGauge::default()
            .label(cpu_label)
            .ratio(cpu_ratio)
            .filled_style(Style::default().fg(gauge_color(cpu_ratio as f32)).add_modifier(Modifier::BOLD))
            .unfilled_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(cpu_gauge, chunks[0]);

        let mem_gauge = LineGauge::default()
            .label(mem_label)
            .ratio(mem_ratio)
            .filled_style(Style::default().fg(gauge_color(mem_ratio as f32)).add_modifier(Modifier::BOLD))
            .unfilled_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(mem_gauge, chunks[1]);

        // Side-by-side trend sparklines — left CPU, right Mem.
        let trend_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[2]);

        let cpu_series: Vec<u64> = self
            .history
            .iter()
            .map(|(c, _)| (*c as u64).max(1))
            .collect();
        let mem_series: Vec<u64> = self
            .history
            .iter()
            .map(|(_, m)| (*m as u64).max(1))
            .collect();

        let cpu_spark = Sparkline::default()
            .data(&cpu_series)
            .style(Style::default().fg(Color::Cyan));
        frame.render_widget(cpu_spark, trend_chunks[0]);

        let mem_spark = Sparkline::default()
            .data(&mem_series)
            .style(Style::default().fg(Color::Magenta));
        frame.render_widget(mem_spark, trend_chunks[1]);
    }
}

/// Green / yellow / red threshold for a 0..=1 gauge (mirrors
/// dashboard's helper).
fn gauge_color(ratio: f32) -> Color {
    if ratio < 0.60 {
        Color::Green
    } else if ratio < 0.85 {
        Color::Yellow
    } else {
        Color::Red
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
