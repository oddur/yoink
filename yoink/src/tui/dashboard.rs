//! Dashboard pane: auto-refreshing service/host status grid with
//! per-container CPU% and memory snapshots.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures_util::future::join_all;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::config::Config;
use crate::docker_ops::{ContainerStats, DockerOps, Host};
use crate::output::{format_bytes, format_relative_time};
use crate::status::StatusReport;

use super::ui::{bold, health_style, inline_gauge, inline_sparkline, pane_layout, state_style};

/// How many CPU/mem samples to keep per container for the inline
/// sparklines. 24 samples × 2 s fast tick = ~48 s of trail — long
/// enough to see a deploy spike, short enough to fit in 12 cells.
const HISTORY_LEN: usize = 24;

pub struct DashboardRefresh {
    pub report: Option<StatusReport>,
    pub stats: HashMap<String, ContainerStats>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct DashboardState {
    report: Option<StatusReport>,
    /// Keyed by `<host>/<container>` so the render layer can look up live
    /// stats alongside the container info from the listing.
    stats: HashMap<String, ContainerStats>,
    /// Per-container ring buffer of the last `HISTORY_LEN` (`cpu_pct`,
    /// `mem_pct`) samples. Pushed on every `apply` so the sparkline
    /// trails the live value. Mem is stored as a fraction of limit
    /// (or 0 when no limit set) so both axes are 0..=1 for the
    /// inline gauge.
    history: HashMap<String, VecDeque<(f32, f32)>>,
    last_error: Option<String>,
    loaded: bool,
    /// When `true`, render also includes containers in the `exited` /
    /// `created` / `dead` / `restarting` states alongside running ones.
    /// Toggle with `e` from the Dashboard view. Useful for drilling
    /// into the logs of the just-replaced container after a deploy.
    show_exited: bool,
}

impl DashboardState {
    pub fn toggle_show_exited(&mut self) {
        self.show_exited = !self.show_exited;
    }
}

impl DashboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience for tests + sync paths.
    #[cfg(test)]
    pub async fn refresh(&mut self, ops: &dyn DockerOps, config: &Config) {
        let data = fetch(ops, config).await;
        self.apply(data);
    }

