//! Container detail pane — k9s-style "what's actually configured here?"
//! Shows the container's labels (yoink + others), state + status text,
//! creation time, plus live CPU/mem gauges. Reachable via `i` from
//! `HostDetail` or `ServiceDetail`. From here `Enter`/`l` opens logs,
//! `!` opens shell, `D` opens the debug sidecar.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::docker_ops::{ContainerInfo, ContainerStats, DockerOps, Host};
use crate::output::{format_bytes, format_relative_time};

use super::ui::{bold, gauge_color, health_style, inline_gauge, state_style};

#[derive(Default)]
pub struct ContainerDetailState {
    target: Option<(Host, String)>,
    info: Option<ContainerInfo>,
    stats: Option<ContainerStats>,
    last_error: Option<String>,
    loaded: bool,
}

pub struct ContainerDetailRefresh {
    pub info: Option<ContainerInfo>,
    pub stats: Option<ContainerStats>,
    pub error: Option<String>,
}

impl ContainerDetailState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_target(&mut self, host: Host, container: String) {
        if self.target.as_ref().map(|(h, c)| (h, c.as_str())) != Some((&host, container.as_str())) {
            self.info = None;
            self.stats = None;
            self.loaded = false;
        }
        self.target = Some((host, container));
    }

    pub fn target(&self) -> Option<&(Host, String)> {
        self.target.as_ref()
    }

    pub fn apply(&mut self, data: ContainerDetailRefresh) {
        self.last_error = data.error;
        if self.last_error.is_none() {
            self.info = data.info;
            self.stats = data.stats;
            self.loaded = true;
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(8),  // header card (state, version, created)
                Constraint::Length(4),  // live CPU/mem gauges
                Constraint::Min(0),     // labels table
                Constraint::Length(1),
            ])
            .split(area);

        let header_text = match &self.target {
            Some((h, c)) => format!("yoink container · {}/{}", h.address, c),
            None => "yoink container · (none selected)".into(),
        };
        frame.render_widget(Paragraph::new(header_text).style(bold()), layout[0]);

        if let Some(err) = &self.last_error {
            frame.render_widget(
                Paragraph::new(format!("error: {err}"))
                    .style(Style::default().fg(Color::Red))
                    .block(Block::default().borders(Borders::ALL).title(" container ")),
                layout[1],
            );
            return;
        }

        let Some(info) = &self.info else {
            let msg = if self.loaded {
                "(container not found — may have been removed)"
            } else {
                "(loading…)"
            };
            frame.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(Block::default().borders(Borders::ALL).title(" container ")),
                layout[1],
            );
            return;
        };

        Self::render_info_card(frame, layout[1], info);
        self.render_metrics(frame, layout[2], info);
        Self::render_labels(frame, layout[3], info);

        let footer = Paragraph::new(
            "q quit · esc back · l logs · ! shell · D debug · r refresh",
        )
        .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, layout[4]);
    }

    fn render_info_card(frame: &mut Frame<'_>, area: Rect, info: &ContainerInfo) {
        let block = Block::default().borders(Borders::ALL).title(" container ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let health = info.health_hint().unwrap_or("-");
        let lines: Vec<Line<'static>> = vec![
            kv("service", info.yoink_service.as_deref().unwrap_or("-")),
            kv_styled("state", &info.state, state_style(&info.state)),
            kv_styled("health", health, health_style(health)),
            kv("version", info.yoink_version.as_deref().unwrap_or("-")),
            kv("spec hash", info.yoink_spec_hash.as_deref().unwrap_or("-")),
            kv("created", &format_relative_time(info.created_unix)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn render_metrics(&self, frame: &mut Frame<'_>, area: Rect, _info: &ContainerInfo) {
        let block = Block::default().borders(Borders::ALL).title(" live ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(inner);

        let Some(stats) = &self.stats else {
            frame.render_widget(
                Paragraph::new("(no stats yet)").style(Style::default().fg(Color::DarkGray)),
                chunks[0],
            );
            return;
        };

        let cpu_ratio = (stats.cpu_pct as f32 / 100.0).clamp(0.0, 1.0);
        let mut cpu_spans = vec![Span::raw(format!("CPU {:>5.1}% ", stats.cpu_pct))];
        cpu_spans.extend(inline_gauge(cpu_ratio, 24, gauge_color(cpu_ratio)));
        frame.render_widget(Paragraph::new(Line::from(cpu_spans)), chunks[0]);

        let (mem_label, mem_ratio) = match stats.mem_limit {
            Some(limit) if limit > 0 => (
                format!(
                    "MEM {:>8} / {:<8} ",
                    format_bytes(stats.mem_used),
                    format_bytes(limit)
                ),
                ((stats.mem_used as f32) / (limit as f32)).clamp(0.0, 1.0),
            ),
            _ => (format!("MEM {:>8}            ", format_bytes(stats.mem_used)), 0.0),
        };
        let mut mem_spans = vec![Span::raw(mem_label)];
        mem_spans.extend(inline_gauge(mem_ratio, 24, gauge_color(mem_ratio)));
        frame.render_widget(Paragraph::new(Line::from(mem_spans)), chunks[1]);
    }

    fn render_labels(frame: &mut Frame<'_>, area: Rect, info: &ContainerInfo) {
        let rows: Vec<Row<'_>> = info
            .other_labels
            .iter()
            .map(|(k, v)| Row::new(vec![Cell::from(k.clone()), Cell::from(v.clone())]))
            .collect();
        let widths = [Constraint::Length(28), Constraint::Min(20)];
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("label").style(bold()),
                Cell::from("value").style(bold()),
            ]))
            .block(Block::default().borders(Borders::ALL).title(" labels "));
        frame.render_widget(table, area);
    }
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:>10}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(value.to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ])
}

fn kv_styled(key: &str, value: &str, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:>10}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(value.to_string(), value_style.add_modifier(Modifier::BOLD)),
    ])
}

/// Background-friendly fetch — `'static + Send` so it can be `tokio::spawn`-ed.
/// Re-uses `list_containers_by_label` filtered by name; `container_stats`
/// for CPU/mem. Errors surface in `ContainerDetailRefresh.error`.
pub async fn fetch_owned(
    ops: Arc<dyn DockerOps>,
    host: Host,
    container: String,
) -> ContainerDetailRefresh {
    let containers = ops.list_running_containers(&host).await;
    let info = match containers {
        Ok(list) => list.into_iter().find(|c| c.name == container),
        Err(e) => {
            return ContainerDetailRefresh {
                info: None,
                stats: None,
                error: Some(format!("list containers: {e}")),
            };
        }
    };
    let stats = ops.container_stats(&host, &container).await.ok();
    ContainerDetailRefresh {
        info,
        stats,
        error: None,
    }
}