    /// Apply background-fetched results.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn apply(&mut self, data: DashboardRefresh) {
        self.last_error = data.error;
        if let Some(report) = data.report {
            // Snapshot every container's CPU% (normalized to 0..=1
            // assuming a single core's worth of headroom — bigger
            // values still render fine, the sparkline just clips at
            // the top) and mem% (fraction of limit, 0 when no limit).
            for (key, stats) in &data.stats {
                let cpu_norm = (stats.cpu_pct as f32 / 100.0).max(0.0);
                let mem_norm = match stats.mem_limit {
                    Some(limit) if limit > 0 => (stats.mem_used as f32) / (limit as f32),
                    _ => 0.0,
                };
                let buf = self.history.entry(key.clone()).or_default();
                buf.push_back((cpu_norm, mem_norm));
                if buf.len() > HISTORY_LEN {
                    buf.pop_front();
                }
            }
            // Drop history for containers that disappeared so the map
            // doesn't grow without bound across restarts/rollbacks.
            self.history.retain(|k, _| data.stats.contains_key(k));

            self.report = Some(report);
            self.stats = data.stats;
            self.loaded = true;
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, config: &Config) {
        let layout = pane_layout(area);

        let services = config
            .services
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let header =
            Paragraph::new(format!("yoink dashboard · services: {services}")).style(bold());
        frame.render_widget(header, layout[0]);

        let rows = self.build_rows();
        let widths = [
            Constraint::Length(18), // host
            Constraint::Length(14), // service
            Constraint::Length(26), // container
            Constraint::Length(9),  // state
            Constraint::Length(10), // health
            Constraint::Length(10), // version
            Constraint::Length(20), // cpu% (value + gauge + spark)
            Constraint::Length(28), // mem (value + gauge + spark)
            Constraint::Min(20),    // created
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(vec![
                Cell::from("host").style(bold()),
                Cell::from("service").style(bold()),
                Cell::from("container").style(bold()),
                Cell::from("state").style(bold()),
                Cell::from("health").style(bold()),
                Cell::from("version").style(bold()),
                Cell::from("cpu").style(bold()),
                Cell::from("mem").style(bold()),
                Cell::from("created").style(bold()),
            ]))
            .block(Block::default().borders(Borders::ALL).title("status"));
        frame.render_widget(table, layout[1]);

        let exited_indicator = if self.show_exited {
            "[exited: on]"
        } else {
            "[exited: off]"
        };
        let footer_text = if let Some(err) = &self.last_error {
            format!(
                "error: {err}  ·  q quit · r refresh · e {exited_indicator} · d dashboard · s services · h hosts · l logs"
            )
        } else {
            format!(
                "q quit · r refresh · e {exited_indicator} · d dashboard · s services · h hosts · l logs"
            )
        };
        let footer = Paragraph::new(footer_text).style(if self.last_error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        });
        frame.render_widget(footer, layout[2]);
    }

    fn build_rows(&self) -> Vec<Row<'_>> {
        let Some(report) = &self.report else {
            return vec![Row::new(vec![Cell::from("(loading…)")])];
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
                ]));
                continue;
            }
            for c in &host.containers {
                if !self.show_exited && !c.is_running() {
                    continue;
                }
                let health = c.health_hint().unwrap_or("-");
                let key = stats_key(&host.host, &c.name);
                let stats = self.stats.get(&key);
                let history = self.history.get(&key);

                let cpu_cell = render_cpu_cell(stats, history);
                let mem_cell = render_mem_cell(stats, history);

                rows.push(
                    Row::new(vec![
                        Cell::from(host.host.clone()),
                        Cell::from(c.yoink_service.clone().unwrap_or_else(|| "-".into())),
                        Cell::from(c.name.clone()),
                        Cell::from(c.state.clone()).style(state_style(&c.state)),
                        Cell::from(health.to_string()).style(health_style(health)),
                        Cell::from(c.yoink_version.clone().unwrap_or_else(|| "-".into())),
                        cpu_cell,
                        mem_cell,
                        Cell::from(format_relative_time(c.created_unix)),
                    ])
                    .height(2),
                );
            }
        }
        rows
    }
}

fn stats_key(host: &str, container: &str) -> String {
    format!("{host}/{container}")
}

/// CPU cell: line 1 = percentage + 8-wide block-character gauge,
/// line 2 = sparkline trail. Empty/no-stats containers get a single
/// dim "-" so the column alignment doesn't shift.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_cpu_cell(
    stats: Option<&ContainerStats>,
    history: Option<&VecDeque<(f32, f32)>>,
) -> Cell<'static> {
    let Some(s) = stats else {
        return Cell::from("-").style(Style::default().fg(Color::DarkGray));
    };
    let pct_text = format!("{:>5.1}% ", s.cpu_pct);
    // Gauge ratio: assume 1 core = 100%; clamps inside `inline_gauge`.
    let ratio = (s.cpu_pct as f32 / 100.0).clamp(0.0, 1.0);
    let gauge = inline_gauge(ratio, 8, gauge_color(ratio));
    let line1 = Line::from(vec![Span::raw(pct_text), gauge]);
    let samples: Vec<f32> = history
        .map(|h| h.iter().map(|(c, _)| *c).collect())
        .unwrap_or_default();
    let line2 = Line::from(inline_sparkline(&samples, 18, Color::Cyan));
    Cell::from(vec![line1, line2])
}

/// Mem cell: line 1 = "used / limit" + gauge (against limit when
/// known), line 2 = sparkline of used/limit trail.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_mem_cell(
    stats: Option<&ContainerStats>,
    history: Option<&VecDeque<(f32, f32)>>,
) -> Cell<'static> {
    let Some(s) = stats else {
        return Cell::from("-").style(Style::default().fg(Color::DarkGray));
    };
    let (text, ratio) = match s.mem_limit {
        Some(limit) if limit > 0 => {
            let r = (s.mem_used as f32) / (limit as f32);
            (
                format!("{} / {}", format_bytes(s.mem_used), format_bytes(limit)),
                r.clamp(0.0, 1.0),
            )
        }
        _ => (format_bytes(s.mem_used), 0.0),
    };
    let gauge = inline_gauge(ratio, 8, gauge_color(ratio));
    let line1 = Line::from(vec![Span::raw(format!("{text:<16} ")), gauge]);
    let samples: Vec<f32> = history
        .map(|h| h.iter().map(|(_, m)| *m).collect())
        .unwrap_or_default();
    let line2 = Line::from(inline_sparkline(&samples, 26, Color::Magenta));
    Cell::from(vec![line1, line2])
}

/// Green / yellow / red threshold for a 0..=1 gauge — same scale the
/// real `Gauge` widget uses by convention. <60% green, <85% yellow,
/// otherwise red.
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
pub async fn fetch_owned(ops: Arc<dyn DockerOps>, config: Arc<Config>) -> DashboardRefresh {
    fetch(ops.as_ref(), config.as_ref()).await
}

async fn fetch(ops: &dyn DockerOps, config: &Config) -> DashboardRefresh {
    let report = match StatusReport::collect(ops, config).await {
        Ok(r) => r,
        Err(e) => {
            return DashboardRefresh {
                report: None,
                stats: HashMap::new(),
                error: Some(format!("{e:#}")),
            };
        }
    };
    let host_user_by_address: HashMap<String, String> = config
        .hosts
        .iter()
        .map(|h| (h.address.clone(), h.user.clone()))
        .collect();
    let stats_futs = report
        .hosts
        .iter()
        .flat_map(|h| {
            let host_addr = h.host.clone();
            let user = host_user_by_address
                .get(&host_addr)
                .cloned()
                .unwrap_or_default();
            h.containers
                .iter()
                .filter(|c| c.is_running())
                .map(move |c| {
                    let host = Host {
                        user: user.clone(),
                        address: host_addr.clone(),
                    };
                    let key = stats_key(&host_addr, &c.name);
                    let name = c.name.clone();
                    async move {
                        let result = ops.container_stats(&host, &name).await;
                        (key, result)
                    }
                })
        })
        .collect::<Vec<_>>();
    let stats_results = join_all(stats_futs).await;
    let mut stats = HashMap::new();
    for (key, result) in stats_results {
        if let Ok(s) = result {
            stats.insert(key, s);
        }
    }
    DashboardRefresh {
        report: Some(report),
        stats,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{ContainerInfo, ContainerStats, FakeDockerOps};
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

    fn running_container() -> ContainerInfo {
        ContainerInfo {
            host: "host-a".into(),
            name: "app-a-a1b2c3d".into(),
            state: "running".into(),
            status_text: "Up 1h (healthy)".into(),
            created_unix: Some(1_735_128_000),
            yoink_service: Some("app-a".into()),
            yoink_version: Some("a1b2c3d".into()),
            yoink_spec_hash: None,
            other_labels: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn refresh_collects_listing_then_stats_per_running_container() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        ops.push_container_stats(Ok(ContainerStats {
            cpu_pct: 12.5,
            mem_used: 64 * 1024 * 1024,
            mem_limit: Some(512 * 1024 * 1024),
        }));
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.report.is_some());
        assert_eq!(state.stats.len(), 1);
        let s = state.stats.get("host-a/app-a-a1b2c3d").unwrap();
        assert!((s.cpu_pct - 12.5).abs() < f64::EPSILON);
        assert_eq!(s.mem_used, 64 * 1024 * 1024);
    }

    #[tokio::test]
    async fn refresh_skips_stats_for_non_running_containers() {
        let ops = FakeDockerOps::new();
        let mut stopped = running_container();
        stopped.state = "exited".into();
        ops.push_list_containers(Ok(vec![stopped]));
        // No push_container_stats — refresh shouldn't attempt one.
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.stats.is_empty());
    }

    #[tokio::test]
    async fn refresh_keeps_report_when_individual_stats_fails() {
        let ops = FakeDockerOps::new();
        ops.push_list_containers(Ok(vec![running_container()]));
        // No stats response — fetch returns FakeExhausted, which we
        // tolerate; report is still populated, stats map stays empty.
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.report.is_some());
        assert!(state.stats.is_empty());
    }

    #[tokio::test]
    async fn refresh_captures_error_into_last_error_field() {
        let ops = FakeDockerOps::new();
        let mut state = DashboardState::new();
        state.refresh(&ops, &config()).await;
        assert!(state.last_error.is_some());
        assert!(state.report.is_none());
    }
}
